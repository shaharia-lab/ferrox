use async_trait::async_trait;
use aws_sdk_bedrockruntime::primitives::Blob;
use serde_json::Value;
use uuid::Uuid;

use crate::config::{DefaultsConfig, ProviderConfig};
use crate::error::ProxyError;
use crate::providers::anthropic_events::{make_final_chunk, AnthropicEventProcessor};
use crate::providers::{ProviderAdapter, ProviderStream};
use crate::types::{
    ChatCompletionRequest, ChatCompletionResponse, ChatMessage, Choice, FunctionCall,
    MessageContent, StopSequences, ToolCall, Usage,
};

// ── Adapter ──────────────────────────────────────────────────────────────────

pub struct BedrockAdapter {
    name: String,
    client: aws_sdk_bedrockruntime::Client,
}

impl BedrockAdapter {
    pub async fn new(
        cfg: &ProviderConfig,
        _defaults: &DefaultsConfig,
    ) -> Result<Self, anyhow::Error> {
        let region_str = cfg.region.clone();
        let mut aws_config_loader = aws_config::defaults(aws_config::BehaviorVersion::latest());

        if let Some(region) = region_str {
            let region = aws_config::Region::new(region);
            aws_config_loader = aws_config_loader.region(region);
        }

        let aws_config = aws_config_loader.load().await;
        let client = aws_sdk_bedrockruntime::Client::new(&aws_config);

        Ok(Self {
            name: cfg.name.clone(),
            client,
        })
    }
}

#[async_trait]
impl ProviderAdapter for BedrockAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    async fn chat(
        &self,
        req: &ChatCompletionRequest,
        model_id: &str,
    ) -> Result<ChatCompletionResponse, ProxyError> {
        let body = build_anthropic_body(req, model_id, false);
        let body_bytes = serde_json::to_vec(&body).map_err(ProxyError::SerializationError)?;

        let result = self
            .client
            .invoke_model()
            .model_id(model_id)
            .content_type("application/json")
            .accept("application/json")
            .body(Blob::new(body_bytes))
            .send()
            .await
            .map_err(|e| ProxyError::AwsError(e.to_string()))?;

        let bytes = result.body.into_inner();
        let resp: Value = serde_json::from_slice(&bytes).map_err(ProxyError::SerializationError)?;
        Ok(bedrock_anthropic_to_openai(&resp, model_id))
    }

    async fn chat_stream(
        &self,
        req: &ChatCompletionRequest,
        model_id: &str,
    ) -> Result<ProviderStream, ProxyError> {
        let body = build_anthropic_body(req, model_id, true);
        let body_bytes = serde_json::to_vec(&body).map_err(ProxyError::SerializationError)?;

        let mut event_stream = self
            .client
            .invoke_model_with_response_stream()
            .model_id(model_id)
            .content_type("application/json")
            .accept("application/json")
            .body(Blob::new(body_bytes))
            .send()
            .await
            .map_err(|e| ProxyError::AwsError(e.to_string()))?
            .body;

        let model_id = model_id.to_string();
        let provider_name = self.name.clone();

        let chunk_stream = async_stream::stream! {
            let mut processor = AnthropicEventProcessor::new(Uuid::new_v4().to_string());

            loop {
                match event_stream.recv().await {
                    Ok(Some(event)) => {
                        use aws_sdk_bedrockruntime::types::ResponseStream;
                        if let ResponseStream::Chunk(chunk) = event {
                                let bytes = match chunk.bytes {
                                    Some(b) => b.into_inner(),
                                    None => continue,
                                };
                                let v: Value = match serde_json::from_slice(&bytes) {
                                    Ok(v) => v,
                                    Err(e) => {
                                        yield Err(ProxyError::StreamError(format!(
                                            "Bedrock JSON parse error: {e}"
                                        )));
                                        return;
                                    }
                                };

                                // Anthropic-on-Bedrock uses the same SSE event format,
                                // embedded in the "bytes" field as JSON
                                let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                let data = v.to_string();

                                for chunk_result in processor.process(
                                    event_type,
                                    &data,
                                    &model_id,
                                    &provider_name,
                                ) {
                                    yield chunk_result;
                                }
                        } // if let ResponseStream::Chunk
                    }
                    Ok(None) => break,
                    Err(e) => {
                        yield Err(ProxyError::AwsError(e.to_string()));
                        return;
                    }
                }
            }

            // Emit final chunk
            yield Ok(make_final_chunk(
                &processor.message_id,
                &model_id,
                processor.stop_reason.take(),
                processor.usage.take(),
            ));
        };

        Ok(Box::pin(chunk_stream))
    }
}

// ── Anthropic-format body builder ────────────────────────────────────────────

fn build_anthropic_body(req: &ChatCompletionRequest, model_id: &str, stream: bool) -> Value {
    let system = req.system_message();

    let messages: Vec<Value> = req
        .messages
        .iter()
        .filter(|m| m.role != "system")
        .map(bedrock_message)
        .collect();

    let stop_sequences: Option<Vec<String>> = req.stop.as_ref().map(|s| match s {
        StopSequences::Single(v) => vec![v.clone()],
        StopSequences::Multiple(v) => v.clone(),
    });

    let mut body = serde_json::json!({
        "anthropic_version": "bedrock-2023-05-31",
        "model": model_id,
        "messages": messages,
        "max_tokens": req.max_tokens.unwrap_or(4096),
    });

    if let Some(s) = system {
        body["system"] = Value::String(s);
    }
    if stream {
        body["stream"] = Value::Bool(true);
    }
    if let Some(t) = req.temperature {
        body["temperature"] = Value::from(t as f64);
    }
    if let Some(p) = req.top_p {
        body["top_p"] = Value::from(p as f64);
    }
    if let Some(stop) = stop_sequences {
        body["stop_sequences"] = serde_json::to_value(stop).unwrap_or(Value::Null);
    }
    if let Some(tools) = &req.tools {
        body["tools"] = Value::Array(
            tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.function.name,
                        "description": t.function.description,
                        "input_schema": t.function.parameters.clone()
                            .unwrap_or_else(|| serde_json::json!({"type": "object"})),
                    })
                })
                .collect(),
        );
    }
    if let Some(tc) = req
        .tool_choice
        .as_ref()
        .and_then(openai_tool_choice_to_anthropic)
    {
        body["tool_choice"] = tc;
    }

    body
}

/// Build one Anthropic message Value, handling assistant tool calls (→ `tool_use`
/// blocks) and `role:"tool"` results (→ a user turn with a `tool_result` block).
fn bedrock_message(m: &ChatMessage) -> Value {
    if m.role == "tool" {
        let result = match &m.content {
            Some(MessageContent::Text(t)) => t.clone(),
            _ => String::new(),
        };
        return serde_json::json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                "content": result,
            }],
        });
    }

    let role = if m.role == "assistant" {
        "assistant"
    } else {
        "user"
    };

    // Assistant tool calls (with optional preceding text) → a content-block array.
    if let Some(tcs) = &m.tool_calls {
        let mut blocks: Vec<Value> = Vec::new();
        if let Some(MessageContent::Text(t)) = &m.content {
            if !t.is_empty() {
                blocks.push(serde_json::json!({"type": "text", "text": t}));
            }
        }
        for tc in tcs {
            let input: Value = serde_json::from_str(&tc.function.arguments)
                .unwrap_or_else(|_| serde_json::json!({}));
            blocks.push(serde_json::json!({
                "type": "tool_use",
                "id": tc.id,
                "name": tc.function.name,
                "input": input,
            }));
        }
        return serde_json::json!({ "role": role, "content": blocks });
    }

    let content = match &m.content {
        Some(MessageContent::Text(t)) => Value::String(t.clone()),
        // Multi-part content → Anthropic content-block array (text + image).
        Some(MessageContent::Parts(parts)) => Value::Array(
            parts
                .iter()
                .map(|p| match p {
                    crate::types::ContentPart::Text { text } => {
                        serde_json::json!({"type": "text", "text": text})
                    }
                    crate::types::ContentPart::ImageUrl { image_url } => {
                        anthropic_image_block(&image_url.url)
                    }
                })
                .collect(),
        ),
        None => Value::String(String::new()),
    };
    serde_json::json!({ "role": role, "content": content })
}

/// Build an Anthropic `image` content block from an OpenAI image URL
/// (`data:` → base64 source; otherwise a `url` source).
fn anthropic_image_block(url: &str) -> Value {
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((header, data)) = rest.split_once(',') {
            let media_type = header.split(';').next().unwrap_or("image/jpeg");
            return serde_json::json!({
                "type": "image",
                "source": {"type": "base64", "media_type": media_type, "data": data},
            });
        }
    }
    serde_json::json!({
        "type": "image",
        "source": {"type": "url", "url": url},
    })
}

/// Map an OpenAI `tool_choice` to an Anthropic `tool_choice` object.
fn openai_tool_choice_to_anthropic(tc: &Value) -> Option<Value> {
    match tc {
        Value::String(s) => match s.as_str() {
            "auto" => Some(serde_json::json!({"type": "auto"})),
            "required" => Some(serde_json::json!({"type": "any"})),
            // Anthropic has no explicit "none"; omit to let the model not call tools.
            "none" => None,
            _ => None,
        },
        Value::Object(o) => o
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            .map(|name| serde_json::json!({"type": "tool", "name": name})),
        _ => None,
    }
}

// ── Response conversion ───────────────────────────────────────────────────────

fn bedrock_anthropic_to_openai(resp: &Value, model_id: &str) -> ChatCompletionResponse {
    let id = resp
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Iterate all content blocks: concatenate text, collect tool_use → tool_calls.
    let mut text = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    if let Some(blocks) = resp.get("content").and_then(|c| c.as_array()) {
        for block in blocks {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    text.push_str(block.get("text").and_then(|t| t.as_str()).unwrap_or(""));
                }
                Some("tool_use") => {
                    tool_calls.push(ToolCall {
                        id: block
                            .get("id")
                            .and_then(|i| i.as_str())
                            .unwrap_or("")
                            .to_string(),
                        r#type: "function".to_string(),
                        function: FunctionCall {
                            name: block
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string(),
                            arguments: block
                                .get("input")
                                .map(|i| i.to_string())
                                .unwrap_or_else(|| "{}".to_string()),
                        },
                    });
                }
                _ => {}
            }
        }
    }

    let finish_reason = resp
        .get("stop_reason")
        .and_then(|r| r.as_str())
        .map(|r| match r {
            "end_turn" => "stop".to_string(),
            "max_tokens" => "length".to_string(),
            "tool_use" => "tool_calls".to_string(),
            "stop_sequence" => "stop".to_string(),
            other => other.to_string(),
        });

    let usage = resp.get("usage").map(|u| Usage {
        prompt_tokens: u.get("input_tokens").and_then(|t| t.as_u64()).unwrap_or(0) as u32,
        completion_tokens: u.get("output_tokens").and_then(|t| t.as_u64()).unwrap_or(0) as u32,
        total_tokens: (u.get("input_tokens").and_then(|t| t.as_u64()).unwrap_or(0)
            + u.get("output_tokens").and_then(|t| t.as_u64()).unwrap_or(0))
            as u32,
    });

    let message = ChatMessage {
        role: "assistant".to_string(),
        content: if text.is_empty() {
            None
        } else {
            Some(MessageContent::Text(text))
        },
        name: None,
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        tool_call_id: None,
        reasoning_content: None,
    };

    ChatCompletionResponse {
        id,
        object: "chat.completion".to_string(),
        created: chrono::Utc::now().timestamp() as u64,
        model: model_id.to_string(),
        choices: vec![Choice {
            index: 0,
            message,
            finish_reason,
        }],
        usage,
        system_fingerprint: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Tool, ToolFunction};

    fn req(
        messages: Vec<ChatMessage>,
        with_tools: bool,
        tool_choice: Option<Value>,
    ) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "claude".into(),
            messages,
            stream: None,
            temperature: None,
            max_tokens: None,
            top_p: None,
            stop: None,
            tools: with_tools.then(|| {
                vec![Tool {
                    r#type: "function".into(),
                    function: ToolFunction {
                        name: "get_weather".into(),
                        description: Some("w".into()),
                        parameters: Some(serde_json::json!({"type": "object"})),
                    },
                }]
            }),
            tool_choice,
            system: None,
            extra_headers: Default::default(),
            raw_anthropic_body: None,
            extra: Default::default(),
        }
    }

    fn asst_tool_call() -> ChatMessage {
        ChatMessage {
            role: "assistant".into(),
            content: None,
            name: None,
            tool_calls: Some(vec![ToolCall {
                id: "t1".into(),
                r#type: "function".into(),
                function: FunctionCall {
                    name: "get_weather".into(),
                    arguments: r#"{"loc":"NYC"}"#.into(),
                },
            }]),
            tool_call_id: None,
            reasoning_content: None,
        }
    }

    #[test]
    fn body_includes_tools_and_tool_use_blocks() {
        let tool_msg = ChatMessage {
            role: "tool".into(),
            content: Some(MessageContent::Text("sunny".into())),
            name: None,
            tool_calls: None,
            tool_call_id: Some("t1".into()),
            reasoning_content: None,
        };
        let body = build_anthropic_body(
            &req(
                vec![asst_tool_call(), tool_msg],
                true,
                Some(serde_json::json!("required")),
            ),
            "claude",
            false,
        );
        let dump = body.to_string();
        assert!(dump.contains("\"tools\""), "tools sent upstream: {dump}");
        assert!(dump.contains("input_schema"));
        assert!(
            dump.contains("tool_use"),
            "assistant tool_call → tool_use block"
        );
        assert!(
            dump.contains("tool_result"),
            "tool result → tool_result block"
        );
        assert_eq!(body["tool_choice"]["type"], "any");
    }

    #[test]
    fn response_tool_use_becomes_tool_calls() {
        let resp = serde_json::json!({
            "id": "m",
            "content": [
                {"type": "text", "text": "let me check"},
                {"type": "tool_use", "id": "tu1", "name": "get_weather", "input": {"loc": "NYC"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 5, "output_tokens": 3}
        });
        let out = bedrock_anthropic_to_openai(&resp, "claude");
        let tc = &out.choices[0].message.tool_calls.as_ref().unwrap()[0];
        assert_eq!(tc.function.name, "get_weather");
        assert!(tc.function.arguments.contains("NYC"));
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("tool_calls"));
    }
    #[test]
    fn image_part_becomes_anthropic_image_block() {
        let m = ChatMessage {
            role: "user".into(),
            content: Some(MessageContent::Parts(vec![
                crate::types::ContentPart::Text {
                    text: "look".into(),
                },
                crate::types::ContentPart::ImageUrl {
                    image_url: crate::types::ImageUrl {
                        url: "data:image/png;base64,QUJD".into(),
                        detail: None,
                    },
                },
            ])),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        };
        let v = bedrock_message(&m);
        let blocks = v["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["type"], "base64");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
        assert_eq!(blocks[1]["source"]["data"], "QUJD");
    }
}

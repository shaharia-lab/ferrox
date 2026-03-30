use async_trait::async_trait;
use aws_sdk_bedrockruntime::primitives::Blob;
use serde_json::Value;
use uuid::Uuid;

use crate::config::{DefaultsConfig, ProviderConfig};
use crate::error::ProxyError;
use crate::providers::{ProviderAdapter, ProviderStream};
use crate::types::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ChatMessage, Choice,
    ChunkChoice, ChunkDelta, FunctionCall, MessageContent, StopSequences, ToolCall, Usage,
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

        let chunk_stream = async_stream::stream! {
            let mut state = AnthropicEventState::new(Uuid::new_v4().to_string());

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

                                for chunk_result in process_anthropic_event(
                                    event_type,
                                    &v,
                                    &model_id,
                                    &mut state,
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
            yield Ok(ChatCompletionChunk {
                id: state.message_id,
                object: "chat.completion.chunk".to_string(),
                created: chrono::Utc::now().timestamp() as u64,
                model: model_id,
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta { role: None, content: None, tool_calls: None },
                    finish_reason: state.stop_reason,
                }],
                usage: state.usage,
            });
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
        .map(|m| {
            let role = match m.role.as_str() {
                "assistant" => "assistant",
                _ => "user",
            };
            let content = match &m.content {
                None => Value::String(String::new()),
                Some(MessageContent::Text(t)) => Value::String(t.clone()),
                Some(MessageContent::Parts(_)) => Value::String(String::new()),
            };
            serde_json::json!({ "role": role, "content": content })
        })
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

    body
}

// ── Response conversion ───────────────────────────────────────────────────────

fn bedrock_anthropic_to_openai(resp: &Value, model_id: &str) -> ChatCompletionResponse {
    let id = resp
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let text = resp
        .pointer("/content/0/text")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();

    let finish_reason = resp
        .get("stop_reason")
        .and_then(|r| r.as_str())
        .map(|r| match r {
            "end_turn" => "stop".to_string(),
            "max_tokens" => "length".to_string(),
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
        content: Some(MessageContent::Text(text)),
        name: None,
        tool_calls: None,
        tool_call_id: None,
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

// ── Anthropic-on-Bedrock event processing ─────────────────────────────────────

/// Accumulates mutable per-message state while processing an Anthropic event stream.
struct AnthropicEventState {
    message_id: String,
    pending_tool_id: String,
    pending_tool_name: String,
    pending_tool_args: String,
    pending_tool_index: u32,
    stop_reason: Option<String>,
    usage: Option<Usage>,
}

impl AnthropicEventState {
    fn new(message_id: String) -> Self {
        Self {
            message_id,
            pending_tool_id: String::new(),
            pending_tool_name: String::new(),
            pending_tool_args: String::new(),
            pending_tool_index: 0,
            stop_reason: None,
            usage: None,
        }
    }
}

fn process_anthropic_event(
    event_type: &str,
    v: &Value,
    model_id: &str,
    state: &mut AnthropicEventState,
) -> Vec<Result<ChatCompletionChunk, ProxyError>> {
    let mut results = Vec::new();

    match event_type {
        "message_start" => {
            if let Some(id) = v.pointer("/message/id").and_then(|v| v.as_str()) {
                state.message_id = id.to_string();
            }
        }
        "content_block_start" => {
            if v.pointer("/content_block/type").and_then(|t| t.as_str()) == Some("tool_use") {
                state.pending_tool_id = v
                    .pointer("/content_block/id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                state.pending_tool_name = v
                    .pointer("/content_block/name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                state.pending_tool_args.clear();
                state.pending_tool_index =
                    v.pointer("/index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            }
        }
        "content_block_delta" => {
            let delta_type = v
                .pointer("/delta/type")
                .and_then(|t| t.as_str())
                .unwrap_or("");
            match delta_type {
                "text_delta" => {
                    let text = v
                        .pointer("/delta/text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    if !text.is_empty() {
                        results.push(Ok(ChatCompletionChunk {
                            id: state.message_id.clone(),
                            object: "chat.completion.chunk".to_string(),
                            created: chrono::Utc::now().timestamp() as u64,
                            model: model_id.to_string(),
                            choices: vec![ChunkChoice {
                                index: 0,
                                delta: ChunkDelta {
                                    role: None,
                                    content: Some(text),
                                    tool_calls: None,
                                },
                                finish_reason: None,
                            }],
                            usage: None,
                        }));
                    }
                }
                "input_json_delta" => {
                    let partial = v
                        .pointer("/delta/partial_json")
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    state.pending_tool_args.push_str(partial);
                }
                _ => {}
            }
        }
        "content_block_stop" => {
            if !state.pending_tool_id.is_empty() {
                let tool_call = ToolCall {
                    id: state.pending_tool_id.clone(),
                    r#type: "function".to_string(),
                    function: FunctionCall {
                        name: state.pending_tool_name.clone(),
                        arguments: state.pending_tool_args.clone(),
                    },
                };
                results.push(Ok(ChatCompletionChunk {
                    id: state.message_id.clone(),
                    object: "chat.completion.chunk".to_string(),
                    created: chrono::Utc::now().timestamp() as u64,
                    model: model_id.to_string(),
                    choices: vec![ChunkChoice {
                        index: state.pending_tool_index,
                        delta: ChunkDelta {
                            role: None,
                            content: None,
                            tool_calls: Some(vec![tool_call]),
                        },
                        finish_reason: None,
                    }],
                    usage: None,
                }));
                state.pending_tool_id.clear();
                state.pending_tool_name.clear();
                state.pending_tool_args.clear();
            }
        }
        "message_delta" => {
            state.stop_reason = v
                .pointer("/delta/stop_reason")
                .and_then(|r| r.as_str())
                .map(|r| match r {
                    "end_turn" => "stop".to_string(),
                    "max_tokens" => "length".to_string(),
                    "tool_use" => "tool_calls".to_string(),
                    other => other.to_string(),
                });
            if let Some(output) = v.pointer("/usage/output_tokens").and_then(|t| t.as_u64()) {
                state.usage = Some(Usage {
                    prompt_tokens: 0,
                    completion_tokens: output as u32,
                    total_tokens: output as u32,
                });
            }
        }
        _ => {}
    }

    results
}

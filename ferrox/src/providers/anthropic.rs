use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::config::{DefaultsConfig, ProviderConfig};
use crate::error::ProxyError;
use crate::providers::{parse_sse_stream, ProviderAdapter, ProviderStream};
use crate::types::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ChatMessage, Choice,
    ChunkChoice, ChunkDelta, ContentPart, FunctionCall, MessageContent, StopSequences, ToolCall,
    Usage,
};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 4096;

// ── Adapter ──────────────────────────────────────────────────────────────────

pub struct AnthropicAdapter {
    name: String,
    api_key: String,
    base_url: String,
    client: Client,
}

impl AnthropicAdapter {
    pub fn new(cfg: &ProviderConfig, defaults: &DefaultsConfig) -> Result<Self, anyhow::Error> {
        let api_key = cfg
            .api_key
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Anthropic provider '{}' requires api_key", cfg.name))?;

        let timeouts = cfg.timeouts.as_ref().unwrap_or(&defaults.timeouts);

        let client = Client::builder()
            .connect_timeout(Duration::from_secs(timeouts.connect_secs))
            .timeout(Duration::from_secs(timeouts.ttfb_secs + 3600)) // generous outer bound
            .build()?;

        Ok(Self {
            name: cfg.name.clone(),
            api_key,
            base_url: cfg
                .base_url
                .clone()
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            client,
        })
    }
}

#[async_trait]
impl ProviderAdapter for AnthropicAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    async fn chat(
        &self,
        req: &ChatCompletionRequest,
        model_id: &str,
    ) -> Result<ChatCompletionResponse, ProxyError> {
        let body = build_request_body(req, model_id, false);
        let url = format!("{}/v1/messages", self.base_url);

        let resp = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ProxyError::UpstreamTimeout(e.to_string())
                } else {
                    ProxyError::HttpClientError(e)
                }
            })?;

        let status = resp.status().as_u16();
        if status >= 400 {
            let text = resp.text().await.unwrap_or_default();
            return Err(ProxyError::ProviderError {
                provider: self.name.clone(),
                status,
                message: text,
            });
        }

        let anthropic_resp: AnthropicResponse =
            resp.json().await.map_err(ProxyError::HttpClientError)?;
        Ok(anthropic_to_openai_response(anthropic_resp, model_id))
    }

    async fn chat_stream(
        &self,
        req: &ChatCompletionRequest,
        model_id: &str,
    ) -> Result<ProviderStream, ProxyError> {
        let body = build_request_body(req, model_id, true);
        let url = format!("{}/v1/messages", self.base_url);

        let resp = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ProxyError::UpstreamTimeout(e.to_string())
                } else {
                    ProxyError::HttpClientError(e)
                }
            })?;

        let status = resp.status().as_u16();
        if status >= 400 {
            let text = resp.text().await.unwrap_or_default();
            return Err(ProxyError::ProviderError {
                provider: self.name.clone(),
                status,
                message: text,
            });
        }

        let provider_name = self.name.clone();
        let model_id = model_id.to_string();

        let sse_stream = parse_sse_stream(resp);
        let chunk_stream = transform_stream(sse_stream, provider_name, model_id);

        Ok(Box::pin(chunk_stream))
    }
}

// ── Request building ─────────────────────────────────────────────────────────

#[derive(Serialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequences: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<Value>,
}

#[derive(Serialize, Clone)]
struct AnthropicMessage {
    role: String,
    content: AnthropicContent,
}

#[derive(Serialize, Clone)]
#[serde(untagged)]
enum AnthropicContent {
    Text(String),
    Parts(Vec<AnthropicPart>),
}

#[derive(Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicPart {
    Text {
        text: String,
    },
    Image {
        source: AnthropicImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

#[derive(Serialize, Clone)]
struct AnthropicImageSource {
    #[serde(rename = "type")]
    source_type: String,
    url: String,
}

#[derive(Serialize)]
struct AnthropicTool {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    input_schema: Value,
}

fn build_request_body(
    req: &ChatCompletionRequest,
    model_id: &str,
    stream: bool,
) -> AnthropicRequest {
    let system = req.system_message();

    // Filter out system messages; Anthropic does not allow them in the messages array
    let messages: Vec<AnthropicMessage> = req
        .messages
        .iter()
        .filter(|m| m.role != "system")
        .map(convert_message)
        .collect();

    let stop_sequences = req.stop.as_ref().map(|s| match s {
        StopSequences::Single(v) => vec![v.clone()],
        StopSequences::Multiple(v) => v.clone(),
    });

    let tools = req.tools.as_ref().map(|tools| {
        tools
            .iter()
            .map(|t| AnthropicTool {
                name: t.function.name.clone(),
                description: t.function.description.clone(),
                input_schema: t
                    .function
                    .parameters
                    .clone()
                    .unwrap_or_else(|| serde_json::json!({"type":"object","properties":{}})),
            })
            .collect()
    });

    AnthropicRequest {
        model: model_id.to_string(),
        messages,
        system,
        max_tokens: req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        stream: if stream { Some(true) } else { None },
        temperature: req.temperature,
        top_p: req.top_p,
        stop_sequences,
        tools,
        tool_choice: req.tool_choice.clone(),
    }
}

fn convert_message(msg: &ChatMessage) -> AnthropicMessage {
    let role = match msg.role.as_str() {
        "assistant" => "assistant",
        _ => "user",
    };

    let content = if let Some(tool_calls) = &msg.tool_calls {
        // Assistant message with tool calls
        let parts: Vec<AnthropicPart> = tool_calls
            .iter()
            .map(|tc| AnthropicPart::ToolUse {
                id: tc.id.clone(),
                name: tc.function.name.clone(),
                input: serde_json::from_str(&tc.function.arguments)
                    .unwrap_or(serde_json::json!({})),
            })
            .collect();
        AnthropicContent::Parts(parts)
    } else if let Some(tool_call_id) = &msg.tool_call_id {
        // Tool result message
        let text = msg
            .content
            .as_ref()
            .map(|c| match c {
                MessageContent::Text(t) => t.clone(),
                MessageContent::Parts(parts) => parts
                    .iter()
                    .filter_map(|p| {
                        if let ContentPart::Text { text } = p {
                            Some(text.as_str())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            })
            .unwrap_or_default();
        AnthropicContent::Parts(vec![AnthropicPart::ToolResult {
            tool_use_id: tool_call_id.clone(),
            content: text,
        }])
    } else {
        match &msg.content {
            None => AnthropicContent::Text(String::new()),
            Some(MessageContent::Text(t)) => AnthropicContent::Text(t.clone()),
            Some(MessageContent::Parts(parts)) => {
                let converted: Vec<AnthropicPart> = parts
                    .iter()
                    .map(|p| match p {
                        ContentPart::Text { text } => AnthropicPart::Text { text: text.clone() },
                        ContentPart::ImageUrl { image_url } => AnthropicPart::Image {
                            source: AnthropicImageSource {
                                source_type: "url".to_string(),
                                url: image_url.url.clone(),
                            },
                        },
                    })
                    .collect();
                AnthropicContent::Parts(converted)
            }
        }
    };

    AnthropicMessage {
        role: role.to_string(),
        content,
    }
}

// ── Response conversion ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AnthropicResponse {
    id: String,
    #[allow(dead_code)]
    model: String,
    content: Vec<AnthropicResponseContent>,
    stop_reason: Option<String>,
    usage: Option<AnthropicUsage>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicResponseContent {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
}

#[derive(Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}

fn anthropic_to_openai_response(resp: AnthropicResponse, model_id: &str) -> ChatCompletionResponse {
    let mut text_content = String::new();
    let mut tool_calls = Vec::new();

    for content in resp.content {
        match content {
            AnthropicResponseContent::Text { text } => {
                text_content.push_str(&text);
            }
            AnthropicResponseContent::ToolUse { id, name, input } => {
                tool_calls.push(crate::types::ToolCall {
                    id,
                    r#type: "function".to_string(),
                    function: FunctionCall {
                        name,
                        arguments: input.to_string(),
                    },
                });
            }
        }
    }

    let message = ChatMessage {
        role: "assistant".to_string(),
        content: if text_content.is_empty() {
            None
        } else {
            Some(MessageContent::Text(text_content))
        },
        name: None,
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        tool_call_id: None,
    };

    let finish_reason = resp.stop_reason.map(|r| match r.as_str() {
        "end_turn" => "stop".to_string(),
        "max_tokens" => "length".to_string(),
        "tool_use" => "tool_calls".to_string(),
        other => other.to_string(),
    });

    let usage = resp.usage.map(|u| Usage {
        prompt_tokens: u.input_tokens,
        completion_tokens: u.output_tokens,
        total_tokens: u.input_tokens + u.output_tokens,
    });

    ChatCompletionResponse {
        id: resp.id,
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

// ── Streaming transform ───────────────────────────────────────────────────────

fn transform_stream(
    sse_stream: impl futures::Stream<Item = Result<(Option<String>, String), ProxyError>>
        + Send
        + 'static,
    provider_name: String,
    model_id: String,
) -> impl futures::Stream<Item = Result<ChatCompletionChunk, ProxyError>> + Send + 'static {
    async_stream::stream! {
        futures::pin_mut!(sse_stream);

        let mut message_id = Uuid::new_v4().to_string();
        // Accumulated state for tool calls
        let mut pending_tool_id = String::new();
        let mut pending_tool_name = String::new();
        let mut pending_tool_args = String::new();
        let mut pending_tool_index: u32 = 0;

        let mut final_stop_reason: Option<String> = None;
        let mut final_usage: Option<Usage> = None;

        while let Some(item) = sse_stream.next().await {
            let (event_type, data) = match item {
                Ok(v) => v,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };

            let event = event_type.as_deref().unwrap_or("");

            match event {
                "message_start" => {
                    // Extract message ID from the event
                    if let Ok(v) = serde_json::from_str::<Value>(&data) {
                        if let Some(id) = v.pointer("/message/id").and_then(|v| v.as_str()) {
                            message_id = id.to_string();
                        }
                    }
                }
                "content_block_start" => {
                    // Note block type; if tool_use, capture id/name
                    if let Ok(v) = serde_json::from_str::<Value>(&data) {
                        if v.pointer("/content_block/type").and_then(|t| t.as_str()) == Some("tool_use") {
                            pending_tool_id = v.pointer("/content_block/id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            pending_tool_name = v.pointer("/content_block/name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            pending_tool_args.clear();
                            pending_tool_index = v.pointer("/index")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0) as u32;
                        }
                    }
                }
                "content_block_delta" => {
                    if let Ok(v) = serde_json::from_str::<Value>(&data) {
                        let delta_type = v.pointer("/delta/type").and_then(|t| t.as_str()).unwrap_or("");
                        match delta_type {
                            "text_delta" => {
                                let text = v.pointer("/delta/text")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                if !text.is_empty() {
                                    yield Ok(make_text_chunk(&message_id, &model_id, text));
                                }
                            }
                            "input_json_delta" => {
                                let partial = v.pointer("/delta/partial_json")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("");
                                pending_tool_args.push_str(partial);
                            }
                            _ => {}
                        }
                    }
                }
                "content_block_stop" => {
                    // Emit tool call chunk if we were accumulating one
                    if !pending_tool_id.is_empty() {
                        let tool_call = crate::types::ToolCall {
                            id: pending_tool_id.clone(),
                            r#type: "function".to_string(),
                            function: FunctionCall {
                                name: pending_tool_name.clone(),
                                arguments: pending_tool_args.clone(),
                            },
                        };
                        yield Ok(make_tool_call_chunk(
                            &message_id,
                            &model_id,
                            pending_tool_index,
                            tool_call,
                        ));
                        pending_tool_id.clear();
                        pending_tool_name.clear();
                        pending_tool_args.clear();
                    }
                }
                "message_delta" => {
                    if let Ok(v) = serde_json::from_str::<Value>(&data) {
                        final_stop_reason = v.pointer("/delta/stop_reason")
                            .and_then(|r| r.as_str())
                            .map(|r| match r {
                                "end_turn" => "stop".to_string(),
                                "max_tokens" => "length".to_string(),
                                "tool_use" => "tool_calls".to_string(),
                                other => other.to_string(),
                            });
                        final_usage = parse_usage_from_message_delta(&v);
                    }
                }
                "message_stop" => {
                    // Emit final chunk with finish_reason + usage
                    let chunk = ChatCompletionChunk {
                        id: message_id.clone(),
                        object: "chat.completion.chunk".to_string(),
                        created: chrono::Utc::now().timestamp() as u64,
                        model: model_id.clone(),
                        choices: vec![ChunkChoice {
                            index: 0,
                            delta: ChunkDelta {
                                role: None,
                                content: None,
                                tool_calls: None,
                            },
                            finish_reason: final_stop_reason.take(),
                        }],
                        usage: final_usage.take(),
                    };
                    yield Ok(chunk);
                    return;
                }
                "error" => {
                    let msg = serde_json::from_str::<Value>(&data)
                        .ok()
                        .and_then(|v| v.pointer("/error/message").and_then(|m| m.as_str()).map(|s| s.to_string()))
                        .unwrap_or_else(|| data.clone());
                    yield Err(ProxyError::ProviderError {
                        provider: provider_name.clone(),
                        status: 500,
                        message: msg,
                    });
                    return;
                }
                _ => {}
            }
        }
    }
}

fn make_text_chunk(id: &str, model: &str, text: String) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_string(),
        object: "chat.completion.chunk".to_string(),
        created: chrono::Utc::now().timestamp() as u64,
        model: model.to_string(),
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
    }
}

fn make_tool_call_chunk(
    id: &str,
    model: &str,
    index: u32,
    tool_call: ToolCall,
) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_string(),
        object: "chat.completion.chunk".to_string(),
        created: chrono::Utc::now().timestamp() as u64,
        model: model.to_string(),
        choices: vec![ChunkChoice {
            index,
            delta: ChunkDelta {
                role: None,
                content: None,
                tool_calls: Some(vec![tool_call]),
            },
            finish_reason: None,
        }],
        usage: None,
    }
}

fn parse_usage_from_message_delta(v: &Value) -> Option<Usage> {
    let output = v.pointer("/usage/output_tokens").and_then(|t| t.as_u64())? as u32;
    // input_tokens not available in message_delta; set to 0 (full usage in non-stream response)
    Some(Usage {
        prompt_tokens: 0,
        completion_tokens: output,
        total_tokens: output,
    })
}

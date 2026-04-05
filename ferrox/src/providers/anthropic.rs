use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::config::{DefaultsConfig, ProviderConfig};
use crate::error::ProxyError;
use crate::providers::anthropic_events::AnthropicEventProcessor;
use crate::providers::{parse_sse_stream, ProviderAdapter, ProviderStream};
use crate::types::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ChatMessage, Choice,
    ContentPart, FunctionCall, MessageContent, StopSequences, Usage,
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
        let extras = extract_anthropic_extras(req);
        let body = prepare_body(req, model_id, false, &extras);
        let url = format!("{}/v1/messages", self.base_url);

        let mut builder = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json");
        if let Some(beta) = &extras.beta_header {
            builder = builder.header("anthropic-beta", beta.as_str());
        }
        let resp = builder.json(&body).send().await.map_err(|e| {
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
        let extras = extract_anthropic_extras(req);
        let body = prepare_body(req, model_id, true, &extras);
        let url = format!("{}/v1/messages", self.base_url);

        let mut builder = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json");
        if let Some(beta) = &extras.beta_header {
            builder = builder.header("anthropic-beta", beta.as_str());
        }
        let resp = builder.json(&body).send().await.map_err(|e| {
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
    /// Extended thinking configuration (Anthropic-native only).
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Value>,
}

/// Anthropic-specific extras extracted from `ChatCompletionRequest`.
struct AnthropicExtras {
    /// Value of the `anthropic-beta` header to forward (may be empty).
    beta_header: Option<String>,
    /// Extended thinking config from `_anthropic_thinking` extra key.
    thinking: Option<Value>,
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

/// Extract Anthropic-specific extras that were injected into `ChatCompletionRequest`
/// by the Anthropic-native handler.
fn extract_anthropic_extras(req: &ChatCompletionRequest) -> AnthropicExtras {
    // Merge beta strings from two sources:
    // 1. `anthropic-beta` header forwarded via `extra_headers`
    // 2. `_anthropic_betas` body field forwarded via `extra`
    let mut betas: Vec<String> = Vec::new();

    if let Some(h) = req.extra_headers.get("anthropic-beta") {
        // Header may already contain comma-separated values — keep as-is.
        betas.push(h.clone());
    }
    if let Some(Value::Array(arr)) = req.extra.get("_anthropic_betas") {
        for v in arr {
            if let Some(s) = v.as_str() {
                betas.push(s.to_string());
            }
        }
    }

    let beta_header = if betas.is_empty() {
        None
    } else {
        Some(betas.join(","))
    };

    let thinking = req.extra.get("_anthropic_thinking").cloned();

    AnthropicExtras {
        beta_header,
        thinking,
    }
}

/// Return the body to send to the Anthropic API.
///
/// If the request originated from the Anthropic-native endpoint
/// (`raw_anthropic_body` is set), forward it verbatim — only `model` and
/// `stream` are overridden so the gateway's alias resolution and streaming
/// decision are respected.  This preserves every field the client sent:
/// `cache_control`, `thinking`, `service_tier`, `output_config`, tool
/// attributes (`eager_input_streaming`, `strict`, `defer_loading`), etc.
///
/// Otherwise (request came through the OpenAI-compatible endpoint and was
/// routed to the Anthropic provider) fall back to the field-by-field
/// conversion.
fn prepare_body(
    req: &ChatCompletionRequest,
    model_id: &str,
    stream: bool,
    extras: &AnthropicExtras,
) -> serde_json::Value {
    if let Some(raw) = &req.raw_anthropic_body {
        let mut body = raw.clone();
        if let Some(obj) = body.as_object_mut() {
            // Override model alias with the resolved provider model ID.
            obj.insert("model".to_string(), serde_json::json!(model_id));
            // Set stream flag from the gateway's decision (not the client's raw value).
            if stream {
                obj.insert("stream".to_string(), serde_json::json!(true));
            } else {
                obj.remove("stream");
            }
            // Remove internal-only keys that were injected for pipeline carry-through.
            obj.remove("betas"); // forwarded as header, not body
        }
        return body;
    }

    // Fallback: convert from internal OpenAI format.
    serde_json::to_value(build_request_body(req, model_id, stream, extras)).unwrap_or_default()
}

fn build_request_body(
    req: &ChatCompletionRequest,
    model_id: &str,
    stream: bool,
    extras: &AnthropicExtras,
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
        tool_choice: req
            .tool_choice
            .as_ref()
            .map(openai_tool_choice_to_anthropic),
        thinking: extras.thinking.clone(),
    }
}

/// Convert an OpenAI-format `tool_choice` value to the Anthropic format.
///
/// OpenAI strings: `"auto"` → `{"type":"auto"}`, `"required"` → `{"type":"any"}`,
/// `"none"` → `{"type":"none"}`.
/// OpenAI object: `{"type":"function","function":{"name":"foo"}}` → `{"type":"tool","name":"foo"}`.
fn openai_tool_choice_to_anthropic(tc: &Value) -> Value {
    match tc {
        Value::String(s) => match s.as_str() {
            "auto" => serde_json::json!({"type": "auto"}),
            "required" => serde_json::json!({"type": "any"}),
            "none" => serde_json::json!({"type": "none"}),
            other => serde_json::json!({"type": other}),
        },
        Value::Object(_) => {
            // OpenAI: {"type": "function", "function": {"name": "foo"}}
            // Anthropic: {"type": "tool", "name": "foo"}
            if let Some(name) = tc.pointer("/function/name").and_then(|v| v.as_str()) {
                serde_json::json!({"type": "tool", "name": name})
            } else {
                tc.clone()
            }
        }
        other => other.clone(),
    }
}

fn convert_message(msg: &ChatMessage) -> AnthropicMessage {
    let role = match msg.role.as_str() {
        "assistant" => "assistant",
        _ => "user",
    };

    let content = if let Some(tool_calls) = &msg.tool_calls {
        // Assistant message with tool calls — include any text content first,
        // then one ToolUse block per tool call.
        let mut parts: Vec<AnthropicPart> = Vec::new();

        // Prepend text content if present
        if let Some(msg_content) = &msg.content {
            let text = match msg_content {
                MessageContent::Text(t) => t.clone(),
                MessageContent::Parts(ps) => ps
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
            };
            if !text.is_empty() {
                parts.push(AnthropicPart::Text { text });
            }
        }

        for tc in tool_calls {
            parts.push(AnthropicPart::ToolUse {
                id: tc.id.clone(),
                name: tc.function.name.clone(),
                input: serde_json::from_str(&tc.function.arguments)
                    .unwrap_or(serde_json::json!({})),
            });
        }
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

        let mut processor = AnthropicEventProcessor::new(Uuid::new_v4().to_string());

        while let Some(item) = sse_stream.next().await {
            let (event_type, data) = match item {
                Ok(v) => v,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };

            let event = event_type.as_deref().unwrap_or("");
            let done = event == "message_stop" || event == "error";

            for result in processor.process(event, &data, &model_id, &provider_name) {
                let is_err = result.is_err();
                yield result;
                if is_err {
                    return;
                }
            }

            if done {
                return;
            }
        }
    }
}

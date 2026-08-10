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
        for (k, v) in &req.extra_headers {
            builder = builder.header(k.as_str(), v.as_str());
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
        for (k, v) in &req.extra_headers {
            builder = builder.header(k.as_str(), v.as_str());
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
    /// Plain string when there is no system breakpoint, or a one-element block
    /// array carrying `cache_control` when there is — Anthropic accepts both,
    /// and only the block form can hold a breakpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Value>,
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
        /// Prompt-cache breakpoint recovered from the internal content part, so
        /// a breakpoint set by an OpenAI-format client still reaches Anthropic.
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<Value>,
    },
    Image {
        source: AnthropicImageSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<Value>,
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
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicImageSource {
    /// Inline base64 image data. Required for `data:` URIs — sending those as a
    /// `url` source is rejected by api.anthropic.com.
    Base64 { media_type: String, data: String },
    /// A fetchable http(s) URL.
    Url { url: String },
}

/// Build an Anthropic image source from an OpenAI `image_url` URL, splitting
/// `data:<media>;base64,<data>` into a base64 source and passing URLs through.
fn image_url_to_source(url: &str) -> AnthropicImageSource {
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((header, data)) = rest.split_once(',') {
            let media_type = header.split(';').next().unwrap_or("image/jpeg").to_string();
            return AnthropicImageSource::Base64 {
                media_type,
                data: data.to_string(),
            };
        }
    }
    AnthropicImageSource::Url {
        url: url.to_string(),
    }
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
    let thinking = req.extra.get("_anthropic_thinking").cloned();
    AnthropicExtras { thinking }
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
    // A system breakpoint hoisted by the Anthropic→internal translation (the
    // internal `system` is a plain string and cannot carry one) is restored here
    // by emitting the system prompt in block form.
    let system = req.system_message().map(|text| {
        match req.extra.get(crate::types::ANTHROPIC_SYSTEM_CACHE_CONTROL) {
            Some(cc) => {
                serde_json::json!([{"type": "text", "text": text, "cache_control": cc}])
            }
            None => Value::String(text),
        }
    });

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

/// Pull a `cache_control` breakpoint out of an internal `extra` map, if present.
fn cache_control_of(extra: &std::collections::HashMap<String, Value>) -> Option<Value> {
    extra.get(crate::types::CACHE_CONTROL).cloned()
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
                        if let ContentPart::Text { text, .. } = p {
                            Some(text.as_str())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            };
            if !text.is_empty() {
                parts.push(AnthropicPart::Text {
                    text,
                    cache_control: None,
                });
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
                        if let ContentPart::Text { text, .. } = p {
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
                        ContentPart::Text { text, extra } => AnthropicPart::Text {
                            text: text.clone(),
                            cache_control: cache_control_of(extra),
                        },
                        ContentPart::ImageUrl { image_url, extra } => AnthropicPart::Image {
                            source: image_url_to_source(&image_url.url),
                            cache_control: cache_control_of(extra),
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
    /// Extended-thinking block. Modelled so a thinking response deserializes
    /// (previously it failed the whole response) and is surfaced as
    /// `reasoning_content`.
    Thinking {
        #[serde(default)]
        thinking: String,
    },
    /// Any other block type (e.g. redacted_thinking) — ignored, not fatal.
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
    /// Prompt-cache counters. Absent on upstreams that do not support caching,
    /// so both are optional and default to `None`.
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
}

fn anthropic_to_openai_response(resp: AnthropicResponse, model_id: &str) -> ChatCompletionResponse {
    let mut text_content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();

    for content in resp.content {
        match content {
            AnthropicResponseContent::Text { text } => {
                text_content.push_str(&text);
            }
            AnthropicResponseContent::Thinking { thinking } => {
                reasoning.push_str(&thinking);
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
            AnthropicResponseContent::Other => {}
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
        reasoning_content: if reasoning.is_empty() {
            None
        } else {
            Some(reasoning)
        },
        extra: Default::default(),
    };

    let finish_reason = resp.stop_reason.map(|r| match r.as_str() {
        "end_turn" | "stop_sequence" | "pause_turn" => "stop".to_string(),
        "max_tokens" | "model_context_window_exceeded" => "length".to_string(),
        "tool_use" => "tool_calls".to_string(),
        "refusal" => "content_filter".to_string(),
        // Unknown/future Anthropic reasons default to a valid OpenAI value.
        _ => "stop".to_string(),
    });

    let usage = resp.usage.map(|u| Usage {
        prompt_tokens: u.input_tokens,
        completion_tokens: u.output_tokens,
        total_tokens: u.input_tokens + u.output_tokens,
        extra: crate::types::cache_usage_extra(
            u.cache_creation_input_tokens,
            u.cache_read_input_tokens,
        ),
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
            extra: Default::default(),
        }],
        usage,
        system_fingerprint: None,
        extra: Default::default(),
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── Prompt-cache breakpoints, OpenAI-internal → Anthropic (#127) ─────────

    fn req_with(messages: Vec<ChatMessage>) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "m".to_string(),
            messages,
            stream: None,
            temperature: None,
            max_tokens: Some(10),
            top_p: None,
            stop: None,
            tools: None,
            tool_choice: None,
            system: None,
            extra_headers: Default::default(),
            raw_anthropic_body: None,
            extra: Default::default(),
        }
    }

    fn user_msg_with_parts(parts: Vec<crate::types::ContentPart>) -> ChatMessage {
        ChatMessage {
            role: "user".to_string(),
            content: Some(crate::types::MessageContent::Parts(parts)),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn cache_control_reaches_the_anthropic_body() {
        let ephemeral = serde_json::json!({"type": "ephemeral"});
        let req = req_with(vec![user_msg_with_parts(vec![
            crate::types::ContentPart::Text {
                text: "cached".to_string(),
                extra: std::collections::HashMap::from([(
                    "cache_control".to_string(),
                    ephemeral.clone(),
                )]),
            },
            crate::types::ContentPart::Text {
                text: "fresh".to_string(),
                extra: Default::default(),
            },
        ])]);

        let body = serde_json::to_value(build_request_body(
            &req,
            "claude-sonnet",
            false,
            &AnthropicExtras { thinking: None },
        ))
        .unwrap();

        let parts = &body["messages"][0]["content"];
        assert_eq!(parts[0]["cache_control"], ephemeral);
        assert_eq!(parts[0]["text"], "cached");
        assert!(
            parts[1].get("cache_control").is_none(),
            "unmarked part must stay unmarked: {parts:?}"
        );
    }

    #[test]
    fn system_cache_control_is_restored_as_a_block() {
        let ephemeral = serde_json::json!({"type": "ephemeral"});
        let mut req = req_with(vec![]);
        req.system = Some("You are helpful.".to_string());
        req.extra.insert(
            crate::types::ANTHROPIC_SYSTEM_CACHE_CONTROL.to_string(),
            ephemeral.clone(),
        );

        let body = serde_json::to_value(build_request_body(
            &req,
            "claude-sonnet",
            false,
            &AnthropicExtras { thinking: None },
        ))
        .unwrap();

        assert_eq!(
            body["system"],
            serde_json::json!([{
                "type": "text",
                "text": "You are helpful.",
                "cache_control": {"type": "ephemeral"}
            }])
        );
    }

    #[test]
    fn system_without_breakpoint_stays_a_plain_string() {
        // Byte-compatible with the pre-#127 wire format.
        let mut req = req_with(vec![]);
        req.system = Some("You are helpful.".to_string());

        let body = serde_json::to_value(build_request_body(
            &req,
            "claude-sonnet",
            false,
            &AnthropicExtras { thinking: None },
        ))
        .unwrap();

        assert_eq!(body["system"], serde_json::json!("You are helpful."));
    }

    #[test]
    fn parts_without_extras_serialize_without_cache_control() {
        let req = req_with(vec![user_msg_with_parts(vec![
            crate::types::ContentPart::Text {
                text: "plain".to_string(),
                extra: Default::default(),
            },
        ])]);

        let body = serde_json::to_value(build_request_body(
            &req,
            "claude-sonnet",
            false,
            &AnthropicExtras { thinking: None },
        ))
        .unwrap();

        assert_eq!(
            body["messages"][0]["content"][0],
            serde_json::json!({"type": "text", "text": "plain"})
        );
    }

    #[test]
    fn thinking_block_deserializes_and_maps_to_reasoning() {
        // Regression: a response containing a `thinking` block previously failed
        // to deserialize entirely. It must parse and surface as reasoning_content.
        let json = r#"{"id":"m","model":"glm","content":[{"type":"thinking","thinking":"reasoning here"},{"type":"text","text":"answer"}],"stop_reason":"end_turn","usage":{"input_tokens":5,"output_tokens":3}}"#;
        let resp: AnthropicResponse =
            serde_json::from_str(json).expect("thinking response must deserialize");
        let out = anthropic_to_openai_response(resp, "glm");
        let msg = &out.choices[0].message;
        assert_eq!(msg.reasoning_content.as_deref(), Some("reasoning here"));
        assert!(
            matches!(&msg.content, Some(crate::types::MessageContent::Text(t)) if t == "answer")
        );
    }

    #[test]
    fn cache_counters_land_in_usage_extra() {
        let json = r#"{"id":"m","model":"glm","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","usage":{"input_tokens":47,"output_tokens":2,"cache_creation_input_tokens":100,"cache_read_input_tokens":3968}}"#;
        let resp: AnthropicResponse = serde_json::from_str(json).unwrap();
        let out = anthropic_to_openai_response(resp, "glm");
        let usage = out.usage.expect("usage must be present");
        assert_eq!(usage.prompt_tokens, 47);
        assert_eq!(usage.extra["cache_read_input_tokens"], 3968);
        assert_eq!(usage.extra["cache_creation_input_tokens"], 100);
        // OpenAI-canonical view of the same cache reads.
        assert_eq!(usage.extra["prompt_tokens_details"]["cached_tokens"], 3968);
    }

    #[test]
    fn absent_cache_counters_leave_usage_extra_empty() {
        // A non-caching upstream must serialize exactly as it did before cache
        // support existed — no null or zero-valued keys.
        let json = r#"{"id":"m","model":"glm","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","usage":{"input_tokens":5,"output_tokens":3}}"#;
        let resp: AnthropicResponse = serde_json::from_str(json).unwrap();
        let out = anthropic_to_openai_response(resp, "glm");
        let usage = out.usage.expect("usage must be present");
        assert!(
            usage.extra.is_empty(),
            "no cache fields upstream must mean no extra keys: {:?}",
            usage.extra
        );
        assert_eq!(
            serde_json::to_string(&usage).unwrap(),
            r#"{"prompt_tokens":5,"completion_tokens":3,"total_tokens":8}"#
        );
    }

    #[test]
    fn cache_read_only_omits_creation_key() {
        let json = r#"{"id":"m","model":"glm","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","usage":{"input_tokens":47,"output_tokens":2,"cache_read_input_tokens":3968}}"#;
        let resp: AnthropicResponse = serde_json::from_str(json).unwrap();
        let usage = anthropic_to_openai_response(resp, "glm").usage.unwrap();
        assert_eq!(usage.extra["cache_read_input_tokens"], 3968);
        assert!(!usage.extra.contains_key("cache_creation_input_tokens"));
    }

    #[test]
    fn unknown_content_block_is_ignored_not_fatal() {
        let json = r#"{"id":"m","model":"x","content":[{"type":"redacted_thinking","data":"..."},{"type":"text","text":"hi"}],"stop_reason":"end_turn","usage":null}"#;
        let resp: AnthropicResponse =
            serde_json::from_str(json).expect("unknown block must not be fatal");
        let out = anthropic_to_openai_response(resp, "x");
        assert!(
            matches!(&out.choices[0].message.content, Some(crate::types::MessageContent::Text(t)) if t == "hi")
        );
    }
    #[test]
    fn data_url_image_becomes_base64_source() {
        match image_url_to_source("data:image/png;base64,QUJD") {
            AnthropicImageSource::Base64 { media_type, data } => {
                assert_eq!(media_type, "image/png");
                assert_eq!(data, "QUJD");
            }
            _ => panic!("data: URL must become a base64 source"),
        }
    }

    #[test]
    fn http_url_image_stays_url_source() {
        assert!(matches!(
            image_url_to_source("https://x/i.png"),
            AnthropicImageSource::Url { .. }
        ));
    }
}

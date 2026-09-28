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
use crate::responses_emitter::RESPONSES_THINKING_SIGNATURE;
use crate::responses_types::RESPONSES_ANTHROPIC_THINKING_BLOCKS;
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
        let extras = extract_anthropic_extras(req, model_id);
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
        let extras = extract_anthropic_extras(req, model_id);
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
    /// Extended thinking configuration: the client's own `_anthropic_thinking`,
    /// or one derived from `reasoning_effort` (see [`thinking_for_effort`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Value>,
    /// `{"effort": …}` for adaptive thinking, derived from `reasoning_effort`.
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<Value>,
}

/// Anthropic-specific extras extracted from `ChatCompletionRequest`.
#[derive(Default)]
struct AnthropicExtras {
    /// Extended thinking config from `_anthropic_thinking`, else derived from
    /// `reasoning_effort` for the target model.
    thinking: Option<Value>,
    /// `output_config` carrying the effort of derived adaptive thinking.
    output_config: Option<Value>,
    /// `max_tokens` to send when derived manual thinking needs room for its
    /// budget (the budget must be below `max_tokens`).
    max_tokens: Option<u32>,
    /// Thinking was derived from `reasoning_effort`, so the request fields
    /// Anthropic rejects alongside thinking (`temperature`, a forced
    /// `tool_choice`) are dropped. A client-supplied `_anthropic_thinking` is
    /// forwarded with the request as the client wrote it.
    derived_thinking: bool,
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
    /// A thinking block replayed from a Responses `reasoning` item, so a
    /// manual-thinking tool loop's assistant turn still starts with one.
    Thinking { thinking: String, signature: String },
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
/// by the Anthropic-native handler, or derive thinking from an OpenAI-style
/// `reasoning_effort` for `model_id` when the client set no thinking itself.
fn extract_anthropic_extras(req: &ChatCompletionRequest, model_id: &str) -> AnthropicExtras {
    if let Some(thinking) = req.extra.get("_anthropic_thinking") {
        return AnthropicExtras {
            thinking: Some(thinking.clone()),
            ..AnthropicExtras::default()
        };
    }
    req.extra
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(|effort| thinking_for_effort(effort, model_id, req.max_tokens))
        .unwrap_or_default()
}

/// How a model takes extended thinking.
#[derive(Debug, PartialEq, Eq)]
enum ThinkingStyle {
    /// `{"type":"adaptive"}` + `output_config.effort` (Claude 4.6+). Manual
    /// budgets are rejected with a 400 on 4.7+.
    Adaptive {
        /// Accepts `xhigh` (4.7+).
        xhigh: bool,
        /// Defaults `thinking.display` to `"omitted"` (4.7+), which would
        /// leave nothing to surface as reasoning — so `"summarized"` is asked
        /// for explicitly.
        omits_display: bool,
    },
    /// `{"type":"enabled","budget_tokens":N}` with `N < max_tokens`
    /// (Claude 3.7 up to 4.5).
    Manual,
}

/// Classify a model id by the thinking style it accepts, or `None` when it
/// takes no derived thinking.
///
/// Understands first-party (`claude-opus-4-7`, `claude-3-7-sonnet-20250219`),
/// Bedrock (`us.anthropic.claude-sonnet-4-6-v1:0`) and Vertex
/// (`claude-opus-4-5@20251101`) ids. The version is the first one- or
/// two-digit number and the one right after it, so a date suffix is never
/// read as a minor version. Families newer than Opus (`fable`, `mythos`) and
/// any Claude 4.7+ are adaptive. Claude before 3.7 has no extended thinking,
/// and non-Claude models behind this adapter (Z.AI GLM, Kimi) or ids that do
/// not parse get `None`: never inventing thinking is the one choice that
/// cannot turn a working request into a 400.
fn thinking_style(model_id: &str) -> Option<ThinkingStyle> {
    let lower = model_id.to_ascii_lowercase();
    let (_, rest) = lower.split_once("claude-")?;
    let mut tokens = rest.split(['-', '@', ':', '.']).peekable();
    let mut version: Option<(u32, u32)> = None;
    let mut modern_family = false;
    while let Some(tok) = tokens.next() {
        if tok == "fable" || tok == "mythos" {
            modern_family = true;
        }
        if version.is_none() && (1..=2).contains(&tok.len()) {
            if let Ok(major) = tok.parse::<u32>() {
                let minor = tokens
                    .peek()
                    .filter(|t| (1..=2).contains(&t.len()))
                    .and_then(|t| t.parse::<u32>().ok())
                    .unwrap_or(0);
                version = Some((major, minor));
            }
        }
    }
    let adaptive = |xhigh| ThinkingStyle::Adaptive {
        xhigh,
        omits_display: xhigh,
    };
    match version {
        _ if modern_family => Some(adaptive(true)),
        Some(v) if v >= (4, 7) => Some(adaptive(true)),
        Some(v) if v >= (4, 6) => Some(adaptive(false)),
        Some(v) if v >= (3, 7) => Some(ThinkingStyle::Manual),
        _ => None,
    }
}

/// Manual-mode thinking budget per effort. The API minimum is 1024.
fn manual_budget(effort: &str) -> Option<u32> {
    match effort {
        "minimal" | "low" => Some(1024),
        "medium" => Some(4096),
        "high" | "xhigh" | "max" => Some(16384),
        _ => None,
    }
}

/// Map an OpenAI-style `reasoning_effort` to Anthropic thinking for
/// `model_id`. `none`, unknown values and models without derived thinking
/// leave the request as it was.
fn thinking_for_effort(effort: &str, model_id: &str, max_tokens: Option<u32>) -> AnthropicExtras {
    match thinking_style(model_id) {
        None => AnthropicExtras::default(),
        Some(ThinkingStyle::Adaptive {
            xhigh,
            omits_display,
        }) => {
            let level = match effort {
                "minimal" | "low" => "low",
                "medium" => "medium",
                "high" => "high",
                "xhigh" if xhigh => "xhigh",
                "xhigh" => "high",
                "max" => "max",
                _ => return AnthropicExtras::default(),
            };
            let thinking = if omits_display {
                serde_json::json!({"type": "adaptive", "display": "summarized"})
            } else {
                serde_json::json!({"type": "adaptive"})
            };
            AnthropicExtras {
                thinking: Some(thinking),
                output_config: Some(serde_json::json!({ "effort": level })),
                max_tokens: None,
                derived_thinking: true,
            }
        }
        Some(ThinkingStyle::Manual) => {
            let Some(budget) = manual_budget(effort) else {
                return AnthropicExtras::default();
            };
            // `max_tokens` covers thinking and answer together, and the budget
            // must stay below it. A limit that already leaves at least half
            // for the answer is kept; a smaller one is raised by the budget so
            // the answer keeps the room the client asked for.
            let max = max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
            let max_tokens = if budget <= max / 2 { max } else { max + budget };
            AnthropicExtras {
                thinking: Some(serde_json::json!({"type": "enabled", "budget_tokens": budget})),
                output_config: None,
                max_tokens: Some(max_tokens),
                derived_thinking: true,
            }
        }
    }
}

/// Thinking blocks to replay, from the Responses translation's
/// `_responses_anthropic_thinking_blocks`, keyed by the index in
/// `req.messages` of the assistant message each one precedes.
fn replayed_thinking(req: &ChatCompletionRequest) -> Vec<(usize, AnthropicPart)> {
    let Some(Value::Array(blocks)) = req.extra.get(RESPONSES_ANTHROPIC_THINKING_BLOCKS) else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|b| {
            let index = b.get("message_index")?.as_u64()? as usize;
            let signature = b.get("signature")?.as_str()?.to_string();
            let thinking = b
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Some((
                index,
                AnthropicPart::Thinking {
                    thinking,
                    signature,
                },
            ))
        })
        .collect()
}

/// Put `leading` blocks in front of a converted message's content.
fn prepend_parts(content: AnthropicContent, mut leading: Vec<AnthropicPart>) -> AnthropicContent {
    match content {
        AnthropicContent::Text(text) => {
            if !text.is_empty() {
                leading.push(AnthropicPart::Text {
                    text,
                    cache_control: None,
                });
            }
        }
        AnthropicContent::Parts(parts) => leading.extend(parts),
    }
    AnthropicContent::Parts(leading)
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

    // Filter out system messages; Anthropic does not allow them in the messages
    // array. Replayed thinking blocks are indexed against the unfiltered list,
    // so they are attached before filtering.
    let thinking = replayed_thinking(req);
    let messages: Vec<AnthropicMessage> = req
        .messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role != "system")
        .map(|(i, m)| {
            let mut converted = convert_message(m);
            if m.role == "assistant" {
                let leading: Vec<AnthropicPart> = thinking
                    .iter()
                    .filter(|(at, _)| *at == i)
                    .map(|(_, part)| part.clone())
                    .collect();
                if !leading.is_empty() {
                    converted.content = prepend_parts(converted.content, leading);
                }
            }
            converted
        })
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

    let mut tool_choice = req
        .tool_choice
        .as_ref()
        .map(openai_tool_choice_to_anthropic);
    if extras.derived_thinking {
        // Anthropic rejects a forced tool choice while thinking; downgrade it
        // to `auto`, keeping any other setting (`disable_parallel_tool_use`).
        if let Some(Value::Object(tc)) = tool_choice.as_mut() {
            if matches!(tc.get("type").and_then(Value::as_str), Some("any" | "tool")) {
                tc.insert("type".to_string(), Value::from("auto"));
                tc.remove("name");
            }
        }
    }

    AnthropicRequest {
        model: model_id.to_string(),
        messages,
        system,
        max_tokens: extras
            .max_tokens
            .or(req.max_tokens)
            .unwrap_or(DEFAULT_MAX_TOKENS),
        stream: if stream { Some(true) } else { None },
        // While thinking, `temperature` must be left at its default and
        // `top_p` must be at least 0.95.
        temperature: req.temperature.filter(|_| !extras.derived_thinking),
        top_p: req.top_p.filter(|p| !extras.derived_thinking || *p >= 0.95),
        stop_sequences,
        tools,
        tool_choice,
        thinking: extras.thinking.clone(),
        output_config: extras.output_config.clone(),
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
        /// Needed to send the block back on a later turn.
        #[serde(default)]
        signature: Option<String>,
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
    let mut signatures: Vec<String> = Vec::new();
    let mut tool_calls = Vec::new();

    for content in resp.content {
        match content {
            AnthropicResponseContent::Text { text } => {
                text_content.push_str(&text);
            }
            AnthropicResponseContent::Thinking {
                thinking,
                signature,
            } => {
                reasoning.push_str(&thinking);
                signatures.extend(signature.filter(|s| !s.is_empty()));
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
        extra: thinking_signature_extra(signatures),
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

/// `extra` carrying the thinking signature for the Responses encoder.
///
/// The response's thinking text is joined into one `reasoning_content`, and a
/// signature only verifies the exact block it was issued for, so it is kept
/// only when the response had exactly one signed thinking block. Replaying
/// joined text under one block's signature would be rejected upstream.
fn thinking_signature_extra(
    mut signatures: Vec<String>,
) -> std::collections::HashMap<String, Value> {
    let mut extra = std::collections::HashMap::new();
    if signatures.len() == 1 {
        extra.insert(
            RESPONSES_THINKING_SIGNATURE.to_string(),
            Value::String(signatures.remove(0)),
        );
    }
    extra
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

    /// A Responses request carries `instructions` plus `developer` items, which
    /// both become system messages; every one must reach Anthropic's `system`.
    #[test]
    fn every_responses_system_message_reaches_the_anthropic_system_prompt() {
        let responses: crate::responses_types::ResponsesRequest =
            serde_json::from_value(serde_json::json!({
                "model": "m",
                "instructions": "You are Codex.",
                "input": [
                    {"type": "message", "role": "developer",
                     "content": [{"type": "input_text", "text": "<permissions>ro</permissions>"}]},
                    {"type": "message", "role": "user", "content": "hi"}
                ]
            }))
            .unwrap();
        let req = crate::responses_types::to_chat_completion_request(&responses).unwrap();
        let body = serde_json::to_value(build_request_body(
            &req,
            "claude-sonnet",
            false,
            &AnthropicExtras::default(),
        ))
        .unwrap();
        assert_eq!(
            body["system"],
            "You are Codex.\n\n<permissions>ro</permissions>"
        );
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
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
            &AnthropicExtras::default(),
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
            &AnthropicExtras::default(),
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
            &AnthropicExtras::default(),
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
            &AnthropicExtras::default(),
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

    // ── Image parts, inbound JSON → outbound Anthropic body (#158) ───────────

    fn anthropic_body_from_openai_json(json: &str) -> Value {
        let req: ChatCompletionRequest = serde_json::from_str(json).unwrap();
        serde_json::to_value(build_request_body(
            &req,
            "glm-4.6",
            false,
            &extract_anthropic_extras(&req, "glm-4.6"),
        ))
        .unwrap()
    }

    #[test]
    fn image_part_survives_into_the_anthropic_body() {
        let body = anthropic_body_from_openai_json(
            r#"{"model":"m","max_tokens":10,"messages":[{"role":"user","content":[
                {"type":"text","text":"What colour is this?"},
                {"type":"image_url","image_url":{"url":"data:image/jpeg;base64,AAAA"}}
            ]}]}"#,
        );
        assert_eq!(
            body["messages"][0]["content"][1],
            serde_json::json!({
                "type": "image",
                "source": {"type": "base64", "media_type": "image/jpeg", "data": "AAAA"}
            })
        );
    }

    #[test]
    fn url_image_part_becomes_url_source_in_the_anthropic_body() {
        let body = anthropic_body_from_openai_json(
            r#"{"model":"m","max_tokens":10,"messages":[{"role":"user","content":[
                {"type":"text","text":"What is this?"},
                {"type":"image_url","image_url":{"url":"https://example.com/a.png"}}
            ]}]}"#,
        );
        assert_eq!(
            body["messages"][0]["content"][1],
            serde_json::json!({
                "type": "image",
                "source": {"type": "url", "url": "https://example.com/a.png"}
            })
        );
    }

    /// `/anthropic/v1/messages` bodies with a base64 and a URL image block, as
    /// the handler parses them: typed for translation, raw for pass-through.
    fn anthropic_native_image_request() -> (ChatCompletionRequest, Value) {
        let json = r#"{"model":"m","max_tokens":10,"messages":[{"role":"user","content":[
            {"type":"text","text":"Compare these."},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}},
            {"type":"image","source":{"type":"url","url":"https://example.com/a.png"}}
        ]}]}"#;
        let raw: Value = serde_json::from_str(json).unwrap();
        let typed: crate::anthropic_types::AnthropicMessagesRequest =
            serde_json::from_str(json).unwrap();
        (
            crate::anthropic_types::to_chat_completion_request(typed),
            raw,
        )
    }

    #[test]
    fn anthropic_native_image_block_is_forwarded_verbatim() {
        let (mut req, mut raw) = anthropic_native_image_request();
        // A field the rebuild does not model, so this can only pass through the
        // verbatim branch — a rebuild would produce identical image blocks.
        raw["service_tier"] = serde_json::json!("auto");
        req.raw_anthropic_body = Some(raw.clone());

        let body = prepare_body(
            &req,
            "glm-4.6",
            false,
            &extract_anthropic_extras(&req, "glm-4.6"),
        );
        assert_eq!(
            body["service_tier"], "auto",
            "verbatim branch must be taken"
        );
        assert_eq!(body["messages"], raw["messages"]);
    }

    #[test]
    fn anthropic_native_image_block_survives_a_field_by_field_rebuild() {
        let (req, raw) = anthropic_native_image_request();
        assert!(req.raw_anthropic_body.is_none());

        let body = prepare_body(
            &req,
            "glm-4.6",
            false,
            &extract_anthropic_extras(&req, "glm-4.6"),
        );
        let content = &body["messages"][0]["content"];
        for i in [1, 2] {
            assert_eq!(
                content[i], raw["messages"][0]["content"][i],
                "image block {i} must rebuild byte-identically"
            );
        }
    }

    // ── Responses reasoning round-trip + reasoning_effort mapping (#183) ─────

    fn body_for(json: Value, model_id: &str) -> Value {
        let req: ChatCompletionRequest = serde_json::from_value(json).unwrap();
        let extras = extract_anthropic_extras(&req, model_id);
        serde_json::to_value(build_request_body(&req, model_id, false, &extras)).unwrap()
    }

    fn effort_body(model_id: &str, effort: &str) -> Value {
        body_for(
            serde_json::json!({"model": "a", "reasoning_effort": effort,
                "messages": [{"role": "user", "content": "hi"}]}),
            model_id,
        )
    }

    #[test]
    fn thinking_style_per_model_family() {
        let adaptive_new = Some(ThinkingStyle::Adaptive {
            xhigh: true,
            omits_display: true,
        });
        let adaptive_46 = Some(ThinkingStyle::Adaptive {
            xhigh: false,
            omits_display: false,
        });
        let manual = Some(ThinkingStyle::Manual);
        let cases = [
            ("claude-opus-4-7", &adaptive_new),
            ("claude-opus-4-8", &adaptive_new),
            ("claude-opus-5", &adaptive_new),
            ("claude-opus-5-5", &adaptive_new),
            ("claude-sonnet-5", &adaptive_new),
            ("claude-fable-5-1", &adaptive_new),
            ("claude-mythos-5-1", &adaptive_new),
            ("us.anthropic.claude-opus-4-7-v1:0", &adaptive_new),
            ("claude-opus-4-6", &adaptive_46),
            ("claude-sonnet-4-6", &adaptive_46),
            ("eu.anthropic.claude-sonnet-4-6", &adaptive_46),
            ("claude-sonnet-4-5", &manual),
            ("claude-haiku-4-5-20251001", &manual),
            ("claude-opus-4-5@20251101", &manual),
            // A date suffix is not a minor version.
            ("claude-opus-4-20250514", &manual),
            ("claude-3-7-sonnet-20250219", &manual),
            // Before 3.7 there is no extended thinking.
            ("claude-3-5-haiku-20241022", &None),
            ("claude-3-haiku-20240307", &None),
            // Non-Claude upstreams and ids that do not parse.
            ("glm-4.6", &None),
            ("glm-5.1", &None),
            ("kimi-k2", &None),
            ("kimi-for-coding", &None),
            ("claude-latest", &None),
        ];
        for (model, want) in cases {
            assert_eq!(&thinking_style(model), want, "{model}");
        }
    }

    #[test]
    fn effort_on_non_claude_models_changes_nothing() {
        let tools = serde_json::json!([{"type": "function", "function": {"name": "f",
            "parameters": {"type": "object", "properties": {}}}}]);
        for model in ["glm-4.6", "kimi-k2", "claude-3-5-haiku-20241022"] {
            let body = body_for(
                serde_json::json!({"model": "a", "reasoning_effort": "high",
                    "temperature": 0.2, "top_p": 0.5, "tools": tools,
                    "tool_choice": "required",
                    "messages": [{"role": "user", "content": "hi"}]}),
                model,
            );
            assert!(body.get("thinking").is_none(), "{model}");
            assert!(body.get("output_config").is_none(), "{model}");
            assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS, "{model}");
            assert!(body.get("temperature").is_some(), "{model}");
            assert_eq!(body["top_p"], 0.5, "{model}");
            assert_eq!(body["tool_choice"], serde_json::json!({"type": "any"}));
        }
    }

    #[test]
    fn effort_on_claude_4_7_plus_is_adaptive_with_output_config_effort() {
        for model in ["claude-opus-4-7", "claude-opus-5", "claude-fable-5-1"] {
            let body = effort_body(model, "xhigh");
            assert_eq!(
                body["thinking"],
                serde_json::json!({"type": "adaptive", "display": "summarized"}),
                "{model}"
            );
            assert_eq!(
                body["output_config"],
                serde_json::json!({"effort": "xhigh"})
            );
            assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
        }
        assert_eq!(
            effort_body("claude-opus-4-7", "minimal")["output_config"]["effort"],
            "low"
        );
    }

    #[test]
    fn effort_on_claude_4_6_is_adaptive_without_xhigh() {
        let body = effort_body("claude-sonnet-4-6", "xhigh");
        assert_eq!(body["thinking"], serde_json::json!({"type": "adaptive"}));
        assert_eq!(body["output_config"], serde_json::json!({"effort": "high"}));
        assert_eq!(
            effort_body("claude-opus-4-6", "medium")["output_config"]["effort"],
            "medium"
        );
    }

    #[test]
    fn effort_on_older_models_is_a_manual_budget_below_max_tokens() {
        // No client limit: the default is kept while it leaves half for the
        // answer, else the budget is added on top.
        for (effort, budget, max) in [
            ("low", 1024, DEFAULT_MAX_TOKENS),
            ("medium", 4096, DEFAULT_MAX_TOKENS + 4096),
            ("high", 16384, DEFAULT_MAX_TOKENS + 16384),
        ] {
            let body = effort_body("claude-sonnet-4-5", effort);
            assert_eq!(
                body["thinking"],
                serde_json::json!({"type": "enabled", "budget_tokens": budget})
            );
            assert!(body.get("output_config").is_none());
            assert_eq!(body["max_tokens"], max, "{effort}");
        }

        // A limit that leaves at least half for the answer is kept.
        let body = body_for(
            serde_json::json!({"model": "a", "reasoning_effort": "high", "max_tokens": 64000,
                "messages": [{"role": "user", "content": "hi"}]}),
            "claude-3-7-sonnet-20250219",
        );
        assert_eq!(body["thinking"]["budget_tokens"], 16384);
        assert_eq!(body["max_tokens"], 64000);

        // A small one is raised by the budget, never shrinking the answer.
        let body = body_for(
            serde_json::json!({"model": "a", "reasoning_effort": "low", "max_tokens": 1000,
                "messages": [{"role": "user", "content": "hi"}]}),
            "claude-haiku-4-5",
        );
        assert_eq!(body["thinking"]["budget_tokens"], 1024);
        assert_eq!(body["max_tokens"], 2024);
    }

    #[test]
    fn effort_none_or_unknown_leaves_thinking_off() {
        for (model, effort) in [
            ("claude-opus-4-7", "none"),
            ("claude-sonnet-4-6", "bogus"),
            ("claude-haiku-4-5", "none"),
            ("claude-haiku-4-5", "bogus"),
        ] {
            let body = effort_body(model, effort);
            assert!(body.get("thinking").is_none(), "{model}/{effort}");
            assert!(body.get("output_config").is_none(), "{model}/{effort}");
        }
    }

    #[test]
    fn derived_thinking_drops_temperature_and_forced_tool_choice() {
        let tools = serde_json::json!([{"type": "function", "function": {"name": "f",
            "parameters": {"type": "object", "properties": {}}}}]);
        for model in ["claude-opus-4-7", "claude-haiku-4-5"] {
            for forced in [
                serde_json::json!("required"),
                serde_json::json!({"type": "function", "function": {"name": "f"}}),
            ] {
                let body = body_for(
                    serde_json::json!({"model": "a", "reasoning_effort": "low",
                        "temperature": 0.2, "tools": tools, "tool_choice": forced,
                        "messages": [{"role": "user", "content": "hi"}]}),
                    model,
                );
                assert!(body.get("thinking").is_some(), "{model}");
                assert!(body.get("temperature").is_none(), "{model}");
                assert_eq!(
                    body["tool_choice"],
                    serde_json::json!({"type": "auto"}),
                    "{model}: {forced}"
                );
            }
            // `top_p` below 0.95 is rejected while thinking; at or above, kept.
            for (top_p, kept) in [(0.5, false), (0.95, true), (1.0, true)] {
                let body = body_for(
                    serde_json::json!({"model": "a", "reasoning_effort": "low", "top_p": top_p,
                        "messages": [{"role": "user", "content": "hi"}]}),
                    model,
                );
                assert_eq!(body.get("top_p").is_some(), kept, "{model}: {top_p}");
            }
            // A non-forced choice is kept.
            let body = body_for(
                serde_json::json!({"model": "a", "reasoning_effort": "low",
                    "tools": tools, "tool_choice": "none",
                    "messages": [{"role": "user", "content": "hi"}]}),
                model,
            );
            assert_eq!(body["tool_choice"], serde_json::json!({"type": "none"}));
        }
        // Without thinking, all are forwarded as before.
        let body = body_for(
            serde_json::json!({"model": "a", "temperature": 0.2, "top_p": 0.5, "tools": tools,
                "tool_choice": "required", "messages": [{"role": "user", "content": "hi"}]}),
            "claude-opus-4-7",
        );
        assert!(body.get("temperature").is_some());
        assert!(body.get("top_p").is_some());
        assert_eq!(body["tool_choice"], serde_json::json!({"type": "any"}));
    }

    #[test]
    fn explicit_anthropic_thinking_wins_over_reasoning_effort() {
        let body = body_for(
            serde_json::json!({"model": "a", "reasoning_effort": "high", "temperature": 1.0,
                "_anthropic_thinking": {"type": "enabled", "budget_tokens": 2000},
                "messages": [{"role": "user", "content": "hi"}]}),
            "claude-opus-4-7",
        );
        assert_eq!(
            body["thinking"],
            serde_json::json!({"type": "enabled", "budget_tokens": 2000})
        );
        assert!(body.get("output_config").is_none());
        assert_eq!(body["temperature"], 1.0);
    }

    fn responses_to_anthropic_body(input: Value, model_id: &str) -> Value {
        let req: crate::responses_types::ResponsesRequest = serde_json::from_value(input).unwrap();
        let chat = crate::responses_types::to_chat_completion_request(&req).unwrap();
        let extras = extract_anthropic_extras(&chat, model_id);
        serde_json::to_value(build_request_body(&chat, model_id, false, &extras)).unwrap()
    }

    #[test]
    fn responses_reasoning_item_replays_as_leading_thinking_block() {
        use crate::responses_types::encode_anthropic_thinking_signature;
        // `instructions` becomes a system message that is filtered out of the
        // Anthropic `messages`, so the replay must index the unfiltered list.
        let body = responses_to_anthropic_body(
            serde_json::json!({"model": "m", "instructions": "be brief",
                "reasoning": {"effort": "low"},
                "tools": [{"type": "function", "name": "f",
                    "parameters": {"type": "object", "properties": {}}}],
                "input": [
                {"role": "user", "content": "hi"},
                {"type": "reasoning", "id": "rs", "summary": [],
                 "content": [{"type": "reasoning_text", "text": "I should call f"}],
                 "encrypted_content": encode_anthropic_thinking_signature("sig-1")},
                {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "42"}
            ]}),
            "claude-haiku-4-5",
        );
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(
            messages[1]["content"],
            serde_json::json!([
                {"type": "thinking", "thinking": "I should call f", "signature": "sig-1"},
                {"type": "tool_use", "id": "c1", "name": "f", "input": {}}
            ])
        );
        // Manual thinking on the tool-loop turn, which is only valid because
        // the assistant turn above starts with its thinking block.
        assert_eq!(body["thinking"]["type"], "enabled");
    }

    #[test]
    fn replayed_thinking_precedes_assistant_text() {
        use crate::responses_types::encode_anthropic_thinking_signature;
        let body = responses_to_anthropic_body(
            serde_json::json!({"model": "m", "input": [
                {"role": "user", "content": "hi"},
                {"type": "reasoning", "id": "rs", "summary": [],
                 "content": [{"type": "reasoning_text", "text": "t"}],
                 "encrypted_content": encode_anthropic_thinking_signature("s")},
                {"role": "assistant", "content": "hello"},
                {"role": "user", "content": "again"}
            ]}),
            "claude-opus-4-7",
        );
        assert_eq!(
            body["messages"][1]["content"],
            serde_json::json!([
                {"type": "thinking", "thinking": "t", "signature": "s"},
                {"type": "text", "text": "hello"}
            ])
        );
        // Other messages are untouched.
        assert_eq!(body["messages"][2]["content"], "again");
    }

    #[test]
    fn replay_index_off_an_assistant_message_is_ignored() {
        // A trailing reasoning item has no assistant message to precede.
        let body = body_for(
            serde_json::json!({"model": "a", "messages": [{"role": "user", "content": "hi"}],
                "_responses_anthropic_thinking_blocks":
                    [{"message_index": 0, "thinking": "t", "signature": "s"},
                     {"message_index": 1, "thinking": "t", "signature": "s"}]}),
            "claude-opus-4-7",
        );
        assert_eq!(
            body["messages"],
            serde_json::json!([{"role": "user", "content": "hi"}])
        );
    }

    #[test]
    fn single_signed_thinking_block_carries_its_signature() {
        let json = r#"{"id":"m","model":"c","content":[{"type":"thinking","thinking":"hmm","signature":"sig-9"},{"type":"text","text":"answer"}],"stop_reason":"end_turn","usage":null}"#;
        let resp: AnthropicResponse = serde_json::from_str(json).unwrap();
        let msg = anthropic_to_openai_response(resp, "c").choices[0]
            .message
            .clone();
        assert_eq!(msg.extra[RESPONSES_THINKING_SIGNATURE], "sig-9");
        assert_eq!(msg.reasoning_content.as_deref(), Some("hmm"));
    }

    #[test]
    fn several_thinking_blocks_drop_the_signature() {
        // Their text is joined into one reasoning_content, which no single
        // block's signature verifies.
        let json = r#"{"id":"m","model":"c","content":[{"type":"thinking","thinking":"a","signature":"s1"},{"type":"tool_use","id":"t","name":"f","input":{}},{"type":"thinking","thinking":"b","signature":"s2"}],"stop_reason":"tool_use","usage":null}"#;
        let resp: AnthropicResponse = serde_json::from_str(json).unwrap();
        let msg = anthropic_to_openai_response(resp, "c").choices[0]
            .message
            .clone();
        assert!(!msg.extra.contains_key(RESPONSES_THINKING_SIGNATURE));
        assert_eq!(msg.reasoning_content.as_deref(), Some("ab"));
    }

    #[test]
    fn captured_signature_round_trips_through_responses() {
        use crate::responses_emitter::to_responses_response;
        use crate::responses_types::{
            decode_anthropic_thinking_signature, OutputItem, ResponsesRequest,
        };
        let json = r#"{"id":"m","model":"c","content":[{"type":"thinking","thinking":"plan","signature":"sig-rt"},{"type":"tool_use","id":"c1","name":"f","input":{}}],"stop_reason":"tool_use","usage":null}"#;
        let resp: AnthropicResponse = serde_json::from_str(json).unwrap();
        let req: ResponsesRequest =
            serde_json::from_value(serde_json::json!({"model": "m", "input": "go"})).unwrap();
        let out = to_responses_response(anthropic_to_openai_response(resp, "c"), &req, "resp_x");
        let OutputItem::Reasoning {
            encrypted_content, ..
        } = &out.output[0]
        else {
            panic!("first output item must be reasoning");
        };
        let enc = encrypted_content.as_deref().expect("encrypted_content");
        assert_eq!(decode_anthropic_thinking_signature(enc), Some("sig-rt"));

        // The client sends the output back as input on the next turn.
        let mut input = vec![serde_json::json!({"role": "user", "content": "go"})];
        for item in &out.output {
            input.push(serde_json::to_value(item).unwrap());
        }
        input.push(
            serde_json::json!({"type": "function_call_output", "call_id": "c1",
            "output": "ok"}),
        );
        let body = responses_to_anthropic_body(
            serde_json::json!({"model": "m", "input": input}),
            "claude-haiku-4-5",
        );
        assert_eq!(
            body["messages"][1]["content"][0],
            serde_json::json!({"type": "thinking", "thinking": "plan", "signature": "sig-rt"})
        );
    }

    #[test]
    fn streamed_signature_reaches_the_responses_reasoning_item() {
        use crate::responses_emitter::ResponsesEmitter;
        use crate::responses_types::{decode_anthropic_thinking_signature, ResponsesRequest};
        let events = [
            (
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":3}}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"think"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-s"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hi"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":1}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ];
        let req: ResponsesRequest =
            serde_json::from_value(serde_json::json!({"model": "m", "input": "go"})).unwrap();
        let mut processor = AnthropicEventProcessor::new("x".into());
        let mut emitter = ResponsesEmitter::new(&req, "resp_s");
        let mut frames = Vec::new();
        for (event, data) in events {
            for chunk in processor.process(event, data, "c", "anthropic") {
                frames.extend(emitter.on_chunk(chunk.unwrap()));
            }
        }
        frames.extend(emitter.finish());
        let done = frames
            .iter()
            .map(|f| serde_json::from_str::<Value>(&f.data).unwrap())
            .find(|v| v["type"] == "response.output_item.done" && v["item"]["type"] == "reasoning")
            .expect("reasoning item done");
        let enc = done["item"]["encrypted_content"].as_str().unwrap();
        assert_eq!(decode_anthropic_thinking_signature(enc), Some("sig-s"));
    }
}

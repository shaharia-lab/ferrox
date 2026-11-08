use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value};

use crate::config::{DefaultsConfig, ProviderConfig, ResponsesMode};
use crate::error::ProxyError;
use crate::providers::{parse_sse_stream, ProviderAdapter, ProviderStream};
use crate::responses_types::{NativeResponse, NativeResponsesEvent, NativeResponsesStream};
use crate::types::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ChatMessage,
    MessageContent, StopSequences,
};

/// Default base URL **including the API version prefix**.
///
/// `base_url` must include the version segment so that different providers
/// (e.g. OpenAI `/v1`, GLM `/v4`) can be configured without code changes.
/// The adapter appends only `/chat/completions`.
///
/// **Breaking change from < 0.3.2:** previously the default was
/// `https://api.openai.com` and the adapter appended `/v1/chat/completions`.
/// Configs that omit `base_url` are unaffected (the new default includes `/v1`),
/// but any explicit `base_url` that did not include the version must be updated.
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

// ── Adapter ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct OpenAIAdapter {
    name: String,
    api_key: String,
    base_url: String,
    client: Client,
    /// `responses: native` — serve `/v1/responses` from `{base_url}/responses`.
    native_responses: bool,
}

impl OpenAIAdapter {
    pub fn new(cfg: &ProviderConfig, defaults: &DefaultsConfig) -> Result<Self, anyhow::Error> {
        let api_key = cfg
            .api_key
            .clone()
            .ok_or_else(|| anyhow::anyhow!("OpenAI provider '{}' requires api_key", cfg.name))?;

        let base_url = cfg
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        // Guard against the most common misconfiguration: a base_url that still
        // ends with the full endpoint path.  This would produce a double-path
        // like `.../v1/chat/completions/chat/completions`.
        if base_url.ends_with("/chat/completions") {
            anyhow::bail!(
                "Provider '{}': base_url must not end with '/chat/completions' — \
                 set it to the versioned API root (e.g. 'https://api.openai.com/v1')",
                cfg.name
            );
        }

        let timeouts = cfg.timeouts.as_ref().unwrap_or(&defaults.timeouts);

        let client = Client::builder()
            .connect_timeout(Duration::from_secs(timeouts.connect_secs))
            .timeout(Duration::from_secs(timeouts.ttfb_secs + 3600))
            .build()?;

        Ok(Self {
            name: cfg.name.clone(),
            api_key,
            base_url,
            client,
            native_responses: cfg.responses == ResponsesMode::Native,
        })
    }

    /// Returns the chat completions endpoint URL for this provider.
    ///
    /// `base_url` is expected to include the API version (e.g. `/v1`, `/v4`).
    /// The adapter appends only `/chat/completions`.
    pub(crate) fn completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    /// Returns the Responses API endpoint URL (`{base_url}/responses`), used
    /// when the provider is configured with `responses: native`.
    pub(crate) fn responses_url(&self) -> String {
        format!("{}/responses", self.base_url)
    }

    /// POST a JSON body and turn an HTTP error status into a
    /// [`ProxyError::ProviderError`] carrying the upstream's body.
    async fn post_json(
        &self,
        url: &str,
        body: &impl Serialize,
    ) -> Result<reqwest::Response, ProxyError> {
        let resp = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(body)
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
        Ok(resp)
    }
}

#[async_trait]
impl ProviderAdapter for OpenAIAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    async fn chat(
        &self,
        req: &ChatCompletionRequest,
        model_id: &str,
    ) -> Result<ChatCompletionResponse, ProxyError> {
        let body = build_request_body(req, model_id, false);
        let resp = self.post_json(&self.completions_url(), &body).await?;

        let response: ChatCompletionResponse =
            resp.json().await.map_err(ProxyError::HttpClientError)?;
        Ok(response)
    }

    async fn chat_stream(
        &self,
        req: &ChatCompletionRequest,
        model_id: &str,
    ) -> Result<ProviderStream, ProxyError> {
        let body = build_request_body(req, model_id, true);
        let resp = self.post_json(&self.completions_url(), &body).await?;

        let provider_name = self.name.clone();
        let sse_stream = parse_sse_stream(resp);

        let chunk_stream = async_stream::stream! {
            futures::pin_mut!(sse_stream);
            while let Some(item) = sse_stream.next().await {
                let (_event, data) = match item {
                    Ok(v) => v,
                    Err(e) => {
                        yield Err(e);
                        return;
                    }
                };

                if data == "[DONE]" {
                    return;
                }

                match serde_json::from_str::<ChatCompletionChunk>(&data) {
                    Ok(chunk) => yield Ok(chunk),
                    // Only a frame that is not a chunk is checked for an
                    // in-band error, so a normal chunk is parsed once.
                    Err(e) => {
                        yield Err(in_band_stream_error(&provider_name, &data).unwrap_or_else(
                            || {
                                ProxyError::StreamError(format!(
                                    "Failed to parse OpenAI chunk from {}: {e}",
                                    provider_name
                                ))
                            },
                        ));
                        return;
                    }
                }
            }
        };

        Ok(Box::pin(chunk_stream))
    }

    fn supports_native_responses(&self) -> bool {
        self.native_responses
    }

    async fn responses(
        &self,
        body: &Map<String, Value>,
        model_id: &str,
    ) -> Result<NativeResponse, ProxyError> {
        let body = WithModel { body, model_id };
        let resp = self.post_json(&self.responses_url(), &body).await?;
        let bytes = resp.bytes().await.map_err(ProxyError::HttpClientError)?;
        Ok(NativeResponse::new(bytes))
    }

    async fn responses_stream(
        &self,
        body: &Map<String, Value>,
        model_id: &str,
    ) -> Result<NativeResponsesStream, ProxyError> {
        let body = WithModel { body, model_id };
        let resp = self.post_json(&self.responses_url(), &body).await?;
        let provider_name = self.name.clone();
        let events = parse_sse_stream(resp).map(move |item| {
            let (event, data) = item?;
            // A Responses upstream names every event (its own `error` event
            // included, and passed through verbatim), so only an unnamed frame
            // can be a chat-style in-band error from an OpenAI-compatible shim.
            if event.is_none() && data.contains("\"error\"") {
                if let Some(e) = in_band_stream_error(&provider_name, &data) {
                    return Err(e);
                }
            }
            Ok(NativeResponsesEvent::new(event, data))
        });
        Ok(Box::pin(events))
    }
}

/// An OpenAI-style error an upstream sends *inside* a stream it already
/// answered with 200: a `data:` frame holding `{"error":{"message":…,"code":…}}`
/// and no `choices`. Returned as a [`ProxyError::ProviderStreamError`] carrying
/// the frame and the upstream's message, so the client learns why its stream
/// ended; the status
/// is `error.code` when that is an HTTP error status (400..=599), else 502.
/// `None` for anything else, which the caller reports as a malformed frame.
fn in_band_stream_error(provider: &str, data: &str) -> Option<ProxyError> {
    let Value::Object(mut frame) = serde_json::from_str::<Value>(data).ok()? else {
        return None;
    };
    if frame.contains_key("choices") {
        return None;
    }
    let error = frame.remove("error")?;
    let message = match &error {
        Value::Null => return None,
        Value::String(s) => s.clone(),
        Value::Object(o) => match o.get("message") {
            Some(Value::String(s)) => s.clone(),
            _ => error.to_string(),
        },
        other => other.to_string(),
    };
    let status = error
        .get("code")
        .and_then(|c| match c {
            Value::Number(n) => n.as_u64(),
            Value::String(s) => s.parse().ok(),
            _ => None,
        })
        .and_then(|c| u16::try_from(c).ok())
        .filter(|c| (400..=599).contains(c))
        .unwrap_or(502);
    Some(ProxyError::ProviderStreamError {
        provider: provider.to_string(),
        status,
        message,
        body: data.to_string(),
    })
}

/// A Responses body serialized with `model` replaced by the resolved upstream
/// model, and every other field exactly as the client sent it — without
/// cloning the body per attempt.
struct WithModel<'a> {
    body: &'a Map<String, Value>,
    model_id: &'a str,
}

impl Serialize for WithModel<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let others = self.body.iter().filter(|(k, _)| k.as_str() != "model");
        let mut map = serializer.serialize_map(Some(others.clone().count() + 1))?;
        map.serialize_entry("model", self.model_id)?;
        for (k, v) in others {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

// ── Request building ─────────────────────────────────────────────────────────

#[derive(Serialize)]
struct OpenAIRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<Value>,
    /// Pass-through for standard OpenAI fields Ferrox doesn't model explicitly
    /// (response_format, seed, n, logprobs, frequency/presence_penalty, user, …),
    /// so a transparent proxy forwards them instead of silently dropping them.
    #[serde(flatten)]
    extra: std::collections::HashMap<String, Value>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

/// Keys `OpenAIRequest` serialises explicitly; excluded from the `extra`
/// flatten pass-through so a client-supplied duplicate can't emit a repeated key.
const RESERVED_REQUEST_KEYS: &[&str] = &[
    "model",
    "messages",
    "stream",
    "stream_options",
    "temperature",
    "max_tokens",
    "top_p",
    "stop",
    "tools",
    "tool_choice",
];

fn build_request_body(req: &ChatCompletionRequest, model_id: &str, stream: bool) -> OpenAIRequest {
    // Build messages: if there's a system convenience field, prepend it as a system message
    let mut messages = req.messages.clone();
    if let Some(system) = &req.system {
        // Only inject if not already present as first system message
        if messages.first().map(|m| m.role.as_str()) != Some("system") {
            messages.insert(
                0,
                ChatMessage {
                    role: "system".to_string(),
                    content: Some(MessageContent::Text(system.clone())),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    extra: Default::default(),
                },
            );
        }
    }

    let stop = req.stop.as_ref().map(|s| match s {
        StopSequences::Single(v) => Value::String(v.clone()),
        StopSequences::Multiple(v) => {
            Value::Array(v.iter().map(|s| Value::String(s.clone())).collect())
        }
    });

    let tools = req
        .tools
        .as_ref()
        .map(|t| serde_json::to_value(t).unwrap_or(Value::Null));

    OpenAIRequest {
        model: model_id.to_string(),
        messages,
        stream: if stream { Some(true) } else { None },
        // Always inject stream_options when streaming so OpenAI includes token counts
        stream_options: if stream {
            Some(StreamOptions {
                include_usage: true,
            })
        } else {
            None
        },
        temperature: req.temperature,
        max_tokens: req.max_tokens,
        top_p: req.top_p,
        stop,
        tools,
        tool_choice: req.tool_choice.clone(),
        // Forward client-supplied pass-through fields. Internal `_`-prefixed keys
        // (e.g. `_anthropic_thinking`) are gateway-private, and keys OpenAIRequest
        // sets explicitly are skipped so `#[serde(flatten)]` can't emit a duplicate
        // JSON key (e.g. a client-sent `stream_options`).
        extra: req
            .extra
            .iter()
            .filter(|(k, _)| !k.starts_with('_') && !RESERVED_REQUEST_KEYS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DefaultsConfig, ProviderConfig, ProviderType};

    /// The path from the original report: `/anthropic/v1/messages` routed to a
    /// `type: openai` provider. The breakpoint must reach the outbound body.
    #[test]
    fn cache_control_survives_into_the_openai_body() {
        let ephemeral = serde_json::json!({"type": "ephemeral"});
        let req = ChatCompletionRequest {
            model: "m".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: Some(MessageContent::Parts(vec![
                    crate::types::ContentPart::Text {
                        text: "cached".to_string(),
                        extra: std::collections::HashMap::from([(
                            "cache_control".to_string(),
                            ephemeral.clone(),
                        )]),
                    },
                ])),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                extra: std::collections::HashMap::from([(
                    "cache_control".to_string(),
                    ephemeral.clone(),
                )]),
            }],
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
        };

        let body = serde_json::to_value(build_request_body(&req, "kimi-k2", false)).unwrap();
        let msg = &body["messages"][0];
        assert_eq!(msg["cache_control"], ephemeral, "message-level breakpoint");
        assert_eq!(
            msg["content"][0]["cache_control"], ephemeral,
            "block-level breakpoint"
        );
    }

    #[test]
    fn hoisted_cache_control_keys_do_not_leak_to_openai_upstreams() {
        // The hoisted system and tool breakpoints are gateway-private; the `_`
        // prefix is what keeps them out of the outbound body. Pin that
        // explicitly, since a rename without the prefix would silently leak.
        let mut req = ChatCompletionRequest {
            model: "m".to_string(),
            messages: vec![],
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
        };
        let keys = [
            crate::types::ANTHROPIC_SYSTEM_CACHE_CONTROL,
            crate::types::ANTHROPIC_TOOLS_CACHE_CONTROL,
        ];
        for key in keys {
            req.extra
                .insert(key.to_string(), serde_json::json!({"type": "ephemeral"}));
        }

        let body = serde_json::to_value(build_request_body(&req, "kimi-k2", false)).unwrap();
        for key in keys {
            assert!(
                body.get(key).is_none(),
                "gateway-private key {key} leaked upstream: {body}"
            );
        }
    }

    #[test]
    fn message_without_extras_serializes_without_new_keys() {
        let req = ChatCompletionRequest {
            model: "m".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: Some(MessageContent::Text("hi".to_string())),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                extra: Default::default(),
            }],
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
        };

        let body = serde_json::to_value(build_request_body(&req, "kimi-k2", false)).unwrap();
        assert_eq!(
            body["messages"][0],
            serde_json::json!({"role": "user", "content": "hi"})
        );
    }

    // ── Unset optional message fields are omitted, not sent as null (#159) ───

    #[test]
    fn minimal_user_message_omits_unset_optional_fields() {
        // Groq and Google's OpenAI-compatible endpoint 400 on `"name": null`.
        let body = openai_body_from_json(
            r#"{"model":"m","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#,
        );
        let message = body["messages"][0].as_object().unwrap();
        for key in ["name", "tool_calls", "tool_call_id"] {
            assert!(
                !message.contains_key(key),
                "unset {key} must be omitted, not sent as null: {message:?}"
            );
        }
        assert_eq!(
            body["messages"][0],
            serde_json::json!({"role": "user", "content": "hi"})
        );
    }

    #[test]
    fn set_optional_message_fields_round_trip() {
        let body = openai_body_from_json(
            r#"{"model":"m","messages":[
                {"role":"user","content":"hi","name":"alice"},
                {"role":"assistant","content":null,"tool_calls":[
                    {"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{}"}}
                ]},
                {"role":"tool","content":"42","tool_call_id":"call_1"}
            ]}"#,
        );
        assert_eq!(body["messages"][0]["name"], "alice");
        assert_eq!(
            body["messages"][1]["tool_calls"],
            serde_json::json!([
                {"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{}"}}
            ])
        );
        assert_eq!(body["messages"][2]["tool_call_id"], "call_1");
    }

    // ── Image parts, inbound JSON → outbound OpenAI body (#158) ──────────────

    fn openai_body_from_json(json: &str) -> Value {
        let req: ChatCompletionRequest = serde_json::from_str(json).unwrap();
        serde_json::to_value(build_request_body(&req, "glm-4.5v", false)).unwrap()
    }

    #[test]
    fn image_part_survives_into_the_openai_body() {
        let body = openai_body_from_json(
            r#"{"model":"m","max_tokens":10,"messages":[{"role":"user","content":[
                {"type":"text","text":"What colour is this?"},
                {"type":"image_url","image_url":{"url":"data:image/jpeg;base64,AAAA"}}
            ]}]}"#,
        );
        let part = &body["messages"][0]["content"][1];
        assert_eq!(part["type"], "image_url");
        assert_eq!(part["image_url"]["url"], "data:image/jpeg;base64,AAAA");
        assert!(
            part["image_url"].get("detail").is_none(),
            "unset detail must be omitted, not sent as null: {part}"
        );
    }

    #[test]
    fn url_image_part_survives_into_the_openai_body() {
        let body = openai_body_from_json(
            r#"{"model":"m","messages":[{"role":"user","content":[
                {"type":"text","text":"What is this?"},
                {"type":"image_url","image_url":{"url":"https://example.com/a.png"}}
            ]}]}"#,
        );
        assert_eq!(
            body["messages"][0]["content"][1]["image_url"],
            serde_json::json!({"url": "https://example.com/a.png"})
        );
    }

    #[test]
    fn client_supplied_image_detail_round_trips() {
        let body = openai_body_from_json(
            r#"{"model":"m","messages":[{"role":"user","content":[
                {"type":"image_url","image_url":{"url":"https://example.com/a.png","detail":"high"}}
            ]}]}"#,
        );
        assert_eq!(
            body["messages"][0]["content"][0]["image_url"],
            serde_json::json!({"url": "https://example.com/a.png", "detail": "high"})
        );
    }

    /// `/anthropic/v1/messages` routed to a `type: openai` provider: the native
    /// image block is rebuilt as an `image_url` part carrying a `data:` URL.
    #[test]
    fn anthropic_native_image_block_reaches_the_openai_body() {
        let anthropic: crate::anthropic_types::AnthropicMessagesRequest = serde_json::from_str(
            r#"{"model":"m","max_tokens":10,"messages":[{"role":"user","content":[
                {"type":"text","text":"What colour is this?"},
                {"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":"AAAA"}}
            ]}]}"#,
        )
        .unwrap();
        let req = crate::anthropic_types::to_chat_completion_request(anthropic);

        let body = serde_json::to_value(build_request_body(&req, "k3", false)).unwrap();
        assert_eq!(
            body["messages"][0]["content"][1],
            serde_json::json!({
                "type": "image_url",
                "image_url": {"url": "data:image/jpeg;base64,AAAA"}
            })
        );
    }

    fn defaults() -> DefaultsConfig {
        DefaultsConfig::default()
    }

    fn provider_cfg(base_url: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            name: "test".to_string(),
            provider_type: ProviderType::OpenAI,
            api_key: Some("sk-test".to_string()),
            base_url: base_url.map(str::to_string),
            aws: None,
            timeouts: None,
            circuit_breaker: None,
            responses: Default::default(),
        }
    }

    #[test]
    fn default_base_url_includes_v1() {
        let adapter = OpenAIAdapter::new(&provider_cfg(None), &defaults()).unwrap();
        assert_eq!(
            adapter.completions_url(),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn explicit_openai_base_url_with_v1() {
        let adapter = OpenAIAdapter::new(
            &provider_cfg(Some("https://api.openai.com/v1")),
            &defaults(),
        )
        .unwrap();
        assert_eq!(
            adapter.completions_url(),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn glm_v4_base_url() {
        let adapter = OpenAIAdapter::new(
            &provider_cfg(Some("https://api.z.ai/api/paas/v4")),
            &defaults(),
        )
        .unwrap();
        assert_eq!(
            adapter.completions_url(),
            "https://api.z.ai/api/paas/v4/chat/completions"
        );
    }

    #[test]
    fn extra_fields_pass_through_but_internal_keys_dropped() {
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            "response_format".to_string(),
            serde_json::json!({"type":"json_object"}),
        );
        extra.insert("seed".to_string(), serde_json::json!(42));
        extra.insert(
            "_anthropic_thinking".to_string(),
            serde_json::json!({"type":"enabled"}),
        );
        let req = ChatCompletionRequest {
            model: "m".to_string(),
            messages: vec![],
            stream: None,
            temperature: None,
            max_tokens: None,
            top_p: None,
            stop: None,
            tools: None,
            tool_choice: None,
            system: None,
            extra_headers: Default::default(),
            raw_anthropic_body: None,
            extra,
        };
        let json = serde_json::to_value(build_request_body(&req, "m", false)).unwrap();
        assert_eq!(json["response_format"]["type"], "json_object");
        assert_eq!(json["seed"], 42);
        assert!(
            json.get("_anthropic_thinking").is_none(),
            "internal `_`-prefixed keys must not leak upstream"
        );
    }

    /// A translated Responses request keeps its Responses-only knobs under
    /// `_responses_*` keys; none of them may reach an OpenAI-format upstream,
    /// while the real Chat Completions fields it maps to must.
    #[test]
    fn responses_private_keys_are_stripped_from_the_upstream_body() {
        let req: crate::responses_types::ResponsesRequest =
            serde_json::from_value(serde_json::json!({
                "model": "m",
                "input": "hi",
                "tools": [
                    {"type": "function", "name": "f", "strict": true, "parameters": {}},
                    {"type": "custom", "name": "apply_patch"}
                ],
                "reasoning": {"effort": "low", "summary": "auto"},
                "include": ["reasoning.encrypted_content"],
                "text": {"verbosity": "low", "format": {"type": "json_object"}},
                "truncation": "auto",
                "max_tool_calls": 3,
                "prompt_cache_key": "k"
            }))
            .unwrap();
        let chat = crate::responses_types::to_chat_completion_request(&req).unwrap();
        assert!(chat.extra.keys().any(|k| k.starts_with("_responses_")));

        let json = serde_json::to_value(build_request_body(&chat, "m", false)).unwrap();
        let leaked: Vec<&String> = json
            .as_object()
            .unwrap()
            .keys()
            .filter(|k| k.starts_with('_'))
            .collect();
        assert!(
            leaked.is_empty(),
            "private keys leaked upstream: {leaked:?}"
        );
        assert_eq!(json["reasoning_effort"], "low");
        assert_eq!(json["response_format"]["type"], "json_object");
        assert_eq!(json["prompt_cache_key"], "k");
        assert_eq!(json["tools"][0]["function"]["strict"], true);
        assert!(json["tools"][1]["function"].get("strict").is_none());
    }

    #[test]
    fn client_reserved_key_does_not_duplicate() {
        // A client-sent `stream_options` must not collide with the one we set.
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            "stream_options".to_string(),
            serde_json::json!({"include_usage": false}),
        );
        let req = ChatCompletionRequest {
            model: "m".to_string(),
            messages: vec![],
            stream: None,
            temperature: None,
            max_tokens: None,
            top_p: None,
            stop: None,
            tools: None,
            tool_choice: None,
            system: None,
            extra_headers: Default::default(),
            raw_anthropic_body: None,
            extra,
        };
        // Serialising must not panic on a duplicate key, and streaming keeps our value.
        let json = serde_json::to_value(build_request_body(&req, "m", true)).unwrap();
        assert_eq!(json["stream_options"]["include_usage"], true);
    }

    #[test]
    fn glm_coding_base_url() {
        let adapter = OpenAIAdapter::new(
            &provider_cfg(Some("https://api.z.ai/api/coding/paas/v4")),
            &defaults(),
        )
        .unwrap();
        assert_eq!(
            adapter.completions_url(),
            "https://api.z.ai/api/coding/paas/v4/chat/completions"
        );
    }

    #[test]
    fn custom_openai_compatible_base_url() {
        let adapter = OpenAIAdapter::new(
            &provider_cfg(Some("http://localhost:11434/v1")),
            &defaults(),
        )
        .unwrap();
        assert_eq!(
            adapter.completions_url(),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn rejects_base_url_ending_with_full_endpoint_path() {
        let err = OpenAIAdapter::new(
            &provider_cfg(Some("https://api.openai.com/v1/chat/completions")),
            &defaults(),
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("must not end with '/chat/completions'"));
    }

    #[test]
    fn rejects_base_url_without_version_that_ends_in_chat_completions() {
        // Catches the old-style misconfiguration where someone pasted the full URL
        let err = OpenAIAdapter::new(
            &provider_cfg(Some("https://api.z.ai/api/paas/v4/chat/completions")),
            &defaults(),
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("must not end with '/chat/completions'"));
    }

    #[test]
    fn responses_url_appends_responses_to_the_versioned_root() {
        let adapter = OpenAIAdapter::new(
            &provider_cfg(Some("https://api.moonshot.ai/v1")),
            &defaults(),
        )
        .unwrap();
        assert_eq!(
            adapter.responses_url(),
            "https://api.moonshot.ai/v1/responses"
        );
    }

    #[test]
    fn native_responses_is_opt_in() {
        let adapter = OpenAIAdapter::new(&provider_cfg(None), &defaults()).unwrap();
        assert!(!adapter.supports_native_responses());
        let mut cfg = provider_cfg(None);
        cfg.responses = ResponsesMode::Native;
        let adapter = OpenAIAdapter::new(&cfg, &defaults()).unwrap();
        assert!(adapter.supports_native_responses());
    }

    #[test]
    fn with_model_replaces_only_the_model() {
        let body = serde_json::json!({
            "model": "alias", "input": "hi", "tools": [{"type": "web_search"}],
            "x_new": {"a": [1, 2]}
        });
        let out = serde_json::to_value(WithModel {
            body: body.as_object().unwrap(),
            model_id: "up-v1",
        })
        .unwrap();
        let mut expected = body.clone();
        expected["model"] = serde_json::json!("up-v1");
        assert_eq!(out, expected);

        // A body without `model` gains it.
        let out = serde_json::to_value(WithModel {
            body: serde_json::json!({"input": "hi"}).as_object().unwrap(),
            model_id: "up-v1",
        })
        .unwrap();
        assert_eq!(out, serde_json::json!({"model": "up-v1", "input": "hi"}));
    }

    // ── In-band stream errors ────────────────────────────────────────────────

    /// The frames an OpenAI-compatible shim sends when it fails after it has
    /// already answered 200: one ordinary chunk, then an error object, then
    /// `[DONE]`.
    const CHUNK_THEN_ERROR: &str = concat!(
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"m\",",
        "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},",
        "\"finish_reason\":null}]}\n\n",
        "data: {\"error\":{\"message\":\"Claude stopped after reaching the maximum number of ",
        "turns without producing a reply\",\"type\":\"server_error\",\"code\":null}}\n\n",
        "data: [DONE]\n\n",
    );

    /// An adapter whose upstream answers every POST to `route` with `body`
    /// as an SSE stream.
    async fn adapter_streaming(
        route: &str,
        body: &str,
        native: bool,
    ) -> (wiremock::MockServer, OpenAIAdapter) {
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("POST"))
            .and(path(format!("/v1{route}")))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_raw(body.as_bytes().to_vec(), "text/event-stream"),
            )
            .mount(&server)
            .await;
        let mut cfg = provider_cfg(Some(&format!("{}/v1", server.uri())));
        if native {
            cfg.responses = ResponsesMode::Native;
        }
        let adapter = OpenAIAdapter::new(&cfg, &defaults()).unwrap();
        (server, adapter)
    }

    fn stream_request() -> ChatCompletionRequest {
        serde_json::from_str(
            r#"{"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn chat_stream_surfaces_an_in_band_error_with_the_upstream_message() {
        let (_server, adapter) =
            adapter_streaming("/chat/completions", CHUNK_THEN_ERROR, false).await;
        let items: Vec<_> = adapter
            .chat_stream(&stream_request(), "m")
            .await
            .unwrap()
            .collect()
            .await;

        assert_eq!(items.len(), 2, "the chunk, then the error, then the end");
        let chunk = items[0].as_ref().expect("the chunk before the error");
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hi"));
        match &items[1] {
            Err(ProxyError::ProviderStreamError {
                provider,
                status,
                message,
                body,
            }) => {
                assert_eq!(provider, "test");
                assert_eq!(*status, 502, "a null code falls back to 502");
                assert_eq!(
                    message,
                    "Claude stopped after reaching the maximum number of turns without producing a reply"
                );
                // The frame exactly as the upstream sent it.
                let sent = CHUNK_THEN_ERROR.split("\n\n").nth(1).unwrap();
                assert_eq!(Some(body.as_str()), sent.strip_prefix("data: "));
            }
            other => panic!("expected a ProviderStreamError, got {other:?}"),
        }
    }

    #[test]
    fn in_band_error_status_comes_from_an_http_error_code() {
        let status = |data: &str| match in_band_stream_error("p", data) {
            Some(ProxyError::ProviderStreamError { status, .. }) => status,
            other => panic!("expected a ProviderStreamError, got {other:?}"),
        };
        assert_eq!(status(r#"{"error":{"message":"m","code":429}}"#), 429);
        assert_eq!(status(r#"{"error":{"message":"m","code":"503"}}"#), 503);
        assert_eq!(status(r#"{"error":{"message":"m","code":200}}"#), 502);
        assert_eq!(
            status(r#"{"error":{"message":"m","code":"bad_thing"}}"#),
            502
        );
        assert_eq!(status(r#"{"error":{"message":"m","code":70000}}"#), 502);
        assert_eq!(status(r#"{"error":"plain"}"#), 502);
    }

    #[test]
    fn only_an_error_object_is_an_in_band_error() {
        // Malformed JSON, a chunk, and a frame without `error` all stay
        // malformed-frame StreamErrors at the caller.
        for data in [
            "{not json",
            r#"{"choices":[],"error":{"message":"m"}}"#,
            r#"{"something":"else"}"#,
            r#"{"error":null}"#,
            r#"["error"]"#,
        ] {
            assert!(in_band_stream_error("p", data).is_none(), "{data}");
        }
    }

    #[tokio::test]
    async fn chat_stream_malformed_frame_stays_a_stream_error() {
        let (_server, adapter) =
            adapter_streaming("/chat/completions", "data: {not json\n\n", false).await;
        let items: Vec<_> = adapter
            .chat_stream(&stream_request(), "m")
            .await
            .unwrap()
            .collect()
            .await;
        assert!(
            matches!(items.as_slice(), [Err(ProxyError::StreamError(_))]),
            "{items:?}"
        );
    }

    #[tokio::test]
    async fn native_responses_stream_surfaces_an_unnamed_in_band_error() {
        let body = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"sequence_number\":0}\n\n",
            "data: {\"error\":{\"message\":\"upstream gave up\",\"code\":503}}\n\n",
        );
        let (_server, adapter) = adapter_streaming("/responses", body, true).await;
        let items: Vec<_> = adapter
            .responses_stream(
                serde_json::json!({"input": "hi", "stream": true})
                    .as_object()
                    .unwrap(),
                "m",
            )
            .await
            .unwrap()
            .collect()
            .await;
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].as_ref().unwrap().event.as_deref(),
            Some("response.created")
        );
        assert!(
            matches!(
                &items[1],
                Err(ProxyError::ProviderStreamError { status: 503, message, .. }) if message == "upstream gave up"
            ),
            "{:?}",
            items[1]
        );
    }

    #[tokio::test]
    async fn native_responses_error_event_passes_through_verbatim() {
        let data = r#"{"type":"error","code":"server_error","message":"boom","sequence_number":1}"#;
        let body = format!("event: error\ndata: {data}\n\n");
        let (_server, adapter) = adapter_streaming("/responses", &body, true).await;
        let items: Vec<_> = adapter
            .responses_stream(serde_json::json!({"input": "hi"}).as_object().unwrap(), "m")
            .await
            .unwrap()
            .collect()
            .await;
        let event = items[0].as_ref().unwrap();
        assert_eq!(event.event.as_deref(), Some("error"));
        assert_eq!(event.data, data);
    }
}

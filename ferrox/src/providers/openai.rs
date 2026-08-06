use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::Serialize;
use serde_json::Value;

use crate::config::{DefaultsConfig, ProviderConfig};
use crate::error::ProxyError;
use crate::providers::{parse_sse_stream, ProviderAdapter, ProviderStream};
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
        })
    }

    /// Returns the chat completions endpoint URL for this provider.
    ///
    /// `base_url` is expected to include the API version (e.g. `/v1`, `/v4`).
    /// The adapter appends only `/chat/completions`.
    pub(crate) fn completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
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
        let url = self.completions_url();

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
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
        let url = self.completions_url();

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
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
                    Err(e) => {
                        yield Err(ProxyError::StreamError(format!(
                            "Failed to parse OpenAI chunk from {}: {e}",
                            provider_name
                        )));
                        return;
                    }
                }
            }
        };

        Ok(Box::pin(chunk_stream))
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
            retry: None,
            circuit_breaker: None,
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
}

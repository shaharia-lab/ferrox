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

const DEFAULT_BASE_URL: &str = "https://api.openai.com";

// ── Adapter ──────────────────────────────────────────────────────────────────

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

        let timeouts = cfg.timeouts.as_ref().unwrap_or(&defaults.timeouts);

        let client = Client::builder()
            .connect_timeout(Duration::from_secs(timeouts.connect_secs))
            .timeout(Duration::from_secs(timeouts.ttfb_secs + 3600))
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
        let url = format!("{}/v1/chat/completions", self.base_url);

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
        let url = format!("{}/v1/chat/completions", self.base_url);

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
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

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
    }
}

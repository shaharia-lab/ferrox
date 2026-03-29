pub mod anthropic;
pub mod bedrock;
pub mod gemini;
pub mod openai;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use reqwest::Response;

use crate::config::{DefaultsConfig, ProviderConfig, ProviderType};
use crate::error::ProxyError;
use crate::types::{ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse};

// ── ProviderAdapter trait ────────────────────────────────────────────────────

pub type ProviderStream = BoxStream<'static, Result<ChatCompletionChunk, ProxyError>>;

#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    fn name(&self) -> &str;

    async fn chat(
        &self,
        req: &ChatCompletionRequest,
        model_id: &str,
    ) -> Result<ChatCompletionResponse, ProxyError>;

    async fn chat_stream(
        &self,
        req: &ChatCompletionRequest,
        model_id: &str,
    ) -> Result<ProviderStream, ProxyError>;
}

// ── Registry ─────────────────────────────────────────────────────────────────

pub type ProviderRegistry = HashMap<String, Arc<dyn ProviderAdapter>>;

pub async fn build_registry(
    providers: &[ProviderConfig],
    defaults: &DefaultsConfig,
) -> Result<ProviderRegistry, anyhow::Error> {
    let mut registry = ProviderRegistry::new();

    for cfg in providers {
        let adapter: Arc<dyn ProviderAdapter> = match cfg.provider_type {
            ProviderType::Anthropic => Arc::new(
                anthropic::AnthropicAdapter::new(cfg, defaults).with_context(|| {
                    format!("Failed to build Anthropic provider '{}'", cfg.name)
                })?,
            ),
            ProviderType::OpenAI | ProviderType::Glm => Arc::new(
                openai::OpenAIAdapter::new(cfg, defaults)
                    .with_context(|| format!("Failed to build OpenAI provider '{}'", cfg.name))?,
            ),
            ProviderType::Gemini => Arc::new(
                gemini::GeminiAdapter::new(cfg, defaults)
                    .with_context(|| format!("Failed to build Gemini provider '{}'", cfg.name))?,
            ),
            ProviderType::Bedrock => Arc::new(
                bedrock::BedrockAdapter::new(cfg, defaults)
                    .await
                    .with_context(|| format!("Failed to build Bedrock provider '{}'", cfg.name))?,
            ),
        };
        registry.insert(cfg.name.clone(), adapter);
    }

    Ok(registry)
}

// ── SSE parsing utility ──────────────────────────────────────────────────────

/// Parse a raw byte stream from an HTTP response into `(event_type, data)` pairs.
/// Handles multi-line accumulation and `event:` / `data:` fields per SSE spec.
pub fn parse_sse_stream(
    response: Response,
) -> impl futures::Stream<Item = Result<(Option<String>, String), ProxyError>> + Send + 'static {
    let byte_stream = response.bytes_stream();

    async_stream::stream! {
        let mut buffer = String::new();
        let mut current_event: Option<String> = None;
        let mut current_data = String::new();

        futures::pin_mut!(byte_stream);

        while let Some(chunk) = byte_stream.next().await {
            let chunk = match chunk {
                Ok(b) => b,
                Err(e) => {
                    yield Err(ProxyError::StreamError(e.to_string()));
                    return;
                }
            };

            let text = match std::str::from_utf8(&chunk) {
                Ok(s) => s.to_string(),
                Err(e) => {
                    yield Err(ProxyError::StreamError(format!("UTF-8 decode error: {e}")));
                    return;
                }
            };

            buffer.push_str(&text);

            // Process complete lines
            while let Some(newline_pos) = buffer.find('\n') {
                let line = buffer[..newline_pos].trim_end_matches('\r').to_string();
                buffer = buffer[newline_pos + 1..].to_string();

                if line.is_empty() {
                    // Empty line = dispatch event
                    if !current_data.is_empty() {
                        let data = current_data.trim_end_matches('\n').to_string();
                        yield Ok((current_event.take(), data));
                        current_data.clear();
                    }
                } else if let Some(val) = line.strip_prefix("event:") {
                    current_event = Some(val.trim().to_string());
                } else if let Some(val) = line.strip_prefix("data:") {
                    if !current_data.is_empty() {
                        current_data.push('\n');
                    }
                    current_data.push_str(val.trim());
                }
                // Ignore other fields (id:, retry:, comments)
            }
        }

        // Flush any remaining data
        if !current_data.is_empty() {
            let data = current_data.trim_end_matches('\n').to_string();
            yield Ok((current_event, data));
        }
    }
}

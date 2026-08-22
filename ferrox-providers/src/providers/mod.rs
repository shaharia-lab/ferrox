#[cfg(feature = "anthropic")]
pub mod anthropic;
#[cfg(any(feature = "anthropic", feature = "bedrock"))]
pub mod anthropic_events;
#[cfg(feature = "bedrock")]
pub mod bedrock;
#[cfg(feature = "gemini")]
pub mod gemini;
#[cfg(feature = "openai")]
pub mod openai;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use eventsource_stream::Eventsource;
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
            #[cfg(feature = "anthropic")]
            ProviderType::Anthropic => Arc::new(
                anthropic::AnthropicAdapter::new(cfg, defaults).with_context(|| {
                    format!("Failed to build Anthropic provider '{}'", cfg.name)
                })?,
            ),
            #[cfg(feature = "openai")]
            ProviderType::OpenAI | ProviderType::Glm => Arc::new(
                openai::OpenAIAdapter::new(cfg, defaults)
                    .with_context(|| format!("Failed to build OpenAI provider '{}'", cfg.name))?,
            ),
            #[cfg(feature = "gemini")]
            ProviderType::Gemini => Arc::new(
                gemini::GeminiAdapter::new(cfg, defaults)
                    .with_context(|| format!("Failed to build Gemini provider '{}'", cfg.name))?,
            ),
            #[cfg(feature = "bedrock")]
            ProviderType::Bedrock => Arc::new(
                bedrock::BedrockAdapter::new(cfg, defaults)
                    .await
                    .with_context(|| format!("Failed to build Bedrock provider '{}'", cfg.name))?,
            ),

            // A provider type whose adapter was compiled out. Fails at registry
            // build time with an actionable message rather than at the call site.
            #[allow(unreachable_patterns)]
            ref other => anyhow::bail!(
                "provider '{}' has type {:?}, but ferrox-providers was built without the \
                 corresponding feature — enable it to use this provider",
                cfg.name,
                other
            ),
        };
        registry.insert(cfg.name.clone(), adapter);
    }

    Ok(registry)
}

// ── SSE parsing utility ──────────────────────────────────────────────────────

/// Parse a raw byte stream from an HTTP response into `(event_type, data)` pairs.
///
/// Uses `eventsource-stream` for spec-compliant SSE parsing, including correct
/// handling of chunk boundaries that span multi-byte UTF-8 characters.
///
/// The `event_type` is `Some(name)` when an explicit `event:` field was present
/// in the SSE frame, or `None` when the default "message" type applies.
pub fn parse_sse_stream(
    response: Response,
) -> impl futures::Stream<Item = Result<(Option<String>, String), ProxyError>> + Send + 'static {
    response.bytes_stream().eventsource().map(|result| {
        result
            .map(|event| {
                // eventsource-stream uses "message" as the default event name
                // (per the SSE spec) when no `event:` field is present.
                // Normalise back to None to preserve the existing adapter API.
                let event_type = if event.event == "message" {
                    None
                } else {
                    Some(event.event)
                };
                (event_type, event.data)
            })
            .map_err(|e| ProxyError::StreamError(e.to_string()))
    })
}

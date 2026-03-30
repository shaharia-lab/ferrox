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
    ChunkChoice, ChunkDelta, ContentPart, MessageContent, StopSequences, Usage,
};

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

// ── Adapter ──────────────────────────────────────────────────────────────────

pub struct GeminiAdapter {
    name: String,
    api_key: String,
    base_url: String,
    client: Client,
}

impl GeminiAdapter {
    pub fn new(cfg: &ProviderConfig, defaults: &DefaultsConfig) -> Result<Self, anyhow::Error> {
        let api_key = cfg
            .api_key
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Gemini provider '{}' requires api_key", cfg.name))?;

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
impl ProviderAdapter for GeminiAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    async fn chat(
        &self,
        req: &ChatCompletionRequest,
        model_id: &str,
    ) -> Result<ChatCompletionResponse, ProxyError> {
        let body = build_request_body(req);
        let url = format!(
            "{}/v1beta/models/{}:generateContent?key={}",
            self.base_url, model_id, self.api_key
        );

        let resp = self
            .client
            .post(&url)
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

        let gemini_resp: GeminiResponse = resp.json().await.map_err(ProxyError::HttpClientError)?;
        Ok(gemini_to_openai_response(gemini_resp, model_id))
    }

    async fn chat_stream(
        &self,
        req: &ChatCompletionRequest,
        model_id: &str,
    ) -> Result<ProviderStream, ProxyError> {
        let body = build_request_body(req);
        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?key={}&alt=sse",
            self.base_url, model_id, self.api_key
        );

        let resp = self
            .client
            .post(&url)
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
        let model_id = model_id.to_string();
        let msg_id = Uuid::new_v4().to_string();

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

                let gemini_resp: GeminiResponse = match serde_json::from_str(&data) {
                    Ok(r) => r,
                    Err(e) => {
                        yield Err(ProxyError::StreamError(format!(
                            "Failed to parse Gemini chunk from {}: {e}",
                            provider_name
                        )));
                        return;
                    }
                };

                let usage = gemini_resp.usage_metadata.as_ref().map(|u| Usage {
                    prompt_tokens: u.prompt_token_count,
                    completion_tokens: u.candidates_token_count.unwrap_or(0),
                    total_tokens: u.total_token_count,
                });

                let candidate = match gemini_resp.candidates.into_iter().next() {
                    Some(c) => c,
                    None => continue,
                };

                let text = candidate
                    .content
                    .as_ref()
                    .and_then(|c| c.parts.first())
                    .and_then(|p| p.text.as_deref())
                    .unwrap_or("")
                    .to_string();

                let finish_reason = candidate.finish_reason.map(|r| match r.as_str() {
                    "STOP" => "stop".to_string(),
                    "MAX_TOKENS" => "length".to_string(),
                    other => other.to_lowercase(),
                });

                let chunk = ChatCompletionChunk {
                    id: msg_id.clone(),
                    object: "chat.completion.chunk".to_string(),
                    created: chrono::Utc::now().timestamp() as u64,
                    model: model_id.clone(),
                    choices: vec![ChunkChoice {
                        index: 0,
                        delta: ChunkDelta {
                            role: None,
                            content: if text.is_empty() { None } else { Some(text) },
                            tool_calls: None,
                        },
                        finish_reason,
                    }],
                    usage,
                };
                yield Ok(chunk);
            }
        };

        Ok(Box::pin(chunk_stream))
    }
}

// ── Request types ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiSystemInstruction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GeminiTool>>,
}

#[derive(Serialize, Deserialize)]
struct GeminiContent {
    role: String,
    parts: Vec<GeminiPart>,
}

#[derive(Serialize)]
struct GeminiSystemInstruction {
    parts: Vec<GeminiPart>,
}

#[derive(Serialize, Deserialize)]
struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inline_data: Option<GeminiInlineData>,
}

#[derive(Serialize, Deserialize)]
struct GeminiInlineData {
    mime_type: String,
    data: String,
}

#[derive(Serialize)]
struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequences: Option<Vec<String>>,
}

#[derive(Serialize)]
struct GeminiTool {
    function_declarations: Vec<GeminiFunctionDeclaration>,
}

#[derive(Serialize)]
struct GeminiFunctionDeclaration {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<Value>,
}

fn build_request_body(req: &ChatCompletionRequest) -> GeminiRequest {
    let system = req.system_message();

    let contents: Vec<GeminiContent> = req
        .messages
        .iter()
        .filter(|m| m.role != "system")
        .map(convert_message)
        .collect();

    let system_instruction = system.map(|s| GeminiSystemInstruction {
        parts: vec![GeminiPart {
            text: Some(s),
            inline_data: None,
        }],
    });

    let stop_sequences = req.stop.as_ref().map(|s| match s {
        StopSequences::Single(v) => vec![v.clone()],
        StopSequences::Multiple(v) => v.clone(),
    });

    let generation_config = if req.temperature.is_some()
        || req.max_tokens.is_some()
        || req.top_p.is_some()
        || stop_sequences.is_some()
    {
        Some(GeminiGenerationConfig {
            temperature: req.temperature,
            max_output_tokens: req.max_tokens,
            top_p: req.top_p,
            stop_sequences,
        })
    } else {
        None
    };

    let tools = req.tools.as_ref().map(|tools| {
        vec![GeminiTool {
            function_declarations: tools
                .iter()
                .map(|t| GeminiFunctionDeclaration {
                    name: t.function.name.clone(),
                    description: t.function.description.clone(),
                    parameters: t.function.parameters.clone(),
                })
                .collect(),
        }]
    });

    GeminiRequest {
        contents,
        system_instruction,
        generation_config,
        tools,
    }
}

fn convert_message(msg: &ChatMessage) -> GeminiContent {
    let role = match msg.role.as_str() {
        "assistant" => "model",
        _ => "user",
    };

    let parts = match &msg.content {
        None => vec![GeminiPart {
            text: Some(String::new()),
            inline_data: None,
        }],
        Some(MessageContent::Text(t)) => vec![GeminiPart {
            text: Some(t.clone()),
            inline_data: None,
        }],
        Some(MessageContent::Parts(parts)) => parts
            .iter()
            .map(|p| match p {
                ContentPart::Text { text } => GeminiPart {
                    text: Some(text.clone()),
                    inline_data: None,
                },
                ContentPart::ImageUrl { image_url } => {
                    // For data URLs, extract base64; otherwise use text placeholder
                    if image_url.url.starts_with("data:") {
                        let mut split = image_url.url.splitn(2, ',');
                        let header = split.next().unwrap_or("");
                        let data = split.next().unwrap_or("").to_string();
                        let mime_type = header
                            .strip_prefix("data:")
                            .unwrap_or("image/jpeg")
                            .split(';')
                            .next()
                            .unwrap_or("image/jpeg")
                            .to_string();
                        GeminiPart {
                            text: None,
                            inline_data: Some(GeminiInlineData { mime_type, data }),
                        }
                    } else {
                        GeminiPart {
                            text: Some(image_url.url.clone()),
                            inline_data: None,
                        }
                    }
                }
            })
            .collect(),
    };

    GeminiContent {
        role: role.to_string(),
        parts,
    }
}

// ── Response types ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct GeminiResponse {
    candidates: Vec<GeminiCandidate>,
    #[serde(rename = "usageMetadata")]
    usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Deserialize)]
struct GeminiCandidate {
    content: Option<GeminiContent>,
    #[serde(rename = "finishReason")]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct GeminiUsageMetadata {
    #[serde(rename = "promptTokenCount")]
    prompt_token_count: u32,
    #[serde(rename = "candidatesTokenCount")]
    candidates_token_count: Option<u32>,
    #[serde(rename = "totalTokenCount")]
    total_token_count: u32,
}

fn gemini_to_openai_response(resp: GeminiResponse, model_id: &str) -> ChatCompletionResponse {
    let id = Uuid::new_v4().to_string();

    let usage = resp.usage_metadata.map(|u| Usage {
        prompt_tokens: u.prompt_token_count,
        completion_tokens: u.candidates_token_count.unwrap_or(0),
        total_tokens: u.total_token_count,
    });

    let choices = resp
        .candidates
        .into_iter()
        .enumerate()
        .map(|(i, candidate)| {
            let text = candidate
                .content
                .as_ref()
                .and_then(|c| c.parts.first())
                .and_then(|p| p.text.as_deref())
                .unwrap_or("")
                .to_string();

            let finish_reason = candidate.finish_reason.map(|r| match r.as_str() {
                "STOP" => "stop".to_string(),
                "MAX_TOKENS" => "length".to_string(),
                other => other.to_lowercase(),
            });

            let message = ChatMessage {
                role: "assistant".to_string(),
                content: Some(MessageContent::Text(text)),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            };

            Choice {
                index: i as u32,
                message,
                finish_reason,
            }
        })
        .collect();

    ChatCompletionResponse {
        id,
        object: "chat.completion".to_string(),
        created: chrono::Utc::now().timestamp() as u64,
        model: model_id.to_string(),
        choices,
        usage,
        system_fingerprint: None,
    }
}

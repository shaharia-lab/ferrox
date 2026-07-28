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
    ChunkChoice, ChunkDelta, ContentPart, FunctionCall, MessageContent, StopSequences,
    StreamFunctionCall, StreamToolCall, ToolCall, Usage,
};
use std::collections::HashMap;

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

impl GeminiAdapter {
    /// Gemini can't fetch an arbitrary image URL, so download any http(s) image
    /// parts and inline them as `data:` URLs (which `convert_message` turns into
    /// `inline_data`). A fetch failure leaves the URL untouched.
    async fn inline_remote_images(&self, req: &ChatCompletionRequest) -> ChatCompletionRequest {
        let mut resolved = req.clone();
        for msg in &mut resolved.messages {
            if let Some(MessageContent::Parts(parts)) = &mut msg.content {
                for part in parts.iter_mut() {
                    if let ContentPart::ImageUrl { image_url } = part {
                        if !image_url.url.starts_with("data:") {
                            if let Some(data_url) =
                                self.fetch_image_as_data_url(&image_url.url).await
                            {
                                image_url.url = data_url;
                            }
                        }
                    }
                }
            }
        }
        resolved
    }

    async fn fetch_image_as_data_url(&self, url: &str) -> Option<String> {
        // SSRF guard: only http(s), and refuse hosts that resolve to a non-public
        // address (loopback/private/link-local incl. cloud-metadata 169.254.169.254).
        let parsed = reqwest::Url::parse(url).ok()?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return None;
        }
        let host = parsed.host_str()?;
        let port = parsed.port_or_known_default().unwrap_or(443);
        let mut resolved_any = false;
        for addr in tokio::net::lookup_host((host, port)).await.ok()? {
            resolved_any = true;
            if !is_public_ip(&addr.ip()) {
                tracing::warn!(
                    %host,
                    "refusing to fetch image from a non-public address (SSRF guard)"
                );
                return None;
            }
        }
        if !resolved_any {
            return None;
        }

        let resp = self.client.get(parsed).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let mime = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(';').next())
            .unwrap_or("image/jpeg")
            .to_string();
        let bytes = resp.bytes().await.ok()?;
        // Cap the download to bound memory (20 MiB).
        if bytes.len() > 20 * 1024 * 1024 {
            tracing::warn!(len = bytes.len(), "remote image exceeds size cap; skipping");
            return None;
        }
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Some(format!("data:{mime};base64,{b64}"))
    }
}

/// Whether an IP is a public (non-internal) address — used to block SSRF to
/// loopback, private, link-local (incl. cloud metadata), and unspecified ranges.
fn is_public_ip(ip: &std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.octets()[0] == 0)
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
                return false;
            }
            // IPv4-mapped (::ffff:a.b.c.d) → check the embedded v4.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(&IpAddr::V4(v4));
            }
            let seg0 = v6.segments()[0];
            let unique_local = seg0 & 0xfe00 == 0xfc00; // fc00::/7
            let link_local = seg0 & 0xffc0 == 0xfe80; // fe80::/10
            !(unique_local || link_local)
        }
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
        let req = self.inline_remote_images(req).await;
        let body = build_request_body(&req);
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
        let req = self.inline_remote_images(req).await;
        let body = build_request_body(&req);
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
        extra: Default::default(),
                });

                let candidate = match gemini_resp.candidates.into_iter().next() {
                    Some(c) => c,
                    None => continue,
                };

                let (text, tool_calls) = collect_parts(candidate.content.as_ref());
                // Gemini streams each functionCall whole; emit as a complete
                // streaming tool-call fragment (id/name/args all present).
                let stream_tool_calls: Vec<StreamToolCall> = tool_calls
                    .into_iter()
                    .enumerate()
                    .map(|(i, tc)| StreamToolCall {
                        index: i as i32,
                        id: Some(tc.id),
                        r#type: Some(tc.r#type),
                        function: Some(StreamFunctionCall {
                            name: Some(tc.function.name),
                            arguments: Some(tc.function.arguments),
                        }),
                    })
                    .collect();

                let finish_reason = candidate
                    .finish_reason
                    .as_deref()
                    .map(|r| map_gemini_finish_reason(r, !stream_tool_calls.is_empty()));

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
                            tool_calls: if stream_tool_calls.is_empty() {
                                None
                            } else {
                                Some(stream_tool_calls)
                            },
                            reasoning_content: None,
                        },
                        finish_reason,
                        extra: Default::default(),
                    }],
                    usage,
                    extra: Default::default(),
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
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_config: Option<GeminiToolConfig>,
}

#[derive(Serialize)]
struct GeminiToolConfig {
    function_calling_config: GeminiFunctionCallingConfig,
}

#[derive(Serialize)]
struct GeminiFunctionCallingConfig {
    /// AUTO | ANY | NONE.
    mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed_function_names: Option<Vec<String>>,
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
    // Gemini uses camelCase in responses; rename so `functionCall`/`functionResponse`
    // both deserialize (from responses) and serialize (canonical for requests).
    #[serde(rename = "functionCall", skip_serializing_if = "Option::is_none")]
    function_call: Option<GeminiFunctionCall>,
    #[serde(rename = "functionResponse", skip_serializing_if = "Option::is_none")]
    function_response: Option<GeminiFunctionResponse>,
}

#[derive(Serialize, Deserialize)]
struct GeminiFunctionCall {
    name: String,
    #[serde(default)]
    args: Value,
}

#[derive(Serialize, Deserialize)]
struct GeminiFunctionResponse {
    name: String,
    response: Value,
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

    // Map each assistant tool_call id → function name so `role:"tool"` results can
    // be rendered as Gemini `functionResponse` parts (which key on name, not id).
    let mut tool_names: HashMap<String, String> = HashMap::new();
    for m in &req.messages {
        if let Some(tcs) = &m.tool_calls {
            for tc in tcs {
                tool_names.insert(tc.id.clone(), tc.function.name.clone());
            }
        }
    }

    let contents: Vec<GeminiContent> = req
        .messages
        .iter()
        .filter(|m| m.role != "system")
        .map(|m| convert_message(m, &tool_names))
        .collect();

    let system_instruction = system.map(|s| GeminiSystemInstruction {
        parts: vec![text_part(s)],
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

    let tool_config = req.tool_choice.as_ref().and_then(gemini_tool_config);

    GeminiRequest {
        contents,
        system_instruction,
        generation_config,
        tools,
        tool_config,
    }
}

/// Map an OpenAI `tool_choice` to a Gemini `functionCallingConfig`.
fn gemini_tool_config(tc: &Value) -> Option<GeminiToolConfig> {
    let (mode, allowed) = match tc {
        Value::String(s) => match s.as_str() {
            "auto" => ("AUTO", None),
            "none" => ("NONE", None),
            "required" => ("ANY", None),
            _ => return None,
        },
        Value::Object(o) => {
            let name = o
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())?;
            ("ANY", Some(vec![name.to_string()]))
        }
        _ => return None,
    };
    Some(GeminiToolConfig {
        function_calling_config: GeminiFunctionCallingConfig {
            mode: mode.to_string(),
            allowed_function_names: allowed,
        },
    })
}

fn text_part(text: String) -> GeminiPart {
    GeminiPart {
        text: Some(text),
        inline_data: None,
        function_call: None,
        function_response: None,
    }
}

/// Convert one internal message. `tool_names` maps a tool_call id to the function
/// name (Gemini's `functionResponse` needs the name, not the OpenAI id).
fn convert_message(msg: &ChatMessage, tool_names: &HashMap<String, String>) -> GeminiContent {
    // A `role:"tool"` result → a user turn carrying a `functionResponse` part.
    if msg.role == "tool" {
        let name = msg
            .tool_call_id
            .as_ref()
            .and_then(|id| tool_names.get(id))
            .cloned()
            .unwrap_or_default();
        let result = match &msg.content {
            Some(MessageContent::Text(t)) => t.clone(),
            _ => String::new(),
        };
        return GeminiContent {
            role: "user".to_string(),
            parts: vec![GeminiPart {
                text: None,
                inline_data: None,
                function_call: None,
                function_response: Some(GeminiFunctionResponse {
                    name,
                    response: serde_json::json!({ "result": result }),
                }),
            }],
        };
    }

    let role = if msg.role == "assistant" {
        "model"
    } else {
        "user"
    };
    let mut parts: Vec<GeminiPart> = Vec::new();

    match &msg.content {
        None => {}
        Some(MessageContent::Text(t)) => {
            if !t.is_empty() {
                parts.push(text_part(t.clone()));
            }
        }
        Some(MessageContent::Parts(cparts)) => {
            for p in cparts {
                match p {
                    ContentPart::Text { text } => parts.push(text_part(text.clone())),
                    ContentPart::ImageUrl { image_url } => {
                        if let Some((mime_type, data)) = parse_data_url(&image_url.url) {
                            parts.push(GeminiPart {
                                text: None,
                                inline_data: Some(GeminiInlineData { mime_type, data }),
                                function_call: None,
                                function_response: None,
                            });
                        } else {
                            parts.push(text_part(image_url.url.clone()));
                        }
                    }
                }
            }
        }
    }

    // Assistant tool calls → `functionCall` parts.
    if let Some(tcs) = &msg.tool_calls {
        for tc in tcs {
            let args: Value = serde_json::from_str(&tc.function.arguments)
                .unwrap_or_else(|_| Value::Object(Default::default()));
            parts.push(GeminiPart {
                text: None,
                inline_data: None,
                function_call: Some(GeminiFunctionCall {
                    name: tc.function.name.clone(),
                    args,
                }),
                function_response: None,
            });
        }
    }

    // Gemini rejects empty parts arrays.
    if parts.is_empty() {
        parts.push(text_part(String::new()));
    }

    GeminiContent {
        role: role.to_string(),
        parts,
    }
}

/// Parse a `data:<mime>;base64,<data>` URL into (mime, base64). Returns None for
/// non-data URLs.
fn parse_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let mut split = rest.splitn(2, ',');
    let header = split.next().unwrap_or("");
    let data = split.next()?.to_string();
    let mime_type = header.split(';').next().unwrap_or("image/jpeg").to_string();
    Some((mime_type, data))
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

/// Collect all of a candidate's parts into concatenated text + tool calls
/// (Gemini can return several parts, including multiple `functionCall`s).
fn collect_parts(content: Option<&GeminiContent>) -> (String, Vec<ToolCall>) {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    if let Some(c) = content {
        for p in &c.parts {
            if let Some(t) = &p.text {
                text.push_str(t);
            }
            if let Some(fc) = &p.function_call {
                tool_calls.push(ToolCall {
                    id: format!("call_{}", Uuid::new_v4().simple()),
                    r#type: "function".to_string(),
                    function: FunctionCall {
                        name: fc.name.clone(),
                        arguments: fc.args.to_string(),
                    },
                });
            }
        }
    }
    (text, tool_calls)
}

/// Map a Gemini `finishReason` to an OpenAI `finish_reason`. A turn that produced
/// tool calls always reports `tool_calls`.
fn map_gemini_finish_reason(reason: &str, has_tool_calls: bool) -> String {
    if has_tool_calls {
        return "tool_calls".to_string();
    }
    match reason {
        "STOP" => "stop",
        "MAX_TOKENS" => "length",
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" => "content_filter",
        _ => "stop",
    }
    .to_string()
}

fn gemini_to_openai_response(resp: GeminiResponse, model_id: &str) -> ChatCompletionResponse {
    let id = Uuid::new_v4().to_string();

    let usage = resp.usage_metadata.map(|u| Usage {
        prompt_tokens: u.prompt_token_count,
        completion_tokens: u.candidates_token_count.unwrap_or(0),
        total_tokens: u.total_token_count,
        extra: Default::default(),
    });

    let choices = resp
        .candidates
        .into_iter()
        .enumerate()
        .map(|(i, candidate)| {
            let (text, tool_calls) = collect_parts(candidate.content.as_ref());
            let finish_reason = candidate
                .finish_reason
                .as_deref()
                .map(|r| map_gemini_finish_reason(r, !tool_calls.is_empty()));

            let message = ChatMessage {
                role: "assistant".to_string(),
                content: if text.is_empty() {
                    None
                } else {
                    Some(MessageContent::Text(text))
                },
                name: None,
                tool_calls: if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls)
                },
                tool_call_id: None,
                reasoning_content: None,
            };

            Choice {
                index: i as u32,
                message,
                finish_reason,
                extra: Default::default(),
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
        extra: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DefaultsConfig, ProviderConfig, ProviderType};
    use crate::types::{ImageUrl, Tool, ToolFunction};

    fn adapter() -> GeminiAdapter {
        GeminiAdapter::new(
            &ProviderConfig {
                name: "g".into(),
                provider_type: ProviderType::Gemini,
                api_key: Some("k".into()),
                base_url: None,
                region: None,
                timeouts: None,
                retry: None,
                circuit_breaker: None,
            },
            &DefaultsConfig::default(),
        )
        .unwrap()
    }

    #[test]
    fn ssrf_guard_rejects_internal_addresses() {
        use std::net::IpAddr;
        // Rejected (SSRF targets).
        for s in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254", // cloud metadata
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fc00::1",
        ] {
            assert!(
                !is_public_ip(&s.parse::<IpAddr>().unwrap()),
                "{s} must be blocked"
            );
        }
        // Allowed (public).
        for s in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(
                is_public_ip(&s.parse::<IpAddr>().unwrap()),
                "{s} must be allowed"
            );
        }
    }

    #[tokio::test]
    async fn data_url_images_are_not_refetched() {
        // A data: image must be left untouched (no network fetch).
        let a = adapter();
        let msg = ChatMessage {
            role: "user".into(),
            content: Some(MessageContent::Parts(vec![ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "data:image/png;base64,QUJD".into(),
                    detail: None,
                },
            }])),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        };
        let resolved = a.inline_remote_images(&req(vec![msg], None)).await;
        if let Some(MessageContent::Parts(parts)) = &resolved.messages[0].content {
            if let ContentPart::ImageUrl { image_url } = &parts[0] {
                assert_eq!(image_url.url, "data:image/png;base64,QUJD");
            } else {
                panic!("expected image part");
            }
        } else {
            panic!("expected parts");
        }
    }

    fn req(messages: Vec<ChatMessage>, tool_choice: Option<Value>) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "gemini".into(),
            messages,
            stream: None,
            temperature: None,
            max_tokens: None,
            top_p: None,
            stop: None,
            tools: Some(vec![Tool {
                r#type: "function".into(),
                function: ToolFunction {
                    name: "get_weather".into(),
                    description: None,
                    parameters: Some(serde_json::json!({"type": "object"})),
                },
            }]),
            tool_choice,
            system: None,
            extra_headers: Default::default(),
            raw_anthropic_body: None,
            extra: Default::default(),
        }
    }

    fn asst_tool_call() -> ChatMessage {
        ChatMessage {
            role: "assistant".into(),
            content: None,
            name: None,
            tool_calls: Some(vec![ToolCall {
                id: "t1".into(),
                r#type: "function".into(),
                function: FunctionCall {
                    name: "get_weather".into(),
                    arguments: r#"{"loc":"NYC"}"#.into(),
                },
            }]),
            tool_call_id: None,
            reasoning_content: None,
        }
    }

    fn tool_result() -> ChatMessage {
        ChatMessage {
            role: "tool".into(),
            content: Some(MessageContent::Text("sunny".into())),
            name: None,
            tool_calls: None,
            tool_call_id: Some("t1".into()),
            reasoning_content: None,
        }
    }

    #[test]
    fn request_tool_history_maps_to_function_call_and_response() {
        let body = serde_json::to_value(build_request_body(&req(
            vec![asst_tool_call(), tool_result()],
            None,
        )))
        .unwrap();
        let dump = body.to_string();
        assert!(
            dump.contains("functionCall"),
            "assistant tool_call → functionCall: {dump}"
        );
        assert!(
            dump.contains("functionResponse"),
            "tool result → functionResponse"
        );
        assert!(dump.contains("get_weather"));
    }

    #[test]
    fn tool_choice_named_becomes_any_with_allowed() {
        let body = serde_json::to_value(build_request_body(&req(
            vec![],
            Some(serde_json::json!({"type":"function","function":{"name":"get_weather"}})),
        )))
        .unwrap();
        assert_eq!(
            body["tool_config"]["function_calling_config"]["mode"],
            "ANY"
        );
        assert_eq!(
            body["tool_config"]["function_calling_config"]["allowed_function_names"][0],
            "get_weather"
        );
    }

    #[test]
    fn response_function_call_becomes_tool_call() {
        let resp: GeminiResponse = serde_json::from_str(
            r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"get_weather","args":{"loc":"NYC"}}}]},"finishReason":"STOP"}]}"#,
        )
        .unwrap();
        let out = gemini_to_openai_response(resp, "gemini");
        let tc = &out.choices[0].message.tool_calls.as_ref().unwrap()[0];
        assert_eq!(tc.function.name, "get_weather");
        assert!(tc.function.arguments.contains("NYC"));
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn safety_finish_reason_maps_to_content_filter() {
        assert_eq!(map_gemini_finish_reason("SAFETY", false), "content_filter");
        assert_eq!(map_gemini_finish_reason("STOP", true), "tool_calls");
    }
}

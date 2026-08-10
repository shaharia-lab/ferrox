use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

// ── Inbound request ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub stream: Option<bool>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub top_p: Option<f32>,
    pub stop: Option<StopSequences>,
    pub tools: Option<Vec<Tool>>,
    pub tool_choice: Option<serde_json::Value>,
    /// Convenience field — system prompt (alternative to a system message)
    pub system: Option<String>,
    /// Extra HTTP headers to forward to the upstream provider (e.g. `anthropic-beta`).
    /// Never serialised — carried out-of-band through the pipeline.
    #[serde(skip)]
    pub extra_headers: HashMap<String, String>,
    /// Original Anthropic-format request body.  Set by the `/anthropic/v1/messages`
    /// handler so the Anthropic provider adapter can forward it verbatim (only
    /// overriding `model` and `stream`), preserving every field the client sent —
    /// `cache_control`, `thinking`, `service_tier`, `output_config`, tool attributes, etc.
    /// Never serialised — carried out-of-band.
    #[serde(skip)]
    pub raw_anthropic_body: Option<serde_json::Value>,
    /// Catch-all for unknown fields (pass-through)
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl ChatCompletionRequest {
    pub fn is_streaming(&self) -> bool {
        self.stream.unwrap_or(false)
    }

    /// Return the system prompt from the `system` field or the first message
    /// with `role == "system"`.
    pub fn system_message(&self) -> Option<String> {
        if let Some(s) = &self.system {
            return Some(s.clone());
        }
        self.messages
            .iter()
            .find(|m| m.role == "system")
            .and_then(|m| {
                m.content.as_ref().map(|c| match c {
                    MessageContent::Text(t) => t.clone(),
                    MessageContent::Parts(parts) => parts
                        .iter()
                        .filter_map(|p| match p {
                            ContentPart::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(""),
                })
            })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: Option<MessageContent>,
    pub name: Option<String>,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub tool_call_id: Option<String>,
    /// Chain-of-thought / extended-thinking text. Reasoning models on
    /// OpenAI-compatible APIs (Kimi, GLM, DeepSeek) return this alongside
    /// `content`; preserved so it round-trips instead of being dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Pass-through for message-level attributes with no field of their own —
    /// `cache_control` above all — so they survive the internal representation
    /// instead of being erased in translation. Mirrors the catch-all on
    /// [`ChatCompletionRequest`] and [`Usage`].
    #[serde(
        flatten,
        default,
        skip_serializing_if = "std::collections::HashMap::is_empty"
    )]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
        /// Per-block pass-through, carrying `cache_control` breakpoints across
        /// the internal representation. See [`ChatMessage::extra`].
        #[serde(
            flatten,
            default,
            skip_serializing_if = "std::collections::HashMap::is_empty"
        )]
        extra: std::collections::HashMap<String, serde_json::Value>,
    },
    ImageUrl {
        image_url: ImageUrl,
        #[serde(
            flatten,
            default,
            skip_serializing_if = "std::collections::HashMap::is_empty"
        )]
        extra: std::collections::HashMap<String, serde_json::Value>,
    },
}

/// Key under which Anthropic prompt-cache breakpoints travel, on both
/// [`ChatMessage::extra`] and [`ContentPart::Text::extra`].
pub const CACHE_CONTROL: &str = "cache_control";

/// Request-level [`ChatCompletionRequest::extra`] key holding the `cache_control`
/// of the **system** prompt.
///
/// The internal representation flattens Anthropic system blocks into a single
/// string, which has nowhere to hold a per-block breakpoint, so it is hoisted
/// here — the same private-key convention already used by `_anthropic_thinking`
/// and `_anthropic_betas`.
pub const ANTHROPIC_SYSTEM_CACHE_CONTROL: &str = "_anthropic_system_cache_control";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StopSequences {
    Single(String),
    Multiple(Vec<String>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub r#type: String,
    pub function: ToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    pub description: Option<String>,
    pub parameters: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub r#type: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// A tool call as it appears in a streaming `delta`.
///
/// Unlike the non-streaming [`ToolCall`], every field except `index` is
/// optional: OpenAI-format providers send `id`/`type`/`name` only in the first
/// fragment for a given `index`, then stream `function.arguments` in pieces.
/// Modelling those fields as required (as [`ToolCall`] does) makes continuation
/// fragments fail to deserialize and aborts the whole stream. Consumers
/// accumulate fragments by `index`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamToolCall {
    /// Signed because some OpenAI-compatible providers send `-1` for a single
    /// tool call; consumers clamp negatives to 0 (matching the official SDKs).
    #[serde(default)]
    pub index: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<StreamFunctionCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamFunctionCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

// ── Non-streaming response ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: Option<Usage>,
    pub system_fingerprint: Option<String>,
    /// Pass-through for non-modelled response fields (e.g. choice `logprobs`,
    /// `service_tier`) so they survive the OpenAI-format round-trip.
    #[serde(
        flatten,
        default,
        skip_serializing_if = "std::collections::HashMap::is_empty"
    )]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub index: u32,
    pub message: ChatMessage,
    pub finish_reason: Option<String>,
    /// Pass-through for non-modelled response fields (e.g. choice `logprobs`,
    /// `service_tier`) so they survive the OpenAI-format round-trip.
    #[serde(
        flatten,
        default,
        skip_serializing_if = "std::collections::HashMap::is_empty"
    )]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    /// Pass-through for provider usage details (`prompt_tokens_details`,
    /// `completion_tokens_details` with cache/reasoning token breakdowns, etc.)
    /// so they survive the OpenAI-format response round-trip.
    #[serde(
        flatten,
        default,
        skip_serializing_if = "std::collections::HashMap::is_empty"
    )]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

// ── Prompt-cache token counters carried in `Usage.extra` ─────────────────────

/// Anthropic-native key for tokens written to the prompt cache.
pub const CACHE_CREATION_INPUT_TOKENS: &str = "cache_creation_input_tokens";
/// Anthropic-native key for tokens served from the prompt cache.
pub const CACHE_READ_INPUT_TOKENS: &str = "cache_read_input_tokens";
/// OpenAI-canonical container for input-token breakdowns.
pub const PROMPT_TOKENS_DETAILS: &str = "prompt_tokens_details";
/// OpenAI-canonical key for cache reads, nested under [`PROMPT_TOKENS_DETAILS`].
pub const CACHED_TOKENS: &str = "cached_tokens";
/// OpenAI-canonical key for cache writes, nested under [`PROMPT_TOKENS_DETAILS`].
///
/// Declared by the official SDKs as "the unadjusted number of prompt tokens
/// written to cache" (`openai-python` `PromptTokensDetails.cache_write_tokens`,
/// `openai-go` `CompletionUsagePromptTokensDetails.CacheWriteTokens`).
pub const CACHE_WRITE_TOKENS: &str = "cache_write_tokens";

/// Build the [`Usage::extra`] entries for a pair of prompt-cache counters.
///
/// Each counter is represented twice on purpose: once under its Anthropic-native
/// key (`cache_read_input_tokens` / `cache_creation_input_tokens`) and once
/// under OpenAI's `prompt_tokens_details` (`cached_tokens` / `cache_write_tokens`),
/// so both API surfaces report them without a second translation step. **Each
/// pair is the same tokens — a consumer must read exactly one of the two and
/// never sum them.**
///
/// Returns an empty map when neither counter is present, which keeps the
/// no-cache path allocation-free (`HashMap::new` does not allocate) and makes
/// the serialized response byte-identical to one without cache support.
pub fn cache_usage_extra(
    creation: Option<u32>,
    read: Option<u32>,
) -> HashMap<String, serde_json::Value> {
    let mut extra = HashMap::new();
    let mut details = serde_json::Map::new();
    if let Some(creation) = creation {
        extra.insert(CACHE_CREATION_INPUT_TOKENS.to_string(), creation.into());
        details.insert(CACHE_WRITE_TOKENS.to_string(), creation.into());
    }
    if let Some(read) = read {
        extra.insert(CACHE_READ_INPUT_TOKENS.to_string(), read.into());
        details.insert(CACHED_TOKENS.to_string(), read.into());
    }
    if !details.is_empty() {
        extra.insert(
            PROMPT_TOKENS_DETAILS.to_string(),
            serde_json::Value::Object(details),
        );
    }
    extra
}

/// Prompt-cache counters for a completed response as `(cache_read, cache_write)`,
/// defaulting to zero when the provider reported none.
///
/// The observability surfaces (metrics, logs, `usage_log`) all want plain
/// counts, and they must all read the **same** one of the two equivalent cache
/// read representations — this is that single reading.
pub fn cache_tokens(usage: &Usage) -> (u32, u32) {
    let (creation, read) = cache_tokens_from_extra(&usage.extra);
    (read.unwrap_or(0), creation.unwrap_or(0))
}

/// Recover `(cache_creation, cache_read)` from [`Usage::extra`].
///
/// The Anthropic-native keys win; the `prompt_tokens_details` counterparts
/// (`cached_tokens` for reads, `cache_write_tokens` for writes) are the fallback,
/// so usage that originated from an OpenAI-format upstream still round-trips
/// onto the Anthropic surface.
pub fn cache_tokens_from_extra(
    extra: &HashMap<String, serde_json::Value>,
) -> (Option<u32>, Option<u32>) {
    let as_u32 = |v: &serde_json::Value| v.as_u64().map(|t| t as u32);
    let detail = |key: &str| {
        extra
            .get(PROMPT_TOKENS_DETAILS)
            .and_then(|d| d.get(key))
            .and_then(as_u32)
    };
    let creation = extra
        .get(CACHE_CREATION_INPUT_TOKENS)
        .and_then(as_u32)
        .or_else(|| detail(CACHE_WRITE_TOKENS));
    let read = extra
        .get(CACHE_READ_INPUT_TOKENS)
        .and_then(as_u32)
        .or_else(|| detail(CACHED_TOKENS));
    (creation, read)
}

// ── Streaming chunk ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChunkChoice>,
    pub usage: Option<Usage>,
    /// Pass-through for non-modelled response fields (e.g. choice `logprobs`,
    /// `service_tier`) so they survive the OpenAI-format round-trip.
    #[serde(
        flatten,
        default,
        skip_serializing_if = "std::collections::HashMap::is_empty"
    )]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkChoice {
    pub index: u32,
    pub delta: ChunkDelta,
    pub finish_reason: Option<String>,
    /// Pass-through for non-modelled response fields (e.g. choice `logprobs`,
    /// `service_tier`) so they survive the OpenAI-format round-trip.
    #[serde(
        flatten,
        default,
        skip_serializing_if = "std::collections::HashMap::is_empty"
    )]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkDelta {
    pub role: Option<String>,
    pub content: Option<String>,
    pub tool_calls: Option<Vec<StreamToolCall>>,
    /// Streaming chain-of-thought delta (Kimi/GLM/DeepSeek emit `reasoning_content`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

// ── Models list response ─────────────────────────────────────────────────────

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ModelsResponse {
    /// Always `"list"`.
    #[schema(example = "list")]
    pub object: String,
    pub data: Vec<ModelObject>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ModelObject {
    /// The model alias configured in the gateway.
    #[schema(example = "claude-sonnet")]
    pub id: String,
    /// Always `"model"`.
    #[schema(example = "model")]
    pub object: String,
    /// Unix timestamp; the gateway emits `0` (aliases are not versioned).
    pub created: u64,
    /// Always `"proxy"`.
    #[schema(example = "proxy")]
    pub owned_by: String,
}

// ── Request context (injected by auth middleware) ────────────────────────────

#[derive(Debug, Clone)]
pub struct RequestContext {
    pub request_id: String,
    pub key_name: String,
    pub allowed_models: Vec<String>,
    /// UUID of the authenticated client (from JWT `sub` or ferrox claims).
    /// `None` for static virtual keys (which have no control-plane identity).
    pub client_id: Option<Uuid>,
    /// Token budget from JWT claims.  `None` means unlimited.
    /// Used by handlers for post-response Redis budget recording.
    #[allow(dead_code)]
    pub token_budget: Option<i64>,
    /// Budget period from JWT claims ("daily" or "monthly").
    pub budget_period: Option<String>,
    /// Tokens reserved in the pre-request budget check.
    /// Used by handlers for post-response reconciliation.
    pub budget_reserved_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_streaming ─────────────────────────────────────────────────────────

    fn minimal_req(stream: Option<bool>) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "gpt-4".to_string(),
            messages: vec![],
            stream,
            temperature: None,
            max_tokens: None,
            top_p: None,
            stop: None,
            tools: None,
            tool_choice: None,
            system: None,
            extra_headers: HashMap::new(),
            raw_anthropic_body: None,
            extra: HashMap::new(),
        }
    }

    #[test]
    fn is_streaming_defaults_to_false_when_none() {
        assert!(!minimal_req(None).is_streaming());
    }

    #[test]
    fn is_streaming_true_when_stream_is_true() {
        assert!(minimal_req(Some(true)).is_streaming());
    }

    #[test]
    fn is_streaming_false_when_stream_is_false() {
        assert!(!minimal_req(Some(false)).is_streaming());
    }

    // ── system_message ────────────────────────────────────────────────────────

    fn req_with_system_field(s: &str) -> ChatCompletionRequest {
        let mut r = minimal_req(None);
        r.system = Some(s.to_string());
        r
    }

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: Some(MessageContent::Text(content.to_string())),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn system_message_from_system_field() {
        let r = req_with_system_field("You are helpful.");
        assert_eq!(r.system_message(), Some("You are helpful.".to_string()));
    }

    #[test]
    fn system_message_from_system_role_message() {
        let mut r = minimal_req(None);
        r.messages.push(msg("system", "Be concise."));
        r.messages.push(msg("user", "Hello"));
        assert_eq!(r.system_message(), Some("Be concise.".to_string()));
    }

    #[test]
    fn system_message_prefers_system_field_over_message() {
        let mut r = req_with_system_field("from field");
        r.messages.push(msg("system", "from message"));
        assert_eq!(r.system_message(), Some("from field".to_string()));
    }

    #[test]
    fn system_message_none_when_no_system_content() {
        let mut r = minimal_req(None);
        r.messages.push(msg("user", "Hello"));
        assert_eq!(r.system_message(), None);
    }

    #[test]
    fn system_message_from_text_content_parts() {
        let mut r = minimal_req(None);
        r.messages.push(ChatMessage {
            role: "system".to_string(),
            content: Some(MessageContent::Parts(vec![
                ContentPart::Text {
                    text: "Part1 ".to_string(),
                    extra: Default::default(),
                },
                ContentPart::Text {
                    text: "Part2".to_string(),
                    extra: Default::default(),
                },
            ])),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            extra: Default::default(),
        });
        assert_eq!(r.system_message(), Some("Part1 Part2".to_string()));
    }

    #[test]
    fn system_message_empty_when_system_message_has_no_content() {
        let mut r = minimal_req(None);
        r.messages.push(ChatMessage {
            role: "system".to_string(),
            content: None,
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            extra: Default::default(),
        });
        assert_eq!(r.system_message(), None);
    }

    fn usage_with(extra: HashMap<String, serde_json::Value>) -> Usage {
        Usage {
            prompt_tokens: 47,
            completion_tokens: 2,
            total_tokens: 49,
            extra,
        }
    }

    #[test]
    fn cache_tokens_returns_read_then_write() {
        // Note the ordering flip: the carrier is (creation, read), the
        // observability tuple is (read, write).
        let usage = usage_with(cache_usage_extra(Some(100), Some(3968)));
        assert_eq!(cache_tokens(&usage), (3968, 100));
    }

    #[test]
    fn cache_tokens_defaults_to_zero() {
        let usage = usage_with(HashMap::new());
        assert_eq!(cache_tokens(&usage), (0, 0));
    }

    #[test]
    fn cache_tokens_reads_openai_cached_tokens_fallback() {
        let usage = usage_with(HashMap::from([(
            "prompt_tokens_details".to_string(),
            serde_json::json!({"cached_tokens": 512}),
        )]));
        assert_eq!(cache_tokens(&usage), (512, 0));
    }

    #[test]
    fn cache_tokens_reads_openai_cache_write_tokens_fallback() {
        let usage = usage_with(HashMap::from([(
            "prompt_tokens_details".to_string(),
            serde_json::json!({"cache_write_tokens": 256}),
        )]));
        assert_eq!(cache_tokens(&usage), (0, 256));
    }

    /// Both counters must land under `prompt_tokens_details` as well as their
    /// Anthropic-native keys — the official OpenAI SDKs read the nested pair
    /// (`PromptTokensDetails.cached_tokens` / `.cache_write_tokens`) and ignore
    /// anything at the top level of `usage`.
    #[test]
    fn cache_usage_extra_emits_both_openai_details() {
        let extra = cache_usage_extra(Some(100), Some(3968));
        assert_eq!(extra["cache_creation_input_tokens"], 100);
        assert_eq!(extra["cache_read_input_tokens"], 3968);
        assert_eq!(extra["prompt_tokens_details"]["cache_write_tokens"], 100);
        assert_eq!(extra["prompt_tokens_details"]["cached_tokens"], 3968);
    }

    /// A write with no read still has to produce the OpenAI container, or a
    /// cache-creating request looks cacheless to an OpenAI SDK consumer.
    #[test]
    fn cache_usage_extra_write_only_still_emits_details() {
        let extra = cache_usage_extra(Some(100), None);
        assert_eq!(extra["prompt_tokens_details"]["cache_write_tokens"], 100);
        assert!(extra["prompt_tokens_details"]
            .get("cached_tokens")
            .is_none());
        assert!(!extra.contains_key("cache_read_input_tokens"));
    }

    #[test]
    fn cache_usage_extra_read_only_omits_write_key() {
        let extra = cache_usage_extra(None, Some(3968));
        assert_eq!(extra["prompt_tokens_details"]["cached_tokens"], 3968);
        assert!(extra["prompt_tokens_details"]
            .get("cache_write_tokens")
            .is_none());
        assert!(!extra.contains_key("cache_creation_input_tokens"));
    }

    /// The no-cache path must stay byte-identical to a response from a build
    /// without cache support — no empty `prompt_tokens_details` container.
    #[test]
    fn cache_usage_extra_empty_when_neither_counter_present() {
        assert!(cache_usage_extra(None, None).is_empty());
    }
}

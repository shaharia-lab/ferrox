//! OpenAI **Responses API** wire types and the Responses → Chat Completions
//! request translation.
//!
//! A Responses request (`POST /v1/responses`) is item-based: its `input` is a
//! list of typed items (`message`, `function_call`, `function_call_output`,
//! `reasoning`, …), its tools are flat (`{type, name, parameters}` instead of
//! Chat's nested `{type, function: {…}}`), and several fields are renamed
//! (`max_output_tokens`, `text.format`, `reasoning.effort`).
//! [`to_chat_completion_request`] maps all of that onto the internal
//! [`ChatCompletionRequest`] so every provider adapter can serve it.
//!
//! The endpoint is **stateless**: features that need server-side state
//! (`previous_response_id`, `conversation`, `prompt`, `background`) and
//! OpenAI-hosted built-in tools are rejected with an OpenAI-shaped 400 naming
//! the offending `param` ([`ProxyError::InvalidRequest`]) — never silently
//! dropped.
//!
//! Field inventory: `openai-python` 3.19.2,
//! `src/openai/types/responses/{response_create_params,response_input_item_param,tool_param,function_tool}.py`.

use serde::de::{self, DeserializeOwned, Deserializer, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fmt;

use crate::error::ProxyError;
use crate::types::{
    ChatCompletionRequest, ChatMessage, ContentPart, FunctionCall, ImageUrl, MessageContent, Tool,
    ToolCall, ToolFunction,
};

// ── Private `extra` keys ─────────────────────────────────────────────────────
//
// Responses-only knobs travel on `ChatCompletionRequest::extra` under
// `_`-prefixed keys, the same convention as `_anthropic_thinking`. Providers
// never forward `_` keys upstream (see `providers/openai.rs`), so they only
// reach the Responses response encoder.

/// Names of the tools that were declared as `custom` (free-form text input).
/// They are sent upstream as functions with a single string parameter `input`;
/// the response encoder uses this list to turn their calls back into
/// `custom_tool_call` items.
pub const RESPONSES_CUSTOM_TOOLS: &str = "_responses_custom_tools";
/// The request's `include` list (e.g. `reasoning.encrypted_content`).
pub const RESPONSES_INCLUDE: &str = "_responses_include";
/// `reasoning.summary` (`auto` / `concise` / `detailed`).
pub const RESPONSES_REASONING_SUMMARY: &str = "_responses_reasoning_summary";
/// `text.verbosity` (`low` / `medium` / `high`).
pub const RESPONSES_TEXT_VERBOSITY: &str = "_responses_text_verbosity";
/// `truncation` (`auto` / `disabled`).
pub const RESPONSES_TRUNCATION: &str = "_responses_truncation";
/// `max_tool_calls`.
pub const RESPONSES_MAX_TOOL_CALLS: &str = "_responses_max_tool_calls";
/// Anthropic thinking blocks recovered from `reasoning` input items whose
/// `encrypted_content` carries a Ferrox-encoded signature — an array of
/// `{"message_index": n, "thinking": "…", "signature": "…"}`, where
/// `message_index` is the index in the translated `messages` of the assistant
/// message the block precedes.
pub const RESPONSES_ANTHROPIC_THINKING_BLOCKS: &str = "_responses_anthropic_thinking_blocks";

/// Prefix marking a reasoning item's `encrypted_content` as a Ferrox-encoded
/// Anthropic thinking signature rather than an opaque OpenAI blob.
pub const FERROX_ANTHROPIC_SIGNATURE_PREFIX: &str = "ferrox.anthropic-thinking.v1:";

/// Encode an Anthropic thinking-block signature as a reasoning item's
/// `encrypted_content`, so a stateless client hands it back on the next turn.
pub fn encode_anthropic_thinking_signature(signature: &str) -> String {
    format!("{FERROX_ANTHROPIC_SIGNATURE_PREFIX}{signature}")
}

/// Inverse of [`encode_anthropic_thinking_signature`]. `None` for anything
/// Ferrox did not encode (e.g. real OpenAI encrypted reasoning).
pub fn decode_anthropic_thinking_signature(encrypted_content: &str) -> Option<&str> {
    encrypted_content
        .strip_prefix(FERROX_ANTHROPIC_SIGNATURE_PREFIX)
        .filter(|s| !s.is_empty())
}

/// Built-in (OpenAI-hosted) tool types. They need server-side execution this
/// gateway does not have, so they are recognised only to be rejected.
const BUILTIN_TOOL_TYPES: &[&str] = &[
    "web_search",
    "web_search_preview",
    "web_search_2025_08_26",
    "web_search_preview_2025_03_11",
    "file_search",
    "code_interpreter",
    "computer",
    "computer_use",
    "computer_use_preview",
    "mcp",
    "image_generation",
    "shell",
    "local_shell",
    "apply_patch",
    "tool_search",
    "namespace",
];

// ── Deserialization helpers ──────────────────────────────────────────────────

/// Deserialize a typed JSON object whose variant is chosen by its `type` field,
/// keeping the type name for unknown variants (so the 400 can name it) — which
/// serde's `#[serde(tag, other)]` cannot do. The map is moved into the variant
/// struct, so no string data is copied.
fn take_type<E: de::Error>(map: &Map<String, Value>, default: Option<&str>) -> Result<String, E> {
    match map.get("type") {
        Some(Value::String(t)) => Ok(t.clone()),
        Some(_) => Err(E::custom("`type` must be a string")),
        None => default
            .map(str::to_string)
            .ok_or_else(|| E::missing_field("type")),
    }
}

fn from_map<T: DeserializeOwned, E: de::Error>(map: Map<String, Value>) -> Result<T, E> {
    serde_json::from_value(Value::Object(map)).map_err(E::custom)
}

// ── Inbound request ──────────────────────────────────────────────────────────

/// `POST /v1/responses` request body.
///
/// Unknown top-level fields land in [`extra`](Self::extra) and are ignored, so
/// a newer SDK sending a field this gateway does not know yet still works.
#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    #[serde(default)]
    pub input: Option<ResponsesInput>,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub tools: Option<Vec<ResponsesTool>>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub max_tool_calls: Option<u32>,
    #[serde(default)]
    pub stream: Option<bool>,
    /// Accepted and ignored — nothing is ever stored.
    #[serde(default)]
    pub store: Option<bool>,
    #[serde(default)]
    pub metadata: Option<Value>,
    #[serde(default)]
    pub prompt_cache_key: Option<String>,
    #[serde(default)]
    pub safety_identifier: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub service_tier: Option<String>,
    #[serde(default)]
    pub text: Option<TextConfig>,
    #[serde(default)]
    pub reasoning: Option<ReasoningConfig>,
    #[serde(default)]
    pub include: Option<Vec<String>>,
    #[serde(default)]
    pub truncation: Option<String>,
    /// Rejected: needs server-side conversation state.
    #[serde(default)]
    pub previous_response_id: Option<String>,
    /// Rejected: needs server-side conversation state.
    #[serde(default)]
    pub conversation: Option<Value>,
    /// Rejected when `true`: needs a server-side job store.
    #[serde(default)]
    pub background: Option<bool>,
    /// Every other field, tolerated and ignored.
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

impl ResponsesRequest {
    pub fn is_streaming(&self) -> bool {
        self.stream.unwrap_or(false)
    }
}

/// `input`: a plain string (one user message) or a list of items.
#[derive(Debug, Clone)]
pub enum ResponsesInput {
    Text(String),
    Items(Vec<InputItem>),
}

/// String-or-list visitor shared by `input`, message `content` and tool-call
/// `output`. Hand-written instead of `#[serde(untagged)]` so an error inside
/// the list surfaces as itself, not as "did not match any variant".
struct StringOrSeq<T>(std::marker::PhantomData<T>);

impl<'de, T: Deserialize<'de>> Visitor<'de> for StringOrSeq<T> {
    type Value = Result<String, Vec<T>>;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a string or an array")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(Ok(v.to_string()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
        Ok(Ok(v))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(item) = seq.next_element()? {
            out.push(item);
        }
        Ok(Err(out))
    }
}

impl<'de> Deserialize<'de> for ResponsesInput {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(
            match d.deserialize_any(StringOrSeq(std::marker::PhantomData))? {
                Ok(s) => ResponsesInput::Text(s),
                Err(items) => ResponsesInput::Items(items),
            },
        )
    }
}

/// Message `content` or a tool call's `output`: a string or a list of parts.
#[derive(Debug, Clone)]
pub enum InputContent {
    Text(String),
    Parts(Vec<InputContentPart>),
}

impl<'de> Deserialize<'de> for InputContent {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(
            match d.deserialize_any(StringOrSeq(std::marker::PhantomData))? {
                Ok(s) => InputContent::Text(s),
                Err(parts) => InputContent::Parts(parts),
            },
        )
    }
}

/// One item of `input`.
#[derive(Debug, Clone)]
pub enum InputItem {
    /// `message` — also the shape of an "easy" input message sent without `type`.
    Message(InputMessage),
    FunctionCall(FunctionCallItem),
    FunctionCallOutput(ToolCallOutputItem),
    CustomToolCall(CustomToolCallItem),
    CustomToolCallOutput(ToolCallOutputItem),
    Reasoning(ReasoningItem),
    /// Any other item type (`item_reference`, `web_search_call`, `compaction`, …).
    Unknown(String),
}

impl<'de> Deserialize<'de> for InputItem {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let map = Map::<String, Value>::deserialize(d)?;
        // An EasyInputMessage may omit `type`; it is a message then.
        let kind = take_type(&map, Some("message"))?;
        Ok(match kind.as_str() {
            "message" => InputItem::Message(from_map(map)?),
            "function_call" => InputItem::FunctionCall(from_map(map)?),
            "function_call_output" => InputItem::FunctionCallOutput(from_map(map)?),
            "custom_tool_call" => InputItem::CustomToolCall(from_map(map)?),
            "custom_tool_call_output" => InputItem::CustomToolCallOutput(from_map(map)?),
            "reasoning" => InputItem::Reasoning(from_map(map)?),
            _ => InputItem::Unknown(kind),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct InputMessage {
    pub role: String,
    pub content: InputContent,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FunctionCallItem {
    /// The id the tool call is keyed by — **not** `id`, which is the item's own
    /// platform id (`fc_…`).
    pub call_id: String,
    pub name: String,
    pub arguments: String,
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CustomToolCallItem {
    pub call_id: String,
    pub name: String,
    pub input: String,
    #[serde(default)]
    pub id: Option<String>,
}

/// `function_call_output` / `custom_tool_call_output`.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallOutputItem {
    /// Optional in the SDK types, but required here: without it the output
    /// cannot be matched to its call.
    #[serde(default)]
    pub call_id: Option<String>,
    pub output: InputContent,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReasoningItem {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub summary: Vec<SummaryText>,
    #[serde(default)]
    pub content: Option<Vec<ReasoningText>>,
    #[serde(default)]
    pub encrypted_content: Option<String>,
}

/// `{"type":"summary_text","text":…}` — in a reasoning item's `summary`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SummaryText {
    #[serde(rename = "type", default = "summary_text_type")]
    pub kind: String,
    pub text: String,
}

fn summary_text_type() -> String {
    "summary_text".to_string()
}

impl SummaryText {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            kind: summary_text_type(),
            text: text.into(),
        }
    }
}

/// `{"type":"reasoning_text","text":…}` — in a reasoning item's `content`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReasoningText {
    #[serde(rename = "type", default = "reasoning_text_type")]
    pub kind: String,
    pub text: String,
}

fn reasoning_text_type() -> String {
    "reasoning_text".to_string()
}

/// One part of a message's `content` (or of a tool output list).
#[derive(Debug, Clone)]
pub enum InputContentPart {
    InputText {
        text: String,
    },
    OutputText {
        text: String,
    },
    Refusal {
        refusal: String,
    },
    InputImage(InputImage),
    /// Rejected: needs file storage.
    InputFile,
    Unknown(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct InputImage {
    #[serde(default)]
    pub image_url: Option<String>,
    #[serde(default)]
    pub file_id: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Deserialize)]
struct TextField {
    text: String,
}

#[derive(Deserialize)]
struct RefusalField {
    refusal: String,
}

impl<'de> Deserialize<'de> for InputContentPart {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let map = Map::<String, Value>::deserialize(d)?;
        let kind = take_type(&map, None)?;
        Ok(match kind.as_str() {
            "input_text" => InputContentPart::InputText {
                text: from_map::<TextField, _>(map)?.text,
            },
            "output_text" => InputContentPart::OutputText {
                text: from_map::<TextField, _>(map)?.text,
            },
            "refusal" => InputContentPart::Refusal {
                refusal: from_map::<RefusalField, _>(map)?.refusal,
            },
            "input_image" => InputContentPart::InputImage(from_map(map)?),
            "input_file" => InputContentPart::InputFile,
            _ => InputContentPart::Unknown(kind),
        })
    }
}

/// One entry of `tools`.
#[derive(Debug, Clone)]
pub enum ResponsesTool {
    Function(FunctionTool),
    Custom(CustomTool),
    /// An OpenAI-hosted tool (see [`BUILTIN_TOOL_TYPES`]) — rejected.
    Builtin(String),
    Unknown(String),
}

impl<'de> Deserialize<'de> for ResponsesTool {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let map = Map::<String, Value>::deserialize(d)?;
        let kind = take_type(&map, None)?;
        Ok(match kind.as_str() {
            "function" => ResponsesTool::Function(from_map(map)?),
            "custom" => ResponsesTool::Custom(from_map(map)?),
            k if BUILTIN_TOOL_TYPES.contains(&k) => ResponsesTool::Builtin(kind),
            _ => ResponsesTool::Unknown(kind),
        })
    }
}

/// Flat function tool: `{type:"function", name, description, parameters, strict}`.
#[derive(Debug, Clone, Deserialize)]
pub struct FunctionTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Option<Value>,
    #[serde(default)]
    pub strict: Option<bool>,
}

/// Free-form-input tool: `{type:"custom", name, description, format}`.
#[derive(Debug, Clone, Deserialize)]
pub struct CustomTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// `{type:"text"}` or `{type:"grammar", syntax, definition}`. Not
    /// enforceable on a plain function, so only described to the model.
    #[serde(default)]
    pub format: Option<Value>,
}

/// `text`: output format and verbosity.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TextConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<TextFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<String>,
}

/// `text.format`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TextFormat {
    Text,
    JsonSchema {
        name: String,
        schema: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
    },
    JsonObject,
}

/// `reasoning`: effort and summary preference.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReasoningConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

// ── Response object ──────────────────────────────────────────────────────────

/// The `response` object (non-streaming body, and the payload of the
/// `response.created` / `.in_progress` / `.completed` / `.incomplete` /
/// `.failed` stream events).
///
/// The fields `openai-python` requires (`id`, `created_at`, `model`, `object`,
/// `output`, `parallel_tool_calls`, `tool_choice`, `tools`) are always
/// serialized; nullable ones are serialized as `null`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    /// Always `"response"`.
    pub object: String,
    pub created_at: u64,
    /// `completed`, `incomplete`, `failed`, `in_progress`, …
    pub status: String,
    pub model: String,
    pub output: Vec<OutputItem>,
    #[serde(default)]
    pub usage: Option<ResponseUsage>,
    #[serde(default)]
    pub error: Option<ResponseError>,
    #[serde(default)]
    pub incomplete_details: Option<IncompleteDetails>,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
    pub parallel_tool_calls: bool,
    pub tool_choice: Value,
    pub tools: Vec<Value>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub previous_response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<TextConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

/// One item of a response's `output`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputItem {
    Message {
        id: String,
        /// Always `"assistant"`.
        role: String,
        status: String,
        content: Vec<OutputContent>,
    },
    FunctionCall {
        id: String,
        call_id: String,
        name: String,
        arguments: String,
        status: String,
    },
    CustomToolCall {
        id: String,
        call_id: String,
        name: String,
        input: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },
    Reasoning {
        id: String,
        summary: Vec<SummaryText>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<Vec<ReasoningText>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },
}

/// A part of an output `message`'s `content`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputContent {
    OutputText {
        text: String,
        #[serde(default)]
        annotations: Vec<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        logprobs: Option<Vec<Value>>,
    },
    Refusal {
        refusal: String,
    },
    /// Only in the `content_part.added` / `.done` events of a `reasoning`
    /// item; never inside a `message`.
    ReasoningText {
        text: String,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ResponseUsage {
    pub input_tokens: u32,
    #[serde(default)]
    pub input_tokens_details: InputTokensDetails,
    pub output_tokens: u32,
    #[serde(default)]
    pub output_tokens_details: OutputTokensDetails,
    pub total_tokens: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct InputTokensDetails {
    pub cached_tokens: u32,
    /// Tokens written to the prompt cache. Required by `openai-python`'s
    /// `InputTokensDetails`; `0` when the upstream reported none.
    #[serde(default)]
    pub cache_write_tokens: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OutputTokensDetails {
    pub reasoning_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IncompleteDetails {
    /// `max_output_tokens` or `content_filter`.
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResponseError {
    pub code: String,
    pub message: String,
}

// ── Streaming events ─────────────────────────────────────────────────────────

/// The Responses streaming events the gateway emits. Serialized as the SSE
/// `data:` payload; the SSE `event:` name equals the `type` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ResponseStreamEvent {
    #[serde(rename = "response.created")]
    Created {
        response: Box<Response>,
        sequence_number: u64,
    },
    #[serde(rename = "response.in_progress")]
    InProgress {
        response: Box<Response>,
        sequence_number: u64,
    },
    #[serde(rename = "response.completed")]
    Completed {
        response: Box<Response>,
        sequence_number: u64,
    },
    #[serde(rename = "response.incomplete")]
    Incomplete {
        response: Box<Response>,
        sequence_number: u64,
    },
    #[serde(rename = "response.failed")]
    Failed {
        response: Box<Response>,
        sequence_number: u64,
    },
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded {
        output_index: u32,
        item: OutputItem,
        sequence_number: u64,
    },
    #[serde(rename = "response.output_item.done")]
    OutputItemDone {
        output_index: u32,
        item: OutputItem,
        sequence_number: u64,
    },
    #[serde(rename = "response.content_part.added")]
    ContentPartAdded {
        item_id: String,
        output_index: u32,
        content_index: u32,
        part: OutputContent,
        sequence_number: u64,
    },
    #[serde(rename = "response.content_part.done")]
    ContentPartDone {
        item_id: String,
        output_index: u32,
        content_index: u32,
        part: OutputContent,
        sequence_number: u64,
    },
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        item_id: String,
        output_index: u32,
        content_index: u32,
        delta: String,
        #[serde(default)]
        logprobs: Vec<Value>,
        sequence_number: u64,
    },
    #[serde(rename = "response.output_text.done")]
    OutputTextDone {
        item_id: String,
        output_index: u32,
        content_index: u32,
        text: String,
        #[serde(default)]
        logprobs: Vec<Value>,
        sequence_number: u64,
    },
    #[serde(rename = "response.refusal.delta")]
    RefusalDelta {
        item_id: String,
        output_index: u32,
        content_index: u32,
        delta: String,
        sequence_number: u64,
    },
    #[serde(rename = "response.refusal.done")]
    RefusalDone {
        item_id: String,
        output_index: u32,
        content_index: u32,
        refusal: String,
        sequence_number: u64,
    },
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta {
        item_id: String,
        output_index: u32,
        delta: String,
        sequence_number: u64,
    },
    #[serde(rename = "response.function_call_arguments.done")]
    FunctionCallArgumentsDone {
        item_id: String,
        output_index: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        arguments: String,
        sequence_number: u64,
    },
    #[serde(rename = "response.custom_tool_call_input.delta")]
    CustomToolCallInputDelta {
        item_id: String,
        output_index: u32,
        delta: String,
        sequence_number: u64,
    },
    #[serde(rename = "response.custom_tool_call_input.done")]
    CustomToolCallInputDone {
        item_id: String,
        output_index: u32,
        input: String,
        sequence_number: u64,
    },
    #[serde(rename = "response.reasoning_summary_part.added")]
    ReasoningSummaryPartAdded {
        item_id: String,
        output_index: u32,
        summary_index: u32,
        part: SummaryText,
        sequence_number: u64,
    },
    #[serde(rename = "response.reasoning_summary_part.done")]
    ReasoningSummaryPartDone {
        item_id: String,
        output_index: u32,
        summary_index: u32,
        part: SummaryText,
        sequence_number: u64,
    },
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryTextDelta {
        item_id: String,
        output_index: u32,
        summary_index: u32,
        delta: String,
        sequence_number: u64,
    },
    #[serde(rename = "response.reasoning_summary_text.done")]
    ReasoningSummaryTextDone {
        item_id: String,
        output_index: u32,
        summary_index: u32,
        text: String,
        sequence_number: u64,
    },
    #[serde(rename = "response.reasoning_text.delta")]
    ReasoningTextDelta {
        item_id: String,
        output_index: u32,
        content_index: u32,
        delta: String,
        sequence_number: u64,
    },
    #[serde(rename = "response.reasoning_text.done")]
    ReasoningTextDone {
        item_id: String,
        output_index: u32,
        content_index: u32,
        text: String,
        sequence_number: u64,
    },
    #[serde(rename = "error")]
    Error {
        #[serde(default)]
        code: Option<String>,
        message: String,
        #[serde(default)]
        param: Option<String>,
        sequence_number: u64,
    },
}

impl ResponseStreamEvent {
    /// The event's `type`, used as the SSE `event:` name.
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::Created { .. } => "response.created",
            Self::InProgress { .. } => "response.in_progress",
            Self::Completed { .. } => "response.completed",
            Self::Incomplete { .. } => "response.incomplete",
            Self::Failed { .. } => "response.failed",
            Self::OutputItemAdded { .. } => "response.output_item.added",
            Self::OutputItemDone { .. } => "response.output_item.done",
            Self::ContentPartAdded { .. } => "response.content_part.added",
            Self::ContentPartDone { .. } => "response.content_part.done",
            Self::OutputTextDelta { .. } => "response.output_text.delta",
            Self::OutputTextDone { .. } => "response.output_text.done",
            Self::RefusalDelta { .. } => "response.refusal.delta",
            Self::RefusalDone { .. } => "response.refusal.done",
            Self::FunctionCallArgumentsDelta { .. } => "response.function_call_arguments.delta",
            Self::FunctionCallArgumentsDone { .. } => "response.function_call_arguments.done",
            Self::CustomToolCallInputDelta { .. } => "response.custom_tool_call_input.delta",
            Self::CustomToolCallInputDone { .. } => "response.custom_tool_call_input.done",
            Self::ReasoningSummaryPartAdded { .. } => "response.reasoning_summary_part.added",
            Self::ReasoningSummaryPartDone { .. } => "response.reasoning_summary_part.done",
            Self::ReasoningSummaryTextDelta { .. } => "response.reasoning_summary_text.delta",
            Self::ReasoningSummaryTextDone { .. } => "response.reasoning_summary_text.done",
            Self::ReasoningTextDelta { .. } => "response.reasoning_text.delta",
            Self::ReasoningTextDone { .. } => "response.reasoning_text.done",
            Self::Error { .. } => "error",
        }
    }
}

// ── Responses → ChatCompletion translation ───────────────────────────────────

fn invalid(message: impl Into<String>, param: impl Into<String>) -> ProxyError {
    ProxyError::InvalidRequest {
        message: message.into(),
        param: Some(param.into()),
    }
}

/// Translate a Responses create request into the internal
/// [`ChatCompletionRequest`].
///
/// Fails with [`ProxyError::InvalidRequest`] (a 400 naming the `param`) for
/// anything this stateless gateway cannot honour — it never silently drops a
/// feature the client asked for.
pub fn to_chat_completion_request(
    req: &ResponsesRequest,
) -> Result<ChatCompletionRequest, ProxyError> {
    reject_stateful_features(req)?;

    let mut extra: HashMap<String, Value> = HashMap::new();

    let (tools, custom_tools) = match &req.tools {
        Some(tools) => {
            let (t, c) = translate_tools(tools)?;
            (Some(t), c)
        }
        None => (None, Vec::new()),
    };
    if !custom_tools.is_empty() {
        extra.insert(
            RESPONSES_CUSTOM_TOOLS.to_string(),
            Value::from(custom_tools),
        );
    }

    let tool_choice = req
        .tool_choice
        .as_ref()
        .map(translate_tool_choice)
        .transpose()?;

    let mut messages = Vec::new();
    if let Some(instructions) = &req.instructions {
        messages.push(text_message("system", instructions.clone()));
    }
    let mut thinking_blocks = Vec::new();
    match &req.input {
        Some(ResponsesInput::Text(text)) => messages.push(text_message("user", text.clone())),
        Some(ResponsesInput::Items(items)) => {
            translate_items(items, &mut messages, &mut thinking_blocks)?;
        }
        None => return Err(invalid("`input` is required", "input")),
    }
    if !thinking_blocks.is_empty() {
        extra.insert(
            RESPONSES_ANTHROPIC_THINKING_BLOCKS.to_string(),
            Value::Array(thinking_blocks),
        );
    }

    if let Some(text) = &req.text {
        if let Some(format) = &text.format {
            if let Some(rf) = response_format(format) {
                extra.insert("response_format".to_string(), rf);
            }
        }
        if let Some(v) = &text.verbosity {
            extra.insert(
                RESPONSES_TEXT_VERBOSITY.to_string(),
                Value::from(v.as_str()),
            );
        }
    }

    if let Some(reasoning) = &req.reasoning {
        if let Some(effort) = &reasoning.effort {
            // Only the OpenAI-shaped knob. Which Anthropic thinking mode an
            // effort maps to depends on the target model (manual budgets are
            // rejected by Claude 4.7+), so that belongs in the Anthropic
            // adapter, not in this model-blind translation.
            extra.insert("reasoning_effort".to_string(), Value::from(effort.as_str()));
        }
        if let Some(summary) = &reasoning.summary {
            extra.insert(
                RESPONSES_REASONING_SUMMARY.to_string(),
                Value::from(summary.as_str()),
            );
        }
    }

    let passthrough: [(&str, Option<Value>); 6] = [
        (
            "parallel_tool_calls",
            req.parallel_tool_calls.map(Value::from),
        ),
        ("metadata", req.metadata.clone()),
        (
            "prompt_cache_key",
            req.prompt_cache_key.as_deref().map(Value::from),
        ),
        (
            "safety_identifier",
            req.safety_identifier.as_deref().map(Value::from),
        ),
        ("user", req.user.as_deref().map(Value::from)),
        ("service_tier", req.service_tier.as_deref().map(Value::from)),
    ];
    for (key, value) in passthrough {
        if let Some(v) = value {
            extra.insert(key.to_string(), v);
        }
    }

    if let Some(include) = &req.include {
        extra.insert(RESPONSES_INCLUDE.to_string(), Value::from(include.clone()));
    }
    if let Some(t) = &req.truncation {
        extra.insert(RESPONSES_TRUNCATION.to_string(), Value::from(t.as_str()));
    }
    if let Some(n) = req.max_tool_calls {
        extra.insert(RESPONSES_MAX_TOOL_CALLS.to_string(), Value::from(n));
    }

    Ok(ChatCompletionRequest {
        model: req.model.clone(),
        messages,
        stream: req.stream,
        temperature: req.temperature,
        max_tokens: req.max_output_tokens,
        top_p: req.top_p,
        stop: None,
        tools,
        tool_choice,
        system: None,
        extra_headers: HashMap::new(),
        raw_anthropic_body: None,
        extra,
    })
}

fn reject_stateful_features(req: &ResponsesRequest) -> Result<(), ProxyError> {
    if req.previous_response_id.is_some() {
        return Err(invalid(
            "`previous_response_id` is not supported: this endpoint is stateless — \
             send the full conversation in `input`",
            "previous_response_id",
        ));
    }
    if req.conversation.as_ref().is_some_and(|c| !c.is_null()) {
        return Err(invalid(
            "`conversation` is not supported: this endpoint is stateless — \
             send the full conversation in `input`",
            "conversation",
        ));
    }
    if req.extra.get("prompt").is_some_and(|p| !p.is_null()) {
        return Err(invalid(
            "`prompt` templates are not supported: this endpoint is stateless — \
             send the instructions and input directly",
            "prompt",
        ));
    }
    if req.background == Some(true) {
        return Err(invalid(
            "`background` mode is not supported by this endpoint",
            "background",
        ));
    }
    Ok(())
}

fn text_message(role: &str, text: String) -> ChatMessage {
    ChatMessage {
        role: role.to_string(),
        content: Some(MessageContent::Text(text)),
        name: None,
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        extra: HashMap::new(),
    }
}

/// Map `input` items onto chat messages, appending to `messages`. Consecutive
/// `function_call` / `custom_tool_call` items collapse into one assistant
/// message; `reasoning` items are dropped, except a Ferrox-encoded Anthropic
/// signature, which is collected into `thinking_blocks`.
fn translate_items(
    items: &[InputItem],
    messages: &mut Vec<ChatMessage>,
    thinking_blocks: &mut Vec<Value>,
) -> Result<(), ProxyError> {
    let mut pending_calls: Vec<ToolCall> = Vec::new();

    fn flush(pending: &mut Vec<ToolCall>, messages: &mut Vec<ChatMessage>) {
        if pending.is_empty() {
            return;
        }
        messages.push(ChatMessage {
            role: "assistant".to_string(),
            content: None,
            name: None,
            tool_calls: Some(std::mem::take(pending)),
            tool_call_id: None,
            reasoning_content: None,
            extra: HashMap::new(),
        });
    }

    for (i, item) in items.iter().enumerate() {
        match item {
            InputItem::FunctionCall(call) => pending_calls.push(ToolCall {
                id: call.call_id.clone(),
                r#type: "function".to_string(),
                function: FunctionCall {
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                },
            }),
            InputItem::CustomToolCall(call) => pending_calls.push(ToolCall {
                id: call.call_id.clone(),
                r#type: "function".to_string(),
                function: FunctionCall {
                    name: call.name.clone(),
                    arguments: serde_json::json!({ "input": call.input }).to_string(),
                },
            }),
            InputItem::Reasoning(reasoning) => {
                // A reasoning item sits inside a run of calls without ending it.
                if let Some(signature) = reasoning
                    .encrypted_content
                    .as_deref()
                    .and_then(decode_anthropic_thinking_signature)
                {
                    thinking_blocks.push(serde_json::json!({
                        "message_index": messages.len(),
                        "thinking": reasoning_text(reasoning),
                        "signature": signature,
                    }));
                }
            }
            InputItem::Message(msg) => {
                flush(&mut pending_calls, messages);
                messages.push(translate_message(msg, i)?);
            }
            InputItem::FunctionCallOutput(out) | InputItem::CustomToolCallOutput(out) => {
                flush(&mut pending_calls, messages);
                let call_id = out.call_id.clone().ok_or_else(|| {
                    invalid(
                        "a tool call output needs the `call_id` of its call",
                        format!("input[{i}].call_id"),
                    )
                })?;
                messages.push(ChatMessage {
                    role: "tool".to_string(),
                    content: Some(translate_content(
                        &out.output,
                        &format!("input[{i}].output"),
                    )?),
                    name: None,
                    tool_calls: None,
                    tool_call_id: Some(call_id),
                    reasoning_content: None,
                    extra: HashMap::new(),
                });
            }
            InputItem::Unknown(kind) => {
                return Err(invalid(
                    format!("input item type `{kind}` is not supported"),
                    format!("input[{i}]"),
                ));
            }
        }
    }
    flush(&mut pending_calls, messages);
    Ok(())
}

/// The visible text of a reasoning item: its raw `content` when present,
/// otherwise its summary.
fn reasoning_text(item: &ReasoningItem) -> String {
    match &item.content {
        Some(content) if !content.is_empty() => {
            content.iter().map(|c| c.text.as_str()).collect::<String>()
        }
        _ => item
            .summary
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

fn translate_message(msg: &InputMessage, index: usize) -> Result<ChatMessage, ProxyError> {
    let role = match msg.role.as_str() {
        "user" | "assistant" | "system" => msg.role.clone(),
        "developer" => "system".to_string(),
        other => {
            return Err(invalid(
                format!("message role `{other}` is not supported"),
                format!("input[{index}].role"),
            ))
        }
    };
    let content = translate_content(&msg.content, &format!("input[{index}].content"))?;
    Ok(ChatMessage {
        role,
        content: Some(content),
        name: None,
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        extra: HashMap::new(),
    })
}

/// Content (message content or tool output) → chat content. A lone text part
/// collapses to a plain string, which every upstream accepts.
fn translate_content(content: &InputContent, param: &str) -> Result<MessageContent, ProxyError> {
    let parts = match content {
        InputContent::Text(t) => return Ok(MessageContent::Text(t.clone())),
        InputContent::Parts(parts) => parts,
    };
    let mut out = Vec::with_capacity(parts.len());
    for (j, part) in parts.iter().enumerate() {
        let text = |t: &str| ContentPart::Text {
            text: t.to_string(),
            extra: HashMap::new(),
        };
        out.push(match part {
            InputContentPart::InputText { text: t } | InputContentPart::OutputText { text: t } => {
                text(t)
            }
            InputContentPart::Refusal { refusal } => text(refusal),
            InputContentPart::InputImage(img) => match &img.image_url {
                Some(url) => ContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: url.clone(),
                        detail: img.detail.clone(),
                    },
                    extra: HashMap::new(),
                },
                None => {
                    // Name the field the client actually sent (or should have).
                    let field = if img.file_id.is_some() {
                        "file_id"
                    } else {
                        "image_url"
                    };
                    return Err(invalid(
                        "`input_image` needs an `image_url`; `file_id` is not supported",
                        format!("{param}[{j}].{field}"),
                    ));
                }
            },
            InputContentPart::InputFile => {
                return Err(invalid(
                    "`input_file` content is not supported",
                    format!("{param}[{j}]"),
                ))
            }
            InputContentPart::Unknown(kind) => {
                return Err(invalid(
                    format!("content type `{kind}` is not supported"),
                    format!("{param}[{j}]"),
                ))
            }
        });
    }
    if let [ContentPart::Text { text, .. }] = out.as_mut_slice() {
        return Ok(MessageContent::Text(std::mem::take(text)));
    }
    Ok(MessageContent::Parts(out))
}

/// Flat Responses tools → nested chat tools, plus the names of the `custom`
/// tools (see [`RESPONSES_CUSTOM_TOOLS`]).
fn translate_tools(tools: &[ResponsesTool]) -> Result<(Vec<Tool>, Vec<String>), ProxyError> {
    let mut out = Vec::with_capacity(tools.len());
    let mut custom = Vec::new();
    for (i, tool) in tools.iter().enumerate() {
        out.push(match tool {
            ResponsesTool::Function(f) => Tool {
                r#type: "function".to_string(),
                function: ToolFunction {
                    name: f.name.clone(),
                    description: f.description.clone(),
                    parameters: f.parameters.clone(),
                    strict: f.strict,
                },
            },
            ResponsesTool::Custom(c) => {
                custom.push(c.name.clone());
                custom_tool_as_function(c)
            }
            ResponsesTool::Builtin(kind) => {
                return Err(invalid(
                    format!(
                        "built-in tool `{kind}` is not supported; declare it as a function tool"
                    ),
                    format!("tools[{i}].type"),
                ))
            }
            ResponsesTool::Unknown(kind) => {
                return Err(invalid(
                    format!("tool type `{kind}` is not supported"),
                    format!("tools[{i}].type"),
                ))
            }
        });
    }
    Ok((out, custom))
}

/// A `custom` tool becomes a function taking one string parameter `input`.
/// A grammar `format` cannot be enforced on a function, so it is appended to
/// the description for the model to follow.
fn custom_tool_as_function(tool: &CustomTool) -> Tool {
    let mut input_schema = serde_json::json!({"type": "string"});
    if let Some(format) = &tool.format {
        if format.get("type").and_then(Value::as_str) == Some("grammar") {
            let syntax = format.get("syntax").and_then(Value::as_str).unwrap_or("");
            let definition = format
                .get("definition")
                .and_then(Value::as_str)
                .unwrap_or("");
            input_schema["description"] =
                Value::from(format!("Must match this {syntax} grammar:\n{definition}"));
        }
    }
    Tool {
        r#type: "function".to_string(),
        function: ToolFunction {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {"input": input_schema},
                "required": ["input"],
                "additionalProperties": false,
            })),
            strict: None,
        },
    }
}

/// Responses `tool_choice` → chat `tool_choice`. `auto`/`none`/`required`
/// pass through; a flat `{type:"function"|"custom", name}` becomes the nested
/// `{type:"function", function:{name}}`. Hosted-tool and `allowed_tools`
/// choices have no chat equivalent and are rejected.
fn translate_tool_choice(choice: &Value) -> Result<Value, ProxyError> {
    match choice {
        Value::String(s) if matches!(s.as_str(), "auto" | "none" | "required") => {
            Ok(choice.clone())
        }
        Value::Object(obj) => {
            let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
            let name = obj.get("name").and_then(Value::as_str);
            match (kind, name) {
                ("function" | "custom", Some(name)) => Ok(serde_json::json!({
                    "type": "function",
                    "function": {"name": name},
                })),
                ("function" | "custom", None) => Err(invalid(
                    "`tool_choice.name` is required",
                    "tool_choice.name",
                )),
                _ => Err(invalid(
                    format!("tool_choice type `{kind}` is not supported"),
                    "tool_choice",
                )),
            }
        }
        _ => Err(invalid("unsupported `tool_choice` value", "tool_choice")),
    }
}

/// `text.format` → chat `response_format`. `text` is the default and needs none.
fn response_format(format: &TextFormat) -> Option<Value> {
    match format {
        TextFormat::Text => None,
        TextFormat::JsonObject => Some(serde_json::json!({"type": "json_object"})),
        TextFormat::JsonSchema {
            name,
            schema,
            description,
            strict,
        } => {
            let mut js = serde_json::json!({"name": name, "schema": schema});
            if let Some(d) = description {
                js["description"] = Value::from(d.as_str());
            }
            if let Some(s) = strict {
                js["strict"] = Value::from(*s);
            }
            Some(serde_json::json!({"type": "json_schema", "json_schema": js}))
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: Value) -> ResponsesRequest {
        serde_json::from_value(v).expect("request deserializes")
    }

    fn translate(v: Value) -> ChatCompletionRequest {
        to_chat_completion_request(&parse(v)).expect("request translates")
    }

    /// Assert the request is rejected with a 400 naming `param`.
    fn rejected(v: Value, param: &str) {
        let err = to_chat_completion_request(&parse(v)).expect_err("must be rejected");
        match &err {
            ProxyError::InvalidRequest { param: p, .. } => {
                assert_eq!(p.as_deref(), Some(param), "{err}");
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
        let (status, body) = crate::error::openai_error_body(&err);
        assert_eq!(status, 400);
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["param"], param);
    }

    fn text_of(m: &ChatMessage) -> &str {
        match m.content.as_ref().expect("content") {
            MessageContent::Text(t) => t,
            MessageContent::Parts(_) => panic!("expected text content"),
        }
    }

    /// A multi-turn Codex CLI request: instructions, developer + user context
    /// messages, reasoning with encrypted content, a shell function call and
    /// its output, an apply_patch custom tool call and its output.
    fn codex_fixture() -> Value {
        json!({
            "model": "gpt-5-codex",
            "instructions": "You are Codex, a coding agent.",
            "input": [
                {"type": "message", "role": "developer", "content": [
                    {"type": "input_text", "text": "<permissions>workspace-write</permissions>"}
                ]},
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "<environment_context>cwd=/repo</environment_context>"},
                    {"type": "input_text", "text": "fix the failing test"}
                ]},
                {"type": "reasoning", "id": "rs_1", "summary": [
                    {"type": "summary_text", "text": "Looking at the tests"}
                ], "content": null, "encrypted_content": "gAAAAopaque"},
                {"type": "function_call", "id": "fc_1", "name": "shell",
                 "arguments": "{\"command\":[\"cargo\",\"test\"]}", "call_id": "call_A"},
                {"type": "function_call_output", "call_id": "call_A",
                 "output": "test result: FAILED"},
                {"type": "custom_tool_call", "id": "ctc_1", "status": "completed",
                 "call_id": "call_B", "name": "apply_patch",
                 "input": "*** Begin Patch\n*** End Patch"},
                {"type": "custom_tool_call_output", "call_id": "call_B", "output": "Done!"},
                {"type": "message", "role": "assistant", "id": "msg_1", "phase": "final_answer",
                 "content": [{"type": "output_text", "text": "Fixed.", "annotations": []}]}
            ],
            "tools": [
                {"type": "function", "name": "shell", "description": "Runs a shell command",
                 "strict": false, "parameters": {"type": "object",
                  "properties": {"command": {"type": "array", "items": {"type": "string"}}},
                  "required": ["command"], "additionalProperties": false}},
                {"type": "custom", "name": "apply_patch", "description": "Edit files",
                 "format": {"type": "grammar", "syntax": "lark", "definition": "start: patch"}}
            ],
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "reasoning": {"effort": "medium", "summary": "auto"},
            "store": false,
            "stream": true,
            "include": ["reasoning.encrypted_content"],
            "prompt_cache_key": "0199-session",
            "text": {"verbosity": "low"}
        })
    }

    // ── Codex / SDK fixtures ────────────────────────────────────────────────

    #[test]
    fn codex_request_translates_end_to_end() {
        let req = translate(codex_fixture());
        let roles: Vec<&str> = req.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(
            roles,
            [
                "system",
                "system",
                "user",
                "assistant",
                "tool",
                "assistant",
                "tool",
                "assistant"
            ]
        );
        assert_eq!(req.model, "gpt-5-codex");
        assert_eq!(req.stream, Some(true));
        assert_eq!(req.tools.as_ref().unwrap().len(), 2);
        assert_eq!(req.tool_choice, Some(json!("auto")));
        assert_eq!(req.extra["prompt_cache_key"], "0199-session");
        assert_eq!(req.extra["parallel_tool_calls"], false);
        assert_eq!(
            req.extra[RESPONSES_INCLUDE],
            json!(["reasoning.encrypted_content"])
        );
        assert_eq!(req.extra[RESPONSES_TEXT_VERBOSITY], "low");
        assert_eq!(req.extra[RESPONSES_REASONING_SUMMARY], "auto");
        assert_eq!(req.extra[RESPONSES_CUSTOM_TOOLS], json!(["apply_patch"]));
        assert!(!req.extra.contains_key("store"));
    }

    #[test]
    fn codex_tool_calls_are_keyed_by_call_id_not_item_id() {
        let req = translate(codex_fixture());
        let call = &req.messages[3].tool_calls.as_ref().unwrap()[0];
        assert_eq!(call.id, "call_A");
        assert_eq!(req.messages[4].tool_call_id.as_deref(), Some("call_A"));
        let custom = &req.messages[5].tool_calls.as_ref().unwrap()[0];
        assert_eq!(custom.id, "call_B");
        assert_eq!(req.messages[6].tool_call_id.as_deref(), Some("call_B"));
    }

    #[test]
    fn openai_python_function_calling_example_translates() {
        // From the openai-python README / function-calling guide.
        let req = translate(json!({
            "model": "gpt-4.1",
            "input": [
                {"role": "user", "content": "What's the weather in Paris?"},
                {"type": "function_call", "call_id": "call_12345xyz", "name": "get_weather",
                 "arguments": "{\"location\":\"Paris, France\"}"},
                {"type": "function_call_output", "call_id": "call_12345xyz", "output": "15C"}
            ],
            "tools": [{"type": "function", "name": "get_weather", "strict": true,
                       "parameters": {"type": "object", "properties": {"location": {"type": "string"}},
                                      "required": ["location"], "additionalProperties": false}}]
        }));
        assert_eq!(req.messages.len(), 3);
        assert_eq!(text_of(&req.messages[0]), "What's the weather in Paris?");
        assert_eq!(text_of(&req.messages[2]), "15C");
        assert_eq!(req.tools.unwrap()[0].function.strict, Some(true));
    }

    #[test]
    fn unknown_top_level_fields_are_tolerated() {
        let parsed = parse(json!({
            "model": "m", "input": "hi",
            "prompt_cache_retention": "24h", "some_future_field": {"x": 1},
            "stream_options": {"include_obfuscation": false}
        }));
        assert!(parsed.extra.contains_key("some_future_field"));
        let req = to_chat_completion_request(&parsed).unwrap();
        assert!(!req.extra.contains_key("some_future_field"));
        assert!(!req.extra.contains_key("prompt_cache_retention"));
    }

    // ── input ───────────────────────────────────────────────────────────────

    #[test]
    fn string_input_is_one_user_message() {
        let req = translate(json!({"model": "m", "input": "hello"}));
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, "user");
        assert_eq!(text_of(&req.messages[0]), "hello");
    }

    #[test]
    fn missing_input_is_rejected() {
        rejected(json!({"model": "m"}), "input");
    }

    #[test]
    fn instructions_become_the_first_system_message() {
        let req = translate(json!({"model": "m", "instructions": "be terse", "input": "hi"}));
        assert_eq!(req.messages[0].role, "system");
        assert_eq!(text_of(&req.messages[0]), "be terse");
        assert_eq!(req.messages[1].role, "user");
    }

    #[test]
    fn developer_role_maps_to_system() {
        let req = translate(json!({"model": "m", "input": [
            {"type": "message", "role": "developer", "content": "rules"}
        ]}));
        assert_eq!(req.messages[0].role, "system");
    }

    #[test]
    fn user_assistant_system_roles_pass_through() {
        let req = translate(json!({"model": "m", "input": [
            {"role": "system", "content": "s"},
            {"role": "user", "content": "u"},
            {"role": "assistant", "content": "a"}
        ]}));
        let roles: Vec<&str> = req.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["system", "user", "assistant"]);
    }

    #[test]
    fn unknown_role_is_rejected() {
        rejected(
            json!({"model": "m", "input": [{"role": "critic", "content": "x"}]}),
            "input[0].role",
        );
    }

    #[test]
    fn easy_message_without_type_is_a_message() {
        let req = translate(json!({"model": "m", "input": [{"role": "user", "content": "hi"}]}));
        assert_eq!(text_of(&req.messages[0]), "hi");
    }

    #[test]
    fn single_text_part_collapses_to_a_string() {
        let req = translate(json!({"model": "m", "input": [
            {"role": "user", "content": [{"type": "input_text", "text": "only"}]}
        ]}));
        assert_eq!(text_of(&req.messages[0]), "only");
    }

    #[test]
    fn input_text_and_output_text_become_text_parts() {
        let req = translate(json!({"model": "m", "input": [
            {"role": "assistant", "content": [
                {"type": "output_text", "text": "a", "annotations": []},
                {"type": "output_text", "text": "b"}
            ]},
            {"role": "user", "content": [
                {"type": "input_text", "text": "c"}, {"type": "input_text", "text": "d"}
            ]}
        ]}));
        for m in &req.messages {
            match m.content.as_ref().unwrap() {
                MessageContent::Parts(p) => {
                    assert_eq!(p.len(), 2);
                    assert!(p.iter().all(|x| matches!(x, ContentPart::Text { .. })));
                }
                MessageContent::Text(_) => panic!("expected parts"),
            }
        }
    }

    #[test]
    fn input_image_becomes_image_url_with_detail() {
        let req = translate(json!({"model": "m", "input": [{"role": "user", "content": [
            {"type": "input_text", "text": "what is this"},
            {"type": "input_image", "image_url": "data:image/png;base64,AAAA", "detail": "high"}
        ]}]}));
        let MessageContent::Parts(parts) = req.messages[0].content.as_ref().unwrap() else {
            panic!("expected parts")
        };
        match &parts[1] {
            ContentPart::ImageUrl { image_url, .. } => {
                assert_eq!(image_url.url, "data:image/png;base64,AAAA");
                assert_eq!(image_url.detail.as_deref(), Some("high"));
            }
            other => panic!("expected image, got {other:?}"),
        }
    }

    #[test]
    fn input_image_by_file_id_is_rejected() {
        rejected(
            json!({"model": "m", "input": [{"role": "user", "content": [
                {"type": "input_image", "file_id": "file-123", "detail": "auto"}
            ]}]}),
            "input[0].content[0].file_id",
        );
    }

    #[test]
    fn input_image_without_a_source_names_image_url() {
        rejected(
            json!({"model": "m", "input": [{"role": "user", "content": [
                {"type": "input_image", "detail": "auto"}
            ]}]}),
            "input[0].content[0].image_url",
        );
    }

    #[test]
    fn input_file_is_rejected() {
        rejected(
            json!({"model": "m", "input": [{"role": "user", "content": [
                {"type": "input_text", "text": "summarize"},
                {"type": "input_file", "file_id": "file-abc"}
            ]}]}),
            "input[0].content[1]",
        );
    }

    #[test]
    fn unknown_content_part_is_rejected() {
        rejected(
            json!({"model": "m", "input": [{"role": "user", "content": [
                {"type": "input_audio", "input_audio": {}}
            ]}]}),
            "input[0].content[0]",
        );
    }

    #[test]
    fn consecutive_function_calls_merge_into_one_assistant_message() {
        let req = translate(json!({"model": "m", "input": [
            {"role": "user", "content": "go"},
            {"type": "function_call", "call_id": "c1", "name": "a", "arguments": "{}"},
            {"type": "function_call", "call_id": "c2", "name": "b", "arguments": "{\"x\":1}"},
            {"type": "function_call_output", "call_id": "c1", "output": "r1"},
            {"type": "function_call_output", "call_id": "c2", "output": "r2"}
        ]}));
        assert_eq!(req.messages.len(), 4);
        let calls = req.messages[1].tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].function.arguments, "{\"x\":1}");
        assert!(req.messages[1].content.is_none());
    }

    #[test]
    fn reasoning_between_calls_does_not_split_the_run() {
        let req = translate(json!({"model": "m", "input": [
            {"type": "function_call", "call_id": "c1", "name": "a", "arguments": "{}"},
            {"type": "reasoning", "id": "rs", "summary": []},
            {"type": "function_call", "call_id": "c2", "name": "b", "arguments": "{}"}
        ]}));
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].tool_calls.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn function_calls_separated_by_output_are_separate_messages() {
        let req = translate(json!({"model": "m", "input": [
            {"type": "function_call", "call_id": "c1", "name": "a", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c1", "output": "r"},
            {"type": "function_call", "call_id": "c2", "name": "a", "arguments": "{}"}
        ]}));
        assert_eq!(req.messages.len(), 3);
        assert_eq!(req.messages[2].tool_calls.as_ref().unwrap()[0].id, "c2");
    }

    #[test]
    fn custom_tool_call_input_is_wrapped_as_json_arguments() {
        let req = translate(json!({"model": "m", "input": [
            {"type": "custom_tool_call", "call_id": "c", "name": "apply_patch", "input": "a \"b\"\n"}
        ]}));
        let call = &req.messages[0].tool_calls.as_ref().unwrap()[0];
        let args: Value = serde_json::from_str(&call.function.arguments).unwrap();
        assert_eq!(args, json!({"input": "a \"b\"\n"}));
        assert_eq!(call.r#type, "function");
    }

    #[test]
    fn function_call_output_list_becomes_parts() {
        let req = translate(json!({"model": "m", "input": [
            {"type": "function_call_output", "call_id": "c", "output": [
                {"type": "input_text", "text": "see image"},
                {"type": "input_image", "image_url": "https://x/y.png"}
            ]}
        ]}));
        assert_eq!(req.messages[0].role, "tool");
        assert!(matches!(
            req.messages[0].content,
            Some(MessageContent::Parts(ref p)) if p.len() == 2
        ));
    }

    #[test]
    fn function_call_output_without_call_id_is_rejected() {
        rejected(
            json!({"model": "m", "input": [{"type": "function_call_output", "output": "x"}]}),
            "input[0].call_id",
        );
    }

    #[test]
    fn plain_reasoning_items_are_dropped() {
        let req = translate(json!({"model": "m", "input": [
            {"role": "user", "content": "hi"},
            {"type": "reasoning", "id": "rs", "summary": [{"type": "summary_text", "text": "t"}],
             "encrypted_content": "gAAAA-openai-blob"},
            {"role": "assistant", "content": "hello"}
        ]}));
        assert_eq!(req.messages.len(), 2);
        assert!(!req.extra.contains_key(RESPONSES_ANTHROPIC_THINKING_BLOCKS));
    }

    #[test]
    fn ferrox_encoded_thinking_signature_is_kept() {
        let enc = encode_anthropic_thinking_signature("sig-abc");
        let req = translate(json!({"model": "m", "input": [
            {"role": "user", "content": "hi"},
            {"type": "reasoning", "id": "rs", "summary": [{"type": "summary_text", "text": "thought"}],
             "encrypted_content": enc},
            {"type": "function_call", "call_id": "c", "name": "f", "arguments": "{}"}
        ]}));
        let blocks = &req.extra[RESPONSES_ANTHROPIC_THINKING_BLOCKS];
        assert_eq!(
            blocks,
            &json!([{"message_index": 1, "thinking": "thought", "signature": "sig-abc"}])
        );
        assert_eq!(req.messages[1].role, "assistant");
    }

    #[test]
    fn thinking_block_prefers_raw_reasoning_content() {
        let enc = encode_anthropic_thinking_signature("s");
        let req = translate(json!({"model": "m", "input": [
            {"type": "reasoning", "id": "rs", "summary": [{"type": "summary_text", "text": "sum"}],
             "content": [{"type": "reasoning_text", "text": "raw"}], "encrypted_content": enc}
        ]}));
        assert_eq!(
            req.extra[RESPONSES_ANTHROPIC_THINKING_BLOCKS][0]["thinking"],
            "raw"
        );
    }

    #[test]
    fn signature_encoding_round_trips() {
        let enc = encode_anthropic_thinking_signature("EqQBCkgIARABGAIi");
        assert_eq!(
            decode_anthropic_thinking_signature(&enc),
            Some("EqQBCkgIARABGAIi")
        );
        assert_eq!(decode_anthropic_thinking_signature("gAAAAB"), None);
        assert_eq!(
            decode_anthropic_thinking_signature(FERROX_ANTHROPIC_SIGNATURE_PREFIX),
            None
        );
    }

    #[test]
    fn unknown_input_item_is_rejected_naming_it() {
        let err = to_chat_completion_request(&parse(json!({"model": "m", "input": [
            {"role": "user", "content": "hi"},
            {"type": "item_reference", "id": "msg_1"}
        ]})))
        .unwrap_err();
        assert!(err.to_string().contains("item_reference"), "{err}");
        rejected(
            json!({"model": "m", "input": [{"type": "web_search_call", "id": "ws"}]}),
            "input[0]",
        );
    }

    #[test]
    fn malformed_known_item_is_a_deserialize_error() {
        let r: Result<ResponsesRequest, _> = serde_json::from_value(json!({
            "model": "m", "input": [{"type": "function_call", "name": "f", "arguments": "{}"}]
        }));
        assert!(r.unwrap_err().to_string().contains("call_id"));
    }

    // ── tools / tool_choice ─────────────────────────────────────────────────

    #[test]
    fn flat_function_tool_becomes_nested() {
        let req = translate(json!({"model": "m", "input": "x", "tools": [
            {"type": "function", "name": "f", "description": "d",
             "parameters": {"type": "object", "properties": {}}}
        ]}));
        let t = &req.tools.unwrap()[0];
        assert_eq!(t.r#type, "function");
        assert_eq!(t.function.name, "f");
        assert_eq!(t.function.description.as_deref(), Some("d"));
        assert_eq!(
            t.function.parameters,
            Some(json!({"type": "object", "properties": {}}))
        );
        assert_eq!(t.function.strict, None);
    }

    #[test]
    fn custom_tool_becomes_function_with_string_input() {
        let req = translate(json!({"model": "m", "input": "x", "tools": [
            {"type": "custom", "name": "apply_patch", "description": "patch",
             "format": {"type": "grammar", "syntax": "lark", "definition": "start: x"}}
        ]}));
        let f = &req.tools.unwrap()[0].function;
        let params = f.parameters.as_ref().unwrap();
        assert_eq!(params["properties"]["input"]["type"], "string");
        assert_eq!(params["required"], json!(["input"]));
        assert!(params["properties"]["input"]["description"]
            .as_str()
            .unwrap()
            .contains("start: x"));
        assert_eq!(req.extra[RESPONSES_CUSTOM_TOOLS], json!(["apply_patch"]));
    }

    #[test]
    fn every_builtin_tool_is_rejected() {
        for kind in [
            "web_search",
            "file_search",
            "code_interpreter",
            "computer",
            "mcp",
            "image_generation",
            "shell",
            "local_shell",
            "apply_patch",
            "tool_search",
            "namespace",
        ] {
            rejected(
                json!({"model": "m", "input": "x", "tools": [
                    {"type": "function", "name": "ok", "parameters": {}},
                    {"type": kind}
                ]}),
                "tools[1].type",
            );
        }
    }

    #[test]
    fn unknown_tool_type_is_rejected() {
        rejected(
            json!({"model": "m", "input": "x", "tools": [{"type": "hologram"}]}),
            "tools[0].type",
        );
    }

    #[test]
    fn tool_choice_strings_pass_through() {
        for s in ["auto", "none", "required"] {
            let req = translate(json!({"model": "m", "input": "x", "tool_choice": s}));
            assert_eq!(req.tool_choice, Some(json!(s)));
        }
    }

    #[test]
    fn flat_function_tool_choice_becomes_nested() {
        let req = translate(json!({"model": "m", "input": "x",
            "tool_choice": {"type": "function", "name": "f"}}));
        assert_eq!(
            req.tool_choice,
            Some(json!({"type": "function", "function": {"name": "f"}}))
        );
        let req = translate(json!({"model": "m", "input": "x",
            "tool_choice": {"type": "custom", "name": "apply_patch"}}));
        assert_eq!(req.tool_choice.unwrap()["function"]["name"], "apply_patch");
    }

    #[test]
    fn hosted_tool_choice_is_rejected() {
        rejected(
            json!({"model": "m", "input": "x", "tool_choice": {"type": "web_search_preview"}}),
            "tool_choice",
        );
        rejected(
            json!({"model": "m", "input": "x", "tool_choice": "sometimes"}),
            "tool_choice",
        );
        rejected(
            json!({"model": "m", "input": "x", "tool_choice": {"type": "function"}}),
            "tool_choice.name",
        );
    }

    // ── sampling / output fields ────────────────────────────────────────────

    #[test]
    fn scalar_fields_map_across() {
        let req = translate(json!({
            "model": "m", "input": "x", "temperature": 0.2, "top_p": 0.9,
            "max_output_tokens": 500, "metadata": {"k": "v"},
            "safety_identifier": "sid", "user": "u1", "service_tier": "flex"
        }));
        assert_eq!(req.temperature, Some(0.2));
        assert_eq!(req.top_p, Some(0.9));
        assert_eq!(req.max_tokens, Some(500));
        assert_eq!(req.extra["metadata"], json!({"k": "v"}));
        assert_eq!(req.extra["safety_identifier"], "sid");
        assert_eq!(req.extra["user"], "u1");
        assert_eq!(req.extra["service_tier"], "flex");
    }

    #[test]
    fn json_schema_format_becomes_response_format() {
        let req = translate(json!({"model": "m", "input": "x", "text": {"format": {
            "type": "json_schema", "name": "out", "strict": true,
            "schema": {"type": "object"}
        }}}));
        assert_eq!(
            req.extra["response_format"],
            json!({"type": "json_schema",
                   "json_schema": {"name": "out", "schema": {"type": "object"}, "strict": true}})
        );
    }

    #[test]
    fn json_object_format_becomes_response_format() {
        let req = translate(json!({"model": "m", "input": "x",
            "text": {"format": {"type": "json_object"}}}));
        assert_eq!(req.extra["response_format"], json!({"type": "json_object"}));
    }

    #[test]
    fn text_format_sets_no_response_format() {
        let req = translate(json!({"model": "m", "input": "x",
            "text": {"format": {"type": "text"}}}));
        assert!(!req.extra.contains_key("response_format"));
    }

    #[test]
    fn reasoning_effort_maps_without_inventing_anthropic_thinking() {
        for effort in ["none", "minimal", "low", "medium", "high", "xhigh"] {
            let req = translate(json!({"model": "m", "input": "x",
                "max_output_tokens": 32000, "reasoning": {"effort": effort}}));
            assert_eq!(req.extra["reasoning_effort"], effort);
            assert!(!req.extra.contains_key("_anthropic_thinking"));
        }
    }

    #[test]
    fn store_is_accepted_and_ignored() {
        let req = translate(json!({"model": "m", "input": "x", "store": true}));
        assert!(!req.extra.contains_key("store"));
    }

    // ── stateless rejections ────────────────────────────────────────────────

    #[test]
    fn previous_response_id_is_rejected() {
        rejected(
            json!({"model": "m", "input": "x", "previous_response_id": "resp_1"}),
            "previous_response_id",
        );
    }

    #[test]
    fn conversation_is_rejected() {
        rejected(
            json!({"model": "m", "input": "x", "conversation": "conv_1"}),
            "conversation",
        );
        // An explicit null is the SDK's "not set" and is fine.
        translate(json!({"model": "m", "input": "x", "conversation": null}));
    }

    #[test]
    fn prompt_template_is_rejected() {
        rejected(
            json!({"model": "m", "input": "x", "prompt": {"id": "pmpt_1", "version": "2"}}),
            "prompt",
        );
        translate(json!({"model": "m", "input": "x", "prompt": null}));
    }

    #[test]
    fn background_true_is_rejected_false_is_fine() {
        rejected(
            json!({"model": "m", "input": "x", "background": true}),
            "background",
        );
        translate(json!({"model": "m", "input": "x", "background": false}));
    }

    #[test]
    fn private_keys_use_only_the_responses_prefix() {
        let req = translate(codex_fixture());
        let body = serde_json::to_value(&req).unwrap();
        // `ChatCompletionRequest` itself still carries them; the OpenAI adapter
        // strips `_`-prefixed keys (asserted in providers/openai.rs).
        assert!(body.get(RESPONSES_CUSTOM_TOOLS).is_some());
        assert!(req
            .extra
            .keys()
            .filter(|k| k.starts_with('_'))
            .all(|k| k.starts_with("_responses_")));
    }

    // ── response / stream types ─────────────────────────────────────────────

    fn sample_response() -> Response {
        Response {
            id: "resp_1".into(),
            object: "response".into(),
            created_at: 1_700_000_000,
            status: "completed".into(),
            model: "m".into(),
            output: vec![
                OutputItem::Reasoning {
                    id: "rs_1".into(),
                    summary: vec![SummaryText::new("thinking")],
                    content: None,
                    encrypted_content: Some(encode_anthropic_thinking_signature("s")),
                    status: None,
                },
                OutputItem::Message {
                    id: "msg_1".into(),
                    role: "assistant".into(),
                    status: "completed".into(),
                    content: vec![OutputContent::OutputText {
                        text: "hi".into(),
                        annotations: vec![],
                        logprobs: None,
                    }],
                },
                OutputItem::FunctionCall {
                    id: "fc_1".into(),
                    call_id: "call_1".into(),
                    name: "f".into(),
                    arguments: "{}".into(),
                    status: "completed".into(),
                },
            ],
            usage: Some(ResponseUsage {
                input_tokens: 10,
                input_tokens_details: InputTokensDetails {
                    cached_tokens: 4,
                    cache_write_tokens: 0,
                },
                output_tokens: 5,
                output_tokens_details: OutputTokensDetails {
                    reasoning_tokens: 2,
                },
                total_tokens: 15,
            }),
            error: None,
            incomplete_details: None,
            instructions: None,
            metadata: None,
            parallel_tool_calls: true,
            tool_choice: json!("auto"),
            tools: vec![],
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            previous_response_id: None,
            reasoning: None,
            text: None,
            store: None,
            service_tier: None,
            user: None,
        }
    }

    #[test]
    fn response_serializes_sdk_required_fields() {
        let v = serde_json::to_value(sample_response()).unwrap();
        for key in [
            "id",
            "object",
            "created_at",
            "model",
            "output",
            "parallel_tool_calls",
            "tool_choice",
            "tools",
            "error",
            "incomplete_details",
        ] {
            assert!(v.get(key).is_some(), "missing {key}");
        }
        assert_eq!(v["output"][1]["type"], "message");
        assert_eq!(v["output"][1]["content"][0]["type"], "output_text");
        assert_eq!(v["output"][2]["type"], "function_call");
        assert_eq!(v["usage"]["input_tokens_details"]["cached_tokens"], 4);
        assert_eq!(v["usage"]["output_tokens_details"]["reasoning_tokens"], 2);
    }

    #[test]
    fn response_round_trips() {
        let original = sample_response();
        let v = serde_json::to_value(&original).unwrap();
        let back: Response = serde_json::from_value(v).unwrap();
        assert_eq!(back.output, original.output);
        assert_eq!(back.usage, original.usage);
    }

    #[test]
    fn custom_tool_call_output_item_serializes() {
        let item = OutputItem::CustomToolCall {
            id: "ctc".into(),
            call_id: "call".into(),
            name: "apply_patch".into(),
            input: "*** Begin Patch".into(),
            status: Some("completed".into()),
        };
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(v["type"], "custom_tool_call");
        assert_eq!(v["input"], "*** Begin Patch");
    }

    #[test]
    fn stream_event_type_matches_serialized_tag() {
        let events = vec![
            ResponseStreamEvent::Created {
                response: Box::new(sample_response()),
                sequence_number: 0,
            },
            ResponseStreamEvent::OutputTextDelta {
                item_id: "msg_1".into(),
                output_index: 1,
                content_index: 0,
                delta: "h".into(),
                logprobs: vec![],
                sequence_number: 1,
            },
            ResponseStreamEvent::FunctionCallArgumentsDone {
                item_id: "fc_1".into(),
                output_index: 2,
                name: Some("f".into()),
                arguments: "{}".into(),
                sequence_number: 2,
            },
            ResponseStreamEvent::ReasoningSummaryTextDelta {
                item_id: "rs_1".into(),
                output_index: 0,
                summary_index: 0,
                delta: "t".into(),
                sequence_number: 3,
            },
            ResponseStreamEvent::Error {
                code: Some("server_error".into()),
                message: "boom".into(),
                param: None,
                sequence_number: 4,
            },
        ];
        for e in events {
            let v = serde_json::to_value(&e).unwrap();
            assert_eq!(v["type"], e.event_type());
            assert!(v.get("sequence_number").is_some());
            let back: ResponseStreamEvent = serde_json::from_value(v).unwrap();
            assert_eq!(back.event_type(), e.event_type());
        }
    }

    #[test]
    fn sdk_stream_event_fixture_deserializes() {
        // Shape of a `response.output_item.added` event from the OpenAI API.
        let e: ResponseStreamEvent = serde_json::from_value(json!({
            "type": "response.output_item.added", "output_index": 0, "sequence_number": 2,
            "item": {"type": "message", "id": "msg_1", "status": "in_progress",
                     "role": "assistant", "content": []}
        }))
        .unwrap();
        assert_eq!(e.event_type(), "response.output_item.added");
    }
}

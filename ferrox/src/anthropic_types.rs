use axum::response::sse::Event;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

use crate::error::ProxyError;
use crate::providers::ProviderStream;
use crate::types::{
    ChatCompletionRequest, ChatCompletionResponse, ChatMessage, ContentPart, FunctionCall,
    MessageContent, StopSequences, Tool, ToolCall, ToolFunction,
};

// ── Inbound request ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AnthropicMessagesRequest {
    pub model: String,
    pub messages: Vec<AnthropicMessage>,
    pub max_tokens: u32,
    pub system: Option<AnthropicSystemContent>,
    pub stream: Option<bool>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub stop_sequences: Option<Vec<String>>,
    pub tools: Option<Vec<AnthropicTool>>,
    pub tool_choice: Option<AnthropicToolChoice>,
    /// Extended thinking configuration — forwarded to Anthropic provider.
    pub thinking: Option<serde_json::Value>,
    /// Beta feature strings (body alternative to `anthropic-beta` header) — forwarded.
    pub betas: Option<Vec<String>>,
    /// `metadata.user_id` is forwarded as OpenAI `user`.
    pub metadata: Option<serde_json::Value>,
    /// Forwarded as `top_k` (accepted by many OpenAI-compatible backends).
    pub top_k: Option<u32>,
}

impl AnthropicMessagesRequest {
    pub fn is_streaming(&self) -> bool {
        self.stream.unwrap_or(false)
    }
}

/// System prompt — either a plain string or an array of typed content blocks.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum AnthropicSystemContent {
    Text(String),
    Blocks(Vec<AnthropicSystemBlock>),
}

#[derive(Debug, Deserialize)]
pub struct AnthropicSystemBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: AnthropicMessageContent,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum AnthropicMessageContent {
    Text(String),
    Blocks(Vec<AnthropicContentBlock>),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicContentBlock {
    Text {
        text: String,
    },
    /// Image blocks are accepted but forwarded as-is (provider decides support).
    Image {
        #[allow(dead_code)]
        source: serde_json::Value,
    },
    /// Assistant-turn tool invocation block.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// User-turn tool result block.
    ToolResult {
        tool_use_id: String,
        /// Content can be a plain string, an array of content blocks, or absent.
        content: Option<serde_json::Value>,
        #[serde(default)]
        #[allow(dead_code)]
        is_error: bool,
    },
    /// Catch-all for document, thinking, search_result, and future block types.
    #[serde(other)]
    Unknown,
}

// ── Tool definition in request ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AnthropicTool {
    pub name: String,
    pub description: Option<String>,
    /// JSON Schema object describing the tool's input.
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicToolChoice {
    /// Model decides whether and which tools to call.
    Auto,
    /// Model must call at least one tool.
    Any,
    /// Model must call the named tool.
    Tool { name: String },
    /// Model must not call any tools.
    None,
}

// ── Outbound response ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AnthropicMessagesResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub response_type: String,
    pub role: String,
    pub model: String,
    pub content: Vec<AnthropicResponseContent>,
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
    pub usage: AnthropicUsage,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicResponseContent {
    /// Extended-thinking block (emitted before text when a reasoning model
    /// returned `reasoning_content`).
    Thinking {
        thinking: String,
    },
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Debug, Serialize)]
pub struct AnthropicUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

// ── Models list response ─────────────────────────────────────────────────────

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AnthropicModelsResponse {
    pub data: Vec<AnthropicModelObject>,
    pub has_more: bool,
    pub first_id: Option<String>,
    pub last_id: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AnthropicModelObject {
    /// Always `"model"`.
    #[serde(rename = "type")]
    #[schema(rename = "type", example = "model")]
    pub object_type: String,
    /// The model alias configured in the gateway.
    #[schema(example = "claude-sonnet")]
    pub id: String,
    pub display_name: String,
    /// RFC 3339 timestamp; the gateway emits an empty string (aliases are not
    /// versioned).
    pub created_at: String,
}

// ── Translation: Anthropic request → internal ────────────────────────────────

pub fn to_chat_completion_request(req: AnthropicMessagesRequest) -> ChatCompletionRequest {
    let system = req.system.map(|s| match s {
        AnthropicSystemContent::Text(t) => t,
        AnthropicSystemContent::Blocks(blocks) => blocks
            .into_iter()
            .filter(|b| b.block_type == "text")
            .filter_map(|b| b.text)
            .collect::<Vec<_>>()
            // Separate system blocks are distinct lines; joining with "" would
            // glue e.g. "You are" + "helpful" into "You arehelpful".
            .join("\n"),
    });

    let messages = anthropic_messages_to_internal(req.messages);

    let tools = req.tools.map(|tools| {
        tools
            .into_iter()
            .map(|t| Tool {
                r#type: "function".to_string(),
                function: ToolFunction {
                    name: t.name,
                    description: t.description,
                    parameters: Some(t.input_schema),
                },
            })
            .collect()
    });

    let tool_choice = req.tool_choice.map(|tc| match tc {
        AnthropicToolChoice::Auto => serde_json::json!("auto"),
        AnthropicToolChoice::Any => serde_json::json!("required"),
        AnthropicToolChoice::Tool { name } => {
            serde_json::json!({"type": "function", "function": {"name": name}})
        }
        AnthropicToolChoice::None => serde_json::json!("none"),
    });

    // Carry Anthropic-specific body fields that have no OpenAI equivalent in the
    // `extra` map using private keys; the Anthropic provider adapter reads them back.
    let mut extra: HashMap<String, serde_json::Value> = HashMap::new();
    if let Some(thinking) = req.thinking {
        extra.insert("_anthropic_thinking".to_string(), thinking);
    }
    if let Some(betas) = req.betas {
        extra.insert(
            "_anthropic_betas".to_string(),
            serde_json::Value::Array(betas.into_iter().map(serde_json::Value::String).collect()),
        );
    }
    // Anthropic `metadata.user_id` → OpenAI `user`; `top_k` passes through as-is
    // (many OpenAI-compatible backends, incl. GLM/Kimi, accept it).
    if let Some(user_id) = req
        .metadata
        .as_ref()
        .and_then(|m| m.get("user_id"))
        .and_then(|u| u.as_str())
    {
        extra.insert(
            "user".to_string(),
            serde_json::Value::String(user_id.to_string()),
        );
    }
    if let Some(top_k) = req.top_k {
        extra.insert("top_k".to_string(), serde_json::Value::from(top_k));
    }

    ChatCompletionRequest {
        model: req.model,
        messages,
        stream: req.stream,
        temperature: req.temperature,
        max_tokens: Some(req.max_tokens),
        top_p: req.top_p,
        stop: req.stop_sequences.map(StopSequences::Multiple),
        tools,
        tool_choice,
        system,
        extra_headers: HashMap::new(),
        raw_anthropic_body: None,
        extra,
    }
}

/// Convert a list of Anthropic messages to internal `ChatMessage` format.
///
/// A single Anthropic user message that contains `tool_result` blocks may expand
/// into multiple internal messages: one per tool result (`role: "tool"`) plus an
/// optional preceding user text message.
fn anthropic_messages_to_internal(messages: Vec<AnthropicMessage>) -> Vec<ChatMessage> {
    let mut result = Vec::new();
    for msg in messages {
        match msg.content {
            AnthropicMessageContent::Text(t) => {
                result.push(ChatMessage {
                    role: msg.role,
                    content: Some(MessageContent::Text(t)),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                });
            }
            AnthropicMessageContent::Blocks(blocks) => {
                convert_blocks(msg.role, blocks, &mut result);
            }
        }
    }
    result
}

/// Expand one Anthropic message (block content) into ≥1 internal messages.
fn convert_blocks(role: String, blocks: Vec<AnthropicContentBlock>, out: &mut Vec<ChatMessage>) {
    let mut content_parts: Vec<ContentPart> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut tool_results: Vec<(String, String)> = Vec::new();
    // Image parts extracted from `tool_result` blocks. Held separately so they
    // ride the trailing `user` message (user turns only) rather than leaking
    // onto an assistant message — mirroring how a `tool_result`'s text is
    // dropped on the assistant path.
    let mut tool_result_image_parts: Vec<ContentPart> = Vec::new();

    for block in blocks {
        match block {
            AnthropicContentBlock::Text { text } => {
                content_parts.push(ContentPart::Text { text });
            }
            AnthropicContentBlock::Image { source } => {
                // Translate to an OpenAI image_url part instead of dropping it.
                if let Some(url) = anthropic_image_source_to_url(&source) {
                    content_parts.push(ContentPart::ImageUrl {
                        image_url: crate::types::ImageUrl { url, detail: None },
                    });
                }
            }
            AnthropicContentBlock::ToolUse { id, name, input } => {
                tool_calls.push(ToolCall {
                    id,
                    r#type: "function".to_string(),
                    function: FunctionCall {
                        name,
                        arguments: input.to_string(),
                    },
                });
            }
            AnthropicContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                // OpenAI `tool` messages are text-only, so an image returned
                // inside a `tool_result` cannot ride on the tool reply. Extract
                // any image blocks (borrowing before `content` is moved into the
                // text flattener); they're re-homed onto this turn's trailing
                // `user` message below, after the `tool` replies — preserving
                // the tool_call/reply adjacency from #104/#105.
                tool_result_image_parts.extend(tool_result_images(content.as_ref()));
                let mut text = tool_result_content_to_string(content);
                // The OpenAI `tool` role has no `is_error`; annotate so the model
                // can tell a failed tool call from a successful one.
                if is_error {
                    text = format!("[tool error] {text}");
                }
                tool_results.push((tool_use_id, text));
            }
            // Document/thinking/unknown blocks have no OpenAI equivalent. Warn so
            // a silently-dropped block isn't mistaken for successful handling.
            AnthropicContentBlock::Unknown => {
                tracing::warn!(
                    "dropping unsupported Anthropic content block (no OpenAI equivalent)"
                );
            }
        }
    }

    if role == "assistant" {
        // Assistant turn: combine text/image + tool_calls into one message.
        out.push(ChatMessage {
            role,
            content: parts_to_content(content_parts),
            name: None,
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            tool_call_id: None,
            reasoning_content: None,
        });
    } else {
        // User turn: emit the `tool` result messages FIRST, then any user
        // text/images.
        //
        // OpenAI-format backends require every `tool` message to immediately
        // follow the assistant message that issued the matching `tool_calls`.
        // Anthropic lets a single user turn mix `tool_result` blocks with plain
        // text (Claude Code does this — e.g. a skill result plus a "Base
        // directory for this skill: …" note). If we emitted that user text
        // before the tool results, it would wedge a `user` message between the
        // assistant `tool_calls` and its `tool` replies, and strict upstreams
        // (Kimi/Moonshot, OpenAI) reject the request with "tool_call_ids did
        // not have response messages". So tool replies come first; the user
        // text follows as its own message after them.
        for (tool_use_id, content) in tool_results {
            out.push(ChatMessage {
                role: "tool".to_string(),
                content: Some(MessageContent::Text(content)),
                name: None,
                tool_calls: None,
                tool_call_id: Some(tool_use_id),
                reasoning_content: None,
            });
        }
        // Re-home any tool_result images onto the trailing user message so they
        // land in a spec-valid position (image_url is invalid on a `tool`
        // message) after the tool replies.
        content_parts.extend(tool_result_image_parts);
        if let Some(content) = parts_to_content(content_parts) {
            out.push(ChatMessage {
                role: role.clone(),
                content: Some(content),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            });
        }
    }
}

/// Collapse collected content parts into a message content: a single `Text` when
/// everything is text (the common case), `Parts` when any image is present, or
/// `None` when empty.
fn parts_to_content(parts: Vec<ContentPart>) -> Option<MessageContent> {
    if parts.is_empty() {
        return None;
    }
    if parts.iter().all(|p| matches!(p, ContentPart::Text { .. })) {
        let text = parts
            .into_iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        return Some(MessageContent::Text(text));
    }
    Some(MessageContent::Parts(parts))
}

/// Convert an Anthropic image `source` object to an OpenAI `image_url` URL.
/// `base64` sources become a `data:` URL; `url` sources pass through.
fn anthropic_image_source_to_url(source: &serde_json::Value) -> Option<String> {
    match source.get("type").and_then(|t| t.as_str()) {
        Some("base64") => {
            let media = source
                .get("media_type")
                .and_then(|m| m.as_str())
                .unwrap_or("image/jpeg");
            let data = source.get("data").and_then(|d| d.as_str())?;
            Some(format!("data:{media};base64,{data}"))
        }
        Some("url") => source.get("url").and_then(|u| u.as_str()).map(String::from),
        _ => None,
    }
}

/// Extract plain text from a `tool_result` content value.
///
/// The value can be:
/// - absent (`None`)
/// - a plain string (`Value::String`)
/// - an array of content blocks (`Value::Array`)
fn tool_result_content_to_string(v: Option<serde_json::Value>) -> String {
    match v {
        None => String::new(),
        Some(serde_json::Value::String(s)) => s,
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|item| match item.get("type").and_then(|t| t.as_str()) {
                Some("text") => item
                    .get("text")
                    .and_then(|t| t.as_str())
                    .map(str::to_string),
                // `image` blocks are re-homed onto the trailing `user` message
                // by `tool_result_images()`, so they're not dropped here — stay
                // silent for them.
                Some("image") => None,
                other => {
                    // Other non-text blocks (e.g. `document`) have no OpenAI
                    // tool-message representation — warn instead of dropping
                    // silently.
                    tracing::warn!(
                        block = other.unwrap_or("unknown"),
                        "dropping non-text tool_result block (OpenAI tool messages are text-only)"
                    );
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(""),
        Some(other) => other.to_string(),
    }
}

/// Extract `image` blocks from a `tool_result` content value as OpenAI
/// `image_url` content parts.
///
/// OpenAI's `tool` role is text-only, so tool-result images can't ride on the
/// tool reply; the caller re-homes these onto the trailing `user` message (a
/// spec-valid position for `image_url`). Only array-form content can carry
/// blocks; string/absent content yields no images. Reuses
/// `anthropic_image_source_to_url()` so base64 (`data:` URL) and remote-URL
/// sources are handled identically to top-level image blocks.
fn tool_result_images(v: Option<&serde_json::Value>) -> Vec<ContentPart> {
    match v {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|item| match item.get("type").and_then(|t| t.as_str()) {
                Some("image") => item
                    .get("source")
                    .and_then(anthropic_image_source_to_url)
                    .map(|url| ContentPart::ImageUrl {
                        image_url: crate::types::ImageUrl { url, detail: None },
                    }),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

// ── Translation: internal response → Anthropic ───────────────────────────────

pub fn to_anthropic_response(resp: ChatCompletionResponse) -> AnthropicMessagesResponse {
    let choice = resp.choices.into_iter().next();

    let mut content: Vec<AnthropicResponseContent> = Vec::new();

    if let Some(ref c) = choice {
        // Thinking block first (reasoning models return reasoning_content).
        if let Some(thinking) = c.message.reasoning_content.as_ref() {
            if !thinking.is_empty() {
                content.push(AnthropicResponseContent::Thinking {
                    thinking: thinking.clone(),
                });
            }
        }

        // Text content
        if let Some(msg_content) = c.message.content.as_ref() {
            let text = match msg_content {
                MessageContent::Text(t) => t.clone(),
                MessageContent::Parts(parts) => parts
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            };
            if !text.is_empty() {
                content.push(AnthropicResponseContent::Text { text });
            }
        }

        // Tool use blocks
        if let Some(tool_calls) = c.message.tool_calls.as_ref() {
            for tc in tool_calls {
                let input = serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                content.push(AnthropicResponseContent::ToolUse {
                    id: tc.id.clone(),
                    name: tc.function.name.clone(),
                    input,
                });
            }
        }
    }

    // Anthropic permits an empty `content` array; do NOT synthesize an empty
    // text block, which some clients reject ("text content blocks must be non-empty").

    let stop_reason = choice
        .as_ref()
        .and_then(|c| c.finish_reason.as_deref())
        .map(finish_reason_to_anthropic)
        .map(str::to_string);

    let id = format!("msg_{}", resp.id.trim_start_matches("chatcmpl-"));

    AnthropicMessagesResponse {
        id,
        response_type: "message".to_string(),
        role: "assistant".to_string(),
        model: resp.model,
        content,
        stop_reason,
        stop_sequence: None,
        usage: AnthropicUsage {
            input_tokens: resp.usage.as_ref().map(|u| u.prompt_tokens).unwrap_or(0),
            output_tokens: resp
                .usage
                .as_ref()
                .map(|u| u.completion_tokens)
                .unwrap_or(0),
        },
    }
}

pub fn finish_reason_to_anthropic(reason: &str) -> &str {
    match reason {
        "stop" => "end_turn",
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        // `content_filter` is not a valid Anthropic stop_reason; `refusal` is the
        // closest. Any other/unknown value defaults to `end_turn` rather than
        // leaking a non-spec value that strict SDK enums would reject.
        "content_filter" => "refusal",
        _ => "end_turn",
    }
}

// ── Streaming translation: OpenAI chunks → Anthropic SSE events ──────────────

struct StreamState {
    inner: ProviderStream,
    model: String,
    msg_id: String,
    is_first: bool,
    pending: VecDeque<Result<Event, ProxyError>>,
    output_tokens: u32,
    /// Prompt tokens. OpenAI-format upstreams deliver this only in the final
    /// chunk's usage (via `stream_options.include_usage`), so we capture it
    /// whenever a chunk carries usage and emit it in `message_delta`.
    input_tokens: u32,
    stop_reason: String,
    stream_done: bool,
    /// Whether the text content_block (index 0) has been opened.
    /// We defer opening it until actual text arrives so tool-only responses
    /// never produce an empty `{"type":"text","text":""}` block.
    text_block_started: bool,
    /// Whether the text content_block (index 0) has been closed.
    text_block_closed: bool,
    /// Running count of content blocks emitted so far (used as the next index).
    next_block_index: u32,
    /// Open tool_use blocks, as `(openai_tool_index, anthropic_block_index)`.
    /// OpenAI-format streams fragment each tool call across many deltas keyed by
    /// `index`; we open one Anthropic block per distinct index, stream its
    /// argument fragments as `input_json_delta`, and close them all at the end.
    tool_blocks: Vec<(u32, u32)>,
    /// Block index assigned to the text content block (0 unless a thinking block
    /// precedes it). Text is no longer hard-coded to index 0 because a `thinking`
    /// block, when present, must come first.
    text_block_index: u32,
    /// Thinking (reasoning) block state. Reasoning models stream chain-of-thought
    /// via `reasoning_content`; it is surfaced as an Anthropic `thinking` block
    /// that precedes text/tool blocks.
    thinking_started: bool,
    thinking_closed: bool,
    thinking_index: u32,
}

/// Wraps a `ProviderStream` (OpenAI chunk format) and re-emits events in the
/// Anthropic SSE event protocol:
/// `message_start` → `content_block_start` → `ping` →
/// N× `content_block_delta` → `content_block_stop` →
/// `message_delta` → `message_stop`
pub fn openai_stream_to_anthropic_sse(
    model: String,
    msg_id: String,
    stream: ProviderStream,
) -> impl Stream<Item = Result<Event, ProxyError>> + Send {
    use futures::StreamExt as _;

    let state = StreamState {
        inner: stream,
        model,
        msg_id,
        is_first: true,
        pending: VecDeque::new(),
        output_tokens: 0,
        input_tokens: 0,
        stop_reason: "end_turn".to_string(),
        stream_done: false,
        text_block_started: false,
        text_block_closed: false,
        next_block_index: 0,
        tool_blocks: Vec::new(),
        text_block_index: 0,
        thinking_started: false,
        thinking_closed: false,
        thinking_index: 0,
    };

    futures::stream::unfold(state, |mut s| async move {
        loop {
            // Drain buffered events before polling the inner stream
            if let Some(ev) = s.pending.pop_front() {
                return Some((ev, s));
            }

            if s.stream_done {
                return None;
            }

            match s.inner.next().await {
                None => {
                    s.stream_done = true;
                    if s.is_first {
                        // Empty upstream — emit a minimal valid Anthropic sequence
                        s.is_first = false;
                        s.pending
                            .push_back(Ok(make_message_start_event(&s.msg_id, &s.model, 0)));
                        s.pending.push_back(Ok(make_ping_event()));
                    }
                    // Close any still-open thinking/text block.
                    if s.thinking_started && !s.thinking_closed {
                        s.thinking_closed = true;
                        s.pending
                            .push_back(Ok(make_content_block_stop_event(s.thinking_index)));
                    }
                    if s.text_block_started && !s.text_block_closed {
                        s.text_block_closed = true;
                        s.pending
                            .push_back(Ok(make_content_block_stop_event(s.text_block_index)));
                    }
                    // Close every open tool_use block (in the order opened).
                    for (_, block_index) in std::mem::take(&mut s.tool_blocks) {
                        s.pending
                            .push_back(Ok(make_content_block_stop_event(block_index)));
                    }
                    s.pending.push_back(Ok(make_message_delta_event(
                        &s.stop_reason,
                        s.input_tokens,
                        s.output_tokens,
                    )));
                    s.pending.push_back(Ok(make_message_stop_event()));
                    // Loop back to drain pending
                }
                Some(Err(e)) => {
                    // Emit a proper Anthropic `error` SSE event instead of yielding
                    // a raw Err (which axum turns into a bare connection close with
                    // no terminal event). Then end the stream.
                    s.stream_done = true;
                    return Some((Ok(make_error_event(&e)), s));
                }
                Some(Ok(chunk)) => {
                    // Update accumulated state
                    if let Some(usage) = &chunk.usage {
                        s.output_tokens = usage.completion_tokens;
                        if usage.prompt_tokens > 0 {
                            s.input_tokens = usage.prompt_tokens;
                        }
                    }
                    if let Some(choice) = chunk.choices.first() {
                        if let Some(reason) = &choice.finish_reason {
                            s.stop_reason = finish_reason_to_anthropic(reason).to_string();
                        }
                    }

                    let text = chunk
                        .choices
                        .first()
                        .and_then(|c| c.delta.content.clone())
                        .unwrap_or_default();

                    let tool_calls = chunk
                        .choices
                        .first()
                        .and_then(|c| c.delta.tool_calls.clone())
                        .unwrap_or_default();

                    let reasoning = chunk
                        .choices
                        .first()
                        .and_then(|c| c.delta.reasoning_content.clone())
                        .unwrap_or_default();

                    if s.is_first {
                        s.is_first = false;
                        let input_tokens =
                            chunk.usage.as_ref().map(|u| u.prompt_tokens).unwrap_or(0);
                        s.pending.push_back(Ok(make_message_start_event(
                            &s.msg_id,
                            &s.model,
                            input_tokens,
                        )));
                        s.pending.push_back(Ok(make_ping_event()));
                        // Do NOT open the text block here; defer until text actually arrives
                        // so tool-only responses never produce an empty text block.
                    }

                    // Thinking (reasoning) block — opened first, before text/tools.
                    // Ignore reasoning that arrives after the block was closed
                    // (e.g. reasoning interleaved after text) to avoid emitting a
                    // delta into a stopped block.
                    if !reasoning.is_empty() && !s.thinking_closed {
                        if !s.thinking_started {
                            s.thinking_started = true;
                            s.thinking_index = s.next_block_index;
                            s.next_block_index += 1;
                            s.pending
                                .push_back(Ok(make_thinking_block_start_event(s.thinking_index)));
                        }
                        s.pending
                            .push_back(Ok(make_thinking_delta_event(s.thinking_index, &reasoning)));
                    }

                    if !text.is_empty() {
                        // Thinking always closes before the text block opens.
                        if s.thinking_started && !s.thinking_closed {
                            s.thinking_closed = true;
                            s.pending
                                .push_back(Ok(make_content_block_stop_event(s.thinking_index)));
                        }
                        // Open the text block on first actual text content.
                        if !s.text_block_started {
                            s.text_block_started = true;
                            s.text_block_index = s.next_block_index;
                            s.next_block_index += 1;
                            s.pending
                                .push_back(Ok(make_content_block_start_event(s.text_block_index)));
                        }
                        s.pending.push_back(Ok(make_content_block_delta_event(
                            s.text_block_index,
                            &text,
                        )));
                    }

                    // Accumulate fragmented tool-call deltas by `index`: open one
                    // Anthropic tool_use block the first time an index is seen
                    // (id/name arrive in that first fragment) and stream later
                    // fragments' arguments into it. Blocks are closed at stream end.
                    for tc in &tool_calls {
                        // Some providers send -1 for a single tool call; clamp
                        // negatives to 0 (matching the official SDKs).
                        let idx = tc.index.max(0) as u32;
                        let name = tc.function.as_ref().and_then(|f| f.name.as_deref());
                        let args = tc.function.as_ref().and_then(|f| f.arguments.as_deref());

                        // Open the block on first sighting of this tool index.
                        if !s.tool_blocks.iter().any(|(oi, _)| *oi == idx) {
                            // Close any open thinking/text block before the first
                            // tool_use block.
                            if s.thinking_started && !s.thinking_closed {
                                s.thinking_closed = true;
                                s.pending
                                    .push_back(Ok(make_content_block_stop_event(s.thinking_index)));
                            }
                            if s.text_block_started && !s.text_block_closed {
                                s.text_block_closed = true;
                                s.pending.push_back(Ok(make_content_block_stop_event(
                                    s.text_block_index,
                                )));
                            }
                            let block_index = s.next_block_index;
                            s.next_block_index += 1;
                            s.tool_blocks.push((idx, block_index));
                            s.pending.push_back(Ok(make_tool_use_block_start_event(
                                block_index,
                                tc.id.as_deref().unwrap_or(""),
                                name.unwrap_or(""),
                            )));
                        }

                        // Stream this fragment's argument piece into the block.
                        if let Some(args) = args.filter(|a| !a.is_empty()) {
                            let block_index = s
                                .tool_blocks
                                .iter()
                                .find(|(oi, _)| *oi == idx)
                                .map(|(_, bi)| *bi)
                                .unwrap_or(0);
                            s.pending
                                .push_back(Ok(make_input_json_delta_event(block_index, args)));
                        }
                    }
                    // Loop back to drain pending or fetch the next chunk
                }
            }
        }
    })
}

// ── SSE event constructors ────────────────────────────────────────────────────

fn make_message_start_event(msg_id: &str, model: &str, input_tokens: u32) -> Event {
    let data = serde_json::json!({
        "type": "message_start",
        "message": {
            "id": msg_id,
            "type": "message",
            "role": "assistant",
            "content": [],
            "model": model,
            "stop_reason": null,
            "stop_sequence": null,
            "usage": {
                "input_tokens": input_tokens,
                "output_tokens": 1
            }
        }
    });
    Event::default()
        .event("message_start")
        .data(data.to_string())
}

fn make_content_block_start_event(index: u32) -> Event {
    let data = serde_json::json!({
        "type": "content_block_start",
        "index": index,
        "content_block": {"type": "text", "text": ""}
    });
    Event::default()
        .event("content_block_start")
        .data(data.to_string())
}

fn make_tool_use_block_start_event(index: u32, id: &str, name: &str) -> Event {
    let data = serde_json::json!({
        "type": "content_block_start",
        "index": index,
        "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}
    });
    Event::default()
        .event("content_block_start")
        .data(data.to_string())
}

fn make_error_event(e: &ProxyError) -> Event {
    let (error_type, message) = match e {
        ProxyError::Unauthorized(m) => ("authentication_error", m.clone()),
        ProxyError::Forbidden(m) => ("permission_error", m.clone()),
        ProxyError::ModelNotFound(m) => ("not_found_error", m.clone()),
        ProxyError::RateLimited(m) | ProxyError::BudgetExceeded(m) => {
            ("rate_limit_error", m.clone())
        }
        ProxyError::CircuitOpen(m) => ("overloaded_error", m.clone()),
        other => ("api_error", other.to_string()),
    };
    let data = serde_json::json!({
        "type": "error",
        "error": {"type": error_type, "message": message}
    });
    Event::default().event("error").data(data.to_string())
}

fn make_thinking_block_start_event(index: u32) -> Event {
    let data = serde_json::json!({
        "type": "content_block_start",
        "index": index,
        "content_block": {"type": "thinking", "thinking": ""}
    });
    Event::default()
        .event("content_block_start")
        .data(data.to_string())
}

fn make_thinking_delta_event(index: u32, thinking: &str) -> Event {
    let data = serde_json::json!({
        "type": "content_block_delta",
        "index": index,
        "delta": {"type": "thinking_delta", "thinking": thinking}
    });
    Event::default()
        .event("content_block_delta")
        .data(data.to_string())
}

fn make_input_json_delta_event(index: u32, partial_json: &str) -> Event {
    let data = serde_json::json!({
        "type": "content_block_delta",
        "index": index,
        "delta": {"type": "input_json_delta", "partial_json": partial_json}
    });
    Event::default()
        .event("content_block_delta")
        .data(data.to_string())
}

fn make_ping_event() -> Event {
    Event::default()
        .event("ping")
        .data(serde_json::json!({"type": "ping"}).to_string())
}

fn make_content_block_delta_event(index: u32, text: &str) -> Event {
    let data = serde_json::json!({
        "type": "content_block_delta",
        "index": index,
        "delta": {"type": "text_delta", "text": text}
    });
    Event::default()
        .event("content_block_delta")
        .data(data.to_string())
}

fn make_content_block_stop_event(index: u32) -> Event {
    Event::default()
        .event("content_block_stop")
        .data(serde_json::json!({"type": "content_block_stop", "index": index}).to_string())
}

fn make_message_delta_event(stop_reason: &str, input_tokens: u32, output_tokens: u32) -> Event {
    // Anthropic clients read the authoritative `input_tokens` from `message_delta`
    // (message_start only carries a placeholder), so surface it here.
    let data = serde_json::json!({
        "type": "message_delta",
        "delta": {
            "stop_reason": stop_reason,
            "stop_sequence": null
        },
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens}
    });
    Event::default()
        .event("message_delta")
        .data(data.to_string())
}

fn make_message_stop_event() -> Event {
    Event::default()
        .event("message_stop")
        .data(serde_json::json!({"type": "message_stop"}).to_string())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Choice, Usage};

    fn text_message(role: &str, content: &str) -> AnthropicMessage {
        AnthropicMessage {
            role: role.to_string(),
            content: AnthropicMessageContent::Text(content.to_string()),
        }
    }

    fn minimal_request(model: &str) -> AnthropicMessagesRequest {
        AnthropicMessagesRequest {
            model: model.to_string(),
            messages: vec![text_message("user", "Hello")],
            max_tokens: 1024,
            system: None,
            stream: None,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            betas: None,
            metadata: None,
            top_k: None,
        }
    }

    // ── is_streaming ─────────────────────────────────────────────────────────

    #[test]
    fn is_streaming_defaults_false() {
        assert!(!minimal_request("claude-sonnet").is_streaming());
    }

    #[test]
    fn is_streaming_true_when_set() {
        let mut req = minimal_request("claude-sonnet");
        req.stream = Some(true);
        assert!(req.is_streaming());
    }

    // ── to_chat_completion_request ────────────────────────────────────────────

    #[test]
    fn converts_model_and_max_tokens() {
        let req = minimal_request("gpt-4o");
        let out = to_chat_completion_request(req);
        assert_eq!(out.model, "gpt-4o");
        assert_eq!(out.max_tokens, Some(1024));
    }

    #[test]
    fn converts_string_system_to_system_field() {
        let mut req = minimal_request("gpt-4o");
        req.system = Some(AnthropicSystemContent::Text("Be concise.".to_string()));
        let out = to_chat_completion_request(req);
        assert_eq!(out.system.as_deref(), Some("Be concise."));
    }

    #[test]
    fn converts_block_system_to_system_field() {
        let mut req = minimal_request("gpt-4o");
        req.system = Some(AnthropicSystemContent::Blocks(vec![AnthropicSystemBlock {
            block_type: "text".to_string(),
            text: Some("Act as a robot.".to_string()),
        }]));
        let out = to_chat_completion_request(req);
        assert_eq!(out.system.as_deref(), Some("Act as a robot."));
    }

    #[test]
    fn ignores_non_text_system_blocks() {
        let mut req = minimal_request("gpt-4o");
        req.system = Some(AnthropicSystemContent::Blocks(vec![AnthropicSystemBlock {
            block_type: "unknown".to_string(),
            text: Some("ignored".to_string()),
        }]));
        let out = to_chat_completion_request(req);
        assert_eq!(out.system.as_deref(), Some(""));
    }

    #[test]
    fn converts_text_content_messages() {
        let req = minimal_request("claude-sonnet");
        let out = to_chat_completion_request(req);
        assert_eq!(out.messages.len(), 1);
        assert_eq!(out.messages[0].role, "user");
        assert!(matches!(&out.messages[0].content, Some(MessageContent::Text(t)) if t == "Hello"));
    }

    #[test]
    fn converts_block_content_messages_single_text_to_plain_string() {
        let req = AnthropicMessagesRequest {
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicMessageContent::Blocks(vec![AnthropicContentBlock::Text {
                    text: "Hi there".to_string(),
                }]),
            }],
            ..minimal_request("claude-sonnet")
        };
        let out = to_chat_completion_request(req);
        assert!(
            matches!(&out.messages[0].content, Some(MessageContent::Text(t)) if t == "Hi there")
        );
    }

    #[test]
    fn converts_stop_sequences() {
        let mut req = minimal_request("gpt-4o");
        req.stop_sequences = Some(vec!["END".to_string(), "STOP".to_string()]);
        let out = to_chat_completion_request(req);
        assert!(matches!(out.stop, Some(StopSequences::Multiple(v)) if v == vec!["END", "STOP"]));
    }

    #[test]
    fn passes_through_temperature_top_p() {
        let mut req = minimal_request("gpt-4o");
        req.temperature = Some(0.8);
        req.top_p = Some(0.9);
        let out = to_chat_completion_request(req);
        assert_eq!(out.temperature, Some(0.8));
        assert_eq!(out.top_p, Some(0.9));
    }

    // ── tool conversion ───────────────────────────────────────────────────────

    #[test]
    fn converts_tools_to_openai_format() {
        let mut req = minimal_request("claude-sonnet");
        req.tools = Some(vec![AnthropicTool {
            name: "search".to_string(),
            description: Some("Search the web".to_string()),
            input_schema: serde_json::json!({"type": "object", "properties": {"q": {"type": "string"}}}),
        }]);
        let out = to_chat_completion_request(req);
        let tools = out.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].r#type, "function");
        assert_eq!(tools[0].function.name, "search");
        assert_eq!(
            tools[0].function.description.as_deref(),
            Some("Search the web")
        );
    }

    #[test]
    fn no_tools_yields_none() {
        let req = minimal_request("claude-sonnet");
        let out = to_chat_completion_request(req);
        assert!(out.tools.is_none());
    }

    #[test]
    fn tool_choice_auto_maps_to_auto() {
        let mut req = minimal_request("claude-sonnet");
        req.tool_choice = Some(AnthropicToolChoice::Auto);
        let out = to_chat_completion_request(req);
        assert_eq!(out.tool_choice, Some(serde_json::json!("auto")));
    }

    #[test]
    fn tool_choice_any_maps_to_required() {
        let mut req = minimal_request("claude-sonnet");
        req.tool_choice = Some(AnthropicToolChoice::Any);
        let out = to_chat_completion_request(req);
        assert_eq!(out.tool_choice, Some(serde_json::json!("required")));
    }

    #[test]
    fn tool_choice_tool_maps_to_function_object() {
        let mut req = minimal_request("claude-sonnet");
        req.tool_choice = Some(AnthropicToolChoice::Tool {
            name: "search".to_string(),
        });
        let out = to_chat_completion_request(req);
        assert_eq!(
            out.tool_choice,
            Some(serde_json::json!({"type": "function", "function": {"name": "search"}}))
        );
    }

    #[test]
    fn tool_choice_none_maps_to_none_string() {
        let mut req = minimal_request("claude-sonnet");
        req.tool_choice = Some(AnthropicToolChoice::None);
        let out = to_chat_completion_request(req);
        assert_eq!(out.tool_choice, Some(serde_json::json!("none")));
    }

    // ── tool_use block in assistant turn ─────────────────────────────────────

    #[test]
    fn assistant_tool_use_block_becomes_tool_calls() {
        let req = AnthropicMessagesRequest {
            messages: vec![AnthropicMessage {
                role: "assistant".to_string(),
                content: AnthropicMessageContent::Blocks(vec![
                    AnthropicContentBlock::Text {
                        text: "Let me search.".to_string(),
                    },
                    AnthropicContentBlock::ToolUse {
                        id: "toolu_abc".to_string(),
                        name: "search".to_string(),
                        input: serde_json::json!({"q": "weather"}),
                    },
                ]),
            }],
            ..minimal_request("claude-sonnet")
        };
        let out = to_chat_completion_request(req);
        assert_eq!(out.messages.len(), 1);
        let msg = &out.messages[0];
        assert_eq!(msg.role, "assistant");
        assert!(matches!(&msg.content, Some(MessageContent::Text(t)) if t == "Let me search."));
        let calls = msg.tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "toolu_abc");
        assert_eq!(calls[0].function.name, "search");
    }

    // ── tool_result block in user turn ────────────────────────────────────────

    #[test]
    fn user_tool_result_block_becomes_tool_message() {
        let req = AnthropicMessagesRequest {
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicMessageContent::Blocks(vec![AnthropicContentBlock::ToolResult {
                    tool_use_id: "toolu_abc".to_string(),
                    content: Some(serde_json::json!("72°F and sunny")),
                    is_error: false,
                }]),
            }],
            ..minimal_request("claude-sonnet")
        };
        let out = to_chat_completion_request(req);
        assert_eq!(out.messages.len(), 1);
        let msg = &out.messages[0];
        assert_eq!(msg.role, "tool");
        assert_eq!(msg.tool_call_id.as_deref(), Some("toolu_abc"));
        assert!(matches!(&msg.content, Some(MessageContent::Text(t)) if t == "72°F and sunny"));
    }

    #[test]
    fn mixed_text_and_tool_result_expands_to_two_messages() {
        let req = AnthropicMessagesRequest {
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicMessageContent::Blocks(vec![
                    AnthropicContentBlock::Text {
                        text: "Here is the result:".to_string(),
                    },
                    AnthropicContentBlock::ToolResult {
                        tool_use_id: "toolu_x".to_string(),
                        content: Some(serde_json::json!("done")),
                        is_error: false,
                    },
                ]),
            }],
            ..minimal_request("claude-sonnet")
        };
        let out = to_chat_completion_request(req);
        assert_eq!(out.messages.len(), 2);
        // The `tool` reply must come FIRST so it can immediately follow the
        // preceding assistant `tool_calls`; the user text follows it.
        assert_eq!(out.messages[0].role, "tool");
        assert_eq!(out.messages[1].role, "user");
    }

    /// Regression for the Kimi/OpenAI "tool_call_ids did not have response
    /// messages" 400: an assistant `tool_use` followed by a user turn that mixes
    /// a `tool_result` with plain text (Claude Code's skill-result pattern) must
    /// NOT wedge a `user` message between the assistant `tool_calls` and its
    /// `tool` reply. The `tool` message must sit immediately after the assistant.
    #[test]
    fn tool_reply_immediately_follows_assistant_tool_call_despite_user_text() {
        let req = AnthropicMessagesRequest {
            messages: vec![
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: AnthropicMessageContent::Blocks(vec![
                        AnthropicContentBlock::ToolUse {
                            id: "tool_0NRZ".to_string(),
                            name: "Skill".to_string(),
                            input: serde_json::json!({"command": "github-issue-refining"}),
                        },
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    // Same-turn mix: the tool result AND a trailing user note,
                    // exactly as Claude Code sends after invoking a skill.
                    content: AnthropicMessageContent::Blocks(vec![
                        AnthropicContentBlock::ToolResult {
                            tool_use_id: "tool_0NRZ".to_string(),
                            content: Some(serde_json::json!("Launching skill: …")),
                            is_error: false,
                        },
                        AnthropicContentBlock::Text {
                            text: "Base directory for this skill: /home/x".to_string(),
                        },
                    ]),
                },
            ],
            ..minimal_request("k3")
        };
        let out = to_chat_completion_request(req);
        let roles: Vec<&str> = out.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["assistant", "tool", "user"]);

        // Every assistant tool_call id must have a `tool` reply immediately after
        // the assistant message — the invariant strict OpenAI backends enforce.
        assert!(out.messages[0].tool_calls.is_some());
        assert_eq!(out.messages[1].role, "tool");
        assert_eq!(out.messages[1].tool_call_id.as_deref(), Some("tool_0NRZ"));

        // General invariant: no `user`/`assistant` message may separate an
        // assistant `tool_calls` from the `tool` reply answering one of its ids.
        for (i, m) in out.messages.iter().enumerate() {
            if let Some(tcs) = &m.tool_calls {
                let want: Vec<&str> = tcs.iter().map(|tc| tc.id.as_str()).collect();
                let replies: Vec<&str> = out.messages[i + 1..]
                    .iter()
                    .take_while(|m| m.role == "tool")
                    .filter_map(|m| m.tool_call_id.as_deref())
                    .collect();
                for id in want {
                    assert!(
                        replies.contains(&id),
                        "tool_call id {id:?} not answered by a `tool` message \
                         immediately following its assistant turn (roles: {roles:?})"
                    );
                }
            }
        }
    }

    #[test]
    fn tool_result_with_block_array_content_extracts_text() {
        let req = AnthropicMessagesRequest {
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicMessageContent::Blocks(vec![AnthropicContentBlock::ToolResult {
                    tool_use_id: "toolu_y".to_string(),
                    content: Some(serde_json::json!([{"type": "text", "text": "block result"}])),
                    is_error: false,
                }]),
            }],
            ..minimal_request("claude-sonnet")
        };
        let out = to_chat_completion_request(req);
        assert_eq!(out.messages.len(), 1);
        assert!(
            matches!(&out.messages[0].content, Some(MessageContent::Text(t)) if t == "block result")
        );
    }

    #[test]
    fn unknown_content_blocks_are_silently_dropped() {
        // Simulate a "document" block which maps to Unknown
        let block_json =
            r#"{"type": "document", "source": {"type": "url", "url": "https://example.com"}}"#;
        let block: AnthropicContentBlock = serde_json::from_str(block_json).unwrap();
        assert!(matches!(block, AnthropicContentBlock::Unknown));
    }

    #[test]
    fn image_block_is_preserved_as_image_url_part() {
        // Regression: image blocks were dropped, so providers hallucinated.
        let json = r#"{"model":"m","max_tokens":10,"messages":[{"role":"user","content":[
            {"type":"text","text":"what is this?"},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}}
        ]}]}"#;
        let req: AnthropicMessagesRequest = serde_json::from_str(json).unwrap();
        let internal = to_chat_completion_request(req);
        match internal.messages[0].content.as_ref().unwrap() {
            MessageContent::Parts(parts) => {
                assert!(
                    parts
                        .iter()
                        .any(|p| matches!(p, ContentPart::ImageUrl { image_url }
                        if image_url.url == "data:image/png;base64,AAAA")),
                    "image preserved as a data URL"
                );
                assert!(parts.iter().any(|p| matches!(p, ContentPart::Text { .. })));
            }
            other => panic!("expected multimodal Parts, got {other:?}"),
        }
    }

    #[test]
    fn url_image_source_passes_through() {
        let src = serde_json::json!({"type":"url","url":"https://x/i.png"});
        assert_eq!(
            anthropic_image_source_to_url(&src).as_deref(),
            Some("https://x/i.png")
        );
    }

    #[test]
    fn usage_details_round_trip() {
        // Provider usage-detail fields survive the OpenAI-format round-trip.
        let json = r#"{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,"prompt_tokens_details":{"cached_tokens":8}}"#;
        let u: crate::types::Usage = serde_json::from_str(json).unwrap();
        assert_eq!(u.prompt_tokens, 10);
        let out = serde_json::to_value(&u).unwrap();
        assert_eq!(out["prompt_tokens_details"]["cached_tokens"], 8);
    }

    #[test]
    fn metadata_user_and_top_k_pass_through() {
        let json = r#"{"model":"m","max_tokens":10,"metadata":{"user_id":"u123"},"top_k":40,"messages":[{"role":"user","content":"hi"}]}"#;
        let req: AnthropicMessagesRequest = serde_json::from_str(json).unwrap();
        let internal = to_chat_completion_request(req);
        assert_eq!(
            internal.extra.get("user").and_then(|v| v.as_str()),
            Some("u123")
        );
        assert_eq!(
            internal.extra.get("top_k").and_then(|v| v.as_u64()),
            Some(40)
        );
    }

    #[test]
    fn response_extra_fields_round_trip() {
        // service_tier + choice logprobs survive the OpenAI-format round-trip.
        let json = r#"{"id":"c","object":"chat.completion","created":0,"model":"m","service_tier":"default","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop","logprobs":{"content":[]}}],"usage":null,"system_fingerprint":null}"#;
        let resp: ChatCompletionResponse = serde_json::from_str(json).unwrap();
        let out = serde_json::to_value(&resp).unwrap();
        assert_eq!(out["service_tier"], "default");
        assert!(out["choices"][0]["logprobs"].is_object());
    }

    #[test]
    fn tool_result_mixed_text_image_keeps_text_rehomes_image() {
        // A tool_result with text + image: text stays on the (text-only) `tool`
        // message; the image is re-homed as an `image_url` part on the trailing
        // `user` message (#106).
        let json = r#"{"model":"m","max_tokens":10,"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"see this:"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}}]}]}]}"#;
        let req: AnthropicMessagesRequest = serde_json::from_str(json).unwrap();
        let internal = to_chat_completion_request(req);

        // tool message: text only, no image part.
        let tool_msg = internal.messages.iter().find(|m| m.role == "tool").unwrap();
        match tool_msg.content.as_ref().unwrap() {
            MessageContent::Text(t) => assert_eq!(t, "see this:"),
            other => panic!("expected text, got {other:?}"),
        }

        // user message: carries the image as an image_url part, after the tool
        // reply.
        let roles: Vec<&str> = internal.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["tool", "user"]);
        let user_msg = internal.messages.iter().find(|m| m.role == "user").unwrap();
        match user_msg.content.as_ref().unwrap() {
            MessageContent::Parts(parts) => {
                let imgs: Vec<&crate::types::ImageUrl> = parts
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::ImageUrl { image_url } => Some(image_url),
                        _ => None,
                    })
                    .collect();
                assert_eq!(imgs.len(), 1);
                assert_eq!(imgs[0].url, "data:image/png;base64,AAAA");
            }
            other => panic!("expected image parts on user message, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_image_only_rehomes_to_user_message() {
        // An image-only tool_result: the `tool` message has empty text, and the
        // image lands on the trailing `user` message (#106).
        let json = r#"{"model":"m","max_tokens":10,"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"image","source":{"type":"url","url":"https://example.com/a.png"}}]}]}]}"#;
        let req: AnthropicMessagesRequest = serde_json::from_str(json).unwrap();
        let internal = to_chat_completion_request(req);
        let roles: Vec<&str> = internal.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["tool", "user"]);

        // tool reply text-only (empty here) — never an image part.
        let tool_msg = &internal.messages[0];
        assert_eq!(tool_msg.tool_call_id.as_deref(), Some("t1"));
        assert!(matches!(
            tool_msg.content.as_ref().unwrap(),
            MessageContent::Text(t) if t.is_empty()
        ));

        let user_msg = &internal.messages[1];
        match user_msg.content.as_ref().unwrap() {
            MessageContent::Parts(parts) => {
                assert_eq!(parts.len(), 1);
                assert!(matches!(
                    &parts[0],
                    ContentPart::ImageUrl { image_url } if image_url.url == "https://example.com/a.png"
                ));
            }
            other => panic!("expected image parts, got {other:?}"),
        }
    }

    /// The #104/#105 invariant must hold when a tool_result carries an image:
    /// assistant `tool_calls` → `tool` reply → `user`(image), with no user
    /// message wedged before the tool reply.
    #[test]
    fn tool_result_image_preserves_tool_reply_adjacency() {
        let req = AnthropicMessagesRequest {
            messages: vec![
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: AnthropicMessageContent::Blocks(vec![
                        AnthropicContentBlock::ToolUse {
                            id: "tool_shot".to_string(),
                            name: "Screenshot".to_string(),
                            input: serde_json::json!({}),
                        },
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: AnthropicMessageContent::Blocks(vec![
                        AnthropicContentBlock::ToolResult {
                            tool_use_id: "tool_shot".to_string(),
                            content: Some(serde_json::json!([
                                {"type": "text", "text": "captured"},
                                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "ZZ"}}
                            ])),
                            is_error: false,
                        },
                    ]),
                },
            ],
            ..minimal_request("k3")
        };
        let out = to_chat_completion_request(req);
        let roles: Vec<&str> = out.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["assistant", "tool", "user"]);
        assert!(out.messages[0].tool_calls.is_some());
        assert_eq!(out.messages[1].tool_call_id.as_deref(), Some("tool_shot"));
    }

    #[test]
    fn assistant_turn_tool_result_image_does_not_leak_onto_assistant_message() {
        // tool_result is a user-turn construct; on the (malformed) assistant
        // path its text is already dropped, so its image must not attach to the
        // assistant message either — it stays text/no-image, symmetric with the
        // text handling.
        let req = AnthropicMessagesRequest {
            messages: vec![AnthropicMessage {
                role: "assistant".to_string(),
                content: AnthropicMessageContent::Blocks(vec![
                    AnthropicContentBlock::Text {
                        text: "hi".to_string(),
                    },
                    AnthropicContentBlock::ToolResult {
                        tool_use_id: "t1".to_string(),
                        content: Some(serde_json::json!([
                            {"type": "image", "source": {"type": "url", "url": "https://example.com/x.png"}}
                        ])),
                        is_error: false,
                    },
                ]),
            }],
            ..minimal_request("k3")
        };
        let out = to_chat_completion_request(req);
        assert_eq!(out.messages.len(), 1);
        let msg = &out.messages[0];
        assert_eq!(msg.role, "assistant");
        // No image_url part leaked onto the assistant message.
        if let Some(MessageContent::Parts(parts)) = msg.content.as_ref() {
            assert!(
                !parts
                    .iter()
                    .any(|p| matches!(p, ContentPart::ImageUrl { .. })),
                "tool_result image leaked onto assistant message"
            );
        }
    }

    #[test]
    fn tool_result_is_error_is_annotated() {
        let json = r#"{"model":"m","max_tokens":10,"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"boom","is_error":true}]}]}"#;
        let req: AnthropicMessagesRequest = serde_json::from_str(json).unwrap();
        let internal = to_chat_completion_request(req);
        let tool_msg = internal.messages.iter().find(|m| m.role == "tool").unwrap();
        match tool_msg.content.as_ref().unwrap() {
            MessageContent::Text(t) => assert!(t.starts_with("[tool error] "), "got: {t}"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    // ── to_anthropic_response ─────────────────────────────────────────────────

    fn make_openai_response(content: &str, finish_reason: &str) -> ChatCompletionResponse {
        ChatCompletionResponse {
            id: "chatcmpl-abc123".to_string(),
            object: "chat.completion".to_string(),
            created: 1_735_000_000,
            model: "gpt-4o".to_string(),
            choices: vec![Choice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: Some(MessageContent::Text(content.to_string())),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                },
                finish_reason: Some(finish_reason.to_string()),
                extra: Default::default(),
            }],
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                extra: Default::default(),
            }),
            system_fingerprint: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn response_type_and_role_are_set() {
        let r = to_anthropic_response(make_openai_response("Hello", "stop"));
        assert_eq!(r.response_type, "message");
        assert_eq!(r.role, "assistant");
    }

    #[test]
    fn response_content_extracted_correctly() {
        let r = to_anthropic_response(make_openai_response("Hi!", "stop"));
        assert_eq!(r.content.len(), 1);
        assert!(matches!(&r.content[0], AnthropicResponseContent::Text { text } if text == "Hi!"));
    }

    #[test]
    fn finish_reason_stop_maps_to_end_turn() {
        let r = to_anthropic_response(make_openai_response("x", "stop"));
        assert_eq!(r.stop_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn finish_reason_length_maps_to_max_tokens() {
        let r = to_anthropic_response(make_openai_response("x", "length"));
        assert_eq!(r.stop_reason.as_deref(), Some("max_tokens"));
    }

    #[test]
    fn finish_reason_tool_calls_maps_to_tool_use() {
        let r = to_anthropic_response(make_openai_response("x", "tool_calls"));
        assert_eq!(r.stop_reason.as_deref(), Some("tool_use"));
    }

    #[test]
    fn usage_is_mapped_correctly() {
        let r = to_anthropic_response(make_openai_response("x", "stop"));
        assert_eq!(r.usage.input_tokens, 10);
        assert_eq!(r.usage.output_tokens, 5);
    }

    #[test]
    fn id_has_msg_prefix() {
        let r = to_anthropic_response(make_openai_response("x", "stop"));
        assert!(r.id.starts_with("msg_"));
    }

    #[test]
    fn tool_calls_in_response_become_tool_use_blocks() {
        let resp = ChatCompletionResponse {
            id: "chatcmpl-xyz".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "gpt-4o".to_string(),
            choices: vec![Choice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: None,
                    name: None,
                    tool_calls: Some(vec![crate::types::ToolCall {
                        id: "call_abc".to_string(),
                        r#type: "function".to_string(),
                        function: crate::types::FunctionCall {
                            name: "search".to_string(),
                            arguments: r#"{"q":"weather"}"#.to_string(),
                        },
                    }]),
                    tool_call_id: None,
                    reasoning_content: None,
                },
                finish_reason: Some("tool_calls".to_string()),
                extra: Default::default(),
            }],
            usage: None,
            system_fingerprint: None,
            extra: Default::default(),
        };
        let r = to_anthropic_response(resp);
        assert_eq!(r.content.len(), 1);
        assert!(matches!(
            &r.content[0],
            AnthropicResponseContent::ToolUse { id, name, .. }
            if id == "call_abc" && name == "search"
        ));
    }

    // ── finish_reason_to_anthropic ────────────────────────────────────────────

    #[test]
    fn finish_reason_unknowns_do_not_leak() {
        // content_filter maps to the valid Anthropic `refusal`; anything else
        // defaults to `end_turn` rather than leaking a non-spec value.
        assert_eq!(finish_reason_to_anthropic("content_filter"), "refusal");
        assert_eq!(finish_reason_to_anthropic("mystery"), "end_turn");
    }

    // ── tool_result_content_to_string ─────────────────────────────────────────

    #[test]
    fn tool_result_none_gives_empty_string() {
        assert_eq!(tool_result_content_to_string(None), "");
    }

    #[test]
    fn tool_result_string_value_passed_through() {
        assert_eq!(
            tool_result_content_to_string(Some(serde_json::json!("hello"))),
            "hello"
        );
    }

    #[test]
    fn tool_result_block_array_extracts_text() {
        let v = serde_json::json!([{"type": "text", "text": "first"}, {"type": "text", "text": " second"}]);
        assert_eq!(tool_result_content_to_string(Some(v)), "first second");
    }

    #[test]
    fn tool_result_block_array_skips_non_text() {
        let v =
            serde_json::json!([{"type": "image", "source": {}}, {"type": "text", "text": "only"}]);
        assert_eq!(tool_result_content_to_string(Some(v)), "only");
    }

    #[test]
    fn tool_result_images_extracts_base64_and_url_sources() {
        let v = serde_json::json!([
            {"type": "text", "text": "ignored"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
            {"type": "image", "source": {"type": "url", "url": "https://example.com/b.jpg"}},
            {"type": "document", "source": {}}
        ]);
        let imgs = tool_result_images(Some(&v));
        let urls: Vec<&str> = imgs
            .iter()
            .filter_map(|p| match p {
                ContentPart::ImageUrl { image_url } => Some(image_url.url.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            urls,
            vec!["data:image/png;base64,AAAA", "https://example.com/b.jpg"]
        );
    }

    #[test]
    fn tool_result_images_empty_for_string_and_absent() {
        assert!(tool_result_images(None).is_empty());
        assert!(tool_result_images(Some(&serde_json::json!("plain text"))).is_empty());
    }

    // ── openai_stream_to_anthropic_sse ────────────────────────────────────────

    use crate::types::{ChatCompletionChunk, ChunkChoice, ChunkDelta};
    use futures::StreamExt;

    fn make_chunk(content: Option<&str>, finish_reason: Option<&str>) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: "chatcmpl-1".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 0,
            model: "gpt-4o".to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: None,
                    content: content.map(str::to_string),
                    tool_calls: None,
                    reasoning_content: None,
                },
                finish_reason: finish_reason.map(str::to_string),
                extra: Default::default(),
            }],
            usage: None,
            extra: Default::default(),
        }
    }

    fn make_tool_call_chunk(
        id: &str,
        name: &str,
        args: &str,
        finish_reason: Option<&str>,
    ) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: "chatcmpl-1".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 0,
            model: "gpt-4o".to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: None,
                    content: None,
                    tool_calls: Some(vec![crate::types::StreamToolCall {
                        index: 0,
                        id: Some(id.to_string()),
                        r#type: Some("function".to_string()),
                        function: Some(crate::types::StreamFunctionCall {
                            name: Some(name.to_string()),
                            arguments: Some(args.to_string()),
                        }),
                    }]),
                    reasoning_content: None,
                },
                finish_reason: finish_reason.map(str::to_string),
                extra: Default::default(),
            }],
            usage: None,
            extra: Default::default(),
        }
    }

    /// One fragmented streaming tool-call delta (id/name only in the first).
    fn tool_frag(
        index: i32,
        id: Option<&str>,
        name: Option<&str>,
        args: Option<&str>,
        finish: Option<&str>,
    ) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: "c".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 0,
            model: "k".to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: None,
                    content: None,
                    tool_calls: Some(vec![crate::types::StreamToolCall {
                        index,
                        id: id.map(str::to_string),
                        r#type: id.map(|_| "function".to_string()),
                        function: Some(crate::types::StreamFunctionCall {
                            name: name.map(str::to_string),
                            arguments: args.map(str::to_string),
                        }),
                    }]),
                    reasoning_content: None,
                },
                finish_reason: finish.map(str::to_string),
                extra: Default::default(),
            }],
            usage: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn streaming_tool_call_continuation_fragment_deserializes() {
        // Regression: continuation fragments carry only `index` + partial
        // `arguments` (no id/type/name). Modelling those as required aborted the
        // whole stream. They must now deserialize.
        let json = r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"k","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"date"}}]},"finish_reason":null}],"usage":null}"#;
        let chunk: ChatCompletionChunk =
            serde_json::from_str(json).expect("continuation fragment must deserialize");
        let tc = &chunk.choices[0].delta.tool_calls.as_ref().unwrap()[0];
        assert_eq!(tc.index, 0);
        assert!(tc.id.is_none());
        assert_eq!(
            tc.function.as_ref().unwrap().arguments.as_deref(),
            Some("date")
        );
    }

    #[tokio::test]
    async fn fragmented_tool_call_accumulates_into_one_block() {
        // id+name in the first fragment, arguments streamed across the rest.
        let chunks: Vec<Result<ChatCompletionChunk, ProxyError>> = vec![
            Ok(tool_frag(0, Some("t1"), Some("Bash"), Some(""), None)),
            Ok(tool_frag(0, None, None, Some(r#"{"cmd":""#), None)),
            Ok(tool_frag(0, None, None, Some("ls"), None)),
            Ok(tool_frag(0, None, None, Some(r#""}"#), None)),
            Ok(make_chunk(None, Some("tool_calls"))),
        ];
        let inner: ProviderStream = Box::pin(futures::stream::iter(chunks));
        let events: Vec<_> =
            openai_stream_to_anthropic_sse("m".to_string(), "msg".to_string(), inner)
                .collect()
                .await;

        assert!(events.iter().all(|e| e.is_ok()), "stream must not abort");
        let dump = events
            .iter()
            .map(|e| format!("{:?}", e.as_ref().unwrap()))
            .collect::<Vec<_>>()
            .join("\n");

        // Exactly ONE tool block: one start, one stop (not one per fragment).
        // Count the SSE event-name line ("content_block_start" also appears in
        // the JSON `type` field, so match the `event:` prefix).
        assert_eq!(
            dump.matches("event: content_block_start").count(),
            1,
            "exactly one content_block_start: {dump}"
        );
        assert_eq!(
            dump.matches("event: content_block_stop").count(),
            1,
            "exactly one content_block_stop"
        );
        // id + name captured from the first fragment.
        assert!(dump.contains("t1") && dump.contains("Bash"));
        // Three non-empty argument fragments streamed as input_json_delta.
        assert_eq!(
            dump.matches("input_json_delta").count(),
            3,
            "one input_json_delta per non-empty arg fragment: {dump}"
        );
    }

    #[tokio::test]
    async fn parallel_tool_calls_produce_distinct_blocks() {
        // Two tool calls interleaved by index 0 and 1.
        let chunks: Vec<Result<ChatCompletionChunk, ProxyError>> = vec![
            Ok(tool_frag(
                0,
                Some("a"),
                Some("Bash"),
                Some(r#"{"c":"#),
                None,
            )),
            Ok(tool_frag(
                1,
                Some("b"),
                Some("Read"),
                Some(r#"{"p":"#),
                None,
            )),
            Ok(tool_frag(0, None, None, Some("1}"), None)),
            Ok(tool_frag(1, None, None, Some("2}"), None)),
            Ok(make_chunk(None, Some("tool_calls"))),
        ];
        let inner: ProviderStream = Box::pin(futures::stream::iter(chunks));
        let events: Vec<_> =
            openai_stream_to_anthropic_sse("m".to_string(), "msg".to_string(), inner)
                .collect()
                .await;
        assert!(events.iter().all(|e| e.is_ok()));
        let dump = events
            .iter()
            .map(|e| format!("{:?}", e.as_ref().unwrap()))
            .collect::<Vec<_>>()
            .join("\n");
        // Two distinct tool blocks: two starts, two stops.
        assert_eq!(dump.matches("event: content_block_start").count(), 2);
        assert_eq!(dump.matches("event: content_block_stop").count(), 2);
        assert!(dump.contains("Bash") && dump.contains("Read"));
    }

    #[tokio::test]
    async fn negative_tool_call_index_is_clamped() {
        // A provider that sends -1 for a single tool call must not abort.
        let chunks: Vec<Result<ChatCompletionChunk, ProxyError>> = vec![
            Ok(tool_frag(
                -1,
                Some("x"),
                Some("Bash"),
                Some(r#"{"c":"ls"}"#),
                None,
            )),
            Ok(make_chunk(None, Some("tool_calls"))),
        ];
        let inner: ProviderStream = Box::pin(futures::stream::iter(chunks));
        let events: Vec<_> =
            openai_stream_to_anthropic_sse("m".to_string(), "msg".to_string(), inner)
                .collect()
                .await;
        assert!(
            events.iter().all(|e| e.is_ok()),
            "stream must not abort on index -1"
        );
        let dump = events
            .iter()
            .map(|e| format!("{:?}", e.as_ref().unwrap()))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(dump.matches("event: content_block_start").count(), 1);
        assert!(dump.contains("Bash"));
    }

    #[tokio::test]
    async fn reasoning_content_emits_thinking_block_before_text() {
        let mut r_chunk = make_chunk(None, None);
        r_chunk.choices[0].delta.reasoning_content = Some("pondering".to_string());
        let chunks: Vec<Result<ChatCompletionChunk, ProxyError>> = vec![
            Ok(r_chunk),
            Ok(make_chunk(Some("answer"), None)),
            Ok(make_chunk(None, Some("stop"))),
        ];
        let inner: ProviderStream = Box::pin(futures::stream::iter(chunks));
        let events: Vec<_> =
            openai_stream_to_anthropic_sse("m".to_string(), "msg".to_string(), inner)
                .collect()
                .await;
        assert!(events.iter().all(|e| e.is_ok()));
        let dump = events
            .iter()
            .map(|e| format!("{:?}", e.as_ref().unwrap()))
            .collect::<Vec<_>>()
            .join("\n");
        // A thinking block with a thinking_delta is emitted.
        assert!(
            dump.contains("thinking_delta"),
            "thinking_delta emitted: {dump}"
        );
        assert!(dump.contains("pondering"));
        // Thinking precedes text; two content blocks open (thinking + text).
        assert!(
            dump.find("pondering").unwrap() < dump.find("answer").unwrap(),
            "thinking must precede text"
        );
        assert_eq!(dump.matches("event: content_block_start").count(), 2);
    }

    /// Tool-only stream must NOT produce an empty text block.
    /// If it did, Anthropic would reject the next request with
    /// "messages: text content blocks must be non-empty".
    #[tokio::test]
    async fn tool_only_stream_emits_no_empty_text_block() {
        let chunks: Vec<Result<ChatCompletionChunk, ProxyError>> = vec![Ok(make_tool_call_chunk(
            "call_abc",
            "bash",
            r#"{"cmd":"ls"}"#,
            Some("tool_calls"),
        ))];
        let inner: ProviderStream = Box::pin(futures::stream::iter(chunks));
        let events: Vec<_> =
            openai_stream_to_anthropic_sse("m".to_string(), "msg_tool".to_string(), inner)
                .collect()
                .await;

        assert!(events.iter().all(|e| e.is_ok()));

        // Verify no content_block_start with type "text" appears
        for sse in events.iter().flatten() {
            let data = format!("{:?}", sse);
            if data.contains("content_block_start") {
                assert!(
                    !data.contains(r#""type":"text""#),
                    "tool-only response must not emit a text content block: {data}"
                );
            }
        }
    }

    #[tokio::test]
    async fn stream_emits_correct_event_sequence() {
        let chunks: Vec<Result<ChatCompletionChunk, ProxyError>> = vec![
            Ok(make_chunk(Some("Hello"), None)),
            Ok(make_chunk(Some(" world"), None)),
            Ok(make_chunk(None, Some("stop"))),
        ];
        let inner = futures::stream::iter(chunks);
        let inner: ProviderStream = Box::pin(inner);

        let events: Vec<_> = openai_stream_to_anthropic_sse(
            "claude-sonnet".to_string(),
            "msg_test123".to_string(),
            inner,
        )
        .collect()
        .await;

        // All events should be Ok
        assert!(events.iter().all(|e| e.is_ok()));

        // We expect: message_start, content_block_start, ping,
        //            delta("Hello"), delta(" world"),
        //            content_block_stop, message_delta, message_stop
        assert_eq!(events.len(), 8);
    }

    #[tokio::test]
    async fn message_delta_includes_input_tokens_from_final_usage() {
        // OpenAI-format upstreams report prompt_tokens only in the final chunk's
        // usage; it must surface as Anthropic `input_tokens` in message_delta.
        let mut usage_chunk = make_chunk(None, Some("stop"));
        usage_chunk.usage = Some(crate::types::Usage {
            prompt_tokens: 123,
            completion_tokens: 45,
            total_tokens: 168,
            extra: Default::default(),
        });
        let chunks: Vec<Result<ChatCompletionChunk, ProxyError>> =
            vec![Ok(make_chunk(Some("hi"), None)), Ok(usage_chunk)];
        let inner: ProviderStream = Box::pin(futures::stream::iter(chunks));

        let events: Vec<_> =
            openai_stream_to_anthropic_sse("m".to_string(), "msg_u".to_string(), inner)
                .collect()
                .await;
        let dump = events
            .iter()
            .map(|e| format!("{:?}", e.as_ref().unwrap()))
            .collect::<Vec<_>>()
            .join("\n");
        // Event Debug-formats its buffer as an escaped byte string, so quotes
        // appear as \"; match the token/value pair rather than exact JSON.
        assert!(
            dump.contains(r#"input_tokens\":123"#),
            "message_delta must carry input_tokens: {dump}"
        );
        assert!(dump.contains(r#"output_tokens\":45"#));
    }

    #[tokio::test]
    async fn empty_stream_emits_valid_sequence() {
        let inner: ProviderStream = Box::pin(futures::stream::empty());
        let events: Vec<_> =
            openai_stream_to_anthropic_sse("m".to_string(), "msg_x".to_string(), inner)
                .collect()
                .await;

        // message_start, ping, message_delta, message_stop = 4 events
        // No text block emitted — empty response has no content blocks.
        assert_eq!(events.len(), 4);
        assert!(events.iter().all(|e| e.is_ok()));
    }

    #[test]
    fn finish_reason_maps_content_filter_and_unknowns() {
        assert_eq!(finish_reason_to_anthropic("content_filter"), "refusal");
        assert_eq!(finish_reason_to_anthropic("something_new"), "end_turn");
        assert_eq!(finish_reason_to_anthropic("tool_calls"), "tool_use");
    }

    #[tokio::test]
    async fn mid_stream_error_emits_anthropic_error_event() {
        let chunks: Vec<Result<ChatCompletionChunk, ProxyError>> = vec![
            Ok(make_chunk(Some("hi"), None)),
            Err(ProxyError::RateLimited("slow down".to_string())),
        ];
        let inner: ProviderStream = Box::pin(futures::stream::iter(chunks));
        let events: Vec<_> =
            openai_stream_to_anthropic_sse("m".to_string(), "msg".to_string(), inner)
                .collect()
                .await;
        // Every event is Ok — the error surfaces as an SSE `error` event, not a
        // raw stream error (which would close the connection with no terminal event).
        assert!(events.iter().all(|e| e.is_ok()), "no raw stream errors");
        let dump = events
            .iter()
            .map(|e| format!("{:?}", e.as_ref().unwrap()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            dump.contains("event: error"),
            "an error event is emitted: {dump}"
        );
        assert!(dump.contains("rate_limit_error"));
    }

    #[tokio::test]
    async fn stream_surfaces_error_as_error_event() {
        let chunks: Vec<Result<ChatCompletionChunk, ProxyError>> = vec![
            Ok(make_chunk(Some("Hi"), None)),
            Err(ProxyError::StreamError("broken".to_string())),
        ];
        let inner: ProviderStream = Box::pin(futures::stream::iter(chunks));

        let events: Vec<_> =
            openai_stream_to_anthropic_sse("m".to_string(), "msg_x".to_string(), inner)
                .collect()
                .await;

        // The upstream error is surfaced as an Anthropic `error` SSE event, not a
        // raw stream Err (which would drop the connection with no terminal event).
        assert!(events.iter().all(|e| e.is_ok()));
        let dump = events
            .iter()
            .map(|e| format!("{:?}", e.as_ref().unwrap()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(dump.contains("event: error"));
    }

    #[tokio::test]
    async fn stream_skips_empty_content_deltas() {
        let chunks: Vec<Result<ChatCompletionChunk, ProxyError>> = vec![
            Ok(make_chunk(Some("Hi"), None)),
            Ok(make_chunk(Some(""), None)), // empty delta — should not emit event
            Ok(make_chunk(None, Some("stop"))),
        ];
        let inner: ProviderStream = Box::pin(futures::stream::iter(chunks));

        let events: Vec<_> =
            openai_stream_to_anthropic_sse("m".to_string(), "msg_x".to_string(), inner)
                .collect()
                .await;

        // message_start, content_block_start, ping, delta("Hi"),
        // content_block_stop, message_delta, message_stop = 7 (no delta for "")
        assert_eq!(events.len(), 7);
    }
}

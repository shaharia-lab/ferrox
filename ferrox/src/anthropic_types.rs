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
    /// Accepted for API compatibility; not forwarded to providers.
    #[allow(dead_code)]
    pub metadata: Option<serde_json::Value>,
    /// Accepted for API compatibility; not forwarded to providers.
    #[allow(dead_code)]
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

#[derive(Debug, Serialize)]
pub struct AnthropicModelsResponse {
    pub data: Vec<AnthropicModelObject>,
    pub has_more: bool,
    pub first_id: Option<String>,
    pub last_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AnthropicModelObject {
    #[serde(rename = "type")]
    pub object_type: String,
    pub id: String,
    pub display_name: String,
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
            .join(""),
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
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut tool_results: Vec<(String, String)> = Vec::new();

    for block in blocks {
        match block {
            AnthropicContentBlock::Text { text } => {
                text_parts.push(text);
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
                ..
            } => {
                tool_results.push((tool_use_id, tool_result_content_to_string(content)));
            }
            // Image, document, thinking, and Unknown blocks are dropped;
            // the downstream provider handles images natively via the
            // pass-through adapters if needed.
            AnthropicContentBlock::Image { .. } | AnthropicContentBlock::Unknown => {}
        }
    }

    if role == "assistant" {
        // Assistant turn: combine text + tool_calls into one message.
        let content = if text_parts.is_empty() {
            None
        } else {
            Some(MessageContent::Text(text_parts.join("")))
        };
        out.push(ChatMessage {
            role,
            content,
            name: None,
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            tool_call_id: None,
        });
    } else {
        // User turn: text first, then each tool result as a separate "tool" message.
        let combined_text = text_parts.join("");
        if !combined_text.is_empty() {
            out.push(ChatMessage {
                role: role.clone(),
                content: Some(MessageContent::Text(combined_text)),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            });
        }
        for (tool_use_id, content) in tool_results {
            out.push(ChatMessage {
                role: "tool".to_string(),
                content: Some(MessageContent::Text(content)),
                name: None,
                tool_calls: None,
                tool_call_id: Some(tool_use_id),
            });
        }
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
            .filter_map(|item| {
                if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                    item.get("text")
                        .and_then(|t| t.as_str())
                        .map(str::to_string)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(""),
        Some(other) => other.to_string(),
    }
}

// ── Translation: internal response → Anthropic ───────────────────────────────

pub fn to_anthropic_response(resp: ChatCompletionResponse) -> AnthropicMessagesResponse {
    let choice = resp.choices.into_iter().next();

    let mut content: Vec<AnthropicResponseContent> = Vec::new();

    if let Some(ref c) = choice {
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

    // Always emit at least one content block so the SDK can parse the response.
    if content.is_empty() {
        content.push(AnthropicResponseContent::Text {
            text: String::new(),
        });
    }

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
        other => other,
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
        stop_reason: "end_turn".to_string(),
        stream_done: false,
        text_block_started: false,
        text_block_closed: false,
        next_block_index: 0,
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
                    // Close the text block only if it was actually opened
                    if s.text_block_started && !s.text_block_closed {
                        s.text_block_closed = true;
                        s.pending.push_back(Ok(make_content_block_stop_event(0)));
                    }
                    s.pending.push_back(Ok(make_message_delta_event(
                        &s.stop_reason,
                        s.output_tokens,
                    )));
                    s.pending.push_back(Ok(make_message_stop_event()));
                    // Loop back to drain pending
                }
                Some(Err(e)) => return Some((Err(e), s)),
                Some(Ok(chunk)) => {
                    // Update accumulated state
                    if let Some(usage) = &chunk.usage {
                        s.output_tokens = usage.completion_tokens;
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

                    if !text.is_empty() {
                        // Open the text block on first actual text content.
                        if !s.text_block_started {
                            s.text_block_started = true;
                            s.pending.push_back(Ok(make_content_block_start_event(0)));
                            s.next_block_index = 1;
                        }
                        s.pending
                            .push_back(Ok(make_content_block_delta_event(0, &text)));
                    }

                    // Emit tool_use blocks for each tool call
                    for tc in &tool_calls {
                        // Close the text block before starting tool_use blocks,
                        // but only if it was actually opened.
                        if s.text_block_started && !s.text_block_closed {
                            s.text_block_closed = true;
                            s.pending.push_back(Ok(make_content_block_stop_event(0)));
                        }
                        let block_index = s.next_block_index;
                        s.next_block_index += 1;
                        s.pending.push_back(Ok(make_tool_use_block_start_event(
                            block_index,
                            &tc.id,
                            &tc.function.name,
                        )));
                        s.pending.push_back(Ok(make_input_json_delta_event(
                            block_index,
                            &tc.function.arguments,
                        )));
                        s.pending
                            .push_back(Ok(make_content_block_stop_event(block_index)));
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

fn make_message_delta_event(stop_reason: &str, output_tokens: u32) -> Event {
    let data = serde_json::json!({
        "type": "message_delta",
        "delta": {
            "stop_reason": stop_reason,
            "stop_sequence": null
        },
        "usage": {"output_tokens": output_tokens}
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
        assert_eq!(out.messages[0].role, "user");
        assert_eq!(out.messages[1].role, "tool");
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
                },
                finish_reason: Some(finish_reason.to_string()),
            }],
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            }),
            system_fingerprint: None,
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
                },
                finish_reason: Some("tool_calls".to_string()),
            }],
            usage: None,
            system_fingerprint: None,
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
    fn finish_reason_passthrough_for_unknown() {
        assert_eq!(
            finish_reason_to_anthropic("content_filter"),
            "content_filter"
        );
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
                },
                finish_reason: finish_reason.map(str::to_string),
            }],
            usage: None,
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
                    tool_calls: Some(vec![crate::types::ToolCall {
                        id: id.to_string(),
                        r#type: "function".to_string(),
                        function: crate::types::FunctionCall {
                            name: name.to_string(),
                            arguments: args.to_string(),
                        },
                    }]),
                },
                finish_reason: finish_reason.map(str::to_string),
            }],
            usage: None,
        }
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
        for ev in &events {
            if let Ok(sse) = ev {
                let data = format!("{:?}", sse);
                if data.contains("content_block_start") {
                    assert!(
                        !data.contains(r#""type":"text""#),
                        "tool-only response must not emit a text content block: {data}"
                    );
                }
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

    #[tokio::test]
    async fn stream_propagates_error() {
        let chunks: Vec<Result<ChatCompletionChunk, ProxyError>> = vec![
            Ok(make_chunk(Some("Hi"), None)),
            Err(ProxyError::StreamError("broken".to_string())),
        ];
        let inner: ProviderStream = Box::pin(futures::stream::iter(chunks));

        let events: Vec<_> =
            openai_stream_to_anthropic_sse("m".to_string(), "msg_x".to_string(), inner)
                .collect()
                .await;

        // Should contain the error somewhere
        assert!(events.iter().any(|e| e.is_err()));
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

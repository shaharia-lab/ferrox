//! Internal chat output → OpenAI **Responses API** output.
//!
//! The inverse of [`crate::responses_types::to_chat_completion_request`]: every
//! provider adapter answers in chat-completions shape, and this module turns
//! that answer back into what a Responses client expects.
//!
//! - [`to_responses_response`] encodes a non-streaming [`ChatCompletionResponse`]
//!   as a [`Response`] object.
//! - [`ResponsesEmitter`] is a framework-free state machine that turns a stream
//!   of [`ChatCompletionChunk`]s into the Responses SSE event sequence, emitting
//!   [`SseFrame`]s the same way the Anthropic emitter in
//!   [`crate::anthropic_types`] does:
//!
//!   ```text
//!   response.created → response.in_progress
//!   → per output item: output_item.added
//!       message:   content_part.added → output_text.delta… → output_text.done → content_part.done
//!       reasoning: content_part.added → reasoning_text.delta… → reasoning_text.done → content_part.done
//!       function:  function_call_arguments.delta… → function_call_arguments.done
//!       custom:    custom_tool_call_input.delta → custom_tool_call_input.done
//!     → output_item.done
//!   → exactly one of response.completed / response.incomplete / response.failed
//!   ```
//!
//!   Every event carries a monotonic `sequence_number` starting at 0, and its
//!   SSE `event:` name equals its `type`. There is no `[DONE]` sentinel.
//! - [`responses_stream_to_frames`] drives the emitter over a
//!   [`ProviderStream`]; `responses_stream_to_sse` (feature `axum`) adapts it to
//!   axum's SSE `Event`.
//!
//! Usage is reported only in the terminal event, and the terminal event is
//! emitted only once the upstream stream has ended — so a metering wrapper
//! around the [`ProviderStream`] (Ferrox's `RequestFinalizer::wrap_stream`) has
//! seen the usage-bearing chunk before the client sees the terminal event.

use std::collections::VecDeque;

use serde::Serialize;
use serde_json::Value;

use crate::error::ProxyError;
use crate::providers::ProviderStream;
use crate::responses_types::{
    encode_anthropic_thinking_signature, IncompleteDetails, InputTokensDetails, OutputContent,
    OutputItem, OutputTokensDetails, ReasoningText, Response, ResponseError, ResponseUsage,
    ResponsesRequest, ResponsesTool,
};
use crate::sse::SseFrame;
use crate::types::{
    cache_tokens, ChatCompletionChunk, ChatCompletionResponse, MessageContent, ToolCall, Usage,
};

/// Key on [`crate::types::ChatMessage::extra`] (non-streaming) and
/// [`crate::types::ChunkChoice::extra`] (streaming) under which an adapter
/// hands over the **signature** of an Anthropic thinking block.
///
/// When present, the encoder emits it on the reasoning item as
/// `encrypted_content` (via [`encode_anthropic_thinking_signature`]), so a
/// stateless client can send it back on the next turn. When absent — every
/// non-Anthropic upstream — `encrypted_content` is omitted. The Anthropic
/// adapter is the producer (#183).
pub const RESPONSES_THINKING_SIGNATURE: &str = "_anthropic_thinking_signature";

/// A fresh Responses `id`: `resp_` plus a random suffix. Generated once per
/// response; item ids are derived from it rather than generated per item.
pub fn new_response_id() -> String {
    format!("resp_{}", uuid::Uuid::new_v4().simple())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── Shared encoding helpers ──────────────────────────────────────────────────

/// Item id derived from the response id, unique per response and output index.
fn item_id(prefix: &str, suffix: &str, output_index: u32) -> String {
    format!("{prefix}_{suffix}_{output_index}")
}

/// `(id, call_id)` of a tool-call item: `fc_…` for a function call, `ctc_…` for
/// a custom tool call, and a generated `call_id` when the upstream sent none —
/// the client needs one to send the tool's output back.
fn tool_ids(
    suffix: &str,
    output_index: u32,
    call_id: Option<String>,
    custom: bool,
) -> (String, String) {
    let id = item_id(if custom { "ctc" } else { "fc" }, suffix, output_index);
    let call_id = call_id
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| item_id("call", suffix, output_index));
    (id, call_id)
}

/// The part of a response id after `resp_`, reused as the item-id suffix.
fn id_suffix(response_id: &str) -> String {
    response_id
        .strip_prefix("resp_")
        .unwrap_or(response_id)
        .to_string()
}

/// A `Response` echoing the request parameters, with no output yet.
fn skeleton(req: &ResponsesRequest, id: String, created_at: u64) -> Response {
    Response {
        id,
        object: "response".to_string(),
        created_at,
        status: "in_progress".to_string(),
        model: req.model.clone(),
        output: Vec::new(),
        usage: None,
        error: None,
        incomplete_details: None,
        instructions: req.instructions.clone(),
        metadata: req.metadata.clone(),
        parallel_tool_calls: req.parallel_tool_calls.unwrap_or(true),
        tool_choice: req
            .tool_choice
            .clone()
            .unwrap_or_else(|| Value::from("auto")),
        tools: req.tools.as_deref().map(echo_tools).unwrap_or_default(),
        temperature: req.temperature,
        top_p: req.top_p,
        max_output_tokens: req.max_output_tokens,
        previous_response_id: None,
        reasoning: req.reasoning.clone(),
        text: req.text.clone(),
        // Stateless gateway: nothing is ever stored, whatever the client asked.
        store: Some(false),
        service_tier: req.service_tier.clone(),
        user: req.user.clone(),
    }
}

/// The request's tools in their Responses wire shape. Built-in and unknown
/// tools never reach here — the request translation rejects them.
fn echo_tools(tools: &[ResponsesTool]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|t| match t {
            ResponsesTool::Function(f) => Some(serde_json::json!({
                "type": "function",
                "name": f.name,
                "description": f.description,
                "parameters": f.parameters,
                "strict": f.strict,
            })),
            ResponsesTool::Custom(c) => {
                let mut v = serde_json::json!({
                    "type": "custom",
                    "name": c.name,
                    "description": c.description,
                });
                if let Some(format) = &c.format {
                    v["format"] = format.clone();
                }
                Some(v)
            }
            ResponsesTool::Builtin(_) | ResponsesTool::Unknown(_) => None,
        })
        .collect()
}

/// Names of the request's `custom` tools, whose calls become
/// `custom_tool_call` items instead of `function_call` ones.
fn custom_tool_names(req: &ResponsesRequest) -> Vec<String> {
    req.tools
        .iter()
        .flatten()
        .filter_map(|t| match t {
            ResponsesTool::Custom(c) => Some(c.name.clone()),
            _ => None,
        })
        .collect()
}

/// A custom tool travels upstream as a function taking `{"input": "…"}`; this
/// recovers the free-form input. Arguments that are not that shape pass
/// through verbatim rather than being lost.
fn custom_tool_input(arguments: &str) -> String {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|v| v.get("input").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| arguments.to_string())
}

/// `(status, incomplete_details)` for a chat `finish_reason`.
fn terminal_status(finish_reason: Option<&str>) -> (&'static str, Option<IncompleteDetails>) {
    let incomplete = |reason: &str| {
        (
            "incomplete",
            Some(IncompleteDetails {
                reason: reason.to_string(),
            }),
        )
    };
    match finish_reason {
        Some("length") => incomplete("max_output_tokens"),
        Some("content_filter") => incomplete("content_filter"),
        _ => ("completed", None),
    }
}

/// Chat usage → Responses usage. Cache counters are read through
/// [`cache_tokens`], the same single reading every other surface uses.
pub fn responses_usage(usage: &Usage) -> ResponseUsage {
    let (cached, cache_write) = cache_tokens(usage);
    let reasoning_tokens = usage
        .extra
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    ResponseUsage {
        input_tokens: usage.prompt_tokens,
        input_tokens_details: InputTokensDetails {
            cached_tokens: cached,
            cache_write_tokens: cache_write,
        },
        output_tokens: usage.completion_tokens,
        output_tokens_details: OutputTokensDetails { reasoning_tokens },
        total_tokens: usage.total_tokens,
    }
}

/// `ResponseError.code` is a closed enum in the SDKs, so only its generic
/// members are used.
fn error_code(e: &ProxyError) -> &'static str {
    match e {
        ProxyError::RateLimited(_) | ProxyError::BudgetExceeded(_) => "rate_limit_exceeded",
        ProxyError::InvalidRequest { .. } => "invalid_prompt",
        _ => "server_error",
    }
}

fn error_message(e: &ProxyError) -> String {
    let (_, body) = crate::error::openai_error_body(e);
    body["error"]["message"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| e.to_string())
}

fn signature_from(extra: &std::collections::HashMap<String, Value>) -> Option<&str> {
    extra
        .get(RESPONSES_THINKING_SIGNATURE)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn reasoning_item(id: String, text: String, signature: Option<&str>, status: &str) -> OutputItem {
    OutputItem::Reasoning {
        id,
        summary: Vec::new(),
        content: Some(vec![ReasoningText {
            kind: "reasoning_text".to_string(),
            text,
        }]),
        encrypted_content: signature.map(encode_anthropic_thinking_signature),
        status: Some(status.to_string()),
    }
}

fn message_item(id: String, text: String, status: &str) -> OutputItem {
    OutputItem::Message {
        id,
        role: "assistant".to_string(),
        status: status.to_string(),
        content: vec![OutputContent::OutputText {
            text,
            annotations: Vec::new(),
            logprobs: Some(Vec::new()),
        }],
    }
}

fn tool_item(
    id: String,
    call_id: String,
    name: String,
    arguments: String,
    custom: bool,
    status: &str,
) -> OutputItem {
    if custom {
        OutputItem::CustomToolCall {
            id,
            call_id,
            name,
            input: custom_tool_input(&arguments),
            status: Some(status.to_string()),
        }
    } else {
        OutputItem::FunctionCall {
            id,
            call_id,
            name,
            arguments,
            status: status.to_string(),
        }
    }
}

// ── Non-streaming ────────────────────────────────────────────────────────────

/// Encode a chat completion as a Responses `Response` object.
///
/// `output` holds, in order: a `reasoning` item when the model returned
/// `reasoning_content`, a `message` item when it returned text, and one
/// `function_call` (or `custom_tool_call`, for a tool declared `custom`) item
/// per tool call. `status` / `incomplete_details` follow `finish_reason`, the
/// request parameters are echoed back, and `store` is always `false`.
///
/// `response_id` is the caller's (see [`new_response_id`]), so it can be
/// logged before encoding, exactly as for [`ResponsesEmitter::new`].
pub fn to_responses_response(
    resp: ChatCompletionResponse,
    req: &ResponsesRequest,
    response_id: impl Into<String>,
) -> Response {
    let id = response_id.into();
    let suffix = id_suffix(&id);
    let mut response = skeleton(req, id, resp.created);
    let custom = custom_tool_names(req);

    let choice = resp.choices.into_iter().next();
    let finish_reason = choice.as_ref().and_then(|c| c.finish_reason.clone());
    let (status, incomplete_details) = terminal_status(finish_reason.as_deref());
    let item_status = if status == "completed" {
        "completed"
    } else {
        "incomplete"
    };

    if let Some(choice) = choice {
        let msg = choice.message;
        let tool_calls: Vec<ToolCall> = msg.tool_calls.unwrap_or_default();
        let mut output = Vec::with_capacity(2 + tool_calls.len());
        let mut next = 0u32;

        if let Some(reasoning) = msg.reasoning_content.filter(|r| !r.is_empty()) {
            output.push(reasoning_item(
                item_id("rs", &suffix, next),
                reasoning,
                signature_from(&msg.extra),
                item_status,
            ));
            next += 1;
        }

        let text = match msg.content {
            Some(MessageContent::Text(t)) => t,
            Some(MessageContent::Parts(parts)) => parts
                .into_iter()
                .filter_map(|p| match p {
                    crate::types::ContentPart::Text { text, .. } => Some(text),
                    _ => None,
                })
                .collect(),
            None => String::new(),
        };
        if !text.is_empty() {
            output.push(message_item(
                item_id("msg", &suffix, next),
                text,
                item_status,
            ));
            next += 1;
        }

        for tc in tool_calls {
            let is_custom = custom.contains(&tc.function.name);
            let (id, call_id) = tool_ids(&suffix, next, Some(tc.id), is_custom);
            output.push(tool_item(
                id,
                call_id,
                tc.function.name,
                tc.function.arguments,
                is_custom,
                item_status,
            ));
            next += 1;
        }
        response.output = output;
    }

    response.status = status.to_string();
    response.incomplete_details = incomplete_details;
    response.usage = resp.usage.as_ref().map(responses_usage);
    response
}

// ── Streaming ────────────────────────────────────────────────────────────────

/// One SSE payload, serialized from borrows so a delta event copies nothing but
/// the bytes it writes. Unset fields are omitted.
#[derive(Serialize, Default)]
struct Event<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<&'a Response>,
    #[serde(skip_serializing_if = "Option::is_none")]
    item_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_index: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_index: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    item: Option<&'a OutputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    part: Option<Part<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<&'a str>,
    /// Always `[]`: required on the `output_text` events, never populated.
    #[serde(skip_serializing_if = "Option::is_none")]
    logprobs: Option<[(); 0]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    param: Option<&'a str>,
    sequence_number: u64,
}

/// A content part in `content_part.added` / `.done`.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Part<'a> {
    OutputText {
        text: &'a str,
        annotations: [(); 0],
        logprobs: [(); 0],
    },
    ReasoningText {
        text: &'a str,
    },
}

/// Serialize `event` with the next sequence number and queue it.
fn push(frames: &mut VecDeque<SseFrame>, seq: &mut u64, mut event: Event<'_>) {
    event.sequence_number = *seq;
    *seq += 1;
    // Serializing borrowed strings, numbers and a `Response` cannot fail.
    let data = serde_json::to_string(&event).unwrap_or_default();
    frames.push_back(SseFrame::new(event.kind, data));
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TextKind {
    Message,
    Reasoning,
}

/// The open text-bearing item (a `message` or a `reasoning` item). At most one
/// is open at a time: opening one closes the other.
struct OpenText {
    kind: TextKind,
    output_index: u32,
    id: String,
    buf: String,
}

/// One tool call, keyed by its chat `index`. Tool items stay open until the
/// stream ends, so interleaved fragments of parallel calls each land in their
/// own item.
struct ToolState {
    chat_index: u32,
    output_index: u32,
    id: String,
    call_id: String,
    name: String,
    args: String,
    custom: bool,
}

/// Framework-free state machine turning [`ChatCompletionChunk`]s into the
/// Responses streaming event sequence.
///
/// Feed it with [`on_chunk`](Self::on_chunk), then end it with exactly one of
/// [`finish`](Self::finish) (upstream ended) or [`on_error`](Self::on_error)
/// (upstream failed). Each call returns the frames it produced; calls after
/// the terminal event produce nothing.
///
/// The full [`Response`] is assembled once, for the terminal event: deltas are
/// serialized from borrows of the running buffers and never clone it.
pub struct ResponsesEmitter {
    /// Echo of the request, reused for `created` / `in_progress` and filled in
    /// for the terminal event.
    response: Response,
    suffix: String,
    custom_tools: Vec<String>,
    seq: u64,
    started: bool,
    finished: bool,
    frames: VecDeque<SseFrame>,
    /// Finished items by output index; `None` while the item is still open.
    output: Vec<Option<OutputItem>>,
    open_text: Option<OpenText>,
    tools: Vec<ToolState>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    signature: Option<String>,
}

impl ResponsesEmitter {
    /// A new emitter for a response to `req` with the given `response_id`
    /// (see [`new_response_id`]).
    pub fn new(req: &ResponsesRequest, response_id: impl Into<String>) -> Self {
        let id = response_id.into();
        let suffix = id_suffix(&id);
        Self {
            response: skeleton(req, id, unix_now()),
            suffix,
            custom_tools: custom_tool_names(req),
            seq: 0,
            started: false,
            finished: false,
            frames: VecDeque::with_capacity(8),
            output: Vec::with_capacity(4),
            open_text: None,
            tools: Vec::with_capacity(2),
            finish_reason: None,
            usage: None,
            signature: None,
        }
    }

    /// The response id every event refers to.
    pub fn response_id(&self) -> &str {
        &self.response.id
    }

    /// Consume one upstream chunk.
    pub fn on_chunk(&mut self, chunk: ChatCompletionChunk) -> impl Iterator<Item = SseFrame> + '_ {
        self.push_chunk(chunk);
        self.frames.drain(..)
    }

    /// The upstream ended normally: close every open item and emit the terminal
    /// `response.completed` or `response.incomplete` event.
    pub fn finish(&mut self) -> impl Iterator<Item = SseFrame> + '_ {
        self.push_finish();
        self.frames.drain(..)
    }

    /// The upstream failed. Before any event was sent this is a bare `error`
    /// event; afterwards it is `response.failed` carrying `response.error`.
    pub fn on_error(&mut self, e: &ProxyError) -> impl Iterator<Item = SseFrame> + '_ {
        self.push_error(e);
        self.frames.drain(..)
    }

    fn next_frame(&mut self) -> Option<SseFrame> {
        self.frames.pop_front()
    }

    fn start(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        for kind in ["response.created", "response.in_progress"] {
            push(
                &mut self.frames,
                &mut self.seq,
                Event {
                    kind,
                    response: Some(&self.response),
                    ..Event::default()
                },
            );
        }
    }

    fn next_output_index(&mut self) -> u32 {
        self.output.push(None);
        (self.output.len() - 1) as u32
    }

    fn push_chunk(&mut self, chunk: ChatCompletionChunk) {
        if self.finished {
            return;
        }
        self.start();
        if let Some(usage) = chunk.usage {
            self.usage = Some(usage);
        }
        let Some(choice) = chunk.choices.into_iter().next() else {
            return;
        };
        if let Some(reason) = choice.finish_reason {
            self.finish_reason = Some(reason);
        }
        if let Some(sig) = signature_from(&choice.extra) {
            self.signature = Some(sig.to_string());
        }
        let delta = choice.delta;
        if let Some(reasoning) = delta.reasoning_content.filter(|r| !r.is_empty()) {
            self.text_delta(TextKind::Reasoning, &reasoning);
        }
        if let Some(text) = delta.content.filter(|t| !t.is_empty()) {
            self.text_delta(TextKind::Message, &text);
        }
        for tc in delta.tool_calls.into_iter().flatten() {
            // Some providers send -1 for a single tool call; clamp to 0 like
            // the official SDKs.
            let chat_index = tc.index.max(0) as u32;
            let (name, args) = match tc.function {
                Some(f) => (f.name, f.arguments),
                None => (None, None),
            };
            let pos = match self.tools.iter().position(|t| t.chat_index == chat_index) {
                Some(pos) => pos,
                None => self.open_tool(chat_index, tc.id, name.unwrap_or_default()),
            };
            if let Some(args) = args.filter(|a| !a.is_empty()) {
                let tool = &mut self.tools[pos];
                tool.args.push_str(&args);
                // A custom tool's arguments are `{"input": …}` JSON, not its
                // input, so its input is sent whole once the call completes.
                if !tool.custom {
                    push(
                        &mut self.frames,
                        &mut self.seq,
                        Event {
                            kind: "response.function_call_arguments.delta",
                            item_id: Some(&tool.id),
                            output_index: Some(tool.output_index),
                            delta: Some(&args),
                            ..Event::default()
                        },
                    );
                }
            }
        }
    }

    fn text_delta(&mut self, kind: TextKind, delta: &str) {
        if self.open_text.as_ref().map(|o| o.kind) != Some(kind) {
            self.close_text("completed");
            self.open_text(kind);
        }
        let Some(open) = self.open_text.as_mut() else {
            return;
        };
        open.buf.push_str(delta);
        let kind = match open.kind {
            TextKind::Message => "response.output_text.delta",
            TextKind::Reasoning => "response.reasoning_text.delta",
        };
        push(
            &mut self.frames,
            &mut self.seq,
            Event {
                kind,
                item_id: Some(&open.id),
                output_index: Some(open.output_index),
                content_index: Some(0),
                delta: Some(delta),
                logprobs: (open.kind == TextKind::Message).then_some([]),
                ..Event::default()
            },
        );
    }

    fn open_text(&mut self, kind: TextKind) {
        let output_index = self.next_output_index();
        let (prefix, cap) = match kind {
            TextKind::Message => ("msg", 256),
            TextKind::Reasoning => ("rs", 256),
        };
        let id = item_id(prefix, &self.suffix, output_index);
        let item = match kind {
            TextKind::Message => OutputItem::Message {
                id: id.clone(),
                role: "assistant".to_string(),
                status: "in_progress".to_string(),
                content: Vec::new(),
            },
            TextKind::Reasoning => OutputItem::Reasoning {
                id: id.clone(),
                summary: Vec::new(),
                content: None,
                encrypted_content: None,
                status: Some("in_progress".to_string()),
            },
        };
        push(
            &mut self.frames,
            &mut self.seq,
            Event {
                kind: "response.output_item.added",
                output_index: Some(output_index),
                item: Some(&item),
                ..Event::default()
            },
        );
        push(
            &mut self.frames,
            &mut self.seq,
            Event {
                kind: "response.content_part.added",
                item_id: Some(&id),
                output_index: Some(output_index),
                content_index: Some(0),
                part: Some(empty_part(kind)),
                ..Event::default()
            },
        );
        self.open_text = Some(OpenText {
            kind,
            output_index,
            id,
            buf: String::with_capacity(cap),
        });
    }

    /// Close the open message / reasoning item, if any: `*.done` →
    /// `content_part.done` → `output_item.done`.
    fn close_text(&mut self, status: &str) {
        let Some(OpenText {
            kind,
            output_index,
            id,
            buf,
        }) = self.open_text.take()
        else {
            return;
        };
        let item = match kind {
            TextKind::Message => message_item(id, buf, status),
            TextKind::Reasoning => {
                reasoning_item(id, buf, self.signature.take().as_deref(), status)
            }
        };
        let (id, text) = item_text(&item);
        let done_kind = match kind {
            TextKind::Message => "response.output_text.done",
            TextKind::Reasoning => "response.reasoning_text.done",
        };
        push(
            &mut self.frames,
            &mut self.seq,
            Event {
                kind: done_kind,
                item_id: Some(id),
                output_index: Some(output_index),
                content_index: Some(0),
                text: Some(text),
                logprobs: (kind == TextKind::Message).then_some([]),
                ..Event::default()
            },
        );
        push(
            &mut self.frames,
            &mut self.seq,
            Event {
                kind: "response.content_part.done",
                item_id: Some(id),
                output_index: Some(output_index),
                content_index: Some(0),
                part: Some(match kind {
                    TextKind::Message => Part::OutputText {
                        text,
                        annotations: [],
                        logprobs: [],
                    },
                    TextKind::Reasoning => Part::ReasoningText { text },
                }),
                ..Event::default()
            },
        );
        push(
            &mut self.frames,
            &mut self.seq,
            Event {
                kind: "response.output_item.done",
                output_index: Some(output_index),
                item: Some(&item),
                ..Event::default()
            },
        );
        self.output[output_index as usize] = Some(item);
    }

    /// Open a tool-call item for a newly seen chat index; returns its position
    /// in `self.tools`.
    fn open_tool(&mut self, chat_index: u32, call_id: Option<String>, name: String) -> usize {
        self.close_text("completed");
        let output_index = self.next_output_index();
        let custom = self.custom_tools.contains(&name);
        let (id, call_id) = tool_ids(&self.suffix, output_index, call_id, custom);
        let item = if custom {
            OutputItem::CustomToolCall {
                id: id.clone(),
                call_id: call_id.clone(),
                name: name.clone(),
                input: String::new(),
                status: Some("in_progress".to_string()),
            }
        } else {
            OutputItem::FunctionCall {
                id: id.clone(),
                call_id: call_id.clone(),
                name: name.clone(),
                arguments: String::new(),
                status: "in_progress".to_string(),
            }
        };
        push(
            &mut self.frames,
            &mut self.seq,
            Event {
                kind: "response.output_item.added",
                output_index: Some(output_index),
                item: Some(&item),
                ..Event::default()
            },
        );
        self.tools.push(ToolState {
            chat_index,
            output_index,
            id,
            call_id,
            name,
            args: String::with_capacity(128),
            custom,
        });
        self.tools.len() - 1
    }

    fn close_tools(&mut self, status: &str) {
        for tool in std::mem::take(&mut self.tools) {
            let item = tool_item(
                tool.id,
                tool.call_id,
                tool.name,
                tool.args,
                tool.custom,
                status,
            );
            match &item {
                OutputItem::FunctionCall {
                    id,
                    name,
                    arguments,
                    ..
                } => push(
                    &mut self.frames,
                    &mut self.seq,
                    Event {
                        kind: "response.function_call_arguments.done",
                        item_id: Some(id),
                        output_index: Some(tool.output_index),
                        name: Some(name),
                        arguments: Some(arguments),
                        ..Event::default()
                    },
                ),
                OutputItem::CustomToolCall { id, input, .. } => {
                    if !input.is_empty() {
                        push(
                            &mut self.frames,
                            &mut self.seq,
                            Event {
                                kind: "response.custom_tool_call_input.delta",
                                item_id: Some(id),
                                output_index: Some(tool.output_index),
                                delta: Some(input),
                                ..Event::default()
                            },
                        );
                    }
                    push(
                        &mut self.frames,
                        &mut self.seq,
                        Event {
                            kind: "response.custom_tool_call_input.done",
                            item_id: Some(id),
                            output_index: Some(tool.output_index),
                            input: Some(input),
                            ..Event::default()
                        },
                    );
                }
                _ => {}
            }
            push(
                &mut self.frames,
                &mut self.seq,
                Event {
                    kind: "response.output_item.done",
                    output_index: Some(tool.output_index),
                    item: Some(&item),
                    ..Event::default()
                },
            );
            self.output[tool.output_index as usize] = Some(item);
        }
    }

    fn push_finish(&mut self) {
        if self.finished {
            return;
        }
        self.start();
        let (status, incomplete_details) = terminal_status(self.finish_reason.as_deref());
        let item_status = if status == "completed" {
            "completed"
        } else {
            "incomplete"
        };
        // Close in output order: opening a tool closes the open text item, so
        // a text item still open was opened after every open tool.
        self.close_tools(item_status);
        self.close_text(item_status);
        self.response.status = status.to_string();
        self.response.incomplete_details = incomplete_details;
        let kind = if status == "completed" {
            "response.completed"
        } else {
            "response.incomplete"
        };
        self.emit_terminal(kind);
    }

    fn push_error(&mut self, e: &ProxyError) {
        if self.finished {
            return;
        }
        let message = error_message(e);
        if !self.started {
            // No response exists for the client yet: a bare `error` event.
            self.finished = true;
            let (_, body) = crate::error::openai_error_body(e);
            let code = body["error"]["type"].as_str().unwrap_or("server_error");
            let param = match e {
                ProxyError::InvalidRequest { param, .. } => param.as_deref(),
                _ => None,
            };
            push(
                &mut self.frames,
                &mut self.seq,
                Event {
                    kind: "error",
                    code: Some(code),
                    message: Some(&message),
                    param,
                    ..Event::default()
                },
            );
            return;
        }
        // Items still open are recorded as incomplete in the failed response;
        // their `.done` events are not sent — the response failed, not them.
        if let Some(OpenText {
            kind,
            output_index,
            id,
            buf,
        }) = self.open_text.take()
        {
            self.output[output_index as usize] = Some(match kind {
                TextKind::Message => message_item(id, buf, "incomplete"),
                TextKind::Reasoning => {
                    reasoning_item(id, buf, self.signature.take().as_deref(), "incomplete")
                }
            });
        }
        for tool in std::mem::take(&mut self.tools) {
            self.output[tool.output_index as usize] = Some(tool_item(
                tool.id,
                tool.call_id,
                tool.name,
                tool.args,
                tool.custom,
                "incomplete",
            ));
        }
        self.response.status = "failed".to_string();
        self.response.error = Some(ResponseError {
            code: error_code(e).to_string(),
            message,
        });
        self.emit_terminal("response.failed");
    }

    /// Assemble the full `Response` — the only time it is built — and emit it.
    fn emit_terminal(&mut self, kind: &'static str) {
        self.finished = true;
        self.response.output = std::mem::take(&mut self.output)
            .into_iter()
            .flatten()
            .collect();
        self.response.usage = self.usage.as_ref().map(responses_usage);
        push(
            &mut self.frames,
            &mut self.seq,
            Event {
                kind,
                response: Some(&self.response),
                ..Event::default()
            },
        );
    }
}

fn empty_part(kind: TextKind) -> Part<'static> {
    match kind {
        TextKind::Message => Part::OutputText {
            text: "",
            annotations: [],
            logprobs: [],
        },
        TextKind::Reasoning => Part::ReasoningText { text: "" },
    }
}

/// `(id, text)` of a message / reasoning item built by `close_text`.
fn item_text(item: &OutputItem) -> (&str, &str) {
    match item {
        OutputItem::Message { id, content, .. } => match content.first() {
            Some(OutputContent::OutputText { text, .. }) => (id, text),
            _ => (id, ""),
        },
        OutputItem::Reasoning { id, content, .. } => (
            id,
            content
                .as_deref()
                .and_then(<[_]>::first)
                .map_or("", |c| c.text.as_str()),
        ),
        OutputItem::FunctionCall { id, .. } | OutputItem::CustomToolCall { id, .. } => (id, ""),
    }
}

/// Drive a [`ResponsesEmitter`] over an upstream chunk stream.
///
/// Upstream errors are encoded in-band (`response.failed`, or `error` before
/// the first event), so the stream never yields `Err`; the `Result` item type
/// mirrors the Anthropic emitter so both plug into the same SSE plumbing.
pub fn responses_stream_to_frames(
    emitter: ResponsesEmitter,
    stream: ProviderStream,
) -> impl futures::Stream<Item = Result<SseFrame, ProxyError>> + Send {
    use futures::StreamExt as _;

    futures::stream::unfold(
        (emitter, stream, false),
        |(mut emitter, mut inner, mut done)| async move {
            loop {
                if let Some(frame) = emitter.next_frame() {
                    return Some((Ok(frame), (emitter, inner, done)));
                }
                if done {
                    return None;
                }
                match inner.next().await {
                    Some(Ok(chunk)) => emitter.push_chunk(chunk),
                    Some(Err(e)) => {
                        done = true;
                        emitter.push_error(&e);
                    }
                    None => {
                        done = true;
                        emitter.push_finish();
                    }
                }
            }
        },
    )
}

/// [`responses_stream_to_frames`], adapted to axum's SSE `Event` — mirroring
/// `openai_stream_to_anthropic_sse`.
#[cfg(feature = "axum")]
pub fn responses_stream_to_sse(
    emitter: ResponsesEmitter,
    stream: ProviderStream,
) -> impl futures::Stream<Item = Result<axum::response::sse::Event, ProxyError>> + Send {
    use futures::StreamExt as _;
    responses_stream_to_frames(emitter, stream).map(|res| res.map(Into::into))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::responses_types::ResponseStreamEvent;
    use serde_json::json;
    use std::collections::{HashMap, HashSet};

    const RESPONSE_ID: &str = "resp_test";

    fn request() -> ResponsesRequest {
        serde_json::from_value(json!({
            "model": "gpt-x",
            "input": "What's the weather in Paris?",
            "instructions": "Be brief.",
            "tools": [
                {"type": "function", "name": "get_weather",
                 "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}},
                {"type": "custom", "name": "apply_patch", "description": "Apply a patch"}
            ],
            "tool_choice": "auto",
            "temperature": 0.2,
            "metadata": {"k": "v"},
            "store": true,
            "stream": true
        }))
        .unwrap()
    }

    /// A chunk from just its first choice's `delta` (and optional finish/usage).
    fn chunk(delta: Value, finish: Option<&str>, usage: Option<Value>) -> ChatCompletionChunk {
        serde_json::from_value(json!({
            "id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 0, "model": "up",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            "usage": usage,
        }))
        .unwrap()
    }

    fn text(t: &str) -> ChatCompletionChunk {
        chunk(json!({"content": t}), None, None)
    }

    fn reasoning(t: &str) -> ChatCompletionChunk {
        chunk(json!({"reasoning_content": t}), None, None)
    }

    fn tool(index: i32, id_name: Option<(&str, &str)>, args: &str) -> ChatCompletionChunk {
        let mut tc = json!({"index": index, "function": {"arguments": args}});
        if let Some((id, name)) = id_name {
            tc["id"] = json!(id);
            tc["type"] = json!("function");
            tc["function"]["name"] = json!(name);
        }
        chunk(json!({"tool_calls": [tc]}), None, None)
    }

    fn usage() -> Value {
        json!({
            "prompt_tokens": 20, "completion_tokens": 7, "total_tokens": 27,
            "prompt_tokens_details": {"cached_tokens": 12, "cache_write_tokens": 3},
            "completion_tokens_details": {"reasoning_tokens": 2}
        })
    }

    fn finish(reason: &str) -> ChatCompletionChunk {
        chunk(json!({}), Some(reason), Some(usage()))
    }

    enum Step {
        Chunk(ChatCompletionChunk),
        Error(ProxyError),
    }

    /// Run the emitter over `steps`, ending with `finish()` unless an error
    /// ended it first.
    fn run(steps: Vec<Step>) -> Vec<SseFrame> {
        let mut e = ResponsesEmitter::new(&request(), RESPONSE_ID);
        let mut frames = Vec::new();
        for step in steps {
            match step {
                Step::Chunk(c) => frames.extend(e.on_chunk(c)),
                Step::Error(err) => frames.extend(e.on_error(&err)),
            }
        }
        frames.extend(e.finish());
        frames
    }

    fn chunks(cs: Vec<ChatCompletionChunk>) -> Vec<Step> {
        cs.into_iter().map(Step::Chunk).collect()
    }

    /// Frames as `{event, data}` values, with the wall-clock `created_at` zeroed.
    fn normalize(frames: &[SseFrame]) -> Vec<Value> {
        frames
            .iter()
            .map(|f| {
                let mut data: Value = serde_json::from_str(&f.data).unwrap();
                if let Some(r) = data.get_mut("response") {
                    r["created_at"] = json!(0);
                }
                json!({"event": f.event, "data": data})
            })
            .collect()
    }

    fn fixture_path(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/responses")
            .join(format!("{name}.jsonl"))
    }

    /// Compare against the golden fixture; `FERROX_BLESS=1` rewrites it.
    fn assert_golden(name: &str, frames: &[SseFrame]) {
        let got = normalize(frames);
        let path = fixture_path(name);
        if std::env::var_os("FERROX_BLESS").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let body: String = got
                .iter()
                .map(|v| format!("{}\n", serde_json::to_string(v).unwrap()))
                .collect();
            std::fs::write(&path, body).unwrap();
        }
        let want: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("missing fixture {}: {e}", path.display()))
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(got.len(), want.len(), "{name}: event count");
        for (i, (g, w)) in got.iter().zip(&want).enumerate() {
            assert_eq!(g, w, "{name}: event {i} differs");
        }
        assert_invariants(frames);
    }

    /// Protocol invariants every emitted sequence must satisfy.
    fn assert_invariants(frames: &[SseFrame]) {
        assert!(!frames.is_empty());
        let events: Vec<Value> = frames
            .iter()
            .map(|f| serde_json::from_str(&f.data).unwrap())
            .collect();
        for (i, (f, ev)) in frames.iter().zip(&events).enumerate() {
            // Sequence numbers are contiguous from 0; `event:` equals `type`.
            assert_eq!(ev["sequence_number"], json!(i), "sequence_number at {i}");
            assert_eq!(ev["type"], json!(f.event));
            // Every frame parses as the typed event, and agrees on its name.
            let typed: ResponseStreamEvent = serde_json::from_str(&f.data)
                .unwrap_or_else(|e| panic!("frame {i} ({}) does not parse: {e}", f.event));
            assert_eq!(typed.event_type(), f.event);
            assert_ne!(f.data, "[DONE]");
        }
        if frames[0].event == "error" {
            assert_eq!(frames.len(), 1, "a bare error is the whole stream");
            return;
        }
        assert_eq!(frames[0].event, "response.created");
        assert_eq!(frames[1].event, "response.in_progress");
        let terminal = [
            "response.completed",
            "response.incomplete",
            "response.failed",
        ];
        let last = frames.last().unwrap();
        assert!(terminal.contains(&last.event.as_str()), "last is terminal");
        assert_eq!(
            frames
                .iter()
                .filter(|f| terminal.contains(&f.event.as_str()))
                .count(),
            1,
            "exactly one terminal event"
        );

        let mut open: HashMap<String, u64> = HashMap::new();
        let mut added = 0u64;
        let mut done_ids = Vec::new();
        for ev in &events[2..events.len() - 1] {
            let kind = ev["type"].as_str().unwrap();
            match kind {
                "response.output_item.added" => {
                    assert_eq!(ev["output_index"], json!(added), "output_index is dense");
                    let id = ev["item"]["id"].as_str().unwrap().to_string();
                    assert!(open.insert(id, added).is_none(), "item ids are unique");
                    added += 1;
                }
                "response.output_item.done" => {
                    let id = ev["item"]["id"].as_str().unwrap();
                    let idx = open.remove(id).expect("done for an open item");
                    assert_eq!(ev["output_index"], json!(idx));
                    done_ids.push(id.to_string());
                }
                _ => {
                    // Every other event targets an open item at its own index.
                    let id = ev["item_id"].as_str().expect("item_id");
                    let idx = open.get(id).expect("event for an open item");
                    assert_eq!(ev["output_index"], json!(idx), "{kind} output_index");
                    if let Some(ci) = ev.get("content_index") {
                        assert_eq!(ci, &json!(0));
                    }
                }
            }
        }
        let response = &events.last().unwrap()["response"];
        let output_ids: Vec<&str> = response["output"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .collect();
        if last.event == "response.failed" {
            assert!(response["error"].is_object());
            assert_eq!(output_ids.len() as u64, added);
        } else {
            assert!(open.is_empty(), "every item is closed before the terminal");
            assert_eq!(output_ids.len() as u64, added);
            // `output` is in output_index order.
            let mut by_index: Vec<_> = done_ids.clone();
            by_index.sort_by_key(|id| id.rsplit('_').next().unwrap().parse::<u32>().unwrap());
            assert_eq!(output_ids, by_index);
        }
    }

    // ── golden fixtures ─────────────────────────────────────────────────────

    #[test]
    fn golden_text_only() {
        let frames = run(chunks(vec![
            chunk(json!({"role": "assistant"}), None, None),
            text("It's "),
            text("sunny."),
            finish("stop"),
        ]));
        assert_golden("text_only", &frames);
    }

    #[test]
    fn golden_single_tool_call() {
        let frames = run(chunks(vec![
            tool(0, Some(("call_1", "get_weather")), ""),
            tool(0, None, "{\"city\":"),
            tool(0, None, "\"Paris\"}"),
            finish("tool_calls"),
        ]));
        assert_golden("single_tool_call", &frames);
    }

    #[test]
    fn golden_parallel_tool_calls_interleaved() {
        let frames = run(chunks(vec![
            tool(0, Some(("call_a", "get_weather")), "{\"city\":"),
            tool(1, Some(("call_b", "get_weather")), "{\"city\":"),
            tool(0, None, "\"Paris\"}"),
            tool(1, None, "\"Rome\"}"),
            finish("tool_calls"),
        ]));
        assert_golden("parallel_tool_calls", &frames);
    }

    #[test]
    fn golden_reasoning_then_text() {
        let frames = run(chunks(vec![
            reasoning("The user wants "),
            reasoning("weather."),
            text("Sunny."),
            finish("stop"),
        ]));
        assert_golden("reasoning_text", &frames);
    }

    #[test]
    fn golden_length_cutoff() {
        let frames = run(chunks(vec![text("It is sun"), finish("length")]));
        assert_golden("length_cutoff", &frames);
        let last: Value = serde_json::from_str(&frames.last().unwrap().data).unwrap();
        assert_eq!(last["response"]["status"], "incomplete");
        assert_eq!(
            last["response"]["incomplete_details"]["reason"],
            "max_output_tokens"
        );
        assert_eq!(last["response"]["output"][0]["status"], "incomplete");
    }

    #[test]
    fn golden_midstream_error() {
        let frames = run(vec![
            Step::Chunk(text("It is ")),
            Step::Error(ProxyError::StreamError("upstream reset".into())),
        ]);
        assert_golden("midstream_error", &frames);
        let last: Value = serde_json::from_str(&frames.last().unwrap().data).unwrap();
        assert_eq!(last["type"], "response.failed");
        assert_eq!(last["response"]["error"]["code"], "server_error");
        assert_eq!(last["response"]["output"][0]["status"], "incomplete");
    }

    #[test]
    fn golden_custom_tool_call() {
        let frames = run(chunks(vec![
            tool(0, Some(("call_p", "apply_patch")), "{\"input\":"),
            tool(0, None, "\"*** Begin Patch\"}"),
            finish("tool_calls"),
        ]));
        assert_golden("custom_tool_call", &frames);
    }

    #[test]
    fn golden_error_before_any_event() {
        let frames = run(vec![Step::Error(ProxyError::RateLimited(
            "slow down".into(),
        ))]);
        assert_golden("error_before_start", &frames);
    }

    #[test]
    fn empty_upstream_still_completes() {
        let frames = run(vec![]);
        assert_invariants(&frames);
        assert_eq!(
            frames.iter().map(|f| f.event.as_str()).collect::<Vec<_>>(),
            [
                "response.created",
                "response.in_progress",
                "response.completed"
            ]
        );
    }

    // ── behaviour ───────────────────────────────────────────────────────────

    fn terminal(frames: &[SseFrame]) -> Value {
        serde_json::from_str(&frames.last().unwrap().data).unwrap()
    }

    #[test]
    fn terminal_usage_equals_upstream_usage() {
        let frames = run(chunks(vec![text("x"), finish("stop")]));
        let u = &terminal(&frames)["response"]["usage"];
        assert_eq!(
            u,
            &json!({
                "input_tokens": 20,
                "input_tokens_details": {"cached_tokens": 12, "cache_write_tokens": 3},
                "output_tokens": 7,
                "output_tokens_details": {"reasoning_tokens": 2},
                "total_tokens": 27
            })
        );
        // Usage appears nowhere before the terminal event.
        for f in &frames[..frames.len() - 1] {
            let v: Value = serde_json::from_str(&f.data).unwrap();
            if let Some(r) = v.get("response") {
                assert!(r["usage"].is_null());
            }
        }
    }

    #[test]
    fn terminal_echoes_request_and_never_stores() {
        let frames = run(chunks(vec![text("x"), finish("stop")]));
        let r = &terminal(&frames)["response"];
        assert_eq!(r["id"], RESPONSE_ID);
        assert_eq!(r["model"], "gpt-x");
        assert_eq!(r["instructions"], "Be brief.");
        assert_eq!(r["metadata"], json!({"k": "v"}));
        assert_eq!(r["tool_choice"], "auto");
        assert_eq!(r["tools"][0]["name"], "get_weather");
        assert_eq!(r["tools"][1]["type"], "custom");
        assert_eq!(r["store"], false);
        assert_eq!(r["parallel_tool_calls"], true);
    }

    #[test]
    fn content_filter_is_incomplete() {
        let frames = run(chunks(vec![text("x"), finish("content_filter")]));
        let r = &terminal(&frames)["response"];
        assert_eq!(frames.last().unwrap().event, "response.incomplete");
        assert_eq!(r["incomplete_details"]["reason"], "content_filter");
    }

    #[test]
    fn anthropic_signature_becomes_encrypted_content() {
        let mut sig = reasoning("hmm");
        sig.choices[0]
            .extra
            .insert(RESPONSES_THINKING_SIGNATURE.into(), json!("sig123"));
        let frames = run(chunks(vec![sig, text("ok"), finish("stop")]));
        let r = &terminal(&frames)["response"];
        assert_eq!(
            r["output"][0]["encrypted_content"],
            encode_anthropic_thinking_signature("sig123")
        );
    }

    #[test]
    fn no_signature_omits_encrypted_content() {
        let frames = run(chunks(vec![reasoning("hmm"), text("ok"), finish("stop")]));
        let r = &terminal(&frames)["response"];
        assert!(r["output"][0].get("encrypted_content").is_none());
    }

    #[test]
    fn nothing_is_emitted_after_the_terminal_event() {
        let mut e = ResponsesEmitter::new(&request(), RESPONSE_ID);
        let _ = e.on_chunk(text("x")).count();
        assert!(e.finish().count() > 0);
        assert_eq!(e.on_chunk(text("late")).count(), 0);
        assert_eq!(e.finish().count(), 0);
        assert_eq!(e.on_error(&ProxyError::StreamError("x".into())).count(), 0);
    }

    #[test]
    fn reasoning_after_text_closes_the_message_first() {
        let frames = run(chunks(vec![
            text("a"),
            reasoning("b"),
            text("c"),
            finish("stop"),
        ]));
        assert_invariants(&frames);
        let r = &terminal(&frames)["response"];
        let kinds: Vec<&str> = r["output"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["message", "reasoning", "message"]);
    }

    #[test]
    fn text_after_tool_call_keeps_output_order() {
        let frames = run(chunks(vec![
            tool(0, Some(("call_1", "get_weather")), "{}"),
            text("done"),
            finish("stop"),
        ]));
        assert_invariants(&frames);
        let r = &terminal(&frames)["response"];
        assert_eq!(r["output"][0]["type"], "function_call");
        assert_eq!(r["output"][1]["type"], "message");
    }

    #[tokio::test]
    async fn stream_adapter_matches_driving_the_emitter_by_hand() {
        use futures::StreamExt as _;
        let upstream = vec![text("It's "), text("sunny."), finish("stop")];
        let by_hand = run(chunks(upstream.clone()));
        let stream: ProviderStream = Box::pin(futures::stream::iter(
            upstream.into_iter().map(Ok::<_, ProxyError>),
        ));
        let frames: Vec<SseFrame> =
            responses_stream_to_frames(ResponsesEmitter::new(&request(), RESPONSE_ID), stream)
                .map(|r| r.unwrap())
                .collect()
                .await;
        assert_eq!(normalize(&frames), normalize(&by_hand));
    }

    #[tokio::test]
    async fn stream_adapter_encodes_upstream_error_in_band() {
        use futures::StreamExt as _;
        let stream: ProviderStream = Box::pin(futures::stream::iter(vec![
            Ok(text("x")),
            Err(ProxyError::StreamError("boom".into())),
            Ok(text("never")),
        ]));
        let frames: Vec<_> =
            responses_stream_to_frames(ResponsesEmitter::new(&request(), RESPONSE_ID), stream)
                .collect()
                .await;
        assert!(frames.iter().all(Result::is_ok));
        let frames: Vec<SseFrame> = frames.into_iter().map(Result::unwrap).collect();
        assert_invariants(&frames);
        assert_eq!(frames.last().unwrap().event, "response.failed");
    }

    /// Randomised sequences of reasoning / text / interleaved tool fragments,
    /// optionally cut by an error, must always satisfy the protocol invariants.
    #[test]
    fn random_sequences_keep_invariants() {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut rand = move |n: u64| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) % n
        };
        for _ in 0..500 {
            let mut steps = Vec::new();
            let mut seen = HashSet::new();
            for _ in 0..rand(12) {
                steps.push(Step::Chunk(match rand(4) {
                    0 => reasoning("r"),
                    1 => text("t"),
                    _ => {
                        let idx = rand(3) as i32;
                        let first = seen.insert(idx);
                        let name = if rand(4) == 0 {
                            "apply_patch"
                        } else {
                            "get_weather"
                        };
                        tool(idx, first.then_some(("call", name)), "{}")
                    }
                }));
            }
            match rand(4) {
                0 => steps.push(Step::Error(ProxyError::StreamError("x".into()))),
                1 => steps.push(Step::Chunk(finish("length"))),
                _ => steps.push(Step::Chunk(finish("stop"))),
            }
            assert_invariants(&run(steps));
        }
    }

    // ── non-streaming ───────────────────────────────────────────────────────

    fn chat_response(message: Value, finish: &str) -> ChatCompletionResponse {
        serde_json::from_value(json!({
            "id": "chatcmpl-1", "object": "chat.completion", "created": 1_700_000_000,
            "model": "up",
            "choices": [{"index": 0, "message": message, "finish_reason": finish}],
            "usage": usage(),
            "system_fingerprint": null
        }))
        .unwrap()
    }

    #[test]
    fn non_streaming_encodes_reasoning_message_and_tool_calls_in_order() {
        let resp = to_responses_response(
            chat_response(
                json!({
                    "role": "assistant",
                    "content": "Checking.",
                    "reasoning_content": "Need weather.",
                    "tool_calls": [
                        {"id": "call_1", "type": "function",
                         "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}},
                        {"id": "call_2", "type": "function",
                         "function": {"name": "apply_patch", "arguments": "{\"input\":\"diff\"}"}}
                    ]
                }),
                "tool_calls",
            ),
            &request(),
            RESPONSE_ID,
        );
        assert_eq!(resp.id, RESPONSE_ID);
        assert_eq!(resp.status, "completed");
        assert_eq!(resp.created_at, 1_700_000_000);
        assert_eq!(resp.store, Some(false));
        let v = serde_json::to_value(&resp).unwrap();
        let out = v["output"].as_array().unwrap();
        assert_eq!(out.len(), 4);
        assert_eq!(out[0]["type"], "reasoning");
        assert_eq!(out[0]["content"][0]["text"], "Need weather.");
        assert!(out[0].get("encrypted_content").is_none());
        assert_eq!(out[1]["type"], "message");
        assert_eq!(out[1]["content"][0]["text"], "Checking.");
        assert_eq!(out[2]["type"], "function_call");
        assert_eq!(out[2]["call_id"], "call_1");
        assert!(out[2]["id"].as_str().unwrap().starts_with("fc_"));
        assert_eq!(out[2]["arguments"], "{\"city\":\"Paris\"}");
        assert_eq!(out[3]["type"], "custom_tool_call");
        assert!(out[3]["id"].as_str().unwrap().starts_with("ctc_"));
        assert_eq!(out[3]["input"], "diff");
        assert_eq!(v["usage"]["input_tokens_details"]["cached_tokens"], 12);
        assert_eq!(v["usage"]["input_tokens_details"]["cache_write_tokens"], 3);
        assert_eq!(v["usage"]["output_tokens_details"]["reasoning_tokens"], 2);
        assert_eq!(v["instructions"], "Be brief.");
        assert_eq!(v["temperature"], json!(0.2f32));
    }

    #[test]
    fn non_streaming_tool_call_without_id_gets_a_call_id() {
        let resp = to_responses_response(
            chat_response(
                json!({"role": "assistant", "content": null, "tool_calls": [
                    {"id": "", "type": "function",
                     "function": {"name": "get_weather", "arguments": "{}"}}
                ]}),
                "tool_calls",
            ),
            &request(),
            RESPONSE_ID,
        );
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["output"][0]["call_id"], "call_test_0");
    }

    #[test]
    fn streaming_tool_call_without_id_gets_a_call_id() {
        let frames = run(chunks(vec![
            tool(0, Some(("", "get_weather")), "{}"),
            finish("tool_calls"),
        ]));
        assert_eq!(
            terminal(&frames)["response"]["output"][0]["call_id"],
            "call_test_0"
        );
    }

    #[test]
    fn non_streaming_length_is_incomplete_with_max_output_tokens() {
        let resp = to_responses_response(
            chat_response(json!({"role": "assistant", "content": "It is"}), "length"),
            &request(),
            RESPONSE_ID,
        );
        assert_eq!(resp.status, "incomplete");
        assert_eq!(resp.incomplete_details.unwrap().reason, "max_output_tokens");
    }

    #[test]
    fn non_streaming_content_filter_is_incomplete() {
        let resp = to_responses_response(
            chat_response(
                json!({"role": "assistant", "content": ""}),
                "content_filter",
            ),
            &request(),
            RESPONSE_ID,
        );
        assert_eq!(resp.status, "incomplete");
        assert_eq!(resp.incomplete_details.unwrap().reason, "content_filter");
        assert!(resp.output.is_empty());
    }

    #[test]
    fn non_streaming_signature_becomes_encrypted_content() {
        let resp = to_responses_response(
            chat_response(
                json!({"role": "assistant", "content": "ok", "reasoning_content": "hmm",
                       RESPONSES_THINKING_SIGNATURE: "sig"}),
                "stop",
            ),
            &request(),
            RESPONSE_ID,
        );
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            v["output"][0]["encrypted_content"],
            encode_anthropic_thinking_signature("sig")
        );
    }

    #[test]
    fn non_streaming_response_fixture() {
        let resp = to_responses_response(
            chat_response(
                json!({"role": "assistant", "content": "Sunny.", "reasoning_content": "Easy."}),
                "stop",
            ),
            &request(),
            RESPONSE_ID,
        );
        // Through the wire string: `to_value` would widen `f32` fields.
        let got: Value = serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        let path = fixture_path("non_streaming");
        if std::env::var_os("FERROX_BLESS").is_some() {
            std::fs::write(&path, format!("{got}\n")).unwrap();
        }
        let want: Value =
            serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim()).unwrap();
        assert_eq!(got, want);
    }
}

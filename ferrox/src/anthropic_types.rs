use axum::response::sse::Event;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

use crate::error::ProxyError;
use crate::providers::ProviderStream;
use crate::types::{
    ChatCompletionRequest, ChatCompletionResponse, ChatMessage, ContentPart, MessageContent,
    StopSequences,
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
    /// Accepted for API compatibility; image forwarding is not supported yet.
    Image {
        #[allow(dead_code)]
        source: serde_json::Value,
    },
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
    Text { text: String },
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

    let messages = req
        .messages
        .into_iter()
        .map(|m| ChatMessage {
            role: m.role,
            content: Some(anthropic_content_to_internal(m.content)),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        })
        .collect();

    ChatCompletionRequest {
        model: req.model,
        messages,
        stream: req.stream,
        temperature: req.temperature,
        max_tokens: Some(req.max_tokens),
        top_p: req.top_p,
        stop: req.stop_sequences.map(StopSequences::Multiple),
        tools: None,
        tool_choice: None,
        system,
        extra: HashMap::new(),
    }
}

fn anthropic_content_to_internal(content: AnthropicMessageContent) -> MessageContent {
    match content {
        AnthropicMessageContent::Text(t) => MessageContent::Text(t),
        AnthropicMessageContent::Blocks(blocks) => {
            let parts: Vec<ContentPart> = blocks
                .into_iter()
                .filter_map(|b| match b {
                    AnthropicContentBlock::Text { text } => Some(ContentPart::Text { text }),
                    AnthropicContentBlock::Image { .. } => None,
                })
                .collect();

            // Collapse single-text-part back to a plain string for cleaner forwarding
            if parts.len() == 1 {
                let part = parts.into_iter().next().unwrap();
                if let ContentPart::Text { text } = part {
                    return MessageContent::Text(text);
                }
                // unreachable: only Text parts are produced above
                MessageContent::Parts(vec![])
            } else {
                MessageContent::Parts(parts)
            }
        }
    }
}

// ── Translation: internal response → Anthropic ───────────────────────────────

pub fn to_anthropic_response(resp: ChatCompletionResponse) -> AnthropicMessagesResponse {
    let choice = resp.choices.into_iter().next();

    let text = choice
        .as_ref()
        .and_then(|c| c.message.content.as_ref())
        .map(|c| match c {
            MessageContent::Text(t) => t.clone(),
            MessageContent::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        })
        .unwrap_or_default();

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
        content: vec![AnthropicResponseContent::Text { text }],
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
                        // Empty upstream — still emit a valid Anthropic sequence
                        s.is_first = false;
                        s.pending
                            .push_back(Ok(make_message_start_event(&s.msg_id, &s.model, 0)));
                        s.pending.push_back(Ok(make_content_block_start_event()));
                        s.pending.push_back(Ok(make_ping_event()));
                    }
                    s.pending.push_back(Ok(make_content_block_stop_event()));
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

                    if s.is_first {
                        s.is_first = false;
                        let input_tokens =
                            chunk.usage.as_ref().map(|u| u.prompt_tokens).unwrap_or(0);
                        s.pending.push_back(Ok(make_message_start_event(
                            &s.msg_id,
                            &s.model,
                            input_tokens,
                        )));
                        s.pending.push_back(Ok(make_content_block_start_event()));
                        s.pending.push_back(Ok(make_ping_event()));
                    }

                    if !text.is_empty() {
                        s.pending
                            .push_back(Ok(make_content_block_delta_event(&text)));
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

fn make_content_block_start_event() -> Event {
    let data = serde_json::json!({
        "type": "content_block_start",
        "index": 0,
        "content_block": {"type": "text", "text": ""}
    });
    Event::default()
        .event("content_block_start")
        .data(data.to_string())
}

fn make_ping_event() -> Event {
    Event::default()
        .event("ping")
        .data(serde_json::json!({"type": "ping"}).to_string())
}

fn make_content_block_delta_event(text: &str) -> Event {
    let data = serde_json::json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": {"type": "text_delta", "text": text}
    });
    Event::default()
        .event("content_block_delta")
        .data(data.to_string())
}

fn make_content_block_stop_event() -> Event {
    Event::default()
        .event("content_block_stop")
        .data(serde_json::json!({"type": "content_block_stop", "index": 0}).to_string())
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

    // ── finish_reason_to_anthropic ────────────────────────────────────────────

    #[test]
    fn finish_reason_passthrough_for_unknown() {
        assert_eq!(
            finish_reason_to_anthropic("content_filter"),
            "content_filter"
        );
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

        // message_start, content_block_start, ping, content_block_stop,
        // message_delta, message_stop = 6 events
        assert_eq!(events.len(), 6);
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

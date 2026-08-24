//! Framework-free Server-Sent Event frames.
//!
//! The Anthropic streaming translation in [`crate::anthropic_types`] emits these
//! rather than a web framework's own event type, so a consumer on any framework
//! (or none) can drive the same state machine. Ferrox converts them to
//! `axum::response::sse::Event` behind the `axum` feature; another consumer
//! writes the equivalent few-line adapter for its own framework.

/// One Server-Sent Event: the `event:` name and its `data:` payload.
///
/// Deliberately minimal — the Anthropic Messages protocol uses only these two
/// fields, never SSE's `id:`, `retry:` or comment lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    /// The `event:` name, e.g. `message_start`, `content_block_delta`.
    pub event: String,
    /// The `data:` payload — a JSON document for every Anthropic event.
    pub data: String,
}

impl SseFrame {
    /// Build a frame from anything string-like.
    pub fn new(event: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            event: event.into(),
            data: data.into(),
        }
    }
}

#[cfg(feature = "axum")]
impl From<SseFrame> for axum::response::sse::Event {
    fn from(frame: SseFrame) -> Self {
        axum::response::sse::Event::default()
            .event(frame.event)
            .data(frame.data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_stores_both_fields() {
        let frame = SseFrame::new("ping", r#"{"type":"ping"}"#);
        assert_eq!(frame.event, "ping");
        assert_eq!(frame.data, r#"{"type":"ping"}"#);
    }

    #[test]
    fn frames_compare_by_value() {
        assert_eq!(SseFrame::new("a", "1"), SseFrame::new("a", "1"));
        assert_ne!(SseFrame::new("a", "1"), SseFrame::new("a", "2"));
    }

    // The axum adapter is what keeps Ferrox's wire output byte-identical to what
    // the emitter produced before it spoke frames, so assert exactly that: going
    // through a frame must yield the same event as building one directly.
    #[cfg(feature = "axum")]
    #[test]
    fn converts_to_the_same_axum_event_as_building_one_directly() {
        use axum::response::sse::Event;

        for (name, data) in [
            ("message_stop", r#"{"type":"message_stop"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            ),
        ] {
            let via_frame: Event = SseFrame::new(name, data).into();
            let direct = Event::default().event(name).data(data);
            assert_eq!(
                format!("{via_frame:?}"),
                format!("{direct:?}"),
                "adapter must reproduce the pre-refactor event for {name}"
            );
        }
    }
}

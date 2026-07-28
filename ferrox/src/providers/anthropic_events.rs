use serde_json::Value;

use crate::error::ProxyError;
use crate::types::{
    ChatCompletionChunk, ChunkChoice, ChunkDelta, StreamFunctionCall, StreamToolCall, Usage,
};

// ── Shared Anthropic SSE event processor ──────────────────────────────────────

/// Accumulated mutable per-message state for the Anthropic SSE event state machine.
///
/// Both the native Anthropic adapter and the Bedrock adapter use the same Anthropic
/// event format. This struct centralises the state so that bug fixes and new event
/// types only need to be applied once.
pub struct AnthropicEventProcessor {
    pub message_id: String,
    pending_tool_id: String,
    pending_tool_name: String,
    pending_tool_index: u32,
    pub stop_reason: Option<String>,
    pub usage: Option<Usage>,
    /// Prompt tokens from the `message_start` event. Anthropic reports
    /// `input_tokens` there, not in `message_delta`, so we capture it up front
    /// and fold it into the final usage.
    input_tokens: u32,
}

impl AnthropicEventProcessor {
    pub fn new(message_id: String) -> Self {
        Self {
            message_id,
            pending_tool_id: String::new(),
            pending_tool_name: String::new(),
            pending_tool_index: 0,
            stop_reason: None,
            usage: None,
            input_tokens: 0,
        }
    }

    /// Process one Anthropic SSE event and return zero or more stream chunks.
    ///
    /// `event_type` is the value of the `type` field in the JSON payload.
    /// `data` is the raw JSON string for the event (as received over SSE).
    /// `provider_name` is used only for `"error"` events.
    pub fn process(
        &mut self,
        event_type: &str,
        data: &str,
        model_id: &str,
        provider_name: &str,
    ) -> Vec<Result<ChatCompletionChunk, ProxyError>> {
        let mut results = Vec::new();

        let v: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return results,
        };

        match event_type {
            "message_start" => {
                if let Some(id) = v.pointer("/message/id").and_then(|v| v.as_str()) {
                    self.message_id = id.to_string();
                }
                // Anthropic delivers prompt tokens in `message_start`, not the
                // trailing `message_delta`; capture it for the final usage.
                if let Some(input) = v
                    .pointer("/message/usage/input_tokens")
                    .and_then(|t| t.as_u64())
                {
                    self.input_tokens = input as u32;
                }
            }
            "content_block_start" => {
                if v.pointer("/content_block/type").and_then(|t| t.as_str()) == Some("tool_use") {
                    self.pending_tool_id = v
                        .pointer("/content_block/id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    self.pending_tool_name = v
                        .pointer("/content_block/name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    self.pending_tool_index =
                        v.pointer("/index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                    // Emit the opening fragment (id + name) so consumers get the
                    // tool call's identity before its arguments stream in.
                    results.push(Ok(make_tool_call_start_chunk(
                        &self.message_id,
                        model_id,
                        self.pending_tool_index,
                        &self.pending_tool_id,
                        &self.pending_tool_name,
                    )));
                }
            }
            "content_block_delta" => {
                let delta_type = v
                    .pointer("/delta/type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                match delta_type {
                    "text_delta" => {
                        let text = v
                            .pointer("/delta/text")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        if !text.is_empty() {
                            results.push(Ok(make_text_chunk(&self.message_id, model_id, text)));
                        }
                    }
                    "input_json_delta" => {
                        let partial = v
                            .pointer("/delta/partial_json")
                            .and_then(|t| t.as_str())
                            .unwrap_or("");
                        // Stream each argument fragment incrementally.
                        if !self.pending_tool_id.is_empty() && !partial.is_empty() {
                            results.push(Ok(make_tool_call_args_chunk(
                                &self.message_id,
                                model_id,
                                self.pending_tool_index,
                                partial,
                            )));
                        }
                    }
                    "thinking_delta" => {
                        // Extended-thinking text → OpenAI-style reasoning_content.
                        let thinking = v
                            .pointer("/delta/thinking")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        if !thinking.is_empty() {
                            results.push(Ok(make_reasoning_chunk(
                                &self.message_id,
                                model_id,
                                thinking,
                            )));
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                // Tool-call fragments were streamed incrementally on start/delta;
                // just clear the pending state here.
                if !self.pending_tool_id.is_empty() {
                    self.pending_tool_id.clear();
                    self.pending_tool_name.clear();
                }
            }
            "message_delta" => {
                self.stop_reason = v
                    .pointer("/delta/stop_reason")
                    .and_then(|r| r.as_str())
                    .map(map_stop_reason);
                self.usage = parse_usage_from_message_delta(&v, self.input_tokens);
            }
            "message_stop" => {
                results.push(Ok(make_final_chunk(
                    &self.message_id,
                    model_id,
                    self.stop_reason.take(),
                    self.usage.take(),
                )));
            }
            "error" => {
                let msg = v
                    .pointer("/error/message")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| data.to_string());
                results.push(Err(ProxyError::ProviderError {
                    provider: provider_name.to_string(),
                    status: 500,
                    message: msg,
                }));
            }
            _ => {}
        }

        results
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub fn map_stop_reason(r: &str) -> String {
    match r {
        "end_turn" | "stop_sequence" | "pause_turn" => "stop".to_string(),
        "max_tokens" | "model_context_window_exceeded" => "length".to_string(),
        "tool_use" => "tool_calls".to_string(),
        "refusal" => "content_filter".to_string(),
        _ => "stop".to_string(),
    }
}

fn parse_usage_from_message_delta(v: &Value, input_tokens: u32) -> Option<Usage> {
    let output = v.pointer("/usage/output_tokens").and_then(|t| t.as_u64())? as u32;
    Some(Usage {
        prompt_tokens: input_tokens,
        completion_tokens: output,
        total_tokens: input_tokens + output,
        extra: Default::default(),
    })
}

pub fn make_text_chunk(id: &str, model: &str, text: String) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_string(),
        object: "chat.completion.chunk".to_string(),
        created: chrono::Utc::now().timestamp() as u64,
        model: model.to_string(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                role: None,
                content: Some(text),
                tool_calls: None,
                reasoning_content: None,
            },
            finish_reason: None,
            extra: Default::default(),
        }],
        usage: None,
        extra: Default::default(),
    }
}

pub fn make_reasoning_chunk(id: &str, model: &str, thinking: String) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_string(),
        object: "chat.completion.chunk".to_string(),
        created: chrono::Utc::now().timestamp() as u64,
        model: model.to_string(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                role: None,
                content: None,
                tool_calls: None,
                reasoning_content: Some(thinking),
            },
            finish_reason: None,
            extra: Default::default(),
        }],
        usage: None,
        extra: Default::default(),
    }
}

fn stream_tool_chunk(id: &str, model: &str, stc: StreamToolCall) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_string(),
        object: "chat.completion.chunk".to_string(),
        created: chrono::Utc::now().timestamp() as u64,
        model: model.to_string(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                role: None,
                content: None,
                tool_calls: Some(vec![stc]),
                reasoning_content: None,
            },
            finish_reason: None,
            extra: Default::default(),
        }],
        usage: None,
        extra: Default::default(),
    }
}

/// Opening fragment of a streaming tool call: id + name, empty arguments.
pub fn make_tool_call_start_chunk(
    id: &str,
    model: &str,
    index: u32,
    tool_id: &str,
    tool_name: &str,
) -> ChatCompletionChunk {
    stream_tool_chunk(
        id,
        model,
        StreamToolCall {
            index: index as i32,
            id: Some(tool_id.to_string()),
            r#type: Some("function".to_string()),
            function: Some(StreamFunctionCall {
                name: Some(tool_name.to_string()),
                arguments: Some(String::new()),
            }),
        },
    )
}

/// Continuation fragment: an `arguments` piece for an already-opened tool call.
pub fn make_tool_call_args_chunk(
    id: &str,
    model: &str,
    index: u32,
    args: &str,
) -> ChatCompletionChunk {
    stream_tool_chunk(
        id,
        model,
        StreamToolCall {
            index: index as i32,
            id: None,
            r#type: None,
            function: Some(StreamFunctionCall {
                name: None,
                arguments: Some(args.to_string()),
            }),
        },
    )
}

pub fn make_final_chunk(
    id: &str,
    model: &str,
    stop_reason: Option<String>,
    usage: Option<Usage>,
) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_string(),
        object: "chat.completion.chunk".to_string(),
        created: chrono::Utc::now().timestamp() as u64,
        model: model.to_string(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                role: None,
                content: None,
                tool_calls: None,
                reasoning_content: None,
            },
            finish_reason: stop_reason,
            extra: Default::default(),
        }],
        usage,
        extra: Default::default(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_processor() -> AnthropicEventProcessor {
        AnthropicEventProcessor::new("msg_test".to_string())
    }

    #[test]
    fn message_start_updates_id() {
        let mut p = make_processor();
        let data = r#"{"type":"message_start","message":{"id":"msg_abc","type":"message"}}"#;
        let results = p.process("message_start", data, "claude-3", "anthropic");
        assert!(results.is_empty());
        assert_eq!(p.message_id, "msg_abc");
    }

    #[test]
    fn text_delta_yields_chunk() {
        let mut p = make_processor();
        let data = r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}"#;
        let results = p.process("content_block_delta", data, "claude-3", "anthropic");
        assert_eq!(results.len(), 1);
        let chunk = results[0].as_ref().unwrap();
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("Hello"));
    }

    #[test]
    fn empty_text_delta_yields_nothing() {
        let mut p = make_processor();
        let data = r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":""}}"#;
        let results = p.process("content_block_delta", data, "claude-3", "anthropic");
        assert!(results.is_empty());
    }

    #[test]
    fn tool_call_streams_incrementally() {
        let mut p = make_processor();

        // Start → opening fragment: id + name, empty args, no arg data yet.
        let start = r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tool_1","name":"get_weather"}}"#;
        let r = p.process("content_block_start", start, "claude-3", "anthropic");
        assert_eq!(r.len(), 1);
        let tc = &r[0].as_ref().unwrap().choices[0]
            .delta
            .tool_calls
            .as_ref()
            .unwrap()[0];
        assert_eq!(tc.id.as_deref(), Some("tool_1"));
        let f = tc.function.as_ref().unwrap();
        assert_eq!(f.name.as_deref(), Some("get_weather"));
        assert_eq!(f.arguments.as_deref(), Some("")); // empty at open

        // Each input_json_delta → an arguments fragment (no id/name).
        let delta1 = r#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{\"loc"}}"#;
        let r = p.process("content_block_delta", delta1, "claude-3", "anthropic");
        assert_eq!(r.len(), 1);
        let tc = &r[0].as_ref().unwrap().choices[0]
            .delta
            .tool_calls
            .as_ref()
            .unwrap()[0];
        assert!(tc.id.is_none(), "continuation fragment carries no id");
        assert_eq!(
            tc.function.as_ref().unwrap().arguments.as_deref(),
            Some("{\"loc")
        );

        let delta2 = r#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"ation\":\"NYC\"}"}}"#;
        assert_eq!(
            p.process("content_block_delta", delta2, "claude-3", "anthropic")
                .len(),
            1
        );

        // Stop emits nothing (fragments already streamed) and clears state.
        assert!(p
            .process(
                "content_block_stop",
                r#"{"type":"content_block_stop"}"#,
                "claude-3",
                "anthropic"
            )
            .is_empty());
        assert!(p.pending_tool_id.is_empty());
    }

    #[test]
    fn thinking_delta_yields_reasoning_chunk() {
        let mut p = make_processor();
        let data =
            r#"{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"hmm"}}"#;
        let results = p.process("content_block_delta", data, "claude-3", "anthropic");
        assert_eq!(results.len(), 1);
        let delta = &results[0].as_ref().unwrap().choices[0].delta;
        assert_eq!(delta.reasoning_content.as_deref(), Some("hmm"));
        assert!(delta.content.is_none());
    }

    #[test]
    fn message_delta_sets_stop_reason_and_usage() {
        let mut p = make_processor();
        let data = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}"#;
        let results = p.process("message_delta", data, "claude-3", "anthropic");
        assert!(results.is_empty());
        assert_eq!(p.stop_reason.as_deref(), Some("stop"));
        assert_eq!(p.usage.as_ref().unwrap().completion_tokens, 42);
    }

    #[test]
    fn message_start_input_tokens_flow_into_final_usage() {
        let mut p = make_processor();
        // Anthropic reports prompt tokens in message_start, not message_delta.
        let start =
            r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":123}}}"#;
        p.process("message_start", start, "claude-3", "anthropic");
        let delta = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#;
        p.process("message_delta", delta, "claude-3", "anthropic");
        let usage = p.usage.as_ref().unwrap();
        assert_eq!(
            usage.prompt_tokens, 123,
            "prompt tokens must come from message_start"
        );
        assert_eq!(usage.completion_tokens, 7);
        assert_eq!(usage.total_tokens, 130);
    }

    #[test]
    fn message_stop_emits_final_chunk() {
        let mut p = make_processor();
        // Seed stop reason and usage via message_delta first
        let delta = r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":10}}"#;
        p.process("message_delta", delta, "claude-3", "anthropic");

        let stop = r#"{"type":"message_stop"}"#;
        let results = p.process("message_stop", stop, "claude-3", "anthropic");
        assert_eq!(results.len(), 1);
        let chunk = results[0].as_ref().unwrap();
        assert_eq!(chunk.choices[0].finish_reason.as_deref(), Some("length"));
        assert_eq!(chunk.usage.as_ref().unwrap().completion_tokens, 10);
        // state should be consumed
        assert!(p.stop_reason.is_none());
        assert!(p.usage.is_none());
    }

    #[test]
    fn error_event_yields_err() {
        let mut p = make_processor();
        let data = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        let results = p.process("error", data, "claude-3", "anthropic");
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
    }

    #[test]
    fn map_stop_reason_known_values() {
        assert_eq!(map_stop_reason("end_turn"), "stop");
        assert_eq!(map_stop_reason("max_tokens"), "length");
        assert_eq!(map_stop_reason("tool_use"), "tool_calls");
        assert_eq!(map_stop_reason("refusal"), "content_filter");
        assert_eq!(map_stop_reason("pause_turn"), "stop");
        assert_eq!(map_stop_reason("model_context_window_exceeded"), "length");
        // Unknown/future reasons default to a valid OpenAI value.
        assert_eq!(map_stop_reason("custom"), "stop");
    }
}

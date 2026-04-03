use serde_json::Value;

use crate::error::ProxyError;
use crate::types::{ChatCompletionChunk, ChunkChoice, ChunkDelta, FunctionCall, ToolCall, Usage};

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
    pending_tool_args: String,
    pending_tool_index: u32,
    pub stop_reason: Option<String>,
    pub usage: Option<Usage>,
}

impl AnthropicEventProcessor {
    pub fn new(message_id: String) -> Self {
        Self {
            message_id,
            pending_tool_id: String::new(),
            pending_tool_name: String::new(),
            pending_tool_args: String::new(),
            pending_tool_index: 0,
            stop_reason: None,
            usage: None,
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
                    self.pending_tool_args.clear();
                    self.pending_tool_index =
                        v.pointer("/index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
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
                        self.pending_tool_args.push_str(partial);
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                if !self.pending_tool_id.is_empty() {
                    let tool_call = ToolCall {
                        id: self.pending_tool_id.clone(),
                        r#type: "function".to_string(),
                        function: FunctionCall {
                            name: self.pending_tool_name.clone(),
                            arguments: self.pending_tool_args.clone(),
                        },
                    };
                    results.push(Ok(make_tool_call_chunk(
                        &self.message_id,
                        model_id,
                        self.pending_tool_index,
                        tool_call,
                    )));
                    self.pending_tool_id.clear();
                    self.pending_tool_name.clear();
                    self.pending_tool_args.clear();
                }
            }
            "message_delta" => {
                self.stop_reason = v
                    .pointer("/delta/stop_reason")
                    .and_then(|r| r.as_str())
                    .map(map_stop_reason);
                self.usage = parse_usage_from_message_delta(&v);
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
        "end_turn" => "stop".to_string(),
        "max_tokens" => "length".to_string(),
        "tool_use" => "tool_calls".to_string(),
        other => other.to_string(),
    }
}

fn parse_usage_from_message_delta(v: &Value) -> Option<Usage> {
    let output = v.pointer("/usage/output_tokens").and_then(|t| t.as_u64())? as u32;
    Some(Usage {
        prompt_tokens: 0,
        completion_tokens: output,
        total_tokens: output,
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
            },
            finish_reason: None,
        }],
        usage: None,
    }
}

pub fn make_tool_call_chunk(
    id: &str,
    model: &str,
    index: u32,
    tool_call: ToolCall,
) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_string(),
        object: "chat.completion.chunk".to_string(),
        created: chrono::Utc::now().timestamp() as u64,
        model: model.to_string(),
        choices: vec![ChunkChoice {
            index,
            delta: ChunkDelta {
                role: None,
                content: None,
                tool_calls: Some(vec![tool_call]),
            },
            finish_reason: None,
        }],
        usage: None,
    }
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
            },
            finish_reason: stop_reason,
        }],
        usage,
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
    fn tool_call_accumulation_and_emit() {
        let mut p = make_processor();

        // Start tool use block
        let start = r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tool_1","name":"get_weather"}}"#;
        assert!(p
            .process("content_block_start", start, "claude-3", "anthropic")
            .is_empty());

        // Accumulate input JSON
        let delta1 = r#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{\"loc"}}"#;
        assert!(p
            .process("content_block_delta", delta1, "claude-3", "anthropic")
            .is_empty());
        let delta2 = r#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"ation\":\"NYC\"}"}}"#;
        assert!(p
            .process("content_block_delta", delta2, "claude-3", "anthropic")
            .is_empty());

        // Stop block — should emit tool call chunk
        let stop = r#"{"type":"content_block_stop"}"#;
        let results = p.process("content_block_stop", stop, "claude-3", "anthropic");
        assert_eq!(results.len(), 1);
        let chunk = results[0].as_ref().unwrap();
        let tc = &chunk.choices[0].delta.tool_calls.as_ref().unwrap()[0];
        assert_eq!(tc.id, "tool_1");
        assert_eq!(tc.function.name, "get_weather");
        assert_eq!(tc.function.arguments, r#"{"location":"NYC"}"#);

        // pending state is cleared
        assert!(p.pending_tool_id.is_empty());
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
        assert_eq!(map_stop_reason("custom"), "custom");
    }
}

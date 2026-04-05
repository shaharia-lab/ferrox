use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
                            ContentPart::Text { text } => Some(text.as_str()),
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
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub index: u32,
    pub message: ChatMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkChoice {
    pub index: u32,
    pub delta: ChunkDelta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkDelta {
    pub role: Option<String>,
    pub content: Option<String>,
    pub tool_calls: Option<Vec<ToolCall>>,
}

// ── Models list response ─────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ModelsResponse {
    pub object: String,
    pub data: Vec<ModelObject>,
}

#[derive(Debug, Serialize)]
pub struct ModelObject {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub owned_by: String,
}

// ── Request context (injected by auth middleware) ────────────────────────────

#[derive(Debug, Clone)]
pub struct RequestContext {
    pub request_id: String,
    pub key_name: String,
    pub allowed_models: Vec<String>,
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
                },
                ContentPart::Text {
                    text: "Part2".to_string(),
                },
            ])),
            name: None,
            tool_calls: None,
            tool_call_id: None,
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
        });
        assert_eq!(r.system_message(), None);
    }
}

//! What a classifier sees of a request: the text of its recent user and
//! assistant turns, the same on every inbound surface.
//!
//! Only text is taken. Images, tool calls, tool results and the system
//! prompt (`system` / `developer` messages, `instructions`) never reach a
//! classifier. Turns are read newest first and only as far back as the cap
//! allows, so a long conversation costs what is kept, not what was sent.

use ferrox_providers::responses_types::{
    InputContent, InputContentPart, InputItem, ResponsesInput, ResponsesRequest,
};

use crate::types::{ChatCompletionRequest, ContentPart, MessageContent};

/// Who said a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    fn parse(role: &str) -> Option<Self> {
        match role {
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            _ => None,
        }
    }
}

/// The text one side said in one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub role: Role,
    pub text: String,
}

/// The conversation tail handed to a classifier, oldest turn first. Either
/// empty (the request has no user text) or ending with a user turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClassifierInput {
    pub turns: Vec<Turn>,
}

impl ClassifierInput {
    /// The input for a chat-format request, at most `max_input_chars` long.
    pub fn from_chat(req: &ChatCompletionRequest, max_input_chars: usize) -> Self {
        cap(chat_turns(req), max_input_chars)
    }

    /// The input for a Responses-format request, at most `max_input_chars`
    /// long.
    pub fn from_responses(req: &ResponsesRequest, max_input_chars: usize) -> Self {
        cap(responses_turns(req), max_input_chars)
    }
}

/// The user and assistant text turns of a chat request, oldest first. Lazy:
/// a message's text is only built when its turn is asked for.
fn chat_turns(req: &ChatCompletionRequest) -> impl DoubleEndedIterator<Item = Turn> + '_ {
    req.messages.iter().filter_map(|message| {
        let role = Role::parse(&message.role)?;
        let text = match message.content.as_ref()? {
            MessageContent::Text(text) => text.clone(),
            MessageContent::Parts(parts) => join(parts.iter().filter_map(|part| match part {
                ContentPart::Text { text, .. } => Some(text.as_str()),
                ContentPart::ImageUrl { .. } => None,
            })),
        };
        turn(role, text)
    })
}

/// The user and assistant text turns of a Responses request, oldest first.
/// Lazy, like [`chat_turns`].
fn responses_turns(req: &ResponsesRequest) -> impl DoubleEndedIterator<Item = Turn> + '_ {
    let (text, items) = match &req.input {
        Some(ResponsesInput::Text(text)) => (Some(text), &[][..]),
        Some(ResponsesInput::Items(items)) => (None, items.as_slice()),
        None => (None, &[][..]),
    };
    let text = text
        .into_iter()
        .filter_map(|text| turn(Role::User, text.clone()));
    let items = items.iter().filter_map(|item| {
        let InputItem::Message(message) = item else {
            return None;
        };
        let role = Role::parse(&message.role)?;
        let text = match &message.content {
            InputContent::Text(text) => text.clone(),
            InputContent::Parts(parts) => join(parts.iter().filter_map(|part| match part {
                InputContentPart::InputText { text } | InputContentPart::OutputText { text } => {
                    Some(text.as_str())
                }
                _ => None,
            })),
        };
        turn(role, text)
    });
    text.chain(items)
}

/// The text parts of one message, one per line.
fn join<'a>(parts: impl Iterator<Item = &'a str>) -> String {
    let mut text = String::new();
    for part in parts.filter(|part| !part.is_empty()) {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(part);
    }
    text
}

/// A turn, unless the message had no text.
fn turn(role: Role, text: String) -> Option<Turn> {
    (!text.is_empty()).then_some(Turn { role, text })
}

/// Keep the last user turn and as many of the turns before it as fit in
/// `max_input_chars`, counted in characters over the turns' text.
///
/// The last user turn is what is being classified, so it is always kept: one
/// longer than the cap is cut down to its final `max_input_chars` characters,
/// the most recent text, in keeping with the rest of the cap. Anything after
/// it (assistant text in a tool loop) is dropped. Earlier turns are added
/// newest first, whole, stopping at the first that does not fit, so the
/// result is always an unbroken tail of the conversation.
fn cap(turns: impl DoubleEndedIterator<Item = Turn>, max_input_chars: usize) -> ClassifierInput {
    let mut newest_first = turns.rev().skip_while(|turn| turn.role != Role::User);
    let Some(mut last) = newest_first.next() else {
        return ClassifierInput::default();
    };

    let mut remaining = max_input_chars;
    let last_chars = last.text.chars().count();
    if last_chars > remaining {
        let cut = last
            .text
            .char_indices()
            .nth(last_chars - remaining)
            .map_or(last.text.len(), |(index, _)| index);
        last.text.drain(..cut);
        if last.text.is_empty() {
            return ClassifierInput::default();
        }
        remaining = 0;
    } else {
        remaining -= last_chars;
    }

    let mut kept = vec![last];
    // Turns are never empty, so with nothing left no earlier one is built.
    while remaining > 0 {
        let Some(turn) = newest_first.next() else {
            break;
        };
        let chars = turn.text.chars().count();
        if chars > remaining {
            break;
        }
        remaining -= chars;
        kept.push(turn);
    }
    kept.reverse();
    ClassifierInput { turns: kept }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::*;

    fn chat(messages: Value) -> ChatCompletionRequest {
        serde_json::from_value(json!({"model": "auto", "messages": messages})).unwrap()
    }

    fn responses(input: Value) -> ResponsesRequest {
        serde_json::from_value(json!({"model": "auto", "input": input})).unwrap()
    }

    fn user(text: &str) -> Turn {
        Turn {
            role: Role::User,
            text: text.to_string(),
        }
    }

    fn assistant(text: &str) -> Turn {
        Turn {
            role: Role::Assistant,
            text: text.to_string(),
        }
    }

    const NO_CAP: usize = usize::MAX;

    #[test]
    fn chat_takes_user_and_assistant_text_only() {
        let req = chat(json!([
            {"role": "system", "content": "You are terse."},
            {"role": "developer", "content": "Prefer Rust."},
            {"role": "user", "content": "What is a borrow?"},
            {"role": "assistant", "content": "A reference.", "tool_calls": [
                {"id": "c1", "type": "function",
                 "function": {"name": "lookup", "arguments": "{\"q\":\"borrow\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "c1", "content": "tool output"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "c2", "type": "function", "function": {"name": "lookup", "arguments": "{}"}}
            ]},
            {"role": "user", "content": "And a move?"},
        ]));

        assert_eq!(
            ClassifierInput::from_chat(&req, NO_CAP).turns,
            [
                user("What is a borrow?"),
                assistant("A reference."),
                user("And a move?"),
            ]
        );
    }

    #[test]
    fn chat_joins_text_parts_and_drops_the_rest() {
        let mut req = chat(json!([
            {"role": "user", "content": [
                {"type": "text", "text": "Look at this"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
                {"type": "text", "text": ""},
                {"type": "text", "text": "and explain it"},
            ]},
        ]));
        // The convenience system prompt is not part of the conversation.
        req.system = Some("You are terse.".to_string());

        assert_eq!(
            ClassifierInput::from_chat(&req, NO_CAP).turns,
            [user("Look at this\nand explain it")]
        );
    }

    #[test]
    fn chat_without_user_text_is_empty() {
        let image_only = chat(json!([
            {"role": "assistant", "content": "Hello"},
            {"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
            ]},
        ]));
        assert_eq!(
            ClassifierInput::from_chat(&image_only, NO_CAP),
            ClassifierInput::default()
        );
        assert_eq!(
            ClassifierInput::from_chat(&chat(json!([])), NO_CAP),
            ClassifierInput::default()
        );
    }

    #[test]
    fn responses_string_input_is_one_user_turn() {
        let mut req = responses(json!("Summarise this"));
        req.instructions = Some("You are terse.".to_string());

        assert_eq!(
            ClassifierInput::from_responses(&req, NO_CAP).turns,
            [user("Summarise this")]
        );
        assert_eq!(
            ClassifierInput::from_responses(&responses(Value::Null), NO_CAP),
            ClassifierInput::default()
        );
    }

    #[test]
    fn responses_takes_message_text_and_drops_other_items_and_parts() {
        let req = responses(json!([
            {"role": "developer", "content": "Prefer Rust."},
            {"role": "user", "content": "Read the config"},
            {"type": "reasoning", "id": "rs_1", "summary": []},
            {"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "Reading it."},
                {"type": "refusal", "refusal": "no"},
            ]},
            {"type": "function_call", "call_id": "c1", "name": "read", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c1", "output": "file body"},
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "Now look at this"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAAA"},
                {"type": "input_text", "text": "and fix it"},
            ]},
        ]));

        assert_eq!(
            ClassifierInput::from_responses(&req, NO_CAP).turns,
            [
                user("Read the config"),
                assistant("Reading it."),
                user("Now look at this\nand fix it"),
            ]
        );
    }

    #[test]
    fn cap_keeps_the_last_user_message_and_the_turns_that_fit_before_it() {
        let turns = || {
            vec![
                user("aaaa"),
                assistant("bbbbbb"),
                user("cc"),
                assistant("ddd"),
                user("eeeee"),
            ]
            .into_iter()
        };

        // Everything fits.
        assert_eq!(cap(turns(), 20).turns.len(), 5);
        // 5 (last) + 3 + 2 fit; the 6-character turn before them does not.
        assert_eq!(
            cap(turns(), 15).turns,
            [user("cc"), assistant("ddd"), user("eeeee")]
        );
        // The tail stays unbroken: "cc" would fit, but "ddd" before it does not.
        assert_eq!(cap(turns(), 7).turns, [user("eeeee")]);
        // Exactly the last message.
        assert_eq!(cap(turns(), 5).turns, [user("eeeee")]);
    }

    #[test]
    fn cap_truncates_an_over_long_last_message_to_its_end() {
        let turns = vec![assistant("earlier"), user("héllo wörld")].into_iter();
        // Counted in characters, cut on a character boundary.
        assert_eq!(cap(turns, 5).turns, [user("wörld")]);

        let nothing_fits = vec![user("hello")].into_iter();
        assert_eq!(cap(nothing_fits, 0), ClassifierInput::default());
    }

    #[test]
    fn cap_counts_characters_not_bytes() {
        // 4 characters, 8 bytes each side: both fit a cap of 8.
        let turns = vec![assistant("éééé"), user("üüüü")].into_iter();
        assert_eq!(cap(turns, 8).turns, [assistant("éééé"), user("üüüü")]);
    }

    #[test]
    fn cap_ends_at_the_last_user_message() {
        let turns = vec![user("fix the bug"), assistant("Looking at the file.")].into_iter();
        assert_eq!(cap(turns, 100).turns, [user("fix the bug")]);

        let no_user = vec![assistant("Hello")].into_iter();
        assert_eq!(cap(no_user, 100), ClassifierInput::default());
    }

    #[test]
    fn turns_before_the_cap_are_never_built() {
        // An over-long last message uses the whole budget, so the extractor
        // is not asked for any earlier message.
        let req = chat(json!([
            {"role": "user", "content": "first"},
            {"role": "assistant", "content": "second"},
            {"role": "user", "content": "third"},
        ]));
        let mut built = 0;
        let counted = chat_turns(&req).inspect(|_| built += 1);
        let input = cap(counted, 3);

        assert_eq!(input.turns, [user("ird")]);
        assert_eq!(built, 1, "only the last turn");
    }
}

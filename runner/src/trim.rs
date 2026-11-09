//! Leaves old file text out of what is sent to the model (issue 21).
//!
//! A model that writes a file puts the whole text in the call's arguments, and every later request
//! carries it again. The text is on disk, so after a while the request only needs to say that the
//! file was written. This module makes that view of the history; the history itself, the database
//! and the log of what was sent are not changed.
//!
//! The rule is a pure function of the history, so a resumed chat gets the same view as a live one:
//! the last [`KEEP`] messages from the user are left whole, and everything before the cutoff is
//! trimmed. The cutoff moves only every [`STEP`] messages, because each move changes the start of
//! the request and the provider's cache has to be filled again from there; in between, the
//! request keeps growing at its end only.

use kernel::{Message, ToolCall};
use serde_json::Value;

use crate::store_prompts::ACTIVE_MARK;

/// Messages from the user at the end of the history that are never trimmed.
pub const KEEP: usize = 6;
/// The cutoff moves in steps of this many messages from the user.
pub const STEP: usize = 4;
/// Text shorter than this (bytes) is left as it is.
pub const MIN_BYTES: usize = 400;

/// The history as the model should see it, or `None` when it would be unchanged.
pub fn trimmed(history: &[Message]) -> Option<Vec<Message>> {
    let users: Vec<usize> = history
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m, Message::User(text) if !text.starts_with(ACTIVE_MARK)))
        .map(|(i, _)| i)
        .collect();
    if users.len() <= KEEP {
        return None;
    }
    let old_messages = (users.len() - KEEP) / STEP * STEP;
    let cutoff = *users.get(old_messages)?;
    let mut changed = false;
    let view: Vec<Message> = history
        .iter()
        .enumerate()
        .map(|(i, message)| match message {
            Message::Assistant {
                content,
                tool_calls,
            } if i < cutoff => {
                let tool_calls: Vec<ToolCall> = tool_calls
                    .iter()
                    .map(|call| match shortened(call) {
                        Some(call) => {
                            changed = true;
                            call
                        }
                        None => call.clone(),
                    })
                    .collect();
                Message::Assistant {
                    content: content.clone(),
                    tool_calls,
                }
            }
            other => other.clone(),
        })
        .collect();
    changed.then_some(view)
}

/// The same call with long file text in its arguments replaced by a note, or `None` when nothing
/// in it is long enough (or it is not a call that carries file text).
fn shortened(call: &ToolCall) -> Option<ToolCall> {
    let fields: &[&str] = match call.name.as_str() {
        "create_file" | "write_file" => &["content"],
        "edit_file" => &["old_string", "new_string"],
        _ => return None,
    };
    let mut args: Value = serde_json::from_str(&call.arguments).ok()?;
    let object = args.as_object_mut()?;
    let mut any = false;
    for field in fields {
        if let Some(Value::String(text)) = object.get(*field)
            && text.len() >= MIN_BYTES
        {
            let note = format!("[{} bytes of text left out to save space]", text.len());
            object.insert((*field).to_owned(), Value::String(note));
            any = true;
        }
    }
    any.then(|| ToolCall {
        id: call.id.clone(),
        name: call.name.clone(),
        arguments: args.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(id: &str, content: &str) -> Message {
        Message::Assistant {
            content: None,
            tool_calls: vec![ToolCall {
                id: id.to_owned(),
                name: "write_file".to_owned(),
                arguments: serde_json::json!({"path": "a.txt", "content": content}).to_string(),
            }],
        }
    }

    /// `n` messages from the user, each followed by a long write and its result.
    fn history(n: usize) -> Vec<Message> {
        let mut h = vec![Message::System("s".to_owned())];
        for i in 0..n {
            h.push(Message::User(format!("message {i}")));
            h.push(write(&format!("c{i}"), &"x".repeat(1000)));
            h.push(Message::Tool {
                tool_call_id: format!("c{i}"),
                content: "wrote 1000 bytes".to_owned(),
            });
            h.push(Message::Assistant {
                content: Some("done".to_owned()),
                tool_calls: Vec::new(),
            });
        }
        h
    }

    fn body_of(message: &Message) -> String {
        let Message::Assistant { tool_calls, .. } = message else {
            panic!("not an assistant message");
        };
        let args: Value = serde_json::from_str(&tool_calls[0].arguments).unwrap();
        args["content"].as_str().unwrap().to_owned()
    }

    #[test]
    fn a_short_chat_is_sent_whole() {
        assert!(trimmed(&history(KEEP)).is_none());
    }

    #[test]
    fn the_cutoff_moves_only_in_steps() {
        // 7 to 9 messages from the user: still nothing; 10 trims the first four.
        for n in KEEP + 1..KEEP + STEP {
            assert!(trimmed(&history(n)).is_none(), "{n} messages");
        }
        let h = history(KEEP + STEP);
        let view = trimmed(&h).expect("trimmed");
        assert_eq!(view.len(), h.len());
        for i in 0..STEP {
            // Message i's write is at 1 + i*4 + 1.
            let body = body_of(&view[2 + i * 4]);
            assert_eq!(body, "[1000 bytes of text left out to save space]");
        }
        assert_eq!(body_of(&view[2 + STEP * 4]), "x".repeat(1000));
        // One more message changes nothing before the cutoff.
        let more = trimmed(&history(KEEP + STEP + 1)).expect("trimmed");
        assert_eq!(more[..view.len()], view[..]);
    }

    #[test]
    fn only_the_assistant_arguments_change_and_the_history_itself_is_untouched() {
        let h = history(KEEP + STEP);
        let before = h.clone();
        let view = trimmed(&h).expect("trimmed");
        assert_eq!(h, before);
        for (a, b) in h.iter().zip(&view) {
            if !matches!(a, Message::Assistant { tool_calls, .. } if !tool_calls.is_empty()) {
                assert_eq!(a, b);
            }
        }
    }

    #[test]
    fn the_instruction_block_is_not_counted_as_a_message() {
        let mut h = history(KEEP + STEP - 1);
        h.insert(
            1,
            Message::User(format!("{ACTIVE_MARK}\n\nsome skill text")),
        );
        assert!(trimmed(&h).is_none());
    }

    #[test]
    fn short_text_and_other_tools_are_left_alone() {
        let mut h = vec![Message::System("s".to_owned())];
        for i in 0..KEEP + STEP {
            h.push(Message::User(format!("m{i}")));
            h.push(write("a", "short"));
            h.push(Message::Assistant {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "b".to_owned(),
                    name: "bash".to_owned(),
                    arguments: serde_json::json!({"command": "x".repeat(1000)}).to_string(),
                }],
            });
        }
        assert!(trimmed(&h).is_none());
    }

    #[test]
    fn edit_text_is_shortened_field_by_field() {
        let call = ToolCall {
            id: "e".to_owned(),
            name: "edit_file".to_owned(),
            arguments: serde_json::json!({
                "path": "a.rs", "old_string": "y".repeat(500), "new_string": "short"
            })
            .to_string(),
        };
        let out = shortened(&call).expect("shortened");
        let args: Value = serde_json::from_str(&out.arguments).unwrap();
        assert_eq!(
            args["old_string"],
            "[500 bytes of text left out to save space]"
        );
        assert_eq!(args["new_string"], "short");
        assert_eq!(args["path"], "a.rs");
    }
}

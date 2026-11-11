//! `assemble()`: the one place a request becomes bytes.
//!
//! Output is OpenAI Chat Completions JSON (decision 06). Field order is the
//! declaration order of the structs below, `Vec`s keep their input order, and
//! nothing here holds a map, so the same inputs always give the same bytes.
//! Compact output, no trailing newline.

// Rust note: how to read this file. The `Wire*` structs below are the exact
// shape of the JSON we send. We don't serialize our own types directly; we
// convert them into these "wire" shapes first (the `wire_*` functions), and then
// the `serde` library turns the wire shapes into JSON text. That keeps the
// neutral types in types.rs free of any one provider's format.

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

// Rust note: `crate::` means "from the root of this crate", i.e. lib.rs, which
// re-exports these four names.
use crate::{Config, Message, ToolCall, ToolSpec};

// Rust note: `#[derive(Serialize)]` generates code that turns this struct into
// JSON, one JSON field per struct field, IN THE ORDER WRITTEN HERE. That fixed
// order is what makes the output bytes stable.
//
// About `<'a>` and `&'a str`: these structs don't own their text, they BORROW
// it from the `Config` and messages passed to `assemble`, which avoids copying.
// `'a` is a "lifetime": a label meaning "these references are valid for as
// long as the thing they point at". The compiler checks that nothing here can
// outlive the data it borrows, which rules out use-after-free bugs.
#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    // Omitted when there are no tools.
    // Rust note: `#[serde(...)]` attributes tweak the JSON. `skip_serializing_if`
    // names a function; if it returns true, the field is left out.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<WireProvider<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    // OpenRouter's automatic prompt caching for Claude models: one marker at the top of the
    // request, and the router keeps the cache breakpoint at the end of the conversation.
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<WireCacheControl>,
    // Always present, always false. No `tool_choice` is ever sent.
    stream: bool,
}

// Rust note: `tag = "role"` makes serde write the variant name as a `"role"`
// field FIRST, e.g. {"role":"user","content":"hi"}. `rename_all = "lowercase"`
// turns the Rust name `User` into `"user"`.
#[derive(Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
enum WireMessage<'a> {
    System {
        content: &'a str,
    },
    User {
        content: &'a str,
    },
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<&'a str>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<WireToolCall<'a>>,
    },
    Tool {
        tool_call_id: &'a str,
        content: &'a str,
    },
}

#[derive(Serialize)]
struct WireToolCall<'a> {
    id: &'a str,
    // Rust note: `type` is a reserved word in Rust, so the field can't be
    // called that. We name it `kind` and tell serde to write it as "type".
    // `&'static str` is a reference that lasts for the whole program, which is
    // what string literals like "function" are.
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireCall<'a>,
}

#[derive(Serialize)]
struct WireCall<'a> {
    name: &'a str,
    // The raw text the model sent, as a JSON string.
    arguments: &'a str,
}

#[derive(Serialize)]
struct WireTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunction<'a>,
}

#[derive(Serialize)]
struct WireFunction<'a> {
    name: &'a str,
    description: &'a str,
    // Verbatim schema text, validated when the `ToolSpec` was built.
    // Rust note: a `RawValue` is JSON text that serde copies into the output
    // exactly as written, instead of parsing and re-formatting it.
    parameters: &'a RawValue,
}

#[derive(Serialize)]
struct WireCacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
}

/// Whether a model id names a Claude model on OpenRouter (`anthropic/...`), which only caches
/// a prompt when asked to.
pub fn asks_for_prompt_cache(model: &str) -> bool {
    model.starts_with("anthropic/")
}

#[derive(Serialize)]
struct WireProvider<'a> {
    // Rust note: `&'a [String]` is a "slice": a borrowed view of a list of
    // Strings (here, the contents of a `Vec<String>`). `<[String]>::is_empty`
    // is how you name the `is_empty` method of a slice.
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    order: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    allow_fallbacks: Option<bool>,
}

// Rust note: `&Message` borrows the message (read only, no copy). The return
// type `WireMessage<'_>` means "a WireMessage that borrows from the input";
// `'_` asks the compiler to work out the lifetime label for us.
fn wire_message(message: &Message) -> WireMessage<'_> {
    // Rust note: `match` checks a value against each pattern in turn. It must
    // cover every variant, so if someone adds a new `Message` variant later,
    // this code stops compiling until they handle it here.
    match message {
        // Pattern `Message::System(content)` also pulls the inner text out into
        // a variable named `content`. `{ content }` is shorthand for
        // `{ content: content }`. A `&String` is accepted where `&str` is
        // wanted, because Rust converts it automatically.
        Message::System(content) => WireMessage::System { content },
        Message::User(content) => WireMessage::User { content },
        Message::Assistant {
            content,
            tool_calls,
        } => WireMessage::Assistant {
            // Rust note: `as_deref()` turns `Option<String>` into
            // `Option<&str>`: same Some/None, but borrowing the text.
            content: content.as_deref(),
            // Rust note: this is an "iterator chain". `.iter()` walks the list,
            // `.map(f)` applies `f` to each element, and `.collect()` gathers
            // the results into a new `Vec`. The target type is inferred from
            // the field being filled in. Order is preserved.
            tool_calls: tool_calls.iter().map(wire_tool_call).collect(),
        },
        Message::Tool {
            tool_call_id,
            content,
        } => WireMessage::Tool {
            tool_call_id,
            content,
        },
    }
}

fn wire_tool_call(call: &ToolCall) -> WireToolCall<'_> {
    WireToolCall {
        id: &call.id,
        kind: "function",
        function: WireCall {
            name: &call.name,
            arguments: &call.arguments,
        },
    }
}

fn wire_tool(spec: &ToolSpec) -> WireTool<'_> {
    WireTool {
        kind: "function",
        function: WireFunction {
            name: &spec.name,
            description: &spec.description,
            // Rust note: `from_str` can fail, so it returns a `Result`.
            // `.expect("...")` means "unwrap the Ok value, or crash the program
            // with this message". That is acceptable here ONLY because
            // `ToolSpec::new` already proved the text is valid JSON, so the
            // failure branch can't happen. It documents the assumption.
            parameters: serde_json::from_str(spec.parameters())
                .expect("ToolSpec::new validated the parameters"),
        },
    }
}

/// Turns a config and a conversation into the exact request bytes.
///
/// Pure: the same `(config, messages)` always gives byte-identical output.
/// Volatile data (dates, working directory) is the caller's to put in the
/// newest user message; nothing is injected here.
//
// Rust note: `&Config` and `&[Message]` are borrows, so the caller keeps its
// data and we only read it. `&[Message]` is a slice, so callers can pass a
// whole `Vec<Message>` (as `&vec`) or part of one. `Vec<u8>` is a list of bytes.
pub fn assemble(config: &Config, messages: &[Message]) -> Vec<u8> {
    let router = &config.router;
    // Rust note: we only want a `provider` object in the JSON if there is
    // something to put in it. `condition.then(|| value)` gives `Some(value)`
    // when the condition is true and `None` otherwise. `|| ...` is a "closure",
    // an anonymous function; here it takes no arguments.
    let provider =
        (!router.provider_order.is_empty() || router.allow_fallbacks.is_some()).then(|| {
            WireProvider {
                order: &router.provider_order,
                allow_fallbacks: router.allow_fallbacks,
            }
        });
    let request = Request {
        model: &config.model,
        // An empty system prompt is no message at all (issue 54: the `bare` profile).
        messages: messages
            .iter()
            .filter(|m| !matches!(m, Message::System(text) if text.trim().is_empty()))
            .map(wire_message)
            .collect(),
        tools: config.tools.iter().map(wire_tool).collect(),
        max_tokens: config.max_tokens,
        provider,
        session_id: router.session_id.as_deref(),
        cache_control: asks_for_prompt_cache(&config.model)
            .then_some(WireCacheControl { kind: "ephemeral" }),
        stream: false,
    };
    // Rust note: no `;` on the last line, so this is the function's return
    // value. `to_vec` writes the JSON as compact bytes. Serializing plain
    // strings can't fail, so `.expect` is fine, same reasoning as above.
    serde_json::to_vec(&request).expect("serializing strings and raw JSON cannot fail")
}

/// One message as the JSON object `assemble` writes into the `messages` array,
/// for storing a conversation. Same bytes as that array element.
pub fn message_to_json(message: &Message) -> String {
    serde_json::to_string(&wire_message(message))
        .expect("serializing strings and raw JSON cannot fail")
}

// Rust note: the mirror image of the `Wire*` types above. These OWN their text
// (`String`, not `&str`) because a parsed message has to outlive the JSON text
// it was read from. `Deserialize` is the reading half of serde. Fields the
// structs don't name (like `"type":"function"`) are ignored while reading.
#[derive(Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
enum StoredMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        #[serde(default)]
        content: Option<String>,
        #[serde(default)]
        tool_calls: Vec<StoredToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Deserialize)]
struct StoredToolCall {
    id: String,
    function: StoredCall,
}

#[derive(Deserialize)]
struct StoredCall {
    name: String,
    arguments: String,
}

/// Reads back what [`message_to_json`] wrote. Lossless: the text, the tool
/// call ids and the raw tool arguments come back exactly, so assembling the
/// result gives the same bytes as the original message did.
pub fn message_from_json(text: &str) -> Result<Message, serde_json::Error> {
    Ok(match serde_json::from_str::<StoredMessage>(text)? {
        StoredMessage::System { content } => Message::System(content),
        StoredMessage::User { content } => Message::User(content),
        StoredMessage::Assistant {
            content,
            tool_calls,
        } => Message::Assistant {
            content,
            tool_calls: tool_calls
                .into_iter()
                .map(|call| ToolCall {
                    id: call.id,
                    name: call.function.name,
                    arguments: call.function.arguments,
                })
                .collect(),
        },
        StoredMessage::Tool {
            tool_call_id,
            content,
        } => Message::Tool {
            tool_call_id,
            content,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples() -> Vec<Message> {
        vec![
            Message::System("be brief".into()),
            Message::User("héllo \"quoted\"\nnew line \\ backslash".into()),
            Message::Assistant {
                content: Some(String::new()),
                tool_calls: vec![],
            },
            Message::Assistant {
                content: None,
                tool_calls: vec![
                    ToolCall {
                        id: "c1".into(),
                        name: "read_file".into(),
                        // Odd spacing and key order must survive untouched.
                        arguments: "{ \"path\" :  \"a.txt\",\"x\":1 }".into(),
                    },
                    ToolCall {
                        id: "c2".into(),
                        name: "bash".into(),
                        arguments: "not even json".into(),
                    },
                ],
            },
            Message::Assistant {
                content: Some("both".into()),
                tool_calls: vec![ToolCall {
                    id: "c3".into(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                }],
            },
            Message::Tool {
                tool_call_id: "c1".into(),
                content: "output\twith tab".into(),
            },
        ]
    }

    #[test]
    fn every_kind_of_message_survives_a_round_trip() {
        for message in samples() {
            let text = message_to_json(&message);
            assert_eq!(message_from_json(&text).unwrap(), message, "{text}");
        }
    }

    #[test]
    fn a_round_tripped_conversation_assembles_to_the_same_bytes() {
        let config = Config {
            model: "m".into(),
            ..Config::default()
        };
        let original = samples();
        let restored: Vec<Message> = original
            .iter()
            .map(|m| message_from_json(&message_to_json(m)).unwrap())
            .collect();
        assert_eq!(assemble(&config, &original), assemble(&config, &restored));
    }

    #[test]
    fn the_stored_json_is_the_element_assemble_writes() {
        let config = Config {
            model: "m".into(),
            ..Config::default()
        };
        let messages = samples();
        let request = String::from_utf8(assemble(&config, &messages)).unwrap();
        for message in &messages {
            assert!(request.contains(&message_to_json(message)));
        }
    }

    #[test]
    fn text_that_is_not_a_message_is_an_error() {
        assert!(message_from_json("not json").is_err());
        assert!(message_from_json(r#"{"role":"robot","content":"x"}"#).is_err());
        assert!(message_from_json(r#"{"role":"user"}"#).is_err());
    }

    #[test]
    fn a_claude_model_on_openrouter_asks_for_prompt_caching_and_no_other_model_does() {
        let ask = |model: &str| {
            let config = Config {
                model: model.into(),
                ..Config::default()
            };
            String::from_utf8(assemble(&config, &[Message::User("hi".into())])).unwrap()
        };
        let claude = ask("anthropic/claude-sonnet-5-5");
        assert!(
            claude.contains(r#""cache_control":{"type":"ephemeral"},"stream":false"#),
            "{claude}"
        );
        for other in [
            "deepseek/deepseek-v4",
            "gpt-5",
            "claude-sonnet-5-5",
            "cc/anthropic/x",
        ] {
            assert!(!ask(other).contains("cache_control"), "{other}");
        }
        // The same model gives the same bytes every time.
        assert_eq!(ask("anthropic/c"), ask("anthropic/c"));
    }
}

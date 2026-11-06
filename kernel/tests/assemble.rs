//! Byte-identity tests for `assemble()`. The golden file is the spec: if it fails,
//! the bytes changed. Do not regenerate the golden to make a test pass without
//! deciding the change is intended (context/coder-only/testing.md).

// Rust note: files in a crate's `tests/` folder are "integration tests". Each
// one is compiled as its own separate program that uses `kernel` the way an
// outside user would, so it can only touch what `kernel` made `pub`. That is
// why we import names from `kernel::` here. Run them with `cargo test`.
use kernel::{Config, Message, RouterOptions, ToolCall, ToolSpec, assemble};

// Rust note: `include_bytes!` is a macro (the `!` marks it) that reads the file
// at COMPILE time and embeds its bytes in the program. The type `&[u8]` is a
// borrowed list of bytes. `const` is a value fixed for the whole program.
const GOLDEN: &[u8] = include_bytes!("golden/conversation.json");

// Rust note: `r#"..."#` is a "raw string": backslashes and double quotes inside
// it are taken literally, so we don't have to escape every `"` in the JSON.
const READ_FILE_SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string","description":"File path"}},"required":["path"]}"#;

// Helper functions: not tests themselves, just shared setup so each test below
// starts from the same known data.
fn read_file_spec() -> ToolSpec {
    // Rust note: `new` returns a `Result`. `.unwrap()` means "give me the Ok
    // value, or crash". Crashing is fine in a test: a crash is a failed test.
    ToolSpec::new("read_file", "Read a UTF-8 text file.", READ_FILE_SCHEMA).unwrap()
}

fn conversation() -> Vec<Message> {
    // Rust note: `vec![a, b, c]` is a macro that builds a `Vec` from the items.
    // `.into()` turns a `&str` literal into the owned `String` the field needs.
    // The shape is a realistic exchange: system prompt, user asks, assistant
    // requests a tool, the tool answers, assistant replies, user says thanks.
    vec![
        Message::System("You are a coding assistant.".into()),
        Message::User("read Cargo.toml".into()),
        Message::Assistant {
            content: None,
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"Cargo.toml"}"#.into(),
            }],
        },
        Message::Tool {
            tool_call_id: "call_1".into(),
            content: "[workspace]\nmembers = [\"kernel\", \"runner\"]".into(),
        },
        Message::Assistant {
            content: Some("The crates are kernel and runner.".into()),
            tool_calls: vec![],
        },
        Message::User("thanks".into()),
    ]
}

fn full_config() -> Config {
    // Every optional setting is filled in, so the golden file exercises all of them.
    Config {
        model: "test/model".into(),
        tools: vec![read_file_spec()],
        max_tokens: Some(1024),
        router: RouterOptions {
            provider_order: vec!["anthropic".into()],
            allow_fallbacks: Some(false),
            session_id: Some("s-1".into()),
        },
    }
}

// Rust note: `#[test]` marks a function as a test. `cargo test` runs each one;
// it passes if the function finishes and fails if anything inside panics
// (crashes). `assert_eq!(left, right)` panics, and prints both sides, if they
// differ. `assert!(condition)` panics if the condition is false.

// The main check: our output must equal the hand-written expected file exactly.
// `&full_config()` borrows a temporary value for the call.
#[test]
fn matches_golden_file_byte_for_byte() {
    assert_eq!(assemble(&full_config(), &conversation()), GOLDEN);
}

// Same input, many runs, always the same bytes.
#[test]
fn same_state_gives_identical_bytes_every_time() {
    let first = assemble(&full_config(), &conversation());
    // Rust note: `0..20` is a range of 20 numbers. `_` means "I don't need the
    // loop counter".
    for _ in 0..20 {
        assert_eq!(assemble(&full_config(), &conversation()), first);
    }
}

// Writing the struct fields in a different order in the source code must not
// change the output. (The output order comes from `assemble`, not from here.)
#[test]
fn struct_literal_field_order_does_not_matter() {
    let reordered = Config {
        router: RouterOptions {
            session_id: Some("s-1".into()),
            allow_fallbacks: Some(false),
            provider_order: vec!["anthropic".into()],
        },
        max_tokens: Some(1024),
        tools: vec![read_file_spec()],
        model: "test/model".into(),
    };
    assert_eq!(
        assemble(&reordered, &conversation()),
        assemble(&full_config(), &conversation())
    );
}

// Tools must come out in the order given, never alphabetically sorted. Order is
// part of the request, so swapping two tools must change the bytes.
#[test]
fn tool_order_is_preserved_not_sorted() {
    let a = ToolSpec::new("a_tool", "A.", "{}").unwrap();
    let b = ToolSpec::new("b_tool", "B.", "{}").unwrap();
    // Rust note: `..Config::default()` is "struct update syntax": take every
    // field I didn't list from a default `Config`. Handy for tests.
    //
    // Rust note on ownership: a value can normally be used in only one place.
    // `vec![a.clone(), b.clone()]` makes copies so `a` and `b` are still ours.
    // On the next line `vec![b, a]` MOVES them into the new list for good,
    // which is allowed because it is their last use.
    let ab = Config {
        model: "m".into(),
        tools: vec![a.clone(), b.clone()],
        ..Config::default()
    };
    let ba = Config {
        model: "m".into(),
        tools: vec![b, a],
        ..Config::default()
    };
    // `&[]` is an empty list of messages. `from_utf8` turns bytes into text
    // (it can fail, hence `unwrap`). `find` gives the position of a substring.
    let ab_text = String::from_utf8(assemble(&ab, &[])).unwrap();
    assert!(ab_text.find("a_tool").unwrap() < ab_text.find("b_tool").unwrap());
    // `assert_ne!` is the opposite of `assert_eq!`: it fails if they are equal.
    assert_ne!(assemble(&ab, &[]), assemble(&ba, &[]));
}

// The model's tool-call arguments are odd-looking on purpose (extra spaces, a
// newline). They must come out character for character, not tidied up.
#[test]
fn tool_call_arguments_survive_verbatim() {
    let arguments = "{ \"b\":1,   \"a\" : [2,\n 3] }";
    let messages = vec![Message::Assistant {
        content: None,
        tool_calls: vec![ToolCall {
            id: "c".into(),
            name: "t".into(),
            arguments: arguments.into(),
        }],
    }];
    let bytes = assemble(&Config::default(), &messages);
    // Rust note: we read the JSON back with `serde_json::Value`, a generic JSON
    // tree, and look up fields with `[...]`. `serde_json` is usable here
    // because it is one of the crate's dependencies. The assertion compares the
    // arguments text we get back with the text we put in.
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        parsed["messages"][0]["tool_calls"][0]["function"]["arguments"],
        arguments
    );
}

// Same idea for a tool's schema: key order and spacing as the author wrote it.
#[test]
fn tool_schema_key_order_and_whitespace_are_kept() {
    let schema = r#"{"z": 1,  "a": {"y":2, "b":3}}"#;
    let config = Config {
        model: "m".into(),
        tools: vec![ToolSpec::new("t", "d", schema).unwrap()],
        ..Config::default()
    };
    let text = String::from_utf8(assemble(&config, &[])).unwrap();
    // Rust note: `format!` builds a String. `{schema}` drops the variable of
    // that name into the text. The extra `"{text}"` after the comma is the
    // failure message, printed only if the assertion fails.
    assert!(text.contains(&format!("\"parameters\":{schema}")), "{text}");
}

// With nothing optional set, the request should be tiny and exact.
#[test]
fn minimal_request_omits_every_optional_field() {
    let config = Config {
        model: "m".into(),
        ..Config::default()
    };
    let bytes = assemble(&config, &[Message::User("hi".into())]);
    // Rust note: `br#"..."#` is a raw string made of bytes instead of text.
    assert_eq!(
        bytes,
        br#"{"model":"m","messages":[{"role":"user","content":"hi"}],"stream":false}"#
    );
}

#[test]
fn no_tool_choice_is_ever_sent() {
    let text = String::from_utf8(assemble(&full_config(), &conversation())).unwrap();
    assert!(!text.contains("tool_choice"));
}

// Invalid schema text must be refused when the tool is created, not later.
#[test]
fn invalid_schema_is_rejected_at_construction() {
    // `is_err()` is true when a `Result` is an `Err`.
    assert!(ToolSpec::new("t", "d", "{not json").is_err());
    assert!(ToolSpec::new("t", "d", "").is_err());
}

// A guard rail: this test reads the kernel's own source files and fails if they
// mention anything that would make the output unstable or break the "pure"
// rule. (That is also why comments in src/ avoid those exact words.)
#[test]
fn kernel_source_stays_pure() {
    // Hash-ordered containers would leak iteration order into the bytes; the
    // other paths would break "no I/O, no clock, no environment" (decision 02).
    let forbidden = [
        "HashMap",
        "HashSet",
        "std::fs",
        "std::env",
        "std::time",
        "std::net",
        "std::process",
    ];
    // Rust note: `env!("CARGO_MANIFEST_DIR")` is the folder holding kernel's
    // Cargo.toml, filled in at compile time. `.join("src")` appends a path part.
    // Tests are allowed to use the file system even though `kernel` itself isn't.
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for entry in std::fs::read_dir(src).unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        for word in forbidden {
            // The text after the comma is the failure message; `{}` and `{word}`
            // are filled in with the path and the offending word.
            assert!(!text.contains(word), "{} contains {word}", path.display());
        }
    }
}

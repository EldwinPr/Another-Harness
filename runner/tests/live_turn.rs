//! One real call through a real router, logged. Ignored by default so that
//! `cargo test` never spends credits (context/coder-only/testing.md). Run it on
//! purpose:
//!
//! ```text
//! cd src && HARNESS_BASE_URL=... HARNESS_MODEL=... \
//!     cargo test -p runner --test live_turn -- --ignored --nocapture
//! ```
//!
//! Add `HARNESS_API_KEY` only if the router needs one. The log goes to `logs/` in the repository root (gitignored).

use std::path::Path;
use std::time::Duration;

use kernel::{Config, Message, assemble};
use runner::provider::HttpProvider;
use runner::turn_log::TurnLog;

fn env(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| panic!("set the environment variable {name}"))
}

#[test]
#[ignore = "calls a real router; needs HARNESS_BASE_URL and HARNESS_MODEL (HARNESS_API_KEY if the router wants one)"]
fn one_real_turn_is_sent_and_logged() {
    let (base_url, model) = (env("HARNESS_BASE_URL"), env("HARNESS_MODEL"));
    // The key is optional: a router that needs none (9router here) is fine.
    let api_key = std::env::var("HARNESS_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let config = Config {
        model,
        max_tokens: Some(16),
        ..Config::default()
    };
    let messages = [
        Message::System("You are a test.".into()),
        Message::User("Reply with the single word: pong".into()),
    ];
    let request = assemble(&config, &messages);

    let provider = HttpProvider::new(&base_url, api_key.as_deref(), Duration::from_secs(60));
    let result = provider.complete(&request);

    // The repository root is two levels above this crate's manifest folder.
    let logs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../logs");
    let mut log = TurnLog::create(&logs).expect("create the log file");
    log.record(&request, &result).expect("write the log line");

    println!("log file: {}", log.path().display());
    match &result {
        Ok(reply) => println!(
            "status {} in {:?}\nmessage: {:?}\nusage: {:?}\nserved_by: {:?}",
            reply.status, reply.elapsed, reply.message, reply.usage, reply.served_by
        ),
        Err(error) => println!("FAILED: {error}\nstatus: {:?}", error.status),
    }

    assert!(result.is_ok(), "the call failed; see the log line");
    let text = std::fs::read_to_string(log.path()).expect("read the log back");
    assert_eq!(text.lines().count(), 1);
    if let Some(key) = &api_key {
        assert!(!text.contains(key), "the key must never be in the log");
    }
}

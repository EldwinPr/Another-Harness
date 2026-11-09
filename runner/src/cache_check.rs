//! The cache measurement behind decision 06: send one byte-identical, padded
//! request several times and see whether the router reports cache reads on the
//! later ones, with the same provider each time.
//!
//! This is a measurement, not part of the agent. The binary `cache_check` wires
//! it to a real router; everything here is testable without one.

use std::time::Duration;

use kernel::{Config, Message, RouterOptions, assemble};

use crate::provider::{Provider, ProviderError, Reply, ServedBy};
use crate::turn_log::TurnLog;

/// One line of filler is about this many bytes (measured, not a token count).
const BYTES_PER_TOKEN_GUESS: usize = 4;

/// Builds the request. `run_id` goes into the user message so that the first
/// call of every run is cold (an earlier run's cache cannot answer it), while
/// all calls inside one run stay byte-identical. `pad_tokens` is a rough size:
/// providers differ in the smallest cacheable prompt, so go well past it.
/// `router` is the provider pinning to test (empty for none).
pub fn build_request(
    model: &str,
    run_id: &str,
    pad_tokens: usize,
    router: &RouterOptions,
) -> Vec<u8> {
    let config = Config {
        model: model.to_owned(),
        max_tokens: Some(16),
        router: router.clone(),
        ..Config::default()
    };
    let messages = [
        Message::System("You are a test. Answer with the single word: ok".into()),
        Message::User(format!(
            "Run {run_id}. The filler below is only padding.\n{}",
            filler(pad_tokens * BYTES_PER_TOKEN_GUESS)
        )),
    ];
    assemble(&config, &messages)
}

/// Fixed, non-repeating-looking text of about `bytes` bytes. The same input
/// always gives the same output.
fn filler(bytes: usize) -> String {
    let mut text = String::with_capacity(bytes + 100);
    let mut i = 0u32;
    while text.len() < bytes {
        // Rust note: `wrapping_mul` is multiplication that wraps around on
        // overflow instead of panicking, a cheap deterministic scramble.
        let mix = i.wrapping_mul(2_654_435_761) >> 8;
        text.push_str(&format!("line {i:04} code {mix:06x} status nominal\n"));
        i += 1;
    }
    text
}

/// What one call told us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    pub prompt_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub served_by: ServedBy,
    pub elapsed: Duration,
    /// Set when the call failed; the numbers are then `None`.
    pub error: Option<String>,
    /// The `usage` object exactly as received.
    pub raw_usage: Option<String>,
}

/// Reads token counts out of a raw `usage` object. Routers spell the cached
/// count two ways; both are accepted.
pub fn parse_usage(raw: &str) -> (Option<u64>, Option<u64>, Option<u64>) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return (None, None, None);
    };
    let details = &v["prompt_tokens_details"];
    let prompt = v["prompt_tokens"].as_u64();
    let cached = details["cached_tokens"]
        .as_u64()
        .or_else(|| v["cached_tokens"].as_u64());
    let written = details["cache_write_tokens"]
        .as_u64()
        .or_else(|| v["cache_creation_input_tokens"].as_u64());
    (prompt, cached, written)
}

fn sample_of(result: &Result<Reply, Box<ProviderError>>) -> Sample {
    match result {
        Ok(reply) => {
            let (prompt_tokens, cached_tokens, cache_write_tokens) =
                reply.usage.as_deref().map(parse_usage).unwrap_or_default();
            Sample {
                prompt_tokens,
                cached_tokens,
                cache_write_tokens,
                served_by: reply.served_by.clone(),
                elapsed: reply.elapsed,
                error: None,
                raw_usage: reply.usage.clone(),
            }
        }
        Err(error) => Sample {
            prompt_tokens: None,
            cached_tokens: None,
            cache_write_tokens: None,
            served_by: error.served_by.clone(),
            elapsed: error.elapsed,
            error: Some(error.message.clone()),
            raw_usage: None,
        },
    }
}

/// Sends the same bytes `times` times, logging every call, pausing `pause`
/// between calls (providers need a moment before a written cache can be read).
pub fn run(
    provider: &dyn Provider,
    request: &[u8],
    times: usize,
    pause: Duration,
    log: &mut TurnLog,
) -> std::io::Result<Vec<Sample>> {
    let mut samples = Vec::with_capacity(times);
    for n in 0..times {
        if n > 0 {
            std::thread::sleep(pause);
        }
        let result = provider.complete(request);
        log.record(request, &result)?;
        samples.push(sample_of(&result));
    }
    Ok(samples)
}

/// The outcome the decision records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The second call read from the cache and the provider did not change.
    /// `provider_known` is false when the router never says who served it, in
    /// which case "the provider stayed constant" has to be checked elsewhere.
    Pass {
        provider_known: bool,
    },
    Fail(String),
    /// A call failed, so nothing can be said.
    Inconclusive(String),
}

pub fn verdict(samples: &[Sample]) -> Verdict {
    if samples.len() < 2 {
        return Verdict::Inconclusive("need at least two calls".into());
    }
    if let Some(e) = samples.iter().find_map(|s| s.error.as_ref()) {
        return Verdict::Inconclusive(format!("a call failed: {e}"));
    }
    let (first, second) = (&samples[0], &samples[1]);
    let Some(cached) = second.cached_tokens else {
        return Verdict::Fail("the second reply has no cached-token count at all".into());
    };
    let same = |a: &Option<String>, b: &Option<String>| a == b;
    let provider_changed = samples.iter().any(|s| {
        !same(&s.served_by.provider, &first.served_by.provider)
            || !same(&s.served_by.account, &first.served_by.account)
    });
    if provider_changed {
        return Verdict::Fail("the provider or account changed between calls".into());
    }
    if cached == 0 {
        return Verdict::Fail("the second call read 0 cached tokens".into());
    }
    let provider_known = first.served_by.provider.is_some() || first.served_by.account.is_some();
    Verdict::Pass { provider_known }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::Scratch;
    use std::collections::VecDeque;

    fn none() -> RouterOptions {
        RouterOptions::default()
    }

    #[test]
    fn pinning_is_part_of_the_request_when_given() {
        let router = RouterOptions {
            provider_order: vec!["deepseek".into()],
            allow_fallbacks: Some(false),
            session_id: Some("s-1".into()),
        };
        let v: serde_json::Value =
            serde_json::from_slice(&build_request("m", "r", 100, &router)).unwrap();
        assert_eq!(v["provider"]["order"][0], "deepseek");
        assert_eq!(v["session_id"], "s-1");
        let plain: serde_json::Value =
            serde_json::from_slice(&build_request("m", "r", 100, &none())).unwrap();
        assert!(plain.get("provider").is_none() && plain.get("session_id").is_none());
    }

    #[test]
    fn the_same_inputs_give_the_same_bytes_and_a_new_run_id_changes_them() {
        let a = build_request("m", "run-1", 500, &none());
        assert_eq!(a, build_request("m", "run-1", 500, &none()));
        assert_ne!(a, build_request("m", "run-2", 500, &none()));
    }

    #[test]
    fn padding_size_follows_the_parameter() {
        let small = build_request("m", "r", 200, &none()).len();
        let big = build_request("m", "r", 2000, &none()).len();
        assert!(big > small * 5, "{small} vs {big}");
        // About four bytes per token: 2000 tokens is roughly 8 KB of filler.
        assert!(big > 8000 && big < 9500, "{big}");
    }

    #[test]
    fn the_filler_does_not_repeat_itself() {
        let text = filler(2000);
        let mut lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        lines.sort_unstable();
        lines.dedup();
        assert_eq!(lines.len(), total);
    }

    #[test]
    fn usage_is_read_in_both_spellings() {
        let nested = r#"{"prompt_tokens":2985,"prompt_tokens_details":{"cached_tokens":960,"cache_write_tokens":5}}"#;
        assert_eq!(parse_usage(nested), (Some(2985), Some(960), Some(5)));
        let flat = r#"{"prompt_tokens":10,"cached_tokens":4,"cache_creation_input_tokens":0}"#;
        assert_eq!(parse_usage(flat), (Some(10), Some(4), Some(0)));
        let null_details = r#"{"prompt_tokens":7,"prompt_tokens_details":null}"#;
        assert_eq!(parse_usage(null_details), (Some(7), None, None));
        assert_eq!(parse_usage("not json"), (None, None, None));
    }

    fn sample(cached: Option<u64>, provider: Option<&str>) -> Sample {
        Sample {
            prompt_tokens: Some(100),
            cached_tokens: cached,
            cache_write_tokens: None,
            served_by: ServedBy {
                router: Some("r".into()),
                provider: provider.map(str::to_owned),
                account: None,
            },
            elapsed: Duration::from_millis(1),
            error: None,
            raw_usage: None,
        }
    }

    #[test]
    fn a_cache_read_on_the_second_call_passes() {
        let v = verdict(&[sample(Some(0), Some("p")), sample(Some(64), Some("p"))]);
        assert_eq!(
            v,
            Verdict::Pass {
                provider_known: true
            }
        );
    }

    #[test]
    fn a_pass_without_a_named_provider_says_so() {
        let v = verdict(&[sample(Some(0), None), sample(Some(64), None)]);
        assert_eq!(
            v,
            Verdict::Pass {
                provider_known: false
            }
        );
    }

    #[test]
    fn zero_cached_tokens_on_the_second_call_fails() {
        let v = verdict(&[sample(Some(0), Some("p")), sample(Some(0), Some("p"))]);
        assert!(matches!(v, Verdict::Fail(why) if why.contains("0 cached")));
    }

    #[test]
    fn a_missing_cached_count_fails() {
        let v = verdict(&[sample(None, Some("p")), sample(None, Some("p"))]);
        assert!(matches!(v, Verdict::Fail(why) if why.contains("no cached-token count")));
    }

    #[test]
    fn a_changed_provider_fails_even_with_cache_reads() {
        let v = verdict(&[sample(Some(0), Some("a")), sample(Some(64), Some("b"))]);
        assert!(matches!(v, Verdict::Fail(why) if why.contains("changed")));
    }

    #[test]
    fn a_failed_call_is_inconclusive_not_a_fail() {
        let mut bad = sample(None, None);
        bad.error = Some("Connection refused".into());
        assert!(matches!(
            verdict(&[sample(Some(0), None), bad]),
            Verdict::Inconclusive(_)
        ));
        assert!(matches!(
            verdict(&[sample(Some(0), None)]),
            Verdict::Inconclusive(_)
        ));
    }

    /// Answers from a list and remembers the bytes it was sent.
    struct Script {
        usages: std::sync::Mutex<VecDeque<&'static str>>,
        seen: std::sync::Mutex<Vec<Vec<u8>>>,
    }
    impl Provider for Script {
        fn complete(&self, request: &[u8]) -> Result<Reply, Box<ProviderError>> {
            self.seen.lock().unwrap().push(request.to_vec());
            let usage = self.usages.lock().unwrap().pop_front().unwrap();
            Ok(Reply {
                status: 200,
                raw_body: "{}".into(),
                elapsed: Duration::from_millis(1),
                served_by: ServedBy::default(),
                message: Message::Assistant {
                    content: Some("ok".into()),
                    tool_calls: vec![],
                },
                finish_reason: Some("stop".into()),
                response_model: Some("m".into()),
                usage: Some(usage.to_owned()),
            })
        }
    }

    #[test]
    fn run_sends_identical_bytes_every_time_and_logs_each_call() {
        let dir = Scratch::new("cache-run");
        let script = Script {
            usages: std::sync::Mutex::new(
                [
                    r#"{"prompt_tokens":900,"prompt_tokens_details":{"cached_tokens":0}}"#,
                    r#"{"prompt_tokens":900,"prompt_tokens_details":{"cached_tokens":768}}"#,
                    r#"{"prompt_tokens":900,"prompt_tokens_details":{"cached_tokens":768}}"#,
                ]
                .into(),
            ),
            seen: std::sync::Mutex::new(vec![]),
        };
        let request = build_request("m", "r", 300, &none());
        let mut log = TurnLog::create(&dir.path().join("logs")).unwrap();
        let samples = run(&script, &request, 3, Duration::ZERO, &mut log).unwrap();

        assert!(
            script
                .seen
                .lock()
                .unwrap()
                .iter()
                .all(|sent| *sent == request)
        );
        let cached: Vec<_> = samples.iter().map(|s| s.cached_tokens).collect();
        assert_eq!(cached, [Some(0), Some(768), Some(768)]);
        assert_eq!(
            verdict(&samples),
            Verdict::Pass {
                provider_known: false
            }
        );
        let lines = std::fs::read_to_string(log.path()).unwrap();
        assert_eq!(lines.lines().count(), 3);
        // The raw usage of both requests is kept for the decision's evidence.
        assert!(lines.contains(r#""cached_tokens":768"#));
    }
}

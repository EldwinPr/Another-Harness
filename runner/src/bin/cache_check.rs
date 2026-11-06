//! Throwaway measurement for decision 06 (issue 5): send one padded request,
//! byte-identical, several times through the configured router and report
//! whether the later calls read from the cache.
//!
//! ```text
//! cd src && HARNESS_BASE_URL=... HARNESS_MODEL=... cargo run -p runner --bin cache_check
//! ```
//!
//! Same settings as the chat binary (`HARNESS_API_KEY` optional, `HARNESS_LOG_DIR`
//! for the log). Extra knobs, all optional:
//!
//! ```text
//! CACHE_CHECK_PAD_TOKENS   rough size of the padding (default 4000)
//! CACHE_CHECK_CALLS        how many identical calls (default 3)
//! CACHE_CHECK_PAUSE_SECS   wait between calls (default 3)
//! ```
//!
//! Exit code: 0 pass, 1 fail or inconclusive, 2 missing settings.
//! It spends a few thousand tokens per call on the router you point it at.

use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use runner::cache_check::{Verdict, build_request, run, verdict};
use runner::config::Settings;
use runner::provider::HttpProvider;
use runner::turn_log::{TurnLog, utc_stamp};

fn number(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn show(value: Option<u64>) -> String {
    value.map_or("-".to_owned(), |v| v.to_string())
}

fn main() -> ExitCode {
    let settings = match Settings::from_env() {
        Ok(s) => s,
        Err(why) => {
            eprintln!("{why}");
            return ExitCode::from(2);
        }
    };
    let pad = number("CACHE_CHECK_PAD_TOKENS", 4000) as usize;
    let calls = number("CACHE_CHECK_CALLS", 3) as usize;
    let pause = Duration::from_secs(number("CACHE_CHECK_PAUSE_SECS", 3));

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let request = build_request(&settings.model, &utc_stamp(now), pad, &settings.router);
    let provider = HttpProvider::new(
        &settings.base_url,
        settings.api_key.as_deref(),
        Duration::from_secs(120),
    );
    let mut log = match TurnLog::create(&settings.log_dir) {
        Ok(log) => log,
        Err(e) => {
            eprintln!("cannot create the log: {e}");
            return ExitCode::FAILURE;
        }
    };

    if settings.router != Default::default() {
        println!("pinning: {:?}", settings.router);
    }
    println!(
        "model {} via {}\n{calls} identical calls of {} bytes (about {pad} tokens of padding), {}s apart\nlog: {}\n",
        settings.model,
        settings.base_url,
        request.len(),
        pause.as_secs(),
        log.path().display()
    );
    let samples = match run(&provider, &request, calls, pause, &mut log) {
        Ok(samples) => samples,
        Err(e) => {
            eprintln!("cannot write the log: {e}");
            return ExitCode::FAILURE;
        }
    };

    println!("call  prompt  cached  written  seconds  provider/account");
    for (n, s) in samples.iter().enumerate() {
        println!(
            "{:>4}  {:>6}  {:>6}  {:>7}  {:>7.1}  {}",
            n + 1,
            show(s.prompt_tokens),
            show(s.cached_tokens),
            show(s.cache_write_tokens),
            s.elapsed.as_secs_f32(),
            s.error.clone().unwrap_or_else(|| format!(
                "{}/{}",
                s.served_by.provider.as_deref().unwrap_or("not in reply"),
                s.served_by.account.as_deref().unwrap_or("not in reply")
            )),
        );
    }
    println!();
    match verdict(&samples) {
        Verdict::Pass { provider_known } => {
            println!("PASS: the second call read from the cache.");
            if !provider_known {
                println!(
                    "Not proven: this router never names the provider or account, so check the\nrouter's own usage record that all calls went to the same one."
                );
            }
            ExitCode::SUCCESS
        }
        Verdict::Fail(why) => {
            println!("FAIL: {why}");
            ExitCode::FAILURE
        }
        Verdict::Inconclusive(why) => {
            println!("INCONCLUSIVE: {why}");
            ExitCode::FAILURE
        }
    }
}

//! Starting the binary with `--decider` and `--eval-decider` (issue 48): what stops startup, what
//! is printed where, and that a key never appears.
//!
//! Every test works on a database in its own folder under the system temp folder, passed through
//! `HARNESS_LOG_DIR`; none opens the real `harness.db`, and nothing here reaches the network (the
//! cases that start a server close its input at once, so no submission is judged).

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use runner::config::ApiStyle;
use runner::providers::ProviderDef;
use runner::store::Store;

const KEY: &str = "SECRETKEY-0123456789";

struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "another-harness-decider-start-{}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn db(&self) -> PathBuf {
        self.0.join("harness.db")
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn def(name: &str, base_url: &str, key: Option<&str>) -> ProviderDef {
    ProviderDef {
        name: name.to_owned(),
        base_url: base_url.to_owned(),
        model: "default-model".to_owned(),
        api_key: key.map(str::to_owned),
        router: kernel::RouterOptions::default(),
        api: ApiStyle::OpenAi,
    }
}

/// A database with two saved providers, one with a key and one without.
fn seeded(tag: &str) -> Dir {
    let dir = Dir::new(tag);
    let mut store = Store::open(&dir.db()).unwrap();
    for provider in [
        def("good", "https://router.example/v1", Some(KEY)),
        def("nokey", "https://router.example/v1", None),
    ] {
        store.add_provider(&provider).unwrap();
    }
    drop(store);
    dir
}

fn run(dir: &Dir, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_runner"));
    command
        .args(args)
        .stdin(Stdio::null())
        .env("HARNESS_LOG_DIR", &dir.0);
    for name in ["HARNESS_BASE_URL", "HARNESS_MODEL"] {
        command.env_remove(name);
    }
    command.output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn an_unknown_provider_stops_the_server_before_it_starts_with_the_reason_and_no_key() {
    let dir = seeded("unknown");
    let before = std::fs::read(dir.db()).unwrap();
    let out = run(&dir, &["--mcp-serve", "--decider", "llm:typo"]);
    assert_eq!(out.status.code(), Some(1));
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert!(
        stdout.is_empty(),
        "stdout is for the protocol alone: {stdout}"
    );
    assert!(
        stderr.contains("--decider") && stderr.contains("no saved provider 'typo'"),
        "{stderr}"
    );
    assert!(
        stderr.contains("good") && stderr.contains("nokey"),
        "{stderr}"
    );
    assert!(!stderr.contains(KEY), "{stderr}");
    assert_eq!(
        std::fs::read(dir.db()).unwrap(),
        before,
        "the database is untouched"
    );
}

#[test]
fn a_provider_without_a_key_stops_the_server_with_the_reason() {
    let dir = seeded("nokey");
    let out = run(&dir, &["--mcp-serve", "--decider", "llm:nokey"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stdout).is_empty());
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("no API key") && stderr.contains("nokey"),
        "{stderr}"
    );
}

#[test]
fn llm_with_no_provider_used_last_stops_the_server() {
    let dir = seeded("nolast");
    let out = run(&dir, &["--mcp-serve", "--decider", "llm"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("no provider was used last"));
}

#[test]
fn bad_combinations_and_bad_specs_are_refused_as_options() {
    let dir = seeded("options");
    for args in [
        &["--mcp-serve", "--decider", "jev:"][..],
        &["--decider", "jev"],
        &["--mcp-serve", "--read-only", "--decider", "jev"],
        &["--eval-decider", "jev:p:"],
        &["--decider", "llm"],
        &["--mcp-serve", "--read-only", "--decider", "llm"],
        &["--eval-decider", "none"],
    ] {
        let out = run(&dir, args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(text(&out.stdout).is_empty());
        assert!(!text(&out.stderr).is_empty());
    }
}

#[test]
fn a_good_provider_starts_the_server_and_says_so_on_stderr_only() {
    let dir = seeded("good");
    let out = run(
        &dir,
        &["--mcp-serve", "--decider", "llm:good:vendor/model:free"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stdout).is_empty());
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("decider: llm:good:vendor/model:free"),
        "{stderr}"
    );
    assert!(!stderr.contains(KEY), "{stderr}");
}

#[test]
fn without_the_flag_or_with_none_no_decider_starts() {
    let dir = seeded("default");
    for args in [&["--mcp-serve"][..], &["--mcp-serve", "--decider", "none"]] {
        let out = run(&dir, args);
        assert_eq!(out.status.code(), Some(0), "{args:?}");
        assert!(text(&out.stdout).is_empty());
        assert!(
            !text(&out.stderr).contains("decider"),
            "{}",
            text(&out.stderr)
        );
    }
}

#[test]
fn eval_decider_with_an_unknown_provider_stops_and_writes_nothing() {
    let dir = seeded("eval");
    let before = std::fs::read(dir.db()).unwrap();
    let out = run(&dir, &["--eval-decider", "llm:typo"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stdout).is_empty());
    assert!(text(&out.stderr).contains("no saved provider 'typo'"));
    assert!(!text(&out.stderr).contains(KEY));
    assert_eq!(std::fs::read(dir.db()).unwrap(), before);
    assert!(!dir.0.join("logs").exists());
}

// ----- Jev (issue 49) -----

/// A database with an OpenRouter provider (with a key), one with no key, and one that is not
/// OpenRouter. Nothing here is called: the cases close the server's input at once.
fn jev_seeded(tag: &str) -> Dir {
    let dir = Dir::new(tag);
    let mut store = Store::open(&dir.db()).unwrap();
    for provider in [
        def("or", "https://openrouter.ai/api/v1", Some(KEY)),
        def("or-nokey", "https://openrouter.ai/api/v1", None),
        def("other", "https://router.example/v1", Some(KEY)),
    ] {
        store.add_provider(&provider).unwrap();
    }
    drop(store);
    dir
}

#[test]
fn jev_specs_parse_as_options_and_start_the_server_naming_the_pinned_model() {
    let dir = jev_seeded("jev-good");
    for (spec, name) in [
        ("jev:or", "jev:typesafe/jev-1.14-20261014"),
        ("jev:or:typesafe/jev-1.15", "jev:typesafe/jev-1.15"),
    ] {
        let out = run(&dir, &["--mcp-serve", "--decider", spec]);
        assert_eq!(out.status.code(), Some(0), "{spec}: {}", text(&out.stderr));
        assert!(text(&out.stdout).is_empty());
        let stderr = text(&out.stderr);
        assert!(stderr.contains(&format!("decider: {name} ")), "{stderr}");
        assert!(!stderr.contains(KEY), "{stderr}");
    }
    // `jev` alone needs a provider used last.
    let out = run(&dir, &["--mcp-serve", "--decider", "jev"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("no provider was used last"));
}

#[test]
fn jev_with_a_provider_that_is_not_openrouter_stops_startup_with_the_reason_and_no_key() {
    let dir = jev_seeded("jev-notor");
    let before = std::fs::read(dir.db()).unwrap();
    let out = run(&dir, &["--mcp-serve", "--decider", "jev:other"]);
    assert_eq!(out.status.code(), Some(1));
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert!(stdout.is_empty(), "{stdout}");
    assert!(
        stderr.contains("--decider") && stderr.contains("jev needs an OpenRouter provider"),
        "{stderr}"
    );
    assert!(!stderr.contains(KEY), "{stderr}");
    assert_eq!(std::fs::read(dir.db()).unwrap(), before);
}

#[test]
fn jev_with_an_unknown_provider_or_no_key_stops_startup_with_the_reason() {
    let dir = jev_seeded("jev-bad");
    let out = run(&dir, &["--mcp-serve", "--decider", "jev:typo"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("no saved provider 'typo'") && stderr.contains("or-nokey"),
        "{stderr}"
    );
    assert!(!stderr.contains(KEY));
    let out = run(&dir, &["--mcp-serve", "--decider", "jev:or-nokey"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("no API key"));
    assert!(text(&out.stdout).is_empty());
}

#[test]
fn eval_decider_jev_with_a_bad_provider_stops_before_any_call_and_writes_nothing() {
    let dir = jev_seeded("jev-eval");
    let before = std::fs::read(dir.db()).unwrap();
    for (spec, reason) in [
        ("jev:other", "jev needs an OpenRouter provider"),
        ("jev:typo", "no saved provider 'typo'"),
        ("jev:or-nokey", "no API key"),
        ("jev", "no provider was used last"),
    ] {
        let out = run(&dir, &["--eval-decider", spec]);
        assert_eq!(out.status.code(), Some(1), "{spec}");
        assert!(text(&out.stdout).is_empty(), "{spec}");
        let stderr = text(&out.stderr);
        assert!(stderr.contains(reason), "{spec}: {stderr}");
        assert!(!stderr.contains(KEY));
    }
    assert_eq!(std::fs::read(dir.db()).unwrap(), before);
    assert!(!dir.0.join("logs").exists());
}

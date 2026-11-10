//! The environment a `bash` command is started with: the program's own, minus what looks like a
//! secret.
//!
//! A command inherits everything the program has, so `env` or `set` shows the provider's API key,
//! and that output goes into the chat, the database and the turn logs. The key is removed from the
//! command's environment instead: a program that needs it can still be given it by name, but a
//! plain `env` cannot print it. The program itself keeps its own environment untouched.

use std::process::Command;

/// Names holding a secret: `HARNESS_API_KEY`, `OPENAI_KEY`, `GITHUB_TOKEN`, `DB_PASSWORD`,
/// `AWS_SECRET_ACCESS_KEY`, `*_CREDENTIALS`. A name ending in `_TOKENIZERS_PARALLELISM` or
/// `_TOKENS` is not a secret, so it is kept.
pub fn is_secret_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if upper.ends_with("_TOKENS") || upper.ends_with("_TOKENIZERS_PARALLELISM") {
        return false;
    }
    [
        "KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "CREDENTIALS",
    ]
    .iter()
    .any(|word| {
        upper == *word
            || upper.ends_with(&format!("_{word}"))
            || upper.starts_with(&format!("{word}_"))
    }) || upper.contains("SECRET_ACCESS_KEY")
        || upper.contains("SESSION_TOKEN")
}

/// The variables of this program that are removed from a command's environment, sorted, so the
/// program can say which ones and a test can check it.
pub fn removed_names() -> Vec<String> {
    let mut names: Vec<String> = std::env::vars()
        .map(|(name, _)| name)
        .filter(|name| is_secret_name(name))
        .collect();
    names.sort();
    names
}

/// Sets up `command` with every secret-looking variable removed.
pub fn scrub(command: &mut Command) -> &mut Command {
    for name in removed_names() {
        command.env_remove(name);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_is_recognised_by_its_name() {
        for name in [
            "HARNESS_API_KEY",
            "OPENAI_KEY",
            "GITHUB_TOKEN",
            "DB_PASSWORD",
            "AWS_SECRET_ACCESS_KEY",
            "my_credentials",
            "SECRET",
        ] {
            assert!(is_secret_name(name), "{name}");
        }
        for name in [
            "PATH",
            "HOME",
            "HARNESS_MODEL",
            "TOKENIZERS_PARALLELISM",
            "MAX_TOKENS",
            "HARNESS_PROVIDER_ORDER",
        ] {
            assert!(!is_secret_name(name), "{name}");
        }
    }

    #[test]
    fn the_scrubbed_environment_of_a_command_holds_no_secret() {
        // Edition 2024 makes setting a variable unsafe: this test is the only thread that runs.
        unsafe { std::env::set_var("PROJECTINATOR_TEST_API_KEY", "sk-1234") };
        assert!(removed_names().contains(&"PROJECTINATOR_TEST_API_KEY".to_owned()));
        let mut command = Command::new("bash");
        command.arg("-c").arg("env");
        let out = scrub(&mut command).output().expect("bash runs");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(!text.contains("sk-1234"), "{text}");
        assert!(!text.contains("PROJECTINATOR_TEST_API_KEY"), "{text}");
        // Everything else is still there, so ordinary commands keep working.
        assert!(text.contains("PATH="), "{text}");
        unsafe { std::env::remove_var("PROJECTINATOR_TEST_API_KEY") };
    }
}

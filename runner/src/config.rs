//! Settings for the binary, read from environment variables.
//!
//! `from_lookup` takes a function instead of reading the environment itself, so
//! tests can pass a fake environment without touching the real one.

use std::path::PathBuf;

use kernel::RouterOptions;

pub const ENV_BASE_URL: &str = "HARNESS_BASE_URL";
pub const ENV_API_KEY: &str = "HARNESS_API_KEY";
pub const ENV_MODEL: &str = "HARNESS_MODEL";
pub const ENV_LOG_DIR: &str = "HARNESS_LOG_DIR";
/// Comma-separated provider names, in the order to try (OpenRouter `provider.order`).
pub const ENV_PROVIDER_ORDER: &str = "HARNESS_PROVIDER_ORDER";
/// `true` or `false` (OpenRouter `provider.allow_fallbacks`). Unset sends nothing.
pub const ENV_ALLOW_FALLBACKS: &str = "HARNESS_ALLOW_FALLBACKS";
/// A fixed key so a router keeps routing the conversation to the same provider.
pub const ENV_SESSION_ID: &str = "HARNESS_SESSION_ID";

/// Where logs go when `HARNESS_LOG_DIR` is not set: `logs` in the directory the
/// binary was started from.
pub const DEFAULT_LOG_DIR: &str = "logs";

/// Where logs and the database go: `HARNESS_LOG_DIR`, else [`DEFAULT_LOG_DIR`].
pub fn log_dir_from_lookup(get: impl Fn(&str) -> Option<String>) -> PathBuf {
    get(ENV_LOG_DIR)
        .filter(|v| !v.trim().is_empty())
        .map_or_else(|| PathBuf::from(DEFAULT_LOG_DIR), PathBuf::from)
}

/// Whether the environment names a provider (a base URL or a model). When it does not, the chat
/// starts on the provider used last time, or on none, and one is chosen inside the chat.
pub fn names_a_provider(get: impl Fn(&str) -> Option<String>) -> bool {
    [ENV_BASE_URL, ENV_MODEL]
        .iter()
        .any(|name| get(name).is_some_and(|v| !v.trim().is_empty()))
}

/// What was typed after the program name.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Continue the most recent saved chat instead of starting a new one.
    pub resume: bool,
}

impl Options {
    /// Reads the command-line arguments (without the program name).
    pub fn from_args(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut options = Self::default();
        for arg in args {
            match arg.as_str() {
                "--resume" => options.resume = true,
                other => {
                    return Err(format!(
                        "unknown option '{other}': the only option is --resume"
                    ));
                }
            }
        }
        Ok(options)
    }
}

pub struct Settings {
    pub base_url: String,
    pub model: String,
    /// Only for routers that want one. Never printed or logged.
    pub api_key: Option<String>,
    pub log_dir: PathBuf,
    /// Provider pinning for routers that support it. All empty by default, in
    /// which case none of it appears in the request.
    pub router: RouterOptions,
}

// Rust note: no `Debug` derive on purpose. `{:?}` would print the API key.

impl Settings {
    /// Builds settings from a lookup function. Blank values count as unset.
    //
    // Rust note: `impl Fn(&str) -> Option<String>` accepts any function or closure
    // with that shape: the real one wraps `std::env::var`, a test passes a closure.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let get = |name: &str| get(name).filter(|v| !v.trim().is_empty());
        let require =
            |name: &str| get(name).ok_or_else(|| format!("set the environment variable {name}"));
        let allow_fallbacks = match get(ENV_ALLOW_FALLBACKS).as_deref().map(str::trim) {
            None => None,
            Some("true") => Some(true),
            Some("false") => Some(false),
            Some(other) => {
                return Err(format!(
                    "{ENV_ALLOW_FALLBACKS} must be true or false, not '{other}'"
                ));
            }
        };
        let provider_order = get(ENV_PROVIDER_ORDER)
            .map(|list| {
                list.split(',')
                    .map(|name| name.trim().to_owned())
                    .filter(|name| !name.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            base_url: require(ENV_BASE_URL)?,
            model: require(ENV_MODEL)?,
            api_key: get(ENV_API_KEY),
            log_dir: get(ENV_LOG_DIR)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(DEFAULT_LOG_DIR)),
            router: RouterOptions {
                provider_order,
                allow_fallbacks,
                session_id: get(ENV_SESSION_ID),
            },
        })
    }

    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn base_url_and_model_are_enough() {
        let s = Settings::from_lookup(env(&[
            (ENV_BASE_URL, "http://localhost:20128/v1"),
            (ENV_MODEL, "m"),
        ]))
        .unwrap();
        assert_eq!(s.base_url, "http://localhost:20128/v1");
        assert_eq!(s.model, "m");
        assert!(s.api_key.is_none());
        assert_eq!(s.log_dir, PathBuf::from("logs"));
    }

    #[test]
    fn the_key_and_log_dir_are_read_when_present() {
        let s = Settings::from_lookup(env(&[
            (ENV_BASE_URL, "u"),
            (ENV_MODEL, "m"),
            (ENV_API_KEY, "secret"),
            (ENV_LOG_DIR, "/tmp/x"),
        ]))
        .unwrap();
        assert_eq!(s.api_key.as_deref(), Some("secret"));
        assert_eq!(s.log_dir, PathBuf::from("/tmp/x"));
    }

    #[test]
    fn a_missing_or_blank_required_value_names_the_variable() {
        let missing = Settings::from_lookup(env(&[(ENV_MODEL, "m")]))
            .err()
            .unwrap();
        assert!(missing.contains(ENV_BASE_URL));
        let blank = Settings::from_lookup(env(&[(ENV_BASE_URL, "u"), (ENV_MODEL, "  ")]))
            .err()
            .unwrap();
        assert!(blank.contains(ENV_MODEL));
    }

    #[test]
    fn pinning_is_off_unless_asked_for() {
        let s = Settings::from_lookup(env(&[(ENV_BASE_URL, "u"), (ENV_MODEL, "m")])).unwrap();
        assert_eq!(s.router, RouterOptions::default());
    }

    #[test]
    fn pinning_is_read_from_the_environment() {
        let s = Settings::from_lookup(env(&[
            (ENV_BASE_URL, "u"),
            (ENV_MODEL, "m"),
            (ENV_PROVIDER_ORDER, " deepseek , fireworks ,"),
            (ENV_ALLOW_FALLBACKS, "false"),
            (ENV_SESSION_ID, "harness-1"),
        ]))
        .unwrap();
        assert_eq!(s.router.provider_order, ["deepseek", "fireworks"]);
        assert_eq!(s.router.allow_fallbacks, Some(false));
        assert_eq!(s.router.session_id.as_deref(), Some("harness-1"));
    }

    #[test]
    fn a_bad_allow_fallbacks_value_is_rejected_by_name() {
        let why = Settings::from_lookup(env(&[
            (ENV_BASE_URL, "u"),
            (ENV_MODEL, "m"),
            (ENV_ALLOW_FALLBACKS, "maybe"),
        ]))
        .err()
        .unwrap();
        assert!(why.contains(ENV_ALLOW_FALLBACKS) && why.contains("maybe"));
    }

    fn args(list: &[&str]) -> Result<Options, String> {
        Options::from_args(list.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn no_arguments_means_a_new_chat() {
        assert_eq!(args(&[]).unwrap(), Options { resume: false });
    }

    #[test]
    fn resume_is_the_one_option() {
        assert_eq!(args(&["--resume"]).unwrap(), Options { resume: true });
        assert!(args(&["--resume", "--resume"]).unwrap().resume);
    }

    #[test]
    fn anything_else_is_rejected_by_name() {
        let why = args(&["--resum"]).unwrap_err();
        assert!(why.contains("--resum") && why.contains("--resume"), "{why}");
        assert!(args(&["--resume", "extra"]).is_err());
    }

    #[test]
    fn a_blank_key_counts_as_no_key() {
        let s = Settings::from_lookup(env(&[
            (ENV_BASE_URL, "u"),
            (ENV_MODEL, "m"),
            (ENV_API_KEY, ""),
        ]))
        .unwrap();
        assert!(s.api_key.is_none());
    }

    #[test]
    fn the_log_dir_is_read_without_needing_a_provider() {
        assert_eq!(log_dir_from_lookup(env(&[])), PathBuf::from("logs"));
        assert_eq!(
            log_dir_from_lookup(env(&[(ENV_LOG_DIR, "/data/x")])),
            PathBuf::from("/data/x")
        );
        assert_eq!(
            log_dir_from_lookup(env(&[(ENV_LOG_DIR, "  ")])),
            PathBuf::from("logs")
        );
    }

    #[test]
    fn a_provider_is_named_when_either_variable_is_set() {
        assert!(!names_a_provider(env(&[])));
        assert!(!names_a_provider(env(&[(ENV_MODEL, " ")])));
        assert!(names_a_provider(env(&[(ENV_BASE_URL, "u")])));
        assert!(names_a_provider(env(&[(ENV_MODEL, "m")])));
    }
}

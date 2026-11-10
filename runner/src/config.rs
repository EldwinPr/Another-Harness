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
/// `openai` (chat completions, the default) or `anthropic` (Anthropic's Messages API).
pub const ENV_API: &str = "HARNESS_API";

/// Which API a provider speaks. Everything else in the program is the same for both: the request
/// is built in the OpenAI shape and the Anthropic provider translates it on the way out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ApiStyle {
    /// `POST {base_url}/chat/completions`: OpenAI, OpenRouter, 9router and most others.
    #[default]
    OpenAi,
    /// `POST {base_url}/messages`: Anthropic's own API (`https://api.anthropic.com/v1`).
    Anthropic,
}

impl ApiStyle {
    pub const ALL: [ApiStyle; 2] = [ApiStyle::OpenAi, ApiStyle::Anthropic];

    pub fn as_str(self) -> &'static str {
        match self {
            ApiStyle::OpenAi => "openai",
            ApiStyle::Anthropic => "anthropic",
        }
    }

    /// What each one is, for a menu.
    pub fn describe(self) -> &'static str {
        match self {
            ApiStyle::OpenAi => {
                "chat completions (/chat/completions): OpenRouter, OpenAI, most routers"
            }
            ApiStyle::Anthropic => "Anthropic Messages (/messages): api.anthropic.com",
        }
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        match text.trim().to_ascii_lowercase().as_str() {
            "openai" => Ok(ApiStyle::OpenAi),
            "anthropic" => Ok(ApiStyle::Anthropic),
            other => Err(format!("the API is openai or anthropic, not '{other}'")),
        }
    }
}

/// Where logs go when `HARNESS_LOG_DIR` is not set: `logs` in the directory the
/// binary was started from.
pub const DEFAULT_LOG_DIR: &str = "logs";

/// The folder the program keeps its own data in, one per user and the same from any project:
/// `harness.db` (chats, providers with their keys, prompts, profiles, the glossary and the context
/// entries) and the turn logs. A repository is never written to for this (issue 36).
pub const APP_DIR: &str = "aharness";

/// Where logs and the database go: `HARNESS_LOG_DIR` when set, else the user's data folder
/// (`$XDG_DATA_HOME/aharness` or `~/.local/share/aharness` on Linux, `~/Library/Application
/// Support/aharness` on macOS, `%APPDATA%\aharness` on Windows). It is an error, not a guess, when
/// no such folder can be found: the program would otherwise write into whatever repository it was
/// started in.
pub fn log_dir_from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<PathBuf, String> {
    let get = |name: &str| get(name).filter(|v| !v.trim().is_empty());
    if let Some(dir) = get(ENV_LOG_DIR) {
        return Ok(PathBuf::from(dir));
    }
    let base = if cfg!(windows) {
        get("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        get("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        get("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| get("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    };
    base.map(|base| base.join(APP_DIR)).ok_or_else(|| {
        format!("cannot find the folder for the program's data: set {ENV_LOG_DIR} to one")
    })
}

/// Environment variables that were renamed (issue 30), each with its new name.
const RENAMED: [(&str, &str); 2] = [
    ("HARNESS_PROFILE", "HARNESS_PROVIDER"),
    ("HARNESS_PROFILE_DIR", "HARNESS_PROVIDER_DIR"),
];

/// A message when the environment still sets a variable under its old name. It is refused rather
/// than ignored: the chat would otherwise start on a different provider than the one asked for.
pub fn renamed_variable(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    RENAMED
        .iter()
        .find(|(old, _)| get(old).is_some())
        .map(|(old, new)| format!("{old} is now called {new}: set it under the new name"))
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
    /// Serve the web page (view and edit) on this port instead of starting a chat (issue 28).
    pub serve: Option<u16>,
    /// MCP servers to add before the chat starts (issue 44), in the order given.
    pub mcp: Vec<McpRequest>,
}

/// One `--mcp` or `--mcp-add` on the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpRequest {
    /// `--mcp NAME`: a ready-made toolset (see `mcp_toolsets.rs`).
    Toolset(String),
    /// `--mcp-add NAME COMMAND [ARGS] [--env N=v] [--timeout S]`: the words after the option, as
    /// `/mcp add` reads them.
    Add(String),
}

/// The options that end the words of `--mcp-add`.
const OWN_OPTIONS: [&str; 4] = ["--resume", "--serve", "--mcp", "--mcp-add"];

/// The port `--serve` uses when none is given.
pub const DEFAULT_WEB_PORT: u16 = 8787;

impl Options {
    /// Reads the command-line arguments (without the program name).
    pub fn from_args(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut options = Self::default();
        let mut args = args.into_iter().peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--resume" => options.resume = true,
                "--serve" => {
                    // A port may follow; anything else that follows is its own argument.
                    let port = match args.next_if(|next| next.chars().all(|c| c.is_ascii_digit())) {
                        Some(number) if number.is_empty() => DEFAULT_WEB_PORT,
                        Some(number) => number
                            .parse::<u16>()
                            .ok()
                            .filter(|port| *port > 0)
                            .ok_or_else(|| format!("'{number}' is not a port (1 to 65535)"))?,
                        None => DEFAULT_WEB_PORT,
                    };
                    options.serve = Some(port);
                }
                "--mcp" => {
                    let name = args
                        .next_if(|next| !next.starts_with("--"))
                        .ok_or("--mcp needs the name of a toolset, for example --mcp rust-docs")?;
                    options.mcp.push(McpRequest::Toolset(name));
                }
                "--mcp-add" => {
                    let mut words: Vec<String> = Vec::new();
                    while let Some(word) =
                        args.next_if(|next| !OWN_OPTIONS.contains(&next.as_str()))
                    {
                        words.push(word);
                    }
                    if words.len() < 2 {
                        return Err(
                            "--mcp-add needs a name and a program: --mcp-add NAME COMMAND [ARGS] [--env NAME=value] [--timeout SECONDS]"
                                .to_owned(),
                        );
                    }
                    options.mcp.push(McpRequest::Add(words.join(" ")));
                }
                other => {
                    return Err(format!(
                        "unknown option '{other}': the options are --resume, --serve [port], --mcp NAME and --mcp-add NAME COMMAND [ARGS]"
                    ));
                }
            }
        }
        if options.serve.is_some() && options.resume {
            return Err(
                "--serve shows a web page and starts no chat: it takes no --resume".to_owned(),
            );
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
    pub api: ApiStyle,
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
        let api = match get(ENV_API) {
            None => ApiStyle::OpenAi,
            Some(text) => ApiStyle::parse(&text).map_err(|why| format!("{ENV_API}: {why}"))?,
        };
        Ok(Self {
            api,
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

    #[test]
    fn a_variable_under_its_old_name_is_refused_with_the_new_name() {
        let with = |name: &'static str| move |n: &str| (n == name).then(|| "x".to_owned());
        assert_eq!(
            renamed_variable(with("HARNESS_PROFILE")).as_deref(),
            Some("HARNESS_PROFILE is now called HARNESS_PROVIDER: set it under the new name")
        );
        assert!(
            renamed_variable(with("HARNESS_PROFILE_DIR"))
                .unwrap()
                .contains("HARNESS_PROVIDER_DIR")
        );
        // Even an empty value counts: it was set.
        assert!(renamed_variable(|n| (n == "HARNESS_PROFILE").then(String::new)).is_some());
        // The new names are fine.
        assert_eq!(renamed_variable(with("HARNESS_PROVIDER")), None);
        assert_eq!(renamed_variable(with("HARNESS_PROVIDER_DIR")), None);
        assert_eq!(renamed_variable(|_| None), None);
    }

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
        assert_eq!(args(&[]).unwrap(), Options::default());
    }

    #[test]
    fn resume_is_the_one_option() {
        assert!(args(&["--resume"]).unwrap().resume);
        assert!(args(&["--resume", "--resume"]).unwrap().resume);
    }

    #[test]
    fn anything_else_is_rejected_by_name() {
        let why = args(&["--resum"]).unwrap_err();
        assert!(why.contains("--resum") && why.contains("--resume"), "{why}");
        assert!(why.contains("--serve"), "{why}");
        assert!(args(&["--resume", "extra"]).is_err());
    }

    #[test]
    fn serve_takes_an_optional_port_and_excludes_resume() {
        assert_eq!(args(&["--serve"]).unwrap().serve, Some(DEFAULT_WEB_PORT));
        assert_eq!(args(&["--serve", "9000"]).unwrap().serve, Some(9000));
        assert!(args(&["--serve", "0"]).is_err());
        assert!(args(&["--serve", "70000"]).is_err());
        assert!(args(&["--serve", "x"]).is_err());
        assert!(args(&["--serve", "--resume"]).is_err());
    }

    #[test]
    fn mcp_options_name_a_toolset_or_add_a_server() {
        let options = args(&[
            "--mcp",
            "rust-docs",
            "--mcp-add",
            "docs",
            "prog",
            "serve",
            "--cache-dir",
            "/x",
            "--env",
            "A=1",
            "--resume",
        ])
        .unwrap();
        assert!(options.resume);
        assert_eq!(
            options.mcp,
            [
                McpRequest::Toolset("rust-docs".to_owned()),
                McpRequest::Add("docs prog serve --cache-dir /x --env A=1".to_owned()),
            ]
        );
        // A name is needed, and for --mcp-add a program too.
        assert!(args(&["--mcp"]).is_err());
        assert!(args(&["--mcp", "--resume"]).is_err());
        assert!(args(&["--mcp-add", "docs"]).is_err());
        assert!(args(&["--mcp-add"]).is_err());
        assert!(args(&["--resume"]).unwrap().serve.is_none());
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
    fn the_data_folder_is_the_override_or_the_users_own_and_never_the_project() {
        let dir = |pairs: &'static [(&'static str, &'static str)]| log_dir_from_lookup(env(pairs));
        assert_eq!(
            dir(&[(ENV_LOG_DIR, "/data/x"), ("HOME", "/home/a")]),
            Ok(PathBuf::from("/data/x"))
        );
        if cfg!(not(any(windows, target_os = "macos"))) {
            assert_eq!(
                dir(&[("HOME", "/home/a")]),
                Ok(PathBuf::from("/home/a/.local/share/aharness"))
            );
            assert_eq!(
                dir(&[
                    (ENV_LOG_DIR, "  "),
                    ("HOME", "/home/a"),
                    ("XDG_DATA_HOME", "/x")
                ]),
                Ok(PathBuf::from("/x/aharness"))
            );
            // A relative XDG_DATA_HOME is ignored, as the standard says.
            assert_eq!(
                dir(&[("HOME", "/home/a"), ("XDG_DATA_HOME", "rel")]),
                Ok(PathBuf::from("/home/a/.local/share/aharness"))
            );
        }
        let none = dir(&[]).unwrap_err();
        assert!(none.contains(ENV_LOG_DIR), "{none}");
    }

    #[test]
    fn a_provider_is_named_when_either_variable_is_set() {
        assert!(!names_a_provider(env(&[])));
        assert!(!names_a_provider(env(&[(ENV_MODEL, " ")])));
        assert!(names_a_provider(env(&[(ENV_BASE_URL, "u")])));
        assert!(names_a_provider(env(&[(ENV_MODEL, "m")])));
    }
}

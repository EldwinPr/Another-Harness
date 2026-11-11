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

/// What `--decider` and `--eval-decider` name (issues 48 and 49).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeciderSpec {
    /// No model: today's behaviour, and what no `--decider` means.
    None,
    /// A normal model behind a saved provider. `provider` is `None` for the one the chat used
    /// last; `model` is `None` for that provider's default model.
    Llm {
        provider: Option<String>,
        model: Option<String>,
    },
    /// OpenRouter's Jev decisions model (issue 49). `provider` is `None` for the one the chat used
    /// last; it supplies the address and the key and must be an OpenRouter one. `model` is `None`
    /// for the pinned snapshot (`jev::MODEL`); the provider's own chat model is never used.
    Jev {
        provider: Option<String>,
        model: Option<String>,
    },
}

impl DeciderSpec {
    /// `none`, `llm`, `llm:PROVIDER`, `llm:PROVIDER:MODEL`, `jev`, `jev:PROVIDER` or
    /// `jev:PROVIDER:MODEL`. Split on the first two colons only: a model may hold `:` and `/`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let (kind, rest) = text
            .split_once(':')
            .map_or((text, None), |(k, r)| (k, Some(r)));
        match (kind, rest) {
            ("none", None) => Ok(DeciderSpec::None),
            ("llm", rest) => {
                let (provider, model) = provider_and_model("llm", rest)?;
                Ok(DeciderSpec::Llm { provider, model })
            }
            ("jev", rest) => {
                let (provider, model) = provider_and_model("jev", rest)?;
                Ok(DeciderSpec::Jev { provider, model })
            }
            _ => Err(format!(
                "'{text}' is not a decider: none, llm, llm:PROVIDER, llm:PROVIDER:MODEL, jev, jev:PROVIDER or jev:PROVIDER:MODEL"
            )),
        }
    }
}

/// What follows `kind:` in a spec: a provider name and then a model (which may hold `:`).
fn provider_and_model(
    kind: &str,
    rest: Option<&str>,
) -> Result<(Option<String>, Option<String>), String> {
    let Some(rest) = rest else {
        return Ok((None, None));
    };
    let (provider, model) = rest
        .split_once(':')
        .map_or((rest, None), |(p, m)| (p, Some(m)));
    if provider.trim().is_empty() {
        return Err(format!(
            "{kind}: needs a provider name: {kind}:PROVIDER[:MODEL]"
        ));
    }
    if model.is_some_and(|m| m.trim().is_empty()) {
        return Err(format!(
            "{kind}:PROVIDER: needs a model after the second colon"
        ));
    }
    Ok((
        Some(provider.trim().to_owned()),
        model.map(|m| m.trim().to_owned()),
    ))
}

/// What was typed after the program name.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Continue the most recent saved chat instead of starting a new one.
    pub resume: bool,
    /// Serve the web page (view and edit) on this port instead of starting a chat (issue 28).
    pub serve: Option<u16>,
    /// Run as an MCP server on standard input and output instead of starting a chat (issue 45).
    /// Without `direct` or `read_only` it is the buffer mode (issue 47): the read tools and `submit`.
    pub mcp_serve: bool,
    /// With `--mcp-serve`: offer the tools that write instead of `submit` (issue 46).
    pub direct: bool,
    /// With `--mcp-serve`: offer the read tools alone (issue 47). Excludes `direct`.
    pub read_only: bool,
    /// With `--mcp-serve` in the buffer mode: the decider that suggests accept, reject or unsure
    /// for each new submission (issue 48). `None` when the flag was not given, which is the same
    /// as `--decider none`.
    pub decider: Option<DeciderSpec>,
    /// With `--mcp-serve --decider SPEC`: apply a new entry the decider confidently accepts, at
    /// once, instead of leaving it for the owner (`--auto-accept`).
    pub auto_accept: bool,
    /// With `--mcp-serve`: speak MCP over HTTP on a localhost port instead of standard input and
    /// output (issue 52). `detach` implies it.
    pub http: bool,
    /// With `--mcp-serve`: start that HTTP server in the background and return (issue 52).
    pub detach: bool,
    /// With `--http`, `--detach` or `--mcp-stop`: the port (`DEFAULT_MCP_PORT` when not given).
    pub port: Option<u16>,
    /// Stop the detached MCP server on the port and exit (issue 52). Takes only `--port`.
    pub mcp_stop: bool,
    /// Run the labelled cases through this decider and print how it did, then exit (issue 48).
    /// Excludes every other option and writes nothing.
    pub eval_decider: Option<DeciderSpec>,
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
const OWN_OPTIONS: [&str; 13] = [
    "--resume",
    "--serve",
    "--mcp",
    "--mcp-add",
    "--mcp-serve",
    "--direct",
    "--read-only",
    "--decider",
    "--eval-decider",
    "--http",
    "--detach",
    "--mcp-stop",
    "--auto-accept",
];

/// The port `--serve` uses when none is given.
pub const DEFAULT_WEB_PORT: u16 = 8787;

/// The port the MCP server uses over HTTP when none is given (issue 52); the web page has 8787.
pub const DEFAULT_MCP_PORT: u16 = 8788;

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
                "--mcp-serve" => options.mcp_serve = true,
                "--http" => options.http = true,
                "--auto-accept" => options.auto_accept = true,
                "-d" | "--detach" => options.detach = true,
                "--mcp-stop" => options.mcp_stop = true,
                "--port" => {
                    let number = args
                        .next_if(|next| {
                            !next.is_empty() && next.chars().all(|c| c.is_ascii_digit())
                        })
                        .ok_or("--port needs a number (1 to 65535)")?;
                    options.port = Some(
                        number
                            .parse::<u16>()
                            .ok()
                            .filter(|port| *port > 0)
                            .ok_or_else(|| format!("'{number}' is not a port (1 to 65535)"))?,
                    );
                }
                "--direct" => options.direct = true,
                "--read-only" => options.read_only = true,
                "--decider" => {
                    let spec = args.next_if(|next| !next.starts_with("--")).ok_or(
                        "--decider needs a value: none, llm[:PROVIDER[:MODEL]] or jev[:PROVIDER[:MODEL]]",
                    )?;
                    options.decider =
                        Some(DeciderSpec::parse(&spec).map_err(|why| format!("--decider: {why}"))?);
                }
                "--eval-decider" => {
                    let spec = args.next_if(|next| !next.starts_with("--")).ok_or(
                        "--eval-decider needs a value: llm[:PROVIDER[:MODEL]] or jev[:PROVIDER[:MODEL]]",
                    )?;
                    options.eval_decider = Some(
                        DeciderSpec::parse(&spec)
                            .map_err(|why| format!("--eval-decider: {why}"))?,
                    );
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
                        "unknown option '{other}': the options are --resume, --serve [port], --mcp NAME, --mcp-add NAME COMMAND [ARGS] and --mcp-serve [--read-only | --direct | --decider SPEC] [--http | -d] [--port N], --mcp-stop and --eval-decider SPEC"
                    ));
                }
            }
        }
        if options.mcp_stop {
            if options.resume
                || options.serve.is_some()
                || options.mcp_serve
                || options.http
                || options.detach
                || options.direct
                || options.read_only
                || options.decider.is_some()
                || options.eval_decider.is_some()
                || !options.mcp.is_empty()
            {
                return Err(
                    "--mcp-stop stops the detached MCP server and exits: it takes only --port"
                        .to_owned(),
                );
            }
        } else if (options.http || options.detach || options.port.is_some()) && !options.mcp_serve {
            return Err(
                "--http, --detach (-d) and --port only go with --mcp-serve: they choose how that server is reached"
                    .to_owned(),
            );
        } else if options.port.is_some() && !options.http && !options.detach {
            return Err(
                "--port needs --http or --detach: over standard input and output there is no port"
                    .to_owned(),
            );
        }
        if options.detach {
            options.http = true;
        }
        if let Some(spec) = &options.eval_decider {
            if *spec == DeciderSpec::None {
                return Err(
                    "--eval-decider none has nothing to measure: name a decider, for example llm, llm:PROVIDER:MODEL or jev"
                        .to_owned(),
                );
            }
            if options.resume
                || options.serve.is_some()
                || options.mcp_serve
                || options.direct
                || options.read_only
                || options.decider.is_some()
                || options.mcp_stop
                || !options.mcp.is_empty()
            {
                return Err(
                    "--eval-decider runs the labelled cases and exits: it takes no other option"
                        .to_owned(),
                );
            }
        }
        if options.auto_accept
            && options
                .decider
                .as_ref()
                .is_none_or(|spec| *spec == DeciderSpec::None)
        {
            return Err(
                "--auto-accept needs a decider: add --decider llm or --decider jev (it applies what the decider accepts)"
                    .to_owned(),
            );
        }
        if options.decider.is_some() {
            if !options.mcp_serve {
                return Err(
                    "--decider only goes with --mcp-serve: it chooses who suggests what to do with a submission"
                        .to_owned(),
                );
            }
            if options.read_only || options.direct {
                return Err(
                    "--decider needs the buffer mode: it excludes --read-only and --direct, which have no inbox"
                        .to_owned(),
                );
            }
        }
        if options.serve.is_some() && options.resume {
            return Err(
                "--serve shows a web page and starts no chat: it takes no --resume".to_owned(),
            );
        }
        if options.mcp_serve
            && (options.resume || options.serve.is_some() || !options.mcp.is_empty())
        {
            return Err(
                "--mcp-serve runs an MCP server and starts no chat: it takes no --resume, --serve, --mcp or --mcp-add"
                    .to_owned(),
            );
        }
        if options.direct && options.read_only {
            return Err(
                "--read-only and --direct exclude each other: --read-only serves the reads alone, --direct lets the server write"
                    .to_owned(),
            );
        }
        if options.read_only && !options.mcp_serve {
            return Err(
                "--read-only only goes with --mcp-serve: it limits that server to the read tools"
                    .to_owned(),
            );
        }
        if options.direct && !options.mcp_serve {
            return Err(
                "--direct only goes with --mcp-serve: it lets that server write to the database"
                    .to_owned(),
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
    fn mcp_serve_is_alone_and_ends_the_words_of_mcp_add() {
        assert!(args(&["--mcp-serve"]).unwrap().mcp_serve);
        assert!(!args(&[]).unwrap().mcp_serve);
        // It starts no chat, so nothing that belongs to a chat or to the web page goes with it.
        for other in [
            &["--resume"][..],
            &["--serve"],
            &["--mcp", "rust-docs"],
            &["--mcp-add", "d", "p"],
        ] {
            let mut list = vec!["--mcp-serve"];
            list.extend_from_slice(other);
            let why = args(&list).unwrap_err();
            assert!(why.contains("--mcp-serve"), "{why}");
        }
        // It is not swallowed as a word of the server --mcp-add describes.
        let why = args(&["--mcp-add", "docs", "prog", "--mcp-serve"]).unwrap_err();
        assert!(why.contains("--mcp-serve"), "{why}");
    }

    #[test]
    fn http_detach_and_port_go_with_mcp_serve_and_detach_implies_http() {
        let plain = args(&["--mcp-serve"]).unwrap();
        assert!(!plain.http && !plain.detach && plain.port.is_none());
        let http = args(&["--mcp-serve", "--http"]).unwrap();
        assert!(http.http && !http.detach);
        for word in ["-d", "--detach"] {
            let options = args(&["--mcp-serve", word, "--direct"]).unwrap();
            assert!(options.detach && options.http && options.direct, "{word}");
        }
        assert_eq!(
            args(&["--mcp-serve", "-d", "--port", "9000"]).unwrap().port,
            Some(9000)
        );
        for alone in [
            &["--http"][..],
            &["-d"],
            &["--detach"],
            &["--port", "9000"],
            &["--resume", "-d"],
        ] {
            let why = args(alone).unwrap_err();
            assert!(why.contains("--mcp-serve"), "{alone:?}: {why}");
        }
        // A port over standard input and output has nothing to open.
        assert!(args(&["--mcp-serve", "--port", "9000"]).is_err());
        for bad in [
            &["--port"][..],
            &["--port", "0"],
            &["--port", "70000"],
            &["--port", "x"],
        ] {
            let mut list = vec!["--mcp-serve", "--http"];
            list.extend_from_slice(bad);
            assert!(args(&list).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn auto_accept_needs_a_decider() {
        let options = args(&["--mcp-serve", "--decider", "jev", "--auto-accept"]).unwrap();
        assert!(options.auto_accept);
        assert!(
            !args(&["--mcp-serve", "--decider", "jev"])
                .unwrap()
                .auto_accept
        );
        for alone in [
            &["--mcp-serve", "--auto-accept"][..],
            &["--mcp-serve", "--decider", "none", "--auto-accept"],
            &["--auto-accept"],
        ] {
            let why = args(alone).unwrap_err();
            assert!(why.contains("--auto-accept"), "{alone:?}: {why}");
        }
    }

    #[test]
    fn mcp_stop_takes_only_a_port() {
        let options = args(&["--mcp-stop"]).unwrap();
        assert!(options.mcp_stop && options.port.is_none());
        assert_eq!(args(&["--mcp-stop", "--port", "9"]).unwrap().port, Some(9));
        for other in [
            &["--mcp-serve"][..],
            &["--resume"],
            &["--serve"],
            &["--http"],
            &["-d"],
            &["--direct"],
            &["--eval-decider", "llm"],
        ] {
            let mut list = vec!["--mcp-stop"];
            list.extend_from_slice(other);
            let why = args(&list).unwrap_err();
            assert!(why.contains("--mcp-stop"), "{other:?}: {why}");
        }
    }

    #[test]
    fn direct_goes_only_with_mcp_serve() {
        let options = args(&["--mcp-serve", "--direct"]).unwrap();
        assert!(options.mcp_serve && options.direct);
        assert!(args(&["--direct", "--mcp-serve"]).unwrap().direct);
        assert!(!args(&["--mcp-serve"]).unwrap().direct);
        for alone in [
            &["--direct"][..],
            &["--direct", "--resume"],
            &["--resume", "--direct"],
        ] {
            let why = args(alone).unwrap_err();
            assert!(
                why.contains("--direct") && why.contains("--mcp-serve"),
                "{why}"
            );
        }
        // It is not swallowed as a word of the server --mcp-add describes.
        assert!(args(&["--mcp-add", "d", "p", "--direct"]).is_err());
    }

    #[test]
    fn read_only_goes_only_with_mcp_serve_and_excludes_direct() {
        let options = args(&["--mcp-serve", "--read-only"]).unwrap();
        assert!(options.mcp_serve && options.read_only && !options.direct);
        assert!(args(&["--read-only", "--mcp-serve"]).unwrap().read_only);
        // The default is neither flag: the buffer mode.
        let plain = args(&["--mcp-serve"]).unwrap();
        assert!(!plain.read_only && !plain.direct);
        for alone in [
            &["--read-only"][..],
            &["--read-only", "--resume"],
            &["--resume", "--read-only"],
        ] {
            let why = args(alone).unwrap_err();
            assert!(
                why.contains("--read-only") && why.contains("--mcp-serve"),
                "{why}"
            );
        }
        for both in [
            &["--mcp-serve", "--read-only", "--direct"][..],
            &["--mcp-serve", "--direct", "--read-only"],
            &["--read-only", "--direct"],
        ] {
            let why = args(both).unwrap_err();
            assert!(
                why.contains("--read-only") && why.contains("--direct"),
                "{why}"
            );
        }
        // It is not swallowed as a word of the server --mcp-add describes.
        assert!(args(&["--mcp-add", "d", "p", "--read-only"]).is_err());
    }

    fn llm(provider: Option<&str>, model: Option<&str>) -> DeciderSpec {
        DeciderSpec::Llm {
            provider: provider.map(str::to_owned),
            model: model.map(str::to_owned),
        }
    }

    #[test]
    fn a_decider_spec_is_none_llm_llm_provider_or_llm_provider_model() {
        assert_eq!(DeciderSpec::parse("none"), Ok(DeciderSpec::None));
        assert_eq!(DeciderSpec::parse("llm"), Ok(llm(None, None)));
        assert_eq!(
            DeciderSpec::parse("llm:openrouter"),
            Ok(llm(Some("openrouter"), None))
        );
        assert_eq!(
            DeciderSpec::parse("llm:openrouter:deepseek-chat"),
            Ok(llm(Some("openrouter"), Some("deepseek-chat")))
        );
        // Split on the first two colons only: a model may hold ':' and '/'.
        assert_eq!(
            DeciderSpec::parse("llm:openrouter:deepseek/deepseek-chat:free"),
            Ok(llm(Some("openrouter"), Some("deepseek/deepseek-chat:free")))
        );
        assert_eq!(
            DeciderSpec::parse("llm:p:a:b:c"),
            Ok(llm(Some("p"), Some("a:b:c")))
        );
    }

    fn jev(provider: Option<&str>, model: Option<&str>) -> DeciderSpec {
        DeciderSpec::Jev {
            provider: provider.map(str::to_owned),
            model: model.map(str::to_owned),
        }
    }

    #[test]
    fn a_bad_decider_spec_is_refused() {
        for bad in [
            "", "LLM", "gpt", "none:x", "llm:", "llm::m", "llm:p:", "llm: :m", "JEV", "jev:",
            "jev::m", "jev:p:", "jev: :m",
        ] {
            assert!(DeciderSpec::parse(bad).is_err(), "{bad:?}");
        }
        let why = args(&["--mcp-serve", "--decider", "jev:"]).unwrap_err();
        assert!(why.contains("--decider") && why.contains("jev"), "{why}");
    }

    #[test]
    fn a_jev_spec_is_jev_jev_provider_or_jev_provider_model() {
        assert_eq!(DeciderSpec::parse("jev"), Ok(jev(None, None)));
        assert_eq!(
            DeciderSpec::parse("jev:openrouter"),
            Ok(jev(Some("openrouter"), None))
        );
        assert_eq!(
            DeciderSpec::parse("jev:openrouter:typesafe/jev-1.14-20261014"),
            Ok(jev(Some("openrouter"), Some("typesafe/jev-1.14-20261014")))
        );
        // Split on the first two colons only: a model may hold ':' and '/'.
        assert_eq!(
            DeciderSpec::parse("jev:p:a/b:c"),
            Ok(jev(Some("p"), Some("a/b:c")))
        );
        for flag in ["--decider", "--eval-decider"] {
            for (spec, want) in [
                ("jev", jev(None, None)),
                ("jev:p", jev(Some("p"), None)),
                ("jev:p:m", jev(Some("p"), Some("m"))),
            ] {
                let list: Vec<&str> = if flag == "--decider" {
                    vec!["--mcp-serve", flag, spec]
                } else {
                    vec![flag, spec]
                };
                let options = args(&list).unwrap();
                let got = if flag == "--decider" {
                    options.decider
                } else {
                    options.eval_decider
                };
                assert_eq!(got, Some(want), "{list:?}");
            }
        }
    }

    #[test]
    fn decider_goes_only_with_the_buffer_mode_of_mcp_serve() {
        let options = args(&["--mcp-serve", "--decider", "llm:p:m"]).unwrap();
        assert_eq!(options.decider, Some(llm(Some("p"), Some("m"))));
        assert_eq!(
            args(&["--decider", "none", "--mcp-serve"]).unwrap().decider,
            Some(DeciderSpec::None)
        );
        // The default is no flag, which is the same as none.
        assert_eq!(args(&["--mcp-serve"]).unwrap().decider, None);
        for alone in [
            &["--decider", "llm"][..],
            &["--resume", "--decider", "llm"],
            &["--serve", "--decider", "none"],
        ] {
            let why = args(alone).unwrap_err();
            assert!(
                why.contains("--decider") && why.contains("--mcp-serve"),
                "{why}"
            );
        }
        for mode in ["--read-only", "--direct"] {
            let why = args(&["--mcp-serve", mode, "--decider", "llm"]).unwrap_err();
            assert!(why.contains("--decider") && why.contains(mode), "{why}");
            assert!(args(&["--decider", "llm", "--mcp-serve", mode]).is_err());
        }
        // A value is needed, and it is not another option.
        assert!(args(&["--mcp-serve", "--decider"]).is_err());
        assert!(args(&["--mcp-serve", "--decider", "--read-only"]).is_err());
        // It is not swallowed as a word of the server --mcp-add describes.
        assert!(args(&["--mcp-add", "d", "p", "--decider", "llm"]).is_err());
    }

    #[test]
    fn eval_decider_excludes_every_other_option_and_none() {
        let options = args(&["--eval-decider", "llm:p:m:free"]).unwrap();
        assert_eq!(options.eval_decider, Some(llm(Some("p"), Some("m:free"))));
        assert!(options.decider.is_none() && !options.mcp_serve);
        let why = args(&["--eval-decider", "none"]).unwrap_err();
        assert!(
            why.contains("--eval-decider") && why.contains("none"),
            "{why}"
        );
        for other in [
            &["--resume"][..],
            &["--serve"],
            &["--mcp-serve"],
            &["--mcp-serve", "--read-only"],
            &["--mcp-serve", "--direct"],
            &["--mcp-serve", "--decider", "llm"],
            &["--mcp", "rust-docs"],
            &["--mcp-add", "d", "p"],
        ] {
            for first in [true, false] {
                let mut list = vec!["--eval-decider", "llm"];
                if first {
                    list.splice(0..0, other.iter().copied());
                } else {
                    list.extend_from_slice(other);
                }
                let why = args(&list).unwrap_err();
                assert!(why.contains("--eval-decider"), "{list:?}: {why}");
            }
        }
        assert!(args(&["--eval-decider"]).is_err());
        assert!(args(&["--eval-decider", "jev", "--mcp-serve"]).is_err());
        assert!(args(&["--eval-decider", "jev", "--decider", "jev"]).is_err());
        assert!(args(&["--mcp-add", "d", "p", "--eval-decider", "llm"]).is_err());
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

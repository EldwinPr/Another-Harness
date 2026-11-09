//! The chat binary. Starts the REPL; the provider and model are chosen inside the chat
//! (`/provider`, `/model`).
//!
//! ```text
//! cd src && ./chat.sh                 # starts on the provider used last, or on none
//! cd src && ./chat.sh --resume        # continue the most recent saved chat
//! ```
//!
//! Without `chat.sh`, `HARNESS_BASE_URL` and `HARNESS_MODEL` start the chat on that provider
//! (`HARNESS_API_KEY` only if the router wants one):
//!
//! ```text
//! cd src && HARNESS_BASE_URL=http://localhost:20128/v1 HARNESS_MODEL=<model> cargo run -p runner
//! ```
//!
//! `HARNESS_LOG_DIR` overrides where the log and the database go (default: `logs` under the
//! directory you started in), and `HARNESS_PROFILE_DIR` names the folder of profile files that
//! `/provider` lists next to the providers saved in the database.
//!
//! Chats are saved in SQLite at `<log dir>/harness.db` (decision 08). The JSONL turn
//! log next to it is a secondary record.
//!
//! Exit code: 0 on a normal quit, 1 on a failure to start, 2 on missing settings or a
//! bad option.

use std::io::{self, IsTerminal};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

use runner::agent::{Agent, MAX_CALLS_PER_MESSAGE, StoreContext};
use runner::config::{self, Options, Settings};
use runner::provider::{HttpProvider, NoProvider, Provider, router_host};
use runner::providers::Catalog;
use runner::repl::{self, Terminal, TerminalApprover};
use runner::store::Store;
use runner::style::Style;
use runner::tools::{Bash, DEFAULT_TIMEOUT, Mode, ReadFile, Tool, file_tools, shared_mode};
use runner::turn_log::TurnLog;

/// How long to wait for the router to answer one call.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// The name to show for a provider given through the environment (default: the router's host).
const ENV_PROFILE: &str = "HARNESS_PROFILE";

/// The folder of profile files `/provider` lists (`chat.sh` passes an absolute path).
const ENV_PROFILE_DIR: &str = "HARNESS_PROFILE_DIR";

/// The database file, inside the log directory.
const DB_FILE: &str = "harness.db";

fn main() -> ExitCode {
    let options = match Options::from_args(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(why) => {
            eprintln!("{why}");
            return ExitCode::from(2);
        }
    };
    let env = |name: &str| std::env::var(name).ok();
    let log_dir = config::log_dir_from_lookup(env);
    // Only when the environment names a provider does it have to name a whole one.
    let from_env = if config::names_a_provider(env) {
        match Settings::from_env() {
            Ok(settings) => Some(settings),
            Err(why) => {
                eprintln!("{why}");
                return ExitCode::from(2);
            }
        }
    } else {
        None
    };
    match start(log_dir, from_env, &options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("error: {why}");
            ExitCode::FAILURE
        }
    }
}

fn start(
    log_dir: std::path::PathBuf,
    from_env: Option<Settings>,
    options: &Options,
) -> Result<(), String> {
    // The tools work in the directory the program was started in.
    let cwd = std::env::current_dir().map_err(|e| format!("no working directory: {e}"))?;
    let log = TurnLog::create(&log_dir).map_err(|e| format!("cannot create the log: {e}"))?;
    let db_path = log_dir.join(DB_FILE);
    let store =
        Store::open(&db_path).map_err(|e| format!("cannot open {}: {e}", db_path.display()))?;

    // Rust note: `io::stdin().lock()` holds the keyboard for the whole run, so
    // the approval question and the main prompt read from the same place.
    // Colour only when printing to a terminal and the user has not asked for none (NO_COLOR).
    let style = Style::detect(io::stdout().is_terminal(), |name| std::env::var(name).ok());
    let term = Terminal::shared_styled(io::stdin().lock(), io::stdout(), style);
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        // An API key typed with /provider add stays off the screen.
        term.borrow_mut().hide_secrets();
        // Arrow-key menus for /provider, /model and /provider config.
        term.borrow_mut().enable_menus(true);
        // A longer text (a prompt) is edited in the owner's editor when there is one.
        let editor = std::env::var("VISUAL")
            .ok()
            .filter(|e| !e.trim().is_empty())
            .or_else(|| std::env::var("EDITOR").ok());
        term.borrow_mut().set_editor(editor);
    }
    // The mode is read from the database once the agent has it; until then every change asks.
    let approval = shared_mode(Mode::Manual);
    term.borrow_mut().set_root(cwd.clone());
    let cancel = runner::cancel::Cancel::new();
    let mut tools: Vec<Box<dyn Tool>> = vec![Box::new(ReadFile::new(&cwd))];
    tools.extend(file_tools(
        &cwd,
        Rc::clone(&approval),
        Rc::new(TerminalApprover(Rc::clone(&term))),
    ));
    tools.push(Box::new(
        Bash::new(
            &cwd,
            DEFAULT_TIMEOUT,
            Box::new(TerminalApprover(Rc::clone(&term))),
        )
        .with_approval(Rc::clone(&approval))
        .with_cancel(cancel.clone()),
    ));
    let build = |def: &runner::providers::ProviderDef| -> Box<dyn Provider> {
        Box::new(HttpProvider::new(
            &def.base_url,
            def.api_key.as_deref(),
            REQUEST_TIMEOUT,
        ))
    };
    let catalog = Catalog {
        profile_dir: std::env::var(ENV_PROFILE_DIR)
            .ok()
            .filter(|dir| !dir.trim().is_empty())
            .map(std::path::PathBuf::from),
        build: Box::new(build),
    };
    let store_context = StoreContext {
        cwd: cwd.display().to_string(),
    };
    // A provider named in the environment is used as given. Otherwise the chat starts on the
    // provider used last time, or on none, and one is chosen inside the chat.
    let mut agent = match &from_env {
        Some(settings) => {
            let name = std::env::var(ENV_PROFILE)
                .ok()
                .filter(|name| !name.trim().is_empty())
                .or_else(|| router_host(&settings.base_url))
                .unwrap_or_default();
            Agent::new(
                settings.model.clone(),
                build(&runner::providers::ProviderDef {
                    name: name.clone(),
                    base_url: settings.base_url.clone(),
                    model: settings.model.clone(),
                    api_key: settings.api_key.clone(),
                    router: settings.router.clone(),
                }),
                tools,
                log,
            )
            .with_router(settings.router.clone())
            .with_target(name, settings.base_url.clone())
        }
        None => Agent::new("", Box::new(NoProvider), tools, log),
    }
    .with_catalog(catalog)
    .with_approval(approval)
    .with_cancel(cancel)
    .with_store(store, store_context);
    agent.load_approval_mode();

    let mut notes = String::new();
    // Profile files are copied into the database once, so the menus can edit them.
    match agent.import_profiles() {
        Ok(0) => {}
        Ok(n) => notes.push_str(&format!(
            "copied {n} profile file{} into the database\n",
            if n == 1 { "" } else { "s" }
        )),
        Err(why) => notes.push_str(&format!("could not import the profile files: {why}\n")),
    }
    if from_env.is_none()
        && let Some(note) = agent.use_remembered_provider()
    {
        notes.push_str(&format!("{note}\n"));
    }

    // `--resume` continues the most recent saved chat; with none saved, start a new one.
    let mut was_resumed = false;
    let chat_line = if options.resume {
        match agent.resume(None) {
            Ok(report) => {
                was_resumed = true;
                notes.push_str(&report.notes());
                report.headline()
            }
            Err(why) => {
                notes.push_str(&format!("note: {why}, starting a new chat\n"));
                "new".to_owned()
            }
        }
    } else {
        "new".to_owned()
    };

    // The system prompt: made from the prompts switched on now, or the saved chat's own.
    let prompt_line = if was_resumed {
        format!(
            "the saved chat's own, {}",
            runner::store_prompts::size_label(agent.prompt_chars())
        )
    } else {
        match agent.load_prompt() {
            Ok(info) => info.summary(),
            Err(why) => {
                notes.push_str(&format!("could not read the prompts: {why}\n"));
                "the built-in text".to_owned()
            }
        }
    };

    // One labelled line per setting, the labels dimmed.
    let row = |label: &str, value: &str| format!("{}{value}\n", style.dim(&format!("{label:<13}")));
    let mut banner = format!(
        "{} {}\n{}",
        style.accent("harness"),
        style.dim(env!("CARGO_PKG_VERSION")),
        style.dim(&"─".repeat(60)),
    );
    banner.push('\n');
    if agent.ready() {
        banner.push_str(&row("model:", &style.bold(agent.model())));
        banner.push_str(&row("provider:", &agent.target().name));
        banner.push_str(&row("router:", &agent.target().base_url));
    } else {
        banner.push_str(&row(
            "provider:",
            &style.yellow("none selected: use /provider <name> or /provider add"),
        ));
    }
    banner.push_str(&row("working dir:", &cwd.display().to_string()));
    banner.push_str(&row("chat:", &chat_line));
    banner.push_str(&row("prompt:", &prompt_line));
    banner.push_str(&row(
        "approve:",
        &format!(
            "{} ({}; /approve changes it)",
            agent.approval_mode().name(),
            agent.approval_mode().describe()
        ),
    ));
    // Only when something is active: a personality and skills are added after each message.
    if let Some(personality) = &agent.chat_prompts().personality {
        banner.push_str(&row("personality:", personality));
    }
    if !agent.chat_prompts().on.is_empty() {
        banner.push_str(&row("skills:", &agent.chat_prompts().on.join(", ")));
    }
    banner.push_str(&row("database:", &db_path.display().to_string()));
    banner.push_str(&row("log:", &agent.log_path().display().to_string()));
    banner.push_str(&row(
        "limit:",
        &format!("{MAX_CALLS_PER_MESSAGE} model calls per message"),
    ));
    banner.push_str(&style.dim(&"─".repeat(60)));
    banner.push('\n');
    if !notes.is_empty() {
        banner.push_str(&style.yellow(notes.trim_end_matches('\n')));
        banner.push('\n');
    }
    banner.push('\n');
    term.borrow_mut().say(&banner);
    if was_resumed {
        // Show the saved conversation so you can see where you were.
        repl::show_history(&term, agent.history());
    }
    repl::run(&mut agent, &term).map_err(|e| e.to_string())
}

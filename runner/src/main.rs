//! The chat binary. Starts the REPL; the provider and model are chosen inside the chat
//! (`/provider`, `/model`).
//!
//! ```text
//! cd src && ./chat.sh                 # starts on the provider used last, or on none
//! cd src && ./chat.sh --resume        # continue the most recent saved chat
//! cd src && ./chat.sh --mcp-serve     # no chat: an MCP server on stdin/stdout: reads, and submit to the owner's inbox (issues 45, 47)
//! cd src && ./chat.sh --mcp-serve --read-only   # the same, with the read tools alone (issue 47)
//! cd src && ./chat.sh --mcp-serve --direct   # the same, with tools that write instead of submit (issue 46)
//! cd src && ./chat.sh --mcp-serve -d [--port N]   # the same over HTTP on localhost, in the background (issue 52); --mcp-stop ends it
//! cd src && ./chat.sh --mcp-serve --decider llm:PROVIDER[:MODEL]   # a model suggests accept/reject for each submission (issue 48)
//! cd src && ./chat.sh --mcp-serve --decider jev[:PROVIDER[:MODEL]]   # the same with OpenRouter's Jev decisions model (issue 49)
//! cd src && ./chat.sh --eval-decider llm:PROVIDER[:MODEL]   # run the labelled cases through a model, write nothing (issue 48)
//! cd src && ./chat.sh --eval-decider jev[:PROVIDER[:MODEL]]   # the same through Jev (issue 49)
//! ```
//!
//! Without `chat.sh`, `HARNESS_BASE_URL` and `HARNESS_MODEL` start the chat on that provider
//! (`HARNESS_API_KEY` only if the router wants one):
//!
//! ```text
//! cd src && HARNESS_BASE_URL=http://localhost:20128/v1 HARNESS_MODEL=<model> cargo run -p runner
//! ```
//!
//! `HARNESS_LOG_DIR` overrides where the log and the database go (default: the program's folder
//! in your user data folder, `~/.local/share/aharness` on Linux; never the project you start it in), and `HARNESS_PROVIDER_DIR` names the folder of provider files that
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

use runner::agent::{Agent, StoreContext};
use runner::config::{self, ApiStyle, DeciderSpec, Options, Settings};
use runner::provider::{HttpProvider, NoProvider, Provider, REQUEST_TIMEOUT, router_host};
use runner::providers::{Catalog, ProviderDef};
use runner::repl::{self, Terminal, TerminalApprover};
use runner::store::Store;
use runner::style::Style;
use runner::tools::{Mode, Tool, shared_mode};
use runner::turn_log::TurnLog;

/// The name to show for a provider given through the environment (default: the router's host).
const ENV_PROVIDER: &str = "HARNESS_PROVIDER";

/// The folder of provider files `/provider` lists (`chat.sh` passes an absolute path).
const ENV_PROVIDER_DIR: &str = "HARNESS_PROVIDER_DIR";

/// The database file, inside the log directory.
const DB_FILE: &str = "harness.db";

/// A provider object for a saved provider: the one place that knows which API a `ProviderDef`
/// speaks. The chat's catalog and the deciders (issue 48) both build through it.
fn build_provider(def: &ProviderDef) -> Box<dyn Provider> {
    match def.api {
        ApiStyle::OpenAi => Box::new(HttpProvider::new(
            &def.base_url,
            def.api_key.as_deref(),
            REQUEST_TIMEOUT,
        )),
        ApiStyle::Anthropic => Box::new(runner::anthropic::AnthropicProvider::new(
            &def.base_url,
            def.api_key.as_deref(),
            REQUEST_TIMEOUT,
        )),
    }
}

/// The decider a spec names, built against the providers saved in the database (issue 48). Reads
/// the database and writes nothing. The error is the reason, and never holds a key.
fn decider_for(
    db: &std::path::Path,
    spec: &DeciderSpec,
) -> Result<Option<Box<dyn runner::decider::Decider>>, String> {
    let store = Store::open_read_only(db).map_err(|e| {
        format!(
            "cannot read {}: {e} (start the chat once to create it, or set HARNESS_LOG_DIR)",
            db.display()
        )
    })?;
    runner::decider::resolve(spec, &store, &build_provider)
}

/// `--mcp-serve -d`: checks what the child would check (the mode against the database, the decider,
/// the token file, the port), then starts the child in the background and says how to reach it.
fn detach(
    db: &std::path::Path,
    dir: &std::path::Path,
    project: &str,
    mode: runner::mcp_serve::Mode,
    decider: Option<Box<dyn runner::decider::Decider>>,
    auto_accept: bool,
    port: u16,
) -> Result<(), String> {
    runner::mcp_serve::build(db, project, mode, decider, auto_accept)?;
    runner::mcp_serve::http::token(dir)?;
    // The port must be free now, and no server noted for it may still be running.
    drop(runner::mcp_serve::http::bind(port)?);
    let args: Vec<String> = std::env::args().skip(1).collect();
    let pid = runner::mcp_serve::spawn_detached(dir, &args, port)?;
    print!(
        "{}",
        runner::mcp_serve::http::announce(port, pid, mode.label(), project, dir)
    );
    Ok(())
}

fn main() -> ExitCode {
    let options = match Options::from_args(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(why) => {
            eprintln!("{why}");
            return ExitCode::from(2);
        }
    };
    let env = |name: &str| std::env::var(name).ok();
    if let Some(why) = config::renamed_variable(env) {
        eprintln!("{why}");
        return ExitCode::from(2);
    }
    let log_dir = match config::log_dir_from_lookup(env) {
        Ok(dir) => dir,
        Err(why) => {
            eprintln!("error: {why}");
            return ExitCode::FAILURE;
        }
    };
    // `--eval-decider` runs the labelled cases through a decider and prints how it did (issue 48).
    // It reads the saved providers and writes nothing, not even to the database.
    if let Some(spec) = &options.eval_decider {
        let outcome = decider_for(&log_dir.join(DB_FILE), spec).and_then(|decider| {
            let decider = decider.ok_or("there is no decider to measure")?;
            let report = runner::decider_cases::run(decider.as_ref(), &mut io::stdout().lock())
                .map_err(|e| format!("cannot print the report: {e}"))?;
            // A run in which calls failed judged less than it says: the exit code tells a script.
            match report.summary().failures {
                0 => Ok(()),
                failed => Err(format!(
                    "{failed} of the calls failed, so this run is incomplete"
                )),
            }
        });
        return match outcome {
            Ok(()) => ExitCode::SUCCESS,
            Err(why) => {
                eprintln!("error: {why}");
                ExitCode::FAILURE
            }
        };
    }
    // `--mcp-stop` ends the detached MCP server on a port (issue 52).
    if options.mcp_stop {
        let port = options.port.unwrap_or(config::DEFAULT_MCP_PORT);
        return match runner::mcp_serve::stop_detached(&log_dir, port) {
            Ok(said) => {
                println!("{said}");
                ExitCode::SUCCESS
            }
            Err(why) => {
                eprintln!("mcp: {why}");
                ExitCode::FAILURE
            }
        };
    }
    // `--mcp-serve` answers another harness over standard input and output and starts no chat and
    // needs no provider (issue 45). By default it reads and queues submissions for the owner
    // (issue 47); `--read-only` reads alone and `--direct` offers the write tools (issue 46). The
    // project is the folder it was started in.
    if options.mcp_serve {
        let project = std::env::current_dir()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default();
        let mode = if options.read_only {
            runner::mcp_serve::Mode::ReadOnly
        } else if options.direct {
            runner::mcp_serve::Mode::Direct
        } else {
            runner::mcp_serve::Mode::Buffer
        };
        let db_path = log_dir.join(DB_FILE);
        // A decider that cannot be built (an unknown provider, no key) stops the start here.
        let decider = match options
            .decider
            .as_ref()
            .filter(|s| **s != DeciderSpec::None)
        {
            None => None,
            Some(spec) => match decider_for(&db_path, spec) {
                Ok(decider) => decider,
                Err(why) => {
                    eprintln!("error: --decider: {why}");
                    return ExitCode::FAILURE;
                }
            },
        };
        if let Some(decider) = &decider {
            eprintln!(
                "decider: {} suggests accept or reject for each submission; the owner still decides",
                decider.name()
            );
        }
        // Over HTTP (issue 52) it is reached on a localhost port; `-d` starts that in the
        // background, after the same checks, and returns.
        let outcome = if options.detach {
            let port = options.port.unwrap_or(config::DEFAULT_MCP_PORT);
            detach(
                &db_path,
                &log_dir,
                &project,
                mode,
                decider,
                options.auto_accept,
                port,
            )
        } else if options.http {
            let port = options.port.unwrap_or(config::DEFAULT_MCP_PORT);
            runner::mcp_serve::run_http(
                &db_path,
                &log_dir,
                &project,
                mode,
                decider,
                options.auto_accept,
                port,
            )
        } else {
            runner::mcp_serve::run(&db_path, &project, mode, decider, options.auto_accept)
        };
        return match outcome {
            Ok(()) => ExitCode::SUCCESS,
            Err(why) => {
                eprintln!("error: {why}");
                ExitCode::FAILURE
            }
        };
    }
    // `--serve` shows the lists in a browser and starts no chat (issue 28).
    if let Some(port) = options.serve {
        let db_path = log_dir.join(DB_FILE);
        let project = std::env::current_dir()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default();
        return match runner::web::serve(&db_path, port, &project) {
            Ok(()) => ExitCode::SUCCESS,
            Err(why) => {
                eprintln!("error: {why}");
                ExitCode::FAILURE
            }
        };
    }
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
    let mut store =
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
    let shells = runner::tools::ShellTasks::new();
    let approver: Rc<dyn runner::tools::Approver> = Rc::new(TerminalApprover(Rc::clone(&term)));
    let mut tools: Vec<Box<dyn Tool>> = runner::tools::chat_tools(
        &cwd,
        &db_path,
        Rc::clone(&approval),
        Rc::clone(&approver),
        cancel.clone(),
        shells.clone(),
    );
    // The MCP servers the owner added start now, and their tools are fixed for this chat
    // (issue 44): a change made with /mcp applies the next time the program starts.
    // `--mcp` and `--mcp-add` save their servers first, so they start in this very chat.
    let added = runner::mcp_ui::apply_startup(&mut store, &options.mcp);
    let mut mcp = runner::mcp::McpServers::start_all(&store, &cwd, &cancel);
    mcp.notes.splice(0..0, added);
    tools.extend(runner::tools::mcp_tools(&mcp, &approval, &approver));
    term.borrow_mut().set_mcp(mcp.clients());
    let catalog = Catalog {
        provider_dir: std::env::var(ENV_PROVIDER_DIR)
            .ok()
            .filter(|dir| !dir.trim().is_empty())
            .map(std::path::PathBuf::from),
        build: Box::new(build_provider),
    };
    let store_context = StoreContext {
        cwd: cwd.display().to_string(),
    };
    // A provider named in the environment is used as given. Otherwise the chat starts on the
    // provider used last time, or on none, and one is chosen inside the chat.
    let mut agent = match &from_env {
        Some(settings) => {
            let name = std::env::var(ENV_PROVIDER)
                .ok()
                .filter(|name| !name.trim().is_empty())
                .or_else(|| router_host(&settings.base_url))
                .unwrap_or_default();
            Agent::new(
                settings.model.clone(),
                build_provider(&ProviderDef {
                    name: name.clone(),
                    base_url: settings.base_url.clone(),
                    model: settings.model.clone(),
                    api_key: settings.api_key.clone(),
                    router: settings.router.clone(),
                    api: settings.api,
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
    .with_shells(shells)
    .with_subagents({
        let (cwd, db_path) = (cwd.clone(), db_path.clone());
        std::sync::Arc::new(move |kit, cancel| runner::subagent::tools(&cwd, &db_path, cancel, kit))
    })
    .with_store(store, store_context);
    agent.load_approval_mode();

    let mut notes = String::new();
    for note in &mcp.notes {
        notes.push_str(note);
        notes.push('\n');
    }
    // Provider files are copied into the database once, so the menus can edit them.
    match agent.import_provider_files() {
        Ok(0) => {}
        Ok(n) => notes.push_str(&format!(
            "copied {n} provider file{} into the database\n",
            if n == 1 { "" } else { "s" }
        )),
        Err(why) => notes.push_str(&format!("could not import the provider files: {why}\n")),
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
    if !mcp.started.is_empty() {
        let line = mcp
            .started
            .iter()
            .map(|s| {
                let (tools, tokens) = s.on_cost();
                format!(
                    "{} ({tools} tools, ~{} tokens a call)",
                    s.server.name,
                    runner::web_usage::compact(i64::try_from(tokens).unwrap_or(i64::MAX))
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        banner.push_str(&row("mcp:", &line));
    }
    banner.push_str(&row("database:", &db_path.display().to_string()));
    banner.push_str(&row("log:", &agent.log_path().display().to_string()));
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

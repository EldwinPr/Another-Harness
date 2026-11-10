//! `/mcp` (issue 44): the MCP servers the chat starts, and what the model may do with their tools.
//!
//! Typed forms only (they work with or without a terminal):
//!
//! ```text
//! /mcp                                       list the servers and their rules
//! /mcp add NAME COMMAND [ARGS] [--env N=v] [--timeout S]
//! /mcp remove NAME
//! /mcp enable NAME | disable NAME
//! /mcp tools NAME                            start the server for a moment and list its tools
//! /mcp rule NAME TOOL|* ask|free|off
//! ```
//!
//! Every change is for the next time the program starts: the tools of a chat are fixed when it
//! starts, so the router's cached prompt is not thrown away midway. The words are split on
//! spaces; a program or argument with a space in it is not supported here.

use std::io::{BufRead, Write};

use crate::agent::Agent;
use crate::cancel::Cancel;
use crate::config::McpRequest;
use crate::config_ui::{done, error};
use crate::mcp::McpClient;
use crate::mcp_toolsets::{self, Installed, TOOLSETS, Toolset};
use crate::menu::MenuItem;
use crate::prompts_ui::with_store;
use crate::repl::{Shared, say_line};
use crate::store::Store;
use crate::store_mcp::{
    ALL_TOOLS, DEFAULT_TIMEOUT_SECS, McpServer, Rule, check_command, check_name, check_var_name,
    rule_of,
};

/// How much of a tool's description a line shows.
const SUMMARY_CHARS: usize = 70;

const USAGE: &str = "usage: /mcp [config | add NAME COMMAND [ARGS] [--env NAME=value] [--timeout SECONDS] | remove NAME | enable NAME | disable NAME | tools NAME | restart NAME | rule NAME TOOL|* ask|free|off | toolset [add NAME]]";

const NEXT_START: &str = "applies the next time the program starts";

/// What `/mcp add` was given.
#[derive(Debug, PartialEq, Eq)]
struct AddArgs {
    name: String,
    command: String,
    args: Vec<String>,
    vars: Vec<(String, String)>,
    timeout_secs: u64,
}

fn parse_add(tail: &str) -> Result<AddArgs, String> {
    let mut words = tail.split_whitespace();
    let (Some(name), Some(command)) = (words.next(), words.next()) else {
        return Err(
            "usage: /mcp add NAME COMMAND [ARGS] [--env NAME=value] [--timeout SECONDS]".to_owned(),
        );
    };
    let mut parsed = AddArgs {
        name: name.to_owned(),
        command: command.to_owned(),
        args: Vec::new(),
        vars: Vec::new(),
        timeout_secs: DEFAULT_TIMEOUT_SECS,
    };
    while let Some(word) = words.next() {
        match word {
            "--env" => {
                let pair = words.next().ok_or("--env needs NAME=value")?;
                let (key, value) = pair
                    .split_once('=')
                    .ok_or_else(|| format!("--env needs NAME=value, got '{pair}'"))?;
                parsed.vars.push((key.to_owned(), value.to_owned()));
            }
            "--timeout" => {
                let secs = words.next().ok_or("--timeout needs a number of seconds")?;
                parsed.timeout_secs = secs
                    .parse()
                    .map_err(|_| format!("--timeout needs a number of seconds, got '{secs}'"))?;
            }
            other => parsed.args.push(other.to_owned()),
        }
    }
    Ok(parsed)
}

pub fn command<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, rest: &str) {
    let style = term.borrow().style();
    let (word, tail) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, ""), |(word, tail)| (word, tail.trim()));
    match word {
        "" | "list" => list(agent, term),
        "config" if term.borrow().menus_enabled() => config_menu(agent, term),
        "config" => say_line(
            term,
            &style.yellow("/mcp config is a menu and needs a terminal; typed: /mcp add, remove, enable, disable, tools, rule"),
        ),
        "add" if tail.is_empty() && term.borrow().menus_enabled() => add_step_by_step(agent, term),
        "add" => add(agent, term, tail),
        "remove" | "enable" | "disable" | "tools" | "restart" if tail.is_empty() => {
            say_line(term, &style.yellow(&format!("usage: /mcp {word} NAME")));
        }
        "remove" => remove(agent, term, tail),
        "enable" => set_enabled(agent, term, tail, true),
        "disable" => set_enabled(agent, term, tail, false),
        "tools" => tools(agent, term, tail),
        "restart" => restart(term, tail),
        "toolset" | "toolsets" => toolset(agent, term, tail),
        "rule" => rule(agent, term, tail),
        _ => say_line(term, &style.yellow(USAGE)),
    }
}

fn list<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
    let style = term.borrow().style();
    let Some(servers) = with_store(agent, term, |s| s.list_mcp_servers()) else {
        return;
    };
    if servers.is_empty() {
        return say_line(
            term,
            &style
                .yellow("no MCP servers: /mcp add NAME COMMAND [ARGS], or /mcp config for a menu"),
        );
    }
    let mut text = format!(
        "{}\n",
        style.bold(&format!("mcp servers, {}:", servers.len()))
    );
    for server in &servers {
        let rules = with_store(agent, term, |s| s.mcp_rules(server.id)).unwrap_or_default();
        let mut line = format!(
            "  {}{}  {} {}",
            style.bold(&server.name),
            if server.enabled { "" } else { " (off)" },
            server.command,
            server.args.join(" ")
        );
        line = line.trim_end().to_owned();
        text.push_str(&line);
        text.push('\n');
        let mut facts = vec![
            state_of(term, server),
            format!("timeout {}s", server.timeout_secs),
            format!("other tools: {}", server.default_rule.name()),
        ];
        if !server.vars.is_empty() {
            let names: Vec<&str> = server.vars.iter().map(|(n, _)| n.as_str()).collect();
            facts.push(format!("variables: {}", names.join(", ")));
        }
        text.push_str(&format!("      {}\n", style.dim(&facts.join("; "))));
        for (tool, rule) in &rules {
            text.push_str(&format!(
                "      {}\n",
                style.dim(&format!("{tool}: {}", rule.name()))
            ));
        }
    }
    text.push_str(&style.dim("changes apply the next time the program starts"));
    text.push('\n');
    term.borrow_mut().say(&text);
}

fn add<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, tail: &str) {
    let given = match parse_add(tail) {
        Ok(given) => given,
        Err(why) => {
            let style = term.borrow().style();
            return say_line(term, &style.yellow(&why));
        }
    };
    save_server(agent, term, &given);
}

fn save_server<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, given: &AddArgs) {
    let saved = with_store(agent, term, |s| {
        s.add_mcp_server(
            &given.name,
            &given.command,
            &given.args,
            &given.vars,
            given.timeout_secs,
        )
    });
    if let Some(server) = saved {
        done(
            term,
            &format!(
                "added {}: every tool asks first; /mcp tools {} lists them, /mcp rule sets what each may do. This {NEXT_START}.",
                server.name, server.name
            ),
        );
    }
}

fn remove<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, name: &str) {
    match with_store(agent, term, |s| s.remove_mcp_server(name)) {
        Some(true) => done(term, &format!("removed {name}. This {NEXT_START}.")),
        Some(false) => not_found(term, name),
        None => {}
    }
}

fn set_enabled<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, name: &str, on: bool) {
    match with_store(agent, term, |s| s.set_mcp_enabled(name, on)) {
        Some(true) => done(
            term,
            &format!(
                "{} {name}. This {NEXT_START}.",
                if on { "enabled" } else { "disabled" }
            ),
        ),
        Some(false) => not_found(term, name),
        None => {}
    }
}

fn rule<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, tail: &str) {
    let style = term.borrow().style();
    let words: Vec<&str> = tail.split_whitespace().collect();
    let [name, tool, rule] = words[..] else {
        return say_line(
            term,
            &style.yellow("usage: /mcp rule NAME TOOL|* ask|free|off  (* is every tool with no rule of its own)"),
        );
    };
    let Some(parsed) = Rule::parse(rule) else {
        return say_line(
            term,
            &style.yellow(&format!("'{rule}' is not a rule: ask, free or off")),
        );
    };
    set_rule(agent, term, name, tool, parsed);
}

fn set_rule<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    name: &str,
    tool: &str,
    rule: Rule,
) {
    match with_store(agent, term, |s| s.set_mcp_rule(name, tool, rule)) {
        Some(true) => done(
            term,
            &format!(
                "{name} {}: {}. This {NEXT_START}.",
                if tool == ALL_TOOLS {
                    "other tools"
                } else {
                    tool
                },
                rule.name()
            ),
        ),
        Some(false) => not_found(term, name),
        None => {}
    }
}

/// `/mcp config` in a terminal: the servers, with "+ add new" at the end; Esc leaves.
fn config_menu<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
    let mut start = 0;
    loop {
        let Some(servers) = with_store(agent, term, |s| s.list_mcp_servers()) else {
            return;
        };
        let mut items: Vec<MenuItem> = servers
            .iter()
            .map(|s| {
                MenuItem::new(&s.name).with_detail(format!(
                    "{} · {} {} · other tools: {}",
                    if s.enabled { "on" } else { "off" },
                    s.command,
                    s.args.join(" "),
                    s.default_rule.name()
                ))
            })
            .collect();
        items.push(MenuItem::action("+ add new"));
        items.push(MenuItem::action("+ add a ready-made toolset"));
        let title = format!("mcp servers, {} (changes {NEXT_START})", servers.len());
        let Some(chosen) = term.borrow_mut().pick_from(&title, &items, start) else {
            return;
        };
        start = chosen;
        if chosen == servers.len() {
            add_step_by_step(agent, term);
            continue;
        }
        if chosen == servers.len() + 1 {
            pick_toolset(agent, term);
            continue;
        }
        server_menu(agent, term, &servers[chosen].name);
    }
}

/// `/mcp toolset`: the ready-made toolsets, and `/mcp toolset add NAME` to take one.
fn toolset<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, tail: &str) {
    let style = term.borrow().style();
    let (word, name) = tail
        .split_once(char::is_whitespace)
        .map_or((tail, ""), |(word, name)| (word, name.trim()));
    match word {
        "" | "list" => {
            let have = with_store(agent, term, |s| s.list_mcp_servers()).unwrap_or_default();
            say_line(term, &style.bold("ready-made MCP toolsets:"));
            for t in TOOLSETS {
                let state = if have.iter().any(|s| s.name == t.name) {
                    " (added)"
                } else {
                    ""
                };
                say_line(
                    term,
                    &format!(
                        "  {}{state}  {}",
                        style.bold(t.name),
                        style.dim(&format!("{}; {}", t.about, t.rules_text()))
                    ),
                );
            }
            say_line(term, &style.dim("/mcp toolset add NAME takes one"));
        }
        "add" if name.is_empty() => say_line(term, &style.yellow("usage: /mcp toolset add NAME")),
        "add" => match mcp_toolsets::find(name) {
            Some(found) => add_toolset(agent, term, found),
            None => say_line(
                term,
                &style.yellow(&format!("there is no toolset '{name}' (see /mcp toolset)")),
            ),
        },
        _ => say_line(term, &style.yellow("usage: /mcp toolset [add NAME]")),
    }
}

fn add_toolset<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, found: &Toolset) {
    let style = term.borrow().style();
    match with_store(agent, term, |s| {
        mcp_toolsets::install(s, found).map_err(crate::store::StoreError::new)
    }) {
        Some(Installed::Added) => {
            done(
                term,
                &format!(
                    "added {}: {}. This {NEXT_START}.",
                    found.name,
                    found.rules_text()
                ),
            );
            if !mcp_toolsets::on_path(found.command) {
                say_line(
                    term,
                    &style.yellow(&format!(
                        "{} was not found on your PATH: {}",
                        found.command, found.install_hint
                    )),
                );
            }
        }
        Some(Installed::AlreadyThere) => say_line(
            term,
            &style.yellow(&format!(
                "{} is already added; /mcp config changes its rules",
                found.name
            )),
        ),
        None => {}
    }
}

/// Asks which toolset to take.
fn pick_toolset<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
    let have = with_store(agent, term, |s| s.list_mcp_servers()).unwrap_or_default();
    let items: Vec<MenuItem> = TOOLSETS
        .iter()
        .map(|t| {
            MenuItem::new(t.name).with_detail(format!(
                "{}{}",
                if have.iter().any(|s| s.name == t.name) {
                    "(added) "
                } else {
                    ""
                },
                t.about
            ))
        })
        .collect();
    let picked = term
        .borrow_mut()
        .pick("ready-made toolsets (set up for you)", &items);
    if let Some(index) = picked {
        add_toolset(agent, term, &TOOLSETS[index]);
    }
}

/// Servers named on the command line (`--mcp`, `--mcp-add`), added before the chat starts. A
/// server that is already saved is left as it is. One line per request for the banner.
pub fn apply_startup(store: &mut Store, requests: &[McpRequest]) -> Vec<String> {
    let mut notes = Vec::new();
    for request in requests {
        let note = match request {
            McpRequest::Toolset(name) => match mcp_toolsets::find(name) {
                None => format!("[mcp: there is no toolset '{name}']"),
                Some(found) => match mcp_toolsets::install(store, found) {
                    Ok(Installed::Added) if mcp_toolsets::on_path(found.command) => {
                        format!("[mcp: added {}]", found.name)
                    }
                    Ok(Installed::Added) => format!(
                        "[mcp: added {}, but {} was not found on your PATH: {}]",
                        found.name, found.command, found.install_hint
                    ),
                    Ok(Installed::AlreadyThere) => {
                        format!("[mcp: {} was already added]", found.name)
                    }
                    Err(why) => format!("[mcp: could not add {name}: {why}]"),
                },
            },
            McpRequest::Add(words) => match parse_add(words) {
                Err(why) => format!("[mcp: could not add: {why}]"),
                Ok(given) => match store.mcp_server(&given.name) {
                    Ok(Some(_)) => format!("[mcp: {} was already added]", given.name),
                    Err(why) => format!("[mcp: could not add {}: {why}]", given.name),
                    Ok(None) => match store.add_mcp_server(
                        &given.name,
                        &given.command,
                        &given.args,
                        &given.vars,
                        given.timeout_secs,
                    ) {
                        Ok(server) => format!("[mcp: added {}]", server.name),
                        Err(why) => format!("[mcp: could not add {}: {why}]", given.name),
                    },
                },
            },
        };
        notes.push(note);
    }
    notes
}

/// "+ add new": one question at a time, each with an example, so nothing has to be remembered.
fn add_step_by_step<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
    let style = term.borrow().style();
    say_line(
        term,
        &style.dim("a new MCP server: a program this chat starts and talks to; Enter on an empty answer cancels the first two questions"),
    );
    let Some(name) = term.borrow_mut().ask("name", None) else {
        return;
    };
    if name.is_empty() {
        return;
    }
    let name = match check_name(&name) {
        Ok(name) => name,
        Err(why) => return error(term, &why),
    };
    let Some(command) = term.borrow_mut().ask("program to run", None) else {
        return;
    };
    if command.is_empty() {
        return;
    }
    let command = match check_command(&command) {
        Ok(command) => command,
        Err(why) => return error(term, &why),
    };
    let Some(args) = term
        .borrow_mut()
        .ask("arguments, separated by spaces (Enter for none)", None)
    else {
        return;
    };
    let mut vars: Vec<(String, String)> = Vec::new();
    loop {
        let question = if vars.is_empty() {
            "environment variable name, such as an API key (Enter for none)"
        } else {
            "another environment variable (Enter when done)"
        };
        let Some(var) = term.borrow_mut().ask(question, None) else {
            return;
        };
        if var.is_empty() {
            break;
        }
        if let Err(why) = check_var_name(&var) {
            error(term, &why);
            continue;
        }
        let Some(value) = term.borrow_mut().ask_secret(&format!("value of {var}")) else {
            return;
        };
        vars.push((var, value.trim().to_owned()));
    }
    let Some(secs) = term.borrow_mut().ask(
        "seconds one tool call may take",
        Some(&DEFAULT_TIMEOUT_SECS.to_string()),
    ) else {
        return;
    };
    let timeout_secs = if secs.is_empty() {
        DEFAULT_TIMEOUT_SECS
    } else {
        match secs.parse() {
            Ok(n) => n,
            Err(_) => return error(term, &format!("'{secs}' is not a number of seconds")),
        }
    };
    save_server(
        agent,
        term,
        &AddArgs {
            name,
            command,
            args: args.split_whitespace().map(str::to_owned).collect(),
            vars,
            timeout_secs,
        },
    );
}

/// What can be done with one server.
fn server_menu<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, name: &str) {
    loop {
        let Some(Some(server)) = with_store(agent, term, |s| s.mcp_server(name)) else {
            return;
        };
        let actions = [
            MenuItem::new(if server.enabled { "disable" } else { "enable" })
                .with_detail("start it with the chat, or not"),
            MenuItem::new("tool rules")
                .with_detail("ask, free or off for each tool (starts the server for a moment)"),
            MenuItem::new(format!("other tools: {}", server.default_rule.name()))
                .with_detail("the rule for tools with none of their own"),
            MenuItem::new("remove").with_detail("forget the server and its rules"),
        ];
        let picked = term.borrow_mut().pick(&server.name, &actions);
        match picked {
            Some(0) => set_enabled(agent, term, name, !server.enabled),
            Some(1) => rules_menu(agent, term, &server),
            Some(2) => {
                if let Some(rule) =
                    pick_rule(term, &format!("{name}: other tools"), server.default_rule)
                {
                    set_rule(agent, term, name, ALL_TOOLS, rule);
                }
            }
            Some(_) => {
                let sure = [
                    MenuItem::new("no"),
                    MenuItem::new("yes").with_detail(format!("remove {name} and its rules")),
                ];
                if term.borrow_mut().pick(&format!("remove {name}?"), &sure) == Some(1) {
                    remove(agent, term, name);
                    return;
                }
            }
            None => return,
        }
    }
}

/// Asks which rule a tool gets; the current one is highlighted first.
fn pick_rule<R: BufRead, W: Write>(
    term: &Shared<R, W>,
    title: &str,
    current: Rule,
) -> Option<Rule> {
    let items: Vec<MenuItem> = Rule::ALL
        .iter()
        .map(|rule| {
            MenuItem::new(rule.name()).with_detail(match rule {
                Rule::Ask => "ask before every call (\"always\" lasts until the program quits)",
                Rule::Free => "run without asking",
                Rule::Off => "not given to the model at all",
            })
        })
        .collect();
    let start = Rule::ALL.iter().position(|r| *r == current).unwrap_or(0);
    let chosen = term.borrow_mut().pick_from(title, &items, start)?;
    Rule::ALL.get(chosen).copied()
}

/// The tools the server offers, each with its rule; pick one to change it.
fn rules_menu<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, server: &McpServer) {
    let style = term.borrow().style();
    let Ok(cwd) = std::env::current_dir() else {
        return error(term, "no working directory");
    };
    say_line(term, &style.dim(&format!("starting {}...", server.name)));
    let (client, defs) = match McpClient::start(server, &cwd, Cancel::new()) {
        Ok(started) => started,
        Err(why) => return error(term, &format!("{} did not start: {why}", server.name)),
    };
    let mut start = 0;
    while let Some((current, rules)) = rules_of(agent, term, server) {
        let items: Vec<MenuItem> = defs
            .iter()
            .map(|def| {
                let first = def.description.lines().next().unwrap_or("");
                MenuItem::new(&def.name).with_detail(format!(
                    "[{}] {}",
                    rule_of(&current, &rules, &def.name).name(),
                    first.chars().take(SUMMARY_CHARS).collect::<String>()
                ))
            })
            .collect();
        let title = format!(
            "{}: {} tools (changes {NEXT_START})",
            server.name,
            defs.len()
        );
        let Some(chosen) = term.borrow_mut().pick_from(&title, &items, start) else {
            break;
        };
        start = chosen;
        let tool = &defs[chosen].name;
        let now = rule_of(&current, &rules, tool);
        if let Some(rule) = pick_rule(term, &format!("{}.{tool}", server.name), now) {
            set_rule(agent, term, &server.name, tool, rule);
        }
    }
    client.shutdown();
}

/// Starts the server for a moment, only to list what it offers and what each tool's rule is.
fn tools<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, name: &str) {
    let style = term.borrow().style();
    let Some(found) = with_store(agent, term, |s| s.mcp_server(name)) else {
        return;
    };
    let Some(server) = found else {
        return not_found(term, name);
    };
    let rules = with_store(agent, term, |s| s.mcp_rules(server.id)).unwrap_or_default();
    let Ok(cwd) = std::env::current_dir() else {
        return error(term, "no working directory");
    };
    say_line(term, &style.dim(&format!("starting {}...", server.name)));
    match McpClient::start(&server, &cwd, Cancel::new()) {
        Ok((client, defs)) => {
            say_line(
                term,
                &style.bold(&format!("{} offers {} tools:", server.name, defs.len())),
            );
            for def in &defs {
                say_line(term, &tool_line(&server, &rules, def, &style));
            }
            say_line(term, &style.dim(&cost_line(&server, &rules, &defs)));
            client.shutdown();
        }
        Err(why) => error(term, &format!("{} did not start: {why}", server.name)),
    }
}

fn tool_line(
    server: &McpServer,
    rules: &[(String, Rule)],
    def: &crate::mcp::ToolDef,
    style: &crate::style::Style,
) -> String {
    let first = def.description.lines().next().unwrap_or("");
    let summary: String = if first.chars().count() > SUMMARY_CHARS {
        format!(
            "{}...",
            first.chars().take(SUMMARY_CHARS).collect::<String>()
        )
    } else {
        first.to_owned()
    };
    format!(
        "  {} [{}] ~{} tokens  {}",
        style.bold(&def.name),
        rule_of(server, rules, &def.name).name(),
        def.tokens(&server.name),
        style.dim(&summary)
    )
}

/// What the tools cost on every model call: the ones that are on, and all of them.
pub fn cost_line(
    server: &McpServer,
    rules: &[(String, Rule)],
    defs: &[crate::mcp::ToolDef],
) -> String {
    let (mut on, mut on_tokens, mut all_tokens) = (0, 0, 0);
    for def in defs {
        let tokens = def.tokens(&server.name);
        all_tokens += tokens;
        if rule_of(server, rules, &def.name) != Rule::Off {
            on += 1;
            on_tokens += tokens;
        }
    }
    format!(
        "on now: {on} of {} tools, about {on_tokens} tokens added to every model call (all of them: about {all_tokens})",
        defs.len()
    )
}

/// Whether the server is running in this chat, for the list.
fn state_of<R: BufRead, W: Write>(term: &Shared<R, W>, server: &McpServer) -> String {
    let client = term.borrow().mcp_client(&server.name);
    match client {
        Some(client) => match client.stopped() {
            None => "running in this chat".to_owned(),
            Some(why) => format!("stopped: {why} (/mcp restart {})", server.name),
        },
        None if server.enabled => "not started in this chat".to_owned(),
        None => "not started".to_owned(),
    }
}

/// `/mcp restart NAME`: starts a server of this chat again, in place. Its tools are the ones the
/// model already has; nothing about the tool list changes.
fn restart<R: BufRead, W: Write>(term: &Shared<R, W>, name: &str) {
    let style = term.borrow().style();
    let client = term.borrow().mcp_client(name);
    let Some(client) = client else {
        return say_line(
            term,
            &style.yellow(&format!(
                "{name} is not running in this chat, so there is nothing to restart; a server added or enabled since the program started begins at the next start"
            )),
        );
    };
    say_line(term, &style.dim(&format!("restarting {name}...")));
    match client.restart() {
        Ok(defs) => done(
            term,
            &format!(
                "{name} is running again ({} tools). The tools the model has are the ones from the start of this chat.",
                defs.len()
            ),
        ),
        Err(why) => error(term, &format!("{name} did not start again: {why}")),
    }
}

fn not_found<R: BufRead, W: Write>(term: &Shared<R, W>, name: &str) {
    let style = term.borrow().style();
    say_line(
        term,
        &style.yellow(&format!("there is no MCP server '{name}' (see /mcp)")),
    );
}

/// The server as saved now and the rules of its tools, read fresh after every change.
fn rules_of<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    server: &McpServer,
) -> Option<(McpServer, Vec<(String, Rule)>)> {
    let rules = with_store(agent, term, |s| s.mcp_rules(server.id))?;
    let current = with_store(agent, term, |s| s.mcp_server(&server.name))??;
    Some((current, rules))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{ScriptedProvider, StoreContext};
    use crate::repl::Terminal;
    use crate::store::Store;
    use crate::style::Style;
    use crate::tools::testutil::Scratch;
    use crate::turn_log::TurnLog;
    use std::io::Cursor;

    fn run(dir: &Scratch, lines: &[&str]) -> String {
        let term = Terminal::shared_styled(Cursor::new(Vec::new()), Vec::new(), Style::plain());
        term.borrow_mut().set_root(dir.path().to_path_buf());
        let log = TurnLog::create(&dir.path().join("logs")).unwrap();
        let store = Store::open(&dir.path().join("harness.db")).unwrap();
        let mut agent = Agent::new(
            "m",
            Box::new(ScriptedProvider::new(vec![])),
            Vec::new(),
            log,
        )
        .with_target("test", "http://x/v1")
        .with_store(
            store,
            StoreContext {
                cwd: dir.path().display().to_string(),
            },
        );
        for line in lines {
            command(&mut agent, &term, line);
        }
        String::from_utf8(term.borrow().output().clone()).unwrap()
    }

    fn store(dir: &Scratch) -> Store {
        Store::open(&dir.path().join("harness.db")).unwrap()
    }

    const DOWN: &str = "\x1b[B";
    const ENTER: &str = "\r";
    const ESC: &str = "\x1b";

    fn run_menu(dir: &Scratch, typed: &str) -> String {
        let term = Terminal::shared_styled(
            Cursor::new(typed.as_bytes().to_vec()),
            Vec::new(),
            Style::plain(),
        );
        term.borrow_mut().set_root(dir.path().to_path_buf());
        term.borrow_mut().enable_menus(false);
        let log = TurnLog::create(&dir.path().join("logs")).unwrap();
        let store = Store::open(&dir.path().join("harness.db")).unwrap();
        let mut agent = Agent::new(
            "m",
            Box::new(ScriptedProvider::new(vec![])),
            Vec::new(),
            log,
        )
        .with_target("test", "http://x/v1")
        .with_store(
            store,
            StoreContext {
                cwd: dir.path().display().to_string(),
            },
        );
        command(&mut agent, &term, "config");
        String::from_utf8(term.borrow().output().clone()).unwrap()
    }

    fn with_server(dir: &Scratch) {
        store(dir)
            .add_mcp_server("docs", "prog", &[], &[], 30)
            .unwrap();
    }

    #[test]
    fn the_menu_adds_a_server_one_question_at_a_time() {
        let dir = Scratch::new("mcp-menu-add");
        // Name, program, arguments, one variable and its value, no more variables, a timeout.
        let answers = "docs\nprog\n--stdio x\nMY_KEY\nhunter2\n\n45\n";
        let screen = run_menu(&dir, &format!("{ENTER}{answers}{ESC}"));
        assert!(screen.contains("added docs"), "{screen}");
        assert!(
            !screen.contains("hunter2"),
            "a typed value is never shown again"
        );
        let saved = store(&dir).mcp_server("docs").unwrap().unwrap();
        assert_eq!(saved.command, "prog");
        assert_eq!(saved.args, ["--stdio", "x"]);
        assert_eq!(saved.vars, [("MY_KEY".to_owned(), "hunter2".to_owned())]);
        assert_eq!(saved.timeout_secs, 45);
    }

    #[test]
    fn the_questions_take_nothing_but_a_name_and_a_program() {
        let dir = Scratch::new("mcp-menu-add-min");
        // Enter on the arguments, variables and timeout questions keeps the defaults.
        let screen = run_menu(&dir, &format!("{ENTER}docs\nprog\n\n\n\n{ESC}"));
        assert!(screen.contains("added docs"), "{screen}");
        let saved = store(&dir).mcp_server("docs").unwrap().unwrap();
        assert!(saved.args.is_empty() && saved.vars.is_empty());
        assert_eq!(saved.timeout_secs, DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn add_alone_in_a_terminal_asks_the_questions() {
        let dir = Scratch::new("mcp-add-alone");
        let term = Terminal::shared_styled(
            Cursor::new(b"docs\nprog\n\n\n\n".to_vec()),
            Vec::new(),
            Style::plain(),
        );
        term.borrow_mut().set_root(dir.path().to_path_buf());
        term.borrow_mut().enable_menus(false);
        let log = TurnLog::create(&dir.path().join("logs")).unwrap();
        let store_ = Store::open(&dir.path().join("harness.db")).unwrap();
        let mut agent = Agent::new(
            "m",
            Box::new(ScriptedProvider::new(vec![])),
            Vec::new(),
            log,
        )
        .with_target("test", "http://x/v1")
        .with_store(
            store_,
            StoreContext {
                cwd: dir.path().display().to_string(),
            },
        );
        command(&mut agent, &term, "add");
        assert!(store(&dir).mcp_server("docs").unwrap().is_some());
    }

    #[test]
    fn an_empty_name_or_a_bad_one_stops_the_questions() {
        let dir = Scratch::new("mcp-menu-add-bad");
        let screen = run_menu(&dir, &format!("{ENTER}\n{ESC}"));
        assert!(!screen.contains("added"), "{screen}");
        let screen = run_menu(&dir, &format!("{ENTER}Bad Name\nprog\n{ESC}"));
        assert!(screen.contains("[error:"), "{screen}");
        assert!(store(&dir).list_mcp_servers().unwrap().is_empty());
    }

    #[test]
    fn the_menu_toggles_a_server_and_sets_the_rule_of_the_other_tools() {
        let dir = Scratch::new("mcp-menu-rules");
        with_server(&dir);
        // Pick docs, "disable", then "other tools" and move from ask to free, then leave twice.
        let typed = format!("{ENTER}{ENTER}{DOWN}{DOWN}{ENTER}{DOWN}{ENTER}{ESC}{ESC}");
        let screen = run_menu(&dir, &typed);
        assert!(screen.contains("disabled docs"), "{screen}");
        assert!(screen.contains("docs other tools: free"), "{screen}");
        let saved = store(&dir).mcp_server("docs").unwrap().unwrap();
        assert!(!saved.enabled);
        assert_eq!(saved.default_rule, Rule::Free);
    }

    #[test]
    fn the_menu_removes_a_server_only_after_a_yes() {
        let dir = Scratch::new("mcp-menu-remove");
        with_server(&dir);
        // "remove", then "no": the server stays.
        let typed = format!("{ENTER}{DOWN}{DOWN}{DOWN}{ENTER}{ENTER}{ESC}{ESC}");
        run_menu(&dir, &typed);
        assert!(store(&dir).mcp_server("docs").unwrap().is_some());
        // "remove", then "yes".
        let typed = format!("{ENTER}{DOWN}{DOWN}{DOWN}{ENTER}{DOWN}{ENTER}{ESC}");
        let screen = run_menu(&dir, &typed);
        assert!(screen.contains("removed docs"), "{screen}");
        assert!(store(&dir).mcp_server("docs").unwrap().is_none());
    }

    #[test]
    fn config_without_a_terminal_points_to_the_typed_forms() {
        let dir = Scratch::new("mcp-config-plain");
        let screen = run(&dir, &["config"]);
        assert!(screen.contains("needs a terminal"), "{screen}");
    }

    #[test]
    fn a_ready_made_toolset_is_listed_and_added_with_its_rules() {
        let dir = Scratch::new("mcp-toolset");
        let screen = run(
            &dir,
            &[
                "toolset",
                "toolset add rust-docs",
                "toolset add rust-docs",
                "toolset add nothing",
                "toolset",
            ],
        );
        assert!(
            screen.contains("added rust-docs: 5 tools run without asking"),
            "{screen}"
        );
        assert!(screen.contains("rust-docs is already added"), "{screen}");
        assert!(screen.contains("there is no toolset 'nothing'"), "{screen}");
        assert!(screen.contains("rust-docs (added)"), "{screen}");
        let store = store(&dir);
        let server = store.mcp_server("rust-docs").unwrap().unwrap();
        assert_eq!(server.default_rule, Rule::Off);
        assert_eq!(store.mcp_rules(server.id).unwrap().len(), 8);
    }

    #[test]
    fn the_menu_offers_the_ready_made_toolsets() {
        let dir = Scratch::new("mcp-menu-toolset");
        // "+ add new" is first, the toolsets are below it; take the first toolset.
        let screen = run_menu(&dir, &format!("{DOWN}{ENTER}{ENTER}{ESC}"));
        assert!(screen.contains("added rust-docs"), "{screen}");
        assert!(store(&dir).mcp_server("rust-docs").unwrap().is_some());
    }

    #[test]
    fn servers_named_on_the_command_line_are_added_once_and_reported() {
        let dir = Scratch::new("mcp-startup");
        let mut store = Store::open(&dir.path().join("harness.db")).unwrap();
        let requests = [
            McpRequest::Toolset("rust-docs".to_owned()),
            McpRequest::Add("mine prog --stdio --env K=v".to_owned()),
            McpRequest::Toolset("rust-docs".to_owned()),
            McpRequest::Add("mine other".to_owned()),
            McpRequest::Toolset("nothing".to_owned()),
            McpRequest::Add("Bad Name prog".to_owned()),
            McpRequest::Add("onlyname".to_owned()),
        ];
        let notes = apply_startup(&mut store, &requests);
        assert!(notes[0].starts_with("[mcp: added rust-docs"), "{notes:?}");
        assert_eq!(notes[1], "[mcp: added mine]");
        assert_eq!(notes[2], "[mcp: rust-docs was already added]");
        assert_eq!(notes[3], "[mcp: mine was already added]");
        assert!(notes[4].contains("no toolset 'nothing'"), "{notes:?}");
        assert!(notes[5].contains("could not add"), "{notes:?}");
        assert!(notes[6].contains("could not add"), "{notes:?}");
        let mine = store.mcp_server("mine").unwrap().unwrap();
        assert_eq!(mine.command, "prog", "the second `mine` changed nothing");
        assert_eq!(mine.vars, [("K".to_owned(), "v".to_owned())]);
    }

    #[test]
    fn add_reads_arguments_variables_and_a_timeout_in_any_order() {
        let got = parse_add("docs prog --stdio --env A=1 x --timeout 30 --env B=two=2").unwrap();
        assert_eq!(
            got,
            AddArgs {
                name: "docs".to_owned(),
                command: "prog".to_owned(),
                args: vec!["--stdio".to_owned(), "x".to_owned()],
                vars: vec![
                    ("A".to_owned(), "1".to_owned()),
                    ("B".to_owned(), "two=2".to_owned())
                ],
                timeout_secs: 30,
            }
        );
        assert_eq!(parse_add("a b").unwrap().timeout_secs, DEFAULT_TIMEOUT_SECS);
        for bad in [
            "",
            "onlyname",
            "a b --env",
            "a b --env NOEQUALS",
            "a b --timeout x",
        ] {
            assert!(parse_add(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_server_is_added_listed_ruled_disabled_and_removed_and_each_change_says_when_it_applies() {
        let dir = Scratch::new("mcp-ui");
        let screen = run(
            &dir,
            &[
                "add docs rust-docs-mcp --env API_KEY=hunter2 --timeout 30",
                "rule docs search free",
                "rule docs * off",
                "disable docs",
                "",
            ],
        );
        assert_eq!(screen.matches("next time the program starts").count(), 5);
        assert!(screen.contains("docs (off)  rust-docs-mcp"), "{screen}");
        assert!(
            screen.contains("timeout 30s; other tools: off; variables: API_KEY"),
            "{screen}"
        );
        assert!(screen.contains("search: free"), "{screen}");
        assert!(
            !screen.contains("hunter2"),
            "a variable's value is never shown"
        );

        let saved = store(&dir).mcp_server("docs").unwrap().unwrap();
        assert!(!saved.enabled);
        assert_eq!(saved.default_rule, Rule::Off);
        assert_eq!(saved.vars, [("API_KEY".to_owned(), "hunter2".to_owned())]);

        let after = run(&dir, &["remove docs", "remove docs", ""]);
        assert!(after.contains("removed docs"), "{after}");
        assert!(after.contains("there is no MCP server 'docs'"), "{after}");
        assert!(after.contains("no MCP servers"), "{after}");
    }

    #[test]
    fn mistakes_are_explained_and_nothing_is_saved() {
        let dir = Scratch::new("mcp-ui-bad");
        let screen = run(
            &dir,
            &[
                "add docs",
                "add bad/name prog",
                "rule docs x maybe",
                "rule ghost x free",
                "remove",
                "frobnicate",
            ],
        );
        assert!(screen.contains("usage: /mcp add NAME COMMAND"), "{screen}");
        assert!(screen.contains("[error:"), "{screen}");
        assert!(screen.contains("'maybe' is not a rule"), "{screen}");
        assert!(
            screen.contains("there is no MCP server 'ghost'"),
            "{screen}"
        );
        assert!(screen.contains("usage: /mcp remove NAME"), "{screen}");
        assert!(screen.contains("usage: /mcp [config | add"), "{screen}");
        assert!(store(&dir).list_mcp_servers().unwrap().is_empty());
    }
}

//! `/prompts`, `/skills config` and `/personality config`: the screens that add, edit, reorder and
//! delete the prompts the owner keeps. A prompt has one type, chosen by where it is added
//! (issue 14): a `system` prompt goes through `/prompts`, a `skill` through `/skills`, a
//! `personality` through `/personality`, and each command only lists and changes its own kind.
//!
//! System prompts (issue 13): the ones switched on are joined into the system prompt when a chat
//! starts (`Agent::load_prompt`, `Agent::new_chat`); nothing here changes a chat that is already
//! running.
//!
//! - `/prompts` in a terminal: a checklist, Enter switches a system prompt on or off.
//! - `/prompts config`, `/skills config`, `/personality config` (terminal only): the manager.
//! - typed commands, terminal or not: `add`, `edit`, `delete` for every kind; `on`, `off`, `show`
//!   for system prompts. Skills and personalities are never "on" here: they are chosen per chat
//!   with `/skills` and `/personality`, and a new chat starts without any.

use std::io::{BufRead, Write};

use crate::agent::Agent;
use crate::config_ui::{done, error};
use crate::menu::MenuItem;
use crate::repl::{Shared, say_line};
use crate::store::{Store, StoreError};
use crate::store_prompts::{
    BASE_DESCRIPTION, BASE_NAME, BUILTIN_PROMPT, MAX_PROMPT_BYTES, Prompt, PromptType,
    check_prompt_name, check_prompt_text, short_hash, size_label,
};

/// Said after every change: the running chat keeps the prompt it started with.
const NEXT_CHAT: &str = "applies to the next chat: /new starts one";

/// Runs `f` on the store, reporting a failure on screen. `None` after a failure.
fn with_store<R: BufRead, W: Write, T>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    f: impl FnOnce(&mut Store) -> Result<T, StoreError>,
) -> Option<T> {
    match agent
        .store_mut()
        .and_then(|s| f(s).map_err(|e| e.to_string()))
    {
        Ok(value) => Some(value),
        Err(why) => {
            error(term, &why);
            None
        }
    }
}

/// The prompts of one kind, in order.
fn prompts_of<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    kind: PromptType,
) -> Option<Vec<Prompt>> {
    with_store(agent, term, |s| s.list_prompts())
        .map(|all| all.into_iter().filter(|p| p.prompt_type == kind).collect())
}

fn noun(kind: PromptType) -> &'static str {
    match kind {
        PromptType::System => "system prompt",
        PromptType::Skill => "skill",
        PromptType::Personality => "personality",
    }
}

fn plural(kind: PromptType) -> &'static str {
    match kind {
        PromptType::System => "system prompts",
        PromptType::Skill => "skills",
        PromptType::Personality => "personalities",
    }
}

/// The command that manages a kind.
fn command_of(kind: PromptType) -> &'static str {
    match kind {
        PromptType::System => "/prompts",
        PromptType::Skill => "/skills",
        PromptType::Personality => "/personality",
    }
}

/// When a change shows.
fn applies(kind: PromptType) -> &'static str {
    match kind {
        PromptType::System => NEXT_CHAT,
        _ => "used from the next message that uses it",
    }
}

/// The prompt called `name`, if it is of this kind; otherwise says which command manages it.
fn require<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    name: &str,
    kind: PromptType,
) -> Option<Prompt> {
    let prompt = with_store(agent, term, |s| s.prompt(name))?;
    if prompt.prompt_type != kind {
        error(
            term,
            &format!(
                "{name} is a {}: manage it with {}",
                noun(prompt.prompt_type),
                command_of(prompt.prompt_type)
            ),
        );
        return None;
    }
    Some(prompt)
}

/// Menu rows; only system prompts have an on/off mark.
fn rows(prompts: &[Prompt], kind: PromptType) -> Vec<MenuItem> {
    prompts
        .iter()
        .map(|p| {
            let label = if kind == PromptType::System {
                format!("[{}] {}", if p.enabled { "x" } else { " " }, p.name)
            } else {
                p.name.clone()
            };
            let mut detail = size_label(p.chars());
            if !p.description.is_empty() {
                detail.push_str("  ");
                detail.push_str(&p.description);
            }
            MenuItem::new(label).with_detail(detail)
        })
        .collect()
}

fn yes(answer: Option<String>) -> bool {
    matches!(
        answer.as_deref().map(str::to_ascii_lowercase).as_deref(),
        Some("y" | "yes")
    )
}

/// What a new chat would start with.
fn say_preview<R: BufRead, W: Write>(agent: &Agent, term: &Shared<R, W>) {
    let style = term.borrow().style();
    match agent.prompt_preview() {
        Ok(info) => say_line(
            term,
            &style.dim(&format!("a new chat would start with {}", info.summary())),
        ),
        Err(why) => error(term, &why),
    }
}

/// Reads a prompt file the program was pointed at: UTF-8, not larger than the limit. Returns the
/// text and the file's absolute path.
fn read_prompt_file(path: &str) -> Result<(String, String), String> {
    let path = path.trim();
    let meta = std::fs::metadata(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{path} is not a file"));
    }
    if meta.len() > MAX_PROMPT_BYTES as u64 {
        return Err(format!(
            "{path} is {} bytes; the limit is {MAX_PROMPT_BYTES}",
            meta.len()
        ));
    }
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("cannot read {path} as text: {e}"))?;
    let absolute =
        std::fs::canonicalize(path).map_or_else(|_| path.to_owned(), |p| p.display().to_string());
    Ok((text, absolute))
}

// ---- typed commands ----

/// The plain list of system prompts: one line per prompt.
fn show_list<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
    let style = term.borrow().style();
    let Some(all) = prompts_of(agent, term, PromptType::System) else {
        return;
    };
    if all.is_empty() {
        return say_line(
            term,
            &style.yellow("no system prompts: add one with /prompts add <name>"),
        );
    }
    let width = all
        .iter()
        .map(|p| p.name.chars().count())
        .max()
        .unwrap_or(0);
    let mut text = format!(
        "{}\n",
        style.bold("system prompts (changes apply to the next chat):")
    );
    for p in &all {
        let state = if p.enabled {
            style.green("on ")
        } else {
            style.dim("off")
        };
        text.push_str(&format!(
            "  {state}  {:<width$}  {:>11}  {}\n",
            p.name,
            size_label(p.chars()),
            style.dim(&p.description)
        ));
    }
    term.borrow_mut().say(&text);
    say_preview(agent, term);
}

/// `/prompts show`: the text a new chat would start with.
fn show_assembled<R: BufRead, W: Write>(agent: &Agent, term: &Shared<R, W>) {
    let style = term.borrow().style();
    match agent.assembled_prompt() {
        Err(why) => error(term, &why),
        Ok(assembled) => {
            let header = format!(
                "system prompt of a new chat: {} ({}), hash {}",
                size_label(assembled.chars()),
                if assembled.fallback {
                    "the built-in text, no prompt is switched on".to_owned()
                } else {
                    assembled.names.join(", ")
                },
                short_hash(&assembled.text)
            );
            say_line(
                term,
                &format!(
                    "{}\n{}\n{}",
                    style.bold(&header),
                    assembled.text,
                    style.dim("(end)")
                ),
            );
        }
    }
}

fn set_enabled<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    name: &str,
    enabled: bool,
) {
    if require(agent, term, name, PromptType::System).is_none() {
        return;
    }
    if with_store(agent, term, |s| s.set_prompt_enabled(name, enabled)).is_some() {
        done(
            term,
            &format!(
                "{name} is {}; {NEXT_CHAT}",
                if enabled { "on" } else { "off" }
            ),
        );
        say_preview(agent, term);
    }
}

fn delete_prompt<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    name: &str,
    kind: PromptType,
) {
    let style = term.borrow().style();
    if require(agent, term, name, kind).is_none() {
        return;
    }
    let answer = term
        .borrow_mut()
        .ask(&format!("delete the {} {name}? [y/N]", noun(kind)), None);
    if !yes(answer) {
        return say_line(term, &style.yellow("[not deleted]"));
    }
    if with_store(agent, term, |s| s.delete_prompt(name)).is_some() {
        agent.refresh_chat_prompts();
        done(term, &format!("deleted {name}; {}", applies(kind)));
    }
}

/// The text of a prompt from the owner: the editor or typed lines.
fn text_from_owner<R: BufRead, W: Write>(
    term: &Shared<R, W>,
    what: &str,
    initial: &str,
) -> Option<String> {
    let read = term.borrow_mut().read_text(what, initial);
    match read {
        Ok(text) => text,
        Err(why) => {
            error(term, &why);
            None
        }
    }
}

/// What to do next with a prompt that was just saved.
fn after_save(kind: PromptType, name: &str) -> String {
    match kind {
        PromptType::System => format!("/prompts on {name} switches it on"),
        PromptType::Skill => format!(
            "/skills use {name} uses it for one message, /skills on {name} keeps it on in a chat"
        ),
        PromptType::Personality => format!("/personality {name} selects it"),
    }
}

fn add_typed<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    tail: &str,
    kind: PromptType,
) {
    let style = term.borrow().style();
    let tail = tail.trim();
    let (name, file) = match tail.split_once(char::is_whitespace) {
        Some((name, file)) => (name.to_owned(), Some(file.trim().to_owned())),
        None if !tail.is_empty() => (tail.to_owned(), None),
        None => {
            let asked = term.borrow_mut().ask("name (empty cancels)", None);
            match asked.filter(|n| !n.is_empty()) {
                Some(name) => (name, None),
                None => return say_line(term, &style.yellow("[cancelled: nothing saved]")),
            }
        }
    };
    if let Err(why) = check_prompt_name(&name) {
        return error(term, &why);
    }
    let (text, source) = match file {
        Some(path) => match read_prompt_file(&path) {
            Ok((text, absolute)) => (text, Some(absolute)),
            Err(why) => return error(term, &why),
        },
        None => match text_from_owner(term, &name, "") {
            Some(text) => (text, None),
            None => return say_line(term, &style.yellow("[cancelled: nothing saved]")),
        },
    };
    let saved = with_store(agent, term, |s| {
        s.add_prompt_of_type(&name, "", &text, source.as_deref(), kind)
    });
    if saved.is_some() {
        let tail = if kind == PromptType::System {
            format!("switched off: {}", after_save(kind, &name))
        } else {
            after_save(kind, &name)
        };
        done(
            term,
            &format!(
                "saved {} {name} ({}), {tail}",
                noun(kind),
                size_label(text.chars().count())
            ),
        );
    }
}

fn edit_typed<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    name: &str,
    kind: PromptType,
) {
    let style = term.borrow().style();
    let Some(current) = require(agent, term, name, kind) else {
        return;
    };
    let Some(text) = text_from_owner(term, name, &current.text) else {
        return say_line(term, &style.yellow("[cancelled: nothing changed]"));
    };
    if with_store(agent, term, |s| s.set_prompt_text(name, &text, None)).is_some() {
        done(term, &format!("saved {name}; {}", applies(kind)));
    }
}

fn one_name(tail: &str) -> Option<String> {
    let mut words = tail.split_whitespace();
    match (words.next(), words.next()) {
        (Some(name), None) => Some(name.to_owned()),
        _ => None,
    }
}

/// The commands every kind has: `config`, `add`, `edit`, `delete`. `false` when `word` is none
/// of them, so the caller can try its own.
pub fn manage<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    kind: PromptType,
    word: &str,
    tail: &str,
) -> bool {
    let style = term.borrow().style();
    let menus = term.borrow().menus_enabled();
    let command = command_of(kind);
    match word {
        "config" if menus => manager(agent, term, kind),
        "config" => say_line(
            term,
            &style.yellow(&format!(
                "{command} config needs a terminal: it is an arrow-key menu"
            )),
        ),
        "add" => add_typed(agent, term, tail, kind),
        "edit" | "delete" => match one_name(tail) {
            None => say_line(
                term,
                &style.yellow(&format!("usage: {command} {word} <name>")),
            ),
            Some(name) if word == "edit" => edit_typed(agent, term, &name, kind),
            Some(name) => delete_prompt(agent, term, &name, kind),
        },
        _ => return false,
    }
    true
}

/// `/prompts ...`.
pub fn command<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, rest: &str) {
    let style = term.borrow().style();
    let menus = term.borrow().menus_enabled();
    let (word, tail) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, ""), |(word, tail)| (word, tail.trim()));
    match word {
        "" if menus => checklist(agent, term),
        "" => show_list(agent, term),
        "show" => show_assembled(agent, term),
        "on" | "off" => match one_name(tail) {
            None => say_line(term, &style.yellow(&format!("usage: /prompts {word} <name>"))),
            Some(name) => set_enabled(agent, term, &name, word == "on"),
        },
        _ if manage(agent, term, PromptType::System, word, tail) => {}
        _ => say_line(
            term,
            &style.yellow(
                "usage: /prompts [config | show | on <name> | off <name> | add [name [file]] | edit <name> | delete <name>]",
            ),
        ),
    }
}

// ---- menus ----

/// `/prompts` in a terminal: Enter switches the highlighted system prompt on or off; Esc finishes.
pub fn checklist<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
    let style = term.borrow().style();
    let mut start = 0;
    let mut changed = false;
    loop {
        let Some(all) = prompts_of(agent, term, PromptType::System) else {
            return;
        };
        if all.is_empty() {
            return say_line(
                term,
                &style.yellow("no system prompts: use /prompts config, then + add new"),
            );
        }
        let chosen = term.borrow_mut().pick_from(
            "system prompts: Enter switches on or off, Esc finishes",
            &rows(&all, PromptType::System),
            start,
        );
        let Some(index) = chosen else {
            break;
        };
        let prompt = &all[index];
        if with_store(agent, term, |s| {
            s.set_prompt_enabled(&prompt.name, !prompt.enabled)
        })
        .is_none()
        {
            return;
        }
        changed = true;
        start = index;
    }
    if changed {
        say_line(term, &style.green(NEXT_CHAT));
        say_preview(agent, term);
    }
}

/// `/prompts config`, `/skills config`, `/personality config`: the manager of one kind.
fn manager<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, kind: PromptType) {
    loop {
        let Some(all) = prompts_of(agent, term, kind) else {
            return;
        };
        let base_missing = kind == PromptType::System
            && with_store(agent, term, |s| Ok(s.prompt(BASE_NAME).is_err())) == Some(true);
        let mut items = rows(&all, kind);
        items.push(MenuItem::action("+ add new"));
        if base_missing {
            items.push(MenuItem::action("+ restore built-in base"));
        }
        let chosen = term.borrow_mut().pick(plural(kind), &items);
        match chosen {
            None => return,
            Some(i) if i == all.len() => add_new(agent, term, kind),
            Some(i) if i == all.len() + 1 => {
                let restored = with_store(agent, term, |s| {
                    s.add_prompt(BASE_NAME, BASE_DESCRIPTION, BUILTIN_PROMPT, None)?;
                    s.set_prompt_enabled(BASE_NAME, true)
                });
                if restored.is_some() {
                    done(term, &format!("restored {BASE_NAME}; {NEXT_CHAT}"));
                }
            }
            Some(i) => prompt_screen(agent, term, &all[i].name, kind),
        }
    }
}

#[derive(Clone, Copy)]
enum Action {
    Toggle,
    View,
    Edit,
    Up,
    Down,
    Rename,
    Reload,
    Restore,
    Delete,
}

fn prompt_screen<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    name: &str,
    kind: PromptType,
) {
    let style = term.borrow().style();
    let mut name = name.to_owned();
    let (earlier, later) = match kind {
        PromptType::System => ("earlier in the system prompt", "later in the system prompt"),
        _ => ("earlier in the list", "later in the list"),
    };
    loop {
        let Some(p) = with_store(agent, term, |s| s.prompt(&name)) else {
            return;
        };
        let mut actions = Vec::new();
        if kind == PromptType::System {
            actions.push((
                if p.enabled { "switch off" } else { "switch on" },
                "whether new chats start with it",
                Action::Toggle,
            ));
        }
        actions.extend([
            ("view", "show the whole text", Action::View),
            ("edit", "change the text", Action::Edit),
            ("move up", earlier, Action::Up),
            ("move down", later, Action::Down),
            ("rename", "", Action::Rename),
        ]);
        if p.source_path.is_some() {
            actions.push(("reload from file", "read the file again", Action::Reload));
        }
        if p.name == BASE_NAME && kind == PromptType::System {
            actions.push((
                "restore built-in text",
                "put the program's own text back",
                Action::Restore,
            ));
        }
        actions.push(("delete", "remove it for good", Action::Delete));
        let items: Vec<MenuItem> = actions
            .iter()
            .map(|(label, detail, _)| MenuItem::new(*label).with_detail(*detail))
            .collect();
        let title = if kind == PromptType::System {
            format!(
                "{} ({}, {})",
                p.name,
                if p.enabled { "on" } else { "off" },
                size_label(p.chars())
            )
        } else {
            format!("{} ({})", p.name, size_label(p.chars()))
        };
        let chosen = term.borrow_mut().pick(&title, &items);
        let Some(chosen) = chosen else {
            return;
        };
        match actions[chosen].2 {
            Action::Toggle => set_enabled(agent, term, &name, !p.enabled),
            Action::View => say_line(
                term,
                &format!(
                    "{}\n{}\n{}",
                    style.bold(&format!(
                        "{} ({}, hash {}{})",
                        p.name,
                        size_label(p.chars()),
                        short_hash(&p.text),
                        p.source_path
                            .as_deref()
                            .map_or(String::new(), |f| format!(", from {f}"))
                    )),
                    p.text,
                    style.dim("(end)")
                ),
            ),
            Action::Edit => {
                if let Some(text) = compose(agent, term, &name, &p.text)
                    && with_store(agent, term, |s| s.set_prompt_text(&name, &text, None)).is_some()
                {
                    done(term, &format!("saved {name}; {}", applies(kind)));
                }
            }
            Action::Up | Action::Down => {
                let delta = if matches!(actions[chosen].2, Action::Up) {
                    -1
                } else {
                    1
                };
                match with_store(agent, term, |s| s.move_prompt(&name, delta)) {
                    Some(true) => done(term, &format!("moved {name}; {}", applies(kind))),
                    Some(false) => say_line(
                        term,
                        &style.yellow(if delta < 0 {
                            "already first"
                        } else {
                            "already last"
                        }),
                    ),
                    None => {}
                }
            }
            Action::Rename => {
                let asked = term
                    .borrow_mut()
                    .ask("new name (empty cancels)", Some(&name));
                if let Some(new) = asked.filter(|n| !n.is_empty() && *n != name)
                    && with_store(agent, term, |s| s.rename_prompt(&name, &new)).is_some()
                {
                    agent.refresh_chat_prompts();
                    done(term, &format!("renamed {name} to {new}"));
                    name = new;
                }
            }
            Action::Reload => {
                let path = p.source_path.clone().unwrap_or_default();
                match read_prompt_file(&path) {
                    Err(why) => error(term, &why),
                    Ok((text, absolute)) => {
                        if with_store(agent, term, |s| {
                            s.set_prompt_text(&name, &text, Some(&absolute))
                        })
                        .is_some()
                        {
                            done(
                                term,
                                &format!("reloaded {name} from {absolute}; {}", applies(kind)),
                            );
                        }
                    }
                }
            }
            Action::Restore => {
                if with_store(agent, term, |s| {
                    s.set_prompt_text(&name, BUILTIN_PROMPT, None)
                })
                .is_some()
                {
                    done(
                        term,
                        &format!("{name} holds the built-in text again; {NEXT_CHAT}"),
                    );
                }
            }
            Action::Delete => {
                let answer = term
                    .borrow_mut()
                    .ask(&format!("delete the {} {name}? [y/N]", noun(kind)), None);
                if yes(answer) {
                    if with_store(agent, term, |s| s.delete_prompt(&name)).is_some() {
                        agent.refresh_chat_prompts();
                        done(term, &format!("deleted {name}; {}", applies(kind)));
                    }
                    return;
                }
                say_line(term, &style.yellow("[not deleted]"));
            }
        }
    }
}

fn add_new<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, kind: PromptType) {
    let style = term.borrow().style();
    let cancelled = || say_line(term, &style.yellow("[cancelled: nothing saved]"));
    let asked = term.borrow_mut().ask("name (empty cancels)", None);
    let Some(name) = asked.filter(|n| !n.is_empty()) else {
        return;
    };
    if let Err(why) = check_prompt_name(&name) {
        return error(term, &why);
    }
    let taken = with_store(agent, term, |s| Ok(s.prompt(&name).is_ok()));
    if taken != Some(false) {
        if taken == Some(true) {
            error(term, &format!("there is already a prompt named {name}"));
        }
        return;
    }
    let ways = [
        MenuItem::new("write it here").with_detail("in your editor, or typed lines"),
        MenuItem::new("read it from a file").with_detail("a copy is stored"),
    ];
    let way = term
        .borrow_mut()
        .pick(&format!("the text of the {}", noun(kind)), &ways);
    let (text, source) = match way {
        None => return cancelled(),
        Some(0) => match compose(agent, term, &name, "") {
            Some(text) => (text, None),
            None => return cancelled(),
        },
        Some(_) => {
            let Some(path) = term
                .borrow_mut()
                .ask("path of the file (empty cancels)", None)
                .filter(|p| !p.is_empty())
            else {
                return cancelled();
            };
            match read_prompt_file(&path) {
                Ok((text, absolute)) => (text, Some(absolute)),
                Err(why) => return error(term, &why),
            }
        }
    };
    let Some(description) = term
        .borrow_mut()
        .ask("short description, Enter for none", None)
    else {
        return cancelled();
    };
    let saved = with_store(agent, term, |s| {
        s.add_prompt_of_type(&name, &description, &text, source.as_deref(), kind)
    });
    if saved.is_none() {
        return;
    }
    done(
        term,
        &format!(
            "saved {} {name} ({})",
            noun(kind),
            size_label(text.chars().count())
        ),
    );
    if kind != PromptType::System {
        return say_line(term, &style.dim(&after_save(kind, &name)));
    }
    let on = term.borrow_mut().ask(
        &format!("switch {name} on, so new chats start with it? [Y/n]"),
        None,
    );
    if !matches!(
        on.as_deref().map(str::to_ascii_lowercase).as_deref(),
        Some("n" | "no")
    ) {
        set_enabled(agent, term, &name, true);
    } else {
        say_line(term, &style.dim("left switched off"));
    }
}

/// Gets the text of a prompt: written (or edited from `initial`), then saved, improved with the
/// model, or written again. `None` when the owner backs out. Nothing is saved here.
fn compose<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    name: &str,
    initial: &str,
) -> Option<String> {
    let mut text = text_from_owner(term, name, initial)?;
    loop {
        let size = size_label(text.chars().count());
        let items = [
            MenuItem::new("save").with_detail(&size),
            MenuItem::new("improve with AI")
                .with_detail("one request to the model in use; you review the result"),
            MenuItem::new("edit again"),
        ];
        let chosen = term
            .borrow_mut()
            .pick(&format!("text ready ({size})"), &items);
        match chosen? {
            0 => match check_prompt_text(&text) {
                Ok(()) => return Some(text),
                Err(why) => error(term, &why),
            },
            1 => {
                if let Some(better) = improve(agent, term, &text) {
                    text = better;
                }
            }
            _ => text = text_from_owner(term, name, &text)?,
        }
    }
}

/// One round of "improve with AI": ask, show the result, and let the owner use it, edit it, try
/// again with another note, or keep the original (`None`).
fn improve<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    draft: &str,
) -> Option<String> {
    let style = term.borrow().style();
    let mut note = term
        .borrow_mut()
        .ask("what should change? Enter for a general rewrite", None)?;
    loop {
        say_line(
            term,
            &style.dim(&format!(
                "asking {} to rewrite the prompt (one request, it uses tokens)...",
                agent.model()
            )),
        );
        let improved = match agent.improve_prompt(draft, &note) {
            Ok(text) => text,
            Err(why) => {
                error(term, &why);
                return None;
            }
        };
        say_line(
            term,
            &format!(
                "{}\n{improved}\n{}",
                style.bold(&format!(
                    "improved prompt ({}, was {})",
                    size_label(improved.chars().count()),
                    size_label(draft.chars().count())
                )),
                style.dim("(end)")
            ),
        );
        let items = [
            MenuItem::new("use this"),
            MenuItem::new("edit it"),
            MenuItem::new("try again with a note").with_detail("rewrites your original again"),
            MenuItem::new("keep my original"),
        ];
        let chosen = term.borrow_mut().pick("the improved prompt", &items);
        match chosen {
            Some(0) => return Some(improved),
            Some(1) => return text_from_owner(term, "the improved prompt", &improved),
            Some(2) => {
                note = term.borrow_mut().ask("what should change?", None)?;
            }
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{NoObserver, ScriptedProvider, StoreContext};
    use crate::provider::{CHOOSE_A_PROVIDER, NoProvider};
    use crate::repl::{Terminal, run};
    use crate::store::Store;
    use crate::store_prompts::PROMPT_WRITER_GUIDE;
    use crate::style::Style;
    use crate::tools::testutil::Scratch;
    use crate::turn_log::TurnLog;
    use std::cell::RefCell;
    use std::io::Cursor;
    use std::rc::Rc;

    fn config_menu<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
        manager(agent, term, PromptType::System);
    }

    const DOWN: &str = "\x1b[B";
    const ENTER: &str = "\r";
    const ESC: &str = "\x1b";

    type TestTerm = Shared<Cursor<Vec<u8>>, Vec<u8>>;
    type Requests = Rc<RefCell<Vec<Vec<u8>>>>;

    fn downs(n: usize) -> String {
        DOWN.repeat(n)
    }

    fn screen(term: &TestTerm) -> String {
        String::from_utf8(term.borrow().output().clone()).unwrap()
    }

    fn rig_with(
        dir: &Scratch,
        typed: &str,
        provider: Box<dyn crate::provider::Provider>,
        target: bool,
        menus: bool,
    ) -> (Agent, TestTerm) {
        let term = Terminal::shared_styled(
            Cursor::new(typed.as_bytes().to_vec()),
            Vec::new(),
            Style::plain(),
        );
        if menus {
            term.borrow_mut().enable_menus(false);
        }
        let log = TurnLog::create(&dir.path().join("logs")).unwrap();
        let store = Store::open(&dir.path().join("harness.db")).unwrap();
        let mut agent = Agent::new("m", provider, Vec::new(), log);
        if target {
            agent = agent.with_target("test", "http://x/v1");
        }
        let agent = agent.with_store(
            store,
            StoreContext {
                cwd: dir.path().display().to_string(),
            },
        );
        (agent, term)
    }

    /// An agent saving into the scratch folder, answering with `replies`.
    fn rig(
        dir: &Scratch,
        typed: &str,
        replies: &[&str],
        menus: bool,
    ) -> (Agent, TestTerm, Requests) {
        let provider =
            ScriptedProvider::new(replies.iter().map(|r| ScriptedProvider::text(r)).collect());
        let requests = provider.requests();
        let (agent, term) = rig_with(dir, typed, Box::new(provider), true, menus);
        (agent, term, requests)
    }

    fn store(dir: &Scratch) -> Store {
        Store::open(&dir.path().join("harness.db")).unwrap()
    }

    fn names(dir: &Scratch) -> Vec<String> {
        store(dir)
            .list_prompts()
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect()
    }

    fn add(dir: &Scratch, name: &str, text: &str) {
        store(dir).add_prompt(name, "", text, None).unwrap();
    }

    fn system_of(request: &[u8]) -> String {
        let v: serde_json::Value = serde_json::from_slice(request).unwrap();
        v["messages"][0]["content"].as_str().unwrap().to_owned()
    }

    // ---- the checklist ----

    #[test]
    fn the_checklist_flips_prompts_and_keeps_the_cursor_on_the_one_just_flipped() {
        let dir = Scratch::new("pu-checklist");
        add(&dir, "a", "A text");
        add(&dir, "b", "B text");
        // Down to a, Enter (on), Enter again on the same row (off), Down to b, Enter (on), Esc.
        let typed = [DOWN, ENTER, ENTER, DOWN, ENTER, ESC].concat();
        let (mut agent, term, _) = rig(&dir, &typed, &[], true);
        checklist(&mut agent, &term);
        let on: Vec<String> = store(&dir)
            .list_prompts()
            .unwrap()
            .into_iter()
            .filter(|p| p.enabled)
            .map(|p| p.name)
            .collect();
        assert_eq!(on, ["base", "b"]);
        let out = screen(&term);
        assert!(out.contains("[x] base"), "{out}");
        assert!(out.contains("applies to the next chat"), "{out}");
        assert!(
            out.contains("a new chat would start with 2 on (base, b)"),
            "{out}"
        );
    }

    #[test]
    fn leaving_the_checklist_untouched_says_nothing_and_changes_nothing() {
        let dir = Scratch::new("pu-checklist-esc");
        let (mut agent, term, _) = rig(&dir, ESC, &[], true);
        checklist(&mut agent, &term);
        assert!(!screen(&term).contains("applies to the next chat"));
        let (mut agent, term, _) = rig(&dir, "", &[], true);
        checklist(&mut agent, &term);
        assert!(store(&dir).prompt("base").unwrap().enabled);
    }

    #[test]
    fn an_empty_checklist_points_to_config() {
        let dir = Scratch::new("pu-checklist-empty");
        store(&dir).delete_prompt("base").unwrap();
        let (mut agent, term, _) = rig(&dir, "", &[], true);
        checklist(&mut agent, &term);
        assert!(screen(&term).contains("no system prompts: use /prompts config"));
    }

    // ---- the manager: adding ----

    #[test]
    fn add_new_typed_text_is_saved_with_a_description_and_switched_on() {
        let dir = Scratch::new("pu-add-typed");
        let typed = [
            DOWN,
            ENTER,
            "myprompt\n",
            ENTER, // write it here
            "line one\nline two\n.\n",
            ENTER, // save
            "does things\n",
            "\n", // switch on: Enter means yes
            ESC,
        ]
        .concat();
        let (mut agent, term, _) = rig(&dir, &typed, &[], true);
        config_menu(&mut agent, &term);
        let p = store(&dir).prompt("myprompt").unwrap();
        assert_eq!(p.text, "line one\nline two");
        assert_eq!(p.description, "does things");
        assert!(p.enabled);
        assert_eq!(p.source_path, None);
        let out = screen(&term);
        assert!(
            out.contains("type the text, then a line holding only ."),
            "{out}"
        );
        assert!(
            out.contains("myprompt is on; applies to the next chat"),
            "{out}"
        );
    }

    #[test]
    fn add_new_from_a_file_copies_it_and_remembers_the_path() {
        let dir = Scratch::new("pu-add-file");
        let file = dir.path().join("rules.md");
        std::fs::write(&file, "# Rules\nBe brief.\n").unwrap();
        let typed = [
            DOWN,
            ENTER,
            "fromfile\n",
            DOWN,
            ENTER, // read it from a file
            &format!("{}\n", file.display()),
            "\n",  // no description
            "n\n", // not switched on
            ESC,
        ]
        .concat();
        let (mut agent, term, _) = rig(&dir, &typed, &[], true);
        config_menu(&mut agent, &term);
        let p = store(&dir).prompt("fromfile").unwrap();
        assert_eq!(p.text, "# Rules\nBe brief.\n");
        assert!(!p.enabled);
        assert!(p.source_path.as_deref().unwrap().ends_with("rules.md"));
        assert!(screen(&term).contains("left switched off"));
        // The file is only copied: changing it later changes nothing.
        std::fs::write(&file, "changed").unwrap();
        assert_eq!(
            store(&dir).prompt("fromfile").unwrap().text,
            "# Rules\nBe brief.\n"
        );
    }

    #[test]
    fn a_missing_oversized_or_binary_file_is_refused() {
        let dir = Scratch::new("pu-add-file-bad");
        let big = dir.path().join("big.md");
        std::fs::write(&big, "x".repeat(MAX_PROMPT_BYTES + 1)).unwrap();
        let bin = dir.path().join("bin.dat");
        std::fs::write(&bin, [0xff, 0xfe, 0x00, 0x9f]).unwrap();
        for path in [
            dir.path().join("nope.md"),
            big,
            bin,
            dir.path().to_path_buf(),
        ] {
            let typed = format!("/prompts add x {}\n/exit\n", path.display());
            let (mut agent, term, _) = rig(&dir, &typed, &[], false);
            run(&mut agent, &term).unwrap();
            let out = screen(&term);
            assert!(out.contains("[error: "), "{path:?}: {out}");
            assert_eq!(names(&dir), ["base"], "{path:?}");
        }
    }

    #[test]
    fn a_fake_editor_supplies_the_text_and_a_failing_editor_saves_nothing() {
        let dir = Scratch::new("pu-editor");
        let script = dir.path().join("ed.sh");
        std::fs::write(&script, "printf 'written by the editor\\n' > \"$1\"\n").unwrap();
        let typed = [DOWN, ENTER, "viaeditor\n", ENTER, ENTER, "\n", "n\n", ESC].concat();
        let (mut agent, term, _) = rig(&dir, &typed, &[], true);
        term.borrow_mut()
            .set_editor(Some(format!("sh {}", script.display())));
        config_menu(&mut agent, &term);
        assert_eq!(
            store(&dir).prompt("viaeditor").unwrap().text,
            "written by the editor\n"
        );
        assert!(screen(&term).contains("opening sh "), "{}", screen(&term));
        // An editor that fails: an error, nothing saved.
        let typed = [&downs(2), ENTER, "broken\n", ENTER, ESC].concat();
        let (mut agent, term, _) = rig(&dir, &typed, &[], true);
        term.borrow_mut().set_editor(Some("false".to_owned()));
        config_menu(&mut agent, &term);
        let out = screen(&term);
        assert!(out.contains("exited with"), "{out}");
        assert!(out.contains("[cancelled: nothing saved]"), "{out}");
        assert_eq!(names(&dir), ["base", "viaeditor"]);
    }

    #[test]
    fn backing_out_of_an_add_saves_nothing() {
        let dir = Scratch::new("pu-add-cancel");
        // Esc at the text-ready menu discards the whole add.
        let typed = [DOWN, ENTER, "x\n", ENTER, "some text\n.\n", ESC, ESC].concat();
        let (mut agent, term, _) = rig(&dir, &typed, &[], true);
        config_menu(&mut agent, &term);
        assert!(screen(&term).contains("[cancelled: nothing saved]"));
        assert_eq!(names(&dir), ["base"]);
        // An empty name, a reserved word, an existing name and the end of input.
        for typed in [
            [DOWN, ENTER, "\n", ESC].concat(),
            [DOWN, ENTER, "delete\n", ESC].concat(),
            [DOWN, ENTER, "base\n", ESC].concat(),
            [DOWN, ENTER].concat(),
        ] {
            let (mut agent, term, _) = rig(&dir, &typed, &[], true);
            config_menu(&mut agent, &term);
            assert_eq!(names(&dir), ["base"], "{typed:?}");
        }
    }

    #[test]
    fn an_empty_text_is_not_saved() {
        let dir = Scratch::new("pu-add-empty");
        // Empty text: the text-ready menu offers save, which refuses; then Esc discards.
        let typed = [DOWN, ENTER, "x\n", ENTER, ".\n", ENTER, ESC, ESC].concat();
        let (mut agent, term, _) = rig(&dir, &typed, &[], true);
        config_menu(&mut agent, &term);
        assert!(screen(&term).contains("the prompt is empty"));
        assert_eq!(names(&dir), ["base"]);
    }

    // ---- improve with AI ----

    #[test]
    fn improve_sends_the_guide_and_the_draft_and_nothing_is_saved_until_the_prompt_is() {
        let dir = Scratch::new("pu-improve");
        let typed = [
            DOWN,
            ENTER,
            "imp\n",
            ENTER,
            "draft text\n.\n",
            DOWN,
            ENTER, // improve with AI
            "make it short\n",
            ENTER, // use this
            ENTER, // save
            "\n",
            "n\n",
            ESC,
        ]
        .concat();
        let (mut agent, term, requests) = rig(&dir, &typed, &["short draft"], true);
        config_menu(&mut agent, &term);
        assert_eq!(store(&dir).prompt("imp").unwrap().text, "short draft");
        let requests = requests.borrow();
        assert_eq!(requests.len(), 1);
        assert_eq!(system_of(&requests[0]), PROMPT_WRITER_GUIDE);
        let v: serde_json::Value = serde_json::from_slice(&requests[0]).unwrap();
        let user = v["messages"][1]["content"].as_str().unwrap();
        assert_eq!(user, "Draft prompt:\n\ndraft text\n\nNote: make it short");
        assert!(
            v.get("tools")
                .is_none_or(|t| t.as_array().is_none_or(Vec::is_empty))
        );
        let out = screen(&term);
        assert!(
            out.contains("improved prompt (11 chars, was 10 chars)"),
            "{out}"
        );
        // Nothing of it is in any chat table or in the log.
        let s = store(&dir);
        for table in ["sessions", "messages", "calls"] {
            let n: i64 = s
                .conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "{table}");
        }
        let log = std::fs::read_to_string(agent.log_path()).unwrap_or_default();
        assert!(!log.contains("short draft") && !log.contains("rewrite system prompts"));
    }

    #[test]
    fn improve_can_be_retried_with_a_note_or_the_original_kept() {
        let dir = Scratch::new("pu-improve-retry");
        let typed = [
            DOWN,
            ENTER,
            "imp\n",
            ENTER,
            "my draft\n.\n",
            DOWN,
            ENTER, // improve
            "\n",  // no note
            &downs(2),
            ENTER, // try again with a note
            "shorter\n",
            &downs(3),
            ENTER, // keep my original
            ENTER, // save
            "\n",
            "n\n",
            ESC,
        ]
        .concat();
        let (mut agent, term, requests) = rig(&dir, &typed, &["first try", "second try"], true);
        config_menu(&mut agent, &term);
        assert_eq!(store(&dir).prompt("imp").unwrap().text, "my draft");
        let requests = requests.borrow();
        assert_eq!(requests.len(), 2);
        let user = |i: usize| -> String {
            let v: serde_json::Value = serde_json::from_slice(&requests[i]).unwrap();
            v["messages"][1]["content"].as_str().unwrap().to_owned()
        };
        assert_eq!(user(0), "Draft prompt:\n\nmy draft");
        assert_eq!(
            user(1),
            "Draft prompt:\n\nmy draft\n\nNote: shorter",
            "from the original"
        );
    }

    #[test]
    fn an_improved_text_can_be_edited_before_it_is_used() {
        let dir = Scratch::new("pu-improve-edit");
        let typed = [
            DOWN,
            ENTER,
            "imp\n",
            ENTER,
            "my draft\n.\n",
            DOWN,
            ENTER,
            "\n",
            DOWN,
            ENTER, // edit it
            "hand edited\n.\n",
            ENTER, // save
            "\n",
            "n\n",
            ESC,
        ]
        .concat();
        let (mut agent, term, _) = rig(&dir, &typed, &["```markdown\nfenced answer\n```"], true);
        config_menu(&mut agent, &term);
        assert_eq!(store(&dir).prompt("imp").unwrap().text, "hand edited");
        assert!(
            screen(&term).contains("fenced answer\n"),
            "the fence is stripped before showing"
        );
        assert!(!screen(&term).contains("```"));
    }

    #[test]
    fn improve_without_a_provider_says_to_choose_one_and_keeps_the_draft() {
        let dir = Scratch::new("pu-improve-none");
        let typed = [
            DOWN,
            ENTER,
            "imp\n",
            ENTER,
            "my draft\n.\n",
            DOWN,
            ENTER,
            "\n",
            ENTER, // save
            "\n",
            "n\n",
            ESC,
        ]
        .concat();
        let (mut agent, term) = rig_with(&dir, &typed, Box::new(NoProvider), false, true);
        config_menu(&mut agent, &term);
        assert!(screen(&term).contains(CHOOSE_A_PROVIDER));
        assert_eq!(store(&dir).prompt("imp").unwrap().text, "my draft");
    }

    #[test]
    fn a_failing_or_empty_improve_request_is_reported() {
        let dir = Scratch::new("pu-improve-fail");
        let provider = ScriptedProvider::new(vec![ScriptedProvider::text("   ")]);
        let (agent, _term) = rig_with(&dir, "", Box::new(provider), true, true);
        assert!(
            agent
                .improve_prompt("d", "")
                .unwrap_err()
                .contains("nothing")
        );
        let provider = ScriptedProvider::new(vec![]);
        let (agent, _term) = rig_with(&dir, "", Box::new(provider), true, true);
        assert!(agent.improve_prompt("d", "").is_err());
    }

    // ---- the manager: one prompt ----

    #[test]
    fn a_prompt_can_be_moved_renamed_and_deleted_from_its_screen() {
        let dir = Scratch::new("pu-screen");
        add(&dir, "a", "A");
        add(&dir, "b", "B");
        let typed = [
            &downs(2),
            ENTER, // b
            &downs(3),
            ENTER, // move up
            &downs(3),
            ENTER, // move up again: b is first
            &downs(5),
            ENTER,
            "bb\n", // rename
            &downs(6),
            ENTER,
            "y\n", // delete
            ESC,
        ]
        .concat();
        let (mut agent, term, _) = rig(&dir, &typed, &[], true);
        config_menu(&mut agent, &term);
        let out = screen(&term);
        assert!(out.contains("moved b"), "{out}");
        assert!(out.contains("renamed b to bb"), "{out}");
        assert!(out.contains("deleted bb"), "{out}");
        assert_eq!(names(&dir), ["base", "a"]);
    }

    #[test]
    fn moving_at_an_end_says_so_and_a_no_keeps_the_prompt() {
        let dir = Scratch::new("pu-screen-end");
        add(&dir, "a", "A");
        let typed = [
            DOWN,
            ENTER, // a (last)
            &downs(4),
            ENTER, // move down: already last
            &downs(6),
            ENTER,
            "n\n", // delete: no
            ESC,
            ESC,
        ]
        .concat();
        let (mut agent, term, _) = rig(&dir, &typed, &[], true);
        config_menu(&mut agent, &term);
        let out = screen(&term);
        assert!(out.contains("already last"), "{out}");
        assert!(out.contains("[not deleted]"), "{out}");
        assert_eq!(names(&dir), ["base", "a"]);
    }

    #[test]
    fn view_shows_the_text_with_its_size_and_hash() {
        let dir = Scratch::new("pu-view");
        let typed = [ENTER, DOWN, ENTER, ESC, ESC].concat();
        let (mut agent, term, _) = rig(&dir, &typed, &[], true);
        config_menu(&mut agent, &term);
        let out = screen(&term);
        assert!(out.contains(BUILTIN_PROMPT), "{out}");
        assert!(
            out.contains(&format!("hash {}", short_hash(BUILTIN_PROMPT))),
            "{out}"
        );
    }

    #[test]
    fn edit_changes_the_text_in_the_database() {
        let dir = Scratch::new("pu-edit");
        add(&dir, "a", "old text");
        let typed = [
            DOWN,
            ENTER,
            &downs(2),
            ENTER,
            "new text\n.\n",
            ENTER,
            ESC,
            ESC,
        ]
        .concat();
        let (mut agent, term, _) = rig(&dir, &typed, &[], true);
        config_menu(&mut agent, &term);
        assert_eq!(store(&dir).prompt("a").unwrap().text, "new text");
        assert!(screen(&term).contains("current text of a:"));
    }

    #[test]
    fn switching_from_the_prompt_screen_flips_it() {
        let dir = Scratch::new("pu-screen-toggle");
        add(&dir, "a", "A");
        let typed = [DOWN, ENTER, ENTER, ESC, ESC].concat();
        let (mut agent, term, _) = rig(&dir, &typed, &[], true);
        config_menu(&mut agent, &term);
        assert!(store(&dir).prompt("a").unwrap().enabled);
    }

    #[test]
    fn reload_from_file_reads_the_file_again_and_only_then() {
        let dir = Scratch::new("pu-reload");
        let file = dir.path().join("p.md");
        std::fs::write(&file, "version one").unwrap();
        store(&dir)
            .add_prompt("f", "", "version one", Some(&file.display().to_string()))
            .unwrap();
        std::fs::write(&file, "version two").unwrap();
        assert_eq!(store(&dir).prompt("f").unwrap().text, "version one");
        let typed = [DOWN, ENTER, &downs(6), ENTER, ESC, ESC].concat();
        let (mut agent, term, _) = rig(&dir, &typed, &[], true);
        config_menu(&mut agent, &term);
        assert_eq!(store(&dir).prompt("f").unwrap().text, "version two");
        // A file that has gone says so and keeps the text.
        std::fs::remove_file(&file).unwrap();
        let (mut agent, term, _) = rig(&dir, &typed, &[], true);
        config_menu(&mut agent, &term);
        assert!(
            screen(&term).contains("[error: cannot read"),
            "{}",
            screen(&term)
        );
        assert_eq!(store(&dir).prompt("f").unwrap().text, "version two");
    }

    #[test]
    fn the_built_in_text_can_be_restored_and_a_deleted_base_can_be_recreated() {
        let dir = Scratch::new("pu-restore");
        store(&dir).set_prompt_text("base", "ruined", None).unwrap();
        let typed = [ENTER, &downs(6), ENTER, ESC, ESC].concat();
        let (mut agent, term, _) = rig(&dir, &typed, &[], true);
        config_menu(&mut agent, &term);
        assert_eq!(store(&dir).prompt("base").unwrap().text, BUILTIN_PROMPT);
        assert!(screen(&term).contains("base holds the built-in text again"));

        store(&dir).delete_prompt("base").unwrap();
        add(&dir, "other", "O");
        // Rows now: other, + add new, + restore built-in base.
        let typed = [&downs(2), ENTER, ESC].concat();
        let (mut agent, term, _) = rig(&dir, &typed, &[], true);
        config_menu(&mut agent, &term);
        let base = store(&dir).prompt("base").unwrap();
        assert_eq!(base.text, BUILTIN_PROMPT);
        assert!(base.enabled);
    }

    // ---- typed commands ----

    #[test]
    fn typed_commands_work_without_a_terminal() {
        let dir = Scratch::new("pu-typed");
        let typed = "/prompts\n\
            /prompts add rules\nBe brief.\nNo emoji.\n.\n\
            /prompts on rules\n\
            /prompts show\n\
            /prompts off base\n\
            /prompts edit rules\nBe very brief.\n.\n\
            /prompts delete rules\ny\n\
            /prompts config\n\
            /exit\n";
        let (mut agent, term, _) = rig(&dir, typed, &[], false);
        run(&mut agent, &term).unwrap();
        let out = screen(&term);
        assert!(
            out.contains("system prompts (changes apply to the next chat):\n  on   base"),
            "{out}"
        );
        assert!(
            out.contains("saved system prompt rules (19 chars), switched off: /prompts on rules"),
            "{out}"
        );
        assert!(
            out.contains("rules is on; applies to the next chat"),
            "{out}"
        );
        assert!(
            out.contains(&format!("{BUILTIN_PROMPT}\n\nBe brief.\nNo emoji.\n(end)")),
            "{out}"
        );
        assert!(out.contains("system prompt of a new chat: "), "{out}");
        assert!(
            out.contains("a new chat would start with 1 on (rules)"),
            "{out}"
        );
        assert!(
            out.contains("saved rules; applies to the next chat"),
            "{out}"
        );
        assert!(
            out.contains("delete the system prompt rules? [y/N]: deleted rules"),
            "{out}"
        );
        assert!(out.contains("/prompts config needs a terminal"), "{out}");
        assert_eq!(names(&dir), ["base"]);
        assert!(!store(&dir).prompt("base").unwrap().enabled);
    }

    // ---- one manager per kind (issue 14) ----

    fn add_of_kind(dir: &Scratch, name: &str, kind: PromptType) {
        store(dir)
            .add_prompt_of_type(name, "", &format!("{name} text"), None, kind)
            .unwrap();
    }

    #[test]
    fn a_skill_is_added_from_the_skills_manager_and_is_not_switched_on() {
        let dir = Scratch::new("pu-add-skill");
        let typed = [
            ENTER, // + add new (the only row)
            "tdd\n",
            ENTER, // write it here
            "tests first\n.\n",
            ENTER, // save
            "\n",  // no description
            ESC,
        ]
        .concat();
        let (mut agent, term, _) = rig(&dir, &typed, &[], true);
        manager(&mut agent, &term, PromptType::Skill);
        let p = store(&dir).prompt("tdd").unwrap();
        assert_eq!(p.prompt_type, PromptType::Skill);
        assert!(!p.enabled);
        let out = screen(&term);
        assert!(out.contains("saved skill tdd"), "{out}");
        assert!(out.contains("/skills use tdd"), "{out}");
        // No "switch on" question and no change to the system prompt of a new chat.
        assert!(!out.contains("[Y/n]"), "{out}");
        assert!(
            !agent
                .assembled_prompt()
                .unwrap()
                .text
                .contains("tests first")
        );
    }

    #[test]
    fn a_personality_is_added_from_its_own_manager() {
        let dir = Scratch::new("pu-add-personality");
        let typed = [ENTER, "pirate\n", ENTER, "arr\n.\n", ENTER, "\n", ESC].concat();
        let (mut agent, term, _) = rig(&dir, &typed, &[], true);
        manager(&mut agent, &term, PromptType::Personality);
        let p = store(&dir).prompt("pirate").unwrap();
        assert_eq!(p.prompt_type, PromptType::Personality);
        assert!(screen(&term).contains("saved personality pirate"));
        assert!(screen(&term).contains("/personality pirate selects it"));
    }

    #[test]
    fn each_manager_and_the_checklist_list_only_their_own_kind() {
        let dir = Scratch::new("pu-kinds-listed");
        add_of_kind(&dir, "tdd", PromptType::Skill);
        add_of_kind(&dir, "pirate", PromptType::Personality);
        let shown = |kind: PromptType| {
            let (mut agent, term, _) = rig(&dir, ESC, &[], true);
            manager(&mut agent, &term, kind);
            screen(&term)
        };
        let system = shown(PromptType::System);
        assert!(system.contains("base") && !system.contains("tdd") && !system.contains("pirate"));
        let skills = shown(PromptType::Skill);
        assert!(skills.contains("tdd") && !skills.contains("base") && !skills.contains("pirate"));
        assert!(!skills.contains("[ ]"), "{skills}");
        let personalities = shown(PromptType::Personality);
        assert!(personalities.contains("pirate") && !personalities.contains("tdd"));
        let (mut agent, term, _) = rig(&dir, ESC, &[], true);
        checklist(&mut agent, &term);
        let out = screen(&term);
        assert!(out.contains("[x] base"), "{out}");
        assert!(!out.contains("tdd") && !out.contains("pirate"), "{out}");
    }

    #[test]
    fn a_skill_screen_has_no_switch_and_edits_in_place() {
        let dir = Scratch::new("pu-skill-screen");
        add_of_kind(&dir, "tdd", PromptType::Skill);
        // The skill, then "edit" (rows: view, edit, ...), typed text, save, back out.
        let typed = [ENTER, DOWN, ENTER, "new text\n.\n", ENTER, ESC, ESC].concat();
        let (mut agent, term, _) = rig(&dir, &typed, &[], true);
        manager(&mut agent, &term, PromptType::Skill);
        let out = screen(&term);
        assert!(
            !out.contains("switch on") && !out.contains("switch off"),
            "{out}"
        );
        assert!(
            out.contains("saved tdd; used from the next message that uses it"),
            "{out}"
        );
        assert_eq!(store(&dir).prompt("tdd").unwrap().text, "new text");
    }

    #[test]
    fn typed_commands_manage_skills_and_personalities_and_refuse_the_wrong_kind() {
        let dir = Scratch::new("pu-typed-kinds");
        let typed = "/skills add tdd\nWrite tests first.\n.\n\
            /personality add pirate\nArr.\n.\n\
            /skills edit tdd\nTests first, always.\n.\n\
            /prompts on tdd\n\
            /prompts edit pirate\n\
            /prompts delete tdd\n\
            /skills delete pirate\n\
            /personality edit base\n\
            /skills delete tdd\ny\n\
            /prompts type tdd skill\n\
            /exit\n";
        let (mut agent, term, _) = rig(&dir, typed, &[], false);
        run(&mut agent, &term).unwrap();
        let out = screen(&term);
        assert!(out.contains("saved skill tdd"), "{out}");
        assert!(out.contains("saved personality pirate"), "{out}");
        assert!(
            out.contains("saved tdd; used from the next message that uses it"),
            "{out}"
        );
        assert!(
            out.contains("tdd is a skill: manage it with /skills"),
            "{out}"
        );
        assert!(
            out.contains("pirate is a personality: manage it with /personality"),
            "{out}"
        );
        assert!(
            out.contains("base is a system prompt: manage it with /prompts"),
            "{out}"
        );
        assert!(out.contains("deleted tdd"), "{out}");
        assert!(out.contains("usage: /prompts"), "{out}");
        assert_eq!(names(&dir), ["base", "pirate"]);
    }

    #[test]
    fn a_new_skill_does_not_show_in_prompts() {
        let dir = Scratch::new("pu-prompts-clean");
        add_of_kind(&dir, "tdd", PromptType::Skill);
        add_of_kind(&dir, "pirate", PromptType::Personality);
        let (mut agent, term, _) = rig(&dir, "/prompts\n/exit\n", &[], false);
        run(&mut agent, &term).unwrap();
        let out = screen(&term);
        assert!(out.contains("base"), "{out}");
        assert!(!out.contains("tdd") && !out.contains("pirate"), "{out}");
    }

    #[test]
    fn deleting_a_skill_a_chat_has_on_stops_it_being_sent() {
        let dir = Scratch::new("pu-delete-in-chat");
        add_of_kind(&dir, "tdd", PromptType::Skill);
        let typed = "/skills on tdd\nhello\n/skills delete tdd\ny\nagain\n/exit\n";
        let (mut agent, term, requests) = rig(&dir, typed, &["one", "two"], false);
        run(&mut agent, &term).unwrap();
        let requests = requests.borrow();
        let texts = |i: usize| -> Vec<String> {
            let v: serde_json::Value = serde_json::from_slice(&requests[i]).unwrap();
            v["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["content"].as_str().unwrap_or("").to_owned())
                .collect()
        };
        // The skill went into the chat with the first message; once it is deleted the chat is
        // told that nothing is active, and the newest message is the user's.
        assert!(texts(0)[1].contains("Skill: tdd"));
        assert_eq!(texts(0).last().unwrap(), "hello");
        assert!(
            texts(1)
                .iter()
                .any(|t| t.starts_with(crate::store_prompts::NONE_ACTIVE))
        );
        assert_eq!(texts(1).last().unwrap(), "again");
    }

    #[test]
    fn typed_add_from_a_file_and_the_usual_mistakes() {
        let dir = Scratch::new("pu-typed-bad");
        let file = dir.path().join("rules file.md");
        std::fs::write(&file, "from file").unwrap();
        let typed = format!(
            "/prompts add fromfile {}\n\
             /prompts add on\n\
             /prompts add ../x\n\
             /prompts add fromfile text\n\
             /prompts on nope\n\
             /prompts off\n\
             /prompts edit a b\n\
             /prompts delete base\n\n\
             /prompts bogus\n\
             /exit\n",
            file.display()
        );
        let (mut agent, term, _) = rig(&dir, &typed, &[], false);
        run(&mut agent, &term).unwrap();
        let out = screen(&term);
        assert_eq!(store(&dir).prompt("fromfile").unwrap().text, "from file");
        assert!(
            out.contains("on is a /prompts command, so it cannot be a name"),
            "{out}"
        );
        assert!(out.contains("the name must not start with"), "{out}");
        assert!(
            out.contains("cannot read text: "),
            "a file named 'text' is not there: {out}"
        );
        assert!(out.contains("there is no prompt named nope"), "{out}");
        assert!(out.contains("usage: /prompts off <name>"), "{out}");
        assert!(out.contains("usage: /prompts edit <name>"), "{out}");
        assert!(
            out.contains("[not deleted]"),
            "an empty answer is a no: {out}"
        );
        assert!(out.contains("usage: /prompts [config | show"), "{out}");
        assert_eq!(names(&dir), ["base", "fromfile"]);
    }

    #[test]
    fn the_end_of_input_cancels_a_typed_text() {
        let dir = Scratch::new("pu-typed-eof");
        let (mut agent, term, _) = rig(&dir, "/prompts add half\nno ending line", &[], false);
        run(&mut agent, &term).unwrap();
        assert!(screen(&term).contains("[cancelled: nothing saved]"));
        assert_eq!(names(&dir), ["base"]);
    }

    #[test]
    fn typed_add_without_a_name_asks_for_one() {
        let dir = Scratch::new("pu-typed-noname");
        let (mut agent, term, _) = rig(&dir, "/prompts add\nasked\ntext\n.\n/exit\n", &[], false);
        run(&mut agent, &term).unwrap();
        assert_eq!(store(&dir).prompt("asked").unwrap().text, "text");
    }

    // ---- /new and the frozen prompt ----

    #[test]
    fn a_new_chat_gets_the_prompts_switched_on_now_and_old_chats_keep_theirs() {
        let dir = Scratch::new("pu-new");
        let typed =
            "/prompts add extra\nExtra rule.\n.\n/prompts on extra\nhello\n/new\nagain\n/exit\n";
        let (mut agent, term, requests) = rig(&dir, typed, &["one", "two"], false);
        run(&mut agent, &term).unwrap();
        let out = screen(&term);
        assert!(
            out.contains("new chat; system prompt: 2 on (base, extra)"),
            "{out}"
        );
        let requests = requests.borrow();
        assert_eq!(
            system_of(&requests[0]),
            BUILTIN_PROMPT,
            "the running chat kept its prompt"
        );
        assert_eq!(
            system_of(&requests[1]),
            format!("{BUILTIN_PROMPT}\n\nExtra rule.")
        );
        // Two chats saved, each with the prompt it started with.
        let s = store(&dir);
        let n: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
        let distinct: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM system_prompts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(distinct, 2);
        // The second request has only the new chat in it.
        let v: serde_json::Value = serde_json::from_slice(&requests[1]).unwrap();
        assert_eq!(v["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn new_resets_the_history_and_leaves_the_old_chat_listed() {
        let dir = Scratch::new("pu-new-sessions");
        let (mut agent, _term, _) = rig(&dir, "", &["ok", "ok"], false);
        agent.send("first", &mut NoObserver);
        assert_eq!(agent.current_session(), Some(1));
        let info = agent.new_chat().unwrap();
        assert!(!info.fallback && info.names == ["base"]);
        assert_eq!(agent.current_session(), None);
        assert_eq!(agent.history().len(), 1);
        agent.send("second", &mut NoObserver);
        assert_eq!(agent.current_session(), Some(2));
        assert_eq!(agent.sessions(10).unwrap().len(), 2);
    }

    #[test]
    fn a_chat_in_progress_does_not_reload_its_prompt() {
        let dir = Scratch::new("pu-load");
        let (mut agent, _term, _) = rig(&dir, "", &["ok"], false);
        assert!(
            agent.load_prompt().is_ok(),
            "before the first message it is fine"
        );
        agent.send("hello", &mut NoObserver);
        assert!(agent.load_prompt().unwrap_err().contains("/new"));
    }

    #[test]
    fn with_nothing_on_a_chat_starts_with_the_built_in_text_and_says_so() {
        let dir = Scratch::new("pu-fallback");
        store(&dir).set_prompt_enabled("base", false).unwrap();
        let (mut agent, _term, requests) = rig(&dir, "", &["ok"], false);
        let info = agent.new_chat().unwrap();
        assert!(info.fallback);
        assert!(
            info.summary()
                .contains("the built-in text, no prompt is switched on")
        );
        agent.send("hi", &mut NoObserver);
        assert_eq!(system_of(&requests.borrow()[0]), BUILTIN_PROMPT);
    }

    #[test]
    fn a_resumed_chat_keeps_its_prompt_and_says_when_it_differs_from_the_current_one() {
        let dir = Scratch::new("pu-resume");
        {
            let (mut agent, _term, _) = rig(&dir, "", &["ok"], false);
            agent.send("first", &mut NoObserver);
        }
        let (mut agent, _term, requests) = rig(&dir, "", &["again"], false);
        let same = agent.resume(Some(1)).unwrap();
        assert!(!same.notes().contains("system prompt"), "{}", same.notes());
        store(&dir).add_prompt("extra", "", "More.", None).unwrap();
        store(&dir).set_prompt_enabled("extra", true).unwrap();
        let different = agent.resume(Some(1)).unwrap();
        assert!(
            different.notes().contains("keeps its own system prompt"),
            "{}",
            different.notes()
        );
        agent.send("go on", &mut NoObserver);
        assert_eq!(system_of(&requests.borrow()[0]), BUILTIN_PROMPT);
    }

    #[test]
    fn the_summary_and_sizes_read_well() {
        let info = crate::agent::PromptInfo {
            names: vec!["base".into(), "rules".into()],
            chars: 4200,
            fallback: false,
        };
        assert_eq!(
            info.summary(),
            "2 on (base, rules); 4.2k chars, about 1050 tokens (estimate)"
        );
        assert_eq!(size_label(999), "999 chars");
        assert_eq!(size_label(1000), "1.0k chars");
        assert_eq!(crate::store_prompts::estimated_tokens(0), 0);
        assert_eq!(crate::store_prompts::estimated_tokens(5), 2);
    }

    #[test]
    fn help_lists_the_prompt_commands_and_new() {
        let dir = Scratch::new("pu-help");
        let (mut agent, term, _) = rig(&dir, "/help\n/exit\n", &[], false);
        run(&mut agent, &term).unwrap();
        let out = screen(&term);
        for word in [
            "/prompts config",
            "/prompts show",
            "/prompts add [name [file]]",
            "/new",
        ] {
            assert!(out.contains(word), "{word}: {out}");
        }
    }
}

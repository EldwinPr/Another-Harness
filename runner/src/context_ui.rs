//! `/context` (issue 37): the rules, decisions and shapes the owner has said about a project.
//!
//! In a terminal `/context` is a menu: pick an entry to edit it, switch its preload or delete it,
//! or add a new one. Typed forms work everywhere: `/context [list | show <name> | add <name> |
//! edit <name> | delete <name> | preload <name> on|off]`. The text is typed as lines ended by a lone `.`, or written in the
//! owner's editor. Nothing here asks the model anything; the model's own way in is the
//! `context_propose` tool, which asks the owner (`tools/context.rs`).

use std::io::{BufRead, Write};

use crate::agent::Agent;
use crate::config_ui::{done, error};
use crate::menu::MenuItem;
use crate::prompts_ui::with_store;
use crate::repl::{Shared, say_line};
use crate::store_context::{ContextEntry, check_kind, check_name, check_quote};
use crate::store_glossary::{Saved, check_area};

/// How much of a text a list line shows.
const LIST_CHARS: usize = 60;

const USAGE: &str = "usage: /context [list | show <name> | add <name> | edit <name> | delete <name> | preload <name> on|off]";

pub fn command<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, rest: &str) {
    let style = term.borrow().style();
    let menus = term.borrow().menus_enabled();
    let (word, tail) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, ""), |(word, tail)| (word, tail.trim()));
    match word {
        "" if menus => menu(agent, term),
        "" | "list" => list(agent, term),
        "show" | "add" | "edit" | "delete" | "preload" if tail.is_empty() => say_line(
            term,
            &style.yellow(&format!("usage: /context {word} <name>")),
        ),
        "show" => {
            if let Some(entry) = find(agent, term, tail) {
                show(term, &entry);
            }
        }
        "add" => add(agent, term, tail),
        "edit" => {
            if let Some(entry) = find(agent, term, tail) {
                edit(agent, term, &entry);
            }
        }
        "delete" => {
            if let Some(entry) = find(agent, term, tail) {
                delete(agent, term, &entry);
            }
        }
        "preload" => preload(agent, term, tail),
        _ => say_line(term, &style.yellow(USAGE)),
    }
}

fn first_line(text: &str, limit: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    let mut shown: String = line.chars().take(limit).collect();
    if line.chars().count() > limit || text.trim().lines().count() > 1 {
        shown.push_str("...");
    }
    shown
}

fn find<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    name: &str,
) -> Option<ContextEntry> {
    let project = agent.project();
    let found = with_store(agent, term, |s| s.context_entry(&project, name))?;
    if found.is_none() {
        error(
            term,
            &format!("there is no entry '{name}' here: /context lists them"),
        );
    }
    found
}

fn list<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
    let style = term.borrow().style();
    let project = agent.project();
    let Some(entries) = with_store(agent, term, |s| s.list_context(&project)) else {
        return;
    };
    if entries.is_empty() {
        return say_line(
            term,
            &style.dim("no context entries here yet: /context add <name>, or say a rule and let the model propose it"),
        );
    }
    for e in &entries {
        let scope = if e.project.is_empty() { "all" } else { "here" };
        let preload = if e.preload { " preload" } else { "" };
        say_line(
            term,
            &format!(
                "{}  {} · {scope} · ~{} tokens{preload}  {}",
                style.bold(&e.name),
                e.kind,
                e.tokens(),
                style.dim(&first_line(&e.text, LIST_CHARS))
            ),
        );
    }
}

fn menu<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
    let mut start = 0;
    loop {
        let project = agent.project();
        let Some(all) = with_store(agent, term, |s| s.list_context(&project)) else {
            return;
        };
        let mut items: Vec<MenuItem> = all
            .iter()
            .map(|e| {
                let scope = if e.project.is_empty() { "all" } else { "here" };
                let preload = if e.preload { " · preload" } else { "" };
                MenuItem::new(&e.name).with_detail(format!(
                    "{} · {scope}{preload} · {}",
                    e.kind,
                    first_line(&e.text, LIST_CHARS)
                ))
            })
            .collect();
        items.push(MenuItem::action("+ add new"));
        let title = format!("context, {} entries", all.len());
        let Some(chosen) = term.borrow_mut().pick_from(&title, &items, start) else {
            return;
        };
        start = chosen;
        if chosen == all.len() {
            let Some(name) = term.borrow_mut().ask("name", None) else {
                return;
            };
            add(agent, term, name.trim());
            continue;
        }
        let entry = &all[chosen];
        show(term, entry);
        let switch = if entry.preload {
            "preload off"
        } else {
            "preload on"
        };
        let actions = [
            MenuItem::new("edit").with_detail("change the text, the words or the area"),
            MenuItem::new(switch).with_detail("whether subagents start with it"),
            MenuItem::new("delete").with_detail("remove it for good"),
        ];
        let action = term.borrow_mut().pick(&entry.name, &actions);
        match action {
            Some(0) => edit(agent, term, entry),
            Some(1) => set_preload(agent, term, entry, !entry.preload),
            Some(_) => delete(agent, term, entry),
            None => {}
        }
    }
}

fn show<R: BufRead, W: Write>(term: &Shared<R, W>, e: &ContextEntry) {
    let style = term.borrow().style();
    let scope = if e.project.is_empty() {
        "every project"
    } else {
        "this project"
    };
    let area = if e.area.is_empty() {
        String::new()
    } else {
        format!(" · area {}", e.area)
    };
    say_line(
        term,
        &style.bold(&format!(
            "{} ({}) · {scope}{area} · preload {}",
            e.name,
            e.kind,
            if e.preload { "on" } else { "off" }
        )),
    );
    say_line(term, &e.text);
    if !e.quote.is_empty() {
        say_line(term, &style.dim(&format!("the owner said: {}", e.quote)));
    }
}

fn read_text<R: BufRead, W: Write>(
    term: &Shared<R, W>,
    name: &str,
    initial: &str,
) -> Option<String> {
    match term
        .borrow_mut()
        .read_text(&format!("the text of {name}"), initial)
    {
        Ok(Some(text)) if !text.trim().is_empty() => Some(text),
        Ok(_) => {
            let style = term.borrow().style();
            say_line(term, &style.yellow("[nothing saved]"));
            None
        }
        Err(why) => {
            error(term, &why);
            None
        }
    }
}

/// Asks one line; Enter keeps `current`. `None` when the input ended.
fn ask_or<R: BufRead, W: Write>(
    term: &Shared<R, W>,
    question: &str,
    current: &str,
) -> Option<String> {
    let answer = term.borrow_mut().ask(question, Some(current))?;
    Some(if answer.is_empty() {
        current.to_owned()
    } else {
        answer
    })
}

fn add<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, typed: &str) {
    let name = match check_name(typed) {
        Ok(name) => name,
        Err(why) => return error(term, &why),
    };
    let project = agent.project();
    if with_store(agent, term, |s| s.context_entry(&project, &name))
        .flatten()
        .is_some_and(|e| !e.project.is_empty())
    {
        return error(
            term,
            &format!("'{name}' is here already: /context edit {name}"),
        );
    }
    let Some(kind) = ask_or(term, "kind (rule, decision, shape or guide)", "rule") else {
        return;
    };
    let kind = match check_kind(&kind) {
        Ok(kind) => kind,
        Err(why) => return error(term, &why),
    };
    let Some(text) = read_text(term, &name, "") else {
        return;
    };
    let Some(quote) = ask_or(term, "your own words for it (Enter for none)", "") else {
        return;
    };
    let Some(area) = ask_or(term, "area (Enter for none)", "") else {
        return;
    };
    let Some(everywhere) = ask_or(term, "for every project? [y/N]", "n") else {
        return;
    };
    let scope = if everywhere.eq_ignore_ascii_case("y") {
        String::new()
    } else {
        project
    };
    save(agent, term, &scope, &name, &kind, &text, &quote, &area);
}

fn edit<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, e: &ContextEntry) {
    let Some(text) = read_text(term, &e.name, &e.text) else {
        return;
    };
    let Some(quote) = ask_or(term, "your own words for it", &e.quote) else {
        return;
    };
    let Some(area) = ask_or(term, "area", &e.area) else {
        return;
    };
    save(
        agent, term, &e.project, &e.name, &e.kind, &text, &quote, &area,
    );
}

#[allow(clippy::too_many_arguments)]
fn save<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    scope: &str,
    name: &str,
    kind: &str,
    text: &str,
    quote: &str,
    area: &str,
) {
    let checked = check_quote(quote).and_then(|_| check_area(area));
    if let Err(why) = checked {
        return error(term, &why);
    }
    let saved = with_store(agent, term, |s| {
        s.set_context(scope, name, kind, text, quote, Some(area))
    });
    match saved {
        Some(Saved::Added) => done(term, &format!("added {name}")),
        Some(Saved::Changed) => done(term, &format!("saved the changes to {name}")),
        Some(Saved::Same) => done(term, &format!("{name} is unchanged")),
        None => {}
    }
}

fn delete<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, e: &ContextEntry) {
    let style = term.borrow().style();
    let question = format!("delete '{}'? [y/N]", e.name);
    let sure = term.borrow_mut().ask(&question, None);
    if !matches!(
        sure.as_deref().map(str::to_ascii_lowercase).as_deref(),
        Some("y" | "yes")
    ) {
        return say_line(term, &style.yellow("[not deleted]"));
    }
    if with_store(agent, term, |s| s.delete_context(&e.project, &e.name)).is_some() {
        done(term, &format!("deleted {}", e.name));
    }
}

fn preload<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, tail: &str) {
    let style = term.borrow().style();
    let Some((name, setting)) = tail.rsplit_once(char::is_whitespace) else {
        return say_line(term, &style.yellow("usage: /context preload <name> on|off"));
    };
    let on = match setting {
        "on" => true,
        "off" => false,
        _ => return say_line(term, &style.yellow("usage: /context preload <name> on|off")),
    };
    let Some(entry) = find(agent, term, name.trim()) else {
        return;
    };
    set_preload(agent, term, &entry, on);
}

fn set_preload<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    entry: &ContextEntry,
    on: bool,
) {
    if with_store(agent, term, |s| {
        s.set_context_preload(&entry.project, &entry.name, on)
    })
    .is_some()
    {
        done(
            term,
            &format!(
                "{} will {}be in every subagent's start",
                entry.name,
                if on { "" } else { "not " }
            ),
        );
    }
}

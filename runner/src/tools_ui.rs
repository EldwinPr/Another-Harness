//! `/tools` (issue 38): what the model is given in this chat.
//!
//! `/tools` lists the tools with a one-line summary, then what a reader and a coder subagent get;
//! `/tools <name>` shows one tool in full (its arguments and the description the model reads).
//!
//! Issue 54: a built-in tool is on or off per chat, like a skill. A tool that is off has no
//! definition in the request. In a terminal `/tools` is a checklist (Enter switches); typed:
//! `/tools on|off NAME`, and `/tools save` makes the tools on now the start of this chat's profile.

use std::io::{BufRead, Write};

use crate::agent::Agent;
use crate::config_ui::{done, error};
use crate::menu::MenuItem;
use crate::repl::{Shared, say_line};
use crate::tool_list::ToolInfo;

/// How much of a description a list line shows.
const SUMMARY_CHARS: usize = 70;

pub fn command<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, rest: &str) {
    let style = term.borrow().style();
    let chat = agent.tool_infos();
    let name = rest.trim();
    let (word, tail) = name
        .split_once(char::is_whitespace)
        .map_or((name, ""), |(w, t)| (w, t.trim()));
    match (word, tail) {
        ("on" | "off", "") => {
            return say_line(term, &style.yellow(&format!("usage: /tools {word} NAME")));
        }
        ("on" | "off", tool) => return switch(agent, term, tool, word == "on"),
        ("save", "") => {
            return match agent.save_tools_to_profile() {
                Ok(profile) => done(
                    term,
                    &format!("the tools on now are how a chat under profile {profile} starts"),
                ),
                Err(why) => error(term, &why),
            };
        }
        _ => {}
    }
    if name.is_empty() && term.borrow().menus_enabled() {
        return checklist(agent, term);
    }
    if name.is_empty() {
        let all = agent.builtin_tools();
        let on = all.iter().filter(|(_, on)| *on).count();
        say_line(
            term,
            &style.bold(&format!(
                "built-in tools, {on} of {} on in this chat:",
                all.len()
            )),
        );
        for (tool, is_on) in &all {
            say_line(
                term,
                &format!(
                    "  {} {}  {}",
                    if *is_on { "[x]" } else { "[ ]" },
                    style.bold(&tool.signature()),
                    style.dim(&tool.summary(SUMMARY_CHARS))
                ),
            );
        }
        let others: Vec<String> = chat
            .iter()
            .filter(|t| !all.iter().any(|(b, _)| b.name == t.name))
            .map(|t| t.name.clone())
            .collect();
        if !others.is_empty() {
            say_line(
                term,
                &style.dim(&format!("from MCP servers (/mcp): {}", others.join(", "))),
            );
        }
        for (who, coder) in [("reader", false), ("coder", true)] {
            let list = agent.subagent_tool_infos(coder);
            if !list.is_empty() {
                let names: Vec<&str> = list.iter().map(|t| t.name.as_str()).collect();
                say_line(
                    term,
                    &style.dim(&format!("a subagent as {who} has: {}", names.join(", "))),
                );
            }
        }
        say_line(
            term,
            &style.dim(
                "/tools on|off NAME switches one in this chat; /tools <name> shows one in full",
            ),
        );
        return;
    }
    let found: Option<ToolInfo> = chat
        .into_iter()
        .chain(agent.subagent_tool_infos(false))
        .chain(agent.subagent_tool_infos(true))
        .find(|t| t.name.eq_ignore_ascii_case(name));
    match found {
        Some(tool) => {
            say_line(term, &style.bold(&tool.signature()));
            say_line(term, &tool.description);
        }
        None => say_line(
            term,
            &style.yellow(&format!("there is no tool '{name}' (see /tools)")),
        ),
    }
}

/// `/tools on NAME` and `/tools off NAME`.
fn switch<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, name: &str, on: bool) {
    let style = term.borrow().style();
    match agent.set_tool(name, on) {
        Err(why) => error(term, &why),
        Ok(change) => {
            done(term, &change.message);
            if let Some(warning) = change.warning {
                say_line(term, &style.yellow(&format!("warning: {warning}")));
            }
        }
    }
}

/// `/tools` in a terminal: Enter switches the highlighted tool on or off in this chat.
fn checklist<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
    let style = term.borrow().style();
    let mut start = 0;
    let mut changed = Vec::new();
    loop {
        let all = agent.builtin_tools();
        let rows: Vec<MenuItem> = all
            .iter()
            .map(|(tool, on)| {
                MenuItem::new(format!("{} {}", if *on { "[x]" } else { "[ ]" }, tool.name))
                    .with_detail(tool.summary(SUMMARY_CHARS))
            })
            .collect();
        let chosen = term.borrow_mut().pick_from(
            "tools: Enter switches a tool on or off in this chat; Esc finishes",
            &rows,
            start,
        );
        let Some(index) = chosen else {
            break;
        };
        let (tool, on) = &all[index];
        start = index;
        match agent.set_tool(&tool.name, !*on) {
            Err(why) => error(term, &why),
            Ok(change) => {
                changed.push(change.message);
                if let Some(warning) = change.warning {
                    say_line(term, &style.yellow(&format!("warning: {warning}")));
                }
            }
        }
    }
    for line in changed {
        say_line(term, &style.green(&line));
    }
}

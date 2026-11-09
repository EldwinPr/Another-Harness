//! `/tools` (issue 38): what the model is given in this chat.
//!
//! `/tools` lists the tools with a one-line summary, then what a reader and a coder subagent get;
//! `/tools <name>` shows one tool in full (its arguments and the description the model reads).

use std::io::{BufRead, Write};

use crate::agent::Agent;
use crate::repl::{Shared, say_line};
use crate::tool_list::ToolInfo;

/// How much of a description a list line shows.
const SUMMARY_CHARS: usize = 70;

pub fn command<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, rest: &str) {
    let style = term.borrow().style();
    let chat = agent.tool_infos();
    let name = rest.trim();
    if name.is_empty() {
        say_line(
            term,
            &style.bold(&format!("tools the model has, {}:", chat.len())),
        );
        for tool in &chat {
            say_line(
                term,
                &format!(
                    "  {}  {}",
                    style.bold(&tool.signature()),
                    style.dim(&tool.summary(SUMMARY_CHARS))
                ),
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
        say_line(term, &style.dim("/tools <name> shows one in full"));
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

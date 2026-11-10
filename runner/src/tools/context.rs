//! `context` and `context_propose` (issue 37): the model reads the rules, decisions and shapes the
//! owner has said, and can only add or change one by asking.
//!
//! `context` never asks. Its description lists the entries there were when the tool was built, and
//! each call lists them again, so an entry added while the program runs is found and named: it
//! costs a few tokens per entry and nothing when there are none (then the tool is not offered).
//! `context_propose` asks every time, in `manual` and in `auto` mode alike, never
//! offers "always", and needs the owner's own words as a quote: no quote means the model is
//! guessing, and a guess is not saved. Both use their own connection to the database.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use kernel::ToolSpec;
use serde::Deserialize;

use super::{Approver, SharedMode, Tool};
use crate::store::Store;
use crate::store_context::{KINDS, Saved, check_kind, check_name, check_quote, check_text};

const READ_SCHEMA: &str = r#"{"type":"object","properties":{"name":{"type":"string","description":"The name of the entry"}},"required":["name"]}"#;
const PROPOSE_SCHEMA: &str = r#"{"type":"object","properties":{"kind":{"type":"string","enum":["rule","decision","shape","guide"],"description":"rule: how to work here. decision: a choice and why. shape: how an area is thought of and which way the owner leans. guide: a longer how-to."},"name":{"type":"string","description":"A short name: letters, digits, . - _"},"text":{"type":"string","description":"What to remember, in a few sentences"},"quote":{"type":"string","description":"The owner's own words that this comes from, copied as said"},"area":{"type":"string","description":"The business area, for a shape (optional)"}},"required":["kind","name","text","quote"]}"#;

/// How much of a text the one-line question shows.
const SHOWN_CHARS: usize = 70;

/// The tools for the main chat: `context` and `context_propose`. The reading tool is always
/// offered, because an entry may be saved while the chat runs: its description cannot list names
/// that do not exist yet, so a call with an unknown name lists them.
pub fn context_tools(
    db: impl Into<PathBuf>,
    project: impl Into<String>,
    approver: Rc<dyn Approver>,
    approval: SharedMode,
) -> Vec<Box<dyn Tool>> {
    let (db, project) = (db.into(), project.into());
    vec![
        dynamic_context_read_tool(db.clone(), project.clone()),
        Box::new(Propose::new(db, project, approver, approval)),
    ]
}

/// Only the reading tool, for a subagent (it cannot propose: nobody could confirm). `None` when
/// the project has no entries.
pub fn context_read_tool(
    db: impl Into<PathBuf>,
    project: impl Into<String>,
) -> Option<Box<dyn Tool>> {
    let (db, project) = (db.into(), project.into());
    let names: Vec<String> = Store::open(&db)
        .ok()?
        .list_context(&project)
        .ok()?
        .into_iter()
        .map(|e| e.name)
        .collect();
    if names.is_empty() {
        return None;
    }
    let description = format!(
        "Get what the owner has said about this project: rules, decisions and how its areas are thought of. Give the name of one entry; the text comes back as stored. Entries: {}.",
        names.join(", ")
    );
    Some(Box::new(Read::new(db, project, &description)))
}

/// A chat's `context`: the description cannot list the entries, because one may be saved while the
/// chat runs, so a call with an unknown name lists them. Always offered, and always a real tool.
pub fn dynamic_context_read_tool(
    db: impl Into<PathBuf>,
    project: impl Into<String>,
) -> Box<dyn Tool> {
    let description = "Get what the owner has said about this project: rules, decisions and how its areas are thought of. Give the name of one entry with the `name` argument; the text comes back as stored, including the owner's own words (quote). To see the entry names, call it with an empty name: the error lists them, and a name typed with the wrong case is accepted. Entries saved during this chat are found the same way.";
    Box::new(Read::new(db.into(), project.into(), description))
}

fn open(db: &Path) -> Result<Store, String> {
    Store::open(db).map_err(|e| format!("error: cannot open the context entries: {e}"))
}

/// A text on one line, cut to `limit` characters.
fn cut(text: &str, limit: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= limit {
        return one_line;
    }
    let mut shown: String = one_line.chars().take(limit).collect();
    shown.push_str("...");
    shown
}

#[derive(Deserialize)]
struct ReadArgs {
    name: String,
}

struct Read {
    spec: ToolSpec,
    db: PathBuf,
    project: String,
}

impl Read {
    /// `description` is the whole description sent to the model.
    fn new(db: PathBuf, project: String, description: &str) -> Self {
        Self {
            spec: ToolSpec::new("context", description, READ_SCHEMA)
                .expect("the schema constant is valid JSON"),
            db,
            project,
        }
    }

    fn read(&self, arguments: &str) -> Result<String, String> {
        let args: ReadArgs = serde_json::from_str(arguments)
            .map_err(|e| format!("error: invalid arguments: {e}"))?;
        let store = open(&self.db)?;
        let wanted = args.name.trim().to_ascii_lowercase();
        let entries = store
            .list_context(&self.project)
            .map_err(|e| format!("error: {e}"))?;
        // The name typed with the wrong case is still that entry: the list is right here.
        let found = if wanted.is_empty() {
            None
        } else {
            store
                .context_entry(&self.project, &args.name)
                .map_err(|e| format!("error: {e}"))?
                .or_else(|| {
                    entries
                        .iter()
                        .find(|e| e.name.to_ascii_lowercase() == wanted)
                        .cloned()
                })
        };
        if let Some(entry) = found {
            let area = if entry.area.is_empty() {
                String::new()
            } else {
                format!(", area {}", entry.area)
            };
            let quote = if entry.quote.is_empty() {
                String::new()
            } else {
                format!("\nthe owner said: {}", entry.quote)
            };
            return Ok(format!(
                "{} ({}{area})\n{}{quote}",
                entry.name, entry.kind, entry.text
            ));
        }
        let names: Vec<String> = entries.into_iter().map(|e| e.name).collect();
        Err(format!(
            "error: no entry named '{}'; the entries are: {}",
            args.name.trim(),
            names.join(", ")
        ))
    }
}

impl Tool for Read {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn run(&self, arguments: &str) -> String {
        self.read(arguments).unwrap_or_else(|text| text)
    }
}

#[derive(Deserialize)]
struct ProposeArgs {
    kind: String,
    name: String,
    text: String,
    #[serde(default)]
    quote: String,
    #[serde(default)]
    area: String,
}

struct Propose {
    spec: ToolSpec,
    db: PathBuf,
    project: String,
    approver: Rc<dyn Approver>,
    approval: SharedMode,
}

impl Propose {
    fn new(db: PathBuf, project: String, approver: Rc<dyn Approver>, approval: SharedMode) -> Self {
        Self {
            spec: ToolSpec::new(
                "context_propose",
                "Propose to remember something the owner just said about this project: a rule, a decision, or how an area is thought of. Copy their words into quote; without their words nothing is proposed. The owner is asked to confirm; nothing is saved without a yes.",
                PROPOSE_SCHEMA,
            )
            .expect("the schema constant is valid JSON"),
            db,
            project,
            approver,
            approval,
        }
    }

    fn propose(&self, arguments: &str) -> Result<String, String> {
        let args: ProposeArgs = serde_json::from_str(arguments)
            .map_err(|e| format!("error: invalid arguments: {e}"))?;
        let kind = check_kind(&args.kind).map_err(|e| format!("error: {e}"))?;
        let name = check_name(&args.name).map_err(|e| format!("error: {e}"))?;
        let text = check_text(&args.text).map_err(|e| format!("error: {e}"))?;
        let quote = check_quote(&args.quote).map_err(|e| format!("error: {e}"))?;
        if quote.is_empty() {
            return Err("error: no quote: only what the owner said is saved, not a guess; ask them to say it".to_owned());
        }
        let mut store = open(&self.db)?;
        let old = store
            .context_entry(&self.project, &name)
            .map_err(|e| format!("error: {e}"))?;
        let (question, detail, full) = match &old {
            Some(entry) if entry.text == text && entry.kind == kind => {
                return Ok(format!("no change: '{}' already says this", entry.name));
            }
            Some(entry) => (
                "change this entry?",
                format!(
                    "{}: \"{}\" -> \"{}\"",
                    entry.name,
                    cut(&entry.text, SHOWN_CHARS),
                    cut(&text, SHOWN_CHARS)
                ),
                format!(
                    "Name: {} ({kind})\n\nNow: {}\n\nProposed: {text}\n\nThe owner said: {quote}",
                    entry.name, entry.text
                ),
            ),
            None => (
                "remember this?",
                format!("{kind} {name}: {}", cut(&text, SHOWN_CHARS * 2)),
                format!("Name: {name} ({kind})\n\n{text}\n\nThe owner said: {quote}"),
            ),
        };
        // Asked in manual mode ("always" is offered); not asked in auto mode.
        if !self.approval.confirm_saving(
            &*self.approver,
            ("context", "saved context"),
            (question, &detail, &full),
        ) {
            return Err(self.approval.denial("denied by user: nothing was changed"));
        }
        // A change keeps the entry where it is (a project's own, or the every-project one).
        let scope = old
            .as_ref()
            .map_or(self.project.as_str(), |e| e.project.as_str());
        match store
            .set_context(
                scope,
                &name,
                &kind,
                &text,
                &quote,
                Some(args.area.as_str()).filter(|a| !a.trim().is_empty()),
            )
            .map_err(|e| format!("error: {e}"))?
        {
            Saved::Added => Ok(format!("saved {kind} '{name}'")),
            Saved::Changed => Ok(format!("changed '{name}'")),
            Saved::Same => Ok(format!("no change: '{name}' already says this")),
        }
    }
}

impl Tool for Propose {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn run(&self, arguments: &str) -> String {
        self.propose(arguments).unwrap_or_else(|text| text)
    }
}

/// The kinds, for the help text of `/context`.
pub fn kinds_text() -> String {
    KINDS.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::Scratch;
    use crate::tools::{Answer, Ask, Mode, shared_mode};
    use std::cell::RefCell;
    use std::collections::VecDeque;

    type Asked = Rc<RefCell<Vec<(String, String, bool, String)>>>;

    struct Scripted {
        answers: RefCell<VecDeque<Answer>>,
        asked: Asked,
    }

    impl Approver for Scripted {
        fn approve(&self, ask: &Ask) -> Answer {
            self.asked.borrow_mut().push((
                ask.question.to_owned(),
                ask.detail.to_owned(),
                ask.always.is_some(),
                ask.full.unwrap_or("").to_owned(),
            ));
            self.answers.borrow_mut().pop_front().unwrap_or(Answer::No)
        }
    }

    fn approver(answers: &[Answer]) -> (Rc<Scripted>, Asked) {
        let asked: Asked = Rc::default();
        let approver = Rc::new(Scripted {
            answers: RefCell::new(answers.iter().copied().collect()),
            asked: Rc::clone(&asked),
        });
        (approver, asked)
    }

    fn call(tools: &[Box<dyn Tool>], name: &str, arguments: serde_json::Value) -> String {
        tools
            .iter()
            .find(|t| t.spec().name == name)
            .unwrap()
            .run(&arguments.to_string())
    }

    fn proposal(name: &str, text: &str, quote: &str) -> serde_json::Value {
        serde_json::json!({"kind":"rule","name":name,"text":text,"quote":quote})
    }

    #[test]
    fn always_covers_the_next_entries_and_auto_mode_asks_nothing() {
        for (tag, mode, first, asked_count) in [
            ("ctx-always", Mode::Manual, Answer::Always, 1),
            ("ctx-auto", Mode::Auto, Answer::No, 0),
        ] {
            let dir = Scratch::new(tag);
            let (a, asked) = approver(&[first]);
            let tools = context_tools(dir.path().join("harness.db"), "/p", a, shared_mode(mode));
            for name in ["r1", "r2", "r3"] {
                let said = call(&tools, "context_propose", proposal(name, "Do x.", "do x"));
                assert_eq!(said, format!("saved rule '{name}'"), "{tag}");
            }
            assert_eq!(asked.borrow().len(), asked_count, "{tag}");
        }
    }

    #[test]
    fn the_chat_tool_is_always_offered_and_finds_an_entry_saved_while_the_chat_runs() {
        let dir = Scratch::new("ctx-tools-offer");
        let db = dir.path().join("harness.db");
        let (a, _) = approver(&[Answer::Yes]);
        let tools = context_tools(&db, "/p", a, shared_mode(Mode::Manual));
        let names: Vec<&str> = tools.iter().map(|t| t.spec().name.as_str()).collect();
        // Offered even with no entries, so a proposal made in this chat can be read back.
        assert_eq!(names, ["context", "context_propose"]);
        // A subagent's tool is still only offered when there are entries.
        assert!(context_read_tool(&db, "/p").is_none());

        assert_eq!(
            call(&tools, "context", serde_json::json!({"name": ""})),
            "error: no entry named ''; the entries are: "
        );

        call(
            &tools,
            "context_propose",
            proposal("db-writes", "Use IMMEDIATE.", "always immediate"),
        );
        let got = call(&tools, "context", serde_json::json!({"name": "db-writes"}));
        assert_eq!(
            got,
            "db-writes (rule)\nUse IMMEDIATE.\nthe owner said: always immediate"
        );

        let (a, _) = approver(&[]);
        let tools = context_tools(&db, "/p", a, shared_mode(Mode::Manual));
        assert!(tools[0].spec().description.contains("the `name` argument"));
        let subagent = context_read_tool(&db, "/p").unwrap();
        assert!(subagent.spec().description.contains("Entries: db-writes."));
        // Another project does not see it.
        assert!(context_read_tool(&db, "/other").is_none());
    }

    #[test]
    fn a_proposal_needs_a_quote_and_a_yes_and_offers_always() {
        let dir = Scratch::new("ctx-tools-propose");
        let db = dir.path().join("harness.db");
        let (a, asked) = approver(&[Answer::No, Answer::Yes, Answer::Yes]);
        let tools = context_tools(&db, "/p", a, shared_mode(Mode::Manual));

        let no_quote = call(&tools, "context_propose", proposal("r1", "Do x.", "  "));
        assert!(no_quote.starts_with("error: no quote"), "{no_quote}");
        assert!(asked.borrow().is_empty(), "nobody was asked");

        let denied = call(&tools, "context_propose", proposal("r1", "Do x.", "do x"));
        assert!(denied.starts_with("denied by user"), "{denied}");
        assert!(
            Store::open(&db)
                .unwrap()
                .list_context("/p")
                .unwrap()
                .is_empty()
        );

        assert_eq!(
            call(&tools, "context_propose", proposal("r1", "Do x.", "do x")),
            "saved rule 'r1'"
        );
        let changed = call(&tools, "context_propose", proposal("R1", "Do y.", "do y"));
        assert_eq!(changed, "changed 'R1'");
        let asked = asked.borrow();
        assert_eq!(asked[1].0, "remember this?");
        assert_eq!(asked[2].0, "change this entry?");
        assert!(asked.iter().all(|a| a.2), "always is offered");
        assert!(asked[2].3.contains("Now: Do x.") && asked[2].3.contains("The owner said: do y"));
    }

    #[test]
    fn reading_gives_the_text_and_the_quote_and_an_unknown_name_lists_the_names() {
        let dir = Scratch::new("ctx-tools-read");
        let db = dir.path().join("harness.db");
        let mut store = Store::open(&db).unwrap();
        store
            .set_context(
                "/p",
                "lean",
                "shape",
                "Determinism means consistent output.",
                "more deterministic output",
                Some("harness"),
            )
            .unwrap();
        store
            .set_context("", "global-rule", "rule", "Keep it small.", "", None)
            .unwrap();
        drop(store);
        let tool = context_read_tool(&db, "/p").unwrap();
        assert!(tool.spec().description.contains("global-rule, lean"));
        let got = tool.run(r#"{"name":"LEAN"}"#);
        assert_eq!(
            got,
            "lean (shape, area harness)\nDeterminism means consistent output.\nthe owner said: more deterministic output"
        );
        let missing = tool.run(r#"{"name":"nope"}"#);
        assert_eq!(
            missing,
            "error: no entry named 'nope'; the entries are: global-rule, lean"
        );
    }
}

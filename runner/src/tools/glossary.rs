//! `glossary` and `glossary_propose` (issue 27): the model reads the user's glossary, and can
//! only change it by asking.
//!
//! `glossary` never asks. `glossary_propose` asks every time, in `manual` and in `auto` mode
//! alike, and never offers "always". Both use their own connection to the database (it is in
//! WAL mode, so the chat's connection and this one do not get in each other's way).

use std::path::{Path, PathBuf};
use std::rc::Rc;

use kernel::ToolSpec;
use serde::Deserialize;

use super::{Approver, SharedMode, Tool};
use crate::store::Store;
use crate::store_glossary::{PARTIAL_MATCHES, Saved, check_definition, check_term};

const LOOKUP_SCHEMA: &str = r#"{"type":"object","properties":{"term":{"type":"string","description":"The project word to look up"}},"required":["term"]}"#;
const PROPOSE_SCHEMA: &str = r#"{"type":"object","properties":{"term":{"type":"string","description":"The project word"},"definition":{"type":"string","description":"What it means in this project, in a few sentences"},"reason":{"type":"string","description":"Why you propose this: what you learned or what is wrong with the stored definition"}},"required":["term","definition","reason"]}"#;

/// How much of a text the one-line question shows.
const SHOWN_CHARS: usize = 70;

/// The two glossary tools, in the order the model is told about them.
/// They work for `project` (the working directory): its words and the ones for every project.
pub fn glossary_tools(
    db: impl Into<PathBuf>,
    project: impl Into<String>,
    approver: Rc<dyn Approver>,
    approval: SharedMode,
) -> Vec<Box<dyn Tool>> {
    let (db, project) = (db.into(), project.into());
    vec![
        Box::new(Lookup::new(db.clone(), project.clone())),
        Box::new(Propose::new(db, project, approver, approval)),
    ]
}

/// Only the lookup, for a subagent (it cannot propose: nobody could confirm).
pub fn glossary_lookup_tool(db: impl Into<PathBuf>, project: impl Into<String>) -> Box<dyn Tool> {
    Box::new(Lookup::new(db.into(), project.into()))
}

fn open(db: &Path, project: &str) -> Result<Store, String> {
    Store::open(db)
        .map(|store| store.with_project(project))
        .map_err(|e| format!("error: cannot open the glossary: {e}"))
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
struct LookupArgs {
    term: String,
}

struct Lookup {
    spec: ToolSpec,
    db: PathBuf,
    project: String,
}

impl Lookup {
    fn new(db: PathBuf, project: String) -> Self {
        Self {
            spec: ToolSpec::new(
                "glossary",
                "Look up a project word in the glossary the user keeps. Gives its definition, or the terms that contain the text.",
                LOOKUP_SCHEMA,
            )
            .expect("the schema constant is valid JSON"),
            db,
            project,
        }
    }

    fn look(&self, arguments: &str) -> Result<String, String> {
        let args: LookupArgs = serde_json::from_str(arguments)
            .map_err(|e| format!("error: invalid arguments: {e}"))?;
        let term = check_term(&args.term).map_err(|e| format!("error: {e}"))?;
        let store = open(&self.db, &self.project)?;
        let found = store
            .glossary_entry(&term)
            .map_err(|e| format!("error: {e}"))?;
        if let Some(entry) = found {
            let area = if entry.area.is_empty() {
                String::new()
            } else {
                format!("\narea: {}", entry.area)
            };
            return Ok(format!(
                "term: {}{area}\ndefinition: {}",
                entry.term, entry.definition
            ));
        }
        let similar = store
            .find_glossary(&term, PARTIAL_MATCHES)
            .map_err(|e| format!("error: {e}"))?;
        if similar.is_empty() {
            return Ok(format!("no glossary entry for '{term}'"));
        }
        let names: Vec<&str> = similar.iter().map(|e| e.term.as_str()).collect();
        Ok(format!(
            "no glossary entry named '{term}'; terms that contain it: {}",
            names.join(", ")
        ))
    }
}

impl Tool for Lookup {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn run(&self, arguments: &str) -> String {
        self.look(arguments).unwrap_or_else(|text| text)
    }
}

#[derive(Deserialize)]
struct ProposeArgs {
    term: String,
    definition: String,
    #[serde(default)]
    reason: String,
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
                "glossary_propose",
                "Propose a new glossary entry, or a corrected definition for an existing word. The user is asked to confirm; nothing is saved without a yes.",
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
        let term = check_term(&args.term).map_err(|e| format!("error: {e}"))?;
        let definition = check_definition(&args.definition).map_err(|e| format!("error: {e}"))?;
        let mut store = open(&self.db, &self.project)?;
        let old = store
            .glossary_entry(&term)
            .map_err(|e| format!("error: {e}"))?;
        let reason = args.reason.trim();
        let (question, detail, full) = match &old {
            Some(entry) if entry.definition == definition => {
                return Ok(format!(
                    "no change: '{}' already has this definition",
                    entry.term
                ));
            }
            Some(entry) => (
                "change this definition in the glossary?",
                format!(
                    "{}: \"{}\" -> \"{}\"",
                    entry.term,
                    cut(&entry.definition, SHOWN_CHARS),
                    cut(&definition, SHOWN_CHARS)
                ),
                format!(
                    "Term: {}\n\nNow: {}\n\nProposed: {definition}\n\nReason: {reason}",
                    entry.term, entry.definition
                ),
            ),
            None => (
                "add this word to the glossary?",
                format!("{term}: {}", cut(&definition, SHOWN_CHARS * 2)),
                format!("Term: {term}\n\nDefinition: {definition}\n\nReason: {reason}"),
            ),
        };
        // Asked in manual mode ("always" is offered); not asked in auto mode.
        if !self.approval.confirm_saving(
            &*self.approver,
            ("glossary", "glossary changes"),
            (question, &detail, &full),
        ) {
            return Err(self.approval.denial("denied by user: nothing was changed"));
        }
        match store
            .set_glossary(&term, &definition, None)
            .map_err(|e| format!("error: {e}"))?
        {
            Saved::Added => Ok(format!("added '{term}' to the glossary")),
            Saved::Changed => Ok(format!("changed the definition of '{term}'")),
            Saved::Same => Ok(format!("no change: '{term}' already has this definition")),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::Scratch;
    use crate::tools::{Answer, Ask, Mode, shared_mode};
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// What was asked: the question, the detail, whether "always" was offered, the full text.
    type Asked = Rc<RefCell<Vec<(String, String, bool, String)>>>;

    /// Answers from a script and keeps what it was asked.
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

    fn rig(tag: &str, answers: &[Answer]) -> (Scratch, Vec<Box<dyn Tool>>, Asked) {
        let dir = Scratch::new(tag);
        let asked: Asked = Rc::default();
        let approver = Rc::new(Scripted {
            answers: RefCell::new(answers.iter().copied().collect()),
            asked: Rc::clone(&asked),
        });
        let tools = glossary_tools(
            dir.path().join("harness.db"),
            "",
            approver,
            shared_mode(Mode::Manual),
        );
        (dir, tools, asked)
    }

    fn call(tools: &[Box<dyn Tool>], name: &str, arguments: serde_json::Value) -> String {
        tools
            .iter()
            .find(|t| t.spec().name == name)
            .unwrap()
            .run(&arguments.to_string())
    }

    fn store_of(dir: &Scratch) -> Store {
        Store::open(&dir.path().join("harness.db")).unwrap()
    }

    #[test]
    fn the_tools_work_for_their_project_and_a_proposal_is_saved_for_it() {
        let dir = Scratch::new("glossary-tools-project");
        let asked: Asked = Rc::default();
        let approver = Rc::new(Scripted {
            answers: RefCell::new([Answer::Yes].into()),
            asked: Rc::clone(&asked),
        });
        let db = dir.path().join("harness.db");
        let tools = glossary_tools(&db, "/repo/a", approver, shared_mode(Mode::Manual));
        let other = glossary_tools(
            &db,
            "/repo/b",
            Rc::new(Scripted {
                answers: RefCell::default(),
                asked: Rc::default(),
            }),
            shared_mode(Mode::Manual),
        );
        Store::open(&db)
            .unwrap()
            .set_glossary("claim", "shared meaning", None)
            .unwrap();
        // The proposal changes the word for /repo/a only.
        let out = call(
            &tools,
            "glossary_propose",
            serde_json::json!({"term": "claim", "definition": "a's meaning", "reason": "r"}),
        );
        assert!(
            out.contains("changed the definition") || out.contains("added"),
            "{out}"
        );
        assert!(
            call(&tools, "glossary", serde_json::json!({"term": "claim"})).contains("a's meaning")
        );
        assert!(
            call(&other, "glossary", serde_json::json!({"term": "claim"}))
                .contains("shared meaning")
        );
        let store = Store::open(&db).unwrap();
        assert_eq!(store.list_all_glossary().unwrap().len(), 2);
    }

    #[test]
    fn the_tools_are_named_for_the_model() {
        let (_dir, tools, _) = rig("glossary-names", &[]);
        let names: Vec<&str> = tools.iter().map(|t| t.spec().name.as_str()).collect();
        assert_eq!(names, ["glossary", "glossary_propose"]);
    }

    #[test]
    fn a_lookup_gives_the_definition_or_similar_terms_or_nothing_and_never_asks() {
        let (dir, tools, asked) = rig("glossary-lookup", &[]);
        {
            let mut store = store_of(&dir);
            store
                .set_glossary("kartu stok", "the stock ledger", Some("inventory"))
                .unwrap();
            store
                .set_glossary("kartu kredit", "credit card", None)
                .unwrap();
        }
        let exact = call(
            &tools,
            "glossary",
            serde_json::json!({"term": "KARTU STOK"}),
        );
        assert_eq!(
            exact,
            "term: kartu stok\narea: inventory\ndefinition: the stock ledger"
        );
        let similar = call(&tools, "glossary", serde_json::json!({"term": "kartu"}));
        assert_eq!(
            similar,
            "no glossary entry named 'kartu'; terms that contain it: kartu kredit, kartu stok"
        );
        let none = call(&tools, "glossary", serde_json::json!({"term": "zzz"}));
        assert_eq!(none, "no glossary entry for 'zzz'");
        assert!(call(&tools, "glossary", serde_json::json!({"term": " "})).starts_with("error"));
        assert!(call(&tools, "glossary", serde_json::json!({})).starts_with("error"));
        assert!(asked.borrow().is_empty());
    }

    fn word(term: &str) -> serde_json::Value {
        serde_json::json!({"term": term, "definition": "a definition", "reason": "said so"})
    }

    #[test]
    fn always_covers_the_next_words_and_auto_mode_asks_nothing() {
        for (tag, mode, first, asked_count) in [
            ("glossary-always", Mode::Manual, Answer::Always, 1),
            ("glossary-auto", Mode::Auto, Answer::No, 0),
        ] {
            let dir = Scratch::new(tag);
            let asked: Asked = Rc::default();
            let approver = Rc::new(Scripted {
                answers: RefCell::new([first].into()),
                asked: Rc::clone(&asked),
            });
            let tools = glossary_tools(
                dir.path().join("harness.db"),
                "",
                approver,
                shared_mode(mode),
            );
            for term in ["one", "two", "three"] {
                assert_eq!(
                    call(&tools, "glossary_propose", word(term)),
                    format!("added '{term}' to the glossary"),
                    "{tag}"
                );
            }
            assert_eq!(asked.borrow().len(), asked_count, "{tag}");
        }
    }

    #[test]
    fn a_new_word_is_saved_only_after_a_yes_and_always_is_offered() {
        let (dir, tools, asked) = rig("glossary-add", &[Answer::Yes]);
        let said = call(
            &tools,
            "glossary_propose",
            serde_json::json!({"term": "claim", "definition": "a request for payment", "reason": "the user said so"}),
        );
        assert_eq!(said, "added 'claim' to the glossary");
        let asked = asked.borrow();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].0, "add this word to the glossary?");
        assert!(asked[0].1.contains("claim: a request for payment"));
        assert!(asked[0].2, "always is offered");
        assert!(asked[0].3.contains("Reason: the user said so"));
        assert_eq!(
            store_of(&dir)
                .glossary_entry("claim")
                .unwrap()
                .unwrap()
                .definition,
            "a request for payment"
        );
    }

    #[test]
    fn a_no_saves_nothing_and_tells_the_model_not_to_retry() {
        let (dir, tools, _) = rig("glossary-no", &[Answer::No]);
        let said = call(
            &tools,
            "glossary_propose",
            serde_json::json!({"term": "claim", "definition": "x", "reason": "r"}),
        );
        assert_eq!(said, "denied by user: nothing was changed");
        assert!(store_of(&dir).glossary_entry("claim").unwrap().is_none());
    }

    #[test]
    fn a_changed_definition_shows_the_old_and_the_new_and_replaces_it() {
        let (dir, tools, asked) = rig("glossary-change", &[Answer::Yes]);
        store_of(&dir)
            .set_glossary("claim", "old meaning", Some("billing"))
            .unwrap();
        let said = call(
            &tools,
            "glossary_propose",
            serde_json::json!({"term": "Claim", "definition": "new meaning", "reason": "it was wrong"}),
        );
        assert_eq!(said, "changed the definition of 'Claim'");
        let asked = asked.borrow();
        assert_eq!(asked[0].0, "change this definition in the glossary?");
        assert_eq!(asked[0].1, "claim: \"old meaning\" -> \"new meaning\"");
        assert!(asked[0].3.contains("Now: old meaning"));
        assert!(asked[0].3.contains("Proposed: new meaning"));
        let entry = store_of(&dir).glossary_entry("claim").unwrap().unwrap();
        assert_eq!(entry.definition, "new meaning");
        assert_eq!(entry.area, "billing");
    }

    #[test]
    fn a_proposal_equal_to_what_is_stored_does_not_ask() {
        let (dir, tools, asked) = rig("glossary-same", &[]);
        store_of(&dir).set_glossary("claim", "same", None).unwrap();
        let said = call(
            &tools,
            "glossary_propose",
            serde_json::json!({"term": "claim", "definition": "same", "reason": "r"}),
        );
        assert!(said.starts_with("no change"), "{said}");
        assert!(asked.borrow().is_empty());
    }

    #[test]
    fn bad_proposals_return_error_text_without_asking() {
        let (_dir, tools, asked) = rig("glossary-bad", &[Answer::Yes]);
        for arguments in [
            serde_json::json!({"term": "", "definition": "d", "reason": "r"}),
            serde_json::json!({"term": "t", "definition": " ", "reason": "r"}),
            serde_json::json!({"term": "two\nlines", "definition": "d", "reason": "r"}),
            serde_json::json!({"term": "t", "definition": "x".repeat(2001), "reason": "r"}),
            serde_json::json!({"definition": "d"}),
        ] {
            let said = call(&tools, "glossary_propose", arguments);
            assert!(said.starts_with("error"), "{said}");
        }
        assert!(asked.borrow().is_empty());
    }
}

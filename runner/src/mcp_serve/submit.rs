//! The tools of the default mode of `--mcp-serve`, the buffer (issue 47): `submit` and
//! `submission_get`.
//!
//! A sender cannot change anything the owner keeps. `submit` checks a proposal with the same
//! `check_*` functions the direct tools and the store use (see `store_buffer.rs`) and queues it in
//! the owner's inbox; the owner accepts or rejects it on the web page or with `/inbox`, and
//! deterministic code applies it then. Three things are kept on purpose:
//!
//! - **The only write is one insert into `buffer_entries`** (`Store::queue_submission`). No curated
//!   table is touched and nothing is switched on, selected or reordered; profiles cannot be
//!   submitted at all.
//! - **Every answer is in fixed words**: `queued as #N`, `already queued as #N`,
//!   `error: <reason>` and `error: the inbox is full: ask the owner to review it`.
//! - **A decider only suggests** (issue 48). When the server was started with one, a new row is
//!   judged after it is stored: a slow or dead provider cannot lose a submission, and a failure
//!   stores nothing and writes one line to stderr (stdout carries the protocol alone). The sender
//!   waits at most the provider's request timeout and is never told the opinion. A resubmission
//!   answered `already queued` does not call the model again.
//! - **`submission_get` exposes the outcome and nothing else**: the status and, when the owner
//!   rejected the entry, the owner's note. An apply error that the owner has not acted on yet,
//!   and the stored text, are not shown.

use serde_json::Value;

use super::{Def, Server, failed, required, string_arg};
use crate::decider::Case;
use crate::store::Store;
use crate::store_buffer::{Checked, Status, Submission, Submitted, current_target};

/// `submit` and `submission_get`, after the read tools in `tools/list`.
pub(super) const SUBMIT_TOOLS: [Def; 2] = [
    Def {
        name: "submit",
        description: "Queues. Propose to add, replace or delete a context entry, a glossary term, a skill, a personality or a system prompt. The owner reviews it and nothing changes until then; check later with submission_get. A new skill, personality or prompt is saved switched off. Profiles cannot be submitted. Answers: 'queued as #N', 'already queued as #N' or 'error: <reason>' (fix and resend).",
        schema: r#"{"type":"object","properties":{"kind":{"type":"string","enum":["context","glossary","skill","personality","prompt"],"description":"What it is for. prompt is a system prompt."},"action":{"type":"string","enum":["set","delete"],"description":"set adds the entry or replaces the one with the same name and project; delete takes no text."},"name":{"type":"string","description":"The entry's name (letters, digits, . - _) or the glossary term"},"text":{"type":"string","description":"The whole text (a glossary term's definition). Not for a delete."},"description":{"type":"string","description":"One line: what a skill, personality or prompt is for"},"context_kind":{"type":"string","enum":["rule","decision","shape","guide"],"description":"A context entry's own kind; needed for a context set. rule: how to work here. decision: a choice and why. shape: how an area is thought of. guide: a longer how-to."},"quote":{"type":"string","description":"A context entry: the owner's own words this comes from, when you have them"},"area":{"type":"string","description":"A context entry or glossary term: the business area"},"project":{"type":"string","description":"A context entry or glossary term: a project's folder path. Default: the folder the server was started in. Empty: for every project."},"why":{"type":"string","description":"Why you propose this (required, at most 1000 characters)"},"source":{"type":"string","description":"Your own name, for example claude-code (required, at most 100 characters)"}},"required":["kind","action","name","why","source"]}"#,
    },
    Def {
        name: "submission_get",
        description: "Read-only. Get the outcome of a submission by its number: pending, applied or rejected, and the owner's note when it was rejected.",
        schema: r#"{"type":"object","properties":{"id":{"type":"integer","description":"The number in 'queued as #N'"}},"required":["id"]}"#,
    },
];

impl Server {
    /// Runs one buffer tool. Only called when the server is in buffer mode.
    pub(super) fn call_submit(&self, name: &str, arguments: &Value) -> Result<String, String> {
        match name {
            "submit" => self.submit(arguments),
            "submission_get" => self.submission_get(arguments),
            other => Err(format!("error: unknown tool '{other}'")),
        }
    }

    fn submit(&self, arguments: &Value) -> Result<String, String> {
        let kind = required(arguments, "kind")?;
        let action = required(arguments, "action")?;
        let name = required(arguments, "name")?;
        let why = string_arg(arguments, "why")?.unwrap_or_default();
        let source = string_arg(arguments, "source")?.unwrap_or_default();
        // Context and glossary belong to a folder; the library kinds do not have one.
        let project = match string_arg(arguments, "project")? {
            Some(given) => given,
            None if matches!(kind.to_lowercase().as_str(), "context" | "glossary") => {
                self.project.clone()
            }
            None => String::new(),
        };
        let raw = Submission {
            kind,
            action,
            name,
            project,
            context_kind: string_arg(arguments, "context_kind")?.unwrap_or_default(),
            text: string_arg(arguments, "text")?.unwrap_or_default(),
            description: string_arg(arguments, "description")?.unwrap_or_default(),
            quote: string_arg(arguments, "quote")?.unwrap_or_default(),
            area: string_arg(arguments, "area")?,
        };
        let checked = Checked::new(&raw, &why, &source).map_err(|e| format!("error: {e}"))?;
        // The one write this server makes in buffer mode: a new row in the inbox.
        let mut store = Store::open_for_writing(&self.db)
            .map_err(|e| format!("error: cannot write the database: {e}"))?;
        match store.queue_submission(&checked).map_err(failed)? {
            Submitted::Queued(id) => {
                drop(store);
                self.suggest(id, &checked);
                Ok(format!("queued as #{id}"))
            }
            Submitted::AlreadyQueued(id) => Ok(format!("already queued as #{id}")),
            Submitted::Full => {
                Err("error: the inbox is full: ask the owner to review it".to_owned())
            }
        }
    }

    /// Asks the decider, if there is one, about the row just queued and stores its opinion. The
    /// row is already stored, so a failure here loses nothing: it is one line on stderr and the
    /// entry has no suggestion.
    fn suggest(&self, id: i64, checked: &Checked) {
        let Some(decider) = &self.decider else {
            return;
        };
        if let Err(why) = self.judge_and_store(decider.as_ref(), id, checked) {
            let why = why.replace(['\n', '\r'], " ");
            eprintln!("decider {}: no suggestion for #{id}: {why}", decider.name());
        }
    }

    fn judge_and_store(
        &self,
        decider: &dyn crate::decider::Decider,
        id: i64,
        checked: &Checked,
    ) -> Result<(), String> {
        let case = {
            let store = Store::open_read_only(&self.db).map_err(|e| e.to_string())?;
            Case {
                submission: checked.submission().clone(),
                why: checked.why().to_owned(),
                source: checked.source().to_owned(),
                stored: current_target(&store, checked.submission())?,
            }
        };
        // No database connection is held while the model answers.
        let opinion = decider.judge(&case).map_err(|e| e.to_string())?;
        let mut store = Store::open_for_writing(&self.db).map_err(|e| e.to_string())?;
        store
            .add_suggestion(id, &decider.name(), &opinion)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn submission_get(&self, arguments: &Value) -> Result<String, String> {
        let id = match arguments.get("id") {
            Some(Value::Number(n)) if n.as_i64().is_some() => n.as_i64().unwrap_or_default(),
            Some(Value::String(text)) => text
                .trim()
                .trim_start_matches('#')
                .parse::<i64>()
                .map_err(|_| "error: 'id' is the number in 'queued as #N'".to_owned())?,
            None | Some(Value::Null) => return Err("error: give 'id'".to_owned()),
            Some(_) => return Err("error: 'id' is the number in 'queued as #N'".to_owned()),
        };
        let store = Store::open_read_only(&self.db)
            .map_err(|e| format!("error: cannot read the database: {e}"))?;
        let Some(row) = store.buffer_entry(id).map_err(failed)? else {
            return Err(format!("error: no submission #{id}"));
        };
        let note = row.decision_note.trim();
        // The note on a pending or applied row is the program's own (an apply error); only the
        // owner's reason for a rejection is the sender's to read.
        if row.status == Status::Rejected && !note.is_empty() {
            Ok(format!("#{id}: rejected: {note}"))
        } else {
            Ok(format!("#{id}: {}", row.status.as_str()))
        }
    }
}

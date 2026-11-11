//! The gotchas and the project work log in `--mcp-serve` (issue 50).
//!
//! - **`gotchas_list`, `gotchas_get`, `logs_list`, `logs_get`** only read, in every mode, through
//!   the same read-only connection as the other read tools.
//! - **`log_add` writes in the buffer and direct modes, and is not queued.** It is the one write
//!   in buffer mode besides the insert into `buffer_entries` (issue 47), allowed because a log can
//!   only add a row of its own table: it cannot change or delete anything the owner keeps, and it
//!   is history the owner reads, not an instruction. The store caps a project at
//!   `MAX_PER_PROJECT` entries. In `--read-only` it is an unknown tool.
//! - **`gotcha_set` and `gotcha_delete`** are the direct mode's writes for gotchas, with the
//!   store's checks and no review. In buffer mode a gotcha goes through `submit`.
//!
//! Answers use fixed words: `logged as #N`, `added`, `replaced`, `no change`, `deleted`.

use serde_json::Value;

use super::{Def, Server, failed, or_none, required, string_arg};
use crate::store_buffer::{check_source, scope};
use crate::store_glossary::Saved;
use crate::store_logs::{LogAdded, WorkLog};

/// `log_add`: after the other tools in buffer mode and direct mode.
pub(super) const LOG_TOOLS: [Def; 1] = [Def {
    name: "log_add",
    description: "Writes. Appended at once and never queued. Add one entry to the project's work log: what you did and why, as you would write a line in pm/log.md. Not a program log. An entry cannot be changed or deleted later; correct one with a new entry that says so. Answers: 'logged as #N' or 'error: <reason>'.",
    schema: r#"{"type":"object","properties":{"tag":{"type":"string","enum":["STATUS","DECISION","DISCOVERY","TODO","NOTE"],"description":"STATUS: something finished or changed state. DECISION: a choice and why. DISCOVERY: something found out. TODO: something left to do. NOTE: anything else."},"title":{"type":"string","description":"One line, at most 100 characters"},"summary":{"type":"string","description":"What happened and why, at most 4000 bytes"},"session":{"type":"integer","description":"The number of the chat the entry is about, if you know it"},"project":{"type":"string","description":"A project's folder path. Default: the folder the server was started in. Empty: for every project."},"source":{"type":"string","description":"Your own name, for example claude-code (required, at most 100 characters)"}},"required":["tag","title","summary","source"]}"#,
}];

/// `gotcha_set` and `gotcha_delete`, after the other write tools in direct mode.
pub(super) const GOTCHA_WRITE_TOOLS: [Def; 2] = [
    Def {
        name: "gotcha_set",
        description: "Writes. Add a gotcha, or replace the one with the same name and project. Say what is true, why, and what to do differently.",
        schema: r#"{"type":"object","properties":{"name":{"type":"string","description":"A short name: letters, digits, . - _"},"type":{"type":"string","enum":["convention","hazard","trap","ownership","decision","tooling","workflow"],"description":"What kind of fact it is"},"text":{"type":"string","description":"At most 2000 bytes"},"project":{"type":"string","description":"A project's folder path. Default: the folder the server was started in. Empty: for every project."},"source":{"type":"string","description":"Your own name, for example claude-code"}},"required":["name","type","text"]}"#,
    },
    Def {
        name: "gotcha_delete",
        description: "Writes. Delete the gotcha with this name in exactly this project (default: the folder the server was started in; pass an empty project for an every-project gotcha).",
        schema: r#"{"type":"object","properties":{"name":{"type":"string","description":"The name of the gotcha"},"project":{"type":"string","description":"A project's folder path. Default: the folder the server was started in."}},"required":["name"]}"#,
    },
];

fn log_head(log: &WorkLog) -> String {
    format!("#{} {} {} {}", log.id, log.day, log.tag, log.title)
}

impl Server {
    pub(super) fn gotchas_list(&self, arguments: &Value) -> Result<String, String> {
        let (store, project) = self.open(arguments)?;
        let kind = match string_arg(arguments, "type")?.filter(|t| !t.is_empty()) {
            Some(kind) => {
                Some(crate::store_gotchas::check_type(&kind).map_err(|e| format!("error: {e}"))?)
            }
            None => None,
        };
        let gotchas = store
            .list_gotchas(&project, kind.as_deref())
            .map_err(failed)?;
        if gotchas.is_empty() {
            return Ok("no gotchas".to_owned());
        }
        let lines: Vec<String> = gotchas
            .iter()
            .map(|g| format!("{} ({})", g.name, g.kind))
            .collect();
        Ok(lines.join("\n"))
    }

    pub(super) fn gotchas_get(&self, arguments: &Value) -> Result<String, String> {
        let wanted = required(arguments, "name")?;
        let (store, project) = self.open(arguments)?;
        if let Some(g) = store.gotcha(&project, &wanted).map_err(failed)? {
            return Ok(format!("{} ({})\n{}", g.name, g.kind, g.text));
        }
        let names: Vec<String> = store
            .list_gotchas(&project, None)
            .map_err(failed)?
            .into_iter()
            .map(|g| g.name)
            .collect();
        Err(format!(
            "error: no gotcha named '{wanted}'; the gotchas are: {}",
            or_none(&names)
        ))
    }

    pub(super) fn logs_list(&self, arguments: &Value) -> Result<String, String> {
        let (store, project) = self.open(arguments)?;
        let tag = match string_arg(arguments, "tag")?.filter(|t| !t.is_empty()) {
            Some(tag) => {
                Some(crate::store_logs::check_tag(&tag).map_err(|e| format!("error: {e}"))?)
            }
            None => None,
        };
        let limit = match arguments.get("limit") {
            None | Some(Value::Null) => 20,
            Some(Value::Number(n)) => match n.as_i64() {
                Some(n) if (1..=crate::store_logs::MAX_LISTED).contains(&n) => n,
                _ => {
                    return Err(format!(
                        "error: 'limit' is 1 to {}",
                        crate::store_logs::MAX_LISTED
                    ));
                }
            },
            Some(_) => return Err("error: 'limit' must be a number".to_owned()),
        };
        let logs = store
            .list_logs(&project, tag.as_deref(), limit)
            .map_err(failed)?;
        if logs.is_empty() {
            return Ok("no log entries".to_owned());
        }
        Ok(logs.iter().map(log_head).collect::<Vec<_>>().join("\n"))
    }

    pub(super) fn logs_get(&self, arguments: &Value) -> Result<String, String> {
        let id = match arguments.get("id") {
            Some(Value::Number(n)) if n.as_i64().is_some() => n.as_i64().unwrap_or_default(),
            Some(Value::String(text)) => text
                .trim()
                .trim_start_matches('#')
                .parse::<i64>()
                .map_err(|_| "error: 'id' is the number of a log entry".to_owned())?,
            None | Some(Value::Null) => return Err("error: give 'id'".to_owned()),
            Some(_) => return Err("error: 'id' is the number of a log entry".to_owned()),
        };
        let (store, project) = self.open(arguments)?;
        let Some(log) = store.log(&project, id).map_err(failed)? else {
            return Err(format!("error: no log entry #{id}"));
        };
        let source = if log.source.is_empty() {
            String::new()
        } else {
            format!("\nwritten by {}", log.source)
        };
        let session = log
            .session
            .map(|s| format!("\nabout chat {s}"))
            .unwrap_or_default();
        Ok(format!(
            "{}\n{}{source}{session}",
            log_head(&log),
            log.summary
        ))
    }

    pub(super) fn log_add(&self, arguments: &Value) -> Result<String, String> {
        let tag = required(arguments, "tag")?;
        let title = required(arguments, "title")?;
        let summary = required(arguments, "summary")?;
        let source =
            check_source(&required(arguments, "source")?).map_err(|e| format!("error: {e}"))?;
        let session = match arguments.get("session") {
            None | Some(Value::Null) => None,
            Some(Value::Number(n)) if n.as_i64().is_some() => n.as_i64(),
            Some(_) => return Err("error: 'session' is a chat's number".to_owned()),
        };
        let project = self.project_of(arguments)?;
        match self
            .writer()?
            .add_log(&project, &tag, &title, &summary, &source, session)
            .map_err(failed)?
        {
            LogAdded::Added(id) => Ok(format!("logged as #{id}")),
            LogAdded::Full => Err("error: the log is full: ask the owner to archive it".to_owned()),
        }
    }

    pub(super) fn gotcha_set(&self, arguments: &Value) -> Result<String, String> {
        let name = required(arguments, "name")?;
        let kind = required(arguments, "type")?;
        let text = required(arguments, "text")?;
        let source = string_arg(arguments, "source")?.unwrap_or_default();
        let project = self.project_of(arguments)?;
        let saved = self
            .writer()?
            .set_gotcha_in(&project, &name, &kind, &text, &source)
            .map_err(failed)?;
        let what = format!("gotcha '{name}' for {}", scope(&project));
        Ok(match saved {
            Saved::Added => format!("added {what}"),
            Saved::Changed => format!("replaced {what}"),
            Saved::Same => format!("no change: {what} already says this"),
        })
    }

    pub(super) fn gotcha_delete(&self, arguments: &Value) -> Result<String, String> {
        let name = required(arguments, "name")?;
        let project = self.project_of(arguments)?;
        let what = format!("gotcha '{name}' for {}", scope(&project));
        if self
            .writer()?
            .delete_gotcha_in(&project, &name)
            .map_err(failed)?
        {
            Ok(format!("deleted {what}"))
        } else {
            Err(format!("error: no {what}"))
        }
    }
}

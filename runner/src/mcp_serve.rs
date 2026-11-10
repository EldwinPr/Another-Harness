//! `--mcp-serve` (issue 45): the program as an MCP server for another harness. What it may do is
//! its [`Mode`], chosen by a startup flag and deliberately not stored in the database, so a
//! registered server's command line says what it can do:
//!
//! - **buffer** (the default, issue 47): the read tools, and `submit` (queues a proposal in the
//!   owner's inbox) and `submission_get`; see `submit.rs`. Nothing curated changes until the owner
//!   accepts the entry.
//! - **read-only** (`--read-only`): only the read tools.
//! - **direct** (`--direct`, issue 46): the read tools and the write tools of [`write`], and no
//!   `submit`.
//!
//! Another harness starts `chat.sh --mcp-serve` and talks to it over standard input and output,
//! one JSON-RPC message per line, the other half of what `mcp.rs` does as a client. It can list
//! and fetch the context entries, glossary terms, skills, personalities, system prompts and
//! profiles in the database. No provider is needed and no chat starts.
//!
//! Three properties are kept on purpose:
//!
//! - **Reads cannot write.** Every read call opens the database with
//!   [`Store::open_read_only`], which creates, migrates and writes nothing and refuses a database
//!   of another schema version. The only code in this server that writes is `write.rs` (reached
//!   only in direct mode) and the one insert into `buffer_entries` in `submit.rs` (reached only
//!   in buffer mode); in read-only mode neither is reachable, a call to them is an unknown tool.
//! - **Deterministic.** An answer is a function of the database: the store's own ordering, no
//!   timestamps, and object keys in sorted order (`serde_json` without `preserve_order`).
//! - **Only protocol on stdout.** Notes and errors go to stderr.
//!
//! The project a context entry or glossary term belongs to is a folder path. What applies in a
//! folder is its own entries plus the every-project ones. Each tool takes an optional `project`;
//! without it the folder the server was started in is used.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::decider::Decider;
use crate::mcp::{ACCEPTED_VERSIONS, PROTOCOL_VERSION};
use crate::store::Store;
use crate::store_context::check_scope;
use crate::store_profiles::Profile;
use crate::store_prompts::{Prompt, PromptType};

mod submit;
mod write;

use submit::SUBMIT_TOOLS;
use write::WRITE_TOOLS;

/// The name this server gives itself in `initialize`.
const SERVER_NAME: &str = "aharness";

/// A tool: its name, what it is for, and the JSON schema of its arguments.
struct Def {
    name: &'static str,
    description: &'static str,
    schema: &'static str,
}

/// The tools, in the order `tools/list` gives them. All of them only read.
const TOOLS: [Def; 8] = [
    Def {
        name: "context_list",
        description: "Read-only. List the rules, decisions, shapes and guides the owner keeps for a project: one line each, name and kind. Fetch one with context_get.",
        schema: r#"{"type":"object","properties":{"project":{"type":"string","description":"A project's folder path. Default: the folder the server was started in. Empty: only the entries for every project."}}}"#,
    },
    Def {
        name: "context_get",
        description: "Read-only. Get one context entry by name (case does not matter): its text, and the owner's own words when it has them.",
        schema: r#"{"type":"object","properties":{"name":{"type":"string","description":"The name of the entry"},"project":{"type":"string","description":"A project's folder path. Default: the folder the server was started in."}},"required":["name"]}"#,
    },
    Def {
        name: "glossary_list",
        description: "Read-only. List the project words the owner keeps in the glossary: one line each, term and area. Fetch one with glossary_get.",
        schema: r#"{"type":"object","properties":{"project":{"type":"string","description":"A project's folder path. Default: the folder the server was started in. Empty: only the words for every project."}}}"#,
    },
    Def {
        name: "glossary_get",
        description: "Read-only. Get the definition of one project word (case does not matter). A word that is not there is an error that lists the terms containing it.",
        schema: r#"{"type":"object","properties":{"term":{"type":"string","description":"The project word"},"project":{"type":"string","description":"A project's folder path. Default: the folder the server was started in."}},"required":["term"]}"#,
    },
    Def {
        name: "library_list",
        description: "Read-only. List the skills, personalities or system prompts in the library: name and one-line description. Fetch one with library_get.",
        schema: r#"{"type":"object","properties":{"type":{"type":"string","enum":["skill","personality","prompt"],"description":"prompt is a system prompt"}},"required":["type"]}"#,
    },
    Def {
        name: "library_get",
        description: "Read-only. Get one skill, personality or system prompt by name: its description and its whole text.",
        schema: r#"{"type":"object","properties":{"type":{"type":"string","enum":["skill","personality","prompt"],"description":"prompt is a system prompt"},"name":{"type":"string","description":"The name of the item"}},"required":["type","name"]}"#,
    },
    Def {
        name: "profile_list",
        description: "Read-only. List the profiles (named starting setups for a chat): name and one-line description.",
        schema: r#"{"type":"object","properties":{}}"#,
    },
    Def {
        name: "profile_get",
        description: "Read-only. Get one profile by name (case does not matter): its system prompts, the skills it turns on and its personality.",
        schema: r#"{"type":"object","properties":{"name":{"type":"string","description":"The name of the profile"}},"required":["name"]}"#,
    },
];

/// JSON-RPC error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// What a server may do (see the top of this file).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Only the read tools.
    ReadOnly,
    /// The read tools, `submit` and `submission_get`: the default.
    Buffer,
    /// The read tools and the write tools.
    Direct,
}

/// A server over one database file: read-only, unless it was made [`buffer`](Server::buffer) or
/// [`direct`](Server::direct).
pub struct Server {
    db: PathBuf,
    project: String,
    mode: Mode,
    /// Suggests accept, reject or unsure for each new submission (issue 48); buffer mode only.
    decider: Option<Box<dyn Decider>>,
}

/// Runs the server on the real standard input and output until the client closes its end.
/// Fails at once, before reading a line, when the database cannot be read (or, in buffer and
/// direct mode, written).
///
/// A `decider` (issue 48) only goes with the buffer mode; in another mode it is an error.
pub fn run(
    db: &Path,
    project: &str,
    mode: Mode,
    decider: Option<Box<dyn Decider>>,
) -> Result<(), String> {
    if decider.is_some() && mode != Mode::Buffer {
        return Err("a decider goes with the buffer mode only".to_owned());
    }
    let mut server = Server::new(db, project)?;
    if let Some(decider) = decider {
        server = server.decider(decider);
    }
    match mode {
        Mode::ReadOnly => {}
        Mode::Buffer => server = server.buffer()?,
        Mode::Direct => server = server.direct()?,
    }
    let stdin = io::stdin();
    let stdout = io::stdout();
    server
        .serve(stdin.lock(), stdout.lock())
        .map_err(|e| format!("the connection failed: {e}"))
}

impl Server {
    /// A server for the database at `db`, with `project` as the folder tools use when they are
    /// not given one. The database is opened once here so a missing or wrong-version file is
    /// reported at the start, not by the first call.
    pub fn new(db: &Path, project: &str) -> Result<Self, String> {
        Store::open_read_only(db).map_err(|e| {
            format!(
                "cannot read {}: {e} (start the chat once to create it, or set HARNESS_LOG_DIR)",
                db.display()
            )
        })?;
        Ok(Self {
            db: db.to_path_buf(),
            project: project.to_owned(),
            mode: Mode::ReadOnly,
            decider: None,
        })
    }

    /// Has `decider` suggest accept, reject or unsure for each submission this server queues
    /// (issue 48). The suggestion is stored next to the entry and the sender never sees it; it
    /// does nothing in a mode that has no `submit`.
    pub fn decider(mut self, decider: Box<dyn Decider>) -> Self {
        self.decider = Some(decider);
        self
    }

    /// Makes the server offer the write tools too. Checks at once that the database can be
    /// opened for writing, so a read-only file or another schema version is reported at the start.
    pub fn direct(mut self) -> Result<Self, String> {
        self.can_write("--direct needs a database this program can change")?;
        self.mode = Mode::Direct;
        Ok(self)
    }

    /// Makes the server offer `submit` and `submission_get` (issue 47). Checks at once that the
    /// database can be opened for writing, as `direct` does: a submission is a row in it.
    pub fn buffer(mut self) -> Result<Self, String> {
        self.can_write(
            "the inbox needs a database this program can change; --read-only serves the reads alone",
        )?;
        self.mode = Mode::Buffer;
        Ok(self)
    }

    /// Whether the database can be opened for writing, with `hint` after the reason when not.
    fn can_write(&self, hint: &str) -> Result<(), String> {
        let cannot = |why: String| format!("cannot write {}: {why} ({hint})", self.db.display());
        Store::open_for_writing(&self.db).map_err(|e| cannot(e.to_string()))?;
        // SQLite opens a file it may not write read-only without saying so; ask the file system.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&self.db)
            .map_err(|e| cannot(e.to_string()))?;
        Ok(())
    }

    /// The tools this server offers, in the order `tools/list` gives them.
    fn offered(&self) -> impl Iterator<Item = &'static Def> {
        let more: &'static [Def] = match self.mode {
            Mode::ReadOnly => &[],
            Mode::Buffer => &SUBMIT_TOOLS,
            Mode::Direct => &WRITE_TOOLS,
        };
        TOOLS.iter().chain(more)
    }

    /// Answers every line of `input` on `output` until `input` ends. A blank line is skipped.
    pub fn serve(&self, mut input: impl BufRead, mut output: impl Write) -> io::Result<()> {
        let mut raw = Vec::new();
        loop {
            raw.clear();
            if input.read_until(b'\n', &mut raw)? == 0 {
                return Ok(());
            }
            let reply = match std::str::from_utf8(&raw) {
                Ok(line) if line.trim().is_empty() => continue,
                Ok(line) => self.handle(line.trim()),
                Err(_) => Some(error(&Value::Null, PARSE_ERROR, "the line is not UTF-8")),
            };
            if let Some(reply) = reply {
                let text = serde_json::to_string(&reply).expect("a JSON value serialises");
                output.write_all(text.as_bytes())?;
                output.write_all(b"\n")?;
                output.flush()?;
            }
        }
    }

    /// The answer to one message, or `None` when it needs none (a notification, or an answer from
    /// the client to a request this server never sends).
    fn handle(&self, line: &str) -> Option<Value> {
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(e) => {
                return Some(error(
                    &Value::Null,
                    PARSE_ERROR,
                    &format!("parse error: {e}"),
                ));
            }
        };
        let Some(object) = message.as_object() else {
            return Some(error(
                &Value::Null,
                INVALID_REQUEST,
                "a message is a JSON object",
            ));
        };
        // No `id` means a notification: it is never answered, even when it is unknown.
        let id = object.get("id")?;
        // A message with an id but no method is an answer to a request this server never made.
        let method = object.get("method").and_then(Value::as_str)?;
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        Some(match method {
            "initialize" => ok(id, initialize(&params)),
            "ping" => ok(id, json!({})),
            "tools/list" => ok(id, self.tool_list()),
            "tools/call" => self.tools_call(id, &params),
            other => error(id, METHOD_NOT_FOUND, &format!("method not found: {other}")),
        })
    }

    fn tools_call(&self, id: &Value, params: &Value) -> Value {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return error(id, INVALID_PARAMS, "tools/call needs the name of a tool");
        };
        if !self.offered().any(|tool| tool.name == name) {
            return error(id, INVALID_PARAMS, &format!("unknown tool '{name}'"));
        }
        let arguments = match params.get("arguments") {
            None | Some(Value::Null) => json!({}),
            Some(arguments) if arguments.is_object() => arguments.clone(),
            Some(_) => return error(id, INVALID_PARAMS, "arguments is a JSON object"),
        };
        let (text, failed) = match self.call(name, &arguments) {
            Ok(text) => (text, false),
            Err(text) => (text, true),
        };
        ok(
            id,
            json!({"content": [{"type": "text", "text": text}], "isError": failed}),
        )
    }

    /// Runs one tool. `Err` is a failure the caller can read and fix, not a protocol error.
    fn call(&self, name: &str, arguments: &Value) -> Result<String, String> {
        match name {
            "context_list" => self.context_list(arguments),
            "context_get" => self.context_get(arguments),
            "glossary_list" => self.glossary_list(arguments),
            "glossary_get" => self.glossary_get(arguments),
            "library_list" => self.library_list(arguments),
            "library_get" => self.library_get(arguments),
            "profile_list" => self.profile_list(),
            "profile_get" => self.profile_get(arguments),
            other if self.mode == Mode::Direct => self.call_write(other, arguments),
            other if self.mode == Mode::Buffer => self.call_submit(other, arguments),
            other => Err(format!("error: unknown tool '{other}'")),
        }
    }

    /// The project in the arguments (empty means every project), or the server's own.
    fn project_of(&self, arguments: &Value) -> Result<String, String> {
        match string_arg(arguments, "project")? {
            Some(given) => check_scope(&given).map_err(|e| format!("error: {e}")),
            None => Ok(self.project.clone()),
        }
    }

    /// A fresh read-only connection, for the project in the arguments or the server's own.
    fn open(&self, arguments: &Value) -> Result<(Store, String), String> {
        let project = self.project_of(arguments)?;
        let store = Store::open_read_only(&self.db)
            .map_err(|e| format!("error: cannot read the database: {e}"))?
            .with_project(project.clone());
        Ok((store, project))
    }

    fn context_list(&self, arguments: &Value) -> Result<String, String> {
        let (store, project) = self.open(arguments)?;
        let entries = store.list_context(&project).map_err(failed)?;
        if entries.is_empty() {
            return Ok("no context entries".to_owned());
        }
        let lines: Vec<String> = entries
            .iter()
            .map(|e| {
                let area = if e.area.is_empty() {
                    String::new()
                } else {
                    format!(", area {}", e.area)
                };
                format!("{} ({}{area})", e.name, e.kind)
            })
            .collect();
        Ok(lines.join("\n"))
    }

    fn context_get(&self, arguments: &Value) -> Result<String, String> {
        let wanted = required(arguments, "name")?;
        let (store, project) = self.open(arguments)?;
        if let Some(entry) = store.context_entry(&project, &wanted).map_err(failed)? {
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
        let names: Vec<String> = store
            .list_context(&project)
            .map_err(failed)?
            .into_iter()
            .map(|e| e.name)
            .collect();
        Err(format!(
            "error: no entry named '{wanted}'; the entries are: {}",
            or_none(&names)
        ))
    }

    fn glossary_list(&self, arguments: &Value) -> Result<String, String> {
        let (store, _) = self.open(arguments)?;
        let entries = store.list_glossary().map_err(failed)?;
        if entries.is_empty() {
            return Ok("no glossary terms".to_owned());
        }
        let lines: Vec<String> = entries
            .iter()
            .map(|e| {
                if e.area.is_empty() {
                    e.term.clone()
                } else {
                    format!("{} (area {})", e.term, e.area)
                }
            })
            .collect();
        Ok(lines.join("\n"))
    }

    fn glossary_get(&self, arguments: &Value) -> Result<String, String> {
        let wanted = required(arguments, "term")?;
        let (store, _) = self.open(arguments)?;
        // The store matches the term as typed; the list is right here for the other case.
        let found = match store.glossary_entry(&wanted).map_err(failed)? {
            Some(entry) => Some(entry),
            None => store
                .list_glossary()
                .map_err(failed)?
                .into_iter()
                .find(|e| e.term.eq_ignore_ascii_case(&wanted)),
        };
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
        let similar: Vec<String> = store
            .find_glossary(&wanted, 20)
            .map_err(failed)?
            .into_iter()
            .map(|e| e.term)
            .collect();
        Err(format!(
            "error: no glossary term '{wanted}'; terms that contain it: {}",
            or_none(&similar)
        ))
    }

    fn library_list(&self, arguments: &Value) -> Result<String, String> {
        let kind = kind_arg(arguments)?;
        let (store, _) = self.open(arguments)?;
        let items: Vec<Prompt> = store
            .list_prompts()
            .map_err(failed)?
            .into_iter()
            .filter(|p| p.prompt_type == kind)
            .collect();
        if items.is_empty() {
            return Ok(format!("no {} saved", plural(kind)));
        }
        let lines: Vec<String> = items
            .iter()
            .map(|p| {
                let on = match (kind, p.enabled) {
                    (PromptType::System, true) => " [on]",
                    (PromptType::System, false) => " [off]",
                    _ => "",
                };
                if p.description.is_empty() {
                    format!("{}{on}", p.name)
                } else {
                    format!("{}{on} - {}", p.name, p.description)
                }
            })
            .collect();
        Ok(lines.join("\n"))
    }

    fn library_get(&self, arguments: &Value) -> Result<String, String> {
        let kind = kind_arg(arguments)?;
        let wanted = required(arguments, "name")?;
        let (store, _) = self.open(arguments)?;
        let all = store.list_prompts().map_err(failed)?;
        let prompt = find_item(&all, kind, &wanted)?;
        let on = match (kind, prompt.enabled) {
            (PromptType::System, true) => ", on",
            (PromptType::System, false) => ", off",
            _ => "",
        };
        Ok(format!(
            "{} ({}{on})\nDescription: {}\n\n{}",
            prompt.name,
            noun(kind),
            prompt.description,
            prompt.text
        ))
    }

    fn profile_list(&self) -> Result<String, String> {
        let (store, _) = self.open(&json!({}))?;
        let profiles = store.list_profiles().map_err(failed)?;
        if profiles.is_empty() {
            return Ok("no profiles".to_owned());
        }
        let lines: Vec<String> = profiles
            .iter()
            .map(|p| {
                if p.description.is_empty() {
                    p.name.clone()
                } else {
                    format!("{} - {}", p.name, p.description)
                }
            })
            .collect();
        Ok(lines.join("\n"))
    }

    fn profile_get(&self, arguments: &Value) -> Result<String, String> {
        let wanted = required(arguments, "name")?;
        let (store, _) = self.open(arguments)?;
        let profiles = store.list_profiles().map_err(failed)?;
        let profile = find_profile(&profiles, &wanted)?;
        Ok(describe(profile))
    }
}

/// The item of this kind called `wanted` (case does not matter), or why there is none.
fn find_item<'a>(all: &'a [Prompt], kind: PromptType, wanted: &str) -> Result<&'a Prompt, String> {
    let found = all
        .iter()
        .find(|p| p.prompt_type == kind && p.name == wanted)
        .or_else(|| {
            all.iter()
                .find(|p| p.prompt_type == kind && p.name.eq_ignore_ascii_case(wanted))
        });
    if let Some(prompt) = found {
        return Ok(prompt);
    }
    // Another kind with that name is worth saying: the caller probably mixed them up.
    if let Some(other) = all.iter().find(|p| p.name.eq_ignore_ascii_case(wanted)) {
        return Err(format!(
            "error: {} is a {}, not a {}",
            other.name,
            noun(other.prompt_type),
            noun(kind)
        ));
    }
    let names: Vec<String> = all
        .iter()
        .filter(|p| p.prompt_type == kind)
        .map(|p| p.name.clone())
        .collect();
    Err(format!(
        "error: no {} named '{wanted}'; the {} are: {}",
        noun(kind),
        plural(kind),
        or_none(&names)
    ))
}

/// The profile called `wanted` (case does not matter), or why there is none.
fn find_profile<'a>(all: &'a [Profile], wanted: &str) -> Result<&'a Profile, String> {
    all.iter()
        .find(|p| p.name.eq_ignore_ascii_case(wanted))
        .ok_or_else(|| {
            let names: Vec<String> = all.iter().map(|p| p.name.clone()).collect();
            format!(
                "error: no profile named '{wanted}'; the profiles are: {}",
                or_none(&names)
            )
        })
}

fn describe(p: &Profile) -> String {
    format!(
        "{}\nDescription: {}\nSystem prompts: {}\nSkills on: {}\nPersonality: {}",
        p.name,
        p.description,
        or_none(&p.prompts),
        or_none(&p.skills),
        p.personality.as_deref().unwrap_or("none")
    )
}

/// The answer to `initialize`: the client's protocol version when it is one this program speaks,
/// else the newest.
fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = asked
        .filter(|v| ACCEPTED_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
    })
}

impl Server {
    fn tool_list(&self) -> Value {
        let tools: Vec<Value> = self
            .offered()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": serde_json::from_str::<Value>(tool.schema)
                        .expect("the schema constant is valid JSON"),
                })
            })
            .collect();
        json!({"tools": tools})
    }
}

fn ok(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn failed(e: crate::store::StoreError) -> String {
    format!("error: {e}")
}

/// A string argument, trimmed; `None` when it is absent or null.
fn string_arg(arguments: &Value, key: &str) -> Result<Option<String>, String> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.trim().to_owned())),
        Some(_) => Err(format!("error: '{key}' must be a string")),
    }
}

/// A string argument that has to be there and say something.
fn required(arguments: &Value, key: &str) -> Result<String, String> {
    string_arg(arguments, key)?
        .filter(|text| !text.is_empty())
        .ok_or_else(|| format!("error: give '{key}'"))
}

/// The `type` argument of the library tools.
fn kind_arg(arguments: &Value) -> Result<PromptType, String> {
    match required(arguments, "type")?.as_str() {
        "skill" => Ok(PromptType::Skill),
        "personality" => Ok(PromptType::Personality),
        "prompt" => Ok(PromptType::System),
        other => Err(format!(
            "error: type is skill, personality or prompt, not '{other}'"
        )),
    }
}

/// What the owner calls this kind.
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

fn or_none(names: &[String]) -> String {
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

#[cfg(test)]
mod decider_tests;
#[cfg(test)]
mod submit_tests;
#[cfg(test)]
mod write_tests;

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::tools::testutil::Scratch;

    pub(super) const PROJECT: &str = "/work/app";

    /// A database with a little of everything, in a scratch folder.
    pub(super) fn seeded(tag: &str) -> (Scratch, PathBuf) {
        let dir = Scratch::new(tag);
        let path = dir.path().join("harness.db");
        let mut store = Store::open(&path).unwrap();
        store
            .set_context("", "style", "rule", "Write plainly.", "keep it plain", None)
            .unwrap();
        store
            .set_context(
                PROJECT,
                "style",
                "rule",
                "App style: short lines.",
                "",
                None,
            )
            .unwrap();
        store
            .set_context("/other", "only-other", "decision", "Elsewhere.", "", None)
            .unwrap();
        store
            .set_context(
                "",
                "billing-view",
                "shape",
                "Money is whole cents.",
                "cents",
                Some("billing"),
            )
            .unwrap();
        store
            .set_glossary_in("", "ledger", "The book of record.", Some("billing"))
            .unwrap();
        store
            .set_glossary_in(PROJECT, "settled order", "An order that is paid.", None)
            .unwrap();
        store
            .set_glossary_in("/other", "elsewhere word", "Not here.", None)
            .unwrap();
        store
            .add_prompt("house", "the house prompt", "You are careful.", None)
            .unwrap();
        store
            .add_prompt_of_type(
                "terse",
                "short answers",
                "Be brief.",
                None,
                PromptType::Skill,
            )
            .unwrap();
        store
            .add_prompt_of_type("calm", "", "Stay calm.", None, PromptType::Personality)
            .unwrap();
        store.add_profile("work", "the daily setup").unwrap();
        store
            .set_profile_prompts("work", &["house".to_owned()])
            .unwrap();
        store
            .set_profile_skills("work", &["terse".to_owned()])
            .unwrap();
        store.set_profile_personality("work", Some("calm")).unwrap();
        drop(store);
        (dir, path)
    }

    /// Sends the lines to a server and returns what it wrote, one value per answer.
    pub(super) fn session(path: &Path, project: &str, lines: &[Value]) -> Vec<Value> {
        let input: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let mut output = Vec::new();
        Server::new(path, project)
            .unwrap()
            .serve(Cursor::new(input), &mut output)
            .unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    pub(super) fn request(id: u64, method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    }

    /// Calls one tool and returns its text and whether it was a failure.
    pub(super) fn call(path: &Path, project: &str, tool: &str, arguments: Value) -> (String, bool) {
        let reply = session(
            path,
            project,
            &[request(
                1,
                "tools/call",
                json!({"name": tool, "arguments": arguments}),
            )],
        );
        let result = &reply[0]["result"];
        (
            result["content"][0]["text"].as_str().unwrap().to_owned(),
            result["isError"].as_bool().unwrap(),
        )
    }

    #[test]
    fn initialize_answers_the_clients_version_when_it_is_known_else_the_newest() {
        let (_dir, path) = seeded("serve-init");
        let reply = session(
            &path,
            "",
            &[
                request(1, "initialize", json!({"protocolVersion": "2024-11-05"})),
                request(2, "initialize", json!({"protocolVersion": "1999-01-01"})),
                request(3, "initialize", json!({})),
            ],
        );
        assert_eq!(reply[0]["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(reply[1]["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(reply[2]["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(reply[0]["result"]["serverInfo"]["name"], "aharness");
        assert_eq!(reply[0]["result"]["capabilities"], json!({"tools": {}}));
        assert_eq!(reply[0]["id"], 1);
    }

    #[test]
    fn a_notification_gets_no_answer_and_ping_gets_an_empty_one() {
        let (_dir, path) = seeded("serve-notify");
        let reply = session(
            &path,
            "",
            &[
                json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                json!({"jsonrpc": "2.0", "method": "no/such/notification"}),
                request(7, "ping", Value::Null),
            ],
        );
        assert_eq!(reply.len(), 1);
        assert_eq!(reply[0]["id"], 7);
        assert_eq!(reply[0]["result"], json!({}));
    }

    #[test]
    fn the_tool_list_is_the_eight_read_tools_with_valid_schemas() {
        let (_dir, path) = seeded("serve-list");
        let reply = session(&path, "", &[request(1, "tools/list", Value::Null)]);
        let tools = reply[0]["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "context_list",
                "context_get",
                "glossary_list",
                "glossary_get",
                "library_list",
                "library_get",
                "profile_list",
                "profile_get"
            ]
        );
        for tool in tools {
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert!(
                tool["description"]
                    .as_str()
                    .unwrap()
                    .starts_with("Read-only.")
            );
        }
        // Nothing that could change the database is offered, by name.
        for name in names {
            for verb in [
                "add", "edit", "delete", "set", "propose", "submit", "write", "save",
            ] {
                assert!(!name.contains(verb), "{name} looks like a write");
            }
        }
    }

    #[test]
    fn context_tools_follow_the_project_and_its_own_entry_hides_the_shared_one() {
        let (_dir, path) = seeded("serve-context");
        let (list, failed) = call(&path, PROJECT, "context_list", json!({}));
        assert!(!failed);
        assert_eq!(list, "billing-view (shape, area billing)\nstyle (rule)");
        let (entry, _) = call(&path, PROJECT, "context_get", json!({"name": "STYLE"}));
        assert_eq!(entry, "style (rule)\nApp style: short lines.");
        // With no project, only the every-project entries apply, and the owner's words are shown.
        let (entry, _) = call(&path, "", "context_get", json!({"name": "style"}));
        assert_eq!(
            entry,
            "style (rule)\nWrite plainly.\nthe owner said: keep it plain"
        );
        // The argument beats the folder the server started in.
        let (other, _) = call(&path, PROJECT, "context_list", json!({"project": "/other"}));
        assert!(other.contains("only-other (decision)"), "{other}");
        let (shared, _) = call(&path, PROJECT, "context_list", json!({"project": ""}));
        assert!(!shared.contains("only-other"), "{shared}");
    }

    #[test]
    fn a_context_miss_is_a_failure_that_lists_the_names() {
        let (_dir, path) = seeded("serve-context-miss");
        let (text, failed) = call(&path, PROJECT, "context_get", json!({"name": "nope"}));
        assert!(failed);
        assert_eq!(
            text,
            "error: no entry named 'nope'; the entries are: billing-view, style"
        );
        let (text, failed) = call(&path, PROJECT, "context_get", json!({"name": "  "}));
        assert!(failed);
        assert_eq!(text, "error: give 'name'");
        let (text, failed) = call(&path, PROJECT, "context_get", json!({"name": 5}));
        assert!(failed);
        assert_eq!(text, "error: 'name' must be a string");
    }

    #[test]
    fn glossary_tools_find_a_term_in_any_case_and_a_miss_lists_similar_ones() {
        let (_dir, path) = seeded("serve-glossary");
        let (list, _) = call(&path, PROJECT, "glossary_list", json!({}));
        assert_eq!(list, "ledger (area billing)\nsettled order");
        let (found, failed) = call(&path, PROJECT, "glossary_get", json!({"term": "LEDGER"}));
        assert!(!failed);
        assert_eq!(
            found,
            "term: ledger\narea: billing\ndefinition: The book of record."
        );
        let (miss, failed) = call(&path, PROJECT, "glossary_get", json!({"term": "settled"}));
        assert!(failed);
        assert_eq!(
            miss,
            "error: no glossary term 'settled'; terms that contain it: settled order"
        );
        let (miss, _) = call(&path, PROJECT, "glossary_get", json!({"term": "zzz"}));
        assert_eq!(
            miss,
            "error: no glossary term 'zzz'; terms that contain it: none"
        );
        // Another project's word is not here unless asked for.
        let (text, failed) = call(
            &path,
            PROJECT,
            "glossary_get",
            json!({"term": "elsewhere word"}),
        );
        assert!(failed, "{text}");
        let (text, failed) = call(
            &path,
            PROJECT,
            "glossary_get",
            json!({"term": "elsewhere word", "project": "/other"}),
        );
        assert!(!failed, "{text}");
    }

    #[test]
    fn library_tools_list_and_get_each_kind_in_full() {
        let (_dir, path) = seeded("serve-library");
        let (skills, _) = call(&path, "", "library_list", json!({"type": "skill"}));
        assert_eq!(skills, "terse - short answers");
        let (people, _) = call(&path, "", "library_list", json!({"type": "personality"}));
        assert_eq!(people, "calm");
        let (prompts, _) = call(&path, "", "library_list", json!({"type": "prompt"}));
        assert!(
            prompts.contains("house [") && prompts.contains("the house prompt"),
            "{prompts}"
        );
        let (one, failed) = call(
            &path,
            "",
            "library_get",
            json!({"type": "skill", "name": "Terse"}),
        );
        assert!(!failed);
        assert_eq!(
            one,
            "terse (skill)\nDescription: short answers\n\nBe brief."
        );
        // A long text comes back whole.
        let long = "word ".repeat(2000);
        let mut store = Store::open(&path).unwrap();
        store
            .add_prompt_of_type("long", "", &long, None, PromptType::Skill)
            .unwrap();
        drop(store);
        let (whole, _) = call(
            &path,
            "",
            "library_get",
            json!({"type": "skill", "name": "long"}),
        );
        assert!(whole.ends_with(&long), "the text was cut");
    }

    #[test]
    fn a_library_miss_says_which_kind_it_is_or_lists_the_names() {
        let (_dir, path) = seeded("serve-library-miss");
        let (text, failed) = call(
            &path,
            "",
            "library_get",
            json!({"type": "skill", "name": "calm"}),
        );
        assert!(failed);
        assert_eq!(text, "error: calm is a personality, not a skill");
        let (text, _) = call(
            &path,
            "",
            "library_get",
            json!({"type": "skill", "name": "nope"}),
        );
        assert_eq!(text, "error: no skill named 'nope'; the skills are: terse");
        let (text, failed) = call(&path, "", "library_list", json!({"type": "tool"}));
        assert!(failed);
        assert_eq!(
            text,
            "error: type is skill, personality or prompt, not 'tool'"
        );
        let (text, failed) = call(&path, "", "library_list", json!({}));
        assert!(failed);
        assert_eq!(text, "error: give 'type'");
    }

    #[test]
    fn profile_tools_list_and_describe() {
        let (_dir, path) = seeded("serve-profile");
        let (list, _) = call(&path, "", "profile_list", json!({}));
        assert!(
            list.lines().any(|l| l == "work - the daily setup"),
            "{list}"
        );
        let (one, failed) = call(&path, "", "profile_get", json!({"name": "WORK"}));
        assert!(!failed);
        assert_eq!(
            one,
            "work\nDescription: the daily setup\nSystem prompts: house\nSkills on: terse\nPersonality: calm"
        );
        let (miss, failed) = call(&path, "", "profile_get", json!({"name": "nope"}));
        assert!(failed);
        assert!(
            miss.starts_with("error: no profile named 'nope'; the profiles are: "),
            "{miss}"
        );
    }

    #[test]
    fn bad_messages_get_protocol_errors_and_the_server_goes_on() {
        let (_dir, path) = seeded("serve-errors");
        let input = [
            "this is not json".to_owned(),
            "[1, 2]".to_owned(),
            String::new(),
            request(1, "no/such/method", Value::Null).to_string(),
            request(2, "tools/call", json!({"name": "context_set"})).to_string(),
            request(3, "tools/call", json!({})).to_string(),
            request(
                4,
                "tools/call",
                json!({"name": "context_list", "arguments": [1]}),
            )
            .to_string(),
            json!({"jsonrpc": "2.0", "id": "text-id", "method": "ping"}).to_string(),
        ]
        .join("\n");
        let mut output = Vec::new();
        Server::new(&path, "")
            .unwrap()
            .serve(Cursor::new(input), &mut output)
            .unwrap();
        let reply: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let codes: Vec<i64> = reply
            .iter()
            .filter_map(|r| r["error"]["code"].as_i64())
            .collect();
        assert_eq!(
            codes,
            [
                PARSE_ERROR,
                INVALID_REQUEST,
                METHOD_NOT_FOUND,
                INVALID_PARAMS,
                INVALID_PARAMS,
                INVALID_PARAMS
            ]
        );
        assert_eq!(reply[0]["id"], Value::Null);
        assert_eq!(reply[3]["error"]["message"], "unknown tool 'context_set'");
        // A string id comes back as it was sent, and the server was still answering.
        assert_eq!(reply[6]["id"], "text-id");
        assert_eq!(reply[6]["result"], json!({}));
    }

    #[test]
    fn a_line_that_is_not_utf8_is_a_parse_error_and_not_the_end() {
        let (_dir, path) = seeded("serve-utf8");
        let mut input = vec![0xff, 0xfe, b'\n'];
        input.extend_from_slice(request(1, "ping", Value::Null).to_string().as_bytes());
        input.push(b'\n');
        let mut output = Vec::new();
        Server::new(&path, "")
            .unwrap()
            .serve(Cursor::new(input), &mut output)
            .unwrap();
        let reply: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(reply[0]["error"]["code"], PARSE_ERROR);
        assert_eq!(reply[1]["result"], json!({}));
    }

    #[test]
    fn a_session_leaves_the_database_file_byte_for_byte_as_it_was() {
        let (_dir, path) = seeded("serve-readonly");
        let before = std::fs::read(&path).unwrap();
        let mut lines = vec![request(1, "initialize", json!({}))];
        for (id, tool) in ["context_list", "glossary_list", "profile_list"]
            .iter()
            .enumerate()
        {
            lines.push(request(id as u64 + 2, "tools/call", json!({"name": tool})));
        }
        // Even a call that names a write tool changes nothing: it is not a tool.
        lines.push(request(
            9,
            "tools/call",
            json!({"name": "context_set", "arguments": {"name": "x", "kind": "rule", "text": "y"}}),
        ));
        session(&path, PROJECT, &lines);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let store = Store::open_read_only(&path).unwrap();
        assert!(store.context_entry("", "x").unwrap().is_none());
    }

    #[test]
    fn the_same_questions_give_the_same_bytes() {
        let (_dir, path) = seeded("serve-same");
        let lines = [
            request(1, "initialize", json!({"protocolVersion": "2025-03-26"})),
            request(2, "tools/list", Value::Null),
            request(3, "tools/call", json!({"name": "context_list"})),
            request(
                4,
                "tools/call",
                json!({"name": "library_get", "arguments": {"type": "prompt", "name": "house"}}),
            ),
            request(
                5,
                "tools/call",
                json!({"name": "profile_get", "arguments": {"name": "work"}}),
            ),
        ];
        assert_eq!(
            session(&path, PROJECT, &lines),
            session(&path, PROJECT, &lines)
        );
    }

    #[test]
    fn a_missing_database_stops_the_server_before_it_reads_a_line() {
        let dir = Scratch::new("serve-missing");
        let path = dir.path().join("none.db");
        let why = Server::new(&path, "").err().unwrap();
        assert!(
            why.contains("none.db") && why.contains("cannot read"),
            "{why}"
        );
        assert!(!path.exists(), "starting must not create the database");
    }
}

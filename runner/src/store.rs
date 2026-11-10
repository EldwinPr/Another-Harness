//! The SQLite store: saved chats (decision 08, issue 9).
//!
//! The table layout is owned by `docs/erd.md` and `context/coder-only/database-conventions.md`.
//! In short: a chat is a `sessions` row, its messages are `messages` rows in order, each model
//! call is a `calls` row, and the prompt a chat started with is a `system_prompts` row.
//!
//! Everything that feeds a request is stored verbatim (each message as the JSON object
//! `assemble` writes), so a chat loaded back produces the same request bytes.

use std::fmt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kernel::{Message, RouterOptions, message_from_json, message_to_json};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};

use crate::cache_check::parse_usage;
use crate::config::ApiStyle;
use crate::provider::{ProviderError, Reply};
use crate::providers::ProviderDef;
use crate::store_prompts::{ACTIVE_MARK, SUMMARY_MARK, earlier_line};

/// The schema version this program writes. It is kept in SQLite's `user_version`.
pub const SCHEMA_VERSION: i64 = 25;

/// How long a write waits for another writer before giving up.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

// Rust note: a `const` string can span lines. This is run once, on a new database.
const SCHEMA_V1: &str = "
CREATE TABLE system_prompts (
    id         INTEGER PRIMARY KEY,
    sha256     TEXT NOT NULL UNIQUE,
    text       TEXT NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE sessions (
    id               INTEGER PRIMARY KEY,
    kind             TEXT NOT NULL DEFAULT 'worker' CHECK (kind IN ('worker', 'orchestrator')),
    title            TEXT,
    model            TEXT NOT NULL,
    base_url         TEXT NOT NULL,
    router_options   TEXT NOT NULL,
    system_prompt_id INTEGER NOT NULL REFERENCES system_prompts (id),
    cwd              TEXT NOT NULL,
    harness_version  TEXT NOT NULL,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL
) STRICT;
CREATE INDEX sessions_kind_updated ON sessions (kind, updated_at);

CREATE TABLE messages (
    id         INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES sessions (id),
    seq        INTEGER NOT NULL,
    turn       INTEGER NOT NULL,
    role       TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'tool')),
    json       TEXT NOT NULL,
    text       TEXT,
    status     TEXT NOT NULL DEFAULT 'ok' CHECK (status IN ('ok', 'failed')),
    created_at INTEGER NOT NULL,
    UNIQUE (session_id, seq)
) STRICT;

CREATE TABLE calls (
    id                  INTEGER PRIMARY KEY,
    session_id          INTEGER NOT NULL REFERENCES sessions (id),
    turn                INTEGER NOT NULL,
    message_count       INTEGER NOT NULL,
    request_sha256      TEXT NOT NULL,
    request_bytes       INTEGER NOT NULL,
    http_status         INTEGER,
    response            TEXT,
    response_model      TEXT,
    finish_reason       TEXT,
    prompt_tokens       INTEGER,
    completion_tokens   INTEGER,
    cached_tokens       INTEGER,
    cache_write_tokens  INTEGER,
    elapsed_ms          INTEGER NOT NULL,
    served_by_provider  TEXT,
    served_by_account   TEXT,
    error               TEXT,
    created_at          INTEGER NOT NULL
) STRICT;
CREATE INDEX calls_session ON calls (session_id, id);
";

// Issue 10: which model and router each call was sent to (a chat can change them), and the
// providers added from inside the chat. Old `calls` rows keep NULL in the new columns.
const SCHEMA_V2: &str = "
ALTER TABLE calls ADD COLUMN model TEXT;
ALTER TABLE calls ADD COLUMN base_url TEXT;

CREATE TABLE providers (
    id              INTEGER PRIMARY KEY,
    name            TEXT NOT NULL UNIQUE,
    base_url        TEXT NOT NULL,
    model           TEXT NOT NULL,
    api_key         TEXT,
    provider_order  TEXT NOT NULL DEFAULT '[]',
    allow_fallbacks INTEGER CHECK (allow_fallbacks IN (0, 1)),
    session_id      TEXT,
    created_at      INTEGER NOT NULL
) STRICT;
";

// Issue 10: small named values the program keeps for itself, such as the provider used last.
const SCHEMA_V3: &str = "
CREATE TABLE settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;
";

// Issue 11: the models stored for each provider, shown in the menus. `origin_id` is the id as the
// router listed it (null for a model added by hand); a fetched model that is deleted is only hidden,
// so the next refresh does not bring it back.
const SCHEMA_V4: &str = "
CREATE TABLE provider_models (
    id           INTEGER PRIMARY KEY,
    provider_id  INTEGER NOT NULL REFERENCES providers (id),
    model_id     TEXT NOT NULL,
    display_name TEXT NOT NULL,
    origin_id    TEXT,
    hidden       INTEGER NOT NULL DEFAULT 0 CHECK (hidden IN (0, 1)),
    created_at   INTEGER NOT NULL,
    UNIQUE (provider_id, model_id)
) STRICT;
";

// Issue 13: the prompts the owner keeps and chooses from. The ones switched on, in `position`
// order, are joined into the system prompt of a new chat; `system_prompts` stays the frozen record of
// what each chat started with. `source_path` is where a prompt was imported from, if it was. The
// built-in text is the first row (seeded in `migrate`).
const SCHEMA_V5: &str = "
CREATE TABLE prompts (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL DEFAULT '',
    text        TEXT NOT NULL,
    source_path TEXT,
    enabled     INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
    position    INTEGER NOT NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
) STRICT;
";

// Issue 14: a prompt has a type (`system` goes into the system prompt, a `skill` is added to requests
// when it is on or asked for one turn, a `personality` is one selected at a time), and a chat
// remembers which skills are on and which personality is selected. "This turn only" is not stored.
const SCHEMA_V6: &str = "
ALTER TABLE prompts ADD COLUMN type TEXT NOT NULL DEFAULT 'system'
    CHECK (type IN ('system', 'skill', 'personality'));
CREATE TABLE chat_prompt_state (
    session_id INTEGER NOT NULL REFERENCES sessions (id),
    prompt_id  INTEGER NOT NULL REFERENCES prompts (id),
    state      TEXT NOT NULL CHECK (state IN ('on', 'selected')),
    PRIMARY KEY (session_id, prompt_id)
) STRICT;
";

// Issue 14, second round: skills and personalities are no longer "on by default"; a new chat starts
// with none. The column stays and means "switched on" for system prompts only.
/// Issue 27: the glossary. `term` is unique ignoring case.
// Issue 31: chat profiles. A profile is the starting setup of a chat: system prompts, skills that are
// on, and a personality. The personality is a column (a profile has at most one).
const SCHEMA_V12: &str = "
CREATE TABLE IF NOT EXISTS profiles (
    id             INTEGER PRIMARY KEY,
    name           TEXT NOT NULL UNIQUE COLLATE NOCASE,
    description    TEXT NOT NULL DEFAULT '',
    personality_id INTEGER REFERENCES prompts (id) ON DELETE SET NULL,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS profile_prompts (
    profile_id INTEGER NOT NULL REFERENCES profiles (id) ON DELETE CASCADE,
    prompt_id  INTEGER NOT NULL REFERENCES prompts (id) ON DELETE CASCADE,
    PRIMARY KEY (profile_id, prompt_id)
) STRICT;
CREATE TABLE IF NOT EXISTS profile_skills (
    profile_id INTEGER NOT NULL REFERENCES profiles (id) ON DELETE CASCADE,
    prompt_id  INTEGER NOT NULL REFERENCES prompts (id) ON DELETE CASCADE,
    PRIMARY KEY (profile_id, prompt_id)
) STRICT;
";

// Issue 33: a subagent's chat points at the chat that delegated to it.
const SCHEMA_V13: &str = "
CREATE INDEX IF NOT EXISTS sessions_parent ON sessions (parent_id);
";

// Issue 37: context entries, the rules, decisions and shapes the owner has said, per project.
const SCHEMA_V15: &str = "
CREATE TABLE IF NOT EXISTS context_entries (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL COLLATE NOCASE,
    kind       TEXT NOT NULL CHECK (kind IN ('rule', 'decision', 'shape', 'guide')),
    text       TEXT NOT NULL,
    quote      TEXT NOT NULL DEFAULT '',
    area       TEXT NOT NULL DEFAULT '',
    project    TEXT NOT NULL DEFAULT '',
    preload    INTEGER NOT NULL DEFAULT 0 CHECK (preload IN (0, 1)),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (project, name)
) STRICT;
";

// Issue 44: the MCP servers the owner added and the rule of each of their tools. `args` is a JSON
// array of strings and `env` a JSON object of NAME to value. A tool with no row in
// `mcp_tool_rules` has the server's `default_rule`.
const SCHEMA_V24: &str = "
CREATE TABLE IF NOT EXISTS mcp_servers (
    id           INTEGER PRIMARY KEY,
    name         TEXT NOT NULL UNIQUE,
    command      TEXT NOT NULL,
    args         TEXT NOT NULL DEFAULT '[]',
    env          TEXT NOT NULL DEFAULT '{}',
    timeout_secs INTEGER NOT NULL DEFAULT 60 CHECK (timeout_secs BETWEEN 1 AND 600),
    enabled      INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    default_rule TEXT NOT NULL DEFAULT 'ask' CHECK (default_rule IN ('ask', 'free', 'off')),
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS mcp_tool_rules (
    server_id INTEGER NOT NULL REFERENCES mcp_servers (id) ON DELETE CASCADE,
    tool      TEXT NOT NULL,
    rule      TEXT NOT NULL CHECK (rule IN ('ask', 'free', 'off')),
    PRIMARY KEY (server_id, tool)
) STRICT;
";

const SCHEMA_V11: &str = "CREATE TABLE IF NOT EXISTS glossary (
    id         INTEGER PRIMARY KEY,
    term       TEXT NOT NULL UNIQUE COLLATE NOCASE,
    area       TEXT NOT NULL DEFAULT '',
    definition TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;";

/// Issue 36: a glossary word can belong to one project (the working directory) or, with `project`
/// empty, to every project. Every word there is becomes one for every project.
const SCHEMA_V17: &str = "ALTER TABLE glossary RENAME TO glossary_old;
CREATE TABLE glossary (
    id         INTEGER PRIMARY KEY,
    term       TEXT NOT NULL COLLATE NOCASE,
    area       TEXT NOT NULL DEFAULT '',
    definition TEXT NOT NULL,
    project    TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (project, term)
) STRICT;
INSERT INTO glossary (id, term, area, definition, project, created_at, updated_at)
    SELECT id, term, area, definition, '', created_at, updated_at FROM glossary_old;
DROP TABLE glossary_old;";

/// Issue 40: which API a provider speaks. Every provider so far speaks chat completions.
const SCHEMA_V18: &str = "ALTER TABLE providers ADD COLUMN api TEXT NOT NULL DEFAULT 'openai' CHECK (api IN ('openai', 'anthropic'));";

const SCHEMA_V7: &str = "UPDATE prompts SET enabled = 0 WHERE type <> 'system';";

#[cfg(test)]
pub(crate) const SCHEMA_V1_FOR_TESTS: &str = SCHEMA_V1;
#[cfg(test)]
pub(crate) const SCHEMA_V2_FOR_TESTS: &str = SCHEMA_V2;
#[cfg(test)]
pub(crate) const SCHEMA_V3_FOR_TESTS: &str = SCHEMA_V3;
#[cfg(test)]
pub(crate) const SCHEMA_V4_FOR_TESTS: &str = SCHEMA_V4;
#[cfg(test)]
pub(crate) const SCHEMA_V5_FOR_TESTS: &str = SCHEMA_V5;
#[cfg(test)]
pub(crate) const SCHEMA_V6_FOR_TESTS: &str = SCHEMA_V6;

/// The `settings` key holding the name of the provider the chat last switched to.
pub const LAST_PROVIDER: &str = "last_provider";

/// The `settings` key holding the approval mode (`manual` or `auto`, issue 16).
pub const APPROVE_MODE: &str = "approve_mode";

/// The `settings` key holding the prompt-token limit for automatic compaction (`off` or a number,
/// issue 24).
pub const AUTO_COMPACT: &str = "auto_compact";

/// Anything that went wrong talking to the database, as one line of text.
#[derive(Debug)]
pub struct StoreError(String);

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StoreError {}

// Rust note: implementing `From` is what lets `?` turn a `rusqlite::Error` into a
// `StoreError` automatically.
impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self(e.to_string())
    }
}

fn err(text: impl Into<String>) -> StoreError {
    StoreError(text.into())
}

impl StoreError {
    /// An error with a message of its own (used by the parts of the store in other files).
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }
}

pub(crate) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Lowercase hex SHA-256 of some bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// What a checked edit or delete did (issue 29): it is made only if the row is still as the
/// person saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    Done,
    /// The row changed since it was shown; nothing was written.
    Stale,
    /// The row is gone; nothing was written.
    Gone,
}

/// A short stamp of some texts: two readings of the same row have the same stamp, and a row that
/// was edited in between does not. The parts are separated so `["ab", "c"]` and `["a", "bc"]` differ.
pub fn fingerprint(parts: &[&str]) -> String {
    let mut joined = Vec::new();
    for part in parts {
        joined.extend_from_slice(&(part.len() as u64).to_le_bytes());
        joined.extend_from_slice(part.as_bytes());
    }
    sha256_hex(&joined)[..16].to_owned()
}

/// The router settings as the text stored in `sessions.router_options`. Never holds the API
/// key (it is not part of [`RouterOptions`]). Used both to store and to compare, so the same
/// settings always give the same text.
pub fn router_json(router: &RouterOptions) -> String {
    serde_json::json!({
        "provider_order": router.provider_order,
        "allow_fallbacks": router.allow_fallbacks,
        "session_id": router.session_id,
    })
    .to_string()
}

/// What kind of chat a session is. Only workers exist so far; the orchestrator name is
/// reserved (decision 07).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Worker,
    Orchestrator,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Worker => "worker",
            Kind::Orchestrator => "orchestrator",
        }
    }
}

/// What a new chat records about itself.
pub struct NewSession<'a> {
    pub kind: Kind,
    pub model: &'a str,
    pub base_url: &'a str,
    pub router: &'a RouterOptions,
    pub system_prompt: &'a str,
    pub cwd: &'a str,
    /// The profile the chat began under (issue 31), if there was one.
    pub profile: Option<&'a str>,
    /// The chat that delegated this one to a subagent (issue 33); `None` for a chat of its own.
    pub parent_id: Option<i64>,
}

/// One saved model call, with the messages that became permanent because of it.
pub struct SaveCall<'a> {
    pub session_id: i64,
    /// Which user message this belongs to, counting from 1.
    pub turn: u32,
    /// Messages the request carried that were not stored yet (the user's message, or tool
    /// results). Never the system message.
    pub pending: &'a [Message],
    /// The model's answer, when the call produced one.
    pub reply: Option<&'a Message>,
    /// Mark every message of this turn `failed`, so a resume skips it.
    pub turn_failed: bool,
    /// How many messages the request carried, not counting the system message.
    pub message_count: usize,
    /// The request bytes. Only their hash and length are stored.
    pub request: &'a [u8],
    /// The model and router the request was sent to (a chat can change them between messages).
    pub model: &'a str,
    pub base_url: &'a str,
    pub router: &'a RouterOptions,
    pub result: &'a Result<Reply, Box<ProviderError>>,
}

/// A saved chat, without its messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: i64,
    pub model: String,
    pub base_url: String,
    pub router_options: String,
    /// The exact system prompt text the chat started with.
    pub system_prompt: String,
    pub cwd: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// The profile the chat began under (issue 31), if it was saved with one.
    pub profile: Option<String>,
}

impl SessionInfo {
    /// Plain-language differences between how this chat was saved and how the program is set
    /// up now. Empty when they match. Resuming on different settings still works, but the
    /// router's cache starts over and answers may change.
    pub fn differences(&self, model: &str, base_url: &str, router_options: &str) -> Vec<String> {
        let mut found = Vec::new();
        if self.model != model {
            found.push(format!("model was {}, now {model}", self.model));
        }
        if self.base_url != base_url {
            found.push(format!("router was {}, now {base_url}", self.base_url));
        }
        if self.router_options != router_options {
            found.push("provider settings differ from the saved chat".to_owned());
        }
        found
    }
}

/// A saved chat read back, ready to continue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resumed {
    pub info: SessionInfo,
    /// The messages after the system prompt, in order, without failed turns.
    pub messages: Vec<Message>,
    /// The highest turn number in the chat, failed turns included, so new turns never reuse one.
    pub last_turn: u32,
    /// The last turn never finished (the program stopped in the middle of it), so it was
    /// marked failed and left out.
    pub dropped_unfinished_turn: bool,
}

/// One line of the chat list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: i64,
    pub updated_at: i64,
    pub model: String,
    /// Usable messages (failed ones are not counted).
    pub message_count: i64,
    /// The first usable message the user typed, to recognise the chat by. The "Active
    /// instructions" and summary blocks are not it.
    pub first_message: Option<String>,
    /// The chat began from a compaction summary.
    pub continued: bool,
    /// The profile the chat began under (issue 31), if it was saved with one.
    pub profile: Option<String>,
}

pub struct Store {
    pub(crate) conn: Connection,
    /// The project this connection works for (the working directory); what the glossary shows and
    /// writes by default. Empty means no project: only the words for every project.
    pub(crate) project: String,
}

impl Store {
    /// Works for this project: the glossary shows its words and the shared ones.
    pub fn with_project(mut self, project: impl Into<String>) -> Self {
        self.project = project.into();
        self
    }

    /// The database file this store has open, if it is a file.
    pub fn path(&self) -> Option<std::path::PathBuf> {
        self.conn
            .path()
            .filter(|p| !p.is_empty())
            .map(std::path::PathBuf::from)
    }

    /// Opens an existing database for reading only: nothing is created, migrated or written.
    /// A database of another version is refused (a program of the same version wrote it).
    pub fn open_read_only(path: &Path) -> Result<Self, StoreError> {
        Self::open_existing(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            "reading",
        )
    }

    /// Opens an existing database for reading and writing (the web page's edits, issue 29):
    /// nothing is created or migrated, and a database of another version is refused.
    pub fn open_for_writing(path: &Path) -> Result<Self, StoreError> {
        let store = Self::open_existing(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            "writing",
        )?;
        store.conn.pragma_update(None, "foreign_keys", true)?;
        Ok(store)
    }

    fn open_existing(
        path: &Path,
        flags: rusqlite::OpenFlags,
        what: &str,
    ) -> Result<Self, StoreError> {
        let conn = Connection::open_with_flags(path, flags)
            .map_err(|e| err(format!("cannot open {} for {what}: {e}", path.display())))?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != SCHEMA_VERSION {
            return Err(err(format!(
                "this database is version {version}, this program reads version {SCHEMA_VERSION}: start the chat once to upgrade it"
            )));
        }
        Ok(Self {
            conn,
            project: String::new(),
        })
    }

    /// Opens the database at `path`, creating the file, its folder and the tables if needed.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .map_err(|e| err(format!("cannot create {}: {e}", dir.display())))?;
        }
        // The database holds provider API keys and every chat: a new file is readable by its
        // owner only (SQLite gives the -wal and -shm files the same mode).
        if !path.exists() {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(path)
                .map_err(|e| err(format!("cannot create {}: {e}", path.display())))?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        // WAL lets another program read the chats while this one writes.
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        back_up_before_upgrade(&conn, path)?;
        migrate(&conn)?;
        Ok(Self {
            conn,
            project: String::new(),
        })
    }

    /// Starts a chat and returns its id.
    pub fn create_session(&mut self, new: &NewSession<'_>) -> Result<i64, StoreError> {
        let now = now_secs();
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // The same prompt text always lands in the same row.
        tx.execute(
            "INSERT OR IGNORE INTO system_prompts (sha256, text, created_at) VALUES (?1, ?2, ?3)",
            params![
                sha256_hex(new.system_prompt.as_bytes()),
                new.system_prompt,
                now
            ],
        )?;
        let prompt_id: i64 = tx.query_row(
            "SELECT id FROM system_prompts WHERE sha256 = ?1",
            [sha256_hex(new.system_prompt.as_bytes())],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO sessions (kind, model, base_url, router_options, system_prompt_id, cwd, \
             harness_version, created_at, updated_at, profile, parent_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9, ?10)",
            params![
                new.kind.as_str(),
                new.model,
                new.base_url,
                router_json(new.router),
                prompt_id,
                new.cwd,
                env!("CARGO_PKG_VERSION"),
                now,
                new.profile,
                new.parent_id
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    /// Saves one model call, and the messages that became permanent with it, in one
    /// transaction: either all of it is stored or none of it.
    pub fn save_call(&mut self, call: &SaveCall<'_>) -> Result<(), StoreError> {
        let now = now_secs();
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        insert_messages(
            &tx,
            call.session_id,
            call.turn,
            call.pending.iter().chain(call.reply),
            now,
        )?;
        insert_call(&tx, call, now)?;
        if call.turn_failed {
            mark_failed(&tx, call.session_id, call.turn)?;
        }
        // The chat row always shows what it last used, so a resume compares against that.
        tx.execute(
            "UPDATE sessions SET model = ?2, base_url = ?3, router_options = ?4, updated_at = ?5 \
             WHERE id = ?1",
            params![
                call.session_id,
                call.model,
                call.base_url,
                router_json(call.router),
                now
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Saves a provider added from inside the chat. The name must be unused. Its default model
    /// becomes its first stored model.
    pub fn add_provider(&mut self, provider: &ProviderDef) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let taken: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM providers WHERE name = ?1)",
            [&provider.name],
            |row| row.get(0),
        )?;
        if taken {
            return Err(err(format!(
                "there is already a saved provider named {}",
                provider.name
            )));
        }
        let now = now_secs();
        tx.execute(
            "INSERT INTO providers (name, base_url, model, api_key, provider_order, \
             allow_fallbacks, session_id, api, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                provider.name,
                provider.base_url,
                provider.model,
                provider.api_key,
                serde_json::to_string(&provider.router.provider_order)
                    .map_err(|e| err(e.to_string()))?,
                provider.router.allow_fallbacks,
                provider.router.session_id,
                provider.api.as_str(),
                now
            ],
        )?;
        let provider_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO provider_models (provider_id, model_id, display_name, origin_id, created_at) \
             VALUES (?1, ?2, ?2, NULL, ?3)",
            params![provider_id, provider.model, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The saved providers, by name.
    pub fn list_providers(&self) -> Result<Vec<ProviderDef>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT name, base_url, model, api_key, provider_order, allow_fallbacks, session_id, api \
             FROM providers ORDER BY name",
        )?;
        let rows = statement.query_map([], |row| {
            let order: String = row.get(4)?;
            Ok(ProviderDef {
                name: row.get(0)?,
                base_url: row.get(1)?,
                model: row.get(2)?,
                api_key: row.get(3)?,
                router: RouterOptions {
                    provider_order: serde_json::from_str(&order).unwrap_or_default(),
                    allow_fallbacks: row.get(5)?,
                    session_id: row.get(6)?,
                },
                api: {
                    let text: String = row.get(7)?;
                    ApiStyle::parse(&text).unwrap_or_default()
                },
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// A value the program keeps for itself (see [`LAST_PROVIDER`]).
    pub fn setting(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    /// Sets a value the program keeps for itself.
    pub fn set_setting(&mut self, key: &str, value: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![key, value, now_secs()],
        )?;
        Ok(())
    }

    /// Deletes a saved provider and its stored models. `false` when there was none by that name.
    pub fn remove_provider(&mut self, name: &str) -> Result<bool, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM provider_models WHERE provider_id IN (SELECT id FROM providers WHERE name = ?1)",
            [name],
        )?;
        let removed = tx.execute("DELETE FROM providers WHERE name = ?1", [name])?;
        tx.commit()?;
        Ok(removed > 0)
    }

    /// Saves messages that no model call carried yet, such as tool results after the call cap.
    pub fn save_messages(
        &mut self,
        session_id: i64,
        turn: u32,
        messages: &[Message],
    ) -> Result<(), StoreError> {
        let now = now_secs();
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        insert_messages(&tx, session_id, turn, messages.iter(), now)?;
        touch(&tx, session_id, now)?;
        tx.commit()?;
        Ok(())
    }

    /// Marks every message of a turn `failed`. Best effort by design: the caller is already
    /// handling a failure and may be unable to reach the database.
    pub fn mark_turn_failed(&mut self, session_id: i64, turn: u32) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        mark_failed(&tx, session_id, turn)?;
        tx.commit()?;
        Ok(())
    }

    /// Deletes a chat with all its messages and calls, in one transaction. Other chats and the
    /// shared system prompts are not touched. An unknown id is an error.
    pub fn delete_session(&mut self, session_id: i64) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // The subagent chats under it go too (issue 33); they are never on their own.
        for id in ["SELECT id FROM sessions WHERE parent_id = ?1", "SELECT ?1"] {
            let ids: Vec<i64> = {
                let mut statement = tx.prepare(id)?;
                let rows = statement.query_map([session_id], |row| row.get(0))?;
                rows.collect::<Result<_, _>>()?
            };
            for id in ids {
                tx.execute("DELETE FROM chat_prompt_state WHERE session_id = ?1", [id])?;
                tx.execute("DELETE FROM calls WHERE session_id = ?1", [id])?;
                tx.execute("DELETE FROM messages WHERE session_id = ?1", [id])?;
                if id != session_id {
                    tx.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
                }
            }
        }
        let removed = tx.execute("DELETE FROM sessions WHERE id = ?1", [session_id])?;
        if removed == 0 {
            return Err(err(format!("no saved chat with id {session_id}")));
        }
        tx.commit()?;
        Ok(())
    }

    /// The most recently used chat of this kind that has at least one usable message.
    pub fn latest_session(&self, kind: Kind) -> Result<Option<SessionInfo>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT s.id, s.model, s.base_url, s.router_options, p.text, s.cwd, \
                        s.created_at, s.updated_at, s.profile \
                 FROM sessions s JOIN system_prompts p ON p.id = s.system_prompt_id \
                 WHERE s.kind = ?1 AND s.parent_id IS NULL \
                   AND EXISTS (SELECT 1 FROM messages m WHERE m.session_id = s.id AND m.status = 'ok') \
                 ORDER BY s.updated_at DESC, s.id DESC LIMIT 1",
                [kind.as_str()],
                read_info,
            )
            .optional()?)
    }

    /// The most recently used chats of this kind that have a usable message, newest first.
    pub fn list_sessions(
        &self,
        kind: Kind,
        limit: usize,
    ) -> Result<Vec<SessionSummary>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT s.id, s.updated_at, s.model, \
                    (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id AND m.status = 'ok'), \
                    (SELECT m.text FROM messages m \
                      WHERE m.session_id = s.id AND m.status = 'ok' AND m.role = 'user' \
                        AND substr(m.text, 1, length(?3)) <> ?3 \
                        AND substr(m.text, 1, length(?4)) <> ?4 \
                      ORDER BY m.seq LIMIT 1), \
                    EXISTS (SELECT 1 FROM messages m \
                      WHERE m.session_id = s.id AND m.status = 'ok' AND m.role = 'user' \
                        AND substr(m.text, 1, length(?4)) = ?4), \
                    s.profile \
             FROM sessions s \
             WHERE s.kind = ?1 AND s.parent_id IS NULL \
               AND EXISTS (SELECT 1 FROM messages m WHERE m.session_id = s.id AND m.status = 'ok') \
             ORDER BY s.updated_at DESC, s.id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(
            params![
                kind.as_str(),
                i64::try_from(limit).unwrap_or(i64::MAX),
                ACTIVE_MARK,
                SUMMARY_MARK
            ],
            |row| {
                Ok(SessionSummary {
                    id: row.get(0)?,
                    updated_at: row.get(1)?,
                    model: row.get(2)?,
                    message_count: row.get(3)?,
                    first_message: row.get(4)?,
                    continued: row.get(5)?,
                    profile: row.get(6)?,
                })
            },
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The saved chats that began from a summary made from chat `id`, oldest first.
    pub fn chats_continuing(&self, id: i64) -> Result<Vec<i64>, StoreError> {
        let prefix = format!("{SUMMARY_MARK}\n\n{}", earlier_line(id));
        let mut statement = self.conn.prepare(
            "SELECT DISTINCT session_id FROM messages \
             WHERE role = 'user' AND status = 'ok' AND substr(text, 1, length(?1)) = ?1 \
             ORDER BY session_id",
        )?;
        let rows = statement.query_map([prefix], |row| row.get::<_, i64>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Reads a chat back. An unfinished last turn is marked failed and left out, because the
    /// conversation it would leave (a question with no answer, or a tool call with no result)
    /// is one a provider can reject.
    pub fn load(&mut self, session_id: i64) -> Result<Resumed, StoreError> {
        let info = self
            .conn
            .query_row(
                "SELECT s.id, s.model, s.base_url, s.router_options, p.text, s.cwd, \
                        s.created_at, s.updated_at, s.profile \
                 FROM sessions s JOIN system_prompts p ON p.id = s.system_prompt_id \
                 WHERE s.id = ?1",
                [session_id],
                read_info,
            )
            .optional()?
            .ok_or_else(|| err(format!("no saved chat with id {session_id}")))?;

        let mut last_turn: i64 = 0;
        let mut kept: Vec<(i64, Message)> = Vec::new();
        {
            let mut statement = self.conn.prepare(
                "SELECT turn, status, json FROM messages WHERE session_id = ?1 ORDER BY seq",
            )?;
            let rows = statement.query_map([session_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (turn, status, json) = row?;
                last_turn = last_turn.max(turn);
                if status == "ok" {
                    let message = message_from_json(&json).map_err(|e| {
                        err(format!("a saved message (turn {turn}) is unreadable: {e}"))
                    })?;
                    kept.push((turn, message));
                }
            }
        }

        let unfinished = kept.last().and_then(|(turn, last)| {
            let open = match last {
                Message::User(_) => true,
                Message::Assistant { tool_calls, .. } => !tool_calls.is_empty(),
                _ => false,
            };
            open.then_some(*turn)
        });
        if let Some(turn) = unfinished {
            let tx = self
                .conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            mark_failed(&tx, session_id, u32::try_from(turn).unwrap_or(u32::MAX))?;
            tx.commit()?;
            kept.retain(|(t, _)| *t != turn);
        }

        Ok(Resumed {
            info,
            messages: kept.into_iter().map(|(_, message)| message).collect(),
            last_turn: u32::try_from(last_turn).unwrap_or(u32::MAX),
            dropped_unfinished_turn: unfinished.is_some(),
        })
    }

    /// What the answered calls of a chat reported, for the info line (issue 17).
    pub fn chat_usage(&self, session_id: i64) -> Result<crate::usage::ChatUsage, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT prompt_tokens, completion_tokens, cached_tokens FROM calls \
             WHERE session_id = ?1 AND error IS NULL AND http_status BETWEEN 200 AND 299 \
             ORDER BY id",
        )?;
        let rows = statement.query_map([session_id], |row| {
            Ok(crate::usage::Tokens {
                prompt: row
                    .get::<_, Option<i64>>(0)?
                    .and_then(|n| u64::try_from(n).ok()),
                completion: row
                    .get::<_, Option<i64>>(1)?
                    .and_then(|n| u64::try_from(n).ok()),
                cached: row
                    .get::<_, Option<i64>>(2)?
                    .and_then(|n| u64::try_from(n).ok()),
            })
        })?;
        let mut usage = crate::usage::ChatUsage::default();
        for tokens in rows {
            usage.add(tokens?);
        }
        // The subagents' calls count in the chat that delegated them (issue 33).
        let (prompt, completion): (i64, i64) = self.conn.query_row(
            "SELECT COALESCE(SUM(c.prompt_tokens), 0), COALESCE(SUM(c.completion_tokens), 0) \
             FROM calls c JOIN sessions s ON s.id = c.session_id \
             WHERE s.parent_id = ?1 AND c.error IS NULL AND c.http_status BETWEEN 200 AND 299",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        usage.add_subagent(
            u64::try_from(prompt).unwrap_or(0),
            u64::try_from(completion).unwrap_or(0),
        );
        Ok(usage)
    }

    /// The open connection, for tests that look at the tables directly.
    #[cfg(test)]
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }
}

fn read_info(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionInfo> {
    Ok(SessionInfo {
        id: row.get(0)?,
        model: row.get(1)?,
        base_url: row.get(2)?,
        router_options: row.get(3)?,
        system_prompt: row.get(4)?,
        cwd: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        profile: row.get(8)?,
    })
}

/// An existing database that is about to be upgraded is copied first, to `<file>.before-<version>`
/// (readable by its owner only, like the database). An earlier copy is never replaced: when the
/// name is taken the copy is `.before-<version>.2`, `.3` ...
fn back_up_before_upgrade(conn: &Connection, path: &Path) -> Result<(), StoreError> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if !(1..SCHEMA_VERSION).contains(&version) {
        return Ok(());
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return Ok(());
    };
    let mut copy = path.with_file_name(format!("{name}.before-{SCHEMA_VERSION}"));
    let mut n = 1;
    while copy.exists() {
        n += 1;
        copy = path.with_file_name(format!("{name}.before-{SCHEMA_VERSION}.{n}"));
    }
    let target = copy.to_string_lossy().into_owned();
    conn.execute("VACUUM INTO ?1", [&target])
        .map_err(|e| err(format!("cannot back up the database to {target}: {e}")))?;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o600));
    Ok(())
}

/// Creates the tables on a new database. A database written by a newer program is refused.
fn migrate(conn: &Connection) -> Result<(), StoreError> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(err(format!(
            "this database is version {version}, newer than this program understands ({SCHEMA_VERSION})"
        )));
    }
    if version < 1 {
        conn.execute_batch(&format!(
            "BEGIN; {SCHEMA_V1} PRAGMA user_version = 1; COMMIT;"
        ))?;
    }
    if version < 2 {
        conn.execute_batch(&format!(
            "BEGIN; {SCHEMA_V2} PRAGMA user_version = 2; COMMIT;"
        ))?;
    }
    if version < 3 {
        conn.execute_batch(&format!(
            "BEGIN; {SCHEMA_V3} PRAGMA user_version = 3; COMMIT;"
        ))?;
    }
    if version < 4 {
        conn.execute_batch(&format!(
            "BEGIN; {SCHEMA_V4} PRAGMA user_version = 4; COMMIT;"
        ))?;
    }
    if version < 5 {
        // The first prompt holds the text that used to be hard-coded, switched on, so a database
        // that is upgraded or new starts out with the same system prompt as before.
        let seed = crate::store_prompts::BUILTIN_PROMPT.replace('\'', "''");
        conn.execute_batch(&format!(
            "BEGIN; {SCHEMA_V5} \
             INSERT INTO prompts (name, description, text, source_path, enabled, position, created_at, updated_at) \
             VALUES ('{base}', '{description}', '{seed}', NULL, 1, 1, \
                     CAST(strftime('%s', 'now') AS INTEGER), CAST(strftime('%s', 'now') AS INTEGER)); \
             PRAGMA user_version = 5; COMMIT;",
            base = crate::store_prompts::BASE_NAME,
            description = crate::store_prompts::BASE_DESCRIPTION,
        ))?;
    }
    if version < 6 {
        conn.execute_batch(&format!(
            "BEGIN; {SCHEMA_V6} PRAGMA user_version = 6; COMMIT;"
        ))?;
    }
    if version < 7 {
        conn.execute_batch(&format!(
            "BEGIN; {SCHEMA_V7} PRAGMA user_version = 7; COMMIT;"
        ))?;
    }
    if version < 8 {
        // Issue 16: the built-in prompt now lists the tools. Only a `base` that still holds the
        // previous built-in text is rewritten; one the owner edited is theirs.
        conn.execute_batch(&format!(
            "BEGIN; \
             UPDATE prompts SET text = '{new}', updated_at = CAST(strftime('%s', 'now') AS INTEGER) \
             WHERE name = '{base}' AND text = '{old}'; \
             PRAGMA user_version = 8; COMMIT;",
            new = crate::store_prompts::BUILTIN_PROMPT.replace('\'', "''"),
            old = crate::store_prompts::PREVIOUS_BUILTIN_PROMPT.replace('\'', "''"),
            base = crate::store_prompts::BASE_NAME,
        ))?;
    }
    if version < 9 {
        // Issue 20: the built-in prompt now lists `edit_file`. Only a `base` that still holds the
        // schema 8 text is rewritten.
        conn.execute_batch(&format!(
            "BEGIN; \
             UPDATE prompts SET text = '{new}', updated_at = CAST(strftime('%s', 'now') AS INTEGER) \
             WHERE name = '{base}' AND text = '{old}'; \
             PRAGMA user_version = 9; COMMIT;",
            new = crate::store_prompts::BUILTIN_PROMPT.replace('\'', "''"),
            old = crate::store_prompts::BUILTIN_PROMPT_V8.replace('\'', "''"),
            base = crate::store_prompts::BASE_NAME,
        ))?;
    }
    if version < 10 {
        // The built-in prompt gained rules drawn from the logs (stay inside the working directory,
        // no heredoc patches, batch requests, stop searching). Only an unedited `base` changes.
        conn.execute_batch(&format!(
            "BEGIN; \
             UPDATE prompts SET text = '{new}', updated_at = CAST(strftime('%s', 'now') AS INTEGER) \
             WHERE name = '{base}' AND text = '{old}'; \
             PRAGMA user_version = 10; COMMIT;",
            new = crate::store_prompts::BUILTIN_PROMPT.replace('\'', "''"),
            old = crate::store_prompts::BUILTIN_PROMPT_V9.replace('\'', "''"),
            base = crate::store_prompts::BASE_NAME,
        ))?;
    }
    if version < 11 {
        // Issue 27: the glossary table, and the built-in prompt names its two tools. Only an
        // unedited `base` changes.
        conn.execute_batch(&format!(
            "BEGIN; {SCHEMA_V11} \
             UPDATE prompts SET text = '{new}', updated_at = CAST(strftime('%s', 'now') AS INTEGER) \
             WHERE name = '{base}' AND text = '{old}'; \
             PRAGMA user_version = 11; COMMIT;",
            new = crate::store_prompts::BUILTIN_PROMPT.replace('\'', "''"),
            old = crate::store_prompts::BUILTIN_PROMPT_V10.replace('\'', "''"),
            base = crate::store_prompts::BASE_NAME,
        ))?;
    }
    if version < 12 {
        // Issue 31: chat profiles. `default` starts as the system prompts that are on now, so the
        // first chat after the upgrade has the system prompt it would have had. The column
        // `prompts.enabled` stays but is no longer read.
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(SCHEMA_V12)?;
        let has_column: i64 = tx.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name = 'profile'",
            [],
            |row| row.get(0),
        )?;
        if has_column == 0 {
            tx.execute_batch("ALTER TABLE sessions ADD COLUMN profile TEXT;")?;
        }
        crate::store_profiles::seed(&tx, !cfg!(test))?;
        tx.execute_batch("PRAGMA user_version = 12;")?;
        tx.commit()?;
    }
    if version < 13 {
        // Issue 33: subagents. A subagent's chat has a parent; the `subagent` prompt is added if
        // there is none by that name (one the owner already has is kept as written).
        let tx = conn.unchecked_transaction()?;
        let has_column: i64 = tx.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name = 'parent_id'",
            [],
            |row| row.get(0),
        )?;
        if has_column == 0 {
            tx.execute_batch(
                "ALTER TABLE sessions ADD COLUMN parent_id INTEGER REFERENCES sessions (id);",
            )?;
        }
        tx.execute_batch(SCHEMA_V13)?;
        crate::store_prompts::seed_subagent(&tx, !cfg!(test))?;
        tx.execute_batch("PRAGMA user_version = 13;")?;
        tx.commit()?;
    }
    if version < 14 {
        // Issue 34: the `subagent-coder` prompt, added if there is none by that name (and the
        // `subagent` prompt, if a database never had it).
        let tx = conn.unchecked_transaction()?;
        crate::store_prompts::seed_subagent(&tx, !cfg!(test))?;
        tx.execute_batch("PRAGMA user_version = 14;")?;
        tx.commit()?;
    }
    if version < 15 {
        // Issue 37: the table of context entries.
        conn.execute_batch(&format!(
            "BEGIN; {SCHEMA_V15} PRAGMA user_version = 15; COMMIT;"
        ))?;
    }
    if version < 16 {
        // Issue 38: the prompts name the context tools. Only an unedited `base` and unedited
        // subagent prompts change.
        use crate::store_prompts as p;
        let rewrite = |name: &str, old: &str, new: &str| {
            format!(
                "UPDATE prompts SET text = '{}', updated_at = CAST(strftime('%s', 'now') AS INTEGER) \
                 WHERE name = '{name}' AND text = '{}';",
                new.replace('\'', "''"),
                old.replace('\'', "''")
            )
        };
        conn.execute_batch(&format!(
            "BEGIN; {} {} {} PRAGMA user_version = 16; COMMIT;",
            rewrite(p::BASE_NAME, p::BUILTIN_PROMPT_V15, p::BUILTIN_PROMPT_V16),
            rewrite(p::SUBAGENT_NAME, p::SUBAGENT_PROMPT_V15, p::SUBAGENT_PROMPT),
            rewrite(
                p::SUBAGENT_CODER_NAME,
                p::SUBAGENT_CODER_PROMPT_V15,
                p::SUBAGENT_CODER_PROMPT
            ),
        ))?;
    }
    if version < 17 {
        conn.execute_batch(&format!(
            "BEGIN; {SCHEMA_V17} PRAGMA user_version = 17; COMMIT;"
        ))?;
    }
    if version < 18 {
        // A file whose version was put back by hand already has the column.
        let has_column: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM pragma_table_info('providers') WHERE name = 'api')",
            [],
            |row| row.get(0),
        )?;
        let alter = if has_column { "" } else { SCHEMA_V18 };
        conn.execute_batch(&format!("BEGIN; {alter} PRAGMA user_version = 18; COMMIT;"))?;
    }
    if version < 19 {
        // Issue 41: the base prompt names the `library` and `profile` tools. Only an unedited
        // `base` changes.
        use crate::store_prompts as p;
        conn.execute_batch(&format!(
            "BEGIN; UPDATE prompts SET text = '{}', updated_at = CAST(strftime('%s', 'now') AS INTEGER) \
             WHERE name = '{}' AND text = '{}'; PRAGMA user_version = 19; COMMIT;",
            p::BUILTIN_PROMPT_V17.replace('\'', "''"),
            p::BASE_NAME,
            p::BUILTIN_PROMPT_V16.replace('\'', "''"),
        ))?;
    }
    if version < 20 {
        // Issue 42: the base prompt names `task` and `steps`. Only an unedited `base` changes.
        use crate::store_prompts as p;
        conn.execute_batch(&format!(
            "BEGIN; UPDATE prompts SET text = '{}', updated_at = CAST(strftime('%s', 'now') AS INTEGER) \
             WHERE name = '{}' AND text = '{}'; PRAGMA user_version = 20; COMMIT;",
            p::BUILTIN_PROMPT_V20.replace('\'', "''"),
            p::BASE_NAME,
            p::BUILTIN_PROMPT_V17.replace('\'', "''"),
        ))?;
    }
    if version < 21 {
        // The base prompt says where skills live (the `library` tool, not .claude files). Only an
        // unedited `base` changes.
        use crate::store_prompts as p;
        conn.execute_batch(&format!(
            "BEGIN; UPDATE prompts SET text = '{}', updated_at = CAST(strftime('%s', 'now') AS INTEGER) \
             WHERE name = '{}' AND text = '{}'; PRAGMA user_version = 21; COMMIT;",
            p::BUILTIN_PROMPT_V21.replace('\'', "''"),
            p::BASE_NAME,
            p::BUILTIN_PROMPT_V20.replace('\'', "''"),
        ))?;
    }
    if version < 22 {
        // The base prompt no longer states a call limit and asks the model to plan before it acts.
        // Only an unedited `base` changes.
        use crate::store_prompts as p;
        conn.execute_batch(&format!(
            "BEGIN; UPDATE prompts SET text = '{}', updated_at = CAST(strftime('%s', 'now') AS INTEGER) \
             WHERE name = '{}' AND text = '{}'; PRAGMA user_version = 22; COMMIT;",
            p::BUILTIN_PROMPT.replace('\'', "''"),
            p::BASE_NAME,
            p::BUILTIN_PROMPT_V21.replace('\'', "''"),
        ))?;
    }
    if version < 23 {
        // The base prompt names `grep`, `find` and `write_note`, and the arguments `read_file`,
        // `bash` and `task` gained. Only an unedited `base` (schema 22's text) changes.
        use crate::store_prompts as p;
        conn.execute_batch(&format!(
            "BEGIN; UPDATE prompts SET text = '{}', updated_at = CAST(strftime('%s', 'now') AS INTEGER) \
             WHERE name = '{}' AND text = '{}'; PRAGMA user_version = 23; COMMIT;",
            p::BUILTIN_PROMPT.replace('\'', "''"),
            p::BASE_NAME,
            p::BUILTIN_PROMPT_V22.replace('\'', "''"),
        ))?;
    }
    if version < 24 {
        // Issue 44: the MCP server tables.
        conn.execute_batch(&format!(
            "BEGIN; {SCHEMA_V24} PRAGMA user_version = 24; COMMIT;"
        ))?;
    }
    if version < 25 {
        // The base prompt asks for fewer round trips and names the file tools for reading. Only an
        // unedited `base` (schema 23's text) changes.
        use crate::store_prompts as p;
        conn.execute_batch(&format!(
            "BEGIN; UPDATE prompts SET text = '{}', updated_at = CAST(strftime('%s', 'now') AS INTEGER) \
             WHERE name = '{}' AND text = '{}'; PRAGMA user_version = 25; COMMIT;",
            p::BUILTIN_PROMPT.replace('\'', "''"),
            p::BASE_NAME,
            p::BUILTIN_PROMPT_V23.replace('\'', "''"),
        ))?;
    }
    Ok(())
}

fn role_and_text(message: &Message) -> Option<(&'static str, Option<&str>)> {
    match message {
        Message::System(_) => None,
        Message::User(content) => Some(("user", Some(content))),
        Message::Assistant { content, .. } => Some(("assistant", content.as_deref())),
        Message::Tool { content, .. } => Some(("tool", Some(content))),
    }
}

fn insert_messages<'a>(
    tx: &Transaction<'_>,
    session_id: i64,
    turn: u32,
    messages: impl Iterator<Item = &'a Message>,
    now: i64,
) -> Result<(), StoreError> {
    let first_seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), -1) + 1 FROM messages WHERE session_id = ?1",
        [session_id],
        |row| row.get(0),
    )?;
    // Rust note: `(first_seq..)` counts up forever; `zip` stops when `messages` does.
    for (seq, message) in (first_seq..).zip(messages) {
        let (role, text) = role_and_text(message).ok_or_else(|| {
            err("the system message is part of the session, not a stored message")
        })?;
        tx.execute(
            "INSERT INTO messages (session_id, seq, turn, role, json, text, status, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'ok', ?7)",
            params![
                session_id,
                seq,
                i64::from(turn),
                role,
                message_to_json(message),
                text,
                now
            ],
        )?;
    }
    Ok(())
}

fn int(value: Option<u64>) -> Option<i64> {
    value.and_then(|v| i64::try_from(v).ok())
}

fn insert_call(tx: &Transaction<'_>, call: &SaveCall<'_>, now: i64) -> Result<(), StoreError> {
    let (status, response, response_model, finish_reason, usage, elapsed, served, error) =
        match call.result {
            Ok(r) => (
                Some(i64::from(r.status)),
                Some(r.raw_body.as_str()),
                r.response_model.as_deref(),
                r.finish_reason.as_deref(),
                r.usage.as_deref(),
                r.elapsed,
                &r.served_by,
                None,
            ),
            Err(e) => (
                e.status.map(i64::from),
                e.raw_body.as_deref(),
                None,
                None,
                None,
                e.elapsed,
                &e.served_by,
                Some(e.message.as_str()),
            ),
        };
    let (prompt, cached, written) = usage.map_or((None, None, None), parse_usage);
    let completion = usage
        .and_then(|u| serde_json::from_str::<serde_json::Value>(u).ok())
        .and_then(|v| v["completion_tokens"].as_u64());
    tx.execute(
        "INSERT INTO calls (session_id, turn, message_count, request_sha256, request_bytes, \
         http_status, response, response_model, finish_reason, prompt_tokens, completion_tokens, \
         cached_tokens, cache_write_tokens, elapsed_ms, served_by_provider, served_by_account, \
         error, created_at, model, base_url) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, \
         ?19, ?20)",
        params![
            call.session_id,
            i64::from(call.turn),
            i64::try_from(call.message_count).unwrap_or(i64::MAX),
            sha256_hex(call.request),
            i64::try_from(call.request.len()).unwrap_or(i64::MAX),
            status,
            response,
            response_model,
            finish_reason,
            int(prompt),
            int(completion),
            int(cached),
            int(written),
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
            served.provider,
            served.account,
            error,
            now,
            call.model,
            call.base_url
        ],
    )?;
    Ok(())
}

fn mark_failed(tx: &Transaction<'_>, session_id: i64, turn: u32) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE messages SET status = 'failed' WHERE session_id = ?1 AND turn = ?2",
        params![session_id, i64::from(turn)],
    )?;
    Ok(())
}

fn touch(tx: &Transaction<'_>, session_id: i64, now: i64) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
        params![now, session_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ServedBy;
    use crate::tools::testutil::Scratch;
    use kernel::ToolCall;

    fn open(dir: &Scratch) -> Store {
        Store::open(&dir.path().join("harness.db")).unwrap()
    }

    fn new_session<'a>(router: &'a RouterOptions, prompt: &'a str) -> NewSession<'a> {
        NewSession {
            kind: Kind::Worker,
            model: "m",
            base_url: "http://localhost:20128/v1",
            router,
            system_prompt: prompt,
            cwd: "/work",
            profile: None,
            parent_id: None,
        }
    }

    fn reply(text: &str, usage: Option<&str>) -> Result<Reply, Box<ProviderError>> {
        Ok(Reply {
            status: 200,
            raw_body: format!(r#"{{"said":"{text}"}}"#),
            elapsed: Duration::from_millis(250),
            served_by: ServedBy {
                router: Some("localhost:20128".into()),
                provider: Some("Relace".into()),
                account: None,
            },
            message: Message::Assistant {
                content: Some(text.into()),
                tool_calls: vec![],
            },
            finish_reason: Some("stop".into()),
            response_model: Some("mimo".into()),
            usage: usage.map(str::to_owned),
        })
    }

    fn failure() -> Result<Reply, Box<ProviderError>> {
        Err(Box::new(ProviderError {
            message: "request failed: Connection refused".into(),
            status: None,
            raw_body: None,
            elapsed: Duration::from_millis(3),
            served_by: ServedBy::default(),
        }))
    }

    fn count(store: &Store, sql: &str) -> i64 {
        store.conn().query_row(sql, [], |row| row.get(0)).unwrap()
    }

    /// Saves one call with a user message and the model's text answer.
    fn save_answer(store: &mut Store, session: i64, turn: u32, question: &str, answer: &str) {
        let user = [Message::User(question.into())];
        let result = reply(answer, None);
        let Ok(r) = &result else { unreachable!() };
        store
            .save_call(&SaveCall {
                session_id: session,
                turn,
                pending: &user,
                reply: Some(&r.message),
                turn_failed: false,
                message_count: 1,
                request: b"{}",
                model: "m",
                base_url: "u",
                router: &RouterOptions::default(),
                result: &result,
            })
            .unwrap();
    }

    #[test]
    fn a_new_database_is_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Scratch::new("store-mode");
        let _store = open(&dir);
        let mode = |name: &str| {
            std::fs::metadata(dir.path().join(name)).map(|m| m.permissions().mode() & 0o777)
        };
        assert_eq!(mode("harness.db").unwrap(), 0o600);
        // The -wal and -shm files, once they exist, follow the main file.
        for name in ["harness.db-wal", "harness.db-shm"] {
            if let Ok(found) = mode(name) {
                assert_eq!(found, 0o600, "{name}");
            }
        }
    }

    #[test]
    fn a_new_database_has_the_tables_wal_and_foreign_keys() {
        let dir = Scratch::new("store-open");
        let store = open(&dir);
        assert_eq!(count(&store, "PRAGMA user_version"), SCHEMA_VERSION);
        for table in ["system_prompts", "sessions", "messages", "calls"] {
            let found = count(
                &store,
                &format!(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = '{table}'"
                ),
            );
            assert_eq!(found, 1, "{table}");
        }
        let mode: String = store
            .conn()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        assert_eq!(count(&store, "PRAGMA foreign_keys"), 1);
    }

    #[test]
    fn open_creates_the_folder_and_reopening_keeps_the_data() {
        let dir = Scratch::new("store-reopen");
        let path = dir.path().join("a/b/harness.db");
        let router = RouterOptions::default();
        {
            let mut store = Store::open(&path).unwrap();
            store.create_session(&new_session(&router, "p")).unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(count(&store, "SELECT COUNT(*) FROM sessions"), 1);
        assert_eq!(count(&store, "PRAGMA user_version"), SCHEMA_VERSION);
    }

    #[test]
    fn upgrading_to_schema_13_adds_parent_id_keeps_every_chat_and_makes_a_backup() {
        let dir = Scratch::new("store-v13");
        let path = dir.path().join("harness.db");
        let mut store = Store::open(&path).unwrap();
        let router = RouterOptions::default();
        let id = store.create_session(&new_session(&router, "p")).unwrap();
        drop(store);
        {
            // Put the file back as schema 12 left it.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "DROP INDEX sessions_parent; ALTER TABLE sessions DROP COLUMN parent_id; \
                 PRAGMA user_version = 12;",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(count(&store, "PRAGMA user_version"), SCHEMA_VERSION);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM sessions"), 1);
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM sessions WHERE parent_id IS NULL"
            ),
            1
        );
        assert_eq!(
            count(
                &store,
                &format!("SELECT COUNT(*) FROM sessions WHERE id = {id}")
            ),
            1
        );
        let copy = Connection::open(
            dir.path()
                .join(format!("harness.db.before-{SCHEMA_VERSION}")),
        )
        .unwrap();
        let version: i64 = copy
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 12);
        // Opening again changes nothing.
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(count(&store, "PRAGMA user_version"), SCHEMA_VERSION);
    }

    #[test]
    fn a_backup_name_that_is_taken_is_not_replaced() {
        let dir = Scratch::new("store-backup-name");
        let path = dir.path().join("harness.db");
        drop(Store::open(&path).unwrap());
        Connection::open(&path)
            .unwrap()
            .execute_batch("PRAGMA user_version = 12;")
            .unwrap();
        let taken = dir
            .path()
            .join(format!("harness.db.before-{SCHEMA_VERSION}"));
        std::fs::write(&taken, "an older copy").unwrap();
        drop(Store::open(&path).unwrap());
        assert_eq!(std::fs::read_to_string(&taken).unwrap(), "an older copy");
        assert!(
            dir.path()
                .join(format!("harness.db.before-{SCHEMA_VERSION}.2"))
                .exists()
        );
    }

    #[test]
    fn a_database_from_a_newer_program_is_refused() {
        let dir = Scratch::new("store-newer");
        let path = dir.path().join("harness.db");
        drop(Store::open(&path).unwrap());
        Connection::open(&path)
            .unwrap()
            .execute_batch("PRAGMA user_version = 99")
            .unwrap();
        let error = Store::open(&path).err().unwrap().to_string();
        assert!(error.contains("99") && error.contains("newer"), "{error}");
    }

    #[test]
    fn a_session_records_its_settings_and_never_a_key() {
        let dir = Scratch::new("store-session");
        let mut store = open(&dir);
        let router = RouterOptions {
            provider_order: vec!["relace/fp4".into()],
            allow_fallbacks: Some(false),
            session_id: Some("harness-test".into()),
        };
        let id = store
            .create_session(&new_session(&router, "prompt"))
            .unwrap();
        let (kind, model, base_url, options, cwd): (String, String, String, String, String) = store
            .conn()
            .query_row(
                "SELECT kind, model, base_url, router_options, cwd FROM sessions WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            (kind.as_str(), model.as_str(), cwd.as_str()),
            ("worker", "m", "/work")
        );
        assert_eq!(base_url, "http://localhost:20128/v1");
        let parsed: serde_json::Value = serde_json::from_str(&options).unwrap();
        assert_eq!(parsed["provider_order"][0], "relace/fp4");
        assert_eq!(parsed["allow_fallbacks"], false);
        assert_eq!(parsed["session_id"], "harness-test");
        assert_eq!(parsed.as_object().unwrap().len(), 3);
    }

    #[test]
    fn the_same_prompt_text_is_stored_once() {
        let dir = Scratch::new("store-prompts");
        let mut store = open(&dir);
        let router = RouterOptions::default();
        store.create_session(&new_session(&router, "same")).unwrap();
        store.create_session(&new_session(&router, "same")).unwrap();
        assert_eq!(count(&store, "SELECT COUNT(*) FROM system_prompts"), 1);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM sessions"), 2);
        store
            .create_session(&new_session(&router, "other"))
            .unwrap();
        assert_eq!(count(&store, "SELECT COUNT(*) FROM system_prompts"), 2);
        let hash: String = store
            .conn()
            .query_row(
                "SELECT sha256 FROM system_prompts WHERE text = 'same'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hash, sha256_hex(b"same"));
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn a_call_saves_its_messages_in_order_and_its_numbers() {
        let dir = Scratch::new("store-call");
        let mut store = open(&dir);
        let router = RouterOptions::default();
        let id = store.create_session(&new_session(&router, "p")).unwrap();

        // Call 1: the model answers with a tool call.
        let asked = Message::Assistant {
            content: None,
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: "{ \"path\" : \"a.txt\" }".into(),
            }],
        };
        let first = Ok(Reply {
            message: asked.clone(),
            ..reply("x", Some(r#"{"prompt_tokens":10,"completion_tokens":3}"#)).unwrap()
        });
        let user = [Message::User("read a.txt".into())];
        store
            .save_call(&SaveCall {
                session_id: id,
                turn: 1,
                pending: &user,
                reply: Some(&asked),
                turn_failed: false,
                message_count: 1,
                request: b"request-one",
                model: "m",
                base_url: "u",
                router: &RouterOptions::default(),
                result: &first,
            })
            .unwrap();
        // Call 2 carries the tool result, then the answer.
        let tool = [Message::Tool {
            tool_call_id: "c1".into(),
            content: "hello".into(),
        }];
        let usage = r#"{"prompt_tokens":40,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":32,"cache_write_tokens":2}}"#;
        let second = reply("it says hello", Some(usage));
        let answer = Message::Assistant {
            content: Some("it says hello".into()),
            tool_calls: vec![],
        };
        store
            .save_call(&SaveCall {
                session_id: id,
                turn: 1,
                pending: &tool,
                reply: Some(&answer),
                turn_failed: false,
                message_count: 3,
                request: b"request-two",
                model: "m",
                base_url: "u",
                router: &RouterOptions::default(),
                result: &second,
            })
            .unwrap();

        let rows: Vec<(i64, i64, String, Option<String>, String)> = store
            .conn()
            .prepare("SELECT seq, turn, role, text, status FROM messages ORDER BY seq")
            .unwrap()
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let roles: Vec<&str> = rows.iter().map(|r| r.2.as_str()).collect();
        assert_eq!(roles, ["user", "assistant", "tool", "assistant"]);
        assert_eq!(rows.iter().map(|r| r.0).collect::<Vec<_>>(), [0, 1, 2, 3]);
        assert!(rows.iter().all(|r| r.1 == 1 && r.4 == "ok"));
        // A tool-call message has no plain text.
        assert_eq!(rows[1].3, None);
        assert_eq!(rows[3].3.as_deref(), Some("it says hello"));

        let tool_json: String = store
            .conn()
            .query_row("SELECT json FROM messages WHERE seq = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tool_json, message_to_json(&asked));
        assert!(tool_json.contains(r#""arguments":"{ \"path\" : \"a.txt\" }""#));

        let (hash, bytes, count_in_request, prompt, cached, written, completion, provider): (
            String,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            String,
        ) = store
            .conn()
            .query_row(
                "SELECT request_sha256, request_bytes, message_count, prompt_tokens, cached_tokens, \
                 cache_write_tokens, completion_tokens, served_by_provider FROM calls WHERE id = 2",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(hash, sha256_hex(b"request-two"));
        assert_eq!(
            (bytes, count_in_request, prompt, cached, written, completion),
            (11, 3, 40, 32, 2, 5)
        );
        assert_eq!(provider, "Relace");
        assert_eq!(count(&store, "SELECT COUNT(*) FROM calls"), 2);
    }

    #[test]
    fn a_failed_call_is_a_row_with_an_error_and_marks_its_whole_turn_failed() {
        let dir = Scratch::new("store-failed");
        let mut store = open(&dir);
        let router = RouterOptions::default();
        let id = store.create_session(&new_session(&router, "p")).unwrap();
        save_answer(&mut store, id, 1, "first", "fine");
        // Turn 2: one good call (tool call), then a failure on the next call.
        let asked = Message::Assistant {
            content: None,
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            }],
        };
        let user = [Message::User("second".into())];
        let ok = reply("x", None);
        store
            .save_call(&SaveCall {
                session_id: id,
                turn: 2,
                pending: &user,
                reply: Some(&asked),
                turn_failed: false,
                message_count: 3,
                request: b"{}",
                model: "m",
                base_url: "u",
                router: &RouterOptions::default(),
                result: &ok,
            })
            .unwrap();
        let tool = [Message::Tool {
            tool_call_id: "c1".into(),
            content: "out".into(),
        }];
        let bad = failure();
        store
            .save_call(&SaveCall {
                session_id: id,
                turn: 2,
                pending: &tool,
                reply: None,
                turn_failed: true,
                message_count: 5,
                request: b"{}",
                model: "m",
                base_url: "u",
                router: &RouterOptions::default(),
                result: &bad,
            })
            .unwrap();

        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM messages WHERE turn = 1 AND status = 'ok'"
            ),
            2
        );
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM messages WHERE turn = 2 AND status = 'failed'"
            ),
            3
        );
        let (error, model): (String, Option<String>) = store
            .conn()
            .query_row(
                "SELECT error, response_model FROM calls ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(error, "request failed: Connection refused");
        assert_eq!(model, None);

        // Loading skips the failed turn but remembers its number.
        let loaded = store.load(id).unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.last_turn, 2);
        assert!(!loaded.dropped_unfinished_turn);
    }

    #[test]
    fn the_system_message_is_refused_and_nothing_is_saved() {
        let dir = Scratch::new("store-system");
        let mut store = open(&dir);
        let router = RouterOptions::default();
        let id = store.create_session(&new_session(&router, "p")).unwrap();
        let result = reply("x", None);
        let pending = [Message::User("hi".into()), Message::System("no".into())];
        let error = store
            .save_call(&SaveCall {
                session_id: id,
                turn: 1,
                pending: &pending,
                reply: None,
                turn_failed: false,
                message_count: 2,
                request: b"{}",
                model: "m",
                base_url: "u",
                router: &RouterOptions::default(),
                result: &result,
            })
            .err()
            .unwrap();
        assert!(error.to_string().contains("system message"));
        // The user message that came before it was rolled back too.
        assert_eq!(count(&store, "SELECT COUNT(*) FROM messages"), 0);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM calls"), 0);
    }

    #[test]
    fn a_call_for_an_unknown_session_is_refused() {
        let dir = Scratch::new("store-fk");
        let mut store = open(&dir);
        let result = reply("x", None);
        let pending = [Message::User("hi".into())];
        let error = store.save_call(&SaveCall {
            session_id: 999,
            turn: 1,
            pending: &pending,
            reply: None,
            turn_failed: false,
            message_count: 1,
            request: b"{}",
            model: "m",
            base_url: "u",
            router: &RouterOptions::default(),
            result: &result,
        });
        assert!(error.is_err());
        assert_eq!(count(&store, "SELECT COUNT(*) FROM messages"), 0);
    }

    #[test]
    fn two_messages_cannot_share_a_position() {
        let dir = Scratch::new("store-unique");
        let mut store = open(&dir);
        let router = RouterOptions::default();
        let id = store.create_session(&new_session(&router, "p")).unwrap();
        save_answer(&mut store, id, 1, "q", "a");
        let again = store.conn().execute(
            "INSERT INTO messages (session_id, seq, turn, role, json, status, created_at) \
             VALUES (?1, 0, 1, 'user', '{}', 'ok', 0)",
            [id],
        );
        assert!(again.is_err());
    }

    #[test]
    fn latest_session_is_the_most_recent_worker_with_a_usable_message() {
        let dir = Scratch::new("store-latest");
        let mut store = open(&dir);
        let router = RouterOptions::default();
        assert_eq!(store.latest_session(Kind::Worker).unwrap(), None);

        let older = store.create_session(&new_session(&router, "p")).unwrap();
        save_answer(&mut store, older, 1, "old", "a");
        let newer = store.create_session(&new_session(&router, "p")).unwrap();
        save_answer(&mut store, newer, 1, "new", "b");
        // Empty sessions never win, and neither do other kinds.
        store.create_session(&new_session(&router, "p")).unwrap();
        let mut other = new_session(&router, "p");
        other.kind = Kind::Orchestrator;
        let orchestrator = store.create_session(&other).unwrap();
        save_answer(&mut store, orchestrator, 1, "po", "c");

        // Make "older" the most recently used one.
        store
            .conn()
            .execute(
                "UPDATE sessions SET updated_at = updated_at + 10 WHERE id = ?1",
                [older],
            )
            .unwrap();
        assert_eq!(
            store.latest_session(Kind::Worker).unwrap().unwrap().id,
            older
        );
        store
            .conn()
            .execute(
                "UPDATE sessions SET updated_at = updated_at + 20 WHERE id = ?1",
                [newer],
            )
            .unwrap();
        let latest = store.latest_session(Kind::Worker).unwrap().unwrap();
        assert_eq!(latest.id, newer);
        assert_eq!(latest.system_prompt, "p");
        assert_eq!(latest.cwd, "/work");
        assert_eq!(
            store
                .latest_session(Kind::Orchestrator)
                .unwrap()
                .unwrap()
                .id,
            orchestrator
        );
    }

    #[test]
    fn load_returns_the_messages_in_order_exactly() {
        let dir = Scratch::new("store-load");
        let mut store = open(&dir);
        let router = RouterOptions::default();
        let id = store
            .create_session(&new_session(&router, "the prompt"))
            .unwrap();
        save_answer(&mut store, id, 1, "héllo \"q\"\n", "one");
        save_answer(&mut store, id, 2, "again", "two");
        let loaded = store.load(id).unwrap();
        assert_eq!(loaded.info.system_prompt, "the prompt");
        assert_eq!(loaded.last_turn, 2);
        assert_eq!(
            loaded.messages,
            vec![
                Message::User("héllo \"q\"\n".into()),
                Message::Assistant {
                    content: Some("one".into()),
                    tool_calls: vec![]
                },
                Message::User("again".into()),
                Message::Assistant {
                    content: Some("two".into()),
                    tool_calls: vec![]
                },
            ]
        );
        assert!(
            store
                .load(12345)
                .err()
                .unwrap()
                .to_string()
                .contains("no saved chat")
        );
    }

    #[test]
    fn an_unfinished_last_turn_is_marked_failed_and_left_out() {
        let dir = Scratch::new("store-unfinished");
        let mut store = open(&dir);
        let router = RouterOptions::default();
        let id = store.create_session(&new_session(&router, "p")).unwrap();
        save_answer(&mut store, id, 1, "done", "fine");
        // Turn 2 stopped after the model asked for a tool.
        let asked = Message::Assistant {
            content: None,
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            }],
        };
        let user = [Message::User("never finished".into())];
        let ok = reply("x", None);
        store
            .save_call(&SaveCall {
                session_id: id,
                turn: 2,
                pending: &user,
                reply: Some(&asked),
                turn_failed: false,
                message_count: 3,
                request: b"{}",
                model: "m",
                base_url: "u",
                router: &RouterOptions::default(),
                result: &ok,
            })
            .unwrap();
        let loaded = store.load(id).unwrap();
        assert!(loaded.dropped_unfinished_turn);
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.last_turn, 2);
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM messages WHERE status = 'failed'"
            ),
            2
        );
        // Loading again finds nothing further to drop.
        assert!(!store.load(id).unwrap().dropped_unfinished_turn);
    }

    #[test]
    fn a_turn_that_ends_with_tool_results_is_kept() {
        let dir = Scratch::new("store-cap");
        let mut store = open(&dir);
        let router = RouterOptions::default();
        let id = store.create_session(&new_session(&router, "p")).unwrap();
        let asked = Message::Assistant {
            content: None,
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            }],
        };
        let user = [Message::User("loop".into())];
        let ok = reply("x", None);
        store
            .save_call(&SaveCall {
                session_id: id,
                turn: 1,
                pending: &user,
                reply: Some(&asked),
                turn_failed: false,
                message_count: 1,
                request: b"{}",
                model: "m",
                base_url: "u",
                router: &RouterOptions::default(),
                result: &ok,
            })
            .unwrap();
        store
            .save_messages(
                id,
                1,
                &[Message::Tool {
                    tool_call_id: "c1".into(),
                    content: "out".into(),
                }],
            )
            .unwrap();
        let loaded = store.load(id).unwrap();
        assert!(!loaded.dropped_unfinished_turn);
        assert_eq!(loaded.messages.len(), 3);
    }

    #[test]
    fn an_unreadable_saved_message_gives_a_clear_error() {
        let dir = Scratch::new("store-corrupt");
        let mut store = open(&dir);
        let router = RouterOptions::default();
        let id = store.create_session(&new_session(&router, "p")).unwrap();
        save_answer(&mut store, id, 1, "q", "a");
        store
            .conn()
            .execute("UPDATE messages SET json = 'garbage' WHERE seq = 0", [])
            .unwrap();
        let error = store.load(id).err().unwrap().to_string();
        assert!(
            error.contains("unreadable") && error.contains("turn 1"),
            "{error}"
        );
    }

    #[test]
    fn differences_name_what_changed_and_nothing_else() {
        let router = RouterOptions::default();
        let info = SessionInfo {
            id: 1,
            model: "a".into(),
            base_url: "http://x/v1".into(),
            router_options: router_json(&router),
            system_prompt: String::new(),
            cwd: String::new(),
            created_at: 0,
            updated_at: 0,
            profile: None,
        };
        assert!(
            info.differences("a", "http://x/v1", &router_json(&router))
                .is_empty()
        );
        let pinned = RouterOptions {
            provider_order: vec!["p".into()],
            ..RouterOptions::default()
        };
        let found = info.differences("b", "http://y/v1", &router_json(&pinned));
        assert_eq!(found.len(), 3);
        assert!(found[0].contains("a") && found[0].contains("b"));
        assert!(found[2].contains("provider"));
    }

    #[test]
    fn router_json_is_stable_and_has_only_the_three_settings() {
        let router = RouterOptions {
            provider_order: vec!["x".into(), "y".into()],
            allow_fallbacks: None,
            session_id: None,
        };
        assert_eq!(router_json(&router), router_json(&router.clone()));
        assert_eq!(
            router_json(&router),
            r#"{"allow_fallbacks":null,"provider_order":["x","y"],"session_id":null}"#
        );
    }

    #[test]
    fn the_chat_list_is_newest_first_limited_and_only_usable_chats() {
        let dir = Scratch::new("store-list");
        let mut store = open(&dir);
        let router = RouterOptions::default();
        assert!(store.list_sessions(Kind::Worker, 10).unwrap().is_empty());

        let first = store.create_session(&new_session(&router, "p")).unwrap();
        save_answer(&mut store, first, 1, "first question", "a");
        save_answer(&mut store, first, 2, "more", "b");
        let second = store.create_session(&new_session(&router, "p")).unwrap();
        save_answer(&mut store, second, 1, "second question", "c");
        let third = store.create_session(&new_session(&router, "p")).unwrap();
        save_answer(&mut store, third, 1, "third question", "d");
        // Never listed: an empty chat, and a chat of another kind.
        store.create_session(&new_session(&router, "p")).unwrap();
        let mut other = new_session(&router, "p");
        other.kind = Kind::Orchestrator;
        let po = store.create_session(&other).unwrap();
        save_answer(&mut store, po, 1, "po", "e");
        // Order by last use, not by id.
        store
            .conn()
            .execute(
                "UPDATE sessions SET updated_at = updated_at + 100 WHERE id = ?1",
                [first],
            )
            .unwrap();

        let list = store.list_sessions(Kind::Worker, 10).unwrap();
        assert_eq!(
            list.iter().map(|s| s.id).collect::<Vec<_>>(),
            [first, third, second]
        );
        assert_eq!(list[0].message_count, 4);
        assert_eq!(list[0].first_message.as_deref(), Some("first question"));
        assert_eq!(list[0].model, "m");
        assert_eq!(store.list_sessions(Kind::Worker, 2).unwrap().len(), 2);
        assert_eq!(
            store.list_sessions(Kind::Orchestrator, 10).unwrap().len(),
            1
        );
    }

    #[test]
    fn the_list_names_a_chat_by_what_was_typed_and_finds_chats_made_from_it() {
        use crate::store_prompts::{ACTIVE_MARK, summary_message};
        let dir = Scratch::new("store-list-names");
        let mut store = open(&dir);
        let router = RouterOptions::default();
        let a = store.create_session(&new_session(&router, "p")).unwrap();
        save_answer(&mut store, a, 1, &format!("{ACTIVE_MARK}: skills"), "ok");
        save_answer(&mut store, a, 2, "typed question", "a");
        let b = store.create_session(&new_session(&router, "p")).unwrap();
        save_answer(&mut store, b, 1, &summary_message(Some(a), "S"), "ok");
        save_answer(&mut store, b, 2, "next", "b");
        let c = store.create_session(&new_session(&router, "p")).unwrap();
        save_answer(&mut store, c, 1, &summary_message(None, "S"), "ok");

        let list = store.list_sessions(Kind::Worker, 10).unwrap();
        let of = |id: i64| list.iter().find(|s| s.id == id).unwrap();
        assert_eq!(of(a).first_message.as_deref(), Some("typed question"));
        assert!(!of(a).continued);
        assert_eq!(of(b).first_message.as_deref(), Some("next"));
        assert!(of(b).continued);
        assert_eq!(of(c).first_message, None);
        assert!(of(c).continued);

        assert_eq!(store.chats_continuing(a).unwrap(), [b]);
        assert!(store.chats_continuing(b).unwrap().is_empty());
        // Chat 1 is not mistaken for chat 10 and the like: the number is closed by "]".
        assert!(store.chats_continuing(a * 10).unwrap().is_empty());
    }

    #[test]
    fn failed_messages_do_not_count_or_name_a_chat() {
        let dir = Scratch::new("store-list-failed");
        let mut store = open(&dir);
        let router = RouterOptions::default();
        let id = store.create_session(&new_session(&router, "p")).unwrap();
        // Turn 1 failed; turn 2 worked.
        let user = [Message::User("lost question".into())];
        let bad = failure();
        store
            .save_call(&SaveCall {
                session_id: id,
                turn: 1,
                pending: &user,
                reply: None,
                turn_failed: true,
                message_count: 1,
                request: b"{}",
                model: "m",
                base_url: "u",
                router: &RouterOptions::default(),
                result: &bad,
            })
            .unwrap();
        save_answer(&mut store, id, 2, "kept question", "fine");
        let list = store.list_sessions(Kind::Worker, 10).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].message_count, 2);
        assert_eq!(list[0].first_message.as_deref(), Some("kept question"));
    }

    // ---- schema 2 and the providers table (issue 10) ----

    #[test]
    fn a_version_one_database_is_upgraded_and_keeps_its_rows() {
        let dir = Scratch::new("store-v1-to-v2");
        let path = dir.path().join("harness.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(&format!("{SCHEMA_V1} PRAGMA user_version = 1;"))
                .unwrap();
            conn.execute_batch(
                "INSERT INTO system_prompts (sha256, text, created_at) VALUES ('h', 'p', 1);
                 INSERT INTO sessions (model, base_url, router_options, system_prompt_id, cwd,
                     harness_version, created_at, updated_at)
                     VALUES ('old-model', 'http://old/v1', '{}', 1, '/w', '0', 1, 1);
                 INSERT INTO calls (session_id, turn, message_count, request_sha256,
                     request_bytes, elapsed_ms, created_at) VALUES (1, 1, 1, 'x', 2, 3, 1);",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(count(&store, "PRAGMA user_version"), SCHEMA_VERSION);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM calls"), 1);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM sessions"), 1);
        // Old calls did not record where they went.
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM calls WHERE model IS NULL AND base_url IS NULL"
            ),
            1
        );
        assert_eq!(count(&store, "SELECT COUNT(*) FROM providers"), 0);
        // Opening again changes nothing.
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(count(&store, "PRAGMA user_version"), SCHEMA_VERSION);
    }

    #[test]
    fn a_version_two_database_gains_the_settings_table() {
        let dir = Scratch::new("store-v2-to-v3");
        let path = dir.path().join("harness.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(&format!(
                "{SCHEMA_V1} {SCHEMA_V2} PRAGMA user_version = 2;
                 INSERT INTO providers (name, base_url, model, created_at)
                     VALUES ('kept', 'http://k/v1', 'm', 1);"
            ))
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(count(&store, "PRAGMA user_version"), SCHEMA_VERSION);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM providers"), 1);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM provider_models"), 0);
        assert_eq!(store.setting(LAST_PROVIDER).unwrap(), None);
    }

    #[test]
    fn settings_are_read_back_and_overwritten() {
        let dir = Scratch::new("store-settings");
        let mut store = open(&dir);
        assert_eq!(store.setting("k").unwrap(), None);
        store.set_setting("k", "one").unwrap();
        assert_eq!(store.setting("k").unwrap().as_deref(), Some("one"));
        store.set_setting("k", "two").unwrap();
        assert_eq!(store.setting("k").unwrap().as_deref(), Some("two"));
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM settings WHERE key = 'k'"),
            1
        );
    }

    #[test]
    fn a_call_records_where_it_went_and_the_chat_row_follows() {
        let dir = Scratch::new("store-call-target");
        let mut store = open(&dir);
        let router = RouterOptions::default();
        let id = store.create_session(&new_session(&router, "p")).unwrap();
        let pinned = RouterOptions {
            provider_order: vec!["relace/fp4".into()],
            allow_fallbacks: Some(false),
            session_id: None,
        };
        let result = reply("hi", None);
        let Ok(r) = &result else { unreachable!() };
        let user = [Message::User("q".into())];
        store
            .save_call(&SaveCall {
                session_id: id,
                turn: 1,
                pending: &user,
                reply: Some(&r.message),
                turn_failed: false,
                message_count: 1,
                request: b"{}",
                model: "second-model",
                base_url: "http://second/v1",
                router: &pinned,
                result: &result,
            })
            .unwrap();
        let (model, url): (String, String) = store
            .conn()
            .query_row("SELECT model, base_url FROM calls", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(
            (model.as_str(), url.as_str()),
            ("second-model", "http://second/v1")
        );
        let info = store.load(id).unwrap().info;
        assert_eq!(info.model, "second-model");
        assert_eq!(info.base_url, "http://second/v1");
        assert_eq!(info.router_options, router_json(&pinned));
    }

    fn provider(name: &str) -> ProviderDef {
        ProviderDef {
            api: crate::config::ApiStyle::OpenAi,
            name: name.into(),
            base_url: format!("http://{name}/v1"),
            model: format!("{name}-model"),
            api_key: Some(format!("key-{name}")),
            router: RouterOptions {
                provider_order: vec!["a".into(), "b".into()],
                allow_fallbacks: Some(true),
                session_id: Some("sid".into()),
            },
        }
    }

    #[test]
    fn providers_round_trip_with_their_pinning_and_key() {
        let dir = Scratch::new("store-providers");
        let mut store = open(&dir);
        store.add_provider(&provider("zeta")).unwrap();
        let mut plain = provider("alpha");
        plain.api_key = None;
        plain.router = RouterOptions::default();
        store.add_provider(&plain).unwrap();
        let all = store.list_providers().unwrap();
        assert_eq!(
            all.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["alpha", "zeta"]
        );
        assert_eq!(all[0].api_key, None);
        assert_eq!(all[0].router, RouterOptions::default());
        assert_eq!(all[1].api_key.as_deref(), Some("key-zeta"));
        assert_eq!(all[1].router, provider("zeta").router);
        assert_eq!(all[1].base_url, "http://zeta/v1");
        assert_eq!(all[1].model, "zeta-model");
    }

    #[test]
    fn a_provider_name_is_unique_and_removal_says_whether_it_removed() {
        let dir = Scratch::new("store-provider-unique");
        let mut store = open(&dir);
        store.add_provider(&provider("p")).unwrap();
        let again = store.add_provider(&provider("p")).unwrap_err();
        assert!(again.to_string().contains("already a saved provider"));
        assert_eq!(count(&store, "SELECT COUNT(*) FROM providers"), 1);
        assert!(store.remove_provider("p").unwrap());
        assert!(!store.remove_provider("p").unwrap());
        assert!(store.list_providers().unwrap().is_empty());
    }
}

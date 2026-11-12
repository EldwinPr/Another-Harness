//! Moving a whole database in and out (issue 57): `--export`, `--import` (as a separate "space" or
//! merged into the open one) and `--wipe`.
//!
//! The database file is the unit. An export is a consistent copy made with `VACUUM INTO`; a space
//! is another database file in the data folder's `spaces/` that `--space NAME` opens in place of
//! `harness.db`; a merge adds what the other file has and this one lacks, never replacing a thing
//! that is here; a wipe saves a copy and then empties the database, leaving the seeded defaults.
//!
//! Every function here works on files it is given, so a test can use a temporary folder. None of
//! them touches the turn logs except [`remove_turn_logs`], which the owner asks for by name.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};

use crate::store::Store;

/// The folder inside the data folder that holds the spaces.
pub const SPACES_DIR: &str = "spaces";

/// The name of the main database, which a space cannot take.
pub const MAIN_NAME: &str = "harness";

/// A space name: letters, digits, `-` and `_`, up to 40, and not the main database's name.
pub fn check_space_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        return Err(format!(
            "'{name}' is not a space name: use letters, digits, - and _ (up to 40)"
        ));
    }
    if name.eq_ignore_ascii_case(MAIN_NAME) {
        return Err(format!("'{MAIN_NAME}' is the main database, not a space"));
    }
    Ok(())
}

/// Where the space called `name` lives.
pub fn space_path(data_dir: &Path, name: &str) -> PathBuf {
    data_dir.join(SPACES_DIR).join(format!("{name}.db"))
}

/// The spaces there are, by name.
pub fn list_spaces(data_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(data_dir.join(SPACES_DIR))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".db").map(str::to_owned)
        })
        .collect();
    names.sort();
    names
}

fn err(text: impl Into<String>) -> String {
    text.into()
}

/// A new empty file only its owner can read, failing if the path is taken. `VACUUM INTO` accepts
/// an empty file, so the copy never exists with looser permissions.
fn private_empty_file(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .map_err(|e| err(format!("cannot create {}: {e}", dir.display())))?;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map(|_| ())
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => format!(
                "{} already exists: nothing is ever replaced, choose another name",
                path.display()
            ),
            _ => format!("cannot create {}: {e}", path.display()),
        })
}

/// A consistent copy of the open database at `to`, readable by its owner only.
fn copy_database(conn: &Connection, to: &Path) -> Result<(), String> {
    private_empty_file(to)?;
    let target = to.to_string_lossy().into_owned();
    conn.execute("VACUUM INTO ?1", [&target]).map_err(|e| {
        let _ = std::fs::remove_file(to);
        format!("cannot copy the database to {target}: {e}")
    })?;
    Ok(())
}

/// A backup next to the database, named `<file>.<label>-<UTC stamp>`; an earlier copy is never
/// replaced (`.2`, `.3` ... when the name is taken). Its path.
fn back_up(conn: &Connection, db: &Path, label: &str) -> Result<PathBuf, String> {
    let name = db
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("the database has no file name")?;
    let stamp = crate::turn_log::utc_stamp(u64::try_from(crate::store::now_secs()).unwrap_or(0));
    let mut copy = db.with_file_name(format!("{name}.{label}-{stamp}"));
    let mut n = 1;
    while copy.exists() {
        n += 1;
        copy = db.with_file_name(format!("{name}.{label}-{stamp}.{n}"));
    }
    copy_database(conn, &copy)?;
    Ok(copy)
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).unwrap_or(0)
}

// ---- export ----

/// What an export wrote.
#[derive(Debug, PartialEq, Eq)]
pub struct ExportReport {
    pub chats: i64,
    pub providers: i64,
    /// Provider keys and MCP environment values were left out.
    pub keys_removed: bool,
}

/// Writes a copy of the database at `db` to `to`. Unless `with_keys`, the provider API keys and
/// the environment values of the MCP servers (which often hold tokens) are cleared in the copy;
/// the database itself is not changed. Chat text is copied as it is.
pub fn export(db: &Path, to: &Path, with_keys: bool) -> Result<ExportReport, String> {
    let store = Store::open(db).map_err(|e| format!("cannot open {}: {e}", db.display()))?;
    copy_database(&store.conn, to)?;
    let copy = Connection::open(to).map_err(|e| format!("cannot open the copy: {e}"))?;
    if !with_keys {
        let scrub = copy.execute_batch(
            "UPDATE providers SET api_key = NULL; UPDATE mcp_servers SET env = '{}';",
        );
        if let Err(e) = scrub {
            drop(copy);
            let _ = std::fs::remove_file(to);
            return Err(format!(
                "cannot clear the keys, so nothing was exported: {e}"
            ));
        }
    }
    Ok(ExportReport {
        chats: count(
            &copy,
            "SELECT COUNT(*) FROM sessions WHERE parent_id IS NULL",
        ),
        providers: count(&copy, "SELECT COUNT(*) FROM providers"),
        keys_removed: !with_keys,
    })
}

// ---- import as a space ----

/// Copies `file` into the spaces folder as `name` and brings it to this program's schema. The
/// file is not changed. Refuses a name that is taken, a file that is not a database, and a
/// database written by a newer program.
pub fn import_as_space(file: &Path, data_dir: &Path, name: &str) -> Result<PathBuf, String> {
    check_space_name(name)?;
    let to = space_path(data_dir, name);
    if !file.is_file() {
        return Err(format!("{} is not a file", file.display()));
    }
    private_empty_file(&to)?;
    let done = std::fs::copy(file, &to)
        .map_err(|e| format!("cannot copy {}: {e}", file.display()))
        .and_then(|_| {
            // Opening it upgrades an older file (leaving the usual `.before-N` copy beside it) and
            // refuses a newer one or one that is not a database.
            Store::open(&to)
                .map(|_| ())
                .map_err(|e| format!("{} is not a usable database: {e}", file.display()))
        });
    if let Err(why) = done {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", to.display()));
        }
        return Err(why);
    }
    Ok(to)
}

// ---- merge ----

/// How many rows a merge added to one table, and how many it left alone because the open database
/// already had them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Tally {
    pub added: usize,
    pub kept: usize,
}

/// What a merge did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MergeReport {
    pub chats: Tally,
    pub messages: usize,
    pub prompts: Tally,
    pub profiles: Tally,
    pub providers: Tally,
    pub mcp_servers: Tally,
    pub glossary: Tally,
    pub context: Tally,
    pub gotchas: Tally,
    pub work_logs: Tally,
    /// Inbox entries and settings of the other file; they are never merged.
    pub not_merged: usize,
    /// The copy of the open database made before anything changed.
    pub backup: Option<PathBuf>,
}

impl MergeReport {
    /// The report as lines for the screen.
    pub fn lines(&self) -> Vec<String> {
        let row = |label: &str, t: &Tally| {
            format!("  {label:<12} {} added, {} already here", t.added, t.kept)
        };
        let mut out = vec![
            row("chats", &self.chats),
            format!("  {:<12} {} added", "messages", self.messages),
            row("prompts", &self.prompts),
            row("profiles", &self.profiles),
            row("providers", &self.providers),
            row("mcp servers", &self.mcp_servers),
            row("glossary", &self.glossary),
            row("context", &self.context),
            row("gotchas", &self.gotchas),
            row("work logs", &self.work_logs),
        ];
        if self.not_merged > 0 {
            out.push(format!(
                "  {} inbox entries and settings were not merged (the open database keeps its own)",
                self.not_merged
            ));
        }
        out
    }
}

/// Which rows [`copy_in`] may bring across.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fill {
    /// A merge: only what the open database lacks; settings and the inbox stay as they are.
    Merge,
    /// After a wipe: the same, plus the settings of the fresh database.
    Restore,
}

fn tmp_dir(tag: &str) -> Result<PathBuf, String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!("aharness-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Merges the database in `file` into the one at `db`. A copy of `db` is saved first; `file` is
/// read from a copy, upgraded if older and refused if newer, and never changed. A thing that is
/// already in `db` (a prompt, profile, provider, glossary term, ... with the same name, or a chat
/// with the same start) is kept as it is. All of it happens in one transaction, and a reference
/// that does not resolve ends it with nothing changed.
pub fn merge(db: &Path, file: &Path) -> Result<MergeReport, String> {
    if !file.is_file() {
        return Err(format!("{} is not a file", file.display()));
    }
    let tmp = tmp_dir("merge")?;
    let result = (|| {
        let copy = tmp.join("import.db");
        std::fs::copy(file, &copy).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
        Store::open(&copy)
            .map_err(|e| format!("{} is not a usable database: {e}", file.display()))?;
        let store = Store::open(db).map_err(|e| format!("cannot open {}: {e}", db.display()))?;
        let backup = back_up(&store.conn, db, "before-merge")?;
        let mut report = copy_in(&store.conn, &copy, Fill::Merge)?;
        report.backup = Some(backup);
        Ok(report)
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

fn copy_in(conn: &Connection, other: &Path, how: Fill) -> Result<MergeReport, String> {
    let other_text = other.to_string_lossy().into_owned();
    conn.execute("ATTACH DATABASE ?1 AS imp", [&other_text])
        .map_err(|e| format!("cannot read the other database: {e}"))?;
    let outcome = (|| {
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| format!("the database is busy: {e}"))?;
        match run_copy(conn, how) {
            Ok(report) => {
                let broken = count(conn, "SELECT COUNT(*) FROM pragma_foreign_key_check");
                if broken > 0 {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(format!(
                        "{broken} references would not resolve, so nothing was merged"
                    ));
                }
                conn.execute_batch("COMMIT")
                    .map_err(|e| format!("cannot save the merge: {e}"))?;
                Ok(report)
            }
            Err(why) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(why)
            }
        }
    })();
    let _ = conn.execute_batch("DETACH DATABASE imp");
    outcome
}

fn sql_err(e: rusqlite::Error) -> String {
    format!("merge failed: {e}")
}

/// Reads rows of the other database.
fn read<T>(
    conn: &Connection,
    sql: &str,
    f: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>, String> {
    let mut statement = conn.prepare(sql).map_err(sql_err)?;
    let rows = statement.query_map([], f).map_err(sql_err)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(sql_err)
}

/// Rows added by an `INSERT OR IGNORE ... SELECT`, and rows of the other database it left out.
fn insert_new(conn: &Connection, table: &str, sql: &str) -> Result<Tally, String> {
    let total =
        usize::try_from(count(conn, &format!("SELECT COUNT(*) FROM imp.{table}"))).unwrap_or(0);
    let added = conn.execute(sql, []).map_err(sql_err)?;
    Ok(Tally {
        added,
        kept: total.saturating_sub(added),
    })
}

fn run_copy(conn: &Connection, how: Fill) -> Result<MergeReport, String> {
    let mut report = MergeReport::default();

    // Prompts, skills and personalities, by name. They arrive switched off.
    let mut prompts: HashMap<i64, i64> = HashMap::new();
    for (id, name) in read(conn, "SELECT id, name FROM imp.prompts ORDER BY id", |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })? {
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM main.prompts WHERE name = ?1",
                [&name],
                |r| r.get(0),
            )
            .ok();
        if let Some(existing) = existing {
            prompts.insert(id, existing);
            report.prompts.kept += 1;
            continue;
        }
        conn.execute(
            "INSERT INTO main.prompts (name, description, text, source_path, enabled, position, created_at, updated_at, type) \
             SELECT name, description, text, source_path, 0, (SELECT COALESCE(MAX(position), 0) + 1 FROM main.prompts), created_at, updated_at, type \
             FROM imp.prompts WHERE id = ?1",
            [id],
        )
        .map_err(sql_err)?;
        prompts.insert(id, conn.last_insert_rowid());
        report.prompts.added += 1;
    }

    // Profiles, by name, with their prompts and skills.
    let mut profiles: HashMap<i64, i64> = HashMap::new();
    let imported = read(
        conn,
        "SELECT id, name, personality_id FROM imp.profiles ORDER BY id",
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?,
            ))
        },
    )?;
    for (id, name, personality) in imported {
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM main.profiles WHERE name = ?1",
                [&name],
                |r| r.get(0),
            )
            .ok();
        if let Some(existing) = existing {
            profiles.insert(id, existing);
            report.profiles.kept += 1;
            continue;
        }
        let personality = personality.and_then(|p| prompts.get(&p).copied());
        conn.execute(
            "INSERT INTO main.profiles (name, description, personality_id, created_at, updated_at, tools) \
             SELECT name, description, ?2, created_at, updated_at, tools FROM imp.profiles WHERE id = ?1",
            params![id, personality],
        )
        .map_err(sql_err)?;
        let new = conn.last_insert_rowid();
        profiles.insert(id, new);
        report.profiles.added += 1;
        for table in ["profile_prompts", "profile_skills"] {
            let links = read(
                conn,
                &format!("SELECT prompt_id FROM imp.{table} WHERE profile_id = {id}"),
                |r| r.get::<_, i64>(0),
            )?;
            for prompt in links {
                if let Some(mapped) = prompts.get(&prompt) {
                    conn.execute(
                        &format!("INSERT OR IGNORE INTO main.{table} (profile_id, prompt_id) VALUES (?1, ?2)"),
                        params![new, mapped],
                    )
                    .map_err(sql_err)?;
                }
            }
        }
    }

    // Providers (by name, with their models) and MCP servers (by name, with their tool rules).
    let mut providers: HashMap<i64, i64> = HashMap::new();
    for (id, name) in read(
        conn,
        "SELECT id, name FROM imp.providers ORDER BY id",
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
    )? {
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM main.providers WHERE name = ?1",
                [&name],
                |r| r.get(0),
            )
            .ok();
        if let Some(existing) = existing {
            providers.insert(id, existing);
            report.providers.kept += 1;
            continue;
        }
        conn.execute(
            "INSERT INTO main.providers (name, base_url, model, api_key, provider_order, allow_fallbacks, session_id, created_at, api) \
             SELECT name, base_url, model, api_key, provider_order, allow_fallbacks, session_id, created_at, api \
             FROM imp.providers WHERE id = ?1",
            [id],
        )
        .map_err(sql_err)?;
        let new = conn.last_insert_rowid();
        providers.insert(id, new);
        report.providers.added += 1;
        conn.execute(
            "INSERT OR IGNORE INTO main.provider_models (provider_id, model_id, display_name, origin_id, hidden, created_at) \
             SELECT ?2, model_id, display_name, origin_id, hidden, created_at FROM imp.provider_models WHERE provider_id = ?1",
            params![id, new],
        )
        .map_err(sql_err)?;
    }
    let mut servers: HashMap<i64, i64> = HashMap::new();
    for (id, name) in read(
        conn,
        "SELECT id, name FROM imp.mcp_servers ORDER BY id",
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
    )? {
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM main.mcp_servers WHERE name = ?1",
                [&name],
                |r| r.get(0),
            )
            .ok();
        if let Some(existing) = existing {
            servers.insert(id, existing);
            report.mcp_servers.kept += 1;
            continue;
        }
        conn.execute(
            "INSERT INTO main.mcp_servers (name, command, args, env, timeout_secs, enabled, default_rule, created_at, updated_at) \
             SELECT name, command, args, env, timeout_secs, enabled, default_rule, created_at, updated_at \
             FROM imp.mcp_servers WHERE id = ?1",
            [id],
        )
        .map_err(sql_err)?;
        let new = conn.last_insert_rowid();
        servers.insert(id, new);
        report.mcp_servers.added += 1;
        conn.execute(
            "INSERT OR IGNORE INTO main.mcp_tool_rules (server_id, tool, rule) \
             SELECT ?2, tool, rule FROM imp.mcp_tool_rules WHERE server_id = ?1",
            params![id, new],
        )
        .map_err(sql_err)?;
    }

    // The glossary, the context entries (with their quotes) and the gotchas: by project and name.
    report.glossary = insert_new(
        conn,
        "glossary",
        "INSERT OR IGNORE INTO main.glossary (term, area, definition, project, created_at, updated_at) \
         SELECT term, area, definition, project, created_at, updated_at FROM imp.glossary ORDER BY id",
    )?;
    report.gotchas = insert_new(
        conn,
        "gotchas",
        "INSERT OR IGNORE INTO main.gotchas (name, type, text, source, project, created_at, updated_at) \
         SELECT name, type, text, source, project, created_at, updated_at FROM imp.gotchas ORDER BY id",
    )?;
    for id in read(
        conn,
        "SELECT id FROM imp.context_entries ORDER BY id",
        |r| r.get::<_, i64>(0),
    )? {
        let added = conn
            .execute(
                "INSERT OR IGNORE INTO main.context_entries (name, kind, text, quote, area, project, preload, created_at, updated_at) \
                 SELECT name, kind, text, quote, area, project, preload, created_at, updated_at FROM imp.context_entries WHERE id = ?1",
                [id],
            )
            .map_err(sql_err)?;
        if added == 1 {
            report.context.added += 1;
            conn.execute(
                "INSERT INTO main.context_quotes (entry_id, quote, said_on, claim, replaces, created_at) \
                 SELECT ?2, quote, said_on, claim, replaces, created_at FROM imp.context_quotes WHERE entry_id = ?1",
                params![id, conn.last_insert_rowid()],
            )
            .map_err(sql_err)?;
        } else {
            report.context.kept += 1;
        }
    }

    // Chats. A chat is the same one when its start (time, folder, model, prompt, profile) is the
    // same, so merging a file twice does not copy its chats twice.
    conn.execute(
        "INSERT OR IGNORE INTO main.system_prompts (sha256, text, created_at) \
         SELECT sha256, text, created_at FROM imp.system_prompts",
        [],
    )
    .map_err(sql_err)?;
    let mut chats: HashMap<i64, i64> = HashMap::new();
    let imported = read(
        conn,
        "SELECT s.id, s.parent_id, p.sha256 FROM imp.sessions s \
         JOIN imp.system_prompts p ON p.id = s.system_prompt_id ORDER BY s.id",
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, String>(2)?,
            ))
        },
    )?;
    for (id, parent, sha) in imported {
        let new_prompt: i64 = conn
            .query_row(
                "SELECT id FROM main.system_prompts WHERE sha256 = ?1",
                [&sha],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        let existing: Option<i64> = conn
            .query_row(
                "SELECT m.id FROM main.sessions m JOIN imp.sessions i ON i.id = ?1 \
                 WHERE m.created_at = i.created_at AND m.cwd = i.cwd AND m.model = i.model \
                   AND m.system_prompt_id = ?2 AND m.profile IS i.profile AND m.kind = i.kind \
                 ORDER BY m.id LIMIT 1",
                params![id, new_prompt],
                |r| r.get(0),
            )
            .ok();
        if let Some(existing) = existing {
            chats.insert(id, existing);
            report.chats.kept += 1;
            continue;
        }
        let parent = parent.and_then(|p| chats.get(&p).copied());
        conn.execute(
            "INSERT INTO main.sessions (kind, title, model, base_url, router_options, system_prompt_id, cwd, harness_version, \
                                        created_at, updated_at, profile, parent_id, tools, headless) \
             SELECT kind, title, model, base_url, router_options, ?2, cwd, harness_version, \
                    created_at, updated_at, profile, ?3, tools, headless FROM imp.sessions WHERE id = ?1",
            params![id, new_prompt, parent],
        )
        .map_err(sql_err)?;
        let new = conn.last_insert_rowid();
        chats.insert(id, new);
        report.chats.added += 1;
        report.messages += conn
            .execute(
                "INSERT INTO main.messages (session_id, seq, turn, role, json, text, status, created_at) \
                 SELECT ?2, seq, turn, role, json, text, status, created_at FROM imp.messages WHERE session_id = ?1 ORDER BY seq",
                params![id, new],
            )
            .map_err(sql_err)?;
        conn.execute(
            "INSERT INTO main.calls (session_id, turn, message_count, request_sha256, request_bytes, http_status, response, \
                                     response_model, finish_reason, prompt_tokens, completion_tokens, cached_tokens, \
                                     cache_write_tokens, elapsed_ms, served_by_provider, served_by_account, error, created_at, \
                                     model, base_url) \
             SELECT ?2, turn, message_count, request_sha256, request_bytes, http_status, response, \
                    response_model, finish_reason, prompt_tokens, completion_tokens, cached_tokens, \
                    cache_write_tokens, elapsed_ms, served_by_provider, served_by_account, error, created_at, \
                    model, base_url FROM imp.calls WHERE session_id = ?1 ORDER BY id",
            params![id, new],
        )
        .map_err(sql_err)?;
        for (prompt, state) in read(
            conn,
            &format!("SELECT prompt_id, state FROM imp.chat_prompt_state WHERE session_id = {id}"),
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )? {
            if let Some(mapped) = prompts.get(&prompt) {
                conn.execute(
                    "INSERT OR IGNORE INTO main.chat_prompt_state (session_id, prompt_id, state) VALUES (?1, ?2, ?3)",
                    params![new, mapped, state],
                )
                .map_err(sql_err)?;
            }
        }
        for server in read(
            conn,
            &format!("SELECT server_id FROM imp.chat_mcp WHERE session_id = {id}"),
            |r| r.get::<_, i64>(0),
        )? {
            if let Some(mapped) = servers.get(&server) {
                conn.execute(
                    "INSERT OR IGNORE INTO main.chat_mcp (session_id, server_id) VALUES (?1, ?2)",
                    params![new, mapped],
                )
                .map_err(sql_err)?;
            }
        }
    }

    // Work logs: the chat they name is the new one, or none.
    for (id, session) in read(
        conn,
        "SELECT id, session FROM imp.work_logs ORDER BY id",
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)),
    )? {
        let session = session.and_then(|s| chats.get(&s).copied());
        let added = conn
            .execute(
                "INSERT INTO main.work_logs (project, tag, title, summary, source, session, created_at) \
                 SELECT project, tag, title, summary, source, ?2, created_at FROM imp.work_logs i WHERE id = ?1 \
                 AND NOT EXISTS (SELECT 1 FROM main.work_logs m WHERE m.project = i.project AND m.tag = i.tag \
                                 AND m.title = i.title AND m.created_at = i.created_at)",
                params![id, session],
            )
            .map_err(sql_err)?;
        if added == 1 {
            report.work_logs.added += 1;
        } else {
            report.work_logs.kept += 1;
        }
    }

    // The settings and the inbox are the open database's own, except right after a wipe.
    if how == Fill::Restore {
        conn.execute(
            "INSERT OR REPLACE INTO main.settings (key, value, updated_at) SELECT key, value, updated_at FROM imp.settings",
            [],
        )
        .map_err(sql_err)?;
    } else {
        let left = count(
            conn,
            "SELECT (SELECT COUNT(*) FROM imp.settings) + (SELECT COUNT(*) FROM imp.buffer_entries)",
        );
        report.not_merged = usize::try_from(left).unwrap_or(0);
    }
    Ok(report)
}

// ---- wipe ----

/// What a wipe found to remove, counted before it did.
#[derive(Debug, PartialEq, Eq)]
pub struct WipePlan {
    pub chats: i64,
    pub messages: i64,
    pub providers: i64,
    pub keys: i64,
    pub library: i64,
    pub glossary_and_context: i64,
}

impl WipePlan {
    pub fn lines(&self) -> Vec<String> {
        vec![
            format!("  {} chats ({} messages)", self.chats, self.messages),
            format!(
                "  {} providers, {} of them with a saved API key",
                self.providers, self.keys
            ),
            format!("  {} prompts, skills and personalities", self.library),
            format!(
                "  {} glossary terms, context entries and gotchas",
                self.glossary_and_context
            ),
        ]
    }
}

/// What the database at `db` holds that a wipe would remove.
pub fn wipe_plan(db: &Path) -> Result<WipePlan, String> {
    let store = Store::open(db).map_err(|e| format!("cannot open {}: {e}", db.display()))?;
    Ok(plan_of(&store.conn))
}

/// [`wipe_plan`] for a connection that is already open (the web page reads with its own).
pub fn plan_of(c: &Connection) -> WipePlan {
    WipePlan {
        chats: count(c, "SELECT COUNT(*) FROM sessions"),
        messages: count(c, "SELECT COUNT(*) FROM messages"),
        providers: count(c, "SELECT COUNT(*) FROM providers"),
        keys: count(
            c,
            "SELECT COUNT(*) FROM providers WHERE api_key IS NOT NULL AND api_key <> ''",
        ),
        library: count(c, "SELECT COUNT(*) FROM prompts"),
        glossary_and_context: count(
            c,
            "SELECT (SELECT COUNT(*) FROM glossary) + (SELECT COUNT(*) FROM context_entries) + (SELECT COUNT(*) FROM gotchas)",
        ),
    }
}

/// Saves a copy of the database at `db` (the copy is kept, with its path returned) and then
/// empties it: every table is cleared and the defaults a new database starts with are put back.
/// The file stays where it is, so a program that has it open is not left writing to a deleted
/// file; its next save simply finds its chat gone. Refused while a writer holds the database.
pub fn wipe(db: &Path) -> Result<PathBuf, String> {
    let store = Store::open(db).map_err(|e| format!("cannot open {}: {e}", db.display()))?;
    let backup = back_up(&store.conn, db, "before-wipe")?;
    // A fresh database made by this program: the defaults to put back.
    let tmp = tmp_dir("wipe")?;
    let result = (|| {
        let fresh = tmp.join("fresh.db");
        Store::open(&fresh).map_err(|e| format!("cannot make a fresh database: {e}"))?;
        let conn = &store.conn;
        conn.execute_batch("PRAGMA foreign_keys = OFF")
            .map_err(sql_err)?;
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| format!("the database is in use, so it was not wiped: {e}"))?;
        let tables: Vec<String> = read(
            conn,
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            |r| r.get::<_, String>(0),
        )?;
        let cleared = tables
            .iter()
            .try_for_each(|t| {
                conn.execute(&format!("DELETE FROM \"{t}\""), [])
                    .map(|_| ())
            })
            .map_err(sql_err);
        if let Err(why) = cleared {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(why);
        }
        conn.execute_batch("COMMIT").map_err(sql_err)?;
        conn.execute_batch("PRAGMA foreign_keys = ON")
            .map_err(sql_err)?;
        copy_in(conn, &fresh, Fill::Restore)?;
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;");
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    result.map(|()| backup)
}

/// Deletes the turn logs (`*.jsonl`) in `dir` and says how many. They hold every request exactly
/// as sent, so a wipe that should leave nothing behind has to remove them too.
pub fn remove_turn_logs(dir: &Path) -> usize {
    let mut removed = 0;
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "jsonl") && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// How many turn logs `dir` holds.
pub fn count_turn_logs(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::Scratch;

    fn open(path: &Path) -> Store {
        Store::open(path).unwrap()
    }

    fn n(conn: &Connection, sql: &str) -> i64 {
        count(conn, sql)
    }

    /// A chat with two messages and one call, made by plain SQL. `parent` makes it a subagent's.
    fn put_chat(conn: &Connection, created: i64, prompt: &str, parent: Option<i64>) -> i64 {
        conn.execute(
            "INSERT OR IGNORE INTO system_prompts (sha256, text, created_at) VALUES (?1, ?2, 1)",
            params![format!("sha-{prompt}"), prompt],
        )
        .unwrap();
        let pid: i64 = conn
            .query_row(
                "SELECT id FROM system_prompts WHERE sha256 = ?1",
                [format!("sha-{prompt}")],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO sessions (kind, model, base_url, router_options, system_prompt_id, cwd, harness_version, created_at, updated_at, profile, parent_id) \
             VALUES ('worker', 'm', 'http://x/v1', '{}', ?1, '/w', '0', ?2, ?2, 'default', ?3)",
            params![pid, created, parent],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        for (seq, role) in [(0, "user"), (1, "assistant")] {
            conn.execute(
                "INSERT INTO messages (session_id, seq, turn, role, json, text, created_at) VALUES (?1, ?2, 1, ?3, '{}', 'hi', ?4)",
                params![id, seq, role, created],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO calls (session_id, turn, message_count, request_sha256, request_bytes, elapsed_ms, created_at) \
             VALUES (?1, 1, 1, 'h', 1, 1, ?2)",
            params![id, created],
        )
        .unwrap();
        id
    }

    fn put_provider(conn: &Connection, name: &str, key: Option<&str>) {
        conn.execute(
            "INSERT INTO providers (name, base_url, model, api_key, created_at) VALUES (?1, 'http://x/v1', 'm', ?2, 1)",
            params![name, key],
        )
        .unwrap();
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn an_export_is_private_leaves_the_keys_out_and_changes_nothing() {
        let dir = Scratch::new("tr-export");
        let db = dir.path().join("harness.db");
        {
            let store = open(&db);
            put_provider(&store.conn, "or", Some("sk-secret"));
            store.conn
                .execute("INSERT INTO mcp_servers (name, command, env, created_at, updated_at) VALUES ('s', 'c', '{\"TOKEN\":\"t\"}', 1, 1)", [])
                .unwrap();
            put_chat(&store.conn, 100, "p", None);
        }
        let out = dir.path().join("out").join("backup.db");
        let report = export(&db, &out, false).unwrap();
        assert_eq!(
            report,
            ExportReport {
                chats: 1,
                providers: 1,
                keys_removed: true
            }
        );
        assert_eq!(mode(&out), 0o600);
        let copy = Connection::open(&out).unwrap();
        assert_eq!(
            n(
                &copy,
                "SELECT COUNT(*) FROM providers WHERE api_key IS NULL"
            ),
            1
        );
        assert_eq!(
            n(&copy, "SELECT COUNT(*) FROM mcp_servers WHERE env = '{}'"),
            1
        );
        assert_eq!(n(&copy, "SELECT COUNT(*) FROM messages"), 2);
        // The database itself still has its key.
        let live = open(&db);
        assert_eq!(
            n(
                &live.conn,
                "SELECT COUNT(*) FROM providers WHERE api_key = 'sk-secret'"
            ),
            1
        );
        // Nothing is replaced.
        assert!(
            export(&db, &out, false)
                .unwrap_err()
                .contains("already exists")
        );
        // With the keys asked for, they are in the copy.
        let with = dir.path().join("with.db");
        assert!(!export(&db, &with, true).unwrap().keys_removed);
        let copy = Connection::open(&with).unwrap();
        assert_eq!(
            n(
                &copy,
                "SELECT COUNT(*) FROM providers WHERE api_key = 'sk-secret'"
            ),
            1
        );
    }

    #[test]
    fn a_space_is_a_private_upgraded_copy_that_is_never_replaced() {
        let dir = Scratch::new("tr-space");
        let src = dir.path().join("src.db");
        put_chat(&open(&src).conn, 100, "p", None);
        let data = dir.path().join("data");
        let path = import_as_space(&src, &data, "work").unwrap();
        assert_eq!(path, space_path(&data, "work"));
        assert_eq!(mode(&path), 0o600);
        assert_eq!(n(&open(&path).conn, "SELECT COUNT(*) FROM sessions"), 1);
        assert_eq!(list_spaces(&data), ["work"]);
        // The source is untouched, and a name is not used twice.
        assert_eq!(n(&open(&src).conn, "SELECT COUNT(*) FROM sessions"), 1);
        assert!(
            import_as_space(&src, &data, "work")
                .unwrap_err()
                .contains("already exists")
        );
        // Names: the main database's is taken, and odd characters are refused.
        assert!(check_space_name("harness").is_err());
        assert!(check_space_name("a/b").is_err());
        assert!(check_space_name("").is_err());
        assert!(check_space_name("my-space_2").is_ok());
    }

    #[test]
    fn a_file_that_is_not_a_database_or_is_newer_makes_no_space() {
        let dir = Scratch::new("tr-space-bad");
        let data = dir.path().join("data");
        let junk = dir.path().join("junk.db");
        std::fs::write(
            &junk,
            "this is not sqlite at all, just some text to fill a page",
        )
        .unwrap();
        let why = import_as_space(&junk, &data, "junk").unwrap_err();
        assert!(why.contains("not a usable database"), "{why}");
        assert!(!space_path(&data, "junk").exists());

        let newer = dir.path().join("newer.db");
        drop(open(&newer));
        Connection::open(&newer)
            .unwrap()
            .execute_batch("PRAGMA user_version = 999")
            .unwrap();
        let why = import_as_space(&newer, &data, "newer").unwrap_err();
        assert!(why.contains("newer than this program"), "{why}");
        assert!(!space_path(&data, "newer").exists());
        assert!(list_spaces(&data).is_empty());
    }

    /// A target and a source that share some names and chats and differ in others.
    fn two_databases(dir: &Scratch) -> (PathBuf, PathBuf) {
        let (target, source) = (dir.path().join("target.db"), dir.path().join("source.db"));
        {
            let t = open(&target);
            put_provider(&t.conn, "shared", Some("keep-mine"));
            put_chat(&t.conn, 50, "mine", None);
            put_chat(&t.conn, 100, "same", None); // the chat both have
            t.conn
                .execute("INSERT INTO glossary (term, definition, created_at, updated_at) VALUES ('ledger', 'mine', 1, 1)", [])
                .unwrap();
            t.conn
                .execute(
                    "INSERT INTO settings (key, value, updated_at) VALUES ('mode', 'target', 1)",
                    [],
                )
                .unwrap();
        }
        {
            let s = open(&source);
            put_provider(&s.conn, "shared", Some("theirs"));
            put_provider(&s.conn, "extra", Some("k2"));
            let same = put_chat(&s.conn, 100, "same", None);
            let parent = put_chat(&s.conn, 200, "theirs", None);
            put_chat(&s.conn, 210, "child", Some(parent));
            let _ = same;
            s.conn.execute("INSERT INTO prompts (name, text, enabled, position, created_at, updated_at, type) VALUES ('rules', 'be brief', 1, 99, 1, 1, 'skill')", []).unwrap();
            let skill: i64 = s
                .conn
                .query_row("SELECT id FROM prompts WHERE name = 'rules'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            s.conn.execute("INSERT INTO profiles (name, personality_id, created_at, updated_at) VALUES ('mine2', ?1, 1, 1)", [skill]).unwrap();
            let profile = s.conn.last_insert_rowid();
            s.conn
                .execute(
                    "INSERT INTO profile_skills (profile_id, prompt_id) VALUES (?1, ?2)",
                    params![profile, skill],
                )
                .unwrap();
            s.conn.execute("INSERT INTO chat_prompt_state (session_id, prompt_id, state) VALUES (?1, ?2, 'on')", params![parent, skill]).unwrap();
            s.conn.execute("INSERT INTO glossary (term, definition, created_at, updated_at) VALUES ('ledger', 'theirs', 1, 1), ('node', 'a thing', 1, 1)", []).unwrap();
            s.conn.execute("INSERT INTO context_entries (name, kind, text, created_at, updated_at) VALUES ('style', 'rule', 'terse', 1, 1)", []).unwrap();
            let entry = s.conn.last_insert_rowid();
            s.conn.execute("INSERT INTO context_quotes (entry_id, quote, said_on, created_at) VALUES (?1, 'q', 'today', 1)", [entry]).unwrap();
            s.conn.execute("INSERT INTO work_logs (project, tag, title, summary, session, created_at) VALUES ('/p', 'NOTE', 't', 's', ?1, 5)", [parent]).unwrap();
            s.conn
                .execute(
                    "INSERT INTO settings (key, value, updated_at) VALUES ('mode', 'source', 1)",
                    [],
                )
                .unwrap();
        }
        (target, source)
    }

    #[test]
    fn a_merge_adds_what_is_missing_keeps_what_is_here_and_follows_the_ids() {
        let dir = Scratch::new("tr-merge");
        let (target, source) = two_databases(&dir);
        let report = merge(&target, &source).unwrap();
        assert_eq!(report.chats, Tally { added: 2, kept: 1 });
        assert_eq!(report.messages, 4);
        assert_eq!(report.providers, Tally { added: 1, kept: 1 });
        assert_eq!(report.glossary, Tally { added: 1, kept: 1 });
        assert_eq!(report.context, Tally { added: 1, kept: 0 });
        assert_eq!(report.profiles.added, 1);
        assert!(report.backup.as_ref().unwrap().is_file());
        assert!(report.not_merged >= 1);

        let t = open(&target);
        let c = &t.conn;
        // What was here wins.
        assert_eq!(
            n(
                c,
                "SELECT COUNT(*) FROM providers WHERE name = 'shared' AND api_key = 'keep-mine'"
            ),
            1
        );
        assert_eq!(
            n(
                c,
                "SELECT COUNT(*) FROM glossary WHERE term = 'ledger' AND definition = 'mine'"
            ),
            1
        );
        assert_eq!(
            n(
                c,
                "SELECT COUNT(*) FROM settings WHERE key = 'mode' AND value = 'target'"
            ),
            1
        );
        // The chats arrived whole, once, and the subagent still points at its parent.
        assert_eq!(n(c, "SELECT COUNT(*) FROM sessions"), 4);
        assert_eq!(n(c, "SELECT COUNT(*) FROM messages"), 8);
        assert_eq!(n(c, "SELECT COUNT(*) FROM calls"), 4);
        assert_eq!(
            n(
                c,
                "SELECT COUNT(*) FROM sessions c JOIN sessions p ON p.id = c.parent_id WHERE c.created_at = 210 AND p.created_at = 200"
            ),
            1
        );
        // The skill arrives switched off, the profile uses the new ids, the chat's state too.
        assert_eq!(
            n(
                c,
                "SELECT COUNT(*) FROM prompts WHERE name = 'rules' AND enabled = 0"
            ),
            1
        );
        assert_eq!(
            n(
                c,
                "SELECT COUNT(*) FROM profiles p JOIN prompts q ON q.id = p.personality_id WHERE p.name = 'mine2' AND q.name = 'rules'"
            ),
            1
        );
        assert_eq!(
            n(
                c,
                "SELECT COUNT(*) FROM profile_skills s JOIN prompts q ON q.id = s.prompt_id WHERE q.name = 'rules'"
            ),
            1
        );
        assert_eq!(
            n(
                c,
                "SELECT COUNT(*) FROM chat_prompt_state s JOIN sessions x ON x.id = s.session_id JOIN prompts q ON q.id = s.prompt_id WHERE x.created_at = 200 AND q.name = 'rules'"
            ),
            1
        );
        assert_eq!(
            n(
                c,
                "SELECT COUNT(*) FROM context_quotes q JOIN context_entries e ON e.id = q.entry_id WHERE e.name = 'style'"
            ),
            1
        );
        assert_eq!(
            n(
                c,
                "SELECT COUNT(*) FROM work_logs w JOIN sessions s ON s.id = w.session WHERE s.created_at = 200"
            ),
            1
        );
        assert_eq!(n(c, "SELECT COUNT(*) FROM pragma_foreign_key_check"), 0);
        // The file merged in is not changed.
        assert_eq!(n(&open(&source).conn, "SELECT COUNT(*) FROM sessions"), 3);
    }

    #[test]
    fn merging_the_same_file_twice_adds_nothing_the_second_time() {
        let dir = Scratch::new("tr-merge-twice");
        let (target, source) = two_databases(&dir);
        merge(&target, &source).unwrap();
        let again = merge(&target, &source).unwrap();
        assert_eq!(again.chats, Tally { added: 0, kept: 3 });
        assert_eq!(again.messages, 0);
        assert_eq!(
            again.providers.added
                + again.glossary.added
                + again.context.added
                + again.work_logs.added,
            0
        );
        assert_eq!(n(&open(&target).conn, "SELECT COUNT(*) FROM sessions"), 4);
    }

    #[test]
    fn a_merge_that_cannot_resolve_is_refused_and_a_bad_file_changes_nothing() {
        let dir = Scratch::new("tr-merge-bad");
        let (target, _) = two_databases(&dir);
        let junk = dir.path().join("junk.db");
        std::fs::write(
            &junk,
            "not a database, only text that is long enough to be a page",
        )
        .unwrap();
        let before = n(&open(&target).conn, "SELECT COUNT(*) FROM sessions");
        assert!(
            merge(&target, &junk)
                .unwrap_err()
                .contains("not a usable database")
        );
        assert!(merge(&target, &dir.path().join("missing.db")).is_err());
        assert_eq!(
            n(&open(&target).conn, "SELECT COUNT(*) FROM sessions"),
            before
        );
        // No backup was made for a merge that never started.
        let backups = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("before-merge"))
            .count();
        assert_eq!(backups, 0);
    }

    #[test]
    fn a_wipe_saves_a_copy_and_leaves_a_new_database_that_a_waiting_connection_can_use() {
        let dir = Scratch::new("tr-wipe");
        let (db, _) = two_databases(&dir);
        let defaults = n(
            &Connection::open(&{
                let fresh = dir.path().join("fresh.db");
                drop(open(&fresh));
                fresh
            })
            .unwrap(),
            "SELECT COUNT(*) FROM prompts",
        );
        // Another connection that stays open, as a chat in another terminal would.
        let other = open(&db);
        let plan = wipe_plan(&db).unwrap();
        assert_eq!((plan.chats, plan.providers, plan.keys), (2, 1, 1));

        let backup = wipe(&db).unwrap();
        assert_eq!(mode(&backup), 0o600);
        let saved = Connection::open(&backup).unwrap();
        assert_eq!(n(&saved, "SELECT COUNT(*) FROM sessions"), 2);
        assert_eq!(
            n(
                &saved,
                "SELECT COUNT(*) FROM providers WHERE api_key = 'keep-mine'"
            ),
            1
        );

        for table in [
            "sessions",
            "messages",
            "calls",
            "providers",
            "glossary",
            "context_entries",
        ] {
            assert_eq!(
                n(&other.conn, &format!("SELECT COUNT(*) FROM {table}")),
                0,
                "{table}"
            );
        }
        assert_eq!(n(&other.conn, "SELECT COUNT(*) FROM prompts"), defaults);
        assert_eq!(
            n(
                &other.conn,
                "SELECT COUNT(*) FROM settings WHERE key = 'mode'"
            ),
            0
        );
        assert_eq!(
            n(&other.conn, "PRAGMA user_version"),
            crate::store::SCHEMA_VERSION
        );
        // The open connection works on the new database.
        put_chat(&other.conn, 1, "after", None);
        assert_eq!(n(&other.conn, "SELECT COUNT(*) FROM sessions"), 1);
        // A second wipe does not replace the first copy.
        let second = wipe(&db).unwrap();
        assert_ne!(second, backup);
        assert!(backup.is_file());
    }

    #[test]
    fn turn_logs_are_counted_and_removed_only_when_asked() {
        let dir = Scratch::new("tr-logs");
        for name in ["a.jsonl", "b.jsonl", "keep.txt"] {
            std::fs::write(dir.path().join(name), "{}").unwrap();
        }
        assert_eq!(count_turn_logs(dir.path()), 2);
        assert_eq!(remove_turn_logs(dir.path()), 2);
        assert!(dir.path().join("keep.txt").exists());
        assert_eq!(count_turn_logs(dir.path()), 0);
    }
}

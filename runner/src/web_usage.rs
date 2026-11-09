//! The web pages about token use (issue 32): totals, by day, by model, by chat, and the calls of
//! one chat.
//!
//! Read from the `calls` and `sessions` tables only, and only numbers, times, model and provider
//! names, the finish reason and the short text of a failed call: never a message, a request or
//! response body, a system prompt or a key. Times are UTC, like the rest of the program.

use rusqlite::{Connection, OptionalExtension};

use crate::store::StoreError;
use crate::turn_log::utc_label;
use crate::web_pages::escape;

/// How many days, models and chats a page lists.
const DAYS: usize = 30;
const MODELS: usize = 30;
const CHATS: usize = 50;
/// The most characters of a failed call's text that are shown.
const ERROR_CHARS: usize = 160;

/// Token counts of some calls. A call that did not report a number counts as zero.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Totals {
    pub calls: i64,
    pub failed: i64,
    pub prompt: i64,
    pub cached: i64,
    pub completion: i64,
}

impl Totals {
    /// The share of prompt tokens the router had cached, in percent, if there were any.
    pub fn cache_percent(&self) -> Option<i64> {
        (self.prompt > 0).then(|| self.cached * 100 / self.prompt)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub label: String,
    pub totals: Totals,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRow {
    pub id: i64,
    pub updated_at: i64,
    pub profile: Option<String>,
    pub model: String,
    pub messages: i64,
    /// The calls of the chat and of the subagents it started (issue 33).
    pub totals: Totals,
    /// The chat that started this one, when it is a subagent's.
    pub parent: Option<i64>,
    /// The subagents this chat started; filled for one chat's page only.
    pub subagents: Vec<SubRun>,
}

/// One subagent run: the chat it had, the profile it ran under and what it cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubRun {
    pub id: i64,
    pub profile: Option<String>,
    pub totals: Totals,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallRow {
    pub turn: i64,
    pub at: i64,
    pub status: Option<i64>,
    pub prompt: i64,
    pub cached: i64,
    pub completion: i64,
    pub elapsed_ms: i64,
    pub finish: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub total: Totals,
    pub by_day: Vec<Group>,
    pub by_model: Vec<Group>,
    pub chats: Vec<ChatRow>,
}

/// The sums every query over `calls` makes, as SQL.
const SUMS: &str = "COUNT(*), \
    COALESCE(SUM(CASE WHEN c.error IS NOT NULL OR c.http_status >= 400 THEN 1 ELSE 0 END), 0), \
    COALESCE(SUM(c.prompt_tokens), 0), COALESCE(SUM(c.cached_tokens), 0), COALESCE(SUM(c.completion_tokens), 0)";

fn totals_at(row: &rusqlite::Row<'_>, from: usize) -> rusqlite::Result<Totals> {
    Ok(Totals {
        calls: row.get(from)?,
        failed: row.get(from + 1)?,
        prompt: row.get(from + 2)?,
        cached: row.get(from + 3)?,
        completion: row.get(from + 4)?,
    })
}

fn groups(
    conn: &Connection,
    key: &str,
    order: &str,
    limit: usize,
) -> Result<Vec<Group>, StoreError> {
    let mut statement = conn.prepare(&format!(
        "SELECT {key}, {SUMS} FROM calls c GROUP BY 1 ORDER BY {order} LIMIT {limit}"
    ))?;
    let rows = statement.query_map([], |row| {
        Ok(Group {
            label: row.get(0)?,
            totals: totals_at(row, 1)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Totals of the calls made on one UTC day (`YYYY-MM-DD`).
pub fn on_day(conn: &Connection, day: &str) -> Result<Totals, StoreError> {
    Ok(conn.query_row(
        &format!(
            "SELECT {SUMS} FROM calls c WHERE strftime('%Y-%m-%d', c.created_at, 'unixepoch') = ?1"
        ),
        [day],
        |row| totals_at(row, 0),
    )?)
}

/// Today's date in UTC, from the database's own clock.
pub fn today(conn: &Connection) -> Result<String, StoreError> {
    Ok(conn.query_row("SELECT strftime('%Y-%m-%d', 'now')", [], |row| row.get(0))?)
}

pub fn load(conn: &Connection) -> Result<Usage, StoreError> {
    let total = conn.query_row(&format!("SELECT {SUMS} FROM calls c"), [], |row| {
        totals_at(row, 0)
    })?;
    let by_day = groups(
        conn,
        "strftime('%Y-%m-%d', c.created_at, 'unixepoch')",
        "1 DESC",
        DAYS,
    )?;
    let by_model = groups(
        conn,
        "COALESCE(c.model, '(not recorded)')",
        "2 DESC, 1",
        MODELS,
    )?;
    let chats = chats(conn)?;
    Ok(Usage {
        total,
        by_day,
        by_model,
        chats,
    })
}

/// The chats of the list: the ones with a life of their own, not a subagent's.
fn chats(conn: &Connection) -> Result<Vec<ChatRow>, StoreError> {
    // The sums are fetched per chat below (a subquery cannot return several columns); the list is short.
    let mut ids = conn.prepare(&format!(
        "SELECT s.id FROM sessions s WHERE s.parent_id IS NULL ORDER BY s.id DESC LIMIT {CHATS}"
    ))?;
    let ids: Vec<i64> = ids
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.extend(chat_row(conn, id)?);
    }
    Ok(out)
}

/// One chat (a subagent's too) with the totals of its own calls and its subagents'.
fn chat_row(conn: &Connection, id: i64) -> Result<Option<ChatRow>, StoreError> {
    let head = conn
        .query_row(
            "SELECT s.updated_at, s.profile, s.model, s.parent_id, \
                    (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id AND m.status = 'ok') \
             FROM sessions s WHERE s.id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((updated_at, profile, model, parent, messages)) = head else {
        return Ok(None);
    };
    let totals = conn.query_row(
        &format!(
            "SELECT {SUMS} FROM calls c WHERE c.session_id = ?1 \
             OR c.session_id IN (SELECT id FROM sessions WHERE parent_id = ?1)"
        ),
        [id],
        |row| totals_at(row, 0),
    )?;
    Ok(Some(ChatRow {
        id,
        updated_at,
        profile,
        model,
        messages,
        totals,
        parent,
        subagents: Vec::new(),
    }))
}

/// One chat's row, with the subagent runs it started, if the chat exists.
pub fn chat(conn: &Connection, id: i64) -> Result<Option<ChatRow>, StoreError> {
    let Some(mut row) = chat_row(conn, id)? else {
        return Ok(None);
    };
    let mut statement =
        conn.prepare("SELECT id, profile FROM sessions WHERE parent_id = ?1 ORDER BY id")?;
    let runs: Vec<(i64, Option<String>)> = statement
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (run, profile) in runs {
        let totals = conn.query_row(
            &format!("SELECT {SUMS} FROM calls c WHERE c.session_id = ?1"),
            [run],
            |r| totals_at(r, 0),
        )?;
        row.subagents.push(SubRun {
            id: run,
            profile,
            totals,
        });
    }
    Ok(Some(row))
}

/// The calls of one chat, oldest first. The text of a failed call is cut to a short line.
pub fn calls_of(conn: &Connection, id: i64) -> Result<Vec<CallRow>, StoreError> {
    let mut statement = conn.prepare(
        "SELECT turn, created_at, http_status, COALESCE(prompt_tokens, 0), COALESCE(cached_tokens, 0), \
                COALESCE(completion_tokens, 0), elapsed_ms, finish_reason, served_by_provider, model, error \
         FROM calls WHERE session_id = ?1 ORDER BY id",
    )?;
    let rows = statement.query_map([id], |row| {
        let error: Option<String> = row.get(10)?;
        Ok(CallRow {
            turn: row.get(0)?,
            at: row.get(1)?,
            status: row.get(2)?,
            prompt: row.get(3)?,
            cached: row.get(4)?,
            completion: row.get(5)?,
            elapsed_ms: row.get(6)?,
            finish: row.get(7)?,
            provider: row.get(8)?,
            model: row.get(9)?,
            error: error.map(|e| short(&e)),
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// One line of at most [`ERROR_CHARS`] characters.
fn short(text: &str) -> String {
    let one: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let one = one.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > ERROR_CHARS {
        let cut: String = one.chars().take(ERROR_CHARS).collect();
        format!("{cut}...")
    } else {
        one
    }
}

/// 12345 as `12.3k`, 1234567 as `1.2M`: the pages are for reading, not for adding up.
pub fn compact(n: i64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1_000_000.0),
        n if n >= 10_000 => format!("{:.1}k", n as f64 / 1000.0),
        n => n.to_string(),
    }
}

fn totals_cells(t: &Totals) -> String {
    format!(
        "<td>{}</td><td>{}</td><td title=\"{}\">{}</td><td>{}</td><td>{}</td>",
        t.calls,
        if t.failed > 0 {
            format!("<strong>{}</strong>", t.failed)
        } else {
            "0".to_owned()
        },
        t.prompt,
        compact(t.prompt),
        t.cache_percent()
            .map_or("-".to_owned(), |p| format!("{} ({p}%)", compact(t.cached))),
        compact(t.completion)
    )
}

const TOTALS_HEAD: &str =
    "<th>Calls</th><th>Failed</th><th>Prompt</th><th>Cached</th><th>Output</th>";

fn group_table(first: &str, rows: &[Group]) -> String {
    if rows.is_empty() {
        return "<p class=\"dim\">Nothing yet.</p>".to_owned();
    }
    let body: String = rows
        .iter()
        .map(|g| {
            format!(
                "<tr><td>{}</td>{}</tr>",
                escape(&g.label),
                totals_cells(&g.totals)
            )
        })
        .collect();
    format!(
        "<table><tr><th>{}</th>{TOTALS_HEAD}</tr>{body}</table>",
        escape(first)
    )
}

/// The body of the Usage page.
pub fn page_body(usage: &Usage) -> String {
    let mut out = String::from(
        "<p class=\"dim\">Counted from the calls the program made, as the router reported them (a call with no report counts as zero). \
         Times are UTC. Only numbers are read: no messages, prompts or keys.</p>",
    );
    let t = &usage.total;
    out.push_str(&format!(
        "<table><tr>{TOTALS_HEAD}</tr><tr>{}</tr></table>",
        totals_cells(t)
    ));
    out.push_str("<h2>By day</h2>");
    out.push_str(&group_table("Day", &usage.by_day));
    out.push_str("<h2>By model</h2>");
    out.push_str(&group_table("Model", &usage.by_model));
    out.push_str("<h2>Newest chats</h2><p class=\"dim\">A chat's numbers include the subagents it started; a subagent's own chat is on its parent's page.</p>");
    if usage.chats.is_empty() {
        out.push_str("<p class=\"dim\">No saved chats yet.</p>");
    } else {
        let rows: String = usage
            .chats
            .iter()
            .map(|c| {
                format!(
                    "<tr><td><a href=\"/usage/chat?id={id}\">{id}</a></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td>{}</tr>",
                    escape(&utc_label(u64::try_from(c.updated_at).unwrap_or(0))),
                    escape(c.profile.as_deref().unwrap_or("-")),
                    escape(&c.model),
                    c.messages,
                    totals_cells(&c.totals),
                    id = c.id
                )
            })
            .collect();
        out.push_str(&format!(
            "<table><tr><th>Chat</th><th>Last used</th><th>Profile</th><th>Model</th><th>Messages</th>{TOTALS_HEAD}</tr>{rows}</table>"
        ));
    }
    out
}

/// The body of one chat's page.
pub fn chat_body(chat: &ChatRow, calls: &[CallRow]) -> String {
    let mut out = format!(
        "<p><a href=\"/usage\">All usage</a></p><p class=\"dim\">Last used {}. Profile {}, model {}, {} message{}.</p>",
        escape(&utc_label(u64::try_from(chat.updated_at).unwrap_or(0))),
        escape(chat.profile.as_deref().unwrap_or("none recorded")),
        escape(&chat.model),
        chat.messages,
        if chat.messages == 1 { "" } else { "s" },
    );
    if let Some(parent) = chat.parent {
        out.push_str(&format!(
            "<p class=\"dim\">A subagent's chat, started from chat <a href=\"/usage/chat?id={parent}\">{parent}</a>.</p>"
        ));
    }
    out.push_str(&format!(
        "<table><tr>{TOTALS_HEAD}</tr><tr>{}</tr></table>",
        totals_cells(&chat.totals)
    ));
    if !chat.subagents.is_empty() {
        out.push_str(
            "<h2>Subagent runs</h2><p class=\"dim\">The totals above include these runs.</p>",
        );
        let rows: String = chat
            .subagents
            .iter()
            .map(|run| {
                format!(
                    "<tr><td><a href=\"/usage/chat?id={id}\">{id}</a></td><td>{}</td>{}</tr>",
                    escape(run.profile.as_deref().unwrap_or("-")),
                    totals_cells(&run.totals),
                    id = run.id
                )
            })
            .collect();
        out.push_str(&format!(
            "<table><tr><th>Chat</th><th>Profile</th>{TOTALS_HEAD}</tr>{rows}</table>"
        ));
    }
    if calls.is_empty() {
        out.push_str("<p class=\"dim\">This chat made no calls.</p>");
        return out;
    }
    let rows: String = calls
        .iter()
        .map(|c| {
            let status = c.status.map_or("-".to_owned(), |s| s.to_string());
            let error = c
                .error
                .as_deref()
                .map(|e| format!("<div class=\"error\">{}</div>", escape(e)))
                .unwrap_or_default();
            format!(
                "<tr><td>{}</td><td>{}</td><td>{status}</td><td>{}</td><td>{}</td><td>{}</td><td>{:.1} s</td><td>{}{}{error}</td></tr>",
                c.turn,
                escape(&utc_label(u64::try_from(c.at).unwrap_or(0))),
                compact(c.prompt),
                compact(c.cached),
                compact(c.completion),
                c.elapsed_ms as f64 / 1000.0,
                escape(c.finish.as_deref().unwrap_or("-")),
                match (&c.provider, &c.model) {
                    (Some(p), Some(m)) => format!(" <span class=\"dim\">{} via {}</span>", escape(m), escape(p)),
                    (None, Some(m)) => format!(" <span class=\"dim\">{}</span>", escape(m)),
                    _ => String::new(),
                },
            )
        })
        .collect();
    out.push_str(&format!(
        "<table><tr><th>Turn</th><th>Time</th><th>HTTP</th><th>Prompt</th><th>Cached</th><th>Output</th><th>Took</th><th>Finish</th></tr>{rows}</table>"
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::tools::testutil::Scratch;

    fn db(name: &str) -> (Scratch, Store) {
        let dir = Scratch::new(name);
        let store = Store::open(&dir.path().join("harness.db")).unwrap();
        (dir, store)
    }

    fn session(store: &Store, model: &str, profile: Option<&str>) -> i64 {
        store
            .conn
            .execute(
                "INSERT INTO system_prompts (sha256, text, created_at) VALUES (hex(randomblob(8)), 'x', 1)",
                [],
            )
            .unwrap();
        let prompt = store.conn.last_insert_rowid();
        store
            .conn
            .execute(
                "INSERT INTO sessions (model, base_url, router_options, system_prompt_id, cwd, harness_version, created_at, updated_at, profile) \
                 VALUES (?1, 'http://x', '{}', ?2, '/', 'v', 1000, 2000, ?3)",
                rusqlite::params![model, prompt, profile],
            )
            .unwrap();
        store.conn.last_insert_rowid()
    }

    #[allow(clippy::too_many_arguments)]
    fn call(
        store: &Store,
        session: i64,
        at: i64,
        model: &str,
        prompt: Option<i64>,
        cached: Option<i64>,
        out: Option<i64>,
        status: Option<i64>,
        error: Option<&str>,
    ) {
        store
            .conn
            .execute(
                "INSERT INTO calls (session_id, turn, message_count, request_sha256, request_bytes, http_status, prompt_tokens, \
                    cached_tokens, completion_tokens, elapsed_ms, error, created_at, model) \
                 VALUES (?1, 1, 2, 'h', 10, ?2, ?3, ?4, ?5, 1500, ?6, ?7, ?8)",
                rusqlite::params![session, status, prompt, cached, out, error, at, model],
            )
            .unwrap();
    }

    #[test]
    fn totals_days_models_and_chats_add_up_and_a_missing_report_counts_as_zero() {
        let (_dir, store) = db("usage-sums");
        let a = session(&store, "m1", Some("default"));
        let b = session(&store, "m2", None);
        // 2026-01-01 00:00:00 UTC is 1767225600.
        call(
            &store,
            a,
            1_767_225_600,
            "m1",
            Some(1000),
            Some(400),
            Some(50),
            Some(200),
            None,
        );
        call(
            &store,
            a,
            1_767_225_700,
            "m1",
            Some(2000),
            Some(1500),
            Some(70),
            Some(200),
            None,
        );
        call(
            &store,
            b,
            1_767_312_000,
            "m2",
            None,
            None,
            None,
            None,
            Some("timed out"),
        );
        call(
            &store,
            b,
            1_767_312_100,
            "m2",
            Some(10),
            None,
            Some(1),
            Some(429),
            None,
        );
        let usage = load(&store.conn).unwrap();
        assert_eq!(
            usage.total,
            Totals {
                calls: 4,
                failed: 2,
                prompt: 3010,
                cached: 1900,
                completion: 121
            }
        );
        assert_eq!(usage.total.cache_percent(), Some(63));
        let days: Vec<_> = usage
            .by_day
            .iter()
            .map(|g| (g.label.as_str(), g.totals.calls))
            .collect();
        assert_eq!(days, [("2026-01-02", 2), ("2026-01-01", 2)]);
        let models: Vec<_> = usage
            .by_model
            .iter()
            .map(|g| (g.label.as_str(), g.totals.prompt))
            .collect();
        assert_eq!(models.len(), 2);
        assert!(models.contains(&("m1", 3000)) && models.contains(&("m2", 10)));
        // Newest chat first; each carries its own sums.
        assert_eq!(usage.chats.iter().map(|c| c.id).collect::<Vec<_>>(), [b, a]);
        assert_eq!(usage.chats[1].totals.prompt, 3000);
        assert_eq!(usage.chats[1].profile.as_deref(), Some("default"));
        assert_eq!(usage.chats[0].totals.failed, 2);
        assert_eq!(on_day(&store.conn, "2026-01-01").unwrap().calls, 2);
        assert_eq!(
            on_day(&store.conn, "2020-01-01").unwrap(),
            Totals::default()
        );
    }

    #[test]
    fn an_empty_database_gives_empty_pages() {
        let (_dir, store) = db("usage-empty");
        let usage = load(&store.conn).unwrap();
        assert_eq!(usage.total, Totals::default());
        let html = page_body(&usage);
        assert!(
            html.contains("No saved chats yet") && html.contains("Nothing yet"),
            "{html}"
        );
        assert!(usage.total.cache_percent().is_none());
    }

    #[test]
    fn a_chat_page_lists_its_calls_and_cuts_a_long_error() {
        let (_dir, store) = db("usage-chat");
        let a = session(&store, "m1", Some("planner"));
        let long = format!("HTTP 500: {}", "x".repeat(400));
        call(
            &store,
            a,
            1_767_225_600,
            "m1",
            Some(5000),
            Some(0),
            Some(10),
            Some(500),
            Some(&long),
        );
        let row = chat(&store.conn, a).unwrap().unwrap();
        let calls = calls_of(&store.conn, a).unwrap();
        assert_eq!(calls.len(), 1);
        let shown = calls[0].error.as_deref().unwrap();
        assert!(
            shown.chars().count() <= ERROR_CHARS + 3 && shown.ends_with("..."),
            "{shown}"
        );
        let html = chat_body(&row, &calls);
        assert!(
            html.contains("planner") && html.contains(">5000<"),
            "{html}"
        );
        assert!(html.contains("HTTP"), "{html}");
        assert!(chat(&store.conn, a + 99).unwrap().is_none());
        assert!(calls_of(&store.conn, a + 99).unwrap().is_empty());
    }

    #[test]
    fn nothing_but_numbers_and_short_names_is_read() {
        let (_dir, store) = db("usage-private");
        let a = session(&store, "m1", None);
        store
            .conn
            .execute(
                "INSERT INTO messages (session_id, seq, turn, role, json, text, status, created_at) \
                 VALUES (?1, 1, 1, 'user', '{}', 'SECRET-CHAT-TEXT', 'ok', 1)",
                [a],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO calls (session_id, turn, message_count, request_sha256, request_bytes, response, elapsed_ms, created_at) \
                 VALUES (?1, 1, 1, 'h', 1, 'SECRET-RESPONSE-BODY', 5, 1)",
                [a],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO providers (name, base_url, model, api_key, provider_order, created_at) \
                 VALUES ('p', 'http://x', 'm', 'sk-DISTINCT-KEY-12345', '[]', 1)",
                [],
            )
            .ok();
        let usage = load(&store.conn).unwrap();
        let mut html = page_body(&usage);
        html.push_str(&chat_body(
            &usage.chats[0],
            &calls_of(&store.conn, a).unwrap(),
        ));
        for secret in ["SECRET-CHAT-TEXT", "SECRET-RESPONSE-BODY", "sk-DISTINCT"] {
            assert!(!html.contains(secret), "{secret} reached the page");
        }
        assert_eq!(usage.chats[0].messages, 1, "messages are counted, not read");
    }

    #[test]
    fn stored_names_are_escaped() {
        let (_dir, store) = db("usage-escape");
        let nasty = "<script>alert(1)</script>";
        let a = session(&store, nasty, Some(nasty));
        call(
            &store,
            a,
            1_767_225_600,
            nasty,
            Some(1),
            Some(0),
            Some(1),
            Some(200),
            Some(nasty),
        );
        let usage = load(&store.conn).unwrap();
        let mut html = page_body(&usage);
        html.push_str(&chat_body(
            &usage.chats[0],
            &calls_of(&store.conn, a).unwrap(),
        ));
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn subagent_chats_are_not_listed_their_cost_is_their_parents_and_their_runs_are_shown() {
        let (_dir, store) = db("usage-subagents");
        let parent = session(&store, "m1", Some("default"));
        let child = session(&store, "m1", Some("planner"));
        store
            .conn
            .execute(
                "UPDATE sessions SET parent_id = ?1 WHERE id = ?2",
                [parent, child],
            )
            .unwrap();
        call(
            &store,
            parent,
            1_767_225_600,
            "m1",
            Some(1000),
            Some(0),
            Some(10),
            Some(200),
            None,
        );
        call(
            &store,
            child,
            1_767_225_700,
            "m1",
            Some(5000),
            Some(0),
            Some(40),
            Some(200),
            None,
        );
        call(
            &store,
            child,
            1_767_225_800,
            "m1",
            Some(6000),
            Some(0),
            Some(20),
            Some(200),
            None,
        );

        let usage = load(&store.conn).unwrap();
        // Everything is counted once in the totals; only the parent is listed, with the run in it.
        assert_eq!(usage.total.calls, 3);
        assert_eq!(usage.chats.len(), 1);
        assert_eq!(usage.chats[0].id, parent);
        assert_eq!(usage.chats[0].totals.calls, 3);
        assert_eq!(usage.chats[0].totals.prompt, 12_000);
        let html = page_body(&usage);
        assert!(!html.contains(&format!("chat?id={child}\"")), "{html}");

        // The parent's page lists the run; the run's own page says where it came from.
        let row = chat(&store.conn, parent).unwrap().unwrap();
        assert_eq!(row.subagents.len(), 1);
        assert_eq!(row.subagents[0].profile.as_deref(), Some("planner"));
        assert_eq!(row.subagents[0].totals.calls, 2);
        let page = chat_body(&row, &calls_of(&store.conn, parent).unwrap());
        assert!(page.contains("Subagent runs"), "{page}");
        assert!(page.contains(&format!("chat?id={child}\"")), "{page}");
        assert!(page.contains("planner"), "{page}");
        let run = chat(&store.conn, child).unwrap().unwrap();
        assert_eq!(run.parent, Some(parent));
        assert_eq!(run.totals.calls, 2);
        let page = chat_body(&run, &calls_of(&store.conn, child).unwrap());
        assert!(page.contains("A subagent's chat"), "{page}");
        assert!(!page.contains("Subagent runs"), "{page}");
    }

    #[test]
    fn numbers_are_shortened_for_reading() {
        assert_eq!(compact(999), "999");
        assert_eq!(compact(9_999), "9999");
        assert_eq!(compact(12_345), "12.3k");
        assert_eq!(compact(1_234_567), "1.2M");
    }
}

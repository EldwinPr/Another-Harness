//! The project work log (issue 50): what an agent did and why, as short entries, per project.
//!
//! Part of the store (`impl Store` continues here); the table layout is owned by `docs/erd.md`.
//! This is the database form of `pm/log.md`, not a program log: events, in the tags that file
//! uses. **A log is history, so it is only ever added**: nothing here updates or deletes a row,
//! and a correction is a new entry that says so. A project holds at most [`MAX_PER_PROJECT`] so a
//! runaway agent cannot fill the database.

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::store::{Store, StoreError, now_secs};

/// The tags of an entry, as `pm/log.md` writes them.
pub const TAGS: [&str; 5] = ["STATUS", "DECISION", "DISCOVERY", "TODO", "NOTE"];
/// The longest title, in characters.
pub const MAX_TITLE_CHARS: usize = 100;
/// The longest summary, in bytes.
pub const MAX_SUMMARY_BYTES: usize = 4_000;
/// The most entries one project's log holds.
pub const MAX_PER_PROJECT: i64 = 200;
/// The most entries a listing returns.
pub const MAX_LISTED: i64 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkLog {
    pub id: i64,
    /// The working directory it belongs to; empty means every project.
    pub project: String,
    pub tag: String,
    pub title: String,
    pub summary: String,
    /// Who wrote it, as claimed (empty when not given).
    pub source: String,
    /// The chat the summary is about, when the writer said.
    pub session: Option<i64>,
    pub created_at: i64,
    /// The day it was written, `YYYY-MM-DD` (UTC).
    pub day: String,
}

/// What [`Store::add_log`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogAdded {
    /// A new entry, with this number.
    Added(i64),
    /// The project holds [`MAX_PER_PROJECT`] entries; nothing was added.
    Full,
}

fn err(text: impl Into<String>) -> StoreError {
    StoreError::new(text)
}

/// A tag: one of [`TAGS`], in any case.
pub fn check_tag(tag: &str) -> Result<String, String> {
    let tag = tag.trim().to_uppercase();
    if TAGS.contains(&tag.as_str()) {
        Ok(tag)
    } else {
        Err(format!("a tag is one of: {}", TAGS.join(", ")))
    }
}

/// A title as it will be stored: trimmed, one line, 1 to [`MAX_TITLE_CHARS`] characters.
pub fn check_title(title: &str) -> Result<String, String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("the title is empty".to_owned());
    }
    if title.chars().any(char::is_control) {
        return Err("a title is one line of plain text".to_owned());
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(format!("a title is at most {MAX_TITLE_CHARS} characters"));
    }
    Ok(title.to_owned())
}

/// A summary as it will be stored: trimmed, 1 to [`MAX_SUMMARY_BYTES`] bytes.
pub fn check_summary(summary: &str) -> Result<String, String> {
    let summary = summary.trim();
    if summary.is_empty() {
        return Err("the summary is empty".to_owned());
    }
    if summary.len() > MAX_SUMMARY_BYTES {
        return Err(format!(
            "a summary is at most {MAX_SUMMARY_BYTES} bytes ({} given)",
            summary.len()
        ));
    }
    Ok(summary.to_owned())
}

fn read_log(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkLog> {
    Ok(WorkLog {
        id: row.get(0)?,
        project: row.get(1)?,
        tag: row.get(2)?,
        title: row.get(3)?,
        summary: row.get(4)?,
        source: row.get(5)?,
        session: row.get(6)?,
        created_at: row.get(7)?,
        day: row.get(8)?,
    })
}

const COLUMNS: &str = "id, project, tag, title, summary, source, session, created_at, \
     strftime('%Y-%m-%d', created_at, 'unixepoch')";

impl Store {
    /// Appends an entry to the log of `scope` (a project, or empty for every project). The checks
    /// run here. This is the only write to `work_logs`.
    pub fn add_log(
        &mut self,
        scope: &str,
        tag: &str,
        title: &str,
        summary: &str,
        source: &str,
        session: Option<i64>,
    ) -> Result<LogAdded, StoreError> {
        let tag = check_tag(tag).map_err(err)?;
        let title = check_title(title).map_err(err)?;
        let summary = check_summary(summary).map_err(err)?;
        if session.is_some_and(|s| s < 1) {
            return Err(err("a session is a chat's number"));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let held: i64 = tx.query_row(
            "SELECT COUNT(*) FROM work_logs WHERE project = ?1",
            [scope],
            |row| row.get(0),
        )?;
        if held >= MAX_PER_PROJECT {
            return Ok(LogAdded::Full);
        }
        tx.execute(
            "INSERT INTO work_logs (project, tag, title, summary, source, session, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![scope, tag, title, summary, source, session, now_secs()],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(LogAdded::Added(id))
    }

    /// The entries that apply in `project` (its own and the ones for every project), newest
    /// first, at most `limit` ([`MAX_LISTED`] at most). `tag` keeps only that tag.
    pub fn list_logs(
        &self,
        project: &str,
        tag: Option<&str>,
        limit: i64,
    ) -> Result<Vec<WorkLog>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM work_logs \
             WHERE (project = ?1 OR project = '') AND (?2 IS NULL OR tag = ?2) \
             ORDER BY id DESC LIMIT ?3"
        ))?;
        let rows =
            statement.query_map(params![project, tag, limit.clamp(1, MAX_LISTED)], read_log)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Entry `id`, if it applies in `project`.
    pub fn log(&self, project: &str, id: i64) -> Result<Option<WorkLog>, StoreError> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM work_logs \
                     WHERE id = ?1 AND (project = ?2 OR project = '')"
                ),
                params![id, project],
                read_log,
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::Scratch;

    fn store(tag: &str) -> (Scratch, Store) {
        let dir = Scratch::new(tag);
        let store = Store::open(&dir.path().join("harness.db")).unwrap();
        (dir, store)
    }

    fn add(s: &mut Store, scope: &str, tag: &str, title: &str) -> LogAdded {
        s.add_log(scope, tag, title, "What happened.", "claude", None)
            .unwrap()
    }

    #[test]
    fn entries_are_appended_and_listed_newest_first_with_a_day_and_a_tag() {
        let (_dir, mut s) = store("log-basic");
        assert_eq!(add(&mut s, "/p", "status", "first"), LogAdded::Added(1));
        assert_eq!(add(&mut s, "/p", "DECISION", "second"), LogAdded::Added(2));
        let listed = s.list_logs("/p", None, 20).unwrap();
        let titles: Vec<&str> = listed.iter().map(|l| l.title.as_str()).collect();
        assert_eq!(titles, ["second", "first"]);
        assert_eq!(listed[1].tag, "STATUS", "a tag is stored in capitals");
        assert_eq!(listed[0].day.len(), 10);
        let only = s.list_logs("/p", Some("STATUS"), 20).unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(s.list_logs("/p", None, 1).unwrap().len(), 1);
        assert_eq!(s.log("/p", 2).unwrap().unwrap().summary, "What happened.");
    }

    #[test]
    fn a_project_sees_its_own_log_and_the_shared_one_and_not_another_projects() {
        let (_dir, mut s) = store("log-scope");
        add(&mut s, "/p", "NOTE", "mine");
        add(&mut s, "", "NOTE", "shared");
        add(&mut s, "/other", "NOTE", "theirs");
        let titles: Vec<String> = s
            .list_logs("/p", None, 20)
            .unwrap()
            .into_iter()
            .map(|l| l.title)
            .collect();
        assert_eq!(titles, ["shared", "mine"]);
        assert!(
            s.log("/p", 3).unwrap().is_none(),
            "another project's entry is not found"
        );
    }

    #[test]
    fn the_limits_hold_at_their_edges_and_a_refusal_writes_nothing() {
        let (_dir, mut s) = store("log-limits");
        assert!(s.add_log("/p", "ERROR", "t", "s", "", None).is_err());
        assert!(s.add_log("/p", "NOTE", "  ", "s", "", None).is_err());
        assert!(
            s.add_log("/p", "NOTE", "two\nlines", "s", "", None)
                .is_err()
        );
        assert!(s.add_log("/p", "NOTE", "t", " ", "", None).is_err());
        assert!(s.add_log("/p", "NOTE", "t", "s", "", Some(0)).is_err());
        assert!(
            s.add_log(
                "/p",
                "NOTE",
                &"t".repeat(MAX_TITLE_CHARS + 1),
                "s",
                "",
                None
            )
            .is_err()
        );
        assert!(
            s.add_log("/p", "NOTE", &"t".repeat(MAX_TITLE_CHARS), "s", "", None)
                .is_ok()
        );
        assert!(
            s.add_log(
                "/p",
                "NOTE",
                "t",
                &"s".repeat(MAX_SUMMARY_BYTES + 1),
                "",
                None
            )
            .is_err()
        );
        assert!(
            s.add_log(
                "/p",
                "NOTE",
                "t",
                &"s".repeat(MAX_SUMMARY_BYTES),
                "",
                Some(3)
            )
            .is_ok()
        );
        assert_eq!(s.list_logs("/p", None, 100).unwrap().len(), 2);
    }

    #[test]
    fn a_project_holds_at_most_the_cap_and_another_project_is_not_affected() {
        let (_dir, mut s) = store("log-cap");
        for n in 0..MAX_PER_PROJECT {
            assert!(matches!(
                add(&mut s, "/p", "NOTE", &format!("n{n}")),
                LogAdded::Added(_)
            ));
        }
        assert_eq!(add(&mut s, "/p", "NOTE", "one too many"), LogAdded::Full);
        assert!(matches!(
            add(&mut s, "/q", "NOTE", "fine"),
            LogAdded::Added(_)
        ));
        let held: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM work_logs WHERE project = '/p'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(held, MAX_PER_PROJECT);
    }

    #[test]
    fn the_listing_never_returns_more_than_the_listing_cap() {
        let (_dir, mut s) = store("log-listing-cap");
        for n in 0..(MAX_LISTED + 5) {
            add(&mut s, "/p", "NOTE", &format!("n{n}"));
        }
        assert_eq!(
            s.list_logs("/p", None, 1_000).unwrap().len() as i64,
            MAX_LISTED
        );
        assert_eq!(
            s.list_logs("/p", None, 0).unwrap().len(),
            1,
            "a zero limit is one"
        );
    }
}

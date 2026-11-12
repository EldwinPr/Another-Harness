//! Gotchas (issue 50): things a session learned the hard way, kept per project.
//!
//! Part of the store (`impl Store` continues here); the table layout is owned by `docs/erd.md`.
//! A gotcha is the database form of an entry in `context/gotchas.md` that is not a shape note:
//! a convention, hazard, trap, ownership, decision, tooling or workflow note, in one text that
//! says what is true, why, and what to do differently. It belongs to a project (the working
//! directory) or to every project (`project` empty); a project's own gotcha hides a shared one of
//! the same name. Another harness proposes one through `submit`; `--direct` writes it.

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::store::{Store, StoreError, fingerprint, now_secs};
pub use crate::store_glossary::Saved;

/// The longest text, in bytes.
pub const MAX_TEXT_BYTES: usize = 2_000;
/// The types of gotcha, the routing hints `context/gotchas.md` uses (`shape` is not one: shape is
/// a kind of context).
pub const TYPES: [&str; 7] = [
    "convention",
    "hazard",
    "trap",
    "ownership",
    "decision",
    "tooling",
    "workflow",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gotcha {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub text: String,
    /// Who wrote it, as claimed (empty when not given).
    pub source: String,
    /// The working directory it belongs to; empty means every project.
    pub project: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Gotcha {
    /// A stamp of what the gotcha says, for "changed since you opened it" checks.
    pub fn fingerprint(&self) -> String {
        fingerprint(&[&self.name, &self.kind, &self.text])
    }
}

fn err(text: impl Into<String>) -> StoreError {
    StoreError::new(text)
}

/// A type: one of [`TYPES`].
pub fn check_type(kind: &str) -> Result<String, String> {
    let kind = kind.trim().to_lowercase();
    if TYPES.contains(&kind.as_str()) {
        Ok(kind)
    } else {
        Err(format!("a gotcha type is one of: {}", TYPES.join(", ")))
    }
}

/// The text as it will be stored: trimmed, 1 to [`MAX_TEXT_BYTES`] bytes.
pub fn check_gotcha_text(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("the text is empty".to_owned());
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(format!(
            "a gotcha is at most {MAX_TEXT_BYTES} bytes ({} given)",
            text.len()
        ));
    }
    Ok(text.to_owned())
}

fn read_gotcha(row: &rusqlite::Row<'_>) -> rusqlite::Result<Gotcha> {
    Ok(Gotcha {
        id: row.get(0)?,
        name: row.get(1)?,
        kind: row.get(2)?,
        text: row.get(3)?,
        source: row.get(4)?,
        project: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

const COLUMNS: &str = "id, name, type, text, source, project, created_at, updated_at";

impl Store {
    /// The gotchas that apply in `project`, by name; a project's own hides a shared one of the
    /// same name. `kind` keeps only that type.
    pub fn list_gotchas(
        &self,
        project: &str,
        kind: Option<&str>,
    ) -> Result<Vec<Gotcha>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM gotchas g \
             WHERE (project = ?1 OR project = '') \
             AND NOT (project = '' AND ?1 <> '' AND EXISTS (SELECT 1 FROM gotchas p \
                      WHERE p.project = ?1 AND p.name = g.name)) \
             AND (?2 IS NULL OR type = ?2) \
             ORDER BY name COLLATE NOCASE"
        ))?;
        let rows = statement.query_map(params![project, kind], read_gotcha)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Every gotcha of every project (a screen lists them all).
    pub fn list_all_gotchas(&self) -> Result<Vec<Gotcha>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM gotchas ORDER BY name COLLATE NOCASE, project"
        ))?;
        let rows = statement.query_map([], read_gotcha)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The gotcha called `name` that applies in `project`.
    pub fn gotcha(&self, project: &str, name: &str) -> Result<Option<Gotcha>, StoreError> {
        Ok(self
            .list_gotchas(project, None)?
            .into_iter()
            .find(|g| g.name.eq_ignore_ascii_case(name.trim())))
    }

    /// Adds a gotcha or replaces the one of `scope` with the same name (ignoring case). The
    /// checks run here, so every way in obeys the same limits.
    pub fn set_gotcha_in(
        &mut self,
        scope: &str,
        name: &str,
        kind: &str,
        text: &str,
        source: &str,
    ) -> Result<Saved, StoreError> {
        let name = crate::store_context::check_name(name).map_err(err)?;
        let kind = check_type(kind).map_err(err)?;
        let text = check_gotcha_text(text).map_err(err)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_secs();
        let there = tx
            .query_row(
                &format!("SELECT {COLUMNS} FROM gotchas WHERE project = ?1 AND name = ?2"),
                params![scope, name],
                read_gotcha,
            )
            .optional()?;
        let saved = match there {
            None => {
                tx.execute(
                    "INSERT INTO gotchas (name, type, text, source, project, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                    params![name, kind, text, source, scope, now],
                )?;
                Saved::Added
            }
            Some(old) if old.kind == kind && old.text == text => Saved::Same,
            Some(old) => {
                tx.execute(
                    "UPDATE gotchas SET type = ?2, text = ?3, source = ?4, updated_at = ?5 \
                     WHERE id = ?1",
                    params![old.id, kind, text, source, now],
                )?;
                Saved::Changed
            }
        };
        tx.commit()?;
        Ok(saved)
    }

    /// Deletes the gotcha `name` of this `scope`; false when there was none.
    pub fn delete_gotcha_in(&mut self, scope: &str, name: &str) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "DELETE FROM gotchas WHERE project = ?1 AND name = ?2",
            params![scope, name.trim()],
        )?;
        Ok(changed > 0)
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

    #[test]
    fn a_gotcha_is_added_replaced_found_ignoring_case_and_deleted() {
        let (_dir, mut s) = store("gotcha-basic");
        assert_eq!(
            s.set_gotcha_in("/p", "wal-hazard", "hazard", "Use IMMEDIATE.", "claude")
                .unwrap(),
            Saved::Added
        );
        assert_eq!(
            s.set_gotcha_in("/p", "wal-hazard", "hazard", "Use IMMEDIATE.", "claude")
                .unwrap(),
            Saved::Same
        );
        assert_eq!(
            s.set_gotcha_in(
                "/p",
                "WAL-hazard",
                "trap",
                "Use IMMEDIATE always.",
                "claude"
            )
            .unwrap(),
            Saved::Changed,
            "the name matches ignoring case"
        );
        let got = s.gotcha("/p", "Wal-Hazard").unwrap().unwrap();
        assert_eq!(
            (got.kind.as_str(), got.text.as_str()),
            ("trap", "Use IMMEDIATE always.")
        );
        assert_eq!(s.list_all_gotchas().unwrap().len(), 1);
        assert!(s.delete_gotcha_in("/p", "wal-hazard").unwrap());
        assert!(!s.delete_gotcha_in("/p", "wal-hazard").unwrap());
        assert!(s.gotcha("/p", "wal-hazard").unwrap().is_none());
    }

    #[test]
    fn a_project_sees_its_own_and_the_shared_ones_and_its_own_hides_a_shared_one() {
        let (_dir, mut s) = store("gotcha-scope");
        s.set_gotcha_in("", "shared", "tooling", "For all.", "")
            .unwrap();
        s.set_gotcha_in("", "both", "tooling", "Shared version.", "")
            .unwrap();
        s.set_gotcha_in("/p", "both", "tooling", "Project version.", "")
            .unwrap();
        s.set_gotcha_in("/other", "elsewhere", "tooling", "Not here.", "")
            .unwrap();
        let seen: Vec<String> = s
            .list_gotchas("/p", None)
            .unwrap()
            .into_iter()
            .map(|g| format!("{}={}", g.name, g.text))
            .collect();
        assert_eq!(seen, ["both=Project version.", "shared=For all."]);
        let shared: Vec<String> = s
            .list_gotchas("", None)
            .unwrap()
            .into_iter()
            .map(|g| g.name)
            .collect();
        assert_eq!(shared, ["both", "shared"]);
        let only_traps = s.list_gotchas("/p", Some("trap")).unwrap();
        assert!(only_traps.is_empty());
    }

    #[test]
    fn shape_is_not_a_gotcha_type_and_the_limits_hold_at_their_edges() {
        let (_dir, mut s) = store("gotcha-limits");
        assert!(check_type("shape").is_err());
        assert!(s.set_gotcha_in("/p", "x", "shape", "t", "").is_err());
        assert!(s.set_gotcha_in("/p", "x", "hazard", "   ", "").is_err());
        assert!(s.set_gotcha_in("/p", ".x", "hazard", "t", "").is_err());
        assert!(
            s.set_gotcha_in("/p", "has space", "hazard", "t", "")
                .is_err()
        );
        let longest = "a".repeat(MAX_TEXT_BYTES);
        assert!(s.set_gotcha_in("/p", "ok", "hazard", &longest, "").is_ok());
        let too_long = "a".repeat(MAX_TEXT_BYTES + 1);
        assert!(
            s.set_gotcha_in("/p", "long", "hazard", &too_long, "")
                .is_err()
        );
        assert_eq!(
            s.list_all_gotchas().unwrap().len(),
            1,
            "a refusal wrote nothing"
        );
    }

    #[test]
    fn a_version_27_database_upgrades_to_28_keeping_the_inbox_and_adding_the_kind() {
        let dir = Scratch::new("gotcha-migrate");
        let path = dir.path().join("harness.db");
        {
            let mut s = Store::open(&path).unwrap();
            s.set_context("/p", "style", "rule", "Plainly.", "", None)
                .unwrap();
        }
        // Put the file back as schema 27 left it: the old kind check, three inbox rows (one of
        // each status) and a suggestion that points at the first.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "PRAGMA foreign_keys = OFF;
                 DROP TABLE gotchas; DROP TABLE work_logs; DROP TABLE context_quotes;
                 DROP TABLE buffer_suggestions; DROP TABLE buffer_entries;
                 CREATE TABLE buffer_entries (
                    id INTEGER PRIMARY KEY,
                    kind TEXT NOT NULL CHECK (kind IN ('context', 'glossary', 'skill', 'personality', 'prompt')),
                    action TEXT NOT NULL CHECK (action IN ('set', 'delete')),
                    name TEXT NOT NULL, project TEXT NOT NULL DEFAULT '', entry_json TEXT NOT NULL,
                    why TEXT NOT NULL, source TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'applied', 'rejected')),
                    content_sha256 TEXT NOT NULL, created_at INTEGER NOT NULL, decided_at INTEGER,
                    decision_note TEXT NOT NULL DEFAULT ''
                 ) STRICT;
                 CREATE UNIQUE INDEX buffer_entries_pending ON buffer_entries (content_sha256) WHERE status = 'pending';
                 CREATE TABLE buffer_suggestions (
                    id INTEGER PRIMARY KEY,
                    buffer_id INTEGER NOT NULL REFERENCES buffer_entries (id),
                    decider TEXT NOT NULL,
                    verdict TEXT NOT NULL CHECK (verdict IN ('accept', 'reject', 'unsure')),
                    confidence REAL, reason TEXT NOT NULL, raw TEXT NOT NULL, created_at INTEGER NOT NULL,
                    UNIQUE (buffer_id, decider)
                 ) STRICT;
                 INSERT INTO buffer_entries (id, kind, action, name, entry_json, why, source, status, content_sha256, created_at, decided_at, decision_note) VALUES
                    (7, 'context', 'set', 'a', '{}', 'w', 's', 'pending', 'h1', 100, NULL, ''),
                    (9, 'glossary', 'set', 'b', '{}', 'w', 's', 'applied', 'h2', 101, 102, ''),
                    (12, 'skill', 'set', 'c', '{}', 'w', 's', 'rejected', 'h3', 103, 104, 'no');
                 INSERT INTO buffer_suggestions (buffer_id, decider, verdict, confidence, reason, raw, created_at)
                    VALUES (7, 'llm:x', 'accept', 0.9, 'fine', '{}', 105);
                 PRAGMA foreign_keys = ON;
                 PRAGMA user_version = 27;",
            )
            .unwrap();
        }
        let mut s = Store::open(&path).unwrap();
        let version: i64 = s
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 33);
        let rows: Vec<(i64, String, String, Option<i64>, String)> = s
            .conn
            .prepare("SELECT id, kind, status, decided_at, decision_note FROM buffer_entries ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            [
                (7, "context".into(), "pending".into(), None, String::new()),
                (
                    9,
                    "glossary".into(),
                    "applied".into(),
                    Some(102),
                    String::new()
                ),
                (
                    12,
                    "skill".into(),
                    "rejected".into(),
                    Some(104),
                    "no".into()
                ),
            ]
        );
        assert_eq!(
            s.list_suggestions(7).unwrap().len(),
            1,
            "the suggestion still points at #7"
        );
        let broken: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(broken, 0);
        // The pending index is back (an identical pending hash is refused) and the new kind works.
        assert!(
            s.conn
                .execute(
                    "INSERT INTO buffer_entries (kind, action, name, entry_json, why, source, content_sha256, created_at) \
                     VALUES ('gotcha', 'set', 'g', '{}', 'w', 's', 'h1', 1)",
                    [],
                )
                .is_err(),
            "the partial unique index over pending rows is back"
        );
        s.conn
            .execute(
                "INSERT INTO buffer_entries (kind, action, name, entry_json, why, source, content_sha256, created_at) \
                 VALUES ('gotcha', 'set', 'g', '{}', 'w', 's', 'h9', 1)",
                [],
            )
            .unwrap();
        assert!(s.context_entry("/p", "style").unwrap().is_some());
        assert!(dir.path().join("harness.db.before-33").exists());
        s.set_gotcha_in("/p", "ok", "hazard", "Works.", "").unwrap();
    }
}

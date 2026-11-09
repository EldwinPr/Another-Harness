//! Context entries (issue 37): rules, decisions and shapes the owner has said, kept per project.
//!
//! Part of the store (`impl Store` continues here); the table layout is owned by `docs/erd.md`.
//! An entry belongs to a project (the working directory it was made in) or to every project
//! (`project` empty). The model reads one through the `context` tool and can only add or change
//! one through `context_propose`, which asks the owner every time; `/context` is the owner's own
//! way in. A project entry hides an every-project entry of the same name.

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::store::{Edit, Store, StoreError, fingerprint, now_secs};
pub use crate::store_glossary::Saved;
use crate::store_glossary::check_area;

/// The longest name, in characters.
pub const MAX_NAME_CHARS: usize = 60;
/// The longest text, in bytes.
pub const MAX_TEXT_BYTES: usize = 8_000;
/// The longest quote, in characters.
pub const MAX_QUOTE_CHARS: usize = 1_000;
/// The kinds of entry; a label that says what it is, it does not change what happens.
pub const KINDS: [&str; 4] = ["rule", "decision", "shape", "guide"];
/// The most tokens (estimated) the entries put in a subagent's starting prompt.
pub const PRELOAD_TOKENS: usize = 2_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextEntry {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub text: String,
    pub quote: String,
    pub area: String,
    /// The working directory it belongs to; empty means every project.
    pub project: String,
    pub preload: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

impl ContextEntry {
    /// A stamp of what the entry says, for "changed since you opened it" checks.
    pub fn fingerprint(&self) -> String {
        fingerprint(&[
            &self.name,
            &self.kind,
            &self.text,
            &self.quote,
            &self.area,
            if self.preload { "1" } else { "0" },
        ])
    }

    /// A rough size in tokens (four bytes to a token).
    pub fn tokens(&self) -> usize {
        self.text.len().div_ceil(4)
    }
}

fn err(text: impl Into<String>) -> StoreError {
    StoreError::new(text)
}

/// A name as it will be stored: 1 to [`MAX_NAME_CHARS`] letters, digits, `.`, `-` or `_`, not
/// starting with a dot.
pub fn check_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("the name is empty".to_owned());
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(format!("a name is at most {MAX_NAME_CHARS} characters"));
    }
    if name.starts_with('.')
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return Err(
            "a name is letters, digits, '.', '-' or '_', not starting with a dot".to_owned(),
        );
    }
    Ok(name.to_owned())
}

/// A kind: one of [`KINDS`].
pub fn check_kind(kind: &str) -> Result<String, String> {
    let kind = kind.trim().to_lowercase();
    if KINDS.contains(&kind.as_str()) {
        Ok(kind)
    } else {
        Err(format!("a kind is one of: {}", KINDS.join(", ")))
    }
}

/// The text as it will be stored: trimmed, 1 to [`MAX_TEXT_BYTES`] bytes.
pub fn check_text(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("the text is empty".to_owned());
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(format!(
            "an entry is at most {MAX_TEXT_BYTES} bytes ({} given)",
            text.len()
        ));
    }
    Ok(text.to_owned())
}

/// The owner's words, trimmed, at most [`MAX_QUOTE_CHARS`] characters; may be empty.
pub fn check_quote(quote: &str) -> Result<String, String> {
    let quote = quote.trim();
    if quote.chars().count() > MAX_QUOTE_CHARS {
        return Err(format!("a quote is at most {MAX_QUOTE_CHARS} characters"));
    }
    Ok(quote.to_owned())
}

/// What the web form holds for an entry, as typed.
#[derive(Debug, Clone, Default)]
pub struct ContextDraft {
    pub name: String,
    pub kind: String,
    pub text: String,
    pub quote: String,
    pub area: String,
    pub preload: bool,
}

/// A draft after every check.
struct Checked {
    name: String,
    kind: String,
    text: String,
    quote: String,
    area: String,
    preload: bool,
}

impl ContextDraft {
    /// The form of an entry as it is now.
    pub fn of(entry: &ContextEntry) -> Self {
        Self {
            name: entry.name.clone(),
            kind: entry.kind.clone(),
            text: entry.text.clone(),
            quote: entry.quote.clone(),
            area: entry.area.clone(),
            preload: entry.preload,
        }
    }

    fn check(&self) -> Result<Checked, StoreError> {
        Ok(Checked {
            name: check_name(&self.name).map_err(err)?,
            kind: check_kind(&self.kind).map_err(err)?,
            text: check_text(&self.text).map_err(err)?,
            quote: check_quote(&self.quote).map_err(err)?,
            area: check_area(&self.area).map_err(err)?,
            preload: self.preload,
        })
    }
}

/// A scope typed on the web page: empty (every project) or a folder path on one line.
pub fn check_scope(scope: &str) -> Result<String, String> {
    let scope = scope.trim();
    if scope.chars().any(char::is_control) || scope.chars().count() > 500 {
        return Err(
            "a project is the folder's path on one line (at most 500 characters)".to_owned(),
        );
    }
    Ok(scope.to_owned())
}

fn read_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<ContextEntry> {
    Ok(ContextEntry {
        id: row.get(0)?,
        name: row.get(1)?,
        kind: row.get(2)?,
        text: row.get(3)?,
        quote: row.get(4)?,
        area: row.get(5)?,
        project: row.get(6)?,
        preload: row.get::<_, i64>(7)? == 1,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

const COLUMNS: &str = "id, name, kind, text, quote, area, project, preload, created_at, updated_at";

impl Store {
    /// The entries that apply in `project`, by name; a project entry hides an every-project one
    /// of the same name.
    pub fn list_context(&self, project: &str) -> Result<Vec<ContextEntry>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM context_entries c \
             WHERE (project = ?1 OR project = '') \
             AND NOT (project = '' AND ?1 <> '' AND EXISTS (SELECT 1 FROM context_entries p \
                      WHERE p.project = ?1 AND p.name = c.name)) \
             ORDER BY name COLLATE NOCASE"
        ))?;
        let rows = statement.query_map([project], read_entry)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The entry called `name` that applies in `project`.
    pub fn context_entry(
        &self,
        project: &str,
        name: &str,
    ) -> Result<Option<ContextEntry>, StoreError> {
        Ok(self
            .list_context(project)?
            .into_iter()
            .find(|e| e.name.eq_ignore_ascii_case(name.trim())))
    }

    /// The entries to put in a subagent's starting prompt: the ones that apply and are marked
    /// `preload`, in name order, until [`PRELOAD_TOKENS`] is used. The second list holds the
    /// names left out because they did not fit.
    pub fn preload_context(
        &self,
        project: &str,
    ) -> Result<(Vec<ContextEntry>, Vec<String>), StoreError> {
        let (mut used, mut taken, mut left) = (0usize, Vec::new(), Vec::new());
        for entry in self
            .list_context(project)?
            .into_iter()
            .filter(|e| e.preload)
        {
            if used + entry.tokens() > PRELOAD_TOKENS {
                left.push(entry.name);
            } else {
                used += entry.tokens();
                taken.push(entry);
            }
        }
        Ok((taken, left))
    }

    /// Adds an entry or replaces the one of this project with the same name. `scope` is the
    /// entry's own `project` (empty for every project). `area` `None` keeps an existing area; a new
    /// shape is preloaded, other kinds are not, and a replaced entry keeps its preload.
    pub fn set_context(
        &mut self,
        scope: &str,
        name: &str,
        kind: &str,
        text: &str,
        quote: &str,
        area: Option<&str>,
    ) -> Result<Saved, StoreError> {
        let name = check_name(name).map_err(err)?;
        let kind = check_kind(kind).map_err(err)?;
        let text = check_text(text).map_err(err)?;
        let quote = check_quote(quote).map_err(err)?;
        let area = area.map(check_area).transpose().map_err(err)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_secs();
        let there = tx
            .query_row(
                &format!("SELECT {COLUMNS} FROM context_entries WHERE project = ?1 AND name = ?2"),
                params![scope, name],
                read_entry,
            )
            .optional()?;
        let saved = match there {
            None => {
                tx.execute(
                    "INSERT INTO context_entries \
                     (name, kind, text, quote, area, project, preload, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
                    params![
                        name,
                        kind,
                        text,
                        quote,
                        area.unwrap_or_default(),
                        scope,
                        i64::from(kind == "shape"),
                        now
                    ],
                )?;
                Saved::Added
            }
            Some(old) => {
                let new_area = area.unwrap_or_else(|| old.area.clone());
                let new_quote = if quote.is_empty() {
                    old.quote.clone()
                } else {
                    quote
                };
                if old.kind == kind
                    && old.text == text
                    && old.area == new_area
                    && old.quote == new_quote
                {
                    Saved::Same
                } else {
                    tx.execute(
                        "UPDATE context_entries SET kind = ?2, text = ?3, quote = ?4, area = ?5, \
                         updated_at = ?6 WHERE id = ?1",
                        params![old.id, kind, text, new_quote, new_area, now],
                    )?;
                    Saved::Changed
                }
            }
        };
        tx.commit()?;
        Ok(saved)
    }

    /// Deletes the entry `name` of this `scope`; false when there was none.
    pub fn delete_context(&mut self, scope: &str, name: &str) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "DELETE FROM context_entries WHERE project = ?1 AND name = ?2",
            params![scope, name.trim()],
        )?;
        Ok(changed > 0)
    }

    /// Switches the preload of the entry `name` of this `scope`; false when there was none.
    pub fn set_context_preload(
        &mut self,
        scope: &str,
        name: &str,
        preload: bool,
    ) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "UPDATE context_entries SET preload = ?3 WHERE project = ?1 AND name = ?2",
            params![scope, name.trim(), i64::from(preload)],
        )?;
        Ok(changed > 0)
    }

    /// Adds an entry for the web page: refused when this `scope` already has that name. The
    /// area and preload are as typed (no shape default here: the form shows the box).
    pub fn add_context_entry(
        &mut self,
        scope: &str,
        draft: &ContextDraft,
    ) -> Result<(), StoreError> {
        let checked = draft.check()?;
        let scope = check_scope(scope).map_err(err)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let there: i64 = tx.query_row(
            "SELECT COUNT(*) FROM context_entries WHERE project = ?1 AND name = ?2",
            params![scope, checked.name],
            |row| row.get(0),
        )?;
        if there > 0 {
            return Err(err(format!("{} exists already", checked.name)));
        }
        let now = now_secs();
        tx.execute(
            "INSERT INTO context_entries \
             (name, kind, text, quote, area, project, preload, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            params![
                checked.name,
                checked.kind,
                checked.text,
                checked.quote,
                checked.area,
                scope,
                i64::from(checked.preload),
                now
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Changes the entry `id` if it still has the stamp `expected` (see
    /// [`ContextEntry::fingerprint`]). The scope stays as it was.
    pub fn edit_context_entry(
        &mut self,
        id: i64,
        expected: &str,
        draft: &ContextDraft,
    ) -> Result<Edit, StoreError> {
        let checked = draft.check()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let there = tx
            .query_row(
                &format!("SELECT {COLUMNS} FROM context_entries WHERE id = ?1"),
                [id],
                read_entry,
            )
            .optional()?;
        let Some(there) = there else {
            return Ok(Edit::Gone);
        };
        if there.fingerprint() != expected {
            return Ok(Edit::Stale);
        }
        let other: i64 = tx.query_row(
            "SELECT COUNT(*) FROM context_entries WHERE project = ?1 AND name = ?2 AND id <> ?3",
            params![there.project, checked.name, id],
            |row| row.get(0),
        )?;
        if other > 0 {
            return Err(err(format!("{} exists already", checked.name)));
        }
        tx.execute(
            "UPDATE context_entries SET name = ?2, kind = ?3, text = ?4, quote = ?5, area = ?6, \
             preload = ?7, updated_at = ?8 WHERE id = ?1",
            params![
                id,
                checked.name,
                checked.kind,
                checked.text,
                checked.quote,
                checked.area,
                i64::from(checked.preload),
                now_secs()
            ],
        )?;
        tx.commit()?;
        Ok(Edit::Done)
    }

    /// Deletes the entry `id` if it still has the stamp `expected`.
    pub fn delete_context_checked(&mut self, id: i64, expected: &str) -> Result<Edit, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let there = tx
            .query_row(
                &format!("SELECT {COLUMNS} FROM context_entries WHERE id = ?1"),
                [id],
                read_entry,
            )
            .optional()?;
        let Some(there) = there else {
            return Ok(Edit::Gone);
        };
        if there.fingerprint() != expected {
            return Ok(Edit::Stale);
        }
        tx.execute("DELETE FROM context_entries WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(Edit::Done)
    }

    /// Every entry of every project (the web page lists them all).
    pub fn list_all_context(&self) -> Result<Vec<ContextEntry>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM context_entries ORDER BY project, name COLLATE NOCASE"
        ))?;
        let rows = statement.query_map([], read_entry)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
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
    fn an_entry_is_added_changed_and_found_ignoring_case() {
        let (_dir, mut store) = store("ctx-basic");
        assert_eq!(
            store
                .set_context(
                    "/p",
                    "no-input-edits",
                    "rule",
                    "Never edit input/",
                    "never touch input",
                    None
                )
                .unwrap(),
            Saved::Added
        );
        assert_eq!(
            store
                .set_context(
                    "/p",
                    "no-input-edits",
                    "rule",
                    "Never edit input/",
                    "",
                    None
                )
                .unwrap(),
            Saved::Same
        );
        assert_eq!(
            store
                .set_context(
                    "/p",
                    "NO-INPUT-EDITS",
                    "rule",
                    "Never edit input/ at all",
                    "",
                    None
                )
                .unwrap(),
            Saved::Changed
        );
        let entry = store
            .context_entry("/p", "No-Input-Edits")
            .unwrap()
            .unwrap();
        assert_eq!(entry.text, "Never edit input/ at all");
        assert_eq!(
            entry.quote, "never touch input",
            "an empty quote keeps the old one"
        );
        assert!(!entry.preload);
    }

    #[test]
    fn limits_and_kinds_are_checked_in_the_store() {
        let (_dir, mut store) = store("ctx-limits");
        assert!(store.set_context("/p", "", "rule", "x", "", None).is_err());
        assert!(
            store
                .set_context("/p", ".hidden", "rule", "x", "", None)
                .is_err()
        );
        assert!(
            store
                .set_context("/p", "a b", "rule", "x", "", None)
                .is_err()
        );
        assert!(store.set_context("/p", "a", "poem", "x", "", None).is_err());
        assert!(store.set_context("/p", "a", "rule", "", "", None).is_err());
        let long = "x".repeat(MAX_TEXT_BYTES + 1);
        assert!(
            store
                .set_context("/p", "a", "rule", &long, "", None)
                .is_err()
        );
        assert!(store.list_all_context().unwrap().is_empty());
    }

    #[test]
    fn a_project_sees_its_own_and_the_every_project_entries_and_its_own_hides_a_global_one() {
        let (_dir, mut store) = store("ctx-scope");
        store
            .set_context("", "global", "rule", "everywhere", "", None)
            .unwrap();
        store
            .set_context("", "both", "rule", "global text", "", None)
            .unwrap();
        store
            .set_context("/a", "both", "rule", "a text", "", None)
            .unwrap();
        store
            .set_context("/a", "only-a", "decision", "a only", "", None)
            .unwrap();
        store
            .set_context("/b", "only-b", "decision", "b only", "", None)
            .unwrap();
        let names = |p: &str| -> Vec<(String, String)> {
            store
                .list_context(p)
                .unwrap()
                .into_iter()
                .map(|e| (e.name, e.text))
                .collect()
        };
        assert_eq!(
            names("/a"),
            [
                ("both".to_owned(), "a text".to_owned()),
                ("global".to_owned(), "everywhere".to_owned()),
                ("only-a".to_owned(), "a only".to_owned()),
            ]
        );
        assert_eq!(names("/b").len(), 3);
        assert_eq!(names("/c").len(), 2);
        assert_eq!(
            store.context_entry("/c", "both").unwrap().unwrap().text,
            "global text"
        );
    }

    #[test]
    fn shapes_preload_by_default_and_the_preload_stops_at_the_token_cap() {
        let (_dir, mut store) = store("ctx-preload");
        store
            .set_context("/p", "a-shape", "shape", "short", "", Some("orders"))
            .unwrap();
        store
            .set_context("/p", "a-rule", "rule", "not preloaded", "", None)
            .unwrap();
        let big = "y".repeat(7_000);
        store
            .set_context("/p", "big-one", "shape", &big, "", None)
            .unwrap();
        store
            .set_context("/p", "big-two", "shape", &big, "", None)
            .unwrap();
        let (taken, left) = store.preload_context("/p").unwrap();
        let names: Vec<&str> = taken.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a-shape", "big-one"]);
        assert_eq!(left, ["big-two"]);
        assert!(store.set_context_preload("/p", "a-rule", true).unwrap());
        assert!(!store.set_context_preload("/p", "nope", true).unwrap());
        assert!(
            store
                .preload_context("/p")
                .unwrap()
                .0
                .iter()
                .any(|e| e.name == "a-rule")
        );
        assert!(store.delete_context("/p", "a-rule").unwrap());
        assert!(!store.delete_context("/p", "a-rule").unwrap());
    }
}

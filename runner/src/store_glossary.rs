//! The glossary (issue 27): project words and their definitions, kept in the database.
//!
//! Part of the store (`impl Store` continues here); the table layout is owned by `docs/erd.md`.
//! A term is unique ignoring case. The model reads it through the `glossary` tool and can only
//! change it through `glossary_propose`, which asks the user every time; `/glossary` is the
//! owner's own way in.

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::store::{Edit, Store, StoreError, fingerprint, now_secs};

/// The longest term, in characters.
pub const MAX_TERM_CHARS: usize = 60;
/// The longest definition, in bytes.
pub const MAX_DEFINITION_BYTES: usize = 2_000;
/// The longest area, in characters.
pub const MAX_AREA_CHARS: usize = 40;
/// How many terms a partial search returns.
pub const PARTIAL_MATCHES: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: i64,
    pub term: String,
    pub area: String,
    pub definition: String,
    /// The working directory it belongs to; empty means every project.
    pub project: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Entry {
    /// A stamp of what the entry says, for the web page's "changed since you opened it" check.
    pub fn fingerprint(&self) -> String {
        fingerprint(&[&self.term, &self.area, &self.definition])
    }
}

/// What [`Store::set_glossary`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Saved {
    Added,
    Changed,
    /// The same term with the same definition (and area) was there already.
    Same,
}

fn err(text: impl Into<String>) -> StoreError {
    StoreError::new(text)
}

/// A term as it will be stored: trimmed, one line, 1 to [`MAX_TERM_CHARS`] characters.
pub fn check_term(term: &str) -> Result<String, String> {
    let term = term.trim();
    if term.is_empty() {
        return Err("the term is empty".to_owned());
    }
    if term.chars().any(char::is_control) {
        return Err("a term is one line of plain text".to_owned());
    }
    if term.chars().count() > MAX_TERM_CHARS {
        return Err(format!("a term is at most {MAX_TERM_CHARS} characters"));
    }
    Ok(term.to_owned())
}

/// A definition as it will be stored: trimmed, 1 to [`MAX_DEFINITION_BYTES`] bytes.
pub fn check_definition(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("the definition is empty".to_owned());
    }
    if text.len() > MAX_DEFINITION_BYTES {
        return Err(format!(
            "a definition is at most {MAX_DEFINITION_BYTES} bytes ({} given)",
            text.len()
        ));
    }
    Ok(text.to_owned())
}

/// An area as it will be stored: trimmed, one line, at most [`MAX_AREA_CHARS`] characters; it may be empty.
pub fn check_area(area: &str) -> Result<String, String> {
    let area = area.trim();
    if area.chars().any(char::is_control) {
        return Err("an area is one line of plain text".to_owned());
    }
    if area.chars().count() > MAX_AREA_CHARS {
        return Err(format!("an area is at most {MAX_AREA_CHARS} characters"));
    }
    Ok(area.to_owned())
}

fn read_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    Ok(Entry {
        id: row.get(0)?,
        term: row.get(1)?,
        area: row.get(2)?,
        definition: row.get(3)?,
        project: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

const COLUMNS: &str = "id, term, area, definition, project, created_at, updated_at";

/// The rows that apply in the project `?1`: its own and the ones for every project, except a
/// shared word the project has its own version of. (With no project, only the shared ones.)
const APPLIES: &str = "(project = ?1 OR project = '') AND NOT (project = '' AND ?1 <> '' AND EXISTS \
    (SELECT 1 FROM glossary p WHERE p.project = ?1 AND p.term = glossary.term))";

impl Store {
    /// The terms that apply in this store's project, in alphabetical order (ignoring case).
    pub fn list_glossary(&self) -> Result<Vec<Entry>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM glossary WHERE {APPLIES} ORDER BY term COLLATE NOCASE"
        ))?;
        let rows = statement.query_map([&self.project], read_entry)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Every term of every project (the web page lists them all).
    pub fn list_all_glossary(&self) -> Result<Vec<Entry>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM glossary ORDER BY term COLLATE NOCASE, project"
        ))?;
        let rows = statement.query_map([], read_entry)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The term exactly (ignoring case), if it applies in this store's project.
    pub fn glossary_entry(&self, term: &str) -> Result<Option<Entry>, StoreError> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM glossary WHERE term = ?2 AND {APPLIES}"),
                params![self.project, term.trim()],
                read_entry,
            )
            .optional()?)
    }

    /// Up to `limit` terms that contain `text` (ignoring case), alphabetical.
    pub fn find_glossary(&self, text: &str, limit: usize) -> Result<Vec<Entry>, StoreError> {
        // `\` escapes `%` and `_`, so they match themselves.
        let pattern = format!(
            "%{}%",
            text.trim()
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let mut statement = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM glossary WHERE term LIKE ?2 ESCAPE '\\' AND {APPLIES} \
             ORDER BY term COLLATE NOCASE LIMIT ?3"
        ))?;
        let rows = statement.query_map(
            params![
                self.project,
                pattern,
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            read_entry,
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Adds a term, or replaces the definition of one that is there. `area` `None` keeps the
    /// area of an existing term (a new term gets none). The checks run here, so every way in
    /// (the tool, the menu, the import) obeys the same limits.
    pub fn set_glossary(
        &mut self,
        term: &str,
        definition: &str,
        area: Option<&str>,
    ) -> Result<Saved, StoreError> {
        let scope = self.project.clone();
        self.set_glossary_in(&scope, term, definition, area)
    }

    /// Like [`Store::set_glossary`], for the words of `scope` (a project, or empty for every
    /// project) instead of this store's project.
    pub fn set_glossary_in(
        &mut self,
        scope: &str,
        term: &str,
        definition: &str,
        area: Option<&str>,
    ) -> Result<Saved, StoreError> {
        let term = check_term(term).map_err(err)?;
        let definition = check_definition(definition).map_err(err)?;
        let area = area.map(check_area).transpose().map_err(err)?;
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = now_secs();
        let there: Option<(String, String)> = tx
            .query_row(
                "SELECT area, definition FROM glossary WHERE term = ?1 AND project = ?2",
                params![term, scope],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let saved = match there {
            None => {
                tx.execute(
                    "INSERT INTO glossary (term, area, definition, project, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?5, ?4, ?4)",
                    params![term, area.unwrap_or_default(), definition, now, scope],
                )?;
                Saved::Added
            }
            Some((old_area, old_definition)) => {
                let new_area = area.unwrap_or_else(|| old_area.clone());
                if old_definition == definition && old_area == new_area {
                    Saved::Same
                } else {
                    tx.execute(
                        "UPDATE glossary SET area = ?2, definition = ?3, updated_at = ?4 \
                         WHERE term = ?1 AND project = ?5",
                        params![term, new_area, definition, now, scope],
                    )?;
                    Saved::Changed
                }
            }
        };
        tx.commit()?;
        Ok(saved)
    }

    /// Adds a new term (the web page's add form). A term that is there already is an error:
    /// unlike [`Store::set_glossary`] this never replaces one.
    pub fn add_glossary(
        &mut self,
        term: &str,
        area: &str,
        definition: &str,
    ) -> Result<(), StoreError> {
        let scope = self.project.clone();
        self.add_glossary_in(&scope, term, area, definition)
    }

    /// Like [`Store::add_glossary`], for the words of `scope`.
    pub fn add_glossary_in(
        &mut self,
        scope: &str,
        term: &str,
        area: &str,
        definition: &str,
    ) -> Result<(), StoreError> {
        let scope = crate::store_context::check_scope(scope).map_err(err)?;
        let term = check_term(term).map_err(err)?;
        let definition = check_definition(definition).map_err(err)?;
        let area = check_area(area).map_err(err)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let taken: i64 = tx.query_row(
            "SELECT COUNT(*) FROM glossary WHERE term = ?1 AND project = ?2",
            params![term, scope],
            |row| row.get(0),
        )?;
        if taken > 0 {
            return Err(err(format!(
                "{term} is in the glossary already: edit it instead"
            )));
        }
        let now = now_secs();
        tx.execute(
            "INSERT INTO glossary (term, area, definition, project, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?5, ?4, ?4)",
            params![term, area, definition, now, scope],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Changes the entry `id` (term, area and definition) if it still has the stamp `expected`
    /// (see [`Entry::fingerprint`]). The checks and the stamp test happen in one transaction.
    pub fn edit_glossary(
        &mut self,
        id: i64,
        expected: &str,
        term: &str,
        area: &str,
        definition: &str,
    ) -> Result<Edit, StoreError> {
        let term = check_term(term).map_err(err)?;
        let definition = check_definition(definition).map_err(err)?;
        let area = check_area(area).map_err(err)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let there = tx
            .query_row(
                &format!("SELECT {COLUMNS} FROM glossary WHERE id = ?1"),
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
            "SELECT COUNT(*) FROM glossary WHERE term = ?1 AND project = ?3 AND id <> ?2",
            params![term, id, there.project],
            |row| row.get(0),
        )?;
        if other > 0 {
            return Err(err(format!("{term} is in the glossary already")));
        }
        if there.term == term && there.area == area && there.definition == definition {
            return Ok(Edit::Done);
        }
        tx.execute(
            "UPDATE glossary SET term = ?2, area = ?3, definition = ?4, updated_at = ?5 WHERE id = ?1",
            params![id, term, area, definition, now_secs()],
        )?;
        tx.commit()?;
        Ok(Edit::Done)
    }

    /// Deletes the entry `id` if it still has the stamp `expected`.
    pub fn delete_glossary_checked(&mut self, id: i64, expected: &str) -> Result<Edit, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let there = tx
            .query_row(
                &format!("SELECT {COLUMNS} FROM glossary WHERE id = ?1"),
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
        tx.execute("DELETE FROM glossary WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(Edit::Done)
    }

    /// Deletes a term. `false` when there was none.
    pub fn delete_glossary(&mut self, term: &str) -> Result<bool, StoreError> {
        let scope = self.project.clone();
        self.delete_glossary_in(&scope, term)
    }

    /// Deletes a term of `scope`. `false` when there was none.
    pub fn delete_glossary_in(&mut self, scope: &str, term: &str) -> Result<bool, StoreError> {
        let removed = self.conn.execute(
            "DELETE FROM glossary WHERE term = ?1 AND project = ?2",
            params![term.trim(), scope],
        )?;
        Ok(removed > 0)
    }
}

/// The word/definition pairs in the markdown tables of `text`. The first column is the term,
/// the second the definition, a third (when it has text) is added as "Not: ...". The header row
/// (the one above a `---` row), the `---` row itself and placeholder rows (`_{...}_`) are
/// skipped. Returns the pairs and how many table rows were left out.
pub fn parse_tables(text: &str) -> (Vec<(String, String)>, usize) {
    let (mut pairs, mut skipped) = (Vec::new(), 0);
    // The row above, which is a header if a `---` row follows.
    let mut held: Option<Vec<String>> = None;
    let take = |cells: Vec<String>, pairs: &mut Vec<(String, String)>, skipped: &mut usize| {
        let placeholder = cells.iter().any(|c| c.contains("{") && c.contains("}"));
        let term = cells.first().map(String::as_str).unwrap_or("");
        let meaning = cells.get(1).map(String::as_str).unwrap_or("");
        if placeholder || term.is_empty() || meaning.is_empty() {
            *skipped += 1;
            return;
        }
        let definition = match cells.get(2).filter(|c| !c.is_empty()) {
            Some(not) => format!("{meaning} Not: {not}"),
            None => meaning.to_owned(),
        };
        if check_term(term).is_ok() && check_definition(&definition).is_ok() {
            pairs.push((term.to_owned(), definition));
        } else {
            *skipped += 1;
        }
    };
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('|') {
            if let Some(cells) = held.take() {
                take(cells, &mut pairs, &mut skipped);
            }
            continue;
        }
        let cells: Vec<String> = line
            .trim_matches('|')
            .split('|')
            .map(|c| c.trim().to_owned())
            .collect();
        let rule = cells
            .iter()
            .all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':' | ' ')));
        if rule {
            held = None; // the row above was the header
            continue;
        }
        if let Some(above) = held.replace(cells) {
            take(above, &mut pairs, &mut skipped);
        }
    }
    if let Some(cells) = held.take() {
        take(cells, &mut pairs, &mut skipped);
    }
    (pairs, skipped)
}

/// The glossary as a markdown table, for `/glossary export`.
pub fn to_table(entries: &[Entry]) -> String {
    let flat = |text: &str| text.replace(['\n', '\r'], " ").replace('|', "/");
    let mut out = String::from("| Term | Definition |\n|---|---|\n");
    for entry in entries {
        out.push_str(&format!(
            "| {} | {} |\n",
            flat(&entry.term),
            flat(&entry.definition)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::Scratch;

    fn open(dir: &Scratch) -> Store {
        Store::open(&dir.path().join("harness.db")).unwrap()
    }

    #[test]
    fn a_term_and_a_definition_are_checked_before_they_are_stored() {
        assert_eq!(check_term("  kartu stok ").unwrap(), "kartu stok");
        assert!(check_term("").is_err());
        assert!(check_term("two\nlines").is_err());
        assert!(check_term(&"x".repeat(MAX_TERM_CHARS + 1)).is_err());
        assert!(check_term(&"x".repeat(MAX_TERM_CHARS)).is_ok());
        assert!(check_definition("   ").is_err());
        assert!(check_definition(&"x".repeat(MAX_DEFINITION_BYTES + 1)).is_err());
        assert!(check_definition(&"x".repeat(MAX_DEFINITION_BYTES)).is_ok());
        assert!(check_area("a\nb").is_err());
        assert_eq!(check_area("").unwrap(), "");
    }

    #[test]
    fn schema_17_makes_every_existing_word_one_for_every_project() {
        let dir = Scratch::new("glossary-v17");
        let path = dir.path().join("harness.db");
        drop(Store::open(&path).unwrap());
        {
            // A database as it was at schema 16: the old table shape, with a word in it.
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "DROP TABLE glossary; \
                 CREATE TABLE glossary (id INTEGER PRIMARY KEY, term TEXT NOT NULL UNIQUE COLLATE NOCASE, \
                   area TEXT NOT NULL DEFAULT '', definition TEXT NOT NULL, created_at INTEGER NOT NULL, \
                   updated_at INTEGER NOT NULL) STRICT; \
                 INSERT INTO glossary VALUES (7, 'claim', 'billing', 'a request', 1, 2);",
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 16).unwrap();
        }
        let store = Store::open(&path).unwrap().with_project("/repo/a");
        let entry = store.glossary_entry("claim").unwrap().unwrap();
        assert_eq!((entry.id, entry.project.as_str()), (7, ""));
        assert_eq!(
            (entry.area.as_str(), entry.definition.as_str()),
            ("billing", "a request")
        );
        assert!(dir.path().join("harness.db.before-25").exists());
    }

    #[test]
    fn a_word_is_for_a_project_or_for_every_project_and_the_projects_own_wins() {
        let dir = Scratch::new("glossary-scope");
        let mut shared = open(&dir);
        shared
            .set_glossary("claim", "shared", Some("billing"))
            .unwrap();
        shared.set_glossary("order", "a purchase", None).unwrap();
        let mut a = open(&dir).with_project("/repo/a");
        let b = open(&dir).with_project("/repo/b");
        // A project's own version of a shared word, in a, and a word only a has.
        assert_eq!(a.set_glossary("claim", "ours", None).unwrap(), Saved::Added);
        a.set_glossary("sku", "stock unit", None).unwrap();
        let terms = |s: &Store| -> Vec<(String, String)> {
            s.list_glossary()
                .unwrap()
                .into_iter()
                .map(|e| (e.term, e.definition))
                .collect()
        };
        assert_eq!(
            terms(&a),
            [
                ("claim".to_owned(), "ours".to_owned()),
                ("order".to_owned(), "a purchase".to_owned()),
                ("sku".to_owned(), "stock unit".to_owned())
            ]
        );
        assert_eq!(terms(&b).len(), 2, "b sees the shared words only");
        assert_eq!(terms(&b)[0].1, "shared");
        assert_eq!(terms(&shared).len(), 2, "no project: the shared words only");
        assert!(b.glossary_entry("sku").unwrap().is_none());
        assert!(b.find_glossary("sku", 5).unwrap().is_empty());
        assert_eq!(a.find_glossary("c", 5).unwrap().len(), 1);
        assert_eq!(a.list_all_glossary().unwrap().len(), 4);
        // The same word can be added once per scope, not twice in one.
        assert!(a.add_glossary("sku", "", "again").is_err());
        a.add_glossary_in("", "sku", "", "shared sku").unwrap();
        assert!(a.add_glossary_in("", "SKU", "", "x").is_err());
        // An edit by id stays in its scope, and a rename may not collide inside it.
        let id = a.glossary_entry("order").unwrap().unwrap();
        assert_eq!(id.project, "");
        assert!(
            a.edit_glossary(id.id, &id.fingerprint(), "claim", "", "x")
                .is_err()
        );
        assert!(a.delete_glossary_in("/repo/a", "claim").unwrap());
        assert_eq!(
            a.glossary_entry("claim").unwrap().unwrap().definition,
            "shared"
        );
    }

    #[test]
    fn adding_changing_and_deleting_a_term() {
        let dir = Scratch::new("glossary-crud");
        let mut store = open(&dir);
        assert_eq!(
            store
                .set_glossary("Kartu Stok", "stock card", Some("inventory"))
                .unwrap(),
            Saved::Added
        );
        // The same term in other letters is the same term.
        assert_eq!(
            store
                .set_glossary("kartu stok", "stock card", None)
                .unwrap(),
            Saved::Same
        );
        assert_eq!(
            store
                .set_glossary("KARTU STOK", "the stock ledger of one item", None)
                .unwrap(),
            Saved::Changed
        );
        let entry = store.glossary_entry("kartu stok").unwrap().unwrap();
        assert_eq!(entry.term, "Kartu Stok", "the first spelling stays");
        assert_eq!(entry.area, "inventory", "no area given keeps the old one");
        assert_eq!(entry.definition, "the stock ledger of one item");
        assert_eq!(store.list_glossary().unwrap().len(), 1);
        assert!(store.set_glossary("x", "", None).is_err());
        assert!(!store.delete_glossary("nope").unwrap());
        assert!(store.delete_glossary("KARTU stok").unwrap());
        assert!(store.list_glossary().unwrap().is_empty());
    }

    #[test]
    fn a_partial_search_is_alphabetical_limited_and_takes_wildcards_literally() {
        let dir = Scratch::new("glossary-find");
        let mut store = open(&dir);
        for term in ["beta", "Alpha", "alphabet", "100%", "a_b"] {
            store.set_glossary(term, "d", None).unwrap();
        }
        let found = |text: &str, limit: usize| -> Vec<String> {
            store
                .find_glossary(text, limit)
                .unwrap()
                .into_iter()
                .map(|e| e.term)
                .collect()
        };
        assert_eq!(found("alph", 10), ["Alpha", "alphabet"]);
        assert_eq!(found("alph", 1), ["Alpha"]);
        assert_eq!(found("%", 10), ["100%"]);
        assert_eq!(found("_", 10), ["a_b"]);
        assert!(found("zzz", 10).is_empty());
    }

    #[test]
    fn tables_are_read_without_header_rule_or_placeholder_rows() {
        let text = "\
# Vocabulary

| You hear | It is | Not |
|---|---|---|
| _{business term}_ | _{the artifact}_ | _{the guess}_ |
| kartu stok | the stock ledger | `StockCard` |
| order | a purchase | |
| bad row |

Some prose.

| Term | Meaning |
| --- | --- |
| claim | a request for payment |
";
        let (pairs, skipped) = parse_tables(text);
        assert_eq!(
            pairs,
            [
                (
                    "kartu stok".to_owned(),
                    "the stock ledger Not: `StockCard`".to_owned()
                ),
                ("order".to_owned(), "a purchase".to_owned()),
                ("claim".to_owned(), "a request for payment".to_owned()),
            ]
        );
        assert_eq!(skipped, 2, "the placeholder row and the short row");
    }

    #[test]
    fn the_table_written_by_export_can_be_read_back() {
        let dir = Scratch::new("glossary-table");
        let mut store = open(&dir);
        store
            .set_glossary("a|b", "has a | bar\nand a break", None)
            .unwrap();
        store.set_glossary("zed", "last", None).unwrap();
        let text = to_table(&store.list_glossary().unwrap());
        assert!(text.starts_with("| Term | Definition |\n|---|---|\n"));
        let (pairs, skipped) = parse_tables(&text);
        assert_eq!(skipped, 0);
        assert_eq!(
            pairs,
            [
                ("a/b".to_owned(), "has a / bar and a break".to_owned()),
                ("zed".to_owned(), "last".to_owned()),
            ]
        );
    }

    #[test]
    fn upgrading_a_database_copies_it_first_and_updates_an_unedited_base() {
        use crate::store_prompts::{BUILTIN_PROMPT, BUILTIN_PROMPT_V10};
        let dir = Scratch::new("glossary-upgrade");
        let path = dir.path().join("harness.db");
        drop(Store::open(&path).unwrap());
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute("DROP TABLE glossary", []).unwrap();
            conn.execute(
                "UPDATE prompts SET text = ?1 WHERE name = 'base'",
                [BUILTIN_PROMPT_V10],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 10).unwrap();
        }
        let mut store = Store::open(&path).unwrap();
        assert_eq!(store.prompt("base").unwrap().text, BUILTIN_PROMPT);
        assert_eq!(
            store.set_glossary("t", "d", None).unwrap(),
            Saved::Added,
            "the table exists after the upgrade"
        );
        let copy = dir.path().join(format!(
            "harness.db.before-{}",
            crate::store::SCHEMA_VERSION
        ));
        assert!(copy.exists(), "a copy of the version 10 database");
        let old = rusqlite::Connection::open(&copy).unwrap();
        let version: i64 = old
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 10);
        assert!(
            old.prepare("SELECT 1 FROM glossary").is_err(),
            "the copy is from before the table"
        );
    }

    #[test]
    fn an_edited_base_is_left_alone_by_the_upgrade() {
        let dir = Scratch::new("glossary-upgrade-edited");
        let path = dir.path().join("harness.db");
        drop(Store::open(&path).unwrap());
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute("DROP TABLE glossary", []).unwrap();
            conn.execute("UPDATE prompts SET text = 'mine' WHERE name = 'base'", [])
                .unwrap();
            conn.pragma_update(None, "user_version", 10).unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.prompt("base").unwrap().text, "mine");
    }

    #[test]
    fn a_new_database_makes_no_copy() {
        let dir = Scratch::new("glossary-new");
        drop(open(&dir));
        assert!(!dir.path().join("harness.db.before-11").exists());
    }

    #[test]
    fn a_checked_glossary_edit_needs_the_entry_to_be_as_it_was_shown() {
        let dir = Scratch::new("glossary-checked");
        let mut store = open(&dir);
        store.add_glossary(" Claim ", "sales", "a request").unwrap();
        assert!(
            store.add_glossary("claim", "", "again").is_err(),
            "no silent replace"
        );
        store.add_glossary("order", "", "a purchase").unwrap();
        let claim = store.glossary_entry("claim").unwrap().unwrap();
        // A rename, with the checks of the terminal.
        assert_eq!(
            store
                .edit_glossary(
                    claim.id,
                    &claim.fingerprint(),
                    "claims",
                    "ar",
                    "a request for money"
                )
                .unwrap(),
            Edit::Done
        );
        let claims = store.glossary_entry("claims").unwrap().unwrap();
        assert_eq!(
            (claims.area.as_str(), claims.definition.as_str()),
            ("ar", "a request for money")
        );
        assert!(store.glossary_entry("claim").unwrap().is_none());
        // Stale after the change; the taken term, an empty definition and a long term are refused.
        assert_eq!(
            store
                .edit_glossary(claim.id, &claim.fingerprint(), "x", "", "y")
                .unwrap(),
            Edit::Stale
        );
        assert!(
            store
                .edit_glossary(claims.id, &claims.fingerprint(), "ORDER", "", "y")
                .is_err()
        );
        assert!(
            store
                .edit_glossary(claims.id, &claims.fingerprint(), "claims", "", " ")
                .is_err()
        );
        assert!(
            store
                .edit_glossary(claims.id, &claims.fingerprint(), &"t".repeat(61), "", "y")
                .is_err()
        );
        // Keeping the term is not "taken", and changing only the case is allowed.
        assert_eq!(
            store
                .edit_glossary(
                    claims.id,
                    &claims.fingerprint(),
                    "Claims",
                    "ar",
                    "a request for money"
                )
                .unwrap(),
            Edit::Done
        );
        let claims = store.glossary_entry("claims").unwrap().unwrap();
        assert_eq!(
            store.delete_glossary_checked(claims.id, "stale").unwrap(),
            Edit::Stale
        );
        assert_eq!(
            store
                .delete_glossary_checked(claims.id, &claims.fingerprint())
                .unwrap(),
            Edit::Done
        );
        assert_eq!(
            store
                .delete_glossary_checked(claims.id, &claims.fingerprint())
                .unwrap(),
            Edit::Gone
        );
        assert_eq!(
            store
                .edit_glossary(claims.id, &claims.fingerprint(), "q", "", "y")
                .unwrap(),
            Edit::Gone
        );
    }
}

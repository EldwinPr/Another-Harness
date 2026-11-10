//! The inbox of the buffer mode of `--mcp-serve` (issue 47): submissions from another harness that
//! the owner has not decided yet, and the trail of the ones that were.
//!
//! Part of the store (`impl Store` continues here); the table layout is owned by `docs/erd.md`.
//! Three rules hold for the table:
//!
//! - **A row is never deleted.** After the insert only `status`, `decided_at` and `decision_note`
//!   change, and only while the row is `pending` (an apply error is written to `decision_note`
//!   and the row stays pending).
//! - **`entry_json` is stored as it was first serialised** ([`Submission`] fields in declaration
//!   order, no maps, so the bytes are stable) and `content_sha256` is the hash of those bytes. A
//!   partial unique index over the pending rows makes an identical resubmission the same row.
//! - **A submission is checked with the checks the direct tools and the store use** (`check_*`),
//!   so there is one set of rules for what may be stored. [`Checked`] is the only way to get a
//!   submission into the table.
//!
//! [`apply_entry`] is what the owner's "accept" runs: deterministic, no model, the store function
//! of the kind. [`reject`] marks a row rejected and touches no curated table.

use std::collections::HashMap;

use rusqlite::{OptionalExtension, Row, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::decider::{Opinion, Verdict, derive};
use crate::store::{Edit, Store, StoreError, now_secs, sha256_hex};
use crate::store_context::{Saved, check_kind, check_name, check_quote, check_scope, check_text};
use crate::store_glossary::{check_area, check_definition, check_term};
use crate::store_profiles::check_description;
use crate::store_prompts::{PromptType, check_prompt_name, check_prompt_text};

/// The most entries that may wait for the owner at once.
pub const MAX_PENDING: i64 = 200;
/// The longest `why`, in characters.
pub const MAX_WHY_CHARS: usize = 1_000;
/// The most of a decider's reply that is kept in `buffer_suggestions.raw`.
pub const MAX_RAW_CHARS: usize = 20_000;
/// The longest `source`, in characters.
pub const MAX_SOURCE_CHARS: usize = 100;
/// What can be submitted. Profiles cannot: only the owner changes what a chat starts with.
pub const KINDS: [&str; 5] = ["context", "glossary", "skill", "personality", "prompt"];
/// What a submission can ask for: add or replace, or delete.
pub const ACTIONS: [&str; 2] = ["set", "delete"];

/// One proposed change, as the sender gave it and as the checks normalised it. This is the text
/// that goes into `entry_json`, in this field order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submission {
    /// `context`, `glossary`, `skill`, `personality` or `prompt` (a system prompt).
    pub kind: String,
    /// `set` (add or replace) or `delete`.
    pub action: String,
    /// The entry's name, or the glossary term.
    pub name: String,
    /// The folder the entry belongs to; empty for every project. Always empty for the library kinds.
    pub project: String,
    /// A context entry's own kind (`rule`, `decision`, `shape` or `guide`); empty for other kinds.
    pub context_kind: String,
    /// The entry's text (a glossary term's definition); empty for a delete.
    pub text: String,
    /// One line: what a library item is for; empty for other kinds and for a delete.
    pub description: String,
    /// The owner's own words a context entry comes from; empty keeps the stored one.
    pub quote: String,
    /// The business area of a context entry or glossary term; `None` keeps the stored one.
    pub area: Option<String>,
}

/// Characters that draw nothing or reorder what is drawn: the soft hyphen, zero-width and
/// direction marks, bidi overrides, invisible operators, the byte order mark and the Unicode "tag"
/// block. The review screens escape text but cannot show these, so a reviewer would read less than
/// the model does.
fn is_hidden(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{2069}' | '\u{FEFF}' | '\u{E0000}'..='\u{E007F}')
}

/// Refuses a field with a control character (other than a new line or a tab) or a hidden one, so
/// what the owner reads on the review screen is all there is.
fn check_visible(field: &str, text: &str) -> Result<(), String> {
    match text
        .chars()
        .find(|&c| is_hidden(c) || (c.is_control() && c != '\n' && c != '\t'))
    {
        Some(c) => Err(format!(
            "{field} has a character a reviewer cannot see (U+{:04X}): remove it",
            c as u32
        )),
        None => Ok(()),
    }
}

impl Submission {
    /// The submission as it would be stored, or why it cannot be. The same checks as the direct
    /// tools and the store: names, kinds, texts, quotes, areas, descriptions and projects.
    pub fn validate(&self) -> Result<Submission, String> {
        for (field, text) in [
            ("kind", self.kind.as_str()),
            ("action", self.action.as_str()),
            ("context_kind", self.context_kind.as_str()),
            ("name", self.name.as_str()),
            ("text", self.text.as_str()),
            ("description", self.description.as_str()),
            ("quote", self.quote.as_str()),
            ("area", self.area.as_deref().unwrap_or_default()),
            ("project", self.project.as_str()),
        ] {
            check_visible(field, text)?;
        }
        let kind = self.kind.trim().to_lowercase();
        let action = self.action.trim().to_lowercase();
        if kind == "profile" || kind == "profiles" {
            return Err("profiles cannot be submitted: only the owner changes them".to_owned());
        }
        if !KINDS.contains(&kind.as_str()) {
            return Err(format!("a kind is one of: {}", KINDS.join(", ")));
        }
        if !ACTIONS.contains(&action.as_str()) {
            return Err(format!("an action is one of: {}", ACTIONS.join(", ")));
        }
        let delete = action == "delete";
        let allowed: &[&str] = match (kind.as_str(), delete) {
            (_, true) => &[],
            ("context", false) => &["context_kind", "text", "quote", "area"],
            ("glossary", false) => &["text", "area"],
            _ => &["text", "description"],
        };
        let given = [
            ("context_kind", !self.context_kind.trim().is_empty()),
            ("text", !self.text.trim().is_empty()),
            ("description", !self.description.trim().is_empty()),
            ("quote", !self.quote.trim().is_empty()),
            (
                "area",
                self.area.as_deref().is_some_and(|a| !a.trim().is_empty()),
            ),
        ];
        for (field, present) in given {
            if present && !allowed.contains(&field) {
                return Err(if delete {
                    format!("a delete takes no {field}")
                } else {
                    format!("'{field}' does not apply to a {kind}")
                });
            }
        }
        let mut checked = Submission {
            kind: kind.clone(),
            action,
            ..Submission::default()
        };
        match kind.as_str() {
            "context" => {
                checked.name = check_name(&self.name)?;
                checked.project = check_scope(&self.project)?;
                if !delete {
                    checked.context_kind = check_kind(&self.context_kind)?;
                    checked.text = check_text(&self.text)?;
                    checked.quote = check_quote(&self.quote)?;
                    checked.area = self.area.as_deref().map(check_area).transpose()?;
                }
            }
            "glossary" => {
                checked.name = check_term(&self.name)?;
                checked.project = check_scope(&self.project)?;
                if !delete {
                    checked.text = check_definition(&self.text)?;
                    checked.area = self.area.as_deref().map(check_area).transpose()?;
                }
            }
            _ => {
                checked.name = self.name.trim().to_owned();
                check_prompt_name(&checked.name)?;
                if !check_scope(&self.project)?.is_empty() {
                    return Err(format!(
                        "a {kind} is for every project: leave 'project' out"
                    ));
                }
                if !delete {
                    checked.text = self.text.trim().to_owned();
                    check_prompt_text(&checked.text)?;
                    checked.description = check_description(&self.description)?;
                }
            }
        }
        Ok(checked)
    }

    /// The prompt type of a library kind.
    fn prompt_type(&self) -> Option<PromptType> {
        match self.kind.as_str() {
            "skill" => Some(PromptType::Skill),
            "personality" => Some(PromptType::Personality),
            "prompt" => Some(PromptType::System),
            _ => None,
        }
    }

    /// What the owner calls this, for the answers.
    fn what(&self) -> String {
        match self.kind.as_str() {
            "context" => format!("context entry '{}' for {}", self.name, scope(&self.project)),
            "glossary" => format!("glossary term '{}' for {}", self.name, scope(&self.project)),
            "prompt" => format!("system prompt '{}'", self.name),
            other => format!("{other} '{}'", self.name),
        }
    }
}

/// Where an entry applies, for the answers.
pub(crate) fn scope(project: &str) -> String {
    if project.is_empty() {
        "every project".to_owned()
    } else {
        format!("project {project}")
    }
}

/// A `why`: required, at most [`MAX_WHY_CHARS`] characters, plain text (new lines are fine).
pub fn check_why(why: &str) -> Result<String, String> {
    let why = why.trim();
    if why.is_empty() {
        return Err("why is required: say why you propose this".to_owned());
    }
    if why
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
    {
        return Err("why is plain text".to_owned());
    }
    if why.chars().count() > MAX_WHY_CHARS {
        return Err(format!("why is at most {MAX_WHY_CHARS} characters"));
    }
    Ok(why.to_owned())
}

/// A `source`: the sender's own name for itself; required, one line, at most
/// [`MAX_SOURCE_CHARS`] characters. It is a claim and not an identity.
pub fn check_source(source: &str) -> Result<String, String> {
    let source = source.trim();
    if source.is_empty() {
        return Err("source is required: your own name, for example claude-code".to_owned());
    }
    if source.chars().any(char::is_control) {
        return Err("source is one line of plain text".to_owned());
    }
    if source.chars().count() > MAX_SOURCE_CHARS {
        return Err(format!("source is at most {MAX_SOURCE_CHARS} characters"));
    }
    Ok(source.to_owned())
}

/// A submission that passed every check, with its `why` and `source`: the only thing
/// [`Store::queue_submission`] takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    submission: Submission,
    why: String,
    source: String,
}

impl Checked {
    /// Checks the submission, then `why`, then `source`; the first problem is the answer.
    pub fn new(raw: &Submission, why: &str, source: &str) -> Result<Self, String> {
        let submission = raw.validate()?;
        let why = check_why(why)?;
        let source = check_source(source)?;
        check_visible("why", &why)?;
        check_visible("source", &source)?;
        Ok(Self {
            submission,
            why,
            source,
        })
    }

    pub fn submission(&self) -> &Submission {
        &self.submission
    }

    /// The sender's reason, checked.
    pub fn why(&self) -> &str {
        &self.why
    }

    /// What the sender called itself, checked.
    pub fn source(&self) -> &str {
        &self.source
    }
}

/// What [`Store::queue_submission`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submitted {
    /// A new row, pending.
    Queued(i64),
    /// An identical entry is pending already, as this row.
    AlreadyQueued(i64),
    /// [`MAX_PENDING`] entries wait for the owner; nothing was added.
    Full,
}

/// Where a row stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pending,
    Applied,
    Rejected,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Applied => "applied",
            Status::Rejected => "rejected",
        }
    }

    fn parse(text: &str) -> Self {
        match text {
            "applied" => Status::Applied,
            "rejected" => Status::Rejected,
            _ => Status::Pending,
        }
    }
}

/// One row of the inbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferEntry {
    pub id: i64,
    pub kind: String,
    pub action: String,
    pub name: String,
    pub project: String,
    /// The submission as it was stored; [`BufferEntry::submission`] reads it.
    pub entry_json: String,
    pub why: String,
    /// The sender's name for itself, as claimed.
    pub source: String,
    pub status: Status,
    pub content_sha256: String,
    pub created_at: i64,
    pub decided_at: Option<i64>,
    /// The owner's reason on a reject, or the apply error while the row is still pending.
    pub decision_note: String,
}

impl BufferEntry {
    /// The stored submission.
    pub fn submission(&self) -> Result<Submission, String> {
        serde_json::from_str(&self.entry_json)
            .map_err(|e| format!("the stored submission cannot be read: {e}"))
    }
}

const COLUMNS: &str = "id, kind, action, name, project, entry_json, why, source, status, \
     content_sha256, created_at, decided_at, decision_note";

fn read_entry(row: &Row<'_>) -> rusqlite::Result<BufferEntry> {
    Ok(BufferEntry {
        id: row.get(0)?,
        kind: row.get(1)?,
        action: row.get(2)?,
        name: row.get(3)?,
        project: row.get(4)?,
        entry_json: row.get(5)?,
        why: row.get(6)?,
        source: row.get(7)?,
        status: Status::parse(&row.get::<_, String>(8)?),
        content_sha256: row.get(9)?,
        created_at: row.get(10)?,
        decided_at: row.get(11)?,
        decision_note: row.get(12)?,
    })
}

impl Store {
    /// Adds the entry to the inbox, unless an identical one is pending (then that row is the
    /// answer) or [`MAX_PENDING`] are pending. This is the only write `submit` makes.
    pub fn queue_submission(&mut self, checked: &Checked) -> Result<Submitted, StoreError> {
        let sub = &checked.submission;
        let json = serde_json::to_string(sub)
            .map_err(|e| StoreError::new(format!("cannot serialise the submission: {e}")))?;
        let hash = sha256_hex(json.as_bytes());
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let same: Option<i64> = tx
            .query_row(
                "SELECT id FROM buffer_entries WHERE content_sha256 = ?1 AND status = 'pending'",
                [&hash],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = same {
            return Ok(Submitted::AlreadyQueued(id));
        }
        let pending: i64 = tx.query_row(
            "SELECT COUNT(*) FROM buffer_entries WHERE status = 'pending'",
            [],
            |row| row.get(0),
        )?;
        if pending >= MAX_PENDING {
            return Ok(Submitted::Full);
        }
        tx.execute(
            "INSERT INTO buffer_entries \
             (kind, action, name, project, entry_json, why, source, status, content_sha256, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', ?8, ?9)",
            params![
                sub.kind,
                sub.action,
                sub.name,
                sub.project,
                json,
                checked.why,
                checked.source,
                hash,
                now_secs()
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(Submitted::Queued(id))
    }

    /// The entries waiting for the owner, oldest first.
    pub fn list_pending(&self) -> Result<Vec<BufferEntry>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM buffer_entries WHERE status = 'pending' ORDER BY id"
        ))?;
        let rows = statement.query_map([], read_entry)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The entries the owner decided (applied or rejected), newest decision first, at most
    /// `limit`: the trail.
    pub fn list_decided(&self, limit: i64) -> Result<Vec<BufferEntry>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM buffer_entries WHERE status != 'pending' \
             ORDER BY decided_at DESC, id DESC LIMIT ?1"
        ))?;
        let rows = statement.query_map([limit], read_entry)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// One row of the inbox, whatever its status.
    pub fn buffer_entry(&self, id: i64) -> Result<Option<BufferEntry>, StoreError> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM buffer_entries WHERE id = ?1"),
                [id],
                read_entry,
            )
            .optional()?)
    }

    /// Marks a pending row applied; `false` when it is not pending (nothing changed). The note
    /// replaces an apply error the row may have been given.
    pub fn mark_buffer_applied(&mut self, id: i64, note: &str) -> Result<bool, StoreError> {
        self.decide(id, "applied", note)
    }

    /// Marks a pending row rejected, with the owner's reason; `false` when it is not pending.
    pub fn mark_buffer_rejected(&mut self, id: i64, note: &str) -> Result<bool, StoreError> {
        self.decide(id, "rejected", note)
    }

    fn decide(&mut self, id: i64, status: &str, note: &str) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "UPDATE buffer_entries SET status = ?2, decided_at = ?3, decision_note = ?4 \
             WHERE id = ?1 AND status = 'pending'",
            params![id, status, now_secs(), note.trim()],
        )?;
        Ok(changed > 0)
    }

    /// Records why applying failed and leaves the row pending; `false` when it is not pending.
    pub fn set_buffer_note(&mut self, id: i64, note: &str) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "UPDATE buffer_entries SET decision_note = ?2 WHERE id = ?1 AND status = 'pending'",
            params![id, note.trim()],
        )?;
        Ok(changed > 0)
    }
}

/// What a decider suggested about an entry (issue 48): one row of `buffer_suggestions`. It is
/// written once, never changed, and only ever shown; the owner's accept or reject does not read it.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub id: i64,
    pub buffer_id: i64,
    /// Who said it, for example `llm:openrouter:deepseek/x`.
    pub decider: String,
    /// What the decider said.
    pub verdict: Verdict,
    pub confidence: Option<f64>,
    pub reason: String,
    /// The decider's reply exactly as received.
    pub raw: String,
    pub created_at: i64,
}

impl Suggestion {
    /// What a screen shows: accept, reject or unsure, derived from what the decider said with the
    /// thresholds of its kind (see `decider::derive`).
    pub fn shown(&self) -> Verdict {
        derive(&self.decider, self.verdict, self.confidence)
    }
}

fn read_suggestion(row: &Row<'_>) -> rusqlite::Result<Suggestion> {
    Ok(Suggestion {
        id: row.get(0)?,
        buffer_id: row.get(1)?,
        decider: row.get(2)?,
        verdict: Verdict::parse(&row.get::<_, String>(3)?).unwrap_or(Verdict::Unsure),
        confidence: row.get(4)?,
        reason: row.get(5)?,
        raw: row.get(6)?,
        created_at: row.get(7)?,
    })
}

impl Store {
    /// Records what `decider` said about entry `buffer_id`. A decider speaks once per entry:
    /// `false` (and nothing written) when it has already. An error when there is no such entry.
    pub fn add_suggestion(
        &mut self,
        buffer_id: i64,
        decider: &str,
        opinion: &Opinion,
    ) -> Result<bool, StoreError> {
        // Nothing shows `raw`, and a router that ignores its limits can send megabytes.
        let raw: String = opinion.raw.chars().take(MAX_RAW_CHARS).collect();
        let added = self.conn.execute(
            "INSERT INTO buffer_suggestions \
             (buffer_id, decider, verdict, confidence, reason, raw, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT (buffer_id, decider) DO NOTHING",
            params![
                buffer_id,
                decider,
                opinion.verdict.as_str(),
                opinion.confidence,
                opinion.reason,
                raw,
                now_secs()
            ],
        )?;
        Ok(added > 0)
    }

    /// What the deciders said about entry `buffer_id`, oldest first.
    pub fn list_suggestions(&self, buffer_id: i64) -> Result<Vec<Suggestion>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT id, buffer_id, decider, verdict, confidence, reason, raw, created_at \
             FROM buffer_suggestions WHERE buffer_id = ?1 ORDER BY id",
        )?;
        let rows = statement.query_map([buffer_id], read_suggestion)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The suggestions of each of the entries, by entry id; an entry nobody spoke about is absent.
    pub fn suggestions_for(
        &self,
        ids: impl IntoIterator<Item = i64>,
    ) -> Result<HashMap<i64, Vec<Suggestion>>, StoreError> {
        let mut found = HashMap::new();
        for id in ids {
            let list = self.list_suggestions(id)?;
            if !list.is_empty() {
                found.insert(id, list);
            }
        }
        Ok(found)
    }
}

/// What is stored now where a submission points, for a screen to show next to it and to pass
/// back as `expected`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    /// The row id of a context entry, glossary term or library item.
    pub id: i64,
    /// The stamp of the stored item (`fingerprint()` of the item), what `apply_entry` compares.
    pub fingerprint: String,
    /// The text (a glossary term's definition).
    pub text: String,
    pub description: String,
    pub context_kind: String,
    pub quote: String,
    pub area: String,
    /// A system prompt that is in the profile new chats start under: accepting changes what the
    /// next chat sends.
    pub on: bool,
}

/// What is stored now at the target of `sub`, or `None` when there is nothing there. An error
/// names a library item of another kind with that name.
pub fn current_target(store: &Store, sub: &Submission) -> Result<Option<Stored>, String> {
    let failed = |e: StoreError| e.to_string();
    match sub.kind.as_str() {
        "context" => Ok(store
            .list_all_context()
            .map_err(failed)?
            .into_iter()
            .find(|e| e.project == sub.project && e.name.eq_ignore_ascii_case(&sub.name))
            .map(|e| Stored {
                id: e.id,
                fingerprint: e.fingerprint(),
                text: e.text,
                description: String::new(),
                context_kind: e.kind,
                quote: e.quote,
                area: e.area,
                on: false,
            })),
        "glossary" => Ok(store
            .list_all_glossary()
            .map_err(failed)?
            .into_iter()
            .find(|e| e.project == sub.project && e.term.eq_ignore_ascii_case(&sub.name))
            .map(|e| Stored {
                id: e.id,
                fingerprint: e.fingerprint(),
                text: e.definition,
                description: String::new(),
                context_kind: String::new(),
                quote: String::new(),
                area: e.area,
                on: false,
            })),
        _ => {
            let wanted = sub.prompt_type().ok_or("not a kind that can be applied")?;
            let found = store
                .list_prompts()
                .map_err(failed)?
                .into_iter()
                .find(|p| p.name == sub.name);
            match found {
                None => Ok(None),
                Some(p) if p.prompt_type != wanted => Err(format!(
                    "'{}' is a {}, not a {}",
                    p.name,
                    p.prompt_type.as_str(),
                    wanted.as_str()
                )),
                Some(p) => Ok(Some(Stored {
                    id: p.id,
                    fingerprint: p.fingerprint(),
                    text: p.text.clone(),
                    description: p.description.clone(),
                    context_kind: String::new(),
                    quote: String::new(),
                    area: String::new(),
                    on: p.enabled && p.prompt_type == PromptType::System,
                })),
            }
        }
    }
}

/// What accepting an entry did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The change was made (or a delete found the item already gone); the text says which. The
    /// row is now `applied`.
    Applied(String),
    /// What the entry asks for was there already (or the row was applied before). The row is
    /// now `applied`.
    NoChange,
    /// The target is not as the reviewer saw it. Nothing was written; the row stays `pending`.
    Stale,
    /// The change could not be made. Nothing was written; the row stays `pending` with the
    /// reason in `decision_note`.
    Failed(String),
}

/// The note a stale target leaves on the row.
const STALE_NOTE: &str = "the stored item changed since it was shown: open the entry again";

/// Accepts entry `id`: re-validates it, then makes the change with the store function of its kind
/// (nothing here calls a model). `expected` is the stamp of the target as the reviewer saw it
/// ([`Stored::fingerprint`] from [`current_target`]), `None` when nothing was there; a target that
/// is different now is [`ApplyOutcome::Stale`] and nothing is written. A library item is checked
/// inside the store's own transaction too; a context entry or glossary term is checked just before
/// the write, which is enough for one owner at a keyboard.
///
/// A new library item is saved switched off. Accepting is idempotent: a `set` that is there is
/// `NoChange`, a `delete` of something gone is `Applied`, a row that is `applied` already is
/// `NoChange`, and a crash between the write and marking the row is repaired by accepting again.
/// A rejected row is never applied. An `Err` means there is no such row or the database failed
/// while marking it.
pub fn apply_entry(
    store: &mut Store,
    id: i64,
    expected: Option<&str>,
) -> Result<ApplyOutcome, StoreError> {
    let Some(row) = store.buffer_entry(id)? else {
        return Err(StoreError::new(format!("there is no submission #{id}")));
    };
    match row.status {
        Status::Applied => return Ok(ApplyOutcome::NoChange),
        Status::Rejected => {
            return Ok(ApplyOutcome::Failed(format!(
                "#{id} was rejected: submit it again to have it reviewed"
            )));
        }
        Status::Pending => {}
    }
    let outcome = apply_pending(store, &row, expected);
    match &outcome {
        ApplyOutcome::Applied(_) | ApplyOutcome::NoChange => {
            // Another window may have rejected the row between the write and this mark: say so,
            // since the trail would read "rejected" for a change that was made.
            if !store.mark_buffer_applied(id, "")? {
                return Ok(ApplyOutcome::Failed(format!(
                    "#{id} was decided by someone else while it was being applied: check the item and the inbox"
                )));
            }
        }
        ApplyOutcome::Stale => {
            store.set_buffer_note(id, STALE_NOTE)?;
        }
        ApplyOutcome::Failed(why) => {
            store.set_buffer_note(id, why)?;
        }
    }
    Ok(outcome)
}

/// Rejects entry `id` with the owner's reason (may be empty). Touches no curated table. An error
/// when there is no such row or it is not pending any more.
pub fn reject(store: &mut Store, id: i64, note: &str) -> Result<(), StoreError> {
    if store.mark_buffer_rejected(id, note)? {
        return Ok(());
    }
    Err(StoreError::new(match store.buffer_entry(id)? {
        None => format!("there is no submission #{id}"),
        Some(row) => format!("#{id} is {} already", row.status.as_str()),
    }))
}

fn apply_pending(store: &mut Store, row: &BufferEntry, expected: Option<&str>) -> ApplyOutcome {
    let sub = match row.submission().and_then(|stored| stored.validate()) {
        Ok(sub) => sub,
        Err(why) => return ApplyOutcome::Failed(format!("the submission is not valid: {why}")),
    };
    let stored = match current_target(store, &sub) {
        Ok(stored) => stored,
        Err(why) => return ApplyOutcome::Failed(why),
    };
    if stored.as_ref().map(|s| s.fingerprint.as_str()) != expected {
        return ApplyOutcome::Stale;
    }
    let what = sub.what();
    let failed = |e: StoreError| ApplyOutcome::Failed(e.to_string());
    let saved = |saved: Saved| match saved {
        Saved::Added => ApplyOutcome::Applied(format!("added {what}")),
        Saved::Changed => ApplyOutcome::Applied(format!("replaced {what}")),
        Saved::Same => ApplyOutcome::NoChange,
    };
    let deleted = |done: bool| {
        ApplyOutcome::Applied(if done {
            format!("deleted {what}")
        } else {
            format!("already gone: {what}")
        })
    };
    match (sub.kind.as_str(), sub.action.as_str()) {
        ("context", "set") => match store.set_context(
            &sub.project,
            &sub.name,
            &sub.context_kind,
            &sub.text,
            &sub.quote,
            sub.area.as_deref(),
        ) {
            Ok(done) => saved(done),
            Err(e) => failed(e),
        },
        ("context", _) => match store.delete_context(&sub.project, &sub.name) {
            Ok(done) => deleted(done),
            Err(e) => failed(e),
        },
        ("glossary", "set") => {
            match store.set_glossary_in(&sub.project, &sub.name, &sub.text, sub.area.as_deref()) {
                Ok(done) => saved(done),
                Err(e) => failed(e),
            }
        }
        ("glossary", _) => match store.delete_glossary_in(&sub.project, &sub.name) {
            Ok(done) => deleted(done),
            Err(e) => failed(e),
        },
        _ => apply_library(store, &sub, stored.as_ref()),
    }
}

/// A skill, personality or system prompt. `stored` is the item now, already checked against what
/// the reviewer saw.
fn apply_library(store: &mut Store, sub: &Submission, stored: Option<&Stored>) -> ApplyOutcome {
    let Some(kind) = sub.prompt_type() else {
        return ApplyOutcome::Failed("not a kind that can be applied".to_owned());
    };
    let what = sub.what();
    let failed = |e: StoreError| ApplyOutcome::Failed(e.to_string());
    match (sub.action.as_str(), stored) {
        ("set", None) => {
            match store.add_prompt_of_type(&sub.name, &sub.description, &sub.text, None, kind) {
                Ok(()) => ApplyOutcome::Applied(format!("added {what}, switched off")),
                Err(e) => failed(e),
            }
        }
        ("set", Some(old)) => {
            if old.text == sub.text && old.description == sub.description {
                return ApplyOutcome::NoChange;
            }
            match store.edit_prompt(
                old.id,
                &old.fingerprint,
                &sub.name,
                &sub.description,
                &sub.text,
            ) {
                Ok(Edit::Done) => ApplyOutcome::Applied(format!("changed {what}")),
                Ok(Edit::Stale) => ApplyOutcome::Stale,
                Ok(Edit::Gone) => ApplyOutcome::Failed(format!("{what} is gone")),
                Err(e) => failed(e),
            }
        }
        (_, None) => ApplyOutcome::Applied(format!("already gone: {what}")),
        (_, Some(old)) => match store.delete_prompt_checked(old.id, &old.fingerprint) {
            Ok(Edit::Done) => ApplyOutcome::Applied(format!("deleted {what}")),
            Ok(Edit::Stale) => ApplyOutcome::Stale,
            Ok(Edit::Gone) => ApplyOutcome::Applied(format!("already gone: {what}")),
            Err(e) => failed(e),
        },
    }
}

#[cfg(test)]
mod tests;

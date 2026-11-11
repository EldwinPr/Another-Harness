//! The Inbox pages of the web view (issue 47): what other harnesses have submitted through the MCP
//! server's buffer mode, for the owner to accept or reject.
//!
//! Everything a sender wrote (name, text, description, quote, area, `why`, `source`, the project)
//! goes through [`escape`] and is shown as plain text, `why` and `source` labelled as the
//! sender's claim. The forms carry the run's token like every other form; what they do is in
//! `web_write.rs`, which calls `store_buffer::apply_entry` and `store_buffer::reject`.

use std::collections::HashMap;

use crate::inbox_view::{self, DiffLine, line_diff, scope_words};
use crate::store::Store;
use crate::store_buffer::{BufferEntry, Status, Stored, Submission, Suggestion, current_target};
use crate::turn_log::utc_label;
use crate::web_pages::{View, banner, escape, hidden, page, text_field};

/// How many decided entries the trail shows.
pub const DECIDED_SHOWN: i64 = 50;
/// How much of a `why` a list row shows.
const WHY_CHARS: usize = 160;

/// The text of the banner for an enabled system prompt.
pub const ON_BANNER: &str = "This system prompt is on: accepting changes what the next chat sends.";

fn time(secs: i64) -> String {
    utc_label(u64::try_from(secs).unwrap_or(0))
}

fn what(e: &BufferEntry) -> String {
    format!(
        "{} {} <strong>{}</strong>",
        escape(&e.kind),
        escape(&e.action),
        escape(&e.name)
    )
}

/// The suggestions of an entry in a list cell: one line each, or `none`. A suggestion is never a
/// decision, and it is escaped like everything else a model or a sender wrote.
fn suggestion_cell(id: i64, suggestions: &HashMap<i64, Vec<Suggestion>>) -> String {
    match suggestions.get(&id).filter(|list| !list.is_empty()) {
        None => format!("<span class=\"dim\">{}</span>", inbox_view::NO_SUGGESTION),
        Some(list) => list
            .iter()
            .map(|s| escape(&inbox_view::suggestion_short(s)))
            .collect::<Vec<_>>()
            .join("<br>"),
    }
}

/// The Inbox: what waits for the owner, oldest first, then the trail of what was decided.
/// `suggestions` are what the deciders said, by entry id.
pub fn list(
    pending: &[BufferEntry],
    decided: &[BufferEntry],
    suggestions: &HashMap<i64, Vec<Suggestion>>,
    now: i64,
    view: &View<'_>,
) -> String {
    let mut body = banner(view);
    body.push_str("<p class=\"dim\">Proposals from other harnesses (the MCP server in its default buffer mode). Nothing here is stored until you accept it. <strong>From</strong> and <strong>why</strong> are what the sender says about itself; they are not checked.</p>");
    body.push_str("<h2>Waiting for you</h2>");
    if pending.is_empty() {
        body.push_str("<p class=\"dim\">Nothing is waiting. Submissions from other harnesses appear here.</p>");
    } else {
        body.push_str(&format!("<p class=\"dim\">{} waiting.</p>", pending.len()));
        body.push_str(
            "<table><tr><th>#</th><th>What</th><th>For</th><th>From (claimed)</th><th>Why (claimed)</th><th>Suggestion</th><th>Age</th><th></th></tr>",
        );
        for e in pending {
            let failed = if e.decision_note.is_empty() {
                String::new()
            } else {
                format!(
                    "<br><span class=\"dim\">last try failed: {}</span>",
                    escape(&e.decision_note)
                )
            };
            body.push_str(&format!(
                "<tr><td>{id}</td><td>{}</td><td class=\"dim\">{}</td><td>{}</td><td class=\"def\">{}{failed}</td><td>{}</td><td class=\"dim\">{}</td><td><a href=\"/inbox/show?id={id}\">Review</a></td></tr>",
                what(e),
                escape(&scope_words(&e.project)),
                escape(&e.source),
                escape(&inbox_view::first_line(&e.why, WHY_CHARS)),
                suggestion_cell(e.id, suggestions),
                escape(&inbox_view::age(e.created_at, now)),
                id = e.id
            ));
        }
        body.push_str("</table>");
    }
    body.push_str("<h2>Decided</h2>");
    if decided.is_empty() {
        body.push_str("<p class=\"dim\">Nothing has been decided yet.</p>");
    } else {
        body.push_str(&format!(
            "<p class=\"dim\">The last {DECIDED_SHOWN} decisions, newest first. Rows are kept for good.</p>"
        ));
        body.push_str(
            "<table><tr><th>#</th><th>Status</th><th>What</th><th>From (claimed)</th><th>Suggestion</th><th>Decided</th><th>Note</th></tr>",
        );
        for e in decided {
            body.push_str(&format!(
                "<tr><td><a href=\"/inbox/show?id={id}\">{id}</a></td><td><span class=\"badge\">{}</span></td><td>{}</td><td>{}</td><td>{}</td><td class=\"dim\">{}</td><td class=\"def\">{}</td></tr>",
                escape(e.status.as_str()),
                what(e),
                escape(&e.source),
                suggestion_cell(e.id, suggestions),
                e.decided_at.map(time).unwrap_or_default(),
                escape(&e.decision_note),
                id = e.id
            ));
        }
        body.push_str("</table>");
    }
    page("/inbox", "Inbox", &body)
}

/// What the stored item has under a label of [`inbox_view::submitted_fields`].
fn stored_field(label: &str, stored: &Stored) -> String {
    match label {
        "kind" => stored.context_kind.clone(),
        "area" => stored.area.clone(),
        "quote" => stored.quote.clone(),
        "type" => stored.context_kind.clone(),
        "quotes" => inbox_view::quotes_words(&stored.quotes),
        _ => stored.description.clone(),
    }
}

fn cell(text: &str) -> String {
    if text.is_empty() {
        "<span class=\"dim\">(empty)</span>".to_owned()
    } else {
        escape(text)
    }
}

fn diff_block(old: &str, new: &str) -> String {
    let mut out = String::from("<pre class=\"diff\">");
    for line in line_diff(old, new) {
        let class = match line {
            DiffLine::Same(_) => "",
            DiffLine::Removed(_) => " class=\"del\"",
            DiffLine::Added(_) => " class=\"add\"",
        };
        out.push_str(&format!("<span{class}>{}</span>\n", escape(&line.marked())));
    }
    out.push_str("</pre>");
    out
}

/// The part of the page that compares the submission with the stored item.
fn comparison(sub: &Submission, stored: Option<&Stored>) -> String {
    let mut out = String::new();
    if sub.action == "delete" {
        match stored {
            None => out.push_str("<p>Nothing is stored under this name, so there is nothing to delete. Accepting only marks the entry applied.</p>"),
            Some(s) => {
                out.push_str("<h2>Accepting deletes this</h2>");
                let fields: Vec<(&str, String)> = inbox_view::submitted_fields(&Submission {
                    kind: sub.kind.clone(),
                    ..Submission::default()
                });
                for (label, _) in fields {
                    out.push_str(&format!(
                        "<p>{}: {}</p>",
                        escape(label),
                        cell(&stored_field(label, s))
                    ));
                }
                out.push_str(&format!("<pre>{}</pre>", escape(&s.text)));
            }
        }
        return out;
    }
    out.push_str("<h2>Submitted next to what is stored now</h2>");
    match stored {
        None => {
            out.push_str("<p>Nothing is stored under this name: accepting adds it.");
            if !matches!(sub.kind.as_str(), "context" | "glossary" | "gotcha") {
                out.push_str(&format!(
                    " A new {} is saved switched off.",
                    escape(&sub.kind)
                ));
            }
            out.push_str("</p>");
        }
        Some(_) => {
            out.push_str("<p>Accepting replaces the stored item with the submitted one.</p>")
        }
    }
    out.push_str("<table><tr><th></th><th>Stored now</th><th>Submitted</th></tr>");
    for (label, submitted) in inbox_view::submitted_fields(sub) {
        out.push_str(&format!(
            "<tr><td class=\"dim\">{}</td><td class=\"def\">{}</td><td class=\"def\">{}</td></tr>",
            escape(label),
            stored.map_or_else(
                || "<span class=\"dim\">(nothing stored)</span>".to_owned(),
                |s| cell(&stored_field(label, s))
            ),
            escape(&submitted)
        ));
    }
    out.push_str("</table>");
    let old = stored.map_or("", |s| s.text.as_str());
    out.push_str("<h2>The text, line by line</h2>");
    if stored.is_some() && old == sub.text {
        out.push_str("<p class=\"dim\">The text is the same.</p>");
    }
    out.push_str(&diff_block(old, &sub.text));
    out
}

/// What the deciders said about an entry, on the show page: who, what a screen shows, what the
/// decider said, its confidence and its reason. It says it is not a decision; Accept and Reject
/// below never read it.
fn suggestion_block(suggestions: &[Suggestion]) -> String {
    if suggestions.is_empty() {
        return format!(
            "<div class=\"card\"><p class=\"dim\">Suggestion: {}</p></div>",
            inbox_view::NO_SUGGESTION
        );
    }
    let mut out = String::new();
    for s in suggestions {
        out.push_str(&format!(
            "<div class=\"card\"><p class=\"dim\">Suggestion (from <code>{}</code>, not a decision):</p><p><span class=\"badge\">{}</span> · said {} · confidence {}</p><p>Reason: {}</p></div>",
            escape(&s.decider),
            escape(s.shown().label()),
            escape(s.verdict.as_str()),
            escape(&inbox_view::confidence_words(s.confidence)),
            escape(&s.reason)
        ));
    }
    out
}

/// One entry: the sender's claim, the submission next to what is stored now, and the forms.
/// `sub` is the stored submission and `target` what [`current_target`] says; an error in either
/// leaves only Reject. A row that is not pending is shown read-only.
pub fn show(
    entry: &BufferEntry,
    sub: &Result<Submission, String>,
    target: &Result<Option<Stored>, String>,
    suggestions: &[Suggestion],
    now: i64,
    view: &View<'_>,
) -> String {
    let mut body = banner(view);
    body.push_str(&format!(
        "<p>{} · for {} · <span class=\"badge\">{}</span></p>",
        what(entry),
        escape(&scope_words(&entry.project)),
        escape(entry.status.as_str())
    ));
    body.push_str(&format!(
        "<div class=\"card\"><p class=\"dim\">What the sender says about itself (not checked):</p><p>From: <strong>{}</strong> · {} ({})</p><p>Why:</p><pre>{}</pre></div>",
        escape(&entry.source),
        escape(&inbox_view::age(entry.created_at, now)),
        time(entry.created_at),
        escape(&entry.why)
    ));
    body.push_str(&suggestion_block(suggestions));
    let pending = entry.status == Status::Pending;
    if let Some(at) = entry.decided_at {
        body.push_str(&format!(
            "<p>Decided {}.{}</p>",
            time(at),
            if entry.decision_note.is_empty() {
                String::new()
            } else {
                format!(" Note: {}", escape(&entry.decision_note))
            }
        ));
    } else if !entry.decision_note.is_empty() {
        body.push_str(&format!(
            "<p class=\"error\" role=\"alert\">The last try to apply it failed: {}</p>",
            escape(&entry.decision_note)
        ));
    }
    let mut expected = None;
    let mut can_accept = false;
    match (sub, target) {
        (Err(why), _) => body.push_str(&format!(
            "<p class=\"error\" role=\"alert\">{}</p>",
            escape(why)
        )),
        (Ok(_), Err(why)) => body.push_str(&format!(
            "<p class=\"error\" role=\"alert\">This cannot be accepted: {}</p>",
            escape(why)
        )),
        (Ok(sub), Ok(stored)) => {
            if pending && stored.as_ref().is_some_and(|s| s.on) {
                body.push_str(&format!(
                    "<p class=\"error\" role=\"alert\">{}</p>",
                    escape(ON_BANNER)
                ));
            }
            body.push_str(&comparison(sub, stored.as_ref()));
            expected = stored.as_ref().map(|s| s.fingerprint.clone());
            can_accept = true;
        }
    }
    if pending {
        body.push_str("<div class=\"actions\">");
        if can_accept {
            body.push_str(&format!(
                "<form class=\"inline\" method=\"post\" action=\"/inbox/accept\">{}{}{}<button>Accept</button></form>",
                hidden("token", view.token),
                hidden("id", &entry.id.to_string()),
                hidden("expected", expected.as_deref().unwrap_or(""))
            ));
        }
        body.push_str("<a href=\"/inbox\">Back</a></div>");
        body.push_str(&format!(
            "<form method=\"post\" action=\"/inbox/reject\">{}{}{}<div class=\"actions\"><button class=\"danger\">Reject</button></div></form>",
            hidden("token", view.token),
            hidden("id", &entry.id.to_string()),
            text_field(
                "Note for the trail (optional)",
                "note",
                "",
                inbox_view::MAX_NOTE_CHARS,
                false
            )
        ));
    } else {
        body.push_str("<p><a href=\"/inbox\">Back to the inbox</a></p>");
    }
    page("/inbox", &format!("Inbox #{}", entry.id), &body)
}

/// The entry `id` as a page, read from `store` now; `None` when there is no such row.
pub fn show_page(store: &Store, id: i64, view: &View<'_>) -> Result<Option<String>, String> {
    let Some(entry) = store.buffer_entry(id).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let sub = entry.submission();
    let target = match &sub {
        Ok(sub) => current_target(store, sub),
        Err(_) => Ok(None),
    };
    let suggestions = store.list_suggestions(id).map_err(|e| e.to_string())?;
    Ok(Some(show(
        &entry,
        &sub,
        &target,
        &suggestions,
        crate::store::now_secs(),
        view,
    )))
}

/// The page after an accept that was applied (or found nothing to change).
pub fn accepted(id: i64, text: &str) -> String {
    page(
        "/inbox",
        &format!("Accepted #{id}"),
        &format!(
            "<p class=\"notice\" role=\"status\">{}</p><p><a href=\"/inbox\">Back to the inbox</a></p>",
            escape(text)
        ),
    )
}

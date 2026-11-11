//! `/inbox` (issue 47): what other harnesses have submitted through the MCP server's buffer mode,
//! for the owner to look at, accept or reject.
//!
//! ```text
//! /inbox                  list what waits, oldest first
//! /inbox show N           the submission next to what is stored now, with a line diff
//! /inbox accept N         show the same, ask, then apply it
//! /inbox reject N [note]  reject it, with an optional reason
//! ```
//!
//! No model is involved in deciding. A decider's suggestion (issue 48) is printed next to an entry
//! (`none` when there is no suggestion) and is never read by accept or reject. A submission is applied by `store_buffer::apply_entry` with the stamp of
//! the stored item as it was shown here, so an item that changed meanwhile is reported and not
//! overwritten. Everything a sender wrote is printed through `inbox_view::printable`: a terminal
//! control sequence in it is dropped, never run.

use std::io::{BufRead, Write};

use crate::agent::Agent;
use crate::config_ui::{done, error};
use crate::inbox_view::{self, DiffLine, line_diff, printable, scope_words};
use crate::prompts_ui::with_store;
use crate::repl::{Shared, say_line};
use crate::store::now_secs;
use crate::store_buffer::{
    ApplyOutcome, BufferEntry, Status, Stored, Submission, Suggestion, apply_entry, current_target,
    reject,
};
use crate::web_inbox::ON_BANNER;

const USAGE: &str = "usage: /inbox [show N | accept N | reject N [note]]";

/// How much of a `why` a list line shows.
const WHY_CHARS: usize = 80;

pub fn command<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, rest: &str) {
    let style = term.borrow().style();
    let (word, tail) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, ""), |(word, tail)| (word, tail.trim()));
    match word {
        "" | "list" if tail.is_empty() => list(agent, term),
        "show" | "accept" | "reject" => {
            let (number, note) = tail
                .split_once(char::is_whitespace)
                .map_or((tail, ""), |(number, note)| (number, note.trim()));
            let Ok(id) = number.trim_start_matches('#').parse::<i64>() else {
                return say_line(
                    term,
                    &style.yellow(&format!("usage: /inbox {word} N{}", note_usage(word))),
                );
            };
            if word != "reject" && !note.is_empty() {
                return say_line(term, &style.yellow(&format!("usage: /inbox {word} N")));
            }
            match word {
                "show" => show(agent, term, id),
                "accept" => accept(agent, term, id),
                _ => reject_entry(agent, term, id, note),
            }
        }
        _ => say_line(term, &style.yellow(USAGE)),
    }
}

fn note_usage(word: &str) -> &'static str {
    if word == "reject" { " [note]" } else { "" }
}

fn list<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
    let style = term.borrow().style();
    let Some((pending, suggestions)) = with_store(agent, term, |s| {
        let pending = s.list_pending()?;
        let suggestions = s.suggestions_for(pending.iter().map(|e| e.id))?;
        Ok((pending, suggestions))
    }) else {
        return;
    };
    if pending.is_empty() {
        return say_line(
            term,
            &style.dim("the inbox is empty: nothing waits for your decision"),
        );
    }
    let now = now_secs();
    for e in &pending {
        say_line(
            term,
            &format!(
                "{}  {} {} {} · {} · from {} (claimed) · {}",
                style.bold(&format!("#{}", e.id)),
                printable(&e.kind),
                printable(&e.action),
                style.bold(&printable(&e.name)),
                printable(&scope_words(&e.project)),
                printable(&e.source),
                inbox_view::age(e.created_at, now)
            ),
        );
        let mut why = printable(&inbox_view::first_line(&e.why, WHY_CHARS));
        why = why.replace('\n', " ");
        say_line(term, &style.dim(&format!("    why: {why}")));
        let suggested = match suggestions.get(&e.id).filter(|list| !list.is_empty()) {
            None => inbox_view::NO_SUGGESTION.to_owned(),
            Some(list) => list
                .iter()
                .map(inbox_view::suggestion_short)
                .collect::<Vec<_>>()
                .join("; "),
        };
        say_line(
            term,
            &style.dim(&format!("    suggestion: {}", printable(&suggested))),
        );
        if !e.decision_note.is_empty() {
            say_line(
                term,
                &style.yellow(&format!(
                    "    last try failed: {}",
                    printable(&e.decision_note)
                )),
            );
        }
    }
    say_line(
        term,
        &style.dim(&format!(
            "{} waiting: /inbox show N, /inbox accept N, /inbox reject N [note]",
            pending.len()
        )),
    );
}

/// Why an entry cannot be looked at: no such row, or its submission or target cannot be read.
struct Loaded {
    entry: BufferEntry,
    sub: Result<Submission, String>,
    target: Result<Option<Stored>, String>,
    suggestions: Vec<Suggestion>,
}

fn load<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, id: i64) -> Option<Loaded> {
    let found = with_store(agent, term, |s| {
        let Some(entry) = s.buffer_entry(id)? else {
            return Ok(None);
        };
        let sub = entry.submission();
        let target = match &sub {
            Ok(sub) => current_target(s, sub),
            Err(_) => Ok(None),
        };
        let suggestions = s.list_suggestions(id)?;
        Ok(Some(Loaded {
            entry,
            sub,
            target,
            suggestions,
        }))
    })?;
    if found.is_none() {
        error(
            term,
            &format!("there is no submission #{id}: /inbox lists them"),
        );
    }
    found
}

fn show<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, id: i64) {
    if let Some(loaded) = load(agent, term, id) {
        print_entry(term, &loaded);
    }
}

/// The text lines of one stored field, plain.
fn field_line(label: &str, stored: Option<&Stored>, submitted: &str) -> String {
    let now = match (label, stored) {
        (_, None) => "(nothing stored)".to_owned(),
        ("kind", Some(s)) => s.context_kind.clone(),
        ("area", Some(s)) => s.area.clone(),
        ("quote", Some(s)) => s.quote.clone(),
        ("type", Some(s)) => s.context_kind.clone(),
        ("quotes", Some(s)) => inbox_view::quotes_words(&s.quotes),
        (_, Some(s)) => s.description.clone(),
    };
    format!(
        "  {label}: {}  ->  {}",
        printable(if now.is_empty() { "(empty)" } else { &now }),
        printable(submitted)
    )
}

/// What the deciders said about an entry, or `none`. Printed as plain text: a model wrote it.
fn print_suggestions<R: BufRead, W: Write>(term: &Shared<R, W>, suggestions: &[Suggestion]) {
    let style = term.borrow().style();
    if suggestions.is_empty() {
        return say_line(
            term,
            &style.dim(&format!("suggestion: {}", inbox_view::NO_SUGGESTION)),
        );
    }
    for s in suggestions {
        say_line(
            term,
            &style.dim(&format!(
                "suggestion (from {}, not a decision): {}",
                printable(&s.decider),
                printable(&inbox_view::suggestion_short(s))
            )),
        );
        say_line(term, &format!("  reason: {}", printable(&s.reason)));
    }
}

fn print_entry<R: BufRead, W: Write>(term: &Shared<R, W>, loaded: &Loaded) {
    let style = term.borrow().style();
    let e = &loaded.entry;
    let now = now_secs();
    say_line(
        term,
        &style.bold(&format!(
            "#{} {} {} '{}' · {} · {}",
            e.id,
            printable(&e.kind),
            printable(&e.action),
            printable(&e.name),
            printable(&scope_words(&e.project)),
            e.status.as_str()
        )),
    );
    say_line(
        term,
        &style.dim(&format!(
            "from {} (as the sender claims, not checked) · {}",
            printable(&e.source),
            inbox_view::age(e.created_at, now)
        )),
    );
    say_line(term, &style.dim("why (the sender's words):"));
    say_line(term, &printable(&e.why));
    print_suggestions(term, &loaded.suggestions);
    if !e.decision_note.is_empty() {
        let label = if e.status == Status::Pending {
            "the last try to apply it failed"
        } else {
            "note"
        };
        say_line(
            term,
            &style.yellow(&format!("{label}: {}", printable(&e.decision_note))),
        );
    }
    let sub = match &loaded.sub {
        Ok(sub) => sub,
        Err(why) => return error(term, why),
    };
    let stored = match &loaded.target {
        Ok(stored) => stored.as_ref(),
        Err(why) => {
            return error(
                term,
                &format!("this cannot be accepted: {why}; /inbox reject N"),
            );
        }
    };
    if e.status == Status::Pending && stored.is_some_and(|s| s.on) {
        say_line(term, &style.yellow(&format!("! {ON_BANNER}")));
    }
    if sub.action == "delete" {
        match stored {
            None => say_line(
                term,
                "nothing is stored under this name: accepting only marks it applied",
            ),
            Some(s) => {
                say_line(term, &style.bold("accepting deletes this:"));
                say_line(term, &printable(&s.text));
            }
        }
        return;
    }
    match stored {
        None => {
            let off = if matches!(sub.kind.as_str(), "context" | "glossary" | "gotcha") {
                ""
            } else {
                "; a new item is saved switched off"
            };
            say_line(
                term,
                &style.bold(&format!(
                    "nothing is stored under this name: accepting adds it{off}"
                )),
            );
        }
        Some(_) => say_line(term, &style.bold("accepting replaces the stored item:")),
    }
    for (label, submitted) in inbox_view::submitted_fields(sub) {
        say_line(term, &field_line(label, stored, &submitted));
    }
    say_line(term, &style.dim("text (- stored now, + submitted):"));
    if stored.is_some_and(|s| s.text == sub.text) {
        say_line(term, &style.dim("  (the text is the same)"));
    }
    for line in line_diff(stored.map_or("", |s| s.text.as_str()), &sub.text) {
        let marked = printable(&line.marked());
        say_line(
            term,
            &match line {
                DiffLine::Same(_) => marked,
                DiffLine::Removed(_) => style.red(&marked),
                DiffLine::Added(_) => style.green(&marked),
            },
        );
    }
}

fn accept<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, id: i64) {
    let style = term.borrow().style();
    let Some(loaded) = load(agent, term, id) else {
        return;
    };
    if loaded.entry.status != Status::Pending {
        return error(
            term,
            &format!("#{id} is {} already", loaded.entry.status.as_str()),
        );
    }
    if loaded.sub.is_err() || loaded.target.is_err() {
        print_entry(term, &loaded);
        return say_line(
            term,
            &style.yellow(&format!(
                "[nothing accepted] /inbox reject {id} [note] is all that is left"
            )),
        );
    }
    print_entry(term, &loaded);
    // The stamp of the stored item as it was just shown: what apply_entry compares.
    let expected = loaded
        .target
        .as_ref()
        .ok()
        .and_then(|t| t.as_ref())
        .map(|s| s.fingerprint.clone());
    let sure = term.borrow_mut().ask(&format!("accept #{id}? [y/N]"), None);
    if !matches!(
        sure.as_deref().map(str::to_ascii_lowercase).as_deref(),
        Some("y" | "yes")
    ) {
        return say_line(term, &style.yellow("[not accepted]"));
    }
    apply_shown(agent, term, id, expected.as_deref());
}

/// Applies entry `id` with `expected`, the stamp of the stored item as it was shown, and says what
/// happened.
pub(crate) fn apply_shown<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    id: i64,
    expected: Option<&str>,
) {
    let style = term.borrow().style();
    let Some(outcome) = with_store(agent, term, |s| apply_entry(s, id, expected)) else {
        return;
    };
    match outcome {
        ApplyOutcome::Applied(text) => done(term, &printable(&text)),
        ApplyOutcome::NoChange => done(
            term,
            "nothing to change: it is stored already; the entry is marked applied",
        ),
        ApplyOutcome::Stale => say_line(
            term,
            &style.yellow(&format!(
                "the stored item changed while you were reading; check it again: /inbox show {id}"
            )),
        ),
        ApplyOutcome::Failed(why) => error(term, &printable(&why)),
    }
}

fn reject_entry<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, id: i64, note: &str) {
    let note = match inbox_view::check_note(note) {
        Ok(note) => note,
        Err(why) => return error(term, &why),
    };
    if with_store(agent, term, |s| reject(s, id, &note)).is_some() {
        done(term, &format!("rejected #{id}"));
    }
}

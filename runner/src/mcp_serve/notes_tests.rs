//! Tests of the gotchas, the work log and the shape quotes in `--mcp-serve` (issue 50). Every test
//! works on a temporary database made by `seeded`; none opens the real `harness.db`.

use std::path::Path;

use serde_json::{Value, json};

use super::submit_tests::{bcall_in, bsession};
use super::tests::{PROJECT, call, request, seeded, session};
use super::write_tests::dcall_in;
use super::*;
use crate::store_buffer::{self, ApplyOutcome};

fn read(path: &Path, tool: &str, arguments: Value) -> (String, bool) {
    call(path, PROJECT, tool, arguments)
}

fn buffer(path: &Path, tool: &str, arguments: Value) -> (String, bool) {
    bcall_in(path, PROJECT, tool, arguments)
}

fn count(path: &Path, table: &str) -> i64 {
    Store::open_read_only(path)
        .unwrap()
        .conn
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn log_args(tag: &str, title: &str) -> Value {
    json!({"tag": tag, "title": title, "summary": "What happened and why.", "source": "claude-code"})
}

fn submit_gotcha(path: &Path, name: &str) -> (String, bool) {
    buffer(
        path,
        "submit",
        json!({"kind": "gotcha", "action": "set", "name": name, "gotcha_type": "hazard",
               "text": "IMMEDIATE once two connections write.", "why": "it bit us", "source": "claude-code"}),
    )
}

fn accept(path: &Path, id: i64) -> ApplyOutcome {
    let mut store = Store::open_for_writing(path).unwrap();
    let row = store.buffer_entry(id).unwrap().unwrap();
    let sub = row.submission().unwrap();
    let expected = store_buffer::current_target(&store, &sub)
        .unwrap()
        .map(|s| s.fingerprint);
    store_buffer::apply_entry(&mut store, id, expected.as_deref()).unwrap()
}

fn queued_id(text: &str) -> i64 {
    text.strip_prefix("queued as #")
        .unwrap_or_else(|| panic!("{text}"))
        .parse()
        .unwrap()
}

// ---- reads ----

#[test]
fn gotchas_list_and_get_read_the_projects_gotchas_and_say_so_when_there_are_none() {
    let (_dir, path) = seeded("notes-gotchas");
    assert_eq!(
        read(&path, "gotchas_list", json!({})),
        ("no gotchas".to_owned(), false)
    );
    {
        let mut store = Store::open_for_writing(&path).unwrap();
        store
            .set_gotcha_in(PROJECT, "wal", "hazard", "Use IMMEDIATE.", "")
            .unwrap();
        store
            .set_gotcha_in("", "shared", "tooling", "For all.", "")
            .unwrap();
        store
            .set_gotcha_in("/other", "theirs", "trap", "Elsewhere.", "")
            .unwrap();
    }
    let (list, failed) = read(&path, "gotchas_list", json!({}));
    assert!(!failed);
    assert_eq!(list, "shared (tooling)\nwal (hazard)");
    assert_eq!(
        read(&path, "gotchas_list", json!({"type": "hazard"})).0,
        "wal (hazard)"
    );
    assert_eq!(
        read(&path, "gotchas_list", json!({"project": "/other"})).0,
        "shared (tooling)\ntheirs (trap)"
    );
    let (bad, failed) = read(&path, "gotchas_list", json!({"type": "shape"}));
    assert!(failed && bad.contains("a gotcha type is one of"), "{bad}");
    assert_eq!(
        read(&path, "gotchas_get", json!({"name": "WAL"})),
        ("wal (hazard)\nUse IMMEDIATE.".to_owned(), false)
    );
    let (miss, failed) = read(&path, "gotchas_get", json!({"name": "nope"}));
    assert!(
        failed && miss.contains("the gotchas are: shared, wal"),
        "{miss}"
    );
    let (none, failed) = read(&path, "gotchas_get", json!({}));
    assert!(failed && none.contains("give 'name'"), "{none}");
}

#[test]
fn logs_list_and_get_show_newest_first_and_check_the_arguments() {
    let (_dir, path) = seeded("notes-logs-read");
    assert_eq!(read(&path, "logs_list", json!({})).0, "no log entries");
    {
        let mut store = Store::open_for_writing(&path).unwrap();
        store
            .add_log(
                PROJECT,
                "STATUS",
                "first",
                "Did the first.",
                "claude",
                Some(4),
            )
            .unwrap();
        store
            .add_log(PROJECT, "TODO", "second", "Do the second.", "", None)
            .unwrap();
        store
            .add_log("/other", "NOTE", "theirs", "Not here.", "", None)
            .unwrap();
    }
    let (list, failed) = read(&path, "logs_list", json!({}));
    assert!(!failed);
    let lines: Vec<&str> = list.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(
        lines[0].starts_with("#2 ") && lines[0].ends_with(" TODO second"),
        "{list}"
    );
    assert!(
        lines[1].starts_with("#1 ") && lines[1].ends_with(" STATUS first"),
        "{list}"
    );
    assert_eq!(
        read(&path, "logs_list", json!({"limit": 1}))
            .0
            .lines()
            .count(),
        1
    );
    assert!(
        read(&path, "logs_list", json!({"tag": "status"}))
            .0
            .ends_with("STATUS first")
    );
    for bad in [
        json!({"limit": 0}),
        json!({"limit": 101}),
        json!({"limit": "ten"}),
        json!({"tag": "ERROR"}),
    ] {
        assert!(read(&path, "logs_list", bad.clone()).1, "{bad}");
    }
    let (one, failed) = read(&path, "logs_get", json!({"id": 1}));
    assert!(!failed);
    assert!(
        one.contains("STATUS first\nDid the first.\nwritten by claude\nabout chat 4"),
        "{one}"
    );
    assert!(
        read(&path, "logs_get", json!({"id": "#2"}))
            .0
            .contains("Do the second.")
    );
    assert!(
        read(&path, "logs_get", json!({"id": 3})).1,
        "another project's entry is not found"
    );
    assert!(read(&path, "logs_get", json!({"id": 99})).1);
    assert!(read(&path, "logs_get", json!({})).1);
    assert!(read(&path, "logs_get", json!({"id": "x"})).1);
}

#[test]
fn context_get_prints_the_quotes_of_a_shape() {
    let (_dir, path) = seeded("notes-quotes-read");
    {
        let mut store = Store::open_for_writing(&path).unwrap();
        let mut reversal = crate::store_context::Quote {
            quote: "no, the PDF wins now".to_owned(),
            said_on: "2026-11-09".to_owned(),
            claim: "the PDF wins".to_owned(),
            replaces: "the app wins".to_owned(),
        };
        store
            .set_context_with_quotes(
                PROJECT,
                "orders",
                "shape",
                "The PDF is right.",
                "",
                Some("orders"),
                &[
                    crate::store_context::Quote {
                        quote: "the app is always right".to_owned(),
                        said_on: "2026-11-05".to_owned(),
                        claim: "the app wins".to_owned(),
                        replaces: String::new(),
                    },
                    std::mem::take(&mut reversal),
                ],
            )
            .unwrap();
    }
    let (text, failed) = read(&path, "context_get", json!({"name": "orders"}));
    assert!(!failed);
    assert_eq!(
        text,
        "orders (shape, area orders)\nThe PDF is right.\n\
         the owner said on 2026-11-05: the app is always right (supports: the app wins)\n\
         the owner said on 2026-11-09: no, the PDF wins now (supports: the PDF wins) (replaces: the app wins)"
    );
    // An entry without quotes reads as before.
    assert_eq!(
        read(&path, "context_get", json!({"name": "style"})).0,
        "style (rule)\nApp style: short lines."
    );
}

// ---- log_add ----

#[test]
fn log_add_appends_at_once_in_the_buffer_mode_and_touches_nothing_else() {
    let (_dir, path) = seeded("notes-log-add");
    let curated_before = format!(
        "{:?}",
        Store::open_read_only(&path)
            .unwrap()
            .list_all_context()
            .unwrap()
    );
    let inbox = count(&path, "buffer_entries");
    let (text, failed) = buffer(&path, "log_add", log_args("status", "Built the stores"));
    assert_eq!((text.as_str(), failed), ("logged as #1", false));
    assert_eq!(
        buffer(&path, "log_add", log_args("NOTE", "Second")).0,
        "logged as #2"
    );
    assert_eq!(count(&path, "work_logs"), 2);
    assert_eq!(count(&path, "buffer_entries"), inbox, "nothing was queued");
    assert_eq!(
        format!(
            "{:?}",
            Store::open_read_only(&path)
                .unwrap()
                .list_all_context()
                .unwrap()
        ),
        curated_before
    );
    let log = Store::open_read_only(&path)
        .unwrap()
        .log(PROJECT, 1)
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            log.tag.as_str(),
            log.title.as_str(),
            log.source.as_str(),
            log.project.as_str()
        ),
        ("STATUS", "Built the stores", "claude-code", PROJECT)
    );
    // The project can be named, and the entry is for that project only.
    let mut other = log_args("NOTE", "Elsewhere");
    other["project"] = json!("/other");
    assert_eq!(buffer(&path, "log_add", other).0, "logged as #3");
    assert!(read(&path, "logs_list", json!({})).0.lines().count() == 2);
}

#[test]
fn log_add_refuses_what_the_store_refuses_and_a_missing_source() {
    let (_dir, path) = seeded("notes-log-refuse");
    for (arguments, why) in [
        (log_args("ERROR", "t"), "a tag is one of"),
        (log_args("NOTE", &"t".repeat(101)), "a title is at most 100"),
        (
            json!({"tag": "NOTE", "title": "t", "summary": "s"}),
            "give 'source'",
        ),
        (
            json!({"tag": "NOTE", "title": "t", "summary": "s", "source": "a\nb"}),
            "one line",
        ),
        (
            json!({"tag": "NOTE", "title": "t", "summary": "s", "source": "x", "session": "four"}),
            "chat's number",
        ),
        (
            json!({"tag": "NOTE", "title": "t", "source": "x"}),
            "give 'summary'",
        ),
    ] {
        let (text, failed) = buffer(&path, "log_add", arguments.clone());
        assert!(failed && text.contains(why), "{arguments}: {text}");
    }
    assert_eq!(count(&path, "work_logs"), 0, "a refusal wrote nothing");
}

#[test]
fn log_add_stops_at_the_cap_with_a_fixed_answer() {
    let (_dir, path) = seeded("notes-log-cap");
    {
        let mut store = Store::open_for_writing(&path).unwrap();
        for n in 0..crate::store_logs::MAX_PER_PROJECT {
            store
                .add_log(PROJECT, "NOTE", &format!("n{n}"), "s", "", None)
                .unwrap();
        }
    }
    assert_eq!(
        buffer(&path, "log_add", log_args("NOTE", "one too many")),
        (
            "error: the log is full: ask the owner to archive it".to_owned(),
            true
        )
    );
    let mut other = log_args("NOTE", "another project");
    other["project"] = json!("/other");
    assert_eq!(buffer(&path, "log_add", other).0, "logged as #201");
}

#[test]
fn log_add_is_an_unknown_tool_in_the_read_only_mode_and_works_in_the_direct_mode() {
    let (_dir, path) = seeded("notes-log-modes");
    let reply = session(
        &path,
        PROJECT,
        &[request(
            1,
            "tools/call",
            json!({"name": "log_add", "arguments": log_args("NOTE", "t")}),
        )],
    );
    assert_eq!(reply[0]["error"]["code"], INVALID_PARAMS);
    assert_eq!(reply[0]["error"]["message"], "unknown tool 'log_add'");
    assert_eq!(count(&path, "work_logs"), 0);
    assert_eq!(
        dcall_in(&path, PROJECT, "log_add", log_args("NOTE", "t")).0,
        "logged as #1"
    );
}

// ---- gotchas through submit and the direct tools ----

#[test]
fn a_gotcha_is_queued_changes_nothing_until_accepted_and_then_reads_back() {
    let (_dir, path) = seeded("notes-gotcha-submit");
    let (text, failed) = submit_gotcha(&path, "wal-immediate");
    assert!(!failed, "{text}");
    let id = queued_id(&text);
    assert_eq!(
        count(&path, "gotchas"),
        0,
        "nothing is stored until the owner accepts"
    );
    assert_eq!(
        submit_gotcha(&path, "wal-immediate").0,
        format!("already queued as #{id}")
    );
    let row = Store::open_read_only(&path)
        .unwrap()
        .buffer_entry(id)
        .unwrap()
        .unwrap();
    assert_eq!(
        (row.kind.as_str(), row.project.as_str()),
        ("gotcha", PROJECT)
    );
    assert!(matches!(accept(&path, id), ApplyOutcome::Applied(_)));
    assert_eq!(
        read(&path, "gotchas_get", json!({"name": "wal-immediate"})).0,
        "wal-immediate (hazard)\nIMMEDIATE once two connections write."
    );
    assert_eq!(
        Store::open_read_only(&path)
            .unwrap()
            .list_all_gotchas()
            .unwrap()[0]
            .source,
        "claude-code",
        "the sender's name is kept as claimed"
    );
    // A delete is queued the same way and removes it when accepted.
    let (text, _) = buffer(
        &path,
        "submit",
        json!({"kind": "gotcha", "action": "delete", "name": "wal-immediate", "why": "gone", "source": "claude-code"}),
    );
    let delete = queued_id(&text);
    assert_eq!(count(&path, "gotchas"), 1);
    assert!(matches!(accept(&path, delete), ApplyOutcome::Applied(_)));
    assert_eq!(count(&path, "gotchas"), 0);
}

#[test]
fn a_gotcha_submission_is_checked_before_it_is_queued() {
    let (_dir, path) = seeded("notes-gotcha-refuse");
    let base = |extra: Value| {
        let mut args = json!({"kind": "gotcha", "action": "set", "name": "g", "gotcha_type": "hazard",
                              "text": "t", "why": "w", "source": "claude-code"});
        for (k, v) in extra.as_object().unwrap() {
            args[k] = v.clone();
        }
        args
    };
    for (arguments, why) in [
        (
            base(json!({"gotcha_type": "shape"})),
            "a gotcha type is one of",
        ),
        (base(json!({"gotcha_type": ""})), "a gotcha type is one of"),
        (base(json!({"text": ""})), "the text is empty"),
        (
            base(json!({"text": "t".repeat(2_001)})),
            "at most 2000 bytes",
        ),
        (base(json!({"name": "has space"})), "letters, digits"),
        (
            base(json!({"quote": "words"})),
            "'quote' does not apply to a gotcha",
        ),
        (
            base(json!({"context_kind": "rule"})),
            "'context_kind' does not apply to a gotcha",
        ),
        (
            base(json!({"text": "hidden\u{200B}"})),
            "a character a reviewer cannot see",
        ),
        (
            base(json!({"action": "delete", "gotcha_type": "hazard"})),
            "a delete takes no gotcha_type",
        ),
    ] {
        let (text, failed) = buffer(&path, "submit", arguments.clone());
        assert!(failed && text.contains(why), "{arguments}: {text}");
    }
    assert_eq!(count(&path, "buffer_entries"), 0);
}

#[test]
fn a_log_cannot_be_submitted_it_is_appended_with_log_add() {
    let (_dir, path) = seeded("notes-log-not-submittable");
    let (text, failed) = buffer(
        &path,
        "submit",
        json!({"kind": "log", "action": "set", "name": "x", "text": "t", "why": "w", "source": "s"}),
    );
    assert!(failed && text.contains("a kind is one of"), "{text}");
    assert_eq!(count(&path, "buffer_entries"), 0);
}

#[test]
fn the_direct_mode_sets_and_deletes_gotchas_with_the_stores_checks() {
    let (_dir, path) = seeded("notes-gotcha-direct");
    let args =
        json!({"name": "wal", "type": "hazard", "text": "Use IMMEDIATE.", "source": "claude-code"});
    assert_eq!(
        dcall_in(&path, PROJECT, "gotcha_set", args.clone()).0,
        format!("added gotcha 'wal' for project {PROJECT}")
    );
    assert!(
        dcall_in(&path, PROJECT, "gotcha_set", args.clone())
            .0
            .starts_with("no change:")
    );
    let mut changed = args.clone();
    changed["text"] = json!("Use IMMEDIATE always.");
    assert!(
        dcall_in(&path, PROJECT, "gotcha_set", changed)
            .0
            .starts_with("replaced gotcha")
    );
    let mut bad = args.clone();
    bad["type"] = json!("shape");
    assert!(dcall_in(&path, PROJECT, "gotcha_set", bad).1);
    assert_eq!(count(&path, "gotchas"), 1);
    assert_eq!(
        dcall_in(&path, PROJECT, "gotcha_delete", json!({"name": "wal"})).0,
        format!("deleted gotcha 'wal' for project {PROJECT}")
    );
    assert!(dcall_in(&path, PROJECT, "gotcha_delete", json!({"name": "wal"})).1);
}

#[test]
fn gotcha_tools_are_not_offered_in_the_buffer_mode() {
    let (_dir, path) = seeded("notes-gotcha-modes");
    for tool in ["gotcha_set", "gotcha_delete"] {
        let reply = bsession(
            &path,
            PROJECT,
            &[request(
                1,
                "tools/call",
                json!({"name": tool, "arguments": {"name": "x", "type": "hazard", "text": "t"}}),
            )],
        );
        assert_eq!(reply[0]["error"]["code"], INVALID_PARAMS, "{tool}");
    }
    assert_eq!(count(&path, "gotchas"), 0);
}

// ---- shape through submit ----

fn shape_args(extra: Value) -> Value {
    let mut args = json!({"kind": "context", "action": "set", "name": "orders", "context_kind": "shape",
                          "text": "The app is always right.", "area": "orders", "why": "the owner said so",
                          "source": "claude-code"});
    for (k, v) in extra.as_object().unwrap() {
        args[k] = v.clone();
    }
    args
}

fn one_quote() -> Value {
    json!([{"quote": "the app is always right", "said_on": "2026-11-05", "claim": "the app wins"}])
}

#[test]
fn a_shape_with_quotes_is_queued_and_applies_text_and_quotes_together() {
    let (_dir, path) = seeded("notes-shape-submit");
    let (text, failed) = buffer(&path, "submit", shape_args(json!({"quotes": one_quote()})));
    assert!(!failed, "{text}");
    let id = queued_id(&text);
    assert_eq!(
        count(&path, "context_quotes"),
        0,
        "nothing is stored until the owner accepts"
    );
    let again = buffer(&path, "submit", shape_args(json!({"quotes": one_quote()}))).0;
    assert_eq!(again, format!("already queued as #{id}"));
    let different = json!([{"quote": "other words", "said_on": "2026-11-05"}]);
    assert!(
        buffer(&path, "submit", shape_args(json!({"quotes": different})))
            .0
            .starts_with("queued as #"),
        "other quotes are another entry"
    );
    assert!(matches!(accept(&path, id), ApplyOutcome::Applied(_)));
    assert_eq!(count(&path, "context_quotes"), 1);
    assert_eq!(
        read(&path, "context_get", json!({"name": "orders"})).0,
        "orders (shape, area orders)\nThe app is always right.\n\
         the owner said on 2026-11-05: the app is always right (supports: the app wins)"
    );
}

#[test]
fn a_shape_needs_the_owners_words_or_a_mechanism_section_and_only_a_shape_has_quotes() {
    let (_dir, path) = seeded("notes-shape-refuse");
    for (arguments, why) in [
        (shape_args(json!({})), "a shape needs the owner's words"),
        (
            shape_args(json!({"quotes": []})),
            "a shape needs the owner's words",
        ),
        (
            shape_args(json!({"context_kind": "rule", "quotes": one_quote()})),
            "only a shape has quotes",
        ),
        (
            shape_args(json!({"quotes": [{"quote": "w", "said_on": "yesterday"}]})),
            "YYYY-MM-DD",
        ),
        (
            shape_args(json!({"quotes": [{"quote": "", "said_on": "2026-11-05"}]})),
            "a quote is empty",
        ),
        (
            shape_args(json!({"quotes": [{"said_on": "2026-11-05"}]})),
            "a quote is empty",
        ),
        (
            shape_args(json!({"quotes": "the owner said"})),
            "'quotes' must be a list",
        ),
        (shape_args(json!({"quotes": ["words"]})), "each of 'quotes'"),
        (
            shape_args(json!({"quotes": [{"quote": "w\u{202E}x", "said_on": "2026-11-05"}]})),
            "a character a reviewer cannot see",
        ),
        (
            json!({"kind": "glossary", "action": "set", "name": "w", "text": "t", "quotes": one_quote(),
                   "why": "w", "source": "s"}),
            "'quotes' does not apply to a glossary",
        ),
        (
            json!({"kind": "context", "action": "delete", "name": "orders", "quotes": one_quote(),
                   "why": "w", "source": "s"}),
            "a delete takes no quotes",
        ),
    ] {
        let (text, failed) = buffer(&path, "submit", arguments.clone());
        assert!(failed && text.contains(why), "{arguments}: {text}");
    }
    assert_eq!(count(&path, "buffer_entries"), 0, "a refusal added no row");
    // A mechanism section is evidence from the system, and so is a plain quote.
    let by_pointer = shape_args(
        json!({"text": "The flow is X.\n\n## Where the mechanism lives\nsrc/orders.rs"}),
    );
    assert!(
        buffer(&path, "submit", by_pointer)
            .0
            .starts_with("queued as #")
    );
    let by_quote = shape_args(json!({"name": "billing", "quote": "the owner said"}));
    assert!(
        buffer(&path, "submit", by_quote)
            .0
            .starts_with("queued as #")
    );
}

#[test]
fn a_reversal_accepted_keeps_the_old_quote_and_a_stale_target_still_stops_it() {
    let (_dir, path) = seeded("notes-shape-reversal");
    let first = queued_id(&buffer(&path, "submit", shape_args(json!({"quotes": one_quote()}))).0);
    assert!(matches!(accept(&path, first), ApplyOutcome::Applied(_)));
    let reversal = shape_args(json!({
        "text": "The PDF is right.",
        "quotes": [{"quote": "no, the PDF wins now", "said_on": "2026-11-09",
                    "claim": "the PDF wins", "replaces": "the app wins"}]
    }));
    let second = queued_id(&buffer(&path, "submit", reversal).0);
    assert!(matches!(accept(&path, second), ApplyOutcome::Applied(_)));
    let text = read(&path, "context_get", json!({"name": "orders"})).0;
    assert!(text.contains("The PDF is right."), "{text}");
    assert!(
        text.contains("the owner said on 2026-11-05: the app is always right"),
        "{text}"
    );
    assert!(text.contains("(replaces: the app wins)"), "{text}");
    assert_eq!(count(&path, "context_quotes"), 2);
}

#[test]
fn the_inbox_screens_show_the_gotcha_type_and_the_quotes() {
    use crate::inbox_view::{quotes_words, submitted_fields};
    let (_dir, path) = seeded("notes-screens");
    let id = queued_id(&buffer(&path, "submit", shape_args(json!({"quotes": one_quote()}))).0);
    let gid = queued_id(&submit_gotcha(&path, "wal").0);
    let store = Store::open_read_only(&path).unwrap();
    let shape = store
        .buffer_entry(id)
        .unwrap()
        .unwrap()
        .submission()
        .unwrap();
    let fields = submitted_fields(&shape);
    let quotes = fields.iter().find(|(l, _)| *l == "quotes").unwrap();
    assert_eq!(
        quotes.1,
        "2026-11-05: the app is always right [supports: the app wins]"
    );
    assert_eq!(quotes_words(&shape.quotes), quotes.1);
    let gotcha = store
        .buffer_entry(gid)
        .unwrap()
        .unwrap()
        .submission()
        .unwrap();
    assert_eq!(submitted_fields(&gotcha), [("type", "hazard".to_owned())]);
    // The model that suggests sees both, marked as the sender's claim.
    let case = crate::decider::Case {
        submission: shape,
        why: "w".to_owned(),
        source: "s".to_owned(),
        stored: None,
    };
    let block = crate::decider::proposal_block(&case);
    assert!(block.contains("the app is always right"), "{block}");
    assert!(block.contains("unverified"), "{block}");
}

#[test]
fn an_older_stored_submission_without_the_new_fields_reads_and_hashes_as_before() {
    let old = r#"{"kind":"context","action":"set","name":"a","project":"","context_kind":"rule","text":"t","description":"","quote":"","area":null}"#;
    let read: store_buffer::Submission = serde_json::from_str(old).unwrap();
    assert!(read.gotcha_type.is_empty() && read.quotes.is_empty());
    assert_eq!(
        serde_json::to_string(&read).unwrap(),
        old,
        "the new fields add no bytes when empty"
    );
}

#[test]
fn the_direct_mode_asks_a_shape_for_evidence_and_stores_its_quotes() {
    let (_dir, path) = seeded("notes-shape-direct");
    let bare = json!({"name": "orders", "kind": "shape", "text": "The app is right."});
    let (text, failed) = dcall_in(&path, PROJECT, "context_set", bare);
    assert!(
        failed && text.contains("a shape needs the owner's words"),
        "{text}"
    );
    assert!(
        dcall_in(
            &path,
            PROJECT,
            "context_set",
            json!({"name": "orders", "kind": "rule", "text": "t", "quotes": one_quote()})
        )
        .0
        .contains("only a shape has quotes")
    );
    assert!(
        Store::open_read_only(&path)
            .unwrap()
            .context_entry(PROJECT, "orders")
            .unwrap()
            .is_none()
    );
    let with = json!({"name": "orders", "kind": "shape", "text": "The app is right.", "quotes": one_quote()});
    let (text, failed) = dcall_in(&path, PROJECT, "context_set", with);
    assert!(
        !failed && text.starts_with("added context entry 'orders'"),
        "{text}"
    );
    assert_eq!(count(&path, "context_quotes"), 1);
    // A rule needs no evidence, as before.
    let rule = json!({"name": "plain", "kind": "rule", "text": "Be plain."});
    assert!(!dcall_in(&path, PROJECT, "context_set", rule).1);
}

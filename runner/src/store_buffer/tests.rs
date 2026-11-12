//! Tests of the inbox store (issue 47). Every test works on a temporary database in a `Scratch`
//! folder; none opens the real `harness.db`.

use rusqlite::Connection;

use super::*;
use crate::store::SCHEMA_VERSION;
use crate::tools::testutil::Scratch;

fn open(dir: &Scratch) -> Store {
    Store::open(&dir.path().join("harness.db")).unwrap()
}

fn raw(kind: &str, action: &str, name: &str) -> Submission {
    Submission {
        kind: kind.to_owned(),
        action: action.to_owned(),
        name: name.to_owned(),
        ..Submission::default()
    }
}

fn context_set(name: &str, text: &str) -> Submission {
    Submission {
        context_kind: "rule".to_owned(),
        text: text.to_owned(),
        ..raw("context", "set", name)
    }
}

fn checked(raw: &Submission) -> Checked {
    Checked::new(raw, "because", "claude-code").unwrap()
}

fn queue(store: &mut Store, raw: &Submission) -> Submitted {
    store.queue_submission(&checked(raw)).unwrap()
}

fn queued_id(submitted: Submitted) -> i64 {
    match submitted {
        Submitted::Queued(id) => id,
        other => panic!("expected a new row, got {other:?}"),
    }
}

/// Everything the curated tables hold, as one text.
fn curated(store: &Store) -> String {
    format!(
        "{:?}\n{:?}\n{:?}\n{:?}\n{:?}",
        store.list_all_context().unwrap(),
        store.list_all_glossary().unwrap(),
        store.list_prompts().unwrap(),
        store.list_profiles().unwrap(),
        store.selected_profile().unwrap(),
    )
}

fn tables(conn: &Connection) -> Vec<String> {
    let mut statement = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap();
    statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn schema_25_upgrades_through_26_and_27_to_28_keeping_every_table_and_making_one_backup() {
    let dir = Scratch::new("buffer-migrate");
    let path = dir.path().join("harness.db");
    let mut store = Store::open(&path).unwrap();
    store
        .set_context("/p", "style", "rule", "Plainly.", "", None)
        .unwrap();
    store
        .set_glossary_in("", "ledger", "The book.", None)
        .unwrap();
    store
        .add_prompt_of_type("terse", "short", "Be brief.", None, PromptType::Skill)
        .unwrap();
    let before = tables(&store.conn);
    assert!(before.contains(&"buffer_entries".to_owned()));
    drop(store);
    {
        // Put the file back as schema 25 left it.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "DROP TABLE buffer_suggestions; DROP INDEX buffer_entries_pending; DROP TABLE buffer_entries; PRAGMA user_version = 25;",
        )
        .unwrap();
    }
    let store = Store::open(&path).unwrap();
    let version: i64 = store
        .conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    assert_eq!(SCHEMA_VERSION, 33);
    assert_eq!(tables(&store.conn), before, "every table is still there");
    assert!(store.context_entry("/p", "style").unwrap().is_some());
    assert!(store.glossary_entry("ledger").unwrap().is_some());
    assert_eq!(store.list_all_glossary().unwrap().len(), 1);
    assert!(store.prompt("terse").is_ok());
    assert!(store.list_pending().unwrap().is_empty());

    let copy = Connection::open(dir.path().join("harness.db.before-33")).unwrap();
    let old: i64 = copy
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(old, 25);
    assert!(!tables(&copy).contains(&"buffer_entries".to_owned()));
    assert!(!tables(&copy).contains(&"buffer_suggestions".to_owned()));
    // Opening again changes nothing and makes no second copy.
    drop(store);
    drop(Store::open(&path).unwrap());
    assert!(!dir.path().join("harness.db.before-33.2").exists());
}

#[test]
fn schema_26_upgrades_to_28_keeping_the_inbox_and_making_the_before_28_copy() {
    let dir = Scratch::new("buffer-migrate-27");
    let path = dir.path().join("harness.db");
    let mut store = Store::open(&path).unwrap();
    let id = queued_id(queue(&mut store, &context_set("style", "Write plainly.")));
    let second = queue_second(&mut store);
    reject(&mut store, second, "no").unwrap();
    let before = tables(&store.conn);
    assert!(before.contains(&"buffer_suggestions".to_owned()));
    let entries = (
        store.list_pending().unwrap(),
        store.list_decided(10).unwrap(),
    );
    drop(store);
    {
        // Put the file back as schema 26 left it.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("DROP TABLE buffer_suggestions; PRAGMA user_version = 26;")
            .unwrap();
    }
    let store = Store::open(&path).unwrap();
    let version: i64 = store
        .conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 33);
    assert_eq!(tables(&store.conn), before);
    assert_eq!(
        (
            store.list_pending().unwrap(),
            store.list_decided(10).unwrap()
        ),
        entries,
        "every inbox row is as it was"
    );
    assert!(store.list_suggestions(id).unwrap().is_empty());

    let copy = Connection::open(dir.path().join("harness.db.before-33")).unwrap();
    let old: i64 = copy
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(old, 26);
    assert!(!tables(&copy).contains(&"buffer_suggestions".to_owned()));
    assert!(tables(&copy).contains(&"buffer_entries".to_owned()));
}

fn queue_second(store: &mut Store) -> i64 {
    queued_id(queue(
        store,
        &context_set("other", "Another rule entirely."),
    ))
}

#[test]
fn the_table_refuses_a_bad_kind_action_or_status() {
    let dir = Scratch::new("buffer-checks");
    let store = open(&dir);
    let insert = |kind: &str, action: &str, status: &str| {
        store.conn.execute(
            "INSERT INTO buffer_entries (kind, action, name, entry_json, why, source, status, \
             content_sha256, created_at) VALUES (?1, ?2, 'n', '{}', 'w', 's', ?3, ?4, 1)",
            params![kind, action, status, format!("{kind}{action}{status}")],
        )
    };
    insert("context", "set", "pending").unwrap();
    for kind in KINDS {
        insert(kind, "delete", "rejected").unwrap();
    }
    assert!(insert("profile", "set", "pending").is_err());
    assert!(insert("", "set", "pending").is_err());
    assert!(insert("context", "replace", "pending").is_err());
    assert!(insert("context", "set", "done").is_err());
    assert!(insert("context", "set", "").is_err());
}

#[test]
fn an_identical_pending_entry_is_the_same_row_and_can_be_queued_again_once_decided() {
    let dir = Scratch::new("buffer-dedup");
    let mut store = open(&dir);
    let entry = context_set("style", "Write plainly.");
    let first = queued_id(queue(&mut store, &entry));
    // The same entry again, even from another sender with another reason.
    assert_eq!(queue(&mut store, &entry), Submitted::AlreadyQueued(first));
    let other_sender = Checked::new(&entry, "another reason", "someone-else").unwrap();
    assert_eq!(
        store.queue_submission(&other_sender).unwrap(),
        Submitted::AlreadyQueued(first)
    );
    assert_eq!(store.list_pending().unwrap().len(), 1);
    // A different text is a different entry.
    let second = queued_id(queue(&mut store, &context_set("style", "Write briefly.")));
    assert_ne!(first, second);

    reject(&mut store, first, "not now").unwrap();
    let third = queued_id(queue(&mut store, &entry));
    assert!(third > second, "a decided entry may come back as a new row");
    assert_eq!(queue(&mut store, &entry), Submitted::AlreadyQueued(third));
    // An applied one too.
    assert!(matches!(
        apply_entry(&mut store, third, None).unwrap(),
        ApplyOutcome::Applied(_)
    ));
    let fourth = queued_id(queue(&mut store, &entry));
    assert!(fourth > third);
    // Nothing was deleted: four rows, two of them decided.
    let count: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM buffer_entries", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 4);
}

#[test]
fn the_inbox_holds_200_pending_entries_and_room_returns_when_one_is_decided() {
    let dir = Scratch::new("buffer-cap");
    let mut store = open(&dir);
    let mut ids = Vec::new();
    for n in 0..MAX_PENDING {
        ids.push(queued_id(queue(
            &mut store,
            &context_set(&format!("entry-{n}"), "text"),
        )));
    }
    assert_eq!(store.list_pending().unwrap().len(), 200);
    let over = context_set("entry-over", "text");
    assert_eq!(queue(&mut store, &over), Submitted::Full);
    // An entry that is queued already is still answered with its row when the inbox is full.
    assert_eq!(
        queue(&mut store, &context_set("entry-3", "text")),
        Submitted::AlreadyQueued(ids[3])
    );
    assert_eq!(store.list_pending().unwrap().len(), 200);
    reject(&mut store, ids[0], "").unwrap();
    assert!(matches!(queue(&mut store, &over), Submitted::Queued(_)));
    assert_eq!(
        queue(&mut store, &context_set("another", "t")),
        Submitted::Full
    );
    // Decided rows do not count: 200 pending, 1 rejected.
    let all: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM buffer_entries", [], |r| r.get(0))
        .unwrap();
    assert_eq!(all, 201);
}

#[test]
fn the_stored_json_is_the_submission_in_field_order_and_is_never_rewritten() {
    let dir = Scratch::new("buffer-json");
    let mut store = open(&dir);
    let entry = Submission {
        quote: "the owner said so".to_owned(),
        area: Some("billing".to_owned()),
        ..context_set("  style ", "  Write plainly.  ")
    };
    let id = queued_id(queue(&mut store, &entry));
    let row = store.buffer_entry(id).unwrap().unwrap();
    assert_eq!(
        row.entry_json,
        r#"{"kind":"context","action":"set","name":"style","project":"","context_kind":"rule","text":"Write plainly.","description":"","quote":"the owner said so","area":"billing"}"#
    );
    assert_eq!(row.content_sha256, sha256_hex(row.entry_json.as_bytes()));
    assert_eq!(
        (row.kind.as_str(), row.action.as_str(), row.name.as_str()),
        ("context", "set", "style")
    );
    assert_eq!(row.status, Status::Pending);
    assert_eq!(row.decided_at, None);
    assert_eq!(
        (row.why.as_str(), row.source.as_str()),
        ("because", "claude-code")
    );
    assert_eq!(row.submission().unwrap().text, "Write plainly.");

    // Deciding changes the status, the time and the note, and nothing else.
    reject(&mut store, id, "  too vague ").unwrap();
    let after = store.buffer_entry(id).unwrap().unwrap();
    assert_eq!(after.status, Status::Rejected);
    assert!(after.decided_at.is_some());
    assert_eq!(after.decision_note, "too vague");
    assert_eq!(
        BufferEntry {
            status: Status::Pending,
            decided_at: None,
            decision_note: String::new(),
            ..after
        },
        row
    );
}

#[test]
fn a_decided_row_cannot_be_decided_again_or_given_a_note() {
    let dir = Scratch::new("buffer-final");
    let mut store = open(&dir);
    let id = queued_id(queue(&mut store, &context_set("a", "text")));
    reject(&mut store, id, "no").unwrap();
    let why = reject(&mut store, id, "again").unwrap_err().to_string();
    assert_eq!(why, format!("#{id} is rejected already"));
    assert!(!store.mark_buffer_applied(id, "").unwrap());
    assert!(!store.set_buffer_note(id, "x").unwrap());
    let row = store.buffer_entry(id).unwrap().unwrap();
    assert_eq!(
        (row.status, row.decision_note.as_str()),
        (Status::Rejected, "no")
    );
    assert_eq!(
        reject(&mut store, 999, "").unwrap_err().to_string(),
        "there is no submission #999"
    );
    assert!(store.buffer_entry(999).unwrap().is_none());
}

// ---- validation ----

fn refusal(raw: &Submission) -> String {
    raw.validate().unwrap_err()
}

#[test]
fn every_kind_and_action_that_passes_the_checks_is_accepted_trimmed() {
    let ok = [
        context_set(" style ", " text "),
        raw("context", "delete", "style"),
        Submission {
            text: "A definition.".to_owned(),
            area: Some("billing".to_owned()),
            ..raw("glossary", "set", "ledger")
        },
        raw("glossary", "delete", "ledger"),
        Submission {
            text: "Be brief.".to_owned(),
            description: "short answers".to_owned(),
            ..raw("skill", "set", "terse")
        },
        raw("skill", "delete", "terse"),
        Submission {
            text: "Stay calm.".to_owned(),
            ..raw("personality", "set", "calm")
        },
        raw("personality", "delete", "calm"),
        Submission {
            text: "You are careful.".to_owned(),
            ..raw("PROMPT", " SET ", "house")
        },
        raw("prompt", "delete", "house"),
    ];
    for entry in &ok {
        let done = entry
            .validate()
            .unwrap_or_else(|e| panic!("{entry:?}: {e}"));
        assert_eq!(done, done.validate().unwrap(), "validating twice is stable");
    }
    let done = ok[0].validate().unwrap();
    assert_eq!((done.name.as_str(), done.text.as_str()), ("style", "text"));
    assert_eq!(ok[8].validate().unwrap().kind, "prompt");
    assert_eq!(ok[8].validate().unwrap().action, "set");
}

#[test]
fn profiles_and_unknown_kinds_and_actions_are_refused() {
    for kind in ["profile", "Profiles"] {
        let why = refusal(&raw(kind, "set", "work"));
        assert!(why.contains("profiles cannot be submitted"), "{why}");
    }
    for kind in ["", "tool", "rule", "setting"] {
        let why = refusal(&raw(kind, "set", "x"));
        assert!(
            why.starts_with("a kind is one of: context, glossary"),
            "{why}"
        );
    }
    for action in ["", "add", "edit", "remove"] {
        let why = refusal(&raw("context", action, "x"));
        assert_eq!(why, "an action is one of: set, delete");
    }
}

#[test]
fn the_checks_of_the_store_refuse_what_the_direct_tools_refuse() {
    let cases = [
        // context
        (context_set("", "t"), "the name is empty"),
        (context_set("has space", "t"), "a name is letters"),
        (context_set(&"n".repeat(61), "t"), "a name is at most 60"),
        (context_set("n", ""), "the text is empty"),
        (
            context_set("n", &"x".repeat(8_001)),
            "an entry is at most 8000 bytes",
        ),
        (
            Submission {
                context_kind: "law".to_owned(),
                ..context_set("n", "t")
            },
            "a kind is one of: rule, decision, shape, guide",
        ),
        (
            Submission {
                context_kind: String::new(),
                ..context_set("n", "t")
            },
            "a kind is one of: rule, decision, shape, guide",
        ),
        (
            Submission {
                quote: "q".repeat(1_001),
                ..context_set("n", "t")
            },
            "a quote is at most 1000",
        ),
        (
            Submission {
                area: Some("two\nlines".to_owned()),
                ..context_set("n", "t")
            },
            "an area is one line",
        ),
        (
            Submission {
                project: "/a\n/b".to_owned(),
                ..context_set("n", "t")
            },
            "a project is the folder's path",
        ),
        // glossary
        (
            Submission {
                text: "d".to_owned(),
                ..raw("glossary", "set", "")
            },
            "the term is empty",
        ),
        (
            Submission {
                text: "d".to_owned(),
                ..raw("glossary", "set", "two\nlines")
            },
            "a term is one line",
        ),
        (raw("glossary", "set", "word"), "the definition is empty"),
        (
            Submission {
                text: "d".repeat(2_001),
                ..raw("glossary", "set", "word")
            },
            "a definition is at most 2000",
        ),
        // library
        (
            Submission {
                text: "t".to_owned(),
                ..raw("skill", "set", "on")
            },
            "cannot be a name",
        ),
        (
            Submission {
                text: "t".to_owned(),
                ..raw("skill", "set", "bad name")
            },
            "name",
        ),
        (raw("skill", "set", "terse"), "the prompt is empty"),
        (
            Submission {
                text: "t".repeat(200_001),
                ..raw("skill", "set", "terse")
            },
            "the limit is 200000",
        ),
        (
            Submission {
                text: "t".to_owned(),
                description: "two\nlines".to_owned(),
                ..raw("skill", "set", "terse")
            },
            "a description is one line",
        ),
        (
            Submission {
                text: "t".to_owned(),
                description: "d".repeat(201),
                ..raw("personality", "set", "calm")
            },
            "a description is at most 200",
        ),
        (
            Submission {
                text: "t".to_owned(),
                project: "/work/app".to_owned(),
                ..raw("prompt", "set", "house")
            },
            "for every project",
        ),
    ];
    for (entry, expected) in cases {
        let why = refusal(&entry);
        assert!(
            why.to_lowercase().contains(&expected.to_lowercase()),
            "{entry:?}: wanted '{expected}' in '{why}'"
        );
    }
}

#[test]
fn a_delete_takes_no_text_and_a_field_that_does_not_apply_is_refused() {
    let with_text = Submission {
        text: "x".to_owned(),
        ..raw("context", "delete", "style")
    };
    assert_eq!(refusal(&with_text), "a delete takes no text");
    for (field, entry) in [
        (
            "description",
            Submission {
                description: "d".to_owned(),
                ..raw("skill", "delete", "terse")
            },
        ),
        (
            "quote",
            Submission {
                quote: "q".to_owned(),
                ..raw("glossary", "delete", "ledger")
            },
        ),
        (
            "context_kind",
            Submission {
                context_kind: "rule".to_owned(),
                ..raw("context", "delete", "style")
            },
        ),
        (
            "area",
            Submission {
                area: Some("a".to_owned()),
                ..raw("context", "delete", "style")
            },
        ),
    ] {
        assert_eq!(refusal(&entry), format!("a delete takes no {field}"));
    }
    // Set: fields of another kind are not silently dropped.
    let glossary_with_quote = Submission {
        text: "d".to_owned(),
        quote: "q".to_owned(),
        ..raw("glossary", "set", "word")
    };
    assert_eq!(
        refusal(&glossary_with_quote),
        "'quote' does not apply to a glossary"
    );
    let skill_with_area = Submission {
        text: "t".to_owned(),
        area: Some("a".to_owned()),
        ..raw("skill", "set", "terse")
    };
    assert_eq!(
        refusal(&skill_with_area),
        "'area' does not apply to a skill"
    );
}

#[test]
fn why_and_source_are_required_and_bounded() {
    let entry = context_set("style", "t");
    assert!(Checked::new(&entry, "w", "s").is_ok());
    for why in ["", "   ", "\n"] {
        assert!(
            Checked::new(&entry, why, "s")
                .unwrap_err()
                .starts_with("why is required")
        );
    }
    assert!(Checked::new(&entry, &"w".repeat(1_000), "s").is_ok());
    assert_eq!(
        Checked::new(&entry, &"w".repeat(1_001), "s").unwrap_err(),
        "why is at most 1000 characters"
    );
    // Characters, not bytes.
    assert!(Checked::new(&entry, &"é".repeat(1_000), "s").is_ok());
    assert!(Checked::new(&entry, "line one\nline two", "s").is_ok());
    assert_eq!(
        Checked::new(&entry, "bell\u{7}", "s").unwrap_err(),
        "why is plain text"
    );
    for source in ["", "  "] {
        assert!(
            Checked::new(&entry, "w", source)
                .unwrap_err()
                .starts_with("source is required")
        );
    }
    assert!(Checked::new(&entry, "w", &"s".repeat(100)).is_ok());
    assert_eq!(
        Checked::new(&entry, "w", &"s".repeat(101)).unwrap_err(),
        "source is at most 100 characters"
    );
    for source in ["two\nlines", "tab\there", "esc\u{1b}[0m"] {
        assert_eq!(
            Checked::new(&entry, "w", source).unwrap_err(),
            "source is one line of plain text"
        );
    }
    // The submission is checked first.
    assert_eq!(
        Checked::new(&raw("profile", "set", "x"), "", "").unwrap_err(),
        "profiles cannot be submitted: only the owner changes them"
    );
}

// ---- apply ----

/// A database with a little of everything the entries point at.
fn seeded(tag: &str) -> (Scratch, Store) {
    let dir = Scratch::new(tag);
    let mut store = open(&dir);
    store
        .set_context("", "style", "rule", "Write plainly.", "keep it plain", None)
        .unwrap();
    store
        .set_glossary_in("", "ledger", "The book of record.", Some("billing"))
        .unwrap();
    store
        .add_prompt_of_type(
            "terse",
            "short answers",
            "Be brief.",
            None,
            PromptType::Skill,
        )
        .unwrap();
    store
        .add_prompt_of_type("calm", "", "Stay calm.", None, PromptType::Personality)
        .unwrap();
    store
        .add_prompt("house", "the house prompt", "You are careful.", None)
        .unwrap();
    (dir, store)
}

/// The fingerprint a screen would pass: the stored target right now.
fn seen(store: &Store, id: i64) -> Option<String> {
    let sub = store
        .buffer_entry(id)
        .unwrap()
        .unwrap()
        .submission()
        .unwrap();
    current_target(store, &sub).unwrap().map(|s| s.fingerprint)
}

fn accept(store: &mut Store, id: i64) -> ApplyOutcome {
    let expected = seen(store, id);
    apply_entry(store, id, expected.as_deref()).unwrap()
}

fn applied_words(outcome: ApplyOutcome) -> String {
    match outcome {
        ApplyOutcome::Applied(text) => text,
        other => panic!("expected Applied, got {other:?}"),
    }
}

#[test]
fn accepting_a_context_entry_adds_replaces_and_deletes_it() {
    let (_dir, mut store) = seeded("buffer-apply-context");
    let add = queued_id(queue(
        &mut store,
        &Submission {
            project: "/work/app".to_owned(),
            quote: "owner words".to_owned(),
            area: Some("billing".to_owned()),
            context_kind: "shape".to_owned(),
            ..context_set("billing-view", "Money is whole cents.")
        },
    ));
    assert_eq!(
        applied_words(accept(&mut store, add)),
        "added context entry 'billing-view' for project /work/app"
    );
    let entry = store
        .context_entry("/work/app", "billing-view")
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            entry.kind.as_str(),
            entry.text.as_str(),
            entry.quote.as_str()
        ),
        ("shape", "Money is whole cents.", "owner words")
    );
    assert!(
        entry.preload,
        "a new shape is preloaded, as the store does it"
    );
    assert_eq!(
        store.buffer_entry(add).unwrap().unwrap().status,
        Status::Applied
    );
    assert!(
        store
            .buffer_entry(add)
            .unwrap()
            .unwrap()
            .decided_at
            .is_some()
    );

    let replace = queued_id(queue(&mut store, &context_set("STYLE", "Write briefly.")));
    assert!(applied_words(accept(&mut store, replace)).starts_with("replaced context entry"));
    let style = store.context_entry("", "style").unwrap().unwrap();
    assert_eq!(style.text, "Write briefly.");
    assert_eq!(
        style.quote, "keep it plain",
        "an empty quote keeps the stored one"
    );

    let delete = queued_id(queue(&mut store, &raw("context", "delete", "style")));
    assert_eq!(
        applied_words(accept(&mut store, delete)),
        "deleted context entry 'style' for every project"
    );
    assert!(store.context_entry("", "style").unwrap().is_none());
}

#[test]
fn accepting_a_glossary_term_adds_replaces_and_deletes_it() {
    let (_dir, mut store) = seeded("buffer-apply-glossary");
    let add = queued_id(queue(
        &mut store,
        &Submission {
            text: "An order that is paid.".to_owned(),
            project: "/work/app".to_owned(),
            ..raw("glossary", "set", "settled order")
        },
    ));
    assert!(applied_words(accept(&mut store, add)).starts_with("added glossary term"));
    let replace = queued_id(queue(
        &mut store,
        &Submission {
            text: "The ledger of record.".to_owned(),
            ..raw("glossary", "set", "LEDGER")
        },
    ));
    assert!(applied_words(accept(&mut store, replace)).starts_with("replaced glossary term"));
    let all = store.list_all_glossary().unwrap();
    let ledger = all.iter().find(|e| e.term == "ledger").unwrap();
    assert_eq!(ledger.definition, "The ledger of record.");
    assert_eq!(
        ledger.area, "billing",
        "an omitted area keeps the stored one"
    );
    let delete = queued_id(queue(&mut store, &raw("glossary", "delete", "ledger")));
    assert!(applied_words(accept(&mut store, delete)).starts_with("deleted glossary term"));
    assert_eq!(store.list_all_glossary().unwrap().len(), 1);
}

#[test]
fn a_new_library_item_is_saved_switched_off_and_a_set_on_an_existing_one_is_an_edit() {
    let (_dir, mut store) = seeded("buffer-apply-library");
    for (kind, name) in [
        ("skill", "fresh"),
        ("personality", "bold"),
        ("prompt", "extra"),
    ] {
        let id = queued_id(queue(
            &mut store,
            &Submission {
                text: "New text.".to_owned(),
                description: "new".to_owned(),
                ..raw(kind, "set", name)
            },
        ));
        assert!(applied_words(accept(&mut store, id)).ends_with(", switched off"));
        let prompt = store.prompt(name).unwrap();
        assert!(!prompt.enabled, "{kind} {name} must be off");
        assert_eq!(
            prompt.prompt_type.as_str(),
            if kind == "prompt" { "system" } else { kind }
        );
        assert_eq!(prompt.text, "New text.");
    }
    let before = store.assembled_prompt().unwrap().text;
    // An edit of an existing skill.
    let id = queued_id(queue(
        &mut store,
        &Submission {
            text: "Be very brief.".to_owned(),
            description: "shorter".to_owned(),
            ..raw("skill", "set", "terse")
        },
    ));
    assert_eq!(
        applied_words(accept(&mut store, id)),
        "changed skill 'terse'"
    );
    let terse = store.prompt("terse").unwrap();
    assert_eq!(
        (terse.text.as_str(), terse.description.as_str()),
        ("Be very brief.", "shorter")
    );
    assert_eq!(
        store.assembled_prompt().unwrap().text,
        before,
        "no prompt was switched on"
    );
    // Deletes.
    for (kind, name) in [
        ("skill", "terse"),
        ("personality", "calm"),
        ("prompt", "house"),
    ] {
        let id = queued_id(queue(&mut store, &raw(kind, "delete", name)));
        assert!(applied_words(accept(&mut store, id)).starts_with("deleted"));
        assert!(store.prompt(name).is_err());
    }
}

#[test]
fn a_stale_target_is_reported_writes_nothing_and_leaves_the_row_pending() {
    let (_dir, mut store) = seeded("buffer-apply-stale");
    let id = queued_id(queue(
        &mut store,
        &Submission {
            text: "Be very brief.".to_owned(),
            ..raw("skill", "set", "terse")
        },
    ));
    let shown = seen(&store, id).unwrap();
    // The owner edits it in the meantime.
    let old = store.prompt("terse").unwrap();
    assert_eq!(old.fingerprint(), shown);
    assert_eq!(
        store
            .edit_prompt(
                old.id,
                &shown,
                "terse",
                "short answers",
                "Edited meanwhile."
            )
            .unwrap(),
        Edit::Done
    );
    let before = curated(&store);
    assert_eq!(
        apply_entry(&mut store, id, Some(&shown)).unwrap(),
        ApplyOutcome::Stale
    );
    assert_eq!(curated(&store), before, "nothing was written");
    let row = store.buffer_entry(id).unwrap().unwrap();
    assert_eq!(row.status, Status::Pending);
    assert!(row.decision_note.contains("changed since it was shown"));
    // Looking again and accepting works.
    assert_eq!(
        applied_words(accept(&mut store, id)),
        "changed skill 'terse'"
    );
    let row = store.buffer_entry(id).unwrap().unwrap();
    assert_eq!(
        (row.status, row.decision_note.as_str()),
        (Status::Applied, "")
    );

    // Context and glossary are checked the same way.
    for (entry, change) in [
        (context_set("style", "New."), 0),
        (
            Submission {
                text: "New.".to_owned(),
                ..raw("glossary", "set", "ledger")
            },
            1,
        ),
        (raw("context", "delete", "style"), 2),
        (raw("skill", "delete", "terse"), 3),
    ] {
        let id = queued_id(queue(&mut store, &entry));
        let shown = seen(&store, id);
        match change {
            0 | 2 => {
                store
                    .set_context(
                        "",
                        "style",
                        "rule",
                        &format!("Changed meanwhile {change}."),
                        "",
                        None,
                    )
                    .unwrap();
            }
            1 => {
                store
                    .set_glossary_in("", "ledger", "Changed meanwhile.", None)
                    .unwrap();
            }
            _ => {
                let p = store.prompt("terse").unwrap();
                store
                    .edit_prompt(p.id, &p.fingerprint(), "terse", "", "Changed meanwhile.")
                    .unwrap();
            }
        }
        let before = curated(&store);
        assert_eq!(
            apply_entry(&mut store, id, shown.as_deref()).unwrap(),
            ApplyOutcome::Stale,
            "case {change}"
        );
        assert_eq!(curated(&store), before, "case {change}");
        assert_eq!(
            store.buffer_entry(id).unwrap().unwrap().status,
            Status::Pending
        );
    }
    // A target that was not there and is now: also stale.
    let id = queued_id(queue(
        &mut store,
        &Submission {
            text: "t".to_owned(),
            ..raw("skill", "set", "late")
        },
    ));
    store
        .add_prompt_of_type("late", "", "Added meanwhile.", None, PromptType::Skill)
        .unwrap();
    assert_eq!(
        apply_entry(&mut store, id, None).unwrap(),
        ApplyOutcome::Stale
    );
}

#[test]
fn accepting_twice_is_safe_and_a_set_that_is_there_is_no_change_and_applied() {
    let (_dir, mut store) = seeded("buffer-apply-idempotent");
    let id = queued_id(queue(&mut store, &context_set("fresh", "Fresh text.")));
    let shown = seen(&store, id);
    assert!(matches!(
        apply_entry(&mut store, id, shown.as_deref()).unwrap(),
        ApplyOutcome::Applied(_)
    ));
    let after = curated(&store);
    // The same form submitted again (a double click) does nothing.
    assert_eq!(
        apply_entry(&mut store, id, shown.as_deref()).unwrap(),
        ApplyOutcome::NoChange
    );
    assert_eq!(curated(&store), after);

    // A crash between the write and marking the row: the entry is there, the row is pending.
    let crash = queued_id(queue(&mut store, &context_set("crash", "Written already.")));
    store
        .set_context("", "crash", "rule", "Written already.", "", None)
        .unwrap();
    let before = curated(&store);
    assert_eq!(accept(&mut store, crash), ApplyOutcome::NoChange);
    assert_eq!(curated(&store), before);
    assert_eq!(
        store.buffer_entry(crash).unwrap().unwrap().status,
        Status::Applied
    );

    // The same for a library item and a delete of something already gone.
    let skill = queued_id(queue(
        &mut store,
        &Submission {
            text: "Be brief.".to_owned(),
            description: "short answers".to_owned(),
            ..raw("skill", "set", "terse")
        },
    ));
    assert_eq!(accept(&mut store, skill), ApplyOutcome::NoChange);
    assert_eq!(
        store.buffer_entry(skill).unwrap().unwrap().status,
        Status::Applied
    );
    for (kind, name) in [
        ("context", "ghost"),
        ("glossary", "ghost"),
        ("skill", "ghost"),
    ] {
        let id = queued_id(queue(&mut store, &raw(kind, "delete", name)));
        assert!(
            applied_words(accept(&mut store, id)).starts_with("already gone"),
            "{kind}"
        );
        assert_eq!(
            store.buffer_entry(id).unwrap().unwrap().status,
            Status::Applied
        );
    }
}

#[test]
fn a_failing_apply_leaves_the_row_pending_with_the_reason_and_changes_nothing() {
    let (_dir, mut store) = seeded("buffer-apply-failed");
    // A skill called like an existing personality.
    let id = queued_id(queue(
        &mut store,
        &Submission {
            text: "t".to_owned(),
            ..raw("skill", "set", "calm")
        },
    ));
    let before = curated(&store);
    let outcome = apply_entry(&mut store, id, None).unwrap();
    assert_eq!(
        outcome,
        ApplyOutcome::Failed("'calm' is a personality, not a skill".to_owned())
    );
    assert_eq!(curated(&store), before);
    let row = store.buffer_entry(id).unwrap().unwrap();
    assert_eq!(row.status, Status::Pending);
    assert_eq!(row.decided_at, None);
    assert_eq!(row.decision_note, "'calm' is a personality, not a skill");
    // It is still pending, so the owner can reject it, with a note of their own.
    reject(&mut store, id, "wrong kind").unwrap();
    assert_eq!(
        store.buffer_entry(id).unwrap().unwrap().decision_note,
        "wrong kind"
    );

    // A stored submission that no longer passes the checks is not applied.
    let bad = queued_id(queue(&mut store, &context_set("ok", "t")));
    store
        .conn
        .execute(
            "UPDATE buffer_entries SET entry_json = replace(entry_json, '\"rule\"', '\"law\"') WHERE id = ?1",
            [bad],
        )
        .unwrap();
    match apply_entry(&mut store, bad, None).unwrap() {
        ApplyOutcome::Failed(why) => {
            assert!(why.starts_with("the submission is not valid"), "{why}")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        store.buffer_entry(bad).unwrap().unwrap().status,
        Status::Pending
    );
    assert!(store.context_entry("", "ok").unwrap().is_none());
    // No such row.
    assert!(apply_entry(&mut store, 4242, None).is_err());
}

#[test]
fn a_success_clears_an_earlier_apply_error() {
    let (_dir, mut store) = seeded("buffer-apply-recover");
    let id = queued_id(queue(
        &mut store,
        &Submission {
            text: "t".to_owned(),
            ..raw("skill", "set", "late")
        },
    ));
    // Stale first: a skill appeared with that name.
    store
        .add_prompt_of_type("late", "", "Another.", None, PromptType::Skill)
        .unwrap();
    assert_eq!(
        apply_entry(&mut store, id, None).unwrap(),
        ApplyOutcome::Stale
    );
    assert!(
        !store
            .buffer_entry(id)
            .unwrap()
            .unwrap()
            .decision_note
            .is_empty()
    );
    assert!(matches!(accept(&mut store, id), ApplyOutcome::Applied(_)));
    assert_eq!(store.buffer_entry(id).unwrap().unwrap().decision_note, "");
}

#[test]
fn reject_keeps_the_note_touches_no_curated_table_and_blocks_a_later_accept() {
    let (_dir, mut store) = seeded("buffer-reject");
    let before = curated(&store);
    let ids: Vec<i64> = [
        context_set("style", "Replaced."),
        raw("glossary", "delete", "ledger"),
        raw("prompt", "delete", "house"),
    ]
    .iter()
    .map(|e| queued_id(queue(&mut store, e)))
    .collect();
    for id in &ids {
        reject(&mut store, *id, "not like this").unwrap();
    }
    assert_eq!(curated(&store), before);
    for id in &ids {
        let row = store.buffer_entry(*id).unwrap().unwrap();
        assert_eq!(row.status, Status::Rejected);
        assert_eq!(row.decision_note, "not like this");
        assert!(row.decided_at.is_some());
    }
    assert!(store.list_pending().unwrap().is_empty());
    match apply_entry(&mut store, ids[0], None).unwrap() {
        ApplyOutcome::Failed(why) => assert!(why.contains("was rejected"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(curated(&store), before);
    assert_eq!(
        store.buffer_entry(ids[0]).unwrap().unwrap().status,
        Status::Rejected
    );
}

#[test]
fn current_target_shows_what_is_stored_now_and_whether_a_prompt_is_on() {
    let (_dir, mut store) = seeded("buffer-target");
    let style = context_set("STYLE", "x").validate().unwrap();
    let stored = current_target(&store, &style).unwrap().unwrap();
    assert_eq!(
        (
            stored.text.as_str(),
            stored.context_kind.as_str(),
            stored.quote.as_str()
        ),
        ("Write plainly.", "rule", "keep it plain")
    );
    let ledger = raw("glossary", "delete", "Ledger").validate().unwrap();
    let stored = current_target(&store, &ledger).unwrap().unwrap();
    assert_eq!(
        (stored.text.as_str(), stored.area.as_str()),
        ("The book of record.", "billing")
    );
    // The same name in another project is another target.
    let elsewhere = Submission {
        project: "/other".to_owned(),
        ..context_set("style", "x")
    }
    .validate()
    .unwrap();
    assert!(current_target(&store, &elsewhere).unwrap().is_none());
    // A system prompt in the profile new chats start under is on.
    let house = raw("prompt", "delete", "house").validate().unwrap();
    assert!(!current_target(&store, &house).unwrap().unwrap().on);
    store.set_prompt_enabled("house", true).unwrap();
    let on = current_target(&store, &house).unwrap().unwrap();
    assert!(on.on);
    assert_eq!(on.fingerprint, store.prompt("house").unwrap().fingerprint());
    let wrong = raw("skill", "delete", "house").validate().unwrap();
    assert_eq!(
        current_target(&store, &wrong).unwrap_err(),
        "'house' is a system, not a skill"
    );
}

#[test]
fn the_decided_list_is_applied_and_rejected_rows_newest_first_and_cut_at_the_limit() {
    let (_dir, mut store) = seeded("buffer-decided");
    assert!(store.list_decided(50).unwrap().is_empty());
    let ids: Vec<i64> = ["one", "two", "three", "four"]
        .iter()
        .map(|name| queued_id(queue(&mut store, &context_set(name, "Text."))))
        .collect();
    let waiting = queued_id(queue(&mut store, &context_set("waits", "Text.")));
    assert!(matches!(
        apply_entry(&mut store, ids[0], None).unwrap(),
        ApplyOutcome::Applied(_)
    ));
    reject(&mut store, ids[1], "no").unwrap();
    assert!(matches!(
        apply_entry(&mut store, ids[2], None).unwrap(),
        ApplyOutcome::Applied(_)
    ));
    // The pending ones are not in the trail.
    let decided = store.list_decided(50).unwrap();
    let got: Vec<i64> = decided.iter().map(|e| e.id).collect();
    assert_eq!(got, vec![ids[2], ids[1], ids[0]]);
    assert!(!got.contains(&waiting) && !got.contains(&ids[3]));
    assert_eq!(decided[1].status, Status::Rejected);
    assert_eq!(decided[1].decision_note, "no");
    assert_eq!(store.list_decided(2).unwrap().len(), 2);
    // Reading the trail changes nothing: the pending list is as it was.
    assert_eq!(store.list_pending().unwrap().len(), 2);
}

fn opinion(verdict: Verdict, confidence: Option<f64>, reason: &str) -> Opinion {
    Opinion {
        verdict,
        confidence,
        reason: reason.to_owned(),
        raw: format!("raw reply: {reason}"),
    }
}

#[test]
fn the_suggestion_table_refuses_a_bad_verdict_or_confidence() {
    let dir = Scratch::new("buffer-suggestion-checks");
    let mut store = open(&dir);
    let id = queued_id(queue(&mut store, &context_set("a", "text")));
    let insert = |decider: &str, verdict: &str, confidence: Option<f64>| {
        store.conn.execute(
            "INSERT INTO buffer_suggestions (buffer_id, decider, verdict, confidence, reason, raw, created_at) \
             VALUES (?1, ?2, ?3, ?4, 'r', 'raw', 1)",
            params![id, decider, verdict, confidence],
        )
    };
    insert("d1", "accept", Some(1.0)).unwrap();
    insert("d2", "reject", Some(0.0)).unwrap();
    insert("d3", "unsure", None).unwrap();
    assert!(insert("d4", "maybe", None).is_err());
    assert!(insert("d5", "", None).is_err());
    assert!(insert("d6", "Accept", None).is_err());
    assert!(insert("d7", "accept", Some(1.5)).is_err());
    assert!(insert("d8", "accept", Some(-0.1)).is_err());
}

#[test]
fn a_decider_speaks_once_per_entry_and_another_decider_may_speak_too() {
    let dir = Scratch::new("buffer-suggestion-unique");
    let mut store = open(&dir);
    let id = queued_id(queue(&mut store, &context_set("a", "text")));
    let first = opinion(Verdict::Accept, Some(0.9), "good");
    assert!(store.add_suggestion(id, "llm:p:m", &first).unwrap());
    // The same decider again: refused, and the first answer stays as it was.
    let second = opinion(Verdict::Reject, Some(0.9), "changed its mind");
    assert!(!store.add_suggestion(id, "llm:p:m", &second).unwrap());
    let raw_again = store.conn.execute(
        "INSERT INTO buffer_suggestions (buffer_id, decider, verdict, reason, raw, created_at) \
         VALUES (?1, 'llm:p:m', 'reject', 'r', 'raw', 1)",
        [id],
    );
    assert!(raw_again.is_err(), "the table itself refuses it");
    // Another decider is a second opinion.
    assert!(store.add_suggestion(id, "llm:p:other", &second).unwrap());
    let list = store.list_suggestions(id).unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].decider, "llm:p:m");
    assert_eq!(list[0].verdict, Verdict::Accept);
    assert_eq!(list[0].confidence, Some(0.9));
    assert_eq!(list[0].reason, "good");
    assert_eq!(list[0].raw, "raw reply: good");
    assert_eq!(list[1].decider, "llm:p:other");
    assert_eq!(list[1].verdict, Verdict::Reject);
}

#[test]
fn a_suggestion_needs_an_entry_and_a_missing_confidence_is_kept_missing() {
    let dir = Scratch::new("buffer-suggestion-fk");
    let mut store = open(&dir);
    let nothing = opinion(Verdict::Accept, Some(0.9), "x");
    assert!(store.add_suggestion(999, "llm:p:m", &nothing).is_err());
    let id = queued_id(queue(&mut store, &context_set("a", "text")));
    let unsure = opinion(Verdict::Unsure, None, "cannot tell");
    store.add_suggestion(id, "llm:p:m", &unsure).unwrap();
    let list = store.list_suggestions(id).unwrap();
    assert_eq!(list[0].confidence, None);
    assert_eq!(list[0].shown(), Verdict::Unsure);
    assert!(store.list_suggestions(id + 1).unwrap().is_empty());
}

#[test]
fn suggestions_are_listed_by_entry_and_survive_the_owners_decision() {
    let dir = Scratch::new("buffer-suggestion-by-entry");
    let mut store = open(&dir);
    let a = queued_id(queue(&mut store, &context_set("a", "text a")));
    let b = queued_id(queue(&mut store, &context_set("b", "text b")));
    let c = queued_id(queue(&mut store, &context_set("c", "text c")));
    store
        .add_suggestion(a, "llm:p:m", &opinion(Verdict::Accept, Some(0.95), "ok"))
        .unwrap();
    store
        .add_suggestion(c, "llm:p:m", &opinion(Verdict::Reject, Some(0.7), "no"))
        .unwrap();
    let before = store.list_suggestions(a).unwrap();
    // Accepting and rejecting never read or change a suggestion.
    assert!(matches!(
        apply_entry(&mut store, a, None).unwrap(),
        ApplyOutcome::Applied(_)
    ));
    reject(&mut store, c, "").unwrap();
    assert_eq!(store.list_suggestions(a).unwrap(), before);
    let by_entry = store.suggestions_for([a, b, c]).unwrap();
    assert_eq!(by_entry.len(), 2);
    assert!(!by_entry.contains_key(&b));
    assert_eq!(by_entry[&c][0].shown(), Verdict::Reject);
    // A row can still be decided with no suggestion at all.
    reject(&mut store, b, "").unwrap();
}

#[test]
fn text_a_reviewer_cannot_see_is_refused_in_every_field() {
    // A visible sentence followed by an invisible instruction written in Unicode tag characters.
    let hidden: String = "Use tabs."
        .chars()
        .chain(['\u{E0049}', '\u{E0067}'])
        .collect();
    for bad in [
        hidden.clone(),
        "Use tabs.\u{200B}".to_owned(),
        "Use tabs.\u{202E}".to_owned(),
        "Use tabs.\u{FEFF}".to_owned(),
        "Use tabs.\u{0007}".to_owned(),
        "Use tabs.\u{00AD}".to_owned(),
    ] {
        let mut in_text = context_set("rule-one", &bad);
        assert!(Checked::new(&in_text, "because", "me").is_err(), "{bad:?}");
        in_text.text = "fine".to_owned();
        in_text.quote = bad.clone();
        assert!(Checked::new(&in_text, "because", "me").is_err(), "{bad:?}");
        in_text.quote = String::new();
        in_text.name = bad.clone();
        assert!(Checked::new(&in_text, "because", "me").is_err(), "{bad:?}");
        let ok = context_set("rule-one", "fine");
        assert!(Checked::new(&ok, &bad, "me").is_err(), "why {bad:?}");
        assert!(
            Checked::new(&ok, "because", &bad).is_err(),
            "source {bad:?}"
        );
    }
    let why = Checked::new(&context_set("rule-one", "fine"), "tag \u{E0049}", "me").unwrap_err();
    assert!(why.contains("U+E0049") && why.contains("why"), "{why}");
    // New lines and tabs in a text are fine.
    let plain = context_set("rule-two", "line one\n\tline two");
    assert!(Checked::new(&plain, "because", "me").is_ok());
}

#[test]
fn a_huge_reply_is_kept_only_up_to_the_cap() {
    let dir = Scratch::new("buffer-suggestion-raw-cap");
    let mut store = open(&dir);
    let id = queued_id(queue(&mut store, &context_set("a", "text")));
    let mut huge = opinion(Verdict::Accept, Some(0.9), "good");
    huge.raw = "é".repeat(MAX_RAW_CHARS + 5_000);
    assert!(store.add_suggestion(id, "llm:p:m", &huge).unwrap());
    let kept = &store.list_suggestions(id).unwrap()[0];
    assert_eq!(kept.raw.chars().count(), MAX_RAW_CHARS);
    assert_eq!(kept.reason, "good", "only raw is cut");
}

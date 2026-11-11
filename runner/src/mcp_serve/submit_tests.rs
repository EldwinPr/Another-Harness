//! Tests of the buffer mode of `--mcp-serve` (issue 47): `submit`, `submission_get` and the three
//! modes. Every test works on a temporary database made by `seeded`; none opens the real
//! `harness.db` or touches the network.

use std::io::Cursor;
use std::path::Path;

use serde_json::{Value, json};

use super::tests::{PROJECT, request, seeded, session};
use super::*;
use crate::store_buffer::{self, ApplyOutcome, MAX_PENDING};
use crate::tools::testutil::Scratch;

const READ_NAMES: [&str; 12] = [
    "context_list",
    "context_get",
    "glossary_list",
    "glossary_get",
    "library_list",
    "library_get",
    "profile_list",
    "profile_get",
    "gotchas_list",
    "gotchas_get",
    "logs_list",
    "logs_get",
];

const WRITE_NAMES: [&str; 10] = [
    "context_set",
    "context_delete",
    "glossary_set",
    "glossary_delete",
    "library_add",
    "library_edit",
    "library_delete",
    "profile_add",
    "profile_edit",
    "profile_delete",
];

/// Sends the lines to a buffer server and returns what it wrote, one value per answer.
pub(super) fn bsession(path: &Path, project: &str, lines: &[Value]) -> Vec<Value> {
    let input: String = lines.iter().map(|l| format!("{l}\n")).collect();
    let mut output = Vec::new();
    Server::new(path, project)
        .unwrap()
        .buffer()
        .unwrap()
        .serve(Cursor::new(input), &mut output)
        .unwrap();
    String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Calls one tool on a buffer server started in `project`: the text and whether it failed.
pub(super) fn bcall_in(path: &Path, project: &str, tool: &str, arguments: Value) -> (String, bool) {
    let reply = bsession(
        path,
        project,
        &[request(
            1,
            "tools/call",
            json!({"name": tool, "arguments": arguments}),
        )],
    );
    let result = &reply[0]["result"];
    (
        result["content"][0]["text"].as_str().unwrap().to_owned(),
        result["isError"].as_bool().unwrap(),
    )
}

fn submit_in(path: &Path, project: &str, arguments: Value) -> (String, bool) {
    bcall_in(path, project, "submit", arguments)
}

fn submit(path: &Path, arguments: Value) -> (String, bool) {
    submit_in(path, PROJECT, arguments)
}

/// A submission that has to be queued: its number.
fn queued(path: &Path, arguments: Value) -> i64 {
    let (text, failed) = submit(path, arguments);
    assert!(!failed, "{text}");
    text.strip_prefix("queued as #")
        .unwrap_or_else(|| panic!("{text}"))
        .parse()
        .unwrap()
}

/// A submission that has to be refused with this reason, leaving no row.
fn refused(path: &Path, arguments: Value) -> String {
    let before = row_count(path);
    let (text, failed) = submit(path, arguments.clone());
    assert!(failed, "{arguments} should have been refused: {text}");
    assert_eq!(row_count(path), before, "a refusal added a row: {text}");
    text
}

fn row_count(path: &Path) -> i64 {
    Store::open_read_only(path)
        .unwrap()
        .conn
        .query_row("SELECT COUNT(*) FROM buffer_entries", [], |r| r.get(0))
        .unwrap()
}

/// Everything curated, as one text: equal before and after means nothing was written there.
fn curated(path: &Path) -> String {
    let store = Store::open_read_only(path).unwrap();
    format!(
        "{:?}\n{:?}\n{:?}\n{:?}\n{:?}",
        store.list_all_context().unwrap(),
        store.list_all_glossary().unwrap(),
        store.list_prompts().unwrap(),
        store.list_profiles().unwrap(),
        store.selected_profile().unwrap(),
    )
}

fn names(reply: &Value) -> Vec<String> {
    reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

fn good(kind: &str, action: &str, name: &str) -> Value {
    let mut entry = json!({"kind": kind, "action": action, "name": name,
        "why": "the owner asked for it", "source": "claude-code"});
    if action == "set" {
        entry["text"] = json!("Some text.");
        match kind {
            "context" => entry["context_kind"] = json!("rule"),
            "skill" | "personality" | "prompt" => entry["description"] = json!("what it is for"),
            _ => {}
        }
    }
    entry
}

// ---- the three modes ----

#[test]
fn the_default_buffer_mode_lists_the_twelve_reads_then_submit_submission_get_and_log_add() {
    let (_dir, path) = seeded("buffer-list");
    let reply = bsession(&path, "", &[request(1, "tools/list", Value::Null)]);
    let listed = names(&reply[0]);
    assert_eq!(listed.len(), 15);
    assert_eq!(listed[..12], READ_NAMES);
    assert_eq!(listed[12..], ["submit", "submission_get", "log_add"]);
    let tools = reply[0]["result"]["tools"].as_array().unwrap();
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object");
    }
    for tool in &tools[..12] {
        assert!(
            tool["description"]
                .as_str()
                .unwrap()
                .starts_with("Read-only.")
        );
    }
    let description = tools[12]["description"].as_str().unwrap();
    assert!(description.starts_with("Queues."), "{description}");
    assert!(
        description.contains("The owner reviews it and nothing changes until then"),
        "{description}"
    );
    let fields: Vec<&str> = tools[12]["inputSchema"]["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    for field in [
        "kind",
        "action",
        "name",
        "text",
        "description",
        "quote",
        "area",
        "project",
        "gotcha_type",
        "quotes",
        "why",
        "source",
    ] {
        assert!(fields.contains(&field), "{field}");
    }
    assert_eq!(
        tools[12]["inputSchema"]["required"],
        json!(["kind", "action", "name", "why", "source"])
    );
    assert_eq!(
        tools[12]["inputSchema"]["properties"]["kind"]["enum"],
        json!([
            "context",
            "glossary",
            "gotcha",
            "skill",
            "personality",
            "prompt"
        ])
    );
    // No write tool of the direct mode, and nothing that could switch anything on.
    for name in WRITE_NAMES {
        assert!(!listed.contains(&name.to_owned()), "{name}");
    }
}

#[test]
fn the_read_only_mode_lists_only_the_reads_and_submit_is_an_unknown_tool() {
    let (_dir, path) = seeded("buffer-readonly");
    let before = std::fs::read(&path).unwrap();
    let reply = session(&path, PROJECT, &[request(1, "tools/list", Value::Null)]);
    let listed = names(&reply[0]);
    assert_eq!(listed, READ_NAMES);
    for tool in ["submit", "submission_get", "log_add"] {
        let reply = session(
            &path,
            PROJECT,
            &[request(
                2,
                "tools/call",
                json!({"name": tool, "arguments": good("context", "set", "x")}),
            )],
        );
        assert_eq!(reply[0]["error"]["code"], INVALID_PARAMS, "{tool}");
        assert_eq!(
            reply[0]["error"]["message"],
            format!("unknown tool '{tool}'")
        );
    }
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "the file is byte-identical"
    );
    assert_eq!(row_count(&path), 0);
}

#[test]
fn the_direct_mode_lists_the_reads_and_the_ten_writes_and_no_submit() {
    let (_dir, path) = seeded("buffer-direct");
    let before = std::fs::read(&path).unwrap();
    let mut output = Vec::new();
    let lines = format!(
        "{}\n{}\n",
        request(1, "tools/list", Value::Null),
        request(
            2,
            "tools/call",
            json!({"name": "submit", "arguments": good("context", "set", "x")})
        )
    );
    Server::new(&path, PROJECT)
        .unwrap()
        .direct()
        .unwrap()
        .serve(Cursor::new(lines), &mut output)
        .unwrap();
    let reply: Vec<Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let listed = names(&reply[0]);
    assert_eq!(listed.len(), 25);
    assert_eq!(listed[..12], READ_NAMES);
    assert_eq!(listed[12..22], WRITE_NAMES);
    assert_eq!(listed[22..], ["gotcha_set", "gotcha_delete", "log_add"]);
    assert!(!listed.contains(&"submit".to_owned()));
    assert!(!listed.contains(&"submission_get".to_owned()));
    assert_eq!(reply[1]["error"]["message"], "unknown tool 'submit'");
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn a_buffer_server_has_no_direct_write_tool() {
    let (_dir, path) = seeded("buffer-no-direct");
    let before = curated(&path);
    for name in WRITE_NAMES {
        let reply = bsession(
            &path,
            PROJECT,
            &[request(
                1,
                "tools/call",
                json!({"name": name, "arguments": {"name": "zzz", "term": "zzz", "kind": "rule",
                    "type": "skill", "text": "x", "definition": "x"}}),
            )],
        );
        assert_eq!(reply[0]["error"]["code"], INVALID_PARAMS, "{name}");
        assert_eq!(
            reply[0]["error"]["message"],
            format!("unknown tool '{name}'")
        );
    }
    assert_eq!(curated(&path), before);
    assert_eq!(row_count(&path), 0);
}

#[test]
fn a_database_that_cannot_be_written_stops_a_buffer_server_at_the_start() {
    let dir = Scratch::new("buffer-missing");
    let path = dir.path().join("none.db");
    let why = Server::new(&path, "")
        .and_then(Server::buffer)
        .err()
        .unwrap();
    assert!(why.contains("none.db"), "{why}");
    assert!(!path.exists(), "starting must not create the database");
}

#[cfg(unix)]
#[test]
fn a_read_only_database_file_is_reported_when_the_buffer_is_asked_for_but_reads_still_work() {
    use std::os::unix::fs::PermissionsExt;

    let (_dir, path) = seeded("buffer-readonly-file");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
    // A superuser can write anything; there is nothing to test then.
    if std::fs::OpenOptions::new().write(true).open(&path).is_ok() {
        return;
    }
    let server = Server::new(&path, "").expect("reading still works");
    let why = server.buffer().err().expect("writing is refused");
    assert!(
        why.contains("harness.db") && why.contains("cannot write") && why.contains("--read-only"),
        "{why}"
    );
}

// ---- submit ----

#[test]
fn every_kind_and_action_is_queued_with_a_number_and_stored_as_asked() {
    let (_dir, path) = seeded("buffer-kinds");
    let mut expected = Vec::new();
    for (n, (kind, action)) in [
        ("context", "set"),
        ("context", "delete"),
        ("glossary", "set"),
        ("glossary", "delete"),
        ("skill", "set"),
        ("skill", "delete"),
        ("personality", "set"),
        ("personality", "delete"),
        ("prompt", "set"),
        ("prompt", "delete"),
    ]
    .into_iter()
    .enumerate()
    {
        let (text, failed) = submit(&path, good(kind, action, &format!("item-{n}")));
        assert!(!failed, "{kind} {action}: {text}");
        assert_eq!(text, format!("queued as #{}", n + 1));
        expected.push((kind, action));
    }
    let store = Store::open_read_only(&path).unwrap();
    let rows = store.list_pending().unwrap();
    assert_eq!(rows.len(), 10);
    for (row, (kind, action)) in rows.iter().zip(expected) {
        assert_eq!((row.kind.as_str(), row.action.as_str()), (kind, action));
        assert_eq!(row.why, "the owner asked for it");
        assert_eq!(row.source, "claude-code");
    }
}

#[test]
fn an_identical_resubmission_is_already_queued_under_the_same_number() {
    let (_dir, path) = seeded("buffer-already");
    let first = queued(&path, good("context", "set", "style-2"));
    let (text, failed) = submit(&path, good("context", "set", "style-2"));
    assert!(!failed);
    assert_eq!(text, format!("already queued as #{first}"));
    // Another sender with another reason, same content: still the same row.
    let mut other = good("context", "set", "style-2");
    other["source"] = json!("someone-else");
    other["why"] = json!("a different reason");
    assert_eq!(
        submit(&path, other).0,
        format!("already queued as #{first}")
    );
    assert_eq!(row_count(&path), 1);
    // A change in the text is a new entry.
    let mut changed = good("context", "set", "style-2");
    changed["text"] = json!("Other text.");
    assert_eq!(
        submit(&path, changed).0,
        format!("queued as #{}", first + 1)
    );
}

#[test]
fn the_project_defaults_to_the_servers_folder_for_context_and_glossary_only() {
    let (_dir, path) = seeded("buffer-project");
    let in_project = queued(&path, good("context", "set", "a"));
    let mut shared = good("context", "set", "b");
    shared["project"] = json!("");
    let every = queued(&path, shared);
    let mut other = good("glossary", "set", "c");
    other["project"] = json!("/other");
    let elsewhere = queued(&path, other);
    let skill = queued(&path, good("skill", "set", "d"));
    let store = Store::open_read_only(&path).unwrap();
    let project_of = |id: i64| store.buffer_entry(id).unwrap().unwrap().project;
    assert_eq!(project_of(in_project), PROJECT);
    assert_eq!(project_of(every), "");
    assert_eq!(project_of(elsewhere), "/other");
    assert_eq!(project_of(skill), "");
    // A server with no folder queues for every project.
    let (text, _) = submit_in(&path, "", good("context", "set", "e"));
    assert!(text.starts_with("queued as #"));
    // A library item has no folder.
    let mut bad = good("skill", "set", "f");
    bad["project"] = json!("/work/app");
    assert!(refused(&path, bad).contains("for every project"));
}

#[test]
fn every_refusal_gives_the_reason_and_adds_no_row() {
    let (_dir, path) = seeded("buffer-refusals");
    let with = |mut base: Value, changes: Value| {
        for (key, value) in changes.as_object().unwrap() {
            base[key] = value.clone();
        }
        base
    };
    let ctx = good("context", "set", "ok");
    let cases = [
        (
            with(ctx.clone(), json!({"name": "has space"})),
            "error: a name is letters",
        ),
        (
            with(ctx.clone(), json!({"name": "n".repeat(61)})),
            "error: a name is at most 60",
        ),
        (
            with(ctx.clone(), json!({"context_kind": "law"})),
            "error: a kind is one of: rule",
        ),
        (
            with(ctx.clone(), json!({"context_kind": Value::Null})),
            "error: a kind is one of: rule",
        ),
        (
            with(ctx.clone(), json!({"text": ""})),
            "error: the text is empty",
        ),
        (
            with(ctx.clone(), json!({"text": "x".repeat(8_001)})),
            "error: an entry is at most 8000 bytes",
        ),
        (
            with(ctx.clone(), json!({"quote": "q".repeat(1_001)})),
            "error: a quote is at most 1000",
        ),
        (
            with(ctx.clone(), json!({"area": "a\nb"})),
            "error: an area is one line",
        ),
        (
            with(ctx.clone(), json!({"project": "/a\n/b"})),
            "error: a project is the folder's path",
        ),
        (
            with(ctx.clone(), json!({"description": "d"})),
            "error: 'description' does not apply to a context",
        ),
        (
            with(ctx.clone(), json!({"action": "add"})),
            "error: an action is one of: set, delete",
        ),
        (
            with(ctx.clone(), json!({"kind": "rule"})),
            "error: a kind is one of: context, glossary",
        ),
        (
            with(ctx.clone(), json!({"kind": "profile"})),
            "error: profiles cannot be submitted",
        ),
        (
            with(ctx.clone(), json!({"kind": "profiles"})),
            "error: profiles cannot be submitted",
        ),
        (
            with(good("context", "delete", "ok"), json!({"text": "x"})),
            "error: a delete takes no text",
        ),
        (
            with(
                good("glossary", "set", "word"),
                json!({"name": "two\nlines"}),
            ),
            "error: a term is one line",
        ),
        (
            with(good("glossary", "set", "word"), json!({"text": ""})),
            "error: the definition is empty",
        ),
        (
            with(
                good("glossary", "set", "word"),
                json!({"text": "d".repeat(2_001)}),
            ),
            "error: a definition is at most 2000",
        ),
        (
            with(good("skill", "set", "terse2"), json!({"name": "on"})),
            "error: ",
        ),
        (
            with(good("skill", "set", "terse2"), json!({"text": ""})),
            "error: the prompt is empty",
        ),
        (
            with(
                good("skill", "set", "terse2"),
                json!({"description": "two\nlines"}),
            ),
            "error: a description is one line",
        ),
        (
            with(
                good("skill", "set", "terse2"),
                json!({"description": "d".repeat(201)}),
            ),
            "error: a description is at most 200",
        ),
        (
            with(
                good("prompt", "set", "p"),
                json!({"text": "t".repeat(200_001)}),
            ),
            "error: the prompt is 200001 bytes",
        ),
        // Missing or mistyped arguments.
        (
            with(ctx.clone(), json!({"name": Value::Null})),
            "error: give 'name'",
        ),
        (
            with(ctx.clone(), json!({"kind": 3})),
            "error: 'kind' must be a string",
        ),
        (
            with(ctx.clone(), json!({"text": ["a"]})),
            "error: 'text' must be a string",
        ),
        (
            with(ctx.clone(), json!({"why": Value::Null})),
            "error: why is required",
        ),
        (
            with(ctx.clone(), json!({"source": Value::Null})),
            "error: source is required",
        ),
    ];
    for (arguments, reason) in cases {
        let text = refused(&path, arguments.clone());
        assert!(
            text.starts_with(reason),
            "{arguments}: wanted '{reason}', got '{text}'"
        );
    }
    assert_eq!(row_count(&path), 0);
}

#[test]
fn why_and_source_are_required_and_bounded_through_the_tool() {
    let (_dir, path) = seeded("buffer-why-source");
    let set = |key: &str, value: Value| {
        let mut entry = good("context", "set", "ok");
        entry[key] = value;
        entry
    };
    for why in [json!(""), json!("   ")] {
        assert!(refused(&path, set("why", why)).starts_with("error: why is required"));
    }
    assert_eq!(
        refused(&path, set("why", json!("w".repeat(1_001)))),
        "error: why is at most 1000 characters"
    );
    for source in [json!(""), json!(" ")] {
        assert!(refused(&path, set("source", source)).starts_with("error: source is required"));
    }
    assert_eq!(
        refused(&path, set("source", json!("s".repeat(101)))),
        "error: source is at most 100 characters"
    );
    assert_eq!(
        refused(&path, set("source", json!("two\nlines"))),
        "error: source is one line of plain text"
    );
    // The limits themselves are fine.
    let mut limits = good("context", "set", "ok");
    limits["why"] = json!("w".repeat(1_000));
    limits["source"] = json!("s".repeat(100));
    queued(&path, limits);
}

#[test]
fn a_profile_cannot_be_submitted_in_any_form() {
    let (_dir, path) = seeded("buffer-profile");
    let before = curated(&path);
    for arguments in [
        json!({"kind": "profile", "action": "set", "name": "x", "text": "t", "why": "w", "source": "s"}),
        json!({"kind": "profile", "action": "delete", "name": "work", "why": "w", "source": "s"}),
        json!({"kind": "PROFILE", "action": "set", "name": "x", "why": "w", "source": "s",
            "description": "d"}),
    ] {
        assert!(refused(&path, arguments).contains("profiles cannot be submitted"));
    }
    assert_eq!(curated(&path), before);
}

#[test]
fn a_flood_stops_at_the_cap_of_200_pending() {
    let (_dir, path) = seeded("buffer-flood");
    for n in 0..MAX_PENDING {
        let (text, failed) = submit(&path, good("context", "set", &format!("flood-{n}")));
        assert!(!failed, "{n}: {text}");
    }
    assert_eq!(row_count(&path), 200);
    let (text, failed) = submit(&path, good("context", "set", "flood-200"));
    assert!(failed);
    assert_eq!(text, "error: the inbox is full: ask the owner to review it");
    assert_eq!(row_count(&path), 200);
    // What is queued already still answers with its number.
    assert_eq!(
        submit(&path, good("context", "set", "flood-7")),
        ("already queued as #8".to_owned(), false)
    );
    // Room returns when the owner decides one.
    let mut owner = Store::open(&path).unwrap();
    store_buffer::reject(&mut owner, 1, "enough").unwrap();
    drop(owner);
    assert_eq!(
        submit(&path, good("context", "set", "flood-200")),
        ("queued as #201".to_owned(), false)
    );
}

#[test]
fn submitting_changes_no_curated_table_and_only_adds_rows_to_the_inbox() {
    let (_dir, path) = seeded("buffer-untouched");
    let before = curated(&path);
    let tables_before = other_tables(&path);
    // Everything that could be said, including entries that name things that exist.
    let mut count = 0;
    for kind in ["context", "glossary", "skill", "personality", "prompt"] {
        for name in ["style", "ledger", "terse", "calm", "house", "new-one"] {
            for action in ["set", "delete"] {
                let (text, failed) = submit(&path, good(kind, action, name));
                assert!(!failed, "{kind} {action} {name}: {text}");
                count += 1;
            }
        }
    }
    assert_eq!(row_count(&path), count);
    assert_eq!(
        curated(&path),
        before,
        "a submission changed what the owner keeps"
    );
    assert_eq!(other_tables(&path), tables_before);
    // Not a single prompt was switched on or selected either.
    let store = Store::open_read_only(&path).unwrap();
    assert_eq!(
        store
            .list_prompts()
            .unwrap()
            .iter()
            .filter(|p| p.enabled)
            .count(),
        before.matches("enabled: true").count()
    );
}

/// The row counts of every table but the inbox.
fn other_tables(path: &Path) -> Vec<(String, i64)> {
    let store = Store::open_read_only(path).unwrap();
    let mut statement = store
        .conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name <> 'buffer_entries' ORDER BY name")
        .unwrap();
    let names: Vec<String> = statement
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    names
        .into_iter()
        .map(|name| {
            let n = store
                .conn
                .query_row(&format!("SELECT COUNT(*) FROM \"{name}\""), [], |r| {
                    r.get(0)
                })
                .unwrap();
            (name, n)
        })
        .collect()
}

#[test]
fn the_reads_of_a_buffer_server_answer_as_those_of_a_read_only_one() {
    let (_dir, path) = seeded("buffer-reads");
    let lines = [
        request(1, "tools/call", json!({"name": "context_list"})),
        request(
            2,
            "tools/call",
            json!({"name": "library_get", "arguments": {"type": "prompt", "name": "house"}}),
        ),
        request(3, "tools/call", json!({"name": "profile_list"})),
    ];
    assert_eq!(
        bsession(&path, PROJECT, &lines),
        session(&path, PROJECT, &lines)
    );
}

// ---- submission_get ----

#[test]
fn submission_get_gives_the_status_and_the_owners_note_when_rejected() {
    let (_dir, path) = seeded("buffer-get");
    let pending = queued(&path, good("context", "set", "a"));
    let applied = queued(&path, good("context", "set", "b"));
    let rejected = queued(&path, good("context", "set", "c"));
    let silent = queued(&path, good("context", "set", "d"));
    let mut owner = Store::open(&path).unwrap();
    assert!(matches!(
        store_buffer::apply_entry(&mut owner, applied, None).unwrap(),
        ApplyOutcome::Applied(_)
    ));
    store_buffer::reject(&mut owner, rejected, "too vague").unwrap();
    store_buffer::reject(&mut owner, silent, "").unwrap();
    drop(owner);
    let get = |id: Value| bcall_in(&path, PROJECT, "submission_get", json!({ "id": id }));
    assert_eq!(get(json!(pending)), (format!("#{pending}: pending"), false));
    assert_eq!(get(json!(applied)), (format!("#{applied}: applied"), false));
    assert_eq!(
        get(json!(rejected)),
        (format!("#{rejected}: rejected: too vague"), false)
    );
    assert_eq!(get(json!(silent)), (format!("#{silent}: rejected"), false));
    // The number as it was answered.
    assert_eq!(
        get(json!(format!("#{pending}"))).0,
        format!("#{pending}: pending")
    );
    assert_eq!(
        get(json!(999)),
        ("error: no submission #999".to_owned(), true)
    );
    assert!(get(json!("x")).1);
    assert!(get(json!(1.5)).1);
    assert_eq!(
        bcall_in(&path, PROJECT, "submission_get", json!({})),
        ("error: give 'id'".to_owned(), true)
    );
}

#[test]
fn submission_get_exposes_nothing_else_not_even_an_apply_error() {
    let (_dir, path) = seeded("buffer-get-private");
    let mut entry = good("skill", "set", "calm");
    entry["text"] = json!("A skill with the name of a personality.");
    entry["why"] = json!("secret reason");
    entry["source"] = json!("secret-source");
    let id = queued(&path, entry);
    let mut owner = Store::open(&path).unwrap();
    assert!(matches!(
        store_buffer::apply_entry(&mut owner, id, None).unwrap(),
        ApplyOutcome::Failed(_)
    ));
    assert!(
        !owner
            .buffer_entry(id)
            .unwrap()
            .unwrap()
            .decision_note
            .is_empty()
    );
    drop(owner);
    let (text, failed) = bcall_in(&path, PROJECT, "submission_get", json!({"id": id}));
    assert!(!failed);
    assert_eq!(text, format!("#{id}: pending"));
    for secret in ["secret", "personality", "calm", "skill"] {
        assert!(!text.contains(secret));
    }
}

#[test]
fn a_whole_conversation_initializes_lists_submits_and_asks_for_the_outcome() {
    // The whole conversation of a harness: initialize, list, submit, ask.
    let (_dir, path) = seeded("buffer-conversation");
    let reply = bsession(
        &path,
        PROJECT,
        &[
            request(1, "initialize", json!({"protocolVersion": "2025-06-18"})),
            request(2, "tools/list", Value::Null),
            request(
                3,
                "tools/call",
                json!({"name": "submit", "arguments": good("glossary", "set", "claim")}),
            ),
            request(
                4,
                "tools/call",
                json!({"name": "submission_get", "arguments": {"id": 1}}),
            ),
        ],
    );
    assert_eq!(reply.len(), 4);
    assert_eq!(reply[2]["result"]["content"][0]["text"], "queued as #1");
    assert_eq!(reply[2]["result"]["isError"], false);
    assert_eq!(reply[3]["result"]["content"][0]["text"], "#1: pending");
}

//! Tests of the write tools of `--mcp-serve --direct` (issue 46). Every test works on a temporary
//! database made by `seeded`; none opens the real `harness.db` or touches the network.

use std::io::Cursor;
use std::path::Path;

use serde_json::{Value, json};

use super::tests::{PROJECT, call as read_call, request, seeded};
use super::write::{
    apply_profile_delete, apply_profile_edit, apply_prompt_delete, apply_prompt_edit,
};
use super::*;
use crate::tools::testutil::Scratch;

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

/// Sends the lines to a direct server and returns what it wrote, one value per answer.
fn dsession(path: &Path, project: &str, lines: &[Value]) -> Vec<Value> {
    let input: String = lines.iter().map(|l| format!("{l}\n")).collect();
    let mut output = Vec::new();
    Server::new(path, project)
        .unwrap()
        .direct()
        .unwrap()
        .serve(Cursor::new(input), &mut output)
        .unwrap();
    String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Calls one tool on a direct server, started in `project`.
pub(super) fn dcall_in(path: &Path, project: &str, tool: &str, arguments: Value) -> (String, bool) {
    let reply = dsession(
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

/// A write that has to work: the text of the answer.
fn done(path: &Path, tool: &str, arguments: Value) -> String {
    let (text, failed) = dcall_in(path, PROJECT, tool, arguments);
    assert!(!failed, "{tool} failed: {text}");
    text
}

/// A write that has to be refused and leave the database as it was: the reason.
fn refused(path: &Path, tool: &str, arguments: Value) -> String {
    let before = snapshot(path);
    let (text, failed) = dcall_in(path, PROJECT, tool, arguments);
    assert!(failed, "{tool} should have been refused: {text}");
    assert_eq!(snapshot(path), before, "{tool} changed the database");
    text
}

/// A read through the read-only server, started in `PROJECT`.
fn read(path: &Path, tool: &str, arguments: Value) -> String {
    read_call(path, PROJECT, tool, arguments).0
}

/// Everything the tools can change, as one text: equal before and after means nothing was written.
fn snapshot(path: &Path) -> String {
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

fn names_of(path: &Path) -> Vec<String> {
    let reply = dsession(path, "", &[request(1, "tools/list", Value::Null)]);
    reply[0]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn direct_lists_the_twelve_read_tools_then_the_ten_write_tools_and_the_notes_tools() {
    let (_dir, path) = seeded("direct-list");
    let names = names_of(&path);
    let read_names = [
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
    assert_eq!(names.len(), 25);
    assert_eq!(names[..12], read_names);
    assert_eq!(names[12..22], WRITE_NAMES);
    assert_eq!(names[22..], ["gotcha_set", "gotcha_delete", "log_add"]);
    let reply = dsession(&path, "", &[request(1, "tools/list", Value::Null)]);
    for tool in &reply[0]["result"]["tools"].as_array().unwrap()[12..] {
        let name = tool["name"].as_str().unwrap();
        assert!(
            tool["description"].as_str().unwrap().starts_with("Writes."),
            "{name}"
        );
        assert_eq!(tool["inputSchema"]["type"], "object", "{name}");
        assert!(tool["inputSchema"]["properties"].is_object(), "{name}");
    }
    // The first twelve are the ones the read-only server lists, unchanged.
    for tool in &reply[0]["result"]["tools"].as_array().unwrap()[..12] {
        assert!(
            tool["description"]
                .as_str()
                .unwrap()
                .starts_with("Read-only.")
        );
    }
}

#[test]
fn the_kind_list_in_the_context_set_schema_is_the_stores() {
    let schema: Value = serde_json::from_str(WRITE_TOOLS[0].schema).unwrap();
    assert_eq!(WRITE_TOOLS[0].name, "context_set");
    let kinds: Vec<&str> = schema["properties"]["kind"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap())
        .collect();
    assert_eq!(kinds, crate::store_context::KINDS);
}

#[test]
fn without_direct_no_write_tool_is_listed_or_callable_and_the_file_is_untouched() {
    let (_dir, path) = seeded("direct-off");
    let before = std::fs::read(&path).unwrap();
    let snap = snapshot(&path);
    let reply = super::tests::session(&path, PROJECT, &[request(1, "tools/list", Value::Null)]);
    let listed: Vec<&str> = reply[0]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(listed.len(), 12);
    for name in WRITE_NAMES {
        assert!(!listed.contains(&name), "{name} is listed");
        let reply = super::tests::session(
            &path,
            PROJECT,
            &[request(
                2,
                "tools/call",
                json!({"name": name, "arguments": {
                    "name": "zzz", "term": "zzz", "kind": "rule", "type": "skill",
                    "text": "x", "definition": "x"}}),
            )],
        );
        assert_eq!(reply[0]["error"]["code"], INVALID_PARAMS, "{name}");
        assert_eq!(
            reply[0]["error"]["message"],
            format!("unknown tool '{name}'")
        );
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(snapshot(&path), snap);
}

#[test]
fn a_direct_server_still_answers_an_unknown_tool_with_a_protocol_error() {
    let (_dir, path) = seeded("direct-unknown");
    let reply = dsession(
        &path,
        "",
        &[request(1, "tools/call", json!({"name": "context_wipe"}))],
    );
    assert_eq!(reply[0]["error"]["code"], INVALID_PARAMS);
}

#[test]
fn context_set_adds_replaces_says_no_change_and_deletes() {
    let (_dir, path) = seeded("direct-context");
    let added = done(
        &path,
        "context_set",
        json!({"name": "api-rule", "kind": "rule", "text": "Version the API.", "quote": "version it", "area": "api"}),
    );
    assert_eq!(
        added,
        "added context entry 'api-rule' for project /work/app"
    );
    assert_eq!(
        read(&path, "context_get", json!({"name": "api-rule"})),
        "api-rule (rule, area api)\nVersion the API.\nthe owner said: version it"
    );
    // Same again: nothing to do.
    let same = done(
        &path,
        "context_set",
        json!({"name": "api-rule", "kind": "rule", "text": "Version the API.", "quote": "version it", "area": "api"}),
    );
    assert_eq!(
        same,
        "no change: context entry 'api-rule' for project /work/app already says this"
    );
    // A new text replaces; an omitted area and an empty quote keep the stored ones.
    let replaced = done(
        &path,
        "context_set",
        json!({"name": "api-rule", "kind": "rule", "text": "Version the API in the path."}),
    );
    assert_eq!(
        replaced,
        "replaced context entry 'api-rule' for project /work/app"
    );
    assert_eq!(
        read(&path, "context_get", json!({"name": "api-rule"})),
        "api-rule (rule, area api)\nVersion the API in the path.\nthe owner said: version it"
    );
    let gone = done(&path, "context_delete", json!({"name": "api-rule"}));
    assert_eq!(
        gone,
        "deleted context entry 'api-rule' for project /work/app"
    );
    let (miss, _) = read_call(&path, PROJECT, "context_get", json!({"name": "api-rule"}));
    assert!(
        miss.starts_with("error: no entry named 'api-rule'"),
        "{miss}"
    );
    let again = refused(&path, "context_delete", json!({"name": "api-rule"}));
    assert_eq!(
        again,
        "error: no context entry 'api-rule' for project /work/app"
    );
}

#[test]
fn context_set_refuses_what_the_store_refuses_and_writes_nothing() {
    let (_dir, path) = seeded("direct-context-bad");
    let cases = [
        (
            json!({"name": "bad name!", "kind": "rule", "text": "x"}),
            "letters, digits",
        ),
        (
            json!({"name": ".hidden", "kind": "rule", "text": "x"}),
            "not starting with a dot",
        ),
        (
            json!({"name": "ok", "kind": "memo", "text": "x"}),
            "a kind is one of: rule, decision, shape, guide",
        ),
        (
            json!({"name": "ok", "kind": "rule", "text": "   "}),
            "error: give 'text'",
        ),
        (json!({"kind": "rule", "text": "x"}), "error: give 'name'"),
        (
            json!({"name": "ok", "kind": "rule", "text": "x".repeat(8_001)}),
            "at most 8000 bytes",
        ),
        (
            json!({"name": "ok", "kind": "rule", "text": "x", "quote": "q".repeat(1_001)}),
            "a quote is at most 1000 characters",
        ),
        (
            json!({"name": "ok", "kind": "rule", "text": "x", "area": "a\nb"}),
            "an area is one line",
        ),
        (
            json!({"name": "ok", "kind": "rule", "text": "x", "project": "a\nb"}),
            "a project is the folder's path on one line",
        ),
        (
            json!({"name": 5, "kind": "rule", "text": "x"}),
            "error: 'name' must be a string",
        ),
    ];
    for (arguments, expected) in cases {
        let why = refused(&path, "context_set", arguments.clone());
        assert!(why.contains(expected), "{arguments}: {why}");
    }
}

#[test]
fn context_writes_follow_the_project_and_delete_only_the_exact_scope() {
    let (_dir, path) = seeded("direct-context-scope");
    // "style" is in both the shared scope and the project's own.
    let (_, failed) = dcall_in(&path, PROJECT, "context_delete", json!({"name": "style"}));
    assert!(!failed);
    // The project's entry is gone; the shared one is what applies now.
    assert_eq!(
        read(&path, "context_get", json!({"name": "style"})),
        "style (rule)\nWrite plainly.\nthe owner said: keep it plain"
    );
    // A shared entry is not deleted by a delete in a project.
    let why = refused(&path, "context_delete", json!({"name": "billing-view"}));
    assert_eq!(
        why,
        "error: no context entry 'billing-view' for project /work/app"
    );
    let shared = done(
        &path,
        "context_delete",
        json!({"name": "billing-view", "project": ""}),
    );
    assert_eq!(
        shared,
        "deleted context entry 'billing-view' for every project"
    );
    // An explicit project beats the folder the server started in.
    let other = done(
        &path,
        "context_set",
        json!({"name": "far", "kind": "guide", "text": "Far away.", "project": "/other"}),
    );
    assert_eq!(other, "added context entry 'far' for project /other");
    let (miss, failed) = read_call(&path, PROJECT, "context_get", json!({"name": "far"}));
    assert!(failed, "{miss}");
    assert_eq!(
        read(
            &path,
            "context_get",
            json!({"name": "far", "project": "/other"})
        ),
        "far (guide)\nFar away."
    );
    // An empty project writes for every project, and a project's same-named entry is separate.
    let every = done(
        &path,
        "context_set",
        json!({"name": "far", "kind": "guide", "text": "Everywhere.", "project": ""}),
    );
    assert_eq!(every, "added context entry 'far' for every project");
    assert_eq!(
        read(&path, "context_get", json!({"name": "far", "project": ""})),
        "far (guide)\nEverywhere."
    );
    assert_eq!(
        read(
            &path,
            "context_get",
            json!({"name": "far", "project": "/other"})
        ),
        "far (guide)\nFar away."
    );
    // The server started with no project writes for every project by default.
    let (text, failed) = dcall_in(
        &path,
        "",
        "context_set",
        json!({"name": "from-nowhere", "kind": "rule", "text": "x"}),
    );
    assert!(!failed);
    assert_eq!(text, "added context entry 'from-nowhere' for every project");
    // Deleting the /other one leaves the shared one.
    done(
        &path,
        "context_delete",
        json!({"name": "far", "project": "/other"}),
    );
    assert_eq!(
        read(&path, "context_get", json!({"name": "far"})),
        "far (guide)\nEverywhere."
    );
}

#[test]
fn glossary_set_adds_replaces_says_no_change_and_deletes() {
    let (_dir, path) = seeded("direct-glossary");
    let added = done(
        &path,
        "glossary_set",
        json!({"term": "invoice", "definition": "A bill.", "area": "billing"}),
    );
    assert_eq!(added, "added glossary term 'invoice' for project /work/app");
    assert_eq!(
        read(&path, "glossary_get", json!({"term": "invoice"})),
        "term: invoice\narea: billing\ndefinition: A bill."
    );
    let same = done(
        &path,
        "glossary_set",
        json!({"term": "invoice", "definition": "A bill.", "area": "billing"}),
    );
    assert_eq!(
        same,
        "no change: glossary term 'invoice' for project /work/app already says this"
    );
    // An omitted area keeps the stored one.
    let replaced = done(
        &path,
        "glossary_set",
        json!({"term": "invoice", "definition": "A bill sent to a customer."}),
    );
    assert_eq!(
        replaced,
        "replaced glossary term 'invoice' for project /work/app"
    );
    assert_eq!(
        read(&path, "glossary_get", json!({"term": "invoice"})),
        "term: invoice\narea: billing\ndefinition: A bill sent to a customer."
    );
    let gone = done(&path, "glossary_delete", json!({"term": "invoice"}));
    assert_eq!(
        gone,
        "deleted glossary term 'invoice' for project /work/app"
    );
    let (miss, failed) = read_call(&path, PROJECT, "glossary_get", json!({"term": "invoice"}));
    assert!(failed, "{miss}");
    let again = refused(&path, "glossary_delete", json!({"term": "invoice"}));
    assert_eq!(
        again,
        "error: no glossary term 'invoice' for project /work/app"
    );
}

#[test]
fn glossary_set_refuses_what_the_store_refuses_and_writes_nothing() {
    let (_dir, path) = seeded("direct-glossary-bad");
    let cases = [
        (
            json!({"term": "a\nb", "definition": "x"}),
            "a term is one line of plain text",
        ),
        (
            json!({"term": "t".repeat(200), "definition": "x"}),
            "a term is at most",
        ),
        (
            json!({"term": "ok", "definition": "  "}),
            "error: give 'definition'",
        ),
        (json!({"definition": "x"}), "error: give 'term'"),
        (
            json!({"term": "ok", "definition": "d".repeat(5_000)}),
            "a definition is at most",
        ),
        (
            json!({"term": "ok", "definition": "x", "area": "a\nb"}),
            "an area is one line",
        ),
        (
            json!({"term": "ok", "definition": 3}),
            "error: 'definition' must be a string",
        ),
    ];
    for (arguments, expected) in cases {
        let why = refused(&path, "glossary_set", arguments.clone());
        assert!(why.contains(expected), "{arguments}: {why}");
    }
}

#[test]
fn glossary_writes_follow_the_project_and_delete_only_the_exact_scope() {
    let (_dir, path) = seeded("direct-glossary-scope");
    // "ledger" is a shared word: a delete in the project does not reach it.
    let why = refused(&path, "glossary_delete", json!({"term": "ledger"}));
    assert_eq!(
        why,
        "error: no glossary term 'ledger' for project /work/app"
    );
    let shared = done(
        &path,
        "glossary_delete",
        json!({"term": "ledger", "project": ""}),
    );
    assert_eq!(shared, "deleted glossary term 'ledger' for every project");
    // The project's own word and a shared one of the same term are separate rows.
    done(
        &path,
        "glossary_set",
        json!({"term": "settled order", "definition": "Everywhere too.", "project": ""}),
    );
    let (text, failed) = dcall_in(
        &path,
        PROJECT,
        "glossary_delete",
        json!({"term": "settled order", "project": "/other"}),
    );
    assert!(failed, "{text}");
    assert_eq!(
        text,
        "error: no glossary term 'settled order' for project /other"
    );
    done(&path, "glossary_delete", json!({"term": "settled order"}));
    assert_eq!(
        read(&path, "glossary_get", json!({"term": "settled order"})),
        "term: settled order\ndefinition: Everywhere too."
    );
    // An explicit project is written for, whatever folder the server started in.
    let far = done(
        &path,
        "glossary_set",
        json!({"term": "far word", "definition": "Far.", "project": "/other"}),
    );
    assert_eq!(far, "added glossary term 'far word' for project /other");
    let (text, failed) = read_call(&path, PROJECT, "glossary_get", json!({"term": "far word"}));
    assert!(failed, "{text}");
}

#[test]
fn library_add_saves_each_kind_switched_off() {
    let (_dir, path) = seeded("direct-library-add");
    let skill = done(
        &path,
        "library_add",
        json!({"type": "skill", "name": "review", "text": "Review the diff.", "description": "code review"}),
    );
    assert_eq!(skill, "added skill 'review', switched off");
    let person = done(
        &path,
        "library_add",
        json!({"type": "personality", "name": "dry", "text": "Be dry."}),
    );
    assert_eq!(person, "added personality 'dry', switched off");
    let prompt = done(
        &path,
        "library_add",
        json!({"type": "prompt", "name": "strict", "text": "Be strict."}),
    );
    assert_eq!(prompt, "added system prompt 'strict', switched off");
    assert_eq!(
        read(
            &path,
            "library_get",
            json!({"type": "skill", "name": "review"})
        ),
        "review (skill)\nDescription: code review\n\nReview the diff."
    );
    let listed = read(&path, "library_list", json!({"type": "prompt"}));
    assert!(listed.lines().any(|l| l == "strict [off]"), "{listed}");
    assert_eq!(
        read(
            &path,
            "library_get",
            json!({"type": "prompt", "name": "strict"})
        ),
        "strict (system prompt, off)\nDescription: \n\nBe strict."
    );
    // None of them is in the profile new chats start under.
    let store = Store::open_read_only(&path).unwrap();
    let selected = store.selected_profile().unwrap();
    let profile = store.profile(&selected).unwrap();
    assert!(!profile.prompts.contains(&"strict".to_owned()));
    assert!(!profile.skills.contains(&"review".to_owned()));
    assert_ne!(profile.personality.as_deref(), Some("dry"));
}

#[test]
fn library_add_refuses_what_the_store_refuses_and_writes_nothing() {
    let (_dir, path) = seeded("direct-library-add-bad");
    let cases = [
        (
            json!({"type": "skill", "name": "bad name", "text": "x"}),
            "the name may only use letters",
        ),
        (
            json!({"type": "skill", "name": "-lead", "text": "x"}),
            "must not start with",
        ),
        (
            json!({"type": "skill", "name": "ok", "text": "  "}),
            "error: give 'text'",
        ),
        (
            json!({"type": "skill", "name": "ok", "text": "x", "description": "two\nlines"}),
            "a description is one line of plain text",
        ),
        (
            json!({"type": "skill", "name": "ok", "text": "x", "description": "d".repeat(201)}),
            "a description is at most 200 characters",
        ),
        (
            json!({"type": "skill", "name": "terse", "text": "x"}),
            "already a prompt named terse",
        ),
        (
            json!({"type": "tool", "name": "ok", "text": "x"}),
            "error: type is skill, personality or prompt, not 'tool'",
        ),
        (json!({"name": "ok", "text": "x"}), "error: give 'type'"),
        (
            json!({"type": "skill", "name": "ok", "text": "x".repeat(crate::store_prompts::MAX_PROMPT_BYTES + 1)}),
            "the limit is",
        ),
    ];
    for (arguments, expected) in cases {
        let why = refused(&path, "library_add", arguments.clone());
        assert!(why.contains(expected), "{arguments}: {why}");
    }
}

#[test]
fn library_edit_changes_renames_says_no_change_and_keeps_whether_it_is_on() {
    let (_dir, path) = seeded("direct-library-edit");
    let mut store = Store::open(&path).unwrap();
    store.set_prompt_enabled("house", true).unwrap();
    drop(store);
    let changed = done(
        &path,
        "library_edit",
        json!({"type": "prompt", "name": "HOUSE", "text": "You are very careful."}),
    );
    assert_eq!(changed, "changed system prompt 'house'");
    let listed = read(&path, "library_list", json!({"type": "prompt"}));
    assert!(
        listed.lines().any(|l| l == "house [on] - the house prompt"),
        "{listed}"
    );
    let renamed = done(
        &path,
        "library_edit",
        json!({"type": "prompt", "name": "house", "new_name": "home", "description": "the home prompt"}),
    );
    assert_eq!(renamed, "changed system prompt 'house', now named 'home'");
    assert_eq!(
        read(
            &path,
            "library_get",
            json!({"type": "prompt", "name": "home"})
        ),
        "home (system prompt, on)\nDescription: the home prompt\n\nYou are very careful."
    );
    // The profile that used it follows the rename.
    assert!(read(&path, "profile_get", json!({"name": "work"})).contains("System prompts: home"),);
    let same = done(
        &path,
        "library_edit",
        json!({"type": "prompt", "name": "home", "text": "You are very careful."}),
    );
    assert_eq!(same, "no change: system prompt 'home' already says this");
    // An empty description clears it.
    done(
        &path,
        "library_edit",
        json!({"type": "skill", "name": "terse", "description": ""}),
    );
    assert_eq!(
        read(&path, "library_list", json!({"type": "skill"})),
        "terse"
    );
}

#[test]
fn library_edit_refuses_what_the_store_refuses_and_writes_nothing() {
    let (_dir, path) = seeded("direct-library-edit-bad");
    let cases = [
        (
            json!({"type": "skill", "name": "nope", "text": "x"}),
            "error: no skill named 'nope'; the skills are: terse",
        ),
        (
            json!({"type": "skill", "name": "calm", "text": "x"}),
            "error: calm is a personality, not a skill",
        ),
        (
            json!({"type": "skill", "name": "terse"}),
            "error: give new_name, text or description",
        ),
        (
            json!({"type": "skill", "name": "terse", "text": ""}),
            "the prompt is empty",
        ),
        (
            json!({"type": "skill", "name": "terse", "new_name": "bad name"}),
            "the name may only use letters",
        ),
        (
            json!({"type": "skill", "name": "terse", "description": "a\nb"}),
            "a description is one line of plain text",
        ),
        (
            json!({"type": "skill", "name": "terse", "new_name": "house"}),
            "already a prompt named house",
        ),
        (
            json!({"type": "skill", "name": "terse", "text": 4}),
            "error: 'text' must be a string",
        ),
    ];
    for (arguments, expected) in cases {
        let why = refused(&path, "library_edit", arguments.clone());
        assert!(why.contains(expected), "{arguments}: {why}");
    }
}

#[test]
fn library_delete_removes_the_item_and_takes_it_out_of_the_profiles_that_used_it() {
    let (_dir, path) = seeded("direct-library-delete");
    assert_eq!(
        read(&path, "profile_get", json!({"name": "work"})),
        "work\nDescription: the daily setup\nSystem prompts: house\nSkills on: terse\nPersonality: calm"
    );
    assert_eq!(
        done(
            &path,
            "library_delete",
            json!({"type": "skill", "name": "terse"})
        ),
        "deleted skill 'terse'"
    );
    assert_eq!(
        read(&path, "library_list", json!({"type": "skill"})),
        "no skills saved"
    );
    assert_eq!(
        done(
            &path,
            "library_delete",
            json!({"type": "personality", "name": "calm"})
        ),
        "deleted personality 'calm'"
    );
    assert_eq!(
        done(
            &path,
            "library_delete",
            json!({"type": "prompt", "name": "house"})
        ),
        "deleted system prompt 'house'"
    );
    assert_eq!(
        read(&path, "profile_get", json!({"name": "work"})),
        "work\nDescription: the daily setup\nSystem prompts: none\nSkills on: none\nPersonality: none"
    );
    let prompts = read(&path, "library_list", json!({"type": "prompt"}));
    assert!(!prompts.contains("house"), "{prompts}");
    // The profile itself is still there.
    assert!(read(&path, "profile_list", json!({})).contains("work - the daily setup"));
    // A second delete finds nothing; the wrong kind says which kind it is.
    let why = refused(
        &path,
        "library_delete",
        json!({"type": "skill", "name": "terse"}),
    );
    assert_eq!(why, "error: no skill named 'terse'; the skills are: none");
    let why = refused(
        &path,
        "library_delete",
        json!({"type": "skill", "name": "base"}),
    );
    assert!(why.contains("is a system prompt, not a skill"), "{why}");
}

#[test]
fn profile_add_makes_a_profile_from_existing_members_only() {
    let (_dir, path) = seeded("direct-profile-add");
    let added = done(
        &path,
        "profile_add",
        json!({"name": "review", "description": "reads code", "prompts": ["house"], "skills": ["terse"], "personality": "calm"}),
    );
    assert_eq!(added, "added profile 'review'");
    assert_eq!(
        read(&path, "profile_get", json!({"name": "review"})),
        "review\nDescription: reads code\nSystem prompts: house\nSkills on: terse\nPersonality: calm"
    );
    // Members are found in any case and stored under their own names.
    done(
        &path,
        "profile_add",
        json!({"name": "loud", "prompts": ["HOUSE", "house"], "skills": ["Terse"], "personality": "CALM"}),
    );
    assert_eq!(
        read(&path, "profile_get", json!({"name": "loud"})),
        "loud\nDescription: \nSystem prompts: house\nSkills on: terse\nPersonality: calm"
    );
    let bare = done(&path, "profile_add", json!({"name": "bare"}));
    assert_eq!(bare, "added profile 'bare'");
    assert_eq!(
        read(&path, "profile_get", json!({"name": "bare"})),
        "bare\nDescription: \nSystem prompts: none\nSkills on: none\nPersonality: none"
    );
    // The selected profile is still the one new chats start under.
    assert_eq!(
        Store::open_read_only(&path)
            .unwrap()
            .selected_profile()
            .unwrap(),
        "default"
    );
}

#[test]
fn profile_add_refuses_what_the_store_refuses_and_creates_no_profile() {
    let (_dir, path) = seeded("direct-profile-add-bad");
    let cases = [
        (json!({"name": "bad name"}), "the name may only use letters"),
        (json!({"name": "work"}), "already"),
        (
            json!({"name": "x", "description": "a\nb"}),
            "a description is one line of plain text",
        ),
        (
            json!({"name": "x", "prompts": ["nope"]}),
            "error: no system prompt named 'nope'; the system prompts are:",
        ),
        (
            json!({"name": "x", "prompts": ["house", "nope"]}),
            "error: no system prompt named 'nope'",
        ),
        (
            json!({"name": "x", "prompts": ["terse"]}),
            "error: terse is a skill, not a system prompt",
        ),
        (
            json!({"name": "x", "skills": ["house"]}),
            "error: house is a system prompt, not a skill",
        ),
        (
            json!({"name": "x", "personality": "terse"}),
            "error: terse is a skill, not a personality",
        ),
        (
            json!({"name": "x", "personality": "nobody"}),
            "error: no personality named 'nobody'",
        ),
        (
            json!({"name": "x", "prompts": "house"}),
            "error: 'prompts' must be a list of names",
        ),
        (
            json!({"name": "x", "skills": [1]}),
            "error: 'skills' must be a list of names",
        ),
        (json!({}), "error: give 'name'"),
    ];
    for (arguments, expected) in cases {
        let why = refused(&path, "profile_add", arguments.clone());
        assert!(why.contains(expected), "{arguments}: {why}");
    }
    let profiles = Store::open_read_only(&path)
        .unwrap()
        .list_profiles()
        .unwrap();
    assert!(profiles.iter().all(|p| p.name != "x"));
}

#[test]
fn profile_edit_replaces_lists_renames_clears_and_says_no_change() {
    let (_dir, path) = seeded("direct-profile-edit");
    let mut store = Store::open(&path).unwrap();
    store
        .add_prompt_of_type("brisk", "", "Be quick.", None, PromptType::Skill)
        .unwrap();
    drop(store);
    let changed = done(
        &path,
        "profile_edit",
        json!({"name": "WORK", "skills": ["brisk", "terse"], "description": "the new setup"}),
    );
    assert_eq!(changed, "changed profile 'work'");
    let shown = read(&path, "profile_get", json!({"name": "work"}));
    assert!(shown.contains("Description: the new setup"), "{shown}");
    assert!(shown.contains("Skills on: terse, brisk"), "{shown}");
    assert!(shown.contains("System prompts: house"), "{shown}");
    // An empty personality means none; an empty list empties the list.
    done(
        &path,
        "profile_edit",
        json!({"name": "work", "personality": "", "prompts": []}),
    );
    let shown = read(&path, "profile_get", json!({"name": "work"}));
    assert!(shown.contains("Personality: none"), "{shown}");
    assert!(shown.contains("System prompts: none"), "{shown}");
    let renamed = done(
        &path,
        "profile_edit",
        json!({"name": "work", "new_name": "daily"}),
    );
    assert_eq!(renamed, "changed profile 'work', now named 'daily'");
    let same = done(
        &path,
        "profile_edit",
        json!({"name": "daily", "skills": ["brisk", "terse"]}),
    );
    // The store keeps the library's order, so the order given is not a change.
    assert_eq!(same, "no change: profile 'daily' already says this");
    let same = done(
        &path,
        "profile_edit",
        json!({"name": "daily", "description": "the new setup"}),
    );
    assert_eq!(same, "no change: profile 'daily' already says this");
}

#[test]
fn profile_edit_refuses_what_the_store_refuses_and_writes_nothing() {
    let (_dir, path) = seeded("direct-profile-edit-bad");
    let cases = [
        (
            json!({"name": "nope", "description": "x"}),
            "error: no profile named 'nope'; the profiles are:",
        ),
        (
            json!({"name": "work"}),
            "error: give new_name, description, prompts, skills or personality",
        ),
        (
            json!({"name": "work", "new_name": "bad name"}),
            "the name may only use letters",
        ),
        (
            json!({"name": "work", "new_name": "default"}),
            "there is already a profile named default",
        ),
        (
            json!({"name": "work", "description": "a\nb"}),
            "a description is one line of plain text",
        ),
        (
            json!({"name": "work", "prompts": ["nope"]}),
            "error: no system prompt named 'nope'",
        ),
        (
            json!({"name": "work", "skills": ["house"]}),
            "error: house is a system prompt, not a skill",
        ),
        (
            json!({"name": "work", "personality": "terse"}),
            "error: terse is a skill, not a personality",
        ),
        (
            json!({"name": "work", "prompts": "house"}),
            "error: 'prompts' must be a list of names",
        ),
    ];
    for (arguments, expected) in cases {
        let why = refused(&path, "profile_edit", arguments.clone());
        assert!(why.contains(expected), "{arguments}: {why}");
    }
}

#[test]
fn profile_delete_refuses_the_profile_new_chats_start_under_and_deletes_another() {
    let (_dir, path) = seeded("direct-profile-delete");
    let why = refused(&path, "profile_delete", json!({"name": "default"}));
    assert!(
        why.contains("is the profile new chats start under"),
        "{why}"
    );
    let why = refused(&path, "profile_delete", json!({"name": "DEFAULT"}));
    assert!(
        why.contains("is the profile new chats start under"),
        "{why}"
    );
    assert_eq!(
        done(&path, "profile_delete", json!({"name": "work"})),
        "deleted profile 'work'"
    );
    let (miss, failed) = read_call(&path, PROJECT, "profile_get", json!({"name": "work"}));
    assert!(failed, "{miss}");
    // Its members are still in the library.
    assert_eq!(
        read(&path, "library_list", json!({"type": "skill"})),
        "terse - short answers"
    );
    let why = refused(&path, "profile_delete", json!({"name": "work"}));
    assert!(why.starts_with("error: no profile named 'work'"), "{why}");
    assert_eq!(
        Store::open_read_only(&path)
            .unwrap()
            .selected_profile()
            .unwrap(),
        "default"
    );
}

#[test]
fn nothing_a_write_does_turns_anything_on_or_moves_it() {
    let (_dir, path) = seeded("direct-nothing-on");
    let mut store = Store::open(&path).unwrap();
    store.set_prompt_enabled("house", true).unwrap();
    drop(store);
    let before = Store::open_read_only(&path).unwrap();
    let old_prompts = before.list_prompts().unwrap();
    let old_selected = before.selected_profile().unwrap();
    drop(before);

    let battery = [
        (
            "library_add",
            json!({"type": "prompt", "name": "p1", "text": "One."}),
        ),
        (
            "library_add",
            json!({"type": "prompt", "name": "p2", "text": "Two."}),
        ),
        (
            "library_add",
            json!({"type": "skill", "name": "s1", "text": "Skill."}),
        ),
        (
            "library_add",
            json!({"type": "personality", "name": "q1", "text": "Persona."}),
        ),
        (
            "library_edit",
            json!({"type": "prompt", "name": "house", "text": "Edited."}),
        ),
        (
            "library_edit",
            json!({"type": "prompt", "name": "p2", "new_name": "p3"}),
        ),
        ("library_delete", json!({"type": "prompt", "name": "p3"})),
        (
            "profile_add",
            json!({"name": "mix", "prompts": ["p1"], "skills": ["s1"], "personality": "q1"}),
        ),
        (
            "profile_edit",
            json!({"name": "mix", "prompts": ["house", "p1"]}),
        ),
        ("profile_edit", json!({"name": "work", "skills": []})),
        ("profile_delete", json!({"name": "mix"})),
        (
            "context_set",
            json!({"name": "c", "kind": "shape", "text": "x", "quote": "the owner said x"}),
        ),
        ("glossary_set", json!({"term": "g", "definition": "x"})),
    ];
    for (tool, arguments) in battery {
        done(&path, tool, arguments);
    }

    let after = Store::open_read_only(&path).unwrap();
    assert_eq!(after.selected_profile().unwrap(), old_selected);
    let now = after.list_prompts().unwrap();
    // Every prompt that was there keeps its switch (a rename aside) and its place in the order.
    for old in &old_prompts {
        let still = now.iter().find(|p| p.id == old.id).unwrap();
        assert_eq!(still.enabled, old.enabled, "{}", old.name);
        assert_eq!(still.position, old.position, "{}", old.name);
    }
    let order_before: Vec<i64> = old_prompts.iter().map(|p| p.id).collect();
    let order_now: Vec<i64> = now
        .iter()
        .map(|p| p.id)
        .filter(|id| order_before.contains(id))
        .collect();
    assert_eq!(order_now, order_before);
    // Everything the battery added is off, and the new ones come after the old ones.
    for new in now.iter().filter(|p| !order_before.contains(&p.id)) {
        assert!(!new.enabled, "{} is on", new.name);
    }
    let profile = after.profile(&old_selected).unwrap();
    assert!(!profile.skills.contains(&"s1".to_owned()));
}

#[test]
fn the_same_writes_on_two_fresh_databases_give_the_same_answers() {
    let lines: Vec<Value> = [
        (
            "context_set",
            json!({"name": "a", "kind": "rule", "text": "x"}),
        ),
        (
            "context_set",
            json!({"name": "a", "kind": "rule", "text": "x"}),
        ),
        (
            "context_set",
            json!({"name": "a", "kind": "rule", "text": "y"}),
        ),
        ("context_delete", json!({"name": "a"})),
        ("context_delete", json!({"name": "a"})),
        ("glossary_set", json!({"term": "t", "definition": "d"})),
        ("glossary_delete", json!({"term": "t"})),
        (
            "library_add",
            json!({"type": "skill", "name": "s", "text": "x"}),
        ),
        (
            "library_edit",
            json!({"type": "skill", "name": "s", "new_name": "s2"}),
        ),
        ("library_delete", json!({"type": "skill", "name": "s2"})),
        ("library_delete", json!({"type": "skill", "name": "s2"})),
        ("profile_add", json!({"name": "pp", "skills": ["terse"]})),
        ("profile_add", json!({"name": "pp"})),
        ("profile_edit", json!({"name": "pp", "skills": []})),
        ("profile_delete", json!({"name": "pp"})),
        ("profile_delete", json!({"name": "default"})),
    ]
    .iter()
    .enumerate()
    .map(|(n, (tool, arguments))| {
        request(
            n as u64 + 1,
            "tools/call",
            json!({"name": tool, "arguments": arguments}),
        )
    })
    .collect();
    let (_one, first) = seeded("direct-same-1");
    let (_two, second) = seeded("direct-same-2");
    let a = dsession(&first, PROJECT, &lines);
    let b = dsession(&second, PROJECT, &lines);
    assert_eq!(a.len(), lines.len());
    assert_eq!(a, b);
}

#[test]
fn a_stale_prompt_edit_or_delete_is_reported_and_does_not_overwrite() {
    let (_dir, path) = seeded("direct-stale-prompt");
    let mut mine = Store::open_for_writing(&path).unwrap();
    let old = mine
        .list_prompts()
        .unwrap()
        .into_iter()
        .find(|p| p.name == "terse")
        .unwrap();
    // Someone else changes it after this reader looked.
    done(
        &path,
        "library_edit",
        json!({"type": "skill", "name": "terse", "text": "Be very brief."}),
    );
    let why = apply_prompt_edit(&mut mine, &old, "terse", "mine", "My text.").unwrap_err();
    assert_eq!(
        why,
        "error: the skill changed since it was read; read it again and retry"
    );
    let why = apply_prompt_delete(&mut mine, &old).unwrap_err();
    assert_eq!(
        why,
        "error: the skill changed since it was read; read it again and retry"
    );
    assert_eq!(
        read(
            &path,
            "library_get",
            json!({"type": "skill", "name": "terse"})
        ),
        "terse (skill)\nDescription: short answers\n\nBe very brief."
    );
    // Read again, it goes through.
    let fresh = mine
        .list_prompts()
        .unwrap()
        .into_iter()
        .find(|p| p.name == "terse")
        .unwrap();
    assert_eq!(
        apply_prompt_edit(&mut mine, &fresh, "terse", "mine", "My text.").unwrap(),
        "changed skill 'terse'"
    );
    // A row that is gone is reported as gone.
    done(
        &path,
        "library_delete",
        json!({"type": "skill", "name": "terse"}),
    );
    let fresh_edit = apply_prompt_edit(&mut mine, &fresh, "terse", "mine", "Again.").unwrap_err();
    assert_eq!(fresh_edit, "error: skill 'terse' is gone");
    let fresh_delete = apply_prompt_delete(&mut mine, &fresh).unwrap_err();
    assert_eq!(fresh_delete, "error: skill 'terse' is already gone");
}

#[test]
fn a_stale_profile_edit_or_delete_is_reported_and_does_not_overwrite() {
    let (_dir, path) = seeded("direct-stale-profile");
    let mut mine = Store::open_for_writing(&path).unwrap();
    let old = mine
        .list_profiles()
        .unwrap()
        .into_iter()
        .find(|p| p.name == "work")
        .unwrap();
    done(
        &path,
        "profile_edit",
        json!({"name": "work", "description": "changed by someone else"}),
    );
    let why = apply_profile_edit(
        &mut mine,
        &old,
        "work",
        "mine",
        &old.prompts,
        &old.skills,
        old.personality.as_deref(),
    )
    .unwrap_err();
    assert_eq!(
        why,
        "error: the profile changed since it was read; read it again and retry"
    );
    let why = apply_profile_delete(&mut mine, &old).unwrap_err();
    assert_eq!(
        why,
        "error: the profile changed since it was read; read it again and retry"
    );
    assert_eq!(
        read(&path, "profile_get", json!({"name": "work"})),
        "work\nDescription: changed by someone else\nSystem prompts: house\nSkills on: terse\nPersonality: calm"
    );
    // Read again, it goes through; then the profile is gone for the next stale holder.
    let fresh = mine
        .list_profiles()
        .unwrap()
        .into_iter()
        .find(|p| p.name == "work")
        .unwrap();
    assert_eq!(
        apply_profile_delete(&mut mine, &fresh).unwrap(),
        "deleted profile 'work'"
    );
    let why = apply_profile_delete(&mut mine, &fresh).unwrap_err();
    assert_eq!(why, "error: profile 'work' is already gone");
    let why = apply_profile_edit(&mut mine, &fresh, "work", "x", &[], &[], None).unwrap_err();
    assert_eq!(why, "error: profile 'work' is gone");
}

#[test]
fn a_database_that_cannot_be_opened_for_writing_stops_a_direct_server_at_the_start() {
    let dir = Scratch::new("direct-missing");
    let path = dir.path().join("none.db");
    let why = Server::new(&path, "")
        .and_then(Server::direct)
        .err()
        .unwrap();
    assert!(why.contains("none.db"), "{why}");
    assert!(!path.exists(), "starting must not create the database");
}

#[cfg(unix)]
#[test]
fn a_read_only_database_file_is_reported_with_its_path_when_direct_is_asked_for() {
    use std::os::unix::fs::PermissionsExt;

    let (_dir, path) = seeded("direct-readonly");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
    // A superuser can write anything; there is nothing to test then.
    if std::fs::OpenOptions::new().write(true).open(&path).is_ok() {
        return;
    }
    let server = Server::new(&path, "").expect("reading still works");
    let why = server.direct().err().expect("writing is refused");
    assert!(
        why.contains("harness.db") && why.contains("cannot write"),
        "{why}"
    );
}

#[test]
fn profile_edit_refuses_prompts_skills_and_personality_on_the_selected_profile() {
    let (_dir, path) = seeded("direct-selected-edit");
    // `default` is the profile new chats start under; give it members to protect.
    let mut store = Store::open(&path).unwrap();
    store
        .set_profile_skills("default", &["terse".to_owned()])
        .unwrap();
    store
        .set_profile_personality("default", Some("calm"))
        .unwrap();
    drop(store);
    let members = |path: &Path| {
        let store = Store::open_read_only(path).unwrap();
        let profile = store.profile("default").unwrap();
        let on: Vec<String> = store
            .list_prompts()
            .unwrap()
            .into_iter()
            .filter(|p| p.enabled)
            .map(|p| p.name)
            .collect();
        (profile.prompts, profile.skills, profile.personality, on)
    };
    let before = members(&path);
    let cases = [
        json!({"name": "default", "prompts": ["house"]}),
        json!({"name": "DEFAULT", "prompts": []}),
        json!({"name": "default", "skills": []}),
        json!({"name": "default", "personality": ""}),
        json!({"name": "default", "personality": "calm"}),
        // Even a value equal to the current one is refused: the answer never depends on the data.
        json!({"name": "default", "skills": ["terse"]}),
        // A mixed call is refused whole.
        json!({"name": "default", "description": "mixed", "skills": []}),
        json!({"name": "default", "new_name": "renamed", "prompts": ["house"]}),
    ];
    for arguments in cases {
        let why = refused(&path, "profile_edit", arguments.clone());
        assert!(
            why.starts_with("error: default is the profile new chats start under: its prompts, skills and personality decide what the next chat sends"),
            "{arguments}: {why}"
        );
        assert!(
            why.ends_with("(new_name and description are allowed)"),
            "{why}"
        );
        assert_eq!(members(&path), before, "{arguments}");
    }
    // The name and the description may change, and the profile stays the selected one.
    assert_eq!(
        done(
            &path,
            "profile_edit",
            json!({"name": "default", "description": "the start"})
        ),
        "changed profile 'default'"
    );
    assert_eq!(
        done(
            &path,
            "profile_edit",
            json!({"name": "default", "new_name": "start"})
        ),
        "changed profile 'default', now named 'start'"
    );
    assert_eq!(
        Store::open_read_only(&path)
            .unwrap()
            .selected_profile()
            .unwrap(),
        "start"
    );
    assert_eq!(
        members_of(&path, "start"),
        (before.0.clone(), before.1.clone(), before.2.clone())
    );
    // Now under its new name it is still protected.
    let why = refused(
        &path,
        "profile_edit",
        json!({"name": "start", "prompts": []}),
    );
    assert!(
        why.starts_with("error: start is the profile new chats start under"),
        "{why}"
    );
    // A profile that is not the selected one can change all three.
    done(
        &path,
        "profile_edit",
        json!({"name": "work", "prompts": [], "skills": [], "personality": ""}),
    );
    assert_eq!(
        read(&path, "profile_get", json!({"name": "work"})),
        "work\nDescription: the daily setup\nSystem prompts: none\nSkills on: none\nPersonality: none"
    );
}

fn members_of(path: &Path, name: &str) -> (Vec<String>, Vec<String>, Option<String>) {
    let profile = Store::open_read_only(path).unwrap().profile(name).unwrap();
    (profile.prompts, profile.skills, profile.personality)
}

#[test]
fn deleting_a_library_item_the_selected_profile_uses_is_allowed_on_purpose() {
    // It can only switch something off, never on, so it stays an allowed write.
    let (_dir, path) = seeded("direct-selected-delete");
    let mut store = Store::open(&path).unwrap();
    store.set_prompt_enabled("house", true).unwrap();
    store
        .set_profile_skills("default", &["terse".to_owned()])
        .unwrap();
    drop(store);
    assert!(members_of(&path, "default").0.contains(&"house".to_owned()));
    done(
        &path,
        "library_delete",
        json!({"type": "prompt", "name": "house"}),
    );
    done(
        &path,
        "library_delete",
        json!({"type": "skill", "name": "terse"}),
    );
    let (prompts, skills, _) = members_of(&path, "default");
    assert!(!prompts.contains(&"house".to_owned()));
    assert!(skills.is_empty());
}

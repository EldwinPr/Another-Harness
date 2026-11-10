//! Tests of the decider hook in `submit` (issue 48). Every test works on a temporary database made
//! by `seeded`; the deciders are a fake that records the cases it is given and a `LlmDecider` over
//! `ScriptedProvider`, so nothing here touches the network or the real `harness.db`.

use std::io::Cursor;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::tests::{PROJECT, request, seeded};
use super::*;
use crate::agent::ScriptedProvider;
use crate::decider::{Case, Decider, DeciderError, LlmDecider, Opinion, Verdict};
use crate::store_buffer::{self, Suggestion};

/// A decider that answers from a script and remembers every case it was asked about.
#[derive(Clone)]
struct Fake {
    name: &'static str,
    answers: Arc<Mutex<Vec<Result<Opinion, DeciderError>>>>,
    cases: Arc<Mutex<Vec<Case>>>,
}

impl Fake {
    fn new(name: &'static str, answers: Vec<Result<Opinion, DeciderError>>) -> Self {
        Self {
            name,
            answers: Arc::new(Mutex::new(answers)),
            cases: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn calls(&self) -> usize {
        self.cases.lock().unwrap().len()
    }
}

impl Decider for Fake {
    fn name(&self) -> String {
        self.name.to_owned()
    }

    fn judge(&self, case: &Case) -> Result<Opinion, DeciderError> {
        self.cases.lock().unwrap().push(case.clone());
        let mut answers = self.answers.lock().unwrap();
        if answers.is_empty() {
            Err(DeciderError("nothing left to say".to_owned()))
        } else {
            answers.remove(0)
        }
    }
}

fn opinion(verdict: Verdict, confidence: Option<f64>, reason: &str) -> Opinion {
    Opinion {
        verdict,
        confidence,
        reason: reason.to_owned(),
        raw: format!("raw: {reason}"),
    }
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

/// Calls `submit` on a buffer server that has `decider`; the answer text, whether it failed, and
/// everything the server wrote to its output (which must be protocol alone).
fn submit_with(path: &Path, decider: Box<dyn Decider>, arguments: Value) -> (String, bool) {
    let input = format!(
        "{}\n",
        request(
            1,
            "tools/call",
            json!({"name": "submit", "arguments": arguments})
        )
    );
    let mut output = Vec::new();
    Server::new(path, PROJECT)
        .unwrap()
        .buffer()
        .unwrap()
        .decider(decider)
        .serve(Cursor::new(input), &mut output)
        .unwrap();
    let text = String::from_utf8(output).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "stdout is one protocol line: {text}");
    let reply: Value = serde_json::from_str(lines[0]).unwrap();
    let result = &reply["result"];
    (
        result["content"][0]["text"].as_str().unwrap().to_owned(),
        result["isError"].as_bool().unwrap(),
    )
}

fn suggestions(path: &Path, id: i64) -> Vec<Suggestion> {
    Store::open_read_only(path)
        .unwrap()
        .list_suggestions(id)
        .unwrap()
}

fn count_suggestions(path: &Path) -> i64 {
    Store::open_read_only(path)
        .unwrap()
        .conn
        .query_row("SELECT COUNT(*) FROM buffer_suggestions", [], |r| r.get(0))
        .unwrap()
}

/// What `submission_get` tells a sender about entry `id`.
fn submission_get(path: &Path, id: i64) -> String {
    let input = format!(
        "{}\n",
        request(
            1,
            "tools/call",
            json!({"name": "submission_get", "arguments": {"id": id}})
        )
    );
    let mut output = Vec::new();
    Server::new(path, PROJECT)
        .unwrap()
        .buffer()
        .unwrap()
        .serve(Cursor::new(input), &mut output)
        .unwrap();
    let reply: Value = serde_json::from_str(String::from_utf8(output).unwrap().trim()).unwrap();
    reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn id_of(answer: &str) -> i64 {
    answer
        .strip_prefix("queued as #")
        .unwrap_or_else(|| panic!("{answer}"))
        .parse()
        .unwrap()
}

#[test]
fn a_new_submission_is_judged_after_it_is_stored_and_the_opinion_is_kept() {
    let (_dir, path) = seeded("decider-stores");
    let fake = Fake::new(
        "llm:p:m",
        vec![Ok(opinion(Verdict::Accept, Some(0.93), "Clear."))],
    );
    let (answer, failed) = submit_with(&path, Box::new(fake.clone()), good("context", "set", "a"));
    assert!(!failed, "{answer}");
    let id = id_of(&answer);
    assert_eq!(fake.calls(), 1);
    let list = suggestions(&path, id);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].decider, "llm:p:m");
    assert_eq!(list[0].verdict, Verdict::Accept);
    assert_eq!(list[0].confidence, Some(0.93));
    assert_eq!(list[0].reason, "Clear.");
    assert_eq!(list[0].raw, "raw: Clear.");
    assert_eq!(list[0].shown(), Verdict::Accept);
    // The row itself is as a submission without a decider leaves it: still pending.
    let row = Store::open_read_only(&path)
        .unwrap()
        .buffer_entry(id)
        .unwrap()
        .unwrap();
    assert_eq!(row.status, store_buffer::Status::Pending);
}

#[test]
fn the_case_carries_the_checked_submission_the_claims_and_what_is_stored_now() {
    let (_dir, path) = seeded("decider-case");
    let fake = Fake::new("llm:p:m", vec![Ok(opinion(Verdict::Unsure, None, "?"))]);
    let mut replacement = good("context", "set", "style");
    replacement["text"] = json!("App style: shorter lines.");
    replacement["why"] = json!("clearer");
    replacement["source"] = json!("some-agent");
    let (answer, _) = submit_with(&path, Box::new(fake.clone()), replacement);
    assert!(answer.starts_with("queued as #"), "{answer}");
    let cases = fake.cases.lock().unwrap();
    let case = &cases[0];
    assert_eq!(case.submission.name, "style");
    assert_eq!(case.submission.project, PROJECT);
    assert_eq!(case.submission.text, "App style: shorter lines.");
    assert_eq!(
        (case.why.as_str(), case.source.as_str()),
        ("clearer", "some-agent")
    );
    let stored = case
        .stored
        .as_ref()
        .expect("a replacement carries the old text");
    assert_eq!(stored.text, "App style: short lines.");
    drop(cases);
    // A name nothing is stored under has no old text.
    let fake = Fake::new("llm:p:m", vec![Ok(opinion(Verdict::Unsure, None, "?"))]);
    submit_with(
        &path,
        Box::new(fake.clone()),
        good("context", "set", "brand-new"),
    );
    assert!(fake.cases.lock().unwrap()[0].stored.is_none());
}

#[test]
fn a_failing_decider_stores_nothing_and_the_answer_is_still_queued() {
    let (_dir, path) = seeded("decider-fails");
    let fake = Fake::new(
        "llm:p:m",
        vec![Err(DeciderError(
            "the provider failed: timed out".to_owned(),
        ))],
    );
    let (answer, failed) = submit_with(&path, Box::new(fake.clone()), good("context", "set", "a"));
    assert!(!failed);
    let id = id_of(&answer);
    assert_eq!(fake.calls(), 1);
    assert!(suggestions(&path, id).is_empty());
    assert_eq!(count_suggestions(&path), 0);
    // The entry is there and can be decided as any other.
    let mut owner = Store::open(&path).unwrap();
    assert!(
        owner.list_pending().unwrap().iter().any(|e| e.id == id),
        "the submission is in the inbox"
    );
    store_buffer::reject(&mut owner, id, "").unwrap();
}

fn llm_over(
    replies: Vec<Result<crate::provider::Reply, Box<crate::provider::ProviderError>>>,
) -> Box<dyn Decider> {
    Box::new(LlmDecider::new(
        "p",
        Box::new(ScriptedProvider::new(replies)),
        "m",
    ))
}

#[test]
fn a_provider_error_a_bad_reply_and_a_slow_provider_each_store_nothing_and_still_queue() {
    let (_dir, path) = seeded("decider-llm-fails");
    let cases: [(&str, Vec<_>); 4] = [
        (
            "error",
            vec![ScriptedProvider::failure("request failed: boom")],
        ),
        (
            "timeout",
            vec![ScriptedProvider::failure("request failed: timeout: global")],
        ),
        (
            "not json",
            vec![ScriptedProvider::text("Looks good to me!")],
        ),
        (
            "bad value",
            vec![ScriptedProvider::text(
                r#"{"verdict":"accept","confidence":2,"reason":"x"}"#,
            )],
        ),
    ];
    for (n, (label, replies)) in cases.into_iter().enumerate() {
        let (answer, failed) = submit_with(
            &path,
            llm_over(replies),
            good("context", "set", &format!("entry-{n}")),
        );
        assert!(!failed, "{label}: {answer}");
        assert!(answer.starts_with("queued as #"), "{label}: {answer}");
        assert!(suggestions(&path, id_of(&answer)).is_empty(), "{label}");
    }
    assert_eq!(count_suggestions(&path), 0);
}

#[test]
fn a_model_that_answers_well_gives_a_stored_suggestion_with_the_reply_as_received() {
    let (_dir, path) = seeded("decider-llm-ok");
    let reply = "```json\n{\"verdict\":\"reject\",\"confidence\":0.7,\"reason\":\"Vague.\"}\n```";
    let (answer, _) = submit_with(
        &path,
        llm_over(vec![ScriptedProvider::text(reply)]),
        good("context", "set", "vague-one"),
    );
    let list = suggestions(&path, id_of(&answer));
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].decider, "llm:p:m");
    assert_eq!(list[0].raw, reply);
    assert_eq!(list[0].shown(), Verdict::Reject);
}

#[test]
fn a_resubmission_answered_already_queued_does_not_call_the_model_again() {
    let (_dir, path) = seeded("decider-again");
    let fake = Fake::new(
        "llm:p:m",
        vec![Ok(opinion(Verdict::Reject, Some(0.9), "No."))],
    );
    let (first, _) = submit_with(&path, Box::new(fake.clone()), good("context", "set", "a"));
    let id = id_of(&first);
    // Even from another sender with another reason: it is the same entry.
    let mut again = good("context", "set", "a");
    again["source"] = json!("someone-else");
    let (second, failed) = submit_with(&path, Box::new(fake.clone()), again);
    assert!(!failed);
    assert_eq!(second, format!("already queued as #{id}"));
    assert_eq!(fake.calls(), 1, "the model was asked once");
    assert_eq!(suggestions(&path, id).len(), 1);
}

#[test]
fn a_refused_submission_never_reaches_the_model() {
    let (_dir, path) = seeded("decider-refused");
    let fake = Fake::new("llm:p:m", vec![]);
    let mut bad = good("context", "set", "a");
    bad["why"] = json!("");
    let (answer, failed) = submit_with(&path, Box::new(fake.clone()), bad);
    assert!(failed, "{answer}");
    assert!(answer.starts_with("error:"));
    assert_eq!(fake.calls(), 0);
}

#[test]
fn a_full_inbox_does_not_call_the_model() {
    let (_dir, path) = seeded("decider-full");
    {
        let mut store = Store::open(&path).unwrap();
        for n in 0..store_buffer::MAX_PENDING {
            let raw = store_buffer::Submission {
                kind: "context".to_owned(),
                action: "set".to_owned(),
                name: format!("e{n}"),
                project: PROJECT.to_owned(),
                context_kind: "rule".to_owned(),
                text: format!("text {n}"),
                ..store_buffer::Submission::default()
            };
            let checked = store_buffer::Checked::new(&raw, "why", "src").unwrap();
            store.queue_submission(&checked).unwrap();
        }
    }
    let fake = Fake::new("llm:p:m", vec![]);
    let (answer, failed) = submit_with(&path, Box::new(fake.clone()), good("context", "set", "x"));
    assert!(failed && answer.contains("inbox is full"), "{answer}");
    assert_eq!(fake.calls(), 0);
}

#[test]
fn submission_get_never_shows_the_opinion_to_the_sender() {
    let (_dir, path) = seeded("decider-private");
    let fake = Fake::new(
        "llm:p:m",
        vec![Ok(opinion(Verdict::Reject, Some(0.99), "SECRETREASON"))],
    );
    let (answer, _) = submit_with(&path, Box::new(fake), good("context", "set", "a"));
    let id = id_of(&answer);
    let get = |path: &Path| submission_get(path, id);
    assert_eq!(get(&path), format!("#{id}: pending"));
    // Not after the owner's decision either: only the owner's own note is the sender's to read.
    let mut owner = Store::open(&path).unwrap();
    store_buffer::reject(&mut owner, id, "owner's words").unwrap();
    drop(owner);
    assert_eq!(get(&path), format!("#{id}: rejected: owner's words"));
    assert!(!answer.contains("SECRETREASON"));
}

#[test]
fn a_judged_submission_changes_no_curated_table_and_only_adds_inbox_and_suggestion_rows() {
    let (_dir, path) = seeded("decider-untouched");
    let curated = |path: &Path| {
        let store = Store::open_read_only(path).unwrap();
        format!(
            "{:?}\n{:?}\n{:?}\n{:?}\n{:?}",
            store.list_all_context().unwrap(),
            store.list_all_glossary().unwrap(),
            store.list_prompts().unwrap(),
            store.list_profiles().unwrap(),
            store.selected_profile().unwrap(),
        )
    };
    let counts = |path: &Path| -> Vec<(String, i64)> {
        let store = Store::open_read_only(path).unwrap();
        let mut statement = store
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT IN ('buffer_entries', 'buffer_suggestions') ORDER BY name")
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
    };
    let (before, tables) = (curated(&path), counts(&path));
    for (n, verdict) in [Verdict::Accept, Verdict::Reject, Verdict::Unsure]
        .into_iter()
        .enumerate()
    {
        let fake = Fake::new("llm:p:m", vec![Ok(opinion(verdict, Some(1.0), "r"))]);
        let (answer, _) = submit_with(
            &path,
            Box::new(fake),
            good("context", "set", &format!("n{n}")),
        );
        assert!(answer.starts_with("queued as #"));
    }
    let fake = Fake::new(
        "llm:p:m",
        vec![Ok(opinion(Verdict::Accept, Some(1.0), "r"))],
    );
    submit_with(&path, Box::new(fake), good("prompt", "delete", "house"));
    assert_eq!(count_suggestions(&path), 4);
    assert_eq!(curated(&path), before, "a suggestion applied something");
    assert_eq!(counts(&path), tables);
}

#[test]
fn accepting_and_rejecting_work_the_same_with_or_without_a_suggestion() {
    let (_dir, path) = seeded("decider-owner");
    let fake = Fake::new(
        "llm:p:m",
        vec![Ok(opinion(Verdict::Reject, Some(1.0), "No."))],
    );
    let (judged, _) = submit_with(&path, Box::new(fake), good("context", "set", "judged"));
    let plain = Fake::new("llm:p:m", vec![]);
    let (unjudged, _) = submit_with(&path, Box::new(plain), good("context", "set", "unjudged"));
    let (judged, unjudged) = (id_of(&judged), id_of(&unjudged));
    assert_eq!(suggestions(&path, judged).len(), 1);
    assert!(suggestions(&path, unjudged).is_empty());
    let mut owner = Store::open(&path).unwrap();
    // The model said reject with certainty; the owner may still accept.
    for id in [judged, unjudged] {
        assert!(matches!(
            store_buffer::apply_entry(&mut owner, id, None).unwrap(),
            store_buffer::ApplyOutcome::Applied(_)
        ));
    }
    assert!(owner.context_entry(PROJECT, "judged").unwrap().is_some());
    assert!(owner.context_entry(PROJECT, "unjudged").unwrap().is_some());
}

#[test]
fn a_decider_only_goes_with_the_buffer_mode() {
    let (_dir, path) = seeded("decider-mode");
    for mode in [Mode::ReadOnly, Mode::Direct] {
        let why = run(
            &path,
            PROJECT,
            mode,
            Some(Box::new(Fake::new("llm:p:m", vec![]))),
        )
        .unwrap_err();
        assert!(why.contains("buffer mode"), "{why}");
    }
}

#[test]
fn a_read_only_server_with_a_decider_still_has_no_submit_and_never_calls_it() {
    let (_dir, path) = seeded("decider-readonly");
    let fake = Fake::new("llm:p:m", vec![]);
    let input = format!(
        "{}\n",
        request(
            1,
            "tools/call",
            json!({"name": "submit", "arguments": good("context", "set", "a")})
        )
    );
    let mut output = Vec::new();
    Server::new(&path, PROJECT)
        .unwrap()
        .decider(Box::new(fake.clone()))
        .serve(Cursor::new(input), &mut output)
        .unwrap();
    let reply: Value = serde_json::from_str(String::from_utf8(output).unwrap().trim()).unwrap();
    assert!(reply["error"].is_object(), "{reply}");
    assert_eq!(fake.calls(), 0);
}

// ----- Jev behind the same hook (issue 49) -----

type JevCalls = Arc<Mutex<Vec<String>>>;
/// What a scripted transport answers: a status and a body, or why it could not.
type JevReply = Result<(u16, String), String>;

/// A Jev decider whose transport answers from `replies` in order (a status and a body, or a
/// transport failure) and records every request body. No network, no real key.
fn jev_with(replies: Vec<JevReply>) -> (Box<dyn Decider>, JevCalls) {
    let calls: JevCalls = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&calls);
    let replies = Mutex::new(replies);
    let transport: crate::jev::Transport = Box::new(move |call| {
        seen.lock().unwrap().push(call.body.to_owned());
        let mut replies = replies.lock().unwrap();
        if replies.is_empty() {
            Err("nothing left to say".to_owned())
        } else {
            replies.remove(0)
        }
    });
    (
        Box::new(crate::jev::JevDecider::with_transport(
            "https://openrouter.ai/api/alpha/decisions",
            "NOT-A-REAL-KEY",
            crate::jev::MODEL,
            transport,
        )),
        calls,
    )
}

fn jev_reply(worth: f64, instructs: f64) -> JevReply {
    Ok((
        200,
        format!(
            r#"{{"model":"typesafe/jev-1.14-20261014","answers":{{"worth_keeping":{{"type":"noul","noul":{worth}}},"targets_the_reviewer":{{"type":"noul","noul":{instructs}}}}},"usage":{{"input_tokens":412,"output_tokens":6,"cost":0.0000173}},"id":"gen-dec-abc123","provider":"TypeSafe"}}"#
        ),
    ))
}

#[test]
fn jev_stores_one_row_with_its_name_the_generated_reason_and_the_body_as_received() {
    let (_dir, path) = seeded("decider-jev-stores");
    let (decider, calls) = jev_with(vec![jev_reply(0.91, 0.03)]);
    let (answer, failed) = submit_with(&path, decider, good("context", "set", "a"));
    assert!(!failed, "{answer}");
    let id = id_of(&answer);
    assert_eq!(calls.lock().unwrap().len(), 1);
    let list = suggestions(&path, id);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].decider, "jev:typesafe/jev-1.14-20261014");
    assert_eq!(list[0].verdict, Verdict::Accept);
    assert_eq!(list[0].confidence, Some(0.91));
    assert_eq!(
        list[0].reason,
        "jev: worth keeping 0.91, targets the reviewer 0.03"
    );
    assert!(list[0].raw.contains("gen-dec-abc123") && list[0].raw.contains("TypeSafe"));
    // The shown verdict uses Jev's thresholds: 0.91 clears 0.7.
    assert_eq!(list[0].shown(), Verdict::Accept);
    let row = Store::open_read_only(&path)
        .unwrap()
        .buffer_entry(id)
        .unwrap()
        .unwrap();
    assert_eq!(row.status, store_buffer::Status::Pending);
}

#[test]
fn jev_carries_the_claims_labelled_unverified_and_the_stored_text_for_a_replacement() {
    let (_dir, path) = seeded("decider-jev-request");
    {
        let mut owner = Store::open(&path).unwrap();
        owner
            .set_context(PROJECT, "a", "rule", "OLD STORED TEXT", "", None)
            .unwrap();
    }
    let (decider, calls) = jev_with(vec![jev_reply(0.2, 0.0)]);
    let (answer, _) = submit_with(&path, decider, good("context", "set", "a"));
    assert!(answer.starts_with("queued as #"), "{answer}");
    let body: Value = serde_json::from_str(&calls.lock().unwrap()[0]).unwrap();
    let state = body["state"].as_array().unwrap();
    let proposal = state[0].as_str().unwrap();
    assert!(proposal.contains("why (the sender's reason, unverified): the owner asked for it"));
    assert!(proposal.contains("source (the sender's name for itself, unverified): claude-code"));
    assert!(state[1].as_str().unwrap().contains("OLD STORED TEXT"));
}

#[test]
fn jev_failures_store_nothing_and_the_answer_is_still_queued() {
    let (_dir, path) = seeded("decider-jev-fails");
    let cases: Vec<(&str, JevReply)> = vec![
        (
            "transport",
            Err("request failed: timeout: global".to_owned()),
        ),
        ("401", Ok((401, r#"{"error":"no credits"}"#.to_owned()))),
        ("500", Ok((500, String::new()))),
        ("not json", Ok((200, "<html>".to_owned()))),
        ("wrong shape", Ok((200, r#"{"answers":{}}"#.to_owned()))),
        (
            "missing question",
            Ok((
                200,
                r#"{"answers":{"worth_keeping":{"type":"noul","noul":0.9}}}"#.to_owned(),
            )),
        ),
        ("out of range", jev_reply(1.5, 0.0)),
    ];
    for (n, (label, reply)) in cases.into_iter().enumerate() {
        let (decider, calls) = jev_with(vec![reply]);
        let (answer, failed) = submit_with(
            &path,
            decider,
            good("context", "set", &format!("entry-{n}")),
        );
        assert!(!failed, "{label}: {answer}");
        assert!(answer.starts_with("queued as #"), "{label}: {answer}");
        assert!(suggestions(&path, id_of(&answer)).is_empty(), "{label}");
        assert_eq!(calls.lock().unwrap().len(), 1, "{label}");
    }
    assert_eq!(count_suggestions(&path), 0);
}

#[test]
fn a_resubmission_does_not_call_jev_again() {
    let (_dir, path) = seeded("decider-jev-again");
    let (decider, calls) = jev_with(vec![jev_reply(0.1, 0.0), jev_reply(0.1, 0.0)]);
    let (first, _) = submit_with(&path, decider, good("context", "set", "a"));
    let id = id_of(&first);
    let (decider, again_calls) = jev_with(vec![jev_reply(0.9, 0.0)]);
    let (second, failed) = submit_with(&path, decider, good("context", "set", "a"));
    assert!(!failed);
    assert_eq!(second, format!("already queued as #{id}"));
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert_eq!(
        again_calls.lock().unwrap().len(),
        0,
        "Jev was not asked again"
    );
    assert_eq!(suggestions(&path, id).len(), 1);
}

#[test]
fn submission_get_shows_nothing_of_jev_and_the_screens_show_it_with_jevs_thresholds() {
    let (_dir, path) = seeded("decider-jev-screens");
    let (decider, _) = jev_with(vec![jev_reply(0.69, 0.0), jev_reply(0.02, 0.0)]);
    let (first, _) = submit_with(&path, decider, good("context", "set", "borderline"));
    let (decider, _) = jev_with(vec![jev_reply(0.02, 0.0)]);
    let (second, _) = submit_with(&path, decider, good("context", "set", "clear-reject"));
    let (first, second) = (id_of(&first), id_of(&second));
    for id in [first, second] {
        assert_eq!(submission_get(&path, id), format!("#{id}: pending"));
    }

    let store = Store::open_read_only(&path).unwrap();
    let all = store.suggestions_for([first, second]).unwrap();
    // 0.69 is below the accept threshold of 0.7: Unsure, with what Jev said still shown.
    assert_eq!(
        crate::inbox_view::suggestion_short(&all[&first][0]),
        "Unsure (said accept, confidence 0.69)"
    );
    // A reject of 1 - 0.02 = 0.98 clears 0.7.
    assert_eq!(
        crate::inbox_view::suggestion_short(&all[&second][0]),
        "Reject (said reject, confidence 0.98)"
    );
    let pending = store.list_pending().unwrap();
    let page = crate::web_inbox::list(&pending, &[], &all, 0, &crate::web_pages::View::new("t"));
    assert!(
        page.contains("Unsure (said accept, confidence 0.69)"),
        "{page}"
    );
    assert!(
        page.contains("Reject (said reject, confidence 0.98)"),
        "{page}"
    );
    let shown = crate::web_inbox::show_page(&store, second, &crate::web_pages::View::new("t"))
        .unwrap()
        .unwrap();
    assert!(shown.contains("jev:typesafe/jev-1.14-20261014"), "{shown}");
    assert!(
        shown.contains("jev: worth keeping 0.02, targets the reviewer 0.00"),
        "{shown}"
    );
}

//! Tests of `delegate` and of what a subagent is (issues 33, 34, 35): a provider that answers the
//! main chat and the subagents from separate scripts, a temporary database, no network. `delegate`
//! starts the subagent in the background, so a test lets it go and waits for it.

use super::*;
use crate::agent::background_tests::{
    Handles, Replies, Split, background_agent, count, json, wait_for,
};
use crate::jobs::Status;
use crate::tools::Mode;
use crate::tools::testutil::Scratch;

const TASK: &str = r#"{"task":"find where x is decided","profile":"planner"}"#;

fn setup(dir: &Scratch, main: Replies, child: Replies) -> (Agent, Handles) {
    let (split, handles) = Split::new(main, child);
    (background_agent(dir, split), handles)
}

fn tool_names(request: &serde_json::Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|t| t["function"]["name"].as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn last_content(request: &serde_json::Value) -> String {
    request["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Starts a run, lets its requests go and waits until it is done; its text.
fn run_to_the_end(agent: &mut Agent, h: &Handles, text: &str, requests: usize) -> String {
    let run = agent.start_subagent(text).unwrap();
    h.release_n(requests);
    for _ in 0..500 {
        let runs = agent.subagent_runs();
        if runs
            .iter()
            .any(|r| r.n == run.n && r.status != Status::Running)
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let runs = agent.subagent_runs();
    runs.iter()
        .find(|r| r.n == run.n)
        .unwrap()
        .result
        .clone()
        .unwrap()
}

#[test]
fn a_delegation_returns_at_once_and_only_the_answer_comes_back_at_the_next_message() {
    let dir = Scratch::new("delegate-trip");
    let (mut agent, h) = setup(
        &dir,
        vec![
            ScriptedProvider::tool_call("d1", "delegate", TASK),
            ScriptedProvider::text("Started it."),
            ScriptedProvider::text("Done: see a.txt:1."),
        ],
        vec![ScriptedProvider::text("It is decided in a.txt:1.")],
    );
    assert_eq!(
        agent.send("where is x decided?", &mut NoObserver),
        Outcome::Answer("Started it.".into())
    );
    // The model is offered `delegate` and gets a short note back, not an answer.
    let first = json(&h.main_seen.borrow()[0]);
    assert_eq!(tool_names(&first), ["read_file", "delegate"]);
    let note = last_content(&json(&h.main_seen.borrow()[1]));
    assert!(
        note.starts_with("subagent #1 (planner, reader) is running in the background."),
        "{note}"
    );

    // The subagent is offered only what looks; its only user message is the task, unframed.
    h.release();
    wait_for(&agent, 1, Status::Done);
    let child = json(&h.child_seen.borrow()[0]);
    let system = child["messages"][0]["content"].as_str().unwrap();
    assert!(system.starts_with("You are a subagent."), "{system}");
    assert!(
        system.contains("Your job is to plan, not to build."),
        "{system}"
    );
    assert!(!system.contains("glossary_propose"), "{system}");
    assert!(!system.contains("You are a coding assistant"), "{system}");
    let messages = child["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1]["content"], "find where x is decided");

    // The next message brings the answer and the footer, nothing of the subagent's chat.
    agent.send("and?", &mut NoObserver);
    let back = last_content(&json(&h.main_seen.borrow()[2]));
    assert_eq!(back, "and?");
    let delivered = json(&h.main_seen.borrow()[2])["messages"].to_string();
    assert!(
        delivered.contains("It is decided in a.txt:1.\\n[subagent planner (reader): 1 call, "),
        "{delivered}"
    );
}

#[test]
fn an_unknown_profile_is_answered_with_the_profiles_there_are() {
    let dir = Scratch::new("delegate-unknown");
    let (mut agent, h) = setup(
        &dir,
        vec![
            ScriptedProvider::tool_call("d1", "delegate", r#"{"task":"x","profile":"nope"}"#),
            ScriptedProvider::text("ok"),
        ],
        vec![],
    );
    agent.send("go", &mut NoObserver);
    assert_eq!(h.main_seen.borrow().len(), 2);
    assert!(h.child_seen.borrow().is_empty(), "no subagent was started");
    assert_eq!(
        last_content(&json(&h.main_seen.borrow()[1])),
        "error: no profile named nope; the profiles are: builder, default, planner"
    );
}

#[test]
fn a_subagent_cannot_change_anything_in_manual_mode() {
    let dir = Scratch::new("delegate-readonly");
    let (mut agent, h) = setup(
        &dir,
        vec![],
        vec![
            ScriptedProvider::tool_calls(&[
                ("c1", "bash", r#"{"command":"touch made.txt"}"#),
                ("c2", "create_file", r#"{"path":"made2.txt","content":"x"}"#),
                ("c3", "delegate", TASK),
            ]),
            ScriptedProvider::text("I could not change anything."),
        ],
    );
    run_to_the_end(&mut agent, &h, "planner try", 2);
    assert!(!dir.path().join("made.txt").exists());
    assert!(!dir.path().join("made2.txt").exists());
    let after = json(&h.child_seen.borrow()[1])["messages"].clone();
    let after = after.as_array().unwrap();
    let results: Vec<&str> = after[after.len() - 3..]
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert!(
        results[0].starts_with("error: a subagent can only look"),
        "{results:?}"
    );
    assert_eq!(results[1], "error: unknown tool 'create_file'");
    assert!(
        results[2].starts_with("error: unknown tool 'delegate'"),
        "{results:?}"
    );
}

#[test]
fn a_subagent_that_uses_all_its_calls_is_reported_as_stopped() {
    let dir = Scratch::new("delegate-cap");
    let mut replies = Vec::new();
    for n in 0..crate::subagent::MAX_CALLS {
        std::fs::write(dir.path().join(format!("a{n}.txt")), "x").unwrap();
        replies.push(ScriptedProvider::tool_call(
            &format!("c{n}"),
            "read_file",
            &format!(r#"{{"path":"a{n}.txt"}}"#),
        ));
    }
    let (mut agent, h) = setup(&dir, vec![], replies);
    let back = run_to_the_end(&mut agent, &h, "planner read", 16);
    assert!(
        back.starts_with("error: subagent stopped: it used all 15 of its calls"),
        "{back}"
    );
    assert!(
        back.contains("[subagent planner (reader): 15 calls, "),
        "{back}"
    );
}

#[test]
fn a_long_answer_is_cut() {
    let dir = Scratch::new("delegate-long");
    let long = "y".repeat(7000);
    let (mut agent, h) = setup(&dir, vec![], vec![ScriptedProvider::text(&long)]);
    let back = run_to_the_end(&mut agent, &h, "planner talk", 1);
    assert!(back.contains("[answer cut: 6000 of 7000 characters shown]"));
    assert!(back.chars().count() < 6300);
}

#[test]
fn the_fourth_delegation_is_refused_while_three_run_and_the_count_starts_again() {
    let dir = Scratch::new("delegate-limits");
    let five = [
        ("d1", "delegate", TASK),
        ("d2", "delegate", TASK),
        ("d3", "delegate", TASK),
        ("d4", "delegate", TASK),
        ("d5", "delegate", TASK),
    ];
    let (mut agent, h) = setup(
        &dir,
        vec![
            ScriptedProvider::tool_calls(&five),
            ScriptedProvider::text("ok"),
            ScriptedProvider::text("again"),
        ],
        vec![],
    );
    agent.send("go", &mut NoObserver);
    let last = json(&h.main_seen.borrow()[1]);
    let messages = last["messages"].as_array().unwrap();
    let results: Vec<&str> = messages[messages.len() - 5..]
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    for started in &results[..3] {
        assert!(
            started.contains("is running in the background"),
            "{started}"
        );
    }
    for refused in &results[3..] {
        assert!(
            refused.starts_with("error: 3 subagents are already running"),
            "{refused}"
        );
    }
    assert_eq!(agent.delegations, 3);
    agent.send("again", &mut NoObserver);
    assert_eq!(agent.delegations, 0);
    h.release_n(3);
}

#[test]
fn the_subagent_chat_is_saved_under_its_parent_and_hidden_from_the_lists() {
    let dir = Scratch::new("delegate-saved");
    let (mut agent, h) = setup(
        &dir,
        vec![ScriptedProvider::with_usage(
            ScriptedProvider::text("done"),
            r#"{"prompt_tokens":100,"completion_tokens":5}"#,
        )],
        vec![ScriptedProvider::with_usage(
            ScriptedProvider::text("answer"),
            r#"{"prompt_tokens":700,"completion_tokens":30}"#,
        )],
    );
    run_to_the_end(&mut agent, &h, "planner look", 1);
    let parent = agent.current_session().unwrap();
    agent.send("go", &mut NoObserver);

    // Two chats are stored; the subagent's points at the parent and names the profile.
    assert_eq!(count(&dir, "SELECT COUNT(*) FROM sessions"), 2);
    assert_eq!(
        count(
            &dir,
            &format!(
                "SELECT COUNT(*) FROM sessions WHERE parent_id = {parent} AND profile = 'planner'"
            )
        ),
        1
    );
    // The subagent's tokens count in the chat's totals once its answer is in the chat.
    assert_eq!(agent.usage().prompt_total, 800);
    assert_eq!(agent.usage().completion_total, 35);
    assert_eq!(agent.usage().last.unwrap().prompt, Some(100));

    // Only the parent is listed, and it is the one to resume, with the same totals.
    let listed = agent.sessions(10).unwrap();
    assert_eq!(listed.iter().map(|s| s.id).collect::<Vec<_>>(), [parent]);
    let report = agent.resume(None).unwrap();
    assert_eq!(report.id, parent);
    assert_eq!(agent.usage().prompt_total, 800);
    assert_eq!(agent.usage().completion_total, 35);

    // The subagent has a turn log of its own.
    let logs = std::fs::read_dir(dir.path().join("logs")).unwrap().count();
    assert_eq!(logs, 2);

    // Deleting the parent deletes the subagent's chat with it.
    agent.new_chat().unwrap();
    agent.delete_session(parent).unwrap();
    for table in ["sessions", "messages", "calls"] {
        assert_eq!(
            count(&dir, &format!("SELECT COUNT(*) FROM {table}")),
            0,
            "{table}"
        );
    }
}

#[test]
fn a_chat_without_a_database_cannot_delegate_and_a_chat_without_subagents_has_no_tool() {
    let dir = Scratch::new("delegate-none");
    let (split, h) = Split::new(
        vec![
            ScriptedProvider::tool_call("d1", "delegate", TASK),
            ScriptedProvider::text("ok"),
        ],
        vec![],
    );
    let log = TurnLog::create(&dir.path().join("logs")).unwrap();
    let mut agent =
        Agent::new("m", Box::new(split), vec![], log).with_target("test", "http://x/v1");
    agent.send("go", &mut NoObserver);
    // Not offered, so a call to it is an unknown tool.
    assert!(tool_names(&json(&h.main_seen.borrow()[0])).is_empty());
    assert_eq!(
        last_content(&json(&h.main_seen.borrow()[1])),
        "error: unknown tool 'delegate'"
    );
}

#[test]
fn in_auto_mode_a_subagent_is_a_coder_and_changes_files_in_manual_mode_it_is_a_reader() {
    let dir = Scratch::new("delegate-coder");
    let (mut agent, h) = setup(
        &dir,
        vec![],
        vec![
            // Manual: the builder is a reader.
            ScriptedProvider::text("I can only describe it."),
            // Auto: the builder is a coder.
            ScriptedProvider::tool_call(
                "c1",
                "create_file",
                r#"{"path":"made.txt","content":"built"}"#,
            ),
            ScriptedProvider::text("Created made.txt."),
        ],
    );
    let back = run_to_the_end(&mut agent, &h, "builder implement the plan", 1);
    assert!(back.contains("(reader)"), "{back}");
    let reader = json(&h.child_seen.borrow()[0]);
    let system = reader["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("you can only look"), "{system}");
    assert!(!dir.path().join("made.txt").exists());

    agent.set_approval_mode(Mode::Auto).unwrap();
    let back = run_to_the_end(&mut agent, &h, "builder implement the plan", 2);
    let coder = json(&h.child_seen.borrow()[1]);
    let system = coder["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("you may change files"), "{system}");
    assert!(system.contains("Your job is to build"), "{system}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("made.txt")).unwrap(),
        "built"
    );
    assert!(
        back.starts_with("Created made.txt.\n[subagent builder (coder): 2 calls"),
        "{back}"
    );
}

#[test]
fn a_plan_saved_as_a_note_can_be_read_by_the_next_subagent() {
    let dir = Scratch::new("delegate-notes");
    let (mut agent, h) = setup(
        &dir,
        vec![],
        vec![
            ScriptedProvider::tool_call(
                "c1",
                "write_note",
                r#"{"name":"plan.md","content":"1. make db.py\n2. add the form"}"#,
            ),
            ScriptedProvider::text("Plan in .Aharness/tmp/plan.md: two steps."),
            ScriptedProvider::tool_call("c2", "read_file", r#"{"path":".Aharness/tmp/plan.md"}"#),
            ScriptedProvider::text("Read it: step 1 is db.py."),
        ],
    );
    run_to_the_end(&mut agent, &h, "planner plan it", 2);
    run_to_the_end(
        &mut agent,
        &h,
        "builder read .Aharness/tmp/plan.md and do step 1",
        2,
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join(".Aharness/tmp/plan.md")).unwrap(),
        "1. make db.py\n2. add the form"
    );
    let seen = last_content(&json(&h.child_seen.borrow()[3]));
    assert!(seen.contains("1. make db.py"), "{seen}");
}

#[test]
fn shapes_are_preloaded_into_a_subagent_and_it_can_read_but_not_propose() {
    let dir = Scratch::new("delegate-context");
    let (mut agent, h) = setup(&dir, vec![], vec![ScriptedProvider::text("ok")]);
    let project = dir.path().display().to_string();
    {
        let mut store = Store::open(&dir.path().join("harness.db")).unwrap();
        store
            .set_context(
                &project,
                "cafe-lean",
                "shape",
                "A cafe site is small and static.",
                "keep it static",
                Some("site"),
            )
            .unwrap();
        store
            .set_context(
                &project,
                "no-input-edits",
                "rule",
                "Never edit input/.",
                "never edit input",
                None,
            )
            .unwrap();
        store
            .set_context(
                "/elsewhere",
                "other",
                "shape",
                "Not for this project.",
                "x",
                None,
            )
            .unwrap();
    }
    run_to_the_end(&mut agent, &h, "planner look", 1);
    let child = json(&h.child_seen.borrow()[0]);
    let system = child["messages"][0]["content"].as_str().unwrap();
    assert!(
        system.contains("Background from the project (the owner's words):"),
        "{system}"
    );
    assert!(
        system.contains("## cafe-lean (shape)\nA cafe site is small and static."),
        "{system}"
    );
    assert!(
        !system.contains("Never edit input/."),
        "a rule is not preloaded: {system}"
    );
    assert!(!system.contains("Not for this project."), "{system}");
    let tools = tool_names(&child);
    assert!(tools.contains(&"context".to_owned()), "{tools:?}");
    assert!(!tools.contains(&"context_propose".to_owned()), "{tools:?}");
}

#[test]
fn the_model_can_list_read_and_stop_a_subagent_with_task() {
    let dir = Scratch::new("delegate-task");
    let (mut agent, h) = setup(
        &dir,
        vec![
            ScriptedProvider::tool_calls(&[
                ("d1", "delegate", TASK),
                ("t1", "task", r#"{"action":"list"}"#),
                ("t2", "task", r#"{"action":"output","id":"a1"}"#),
                ("t3", "task", r#"{"action":"stop","id":"a1"}"#),
                ("t4", "task", r#"{"action":"stop","id":"a1"}"#),
                ("t5", "task", r#"{"action":"stop","id":"a9"}"#),
                ("t6", "task", r#"{"action":"stop","id":"zzz"}"#),
            ]),
            ScriptedProvider::text("Stopped it."),
        ],
        vec![ScriptedProvider::text("never delivered")],
    );
    let task_tool = crate::tools::TaskTool::new(agent.shells().clone());
    agent.tools.push(Box::new(task_tool));
    assert_eq!(
        agent.send("start one and stop it", &mut NoObserver),
        Outcome::Answer("Stopped it.".into())
    );
    let seen = json(&h.main_seen.borrow()[1]);
    let messages = seen["messages"].as_array().unwrap();
    let results: Vec<&str> = messages[messages.len() - 7..]
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert!(
        results[0].contains("task with action stop and id a1"),
        "{}",
        results[0]
    );
    assert!(results[1].starts_with("a1  running"), "{}", results[1]);
    assert!(
        results[1].contains("planner: find where x is decided"),
        "{}",
        results[1]
    );
    assert!(results[2].contains("(no answer yet)"), "{}", results[2]);
    assert_eq!(
        results[3],
        "stopping a1; its answer will not be added to the chat"
    );
    // A second stop before the thread has noticed the first is harmless.
    assert!(results[4].starts_with("stopping a1"), "{}", results[4]);
    assert!(
        results[5].starts_with("error: ") && results[5].contains("no subagent #9"),
        "{}",
        results[5]
    );
    assert!(results[6].contains("is not a task id"), "{}", results[6]);

    // The stopped run's answer never reaches the chat.
    h.release();
    wait_for(&agent, 1, Status::Stopped);
    agent.send("and?", &mut NoObserver);
    let delivered = json(&h.main_seen.borrow()[2])["messages"].to_string();
    assert!(!delivered.contains("never delivered"), "{delivered}");
}

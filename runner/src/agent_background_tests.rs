//! Tests of background subagents (issue 35): a provider that holds a subagent's request until the
//! test lets it go, so what happens "while it runs" does not depend on timing.

use super::*;
use crate::agent::Shared;
use crate::jobs::Status;
use crate::tools::testutil::Scratch;
use crate::tools::{Mode, ReadFile};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub(crate) type Replies = Vec<Result<Reply, Box<ProviderError>>>;

/// Answers the main chat from one script and the subagents from another. A subagent's request is
/// told by its system prompt. Each subagent request waits for one permit from the test.
pub(crate) struct Split {
    main: Mutex<VecDeque<Result<Reply, Box<ProviderError>>>>,
    child: Mutex<VecDeque<Result<Reply, Box<ProviderError>>>>,
    gate: Mutex<Receiver<()>>,
    panics: bool,
    replied: Arc<AtomicBool>,
    pub main_seen: Shared<Vec<Vec<u8>>>,
    pub child_seen: Shared<Vec<Vec<u8>>>,
}

/// What the test keeps of a [`Split`].
pub(crate) struct Handles {
    gate: Sender<()>,
    replied: Arc<AtomicBool>,
    pub main_seen: Shared<Vec<Vec<u8>>>,
    pub child_seen: Shared<Vec<Vec<u8>>>,
}

impl Handles {
    /// Lets one waiting subagent request go.
    pub fn release(&self) {
        let _ = self.gate.send(());
    }

    /// Lets `n` waiting subagent requests go.
    pub fn release_n(&self, n: usize) {
        for _ in 0..n {
            self.release();
        }
    }

    /// Waits until a subagent request has been answered.
    pub fn wait_for_reply(&self) {
        for _ in 0..500 {
            if self.replied.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no subagent request was answered");
    }
}

impl Split {
    pub fn new(main: Replies, child: Replies) -> (Self, Handles) {
        let (gate, receiver) = channel();
        let replied = Arc::new(AtomicBool::new(false));
        let split = Self {
            main: Mutex::new(main.into()),
            child: Mutex::new(child.into()),
            gate: Mutex::new(receiver),
            panics: false,
            replied: Arc::clone(&replied),
            main_seen: Shared::new(Vec::new()),
            child_seen: Shared::new(Vec::new()),
        };
        let handles = Handles {
            gate,
            replied,
            main_seen: split.main_seen.clone(),
            child_seen: split.child_seen.clone(),
        };
        (split, handles)
    }

    /// A provider whose subagent requests panic.
    pub fn panicking() -> (Self, Handles) {
        let (mut split, handles) = Self::new(vec![], vec![]);
        split.panics = true;
        (split, handles)
    }
}

impl Provider for Split {
    fn complete(&self, request: &[u8]) -> Result<Reply, Box<ProviderError>> {
        let value: serde_json::Value = serde_json::from_slice(request).unwrap();
        let system = value["messages"][0]["content"].as_str().unwrap_or("");
        if system.starts_with("You are a subagent") {
            self.child_seen.borrow_mut().push(request.to_vec());
            // A closed gate (the test is over) lets it go at once.
            let _ = self
                .gate
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10));
            assert!(!self.panics, "the subagent's provider fails");
            let reply = self
                .child
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| ScriptedProvider::text("(no more scripted replies)"));
            self.replied.store(true, Ordering::SeqCst);
            return reply;
        }
        self.main_seen.borrow_mut().push(request.to_vec());
        self.main
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| ScriptedProvider::text("(no more scripted replies)"))
    }
}

/// A main agent that saves into `<dir>/harness.db` and can start subagents.
pub(crate) fn background_agent(dir: &Scratch, provider: Split) -> Agent {
    let db = dir.path().join("harness.db");
    let mut store = Store::open(&db).unwrap();
    crate::store_profiles::seed(&store.conn, true).unwrap();
    crate::store_prompts::seed_subagent(&store.conn, true).unwrap();
    store.set_setting("noise", "1").unwrap();
    let log = TurnLog::create(&dir.path().join("logs")).unwrap();
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(ReadFile::new(dir.path()))];
    let cwd = dir.path().to_path_buf();
    Agent::new("m", Box::new(provider), tools, log)
        .with_target("test", "http://x/v1")
        .with_subagents(Arc::new(move |kit, cancel| {
            crate::subagent::tools(&cwd, &db, cancel, kit)
        }))
        .with_store(
            store,
            StoreContext {
                cwd: dir.path().display().to_string(),
            },
        )
}

/// How many rows an SQL count finds in `<dir>/harness.db`.
pub(crate) fn count(dir: &Scratch, sql: &str) -> i64 {
    let store = Store::open(&dir.path().join("harness.db")).unwrap();
    store.conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

pub(crate) fn wait_for(agent: &Agent, n: u32, want: Status) {
    for _ in 0..500 {
        if agent
            .subagent_runs()
            .iter()
            .any(|r| r.n == n && r.status == want)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "subagent #{n} did not become {want:?}: {:?}",
        agent.subagent_runs()
    );
}

pub(crate) fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).unwrap()
}

fn contents(request: &[u8]) -> Vec<String> {
    json(request)["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_str().map(str::to_owned))
        .collect()
}

#[derive(Default)]
struct Notes(Vec<String>);

impl Observer for Notes {
    fn subagent_note(&mut self, text: &str) {
        self.0.push(text.to_owned());
    }
}

const DELIVERY_MARK: &str = "This text is from a subagent, not from the user:";

#[test]
fn the_start_returns_at_once_and_the_run_goes_on_until_it_is_let_go() {
    let dir = Scratch::new("bg-start");
    let (split, h) = Split::new(vec![], vec![ScriptedProvider::text("the plan")]);
    let mut agent = background_agent(&dir, split);
    let run = agent.start_subagent("planner plan a login form").unwrap();
    assert_eq!(
        (run.n, run.profile.as_str(), run.coder),
        (1, "planner", false)
    );
    assert_eq!(agent.running_subagents(), 1);
    let runs = agent.subagent_runs();
    assert_eq!(runs[0].task, "plan a login form");
    assert_eq!(runs[0].status, Status::Running);

    h.release();
    wait_for(&agent, 1, Status::Done);
    assert_eq!(agent.running_subagents(), 0);
    let done = &agent.subagent_runs()[0];
    assert!(
        done.result
            .as_deref()
            .unwrap()
            .starts_with("the plan\n[subagent planner (reader)")
    );
    assert_eq!(done.calls, 0);
}

#[test]
fn a_first_word_that_is_not_a_profile_is_part_of_the_task() {
    let dir = Scratch::new("bg-profile");
    let (split, h) = Split::new(vec![], vec![]);
    let mut agent = background_agent(&dir, split);
    let run = agent.start_subagent("unknownword do the thing").unwrap();
    assert_eq!(run.profile, "default");
    assert_eq!(agent.subagent_runs()[0].task, "unknownword do the thing");
    let run = agent.start_subagent("PLANNER again").unwrap();
    assert_eq!(run.profile, "planner");
    assert_eq!(agent.subagent_runs()[1].task, "again");
    assert!(
        agent
            .start_subagent("planner")
            .unwrap_err()
            .contains("usage")
    );
    assert!(agent.start_subagent("   ").unwrap_err().contains("usage"));
    h.release();
    h.release();
}

#[test]
fn an_answer_is_added_ahead_of_the_next_message_once() {
    let dir = Scratch::new("bg-deliver");
    let (split, h) = Split::new(
        vec![
            ScriptedProvider::text("noted"),
            ScriptedProvider::text("ok"),
        ],
        vec![ScriptedProvider::text("the plan")],
    );
    let mut agent = background_agent(&dir, split);
    agent.start_subagent("planner plan it").unwrap();
    h.release();
    wait_for(&agent, 1, Status::Done);

    let mut notes = Notes::default();
    assert_eq!(
        agent.send("what now?", &mut notes),
        Outcome::Answer("noted".into())
    );
    let first = contents(&h.main_seen.borrow()[0]);
    let n = first.len();
    assert_eq!(first[n - 1], "what now?");
    assert!(
        first[n - 2].starts_with("Subagent #1 (planner, reader) finished."),
        "{first:?}"
    );
    assert!(first[n - 2].contains(DELIVERY_MARK));
    assert!(first[n - 2].contains("the plan"));
    assert!(
        notes
            .0
            .iter()
            .any(|l| l.contains("subagent #1 planner done")),
        "{:?}",
        notes.0
    );
    assert!(
        notes
            .0
            .iter()
            .any(|l| l.contains("subagent #1 planner: answer added to the chat")),
        "{:?}",
        notes.0
    );

    // The second message does not bring it again.
    assert_eq!(
        agent.send("and?", &mut Notes::default()),
        Outcome::Answer("ok".into())
    );
    let second = contents(&h.main_seen.borrow()[1]);
    let marks = second.iter().filter(|m| m.contains(DELIVERY_MARK)).count();
    assert_eq!(marks, 1, "{second:?}");
    // The answer is part of the saved chat.
    assert_eq!(
        count(
            &dir,
            "SELECT COUNT(*) FROM messages WHERE text LIKE 'Subagent #1%' AND session_id = 1"
        ),
        1
    );
}

/// Lets the subagent go while the main message is being worked on.
struct ReleasesInTheMiddle<'a>(&'a Handles);

impl Observer for ReleasesInTheMiddle<'_> {
    fn tool_call(&mut self, _call: &ToolCall) {
        self.0.release();
        self.0.wait_for_reply();
    }
}

#[test]
fn a_run_that_ends_during_a_message_waits_for_the_next_one() {
    let dir = Scratch::new("bg-midway");
    std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
    let (split, h) = Split::new(
        vec![
            ScriptedProvider::tool_call("c1", "read_file", r#"{"path":"a.txt"}"#),
            ScriptedProvider::text("read it"),
            ScriptedProvider::text("next ok"),
        ],
        vec![ScriptedProvider::text("the plan")],
    );
    let mut agent = background_agent(&dir, split);
    agent.start_subagent("planner plan it").unwrap();
    assert_eq!(
        agent.send("look", &mut ReleasesInTheMiddle(&h)),
        Outcome::Answer("read it".into())
    );
    for request in h.main_seen.borrow().iter().take(2) {
        let text = contents(request).join("\n");
        assert!(!text.contains(DELIVERY_MARK), "{text}");
    }
    // The second request is the first one and what came back: a prefix, so the cache works.
    wait_for(&agent, 1, Status::Done);
    assert_eq!(
        agent.send("again", &mut Notes::default()),
        Outcome::Answer("next ok".into())
    );
    let last = contents(&h.main_seen.borrow()[2]);
    assert!(last[last.len() - 2].contains(DELIVERY_MARK), "{last:?}");
}

#[test]
fn a_failed_message_takes_the_answer_back_and_the_next_one_brings_it() {
    let dir = Scratch::new("bg-undeliver");
    let (split, h) = Split::new(
        vec![
            ScriptedProvider::failure("boom"),
            ScriptedProvider::text("fine"),
        ],
        vec![ScriptedProvider::text("the plan")],
    );
    let mut agent = background_agent(&dir, split);
    agent.start_subagent("planner plan it").unwrap();
    h.release();
    wait_for(&agent, 1, Status::Done);
    assert!(matches!(
        agent.send("one", &mut Notes::default()),
        Outcome::Failed(_)
    ));
    assert_eq!(
        agent.send("two", &mut Notes::default()),
        Outcome::Answer("fine".into())
    );
    let second = contents(&h.main_seen.borrow()[1]);
    let marks = second.iter().filter(|m| m.contains(DELIVERY_MARK)).count();
    assert_eq!(marks, 1, "{second:?}");
}

#[test]
fn an_answer_goes_only_into_the_chat_that_asked() {
    let dir = Scratch::new("bg-chat");
    let (split, h) = Split::new(
        vec![ScriptedProvider::text("hi")],
        vec![ScriptedProvider::text("the plan")],
    );
    let mut agent = background_agent(&dir, split);
    agent.start_subagent("planner plan it").unwrap();
    h.release();
    wait_for(&agent, 1, Status::Done);
    agent.new_chat().unwrap();
    agent.send("hello", &mut Notes::default());
    let text = contents(&h.main_seen.borrow()[0]).join("\n");
    assert!(!text.contains(DELIVERY_MARK), "{text}");
}

#[test]
fn stop_ends_only_that_run_and_a_stopped_run_is_not_delivered() {
    let dir = Scratch::new("bg-stop");
    let (split, h) = Split::new(
        vec![ScriptedProvider::text("ok")],
        vec![ScriptedProvider::text("a"), ScriptedProvider::text("b")],
    );
    let mut agent = background_agent(&dir, split);
    agent.start_subagent("planner one").unwrap();
    agent.start_subagent("planner two").unwrap();
    agent.stop_subagent(1).unwrap();
    assert!(
        agent
            .stop_subagent(9)
            .unwrap_err()
            .contains("no subagent #9")
    );
    h.release();
    h.release();
    wait_for(&agent, 1, Status::Stopped);
    wait_for(&agent, 2, Status::Done);
    assert!(agent.stop_subagent(2).unwrap_err().contains("not running"));

    agent.send("go", &mut Notes::default());
    let text = contents(&h.main_seen.borrow()[0]).join("\n");
    assert!(text.contains("Subagent #2"), "{text}");
    assert!(!text.contains("Subagent #1"), "{text}");
}

#[test]
fn only_three_run_at_once_and_only_one_changes_files() {
    let dir = Scratch::new("bg-limits");
    let (split, h) = Split::new(vec![], vec![]);
    let mut agent = background_agent(&dir, split);
    for _ in 0..3 {
        agent.start_subagent("planner look").unwrap();
    }
    let why = agent.start_subagent("planner one more").unwrap_err();
    assert!(why.contains("3 subagents are already running"), "{why}");
    for _ in 0..3 {
        h.release();
    }
    for n in 1..=3 {
        wait_for(&agent, n, Status::Done);
    }

    agent.set_approval_mode(Mode::Auto).unwrap();
    let first = agent.start_subagent("builder build it").unwrap();
    assert!(first.coder);
    let why = agent.start_subagent("builder build more").unwrap_err();
    assert!(why.contains("already running (#4)"), "{why}");
    // A planner lists no file tool, so it stays a reader even in auto and starts next to it.
    let next = agent.start_subagent("planner look").unwrap();
    assert!(!next.coder);
    h.release();
}

#[test]
fn a_delegation_is_refused_while_a_coder_runs() {
    let dir = Scratch::new("bg-delegate-busy");
    let (split, h) = Split::new(
        vec![
            ScriptedProvider::tool_call(
                "d1",
                "delegate",
                r#"{"task":"build it too","profile":"builder"}"#,
            ),
            ScriptedProvider::text("understood"),
        ],
        vec![],
    );
    let mut agent = background_agent(&dir, split);
    agent.set_approval_mode(Mode::Auto).unwrap();
    agent.start_subagent("builder build it").unwrap();
    agent.send("also build", &mut Notes::default());
    let back = contents(&h.main_seen.borrow()[1]);
    assert!(
        back.last().unwrap().contains("already running (#1)"),
        "{back:?}"
    );
    h.release();
}

#[test]
fn a_thread_that_panics_is_a_failed_run_and_not_a_hang() {
    let dir = Scratch::new("bg-panic");
    let (split, h) = Split::panicking();
    let mut agent = background_agent(&dir, split);
    agent.start_subagent("planner plan it").unwrap();
    h.release();
    wait_for(&agent, 1, Status::Failed);
    assert!(
        agent.subagent_runs()[0]
            .result
            .as_deref()
            .unwrap()
            .starts_with("error:")
    );
    assert_eq!(agent.running_subagents(), 0);
}

#[test]
fn cancelling_the_main_chat_leaves_the_runs_alone() {
    let dir = Scratch::new("bg-cancel");
    let (split, h) = Split::new(vec![], vec![ScriptedProvider::text("the plan")]);
    let mut agent = background_agent(&dir, split);
    agent.start_subagent("planner plan it").unwrap();
    agent.cancel_handle().cancel();
    assert_eq!(agent.subagent_runs()[0].status, Status::Running);
    h.release();
    wait_for(&agent, 1, Status::Done);
}

#[test]
fn a_chat_that_cannot_save_cannot_start_a_background_run() {
    let dir = Scratch::new("bg-nosave");
    let log = TurnLog::create(&dir.path().join("logs")).unwrap();
    let (split, _h) = Split::new(vec![], vec![]);
    let mut agent =
        Agent::new("m", Box::new(split), vec![], log).with_target("test", "http://x/v1");
    assert!(agent.start_subagent("do it").is_err());
}

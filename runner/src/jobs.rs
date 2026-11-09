//! Subagents running in the background (issue 35): `/subagent` starts one on a thread of its own and
//! the chat goes on.
//!
//! A thread cannot take the agent with it (the program is single-threaded and built on `Rc`), so
//! it builds its own from plain data (`agent::run_child`). What the two sides share is one small
//! state per run behind a lock: the thread writes its progress and its result, the chat reads them.
//! Nothing is printed from the thread. The chat picks up what finished at the start of the next
//! message (`take_finished`), so a request in progress never changes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use kernel::ToolCall;

use crate::agent::{ChildResult, Observer};
use crate::cancel::Cancel;

/// How many runs may be going at once.
pub const MAX_RUNNING: usize = 3;
/// How many of the latest calls of a run are kept for `/subagents show`.
const RECENT_CALLS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Done,
    Failed,
    Stopped,
}

impl Status {
    pub fn name(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Stopped => "stopped",
        }
    }
}

/// What the thread writes and the chat reads.
#[derive(Debug)]
struct State {
    status: Status,
    calls: u32,
    recent: Vec<String>,
    result: Option<String>,
    prompt_tokens: u64,
    completion_tokens: u64,
    ended: Option<Instant>,
}

struct Run {
    n: u32,
    profile: String,
    coder: bool,
    task: String,
    /// The chat that started it: its answer is delivered into that chat only.
    chat: Option<i64>,
    started: Instant,
    cancel: Cancel,
    /// Set by `stop`. The cancel alone is not enough: the subagent's own chat clears it when its
    /// first message starts, so a stop that came before that would be lost.
    stopped: Arc<AtomicBool>,
    state: Arc<Mutex<State>>,
    delivered: bool,
    announced: bool,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

/// What `/subagents` shows about a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunInfo {
    pub n: u32,
    pub profile: String,
    pub coder: bool,
    pub task: String,
    pub status: Status,
    pub calls: u32,
    pub seconds: u64,
    pub recent: Vec<String>,
    pub result: Option<String>,
}

impl RunInfo {
    /// `a2  running  1m 05s  planner: find where x is decided`, the way `task list` shows it.
    pub fn line(&self) -> String {
        let head = format!(
            "a{}  {}  {}  {}: ",
            self.n,
            self.status.name(),
            crate::tools::clock(std::time::Duration::from_secs(self.seconds)),
            self.profile
        );
        let task: String = self
            .task
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(60)
            .collect();
        format!("{head}{task}")
    }

    /// Its line, the latest calls and, once it has one, its answer (cut at `max_answer` characters).
    pub fn detail(&self, max_answer: usize) -> String {
        let mut text = self.line();
        if !self.recent.is_empty() {
            text.push_str("\nlatest calls:");
            for call in &self.recent {
                text.push_str(&format!("\n  - {call}"));
            }
        }
        match &self.result {
            Some(result) => {
                let shown: String = result.chars().take(max_answer).collect();
                text.push_str(&format!("\nanswer:\n{shown}"));
            }
            None => text.push_str("\n(no answer yet)"),
        }
        text
    }
}

/// A run seen from outside the chat: it shares the run's progress, so it stays current.
#[derive(Clone)]
pub struct RunView {
    pub n: u32,
    pub profile: String,
    pub task: String,
    started: Instant,
    state: Arc<Mutex<State>>,
}

impl RunView {
    /// Whether it is still going, how many calls it has made, and for how many seconds it has gone.
    pub fn progress(&self) -> (bool, u32, u64) {
        let state = lock(&self.state);
        let end = state.ended.unwrap_or_else(Instant::now);
        (
            state.status == Status::Running,
            state.calls,
            end.duration_since(self.started).as_secs(),
        )
    }
}

/// An answer to put in the chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub n: u32,
    pub profile: String,
    pub coder: bool,
    pub status: Status,
    pub text: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

impl Delivery {
    /// The header the model reads before the text.
    pub fn header(&self) -> String {
        let kind = if self.coder { "coder" } else { "reader" };
        let what = match self.status {
            Status::Failed => "did not finish",
            _ => "finished",
        };
        format!(
            "Subagent #{} ({}, {kind}) {what}. This text is from a subagent, not from the user:",
            self.n, self.profile
        )
    }
}

/// The text added to the chat for these answers.
pub fn delivery_text(deliveries: &[Delivery]) -> String {
    deliveries
        .iter()
        .map(|d| format!("{}\n\n{}", d.header(), d.text))
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

#[derive(Default)]
pub struct Jobs {
    runs: Vec<Run>,
    next: u32,
}

/// The thread's view: counts calls and remembers the latest few.
struct RunObserver(Arc<Mutex<State>>);

impl Observer for RunObserver {
    fn tool_call(&mut self, call: &ToolCall) {
        let mut state = lock(&self.0);
        state.calls += 1;
        let line = format!(
            "{} {}",
            call.name,
            crate::toolview::summary(&call.name, &call.arguments, 70)
        );
        state.recent.push(line);
        if state.recent.len() > RECENT_CALLS {
            state.recent.remove(0);
        }
    }
}

impl Jobs {
    /// Starts `work` on a thread. Refused when too many are running, or when it changes files and
    /// another that does is running.
    pub fn start(
        &mut self,
        profile: &str,
        coder: bool,
        task: &str,
        chat: Option<i64>,
        cancel: Cancel,
        work: impl FnOnce(&mut dyn Observer) -> ChildResult + Send + 'static,
    ) -> Result<u32, String> {
        if self.running() >= MAX_RUNNING {
            return Err(format!(
                "{MAX_RUNNING} subagents are already running; wait for one or /subagents stop <number>"
            ));
        }
        if coder && let Some(n) = self.coder_running() {
            return Err(format!(
                "a subagent that changes files is already running (#{n}); two in one folder would collide"
            ));
        }
        self.next += 1;
        let n = self.next;
        let state = Arc::new(Mutex::new(State {
            status: Status::Running,
            calls: 0,
            recent: Vec::new(),
            result: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            ended: None,
        }));
        let stopped = Arc::new(AtomicBool::new(false));
        let (thread_state, thread_stopped) = (Arc::clone(&state), Arc::clone(&stopped));
        std::thread::Builder::new()
            .name(format!("subagent-{n}"))
            .spawn(move || {
                let mut observer = RunObserver(Arc::clone(&thread_state));
                let outcome =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&mut observer)));
                let mut state = lock(&thread_state);
                match outcome {
                    Ok(result) => {
                        state.status = if thread_stopped.load(Ordering::SeqCst) {
                            Status::Stopped
                        } else if result.failed() {
                            Status::Failed
                        } else {
                            Status::Done
                        };
                        state.prompt_tokens = result.prompt_tokens;
                        state.completion_tokens = result.completion_tokens;
                        state.result = Some(result.text);
                    }
                    Err(_) => {
                        state.status = Status::Failed;
                        state.result = Some("error: the subagent stopped unexpectedly".to_owned());
                    }
                }
                state.ended = Some(Instant::now());
            })
            .map_err(|e| format!("cannot start a thread: {e}"))?;
        self.runs.push(Run {
            n,
            profile: profile.to_owned(),
            coder,
            task: task.to_owned(),
            chat,
            started: Instant::now(),
            cancel,
            stopped,
            state,
            delivered: false,
            announced: false,
        });
        Ok(n)
    }

    /// How many are running now.
    pub fn running(&self) -> usize {
        self.runs
            .iter()
            .filter(|r| lock(&r.state).status == Status::Running)
            .count()
    }

    /// The number of the running run that changes files, if one is.
    pub fn coder_running(&self) -> Option<u32> {
        self.runs
            .iter()
            .find(|r| r.coder && lock(&r.state).status == Status::Running)
            .map(|r| r.n)
    }

    fn info(run: &Run) -> RunInfo {
        let state = lock(&run.state);
        let end = state.ended.unwrap_or_else(Instant::now);
        RunInfo {
            n: run.n,
            profile: run.profile.clone(),
            coder: run.coder,
            task: run.task.clone(),
            status: state.status,
            calls: state.calls,
            seconds: end.duration_since(run.started).as_secs(),
            recent: state.recent.clone(),
            result: state.result.clone(),
        }
    }

    pub fn list(&self) -> Vec<RunInfo> {
        self.runs.iter().map(Self::info).collect()
    }

    /// The runs as they are now, kept watching: a copy that goes on showing their progress after
    /// the chat has taken the program over (it is what the list under the box reads).
    pub fn views(&self) -> Vec<RunView> {
        self.runs
            .iter()
            .map(|r| RunView {
                n: r.n,
                profile: r.profile.clone(),
                task: r.task.clone(),
                started: r.started,
                state: Arc::clone(&r.state),
            })
            .collect()
    }

    /// Stops one run. It is not delivered to the chat.
    pub fn stop(&mut self, n: u32) -> Result<(), String> {
        let run = self
            .runs
            .iter_mut()
            .find(|r| r.n == n)
            .ok_or_else(|| format!("there is no subagent #{n} in this run (see /subagents)"))?;
        if lock(&run.state).status != Status::Running {
            return Err(format!("subagent #{n} is not running"));
        }
        run.stopped.store(true, Ordering::SeqCst);
        run.cancel.cancel();
        run.delivered = true;
        Ok(())
    }

    /// The runs of chat `chat` that ended and were not yet put in it, marked as delivered.
    /// A run that was stopped is not delivered.
    pub fn take_finished(&mut self, chat: Option<i64>) -> Vec<Delivery> {
        let mut out = Vec::new();
        for run in &mut self.runs {
            if run.delivered || run.chat != chat {
                continue;
            }
            let state = lock(&run.state);
            if state.status == Status::Running {
                continue;
            }
            let Some(text) = state.result.clone() else {
                continue;
            };
            run.delivered = true;
            out.push(Delivery {
                n: run.n,
                profile: run.profile.clone(),
                coder: run.coder,
                status: state.status,
                text,
                prompt_tokens: state.prompt_tokens,
                completion_tokens: state.completion_tokens,
            });
        }
        out
    }

    /// Whether a run of chat `chat` ended with an answer that is not in the chat yet.
    pub fn has_finished(&self, chat: Option<i64>) -> bool {
        self.runs.iter().any(|r| {
            let state = lock(&r.state);
            !r.delivered
                && r.chat == chat
                && state.status != Status::Running
                && state.result.is_some()
        })
    }

    /// Puts runs back as not delivered (the message they were added to did not go through).
    pub fn undeliver(&mut self, numbers: &[u32]) {
        for run in self.runs.iter_mut().filter(|r| numbers.contains(&r.n)) {
            run.delivered = false;
        }
    }

    /// One line for each run that ended since the last call, to show once.
    pub fn take_announcements(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        for run in &mut self.runs {
            if run.announced {
                continue;
            }
            let state = lock(&run.state);
            if state.status == Status::Running {
                continue;
            }
            run.announced = true;
            lines.push(format!(
                "subagent #{} {} {}: {} call{}, {} prompt tokens",
                run.n,
                run.profile,
                state.status.name(),
                state.calls,
                if state.calls == 1 { "" } else { "s" },
                state.prompt_tokens
            ));
        }
        lines
    }
}

//! Commands that run in the background (issue 42): `bash` with `background: true` starts one and
//! returns at once, the `task` tool lists them, reads their latest output and stops them, and the
//! owner sees them with `/tasks`.
//!
//! The commands run on threads of their own (a reader for the output, a waiter for the exit);
//! what the two sides share is one small state per task behind a lock. Each command is in its own
//! process group, so stopping one stops everything it started. Unix only, like `bash`.
//!
//! They stop with the program: dropping the last copy of the registry kills what is still running.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use kernel::ToolSpec;
use serde::Deserialize;

use super::Tool;
use super::bash::kill_group;

/// How many commands may run in the background at once.
pub const MAX_RUNNING: usize = 5;
/// How much of the latest output of a task is kept.
const KEPT_BYTES: usize = 64 * 1024;
/// How much of it `task output` gives the model.
const SHOWN_BYTES: usize = 6000;
/// How many ended tasks are remembered before the oldest are forgotten.
const KEPT_ENDED: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellStatus {
    Running,
    /// Ended by itself, with this exit code.
    Exited(i32),
    /// Ended by a signal it was not sent by us.
    Signalled,
    /// Stopped on request.
    Stopped,
}

impl ShellStatus {
    pub fn text(self) -> String {
        match self {
            ShellStatus::Running => "running".to_owned(),
            ShellStatus::Exited(code) => format!("exited {code}"),
            ShellStatus::Signalled => "killed by a signal".to_owned(),
            ShellStatus::Stopped => "stopped".to_owned(),
        }
    }
}

#[derive(Debug)]
struct State {
    status: ShellStatus,
    /// The latest output, at most [`KEPT_BYTES`].
    tail: Vec<u8>,
    /// How many bytes of output came before `tail`.
    dropped: usize,
    stop_asked: bool,
    ended: Option<Instant>,
}

struct Task {
    n: u32,
    command: String,
    group: u32,
    started: Instant,
    announced: bool,
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct Inner {
    next: u32,
    tasks: Vec<Task>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        for task in &self.tasks {
            if lock(&task.state).status == ShellStatus::Running {
                kill_group(task.group);
            }
        }
    }
}

/// What the owner and the model are told about one task.
#[derive(Debug, Clone)]
pub struct ShellInfo {
    pub n: u32,
    pub command: String,
    pub status: ShellStatus,
    pub elapsed: Duration,
    /// Bytes of output so far, counting what is no longer kept.
    pub bytes: usize,
}

impl ShellInfo {
    /// `s2  running  1m 05s  npm run dev`
    pub fn line(&self) -> String {
        let command: String = self
            .command
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let shown: String = command.chars().take(60).collect();
        let dots = if command.chars().count() > 60 {
            "..."
        } else {
            ""
        };
        format!(
            "s{}  {}  {}  {shown}{dots}",
            self.n,
            self.status.text(),
            clock(self.elapsed)
        )
    }
}

/// `45s`, `1m 05s`, `2h 03m`.
pub fn clock(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, secs % 3600 / 60),
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The commands running in the background in this run. Copies share the same list.
#[derive(Clone, Default)]
pub struct ShellTasks(Arc<Mutex<Inner>>);

impl ShellTasks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts `command` in `cwd` and returns its number.
    pub fn start(&self, command: &str, cwd: &Path) -> Result<u32, String> {
        let mut inner = lock(&self.0);
        let running = inner
            .tasks
            .iter()
            .filter(|t| lock(&t.state).status == ShellStatus::Running)
            .count();
        if running >= MAX_RUNNING {
            return Err(format!(
                "error: {running} commands are already running in the background; stop one first"
            ));
        }
        let mut start = Command::new("bash");
        start
            .arg("-c")
            .arg(format!("exec 2>&1\n{command}"))
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        // Same rule as `bash`: a background command does not get the provider's key either.
        let mut child = super::child_env::scrub(&mut start)
            .spawn()
            .map_err(|e| format!("error: could not start bash: {e}"))?;
        let group = child.id();
        let mut stdout = child.stdout.take().expect("stdout was piped");
        let state = Arc::new(Mutex::new(State {
            status: ShellStatus::Running,
            tail: Vec::new(),
            dropped: 0,
            stop_asked: false,
            ended: None,
        }));

        let for_reader = Arc::clone(&state);
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut state = lock(&for_reader);
                        state.tail.extend_from_slice(&buf[..n]);
                        if state.tail.len() > KEPT_BYTES {
                            let extra = state.tail.len() - KEPT_BYTES;
                            state.tail.drain(..extra);
                            state.dropped += extra;
                        }
                    }
                }
            }
        });
        let for_waiter = Arc::clone(&state);
        thread::spawn(move || {
            let result = child.wait();
            // Whatever it left running is stopped too; that also closes the output pipe.
            kill_group(group);
            let mut state = lock(&for_waiter);
            state.status = if state.stop_asked {
                ShellStatus::Stopped
            } else {
                match result.ok().and_then(|s| s.code()) {
                    Some(code) => ShellStatus::Exited(code),
                    None => ShellStatus::Signalled,
                }
            };
            state.ended = Some(Instant::now());
        });

        inner.next += 1;
        let n = inner.next;
        inner.tasks.push(Task {
            n,
            command: command.to_owned(),
            group,
            started: Instant::now(),
            announced: false,
            state,
        });
        // Forget the oldest ended ones past the limit.
        let ended = inner
            .tasks
            .iter()
            .filter(|t| lock(&t.state).status != ShellStatus::Running)
            .count();
        for _ in KEPT_ENDED..ended {
            if let Some(at) = inner
                .tasks
                .iter()
                .position(|t| lock(&t.state).status != ShellStatus::Running)
            {
                inner.tasks.remove(at);
            }
        }
        Ok(n)
    }

    fn info_of(task: &Task) -> ShellInfo {
        let state = lock(&task.state);
        ShellInfo {
            n: task.n,
            command: task.command.clone(),
            status: state.status,
            elapsed: state
                .ended
                .unwrap_or_else(Instant::now)
                .duration_since(task.started),
            bytes: state.dropped + state.tail.len(),
        }
    }

    /// Every remembered task, oldest first.
    pub fn list(&self) -> Vec<ShellInfo> {
        lock(&self.0).tasks.iter().map(Self::info_of).collect()
    }

    pub fn running(&self) -> usize {
        self.list()
            .iter()
            .filter(|t| t.status == ShellStatus::Running)
            .count()
    }

    /// The latest output of task `n`, at most `max` bytes (cut at a character), and its facts.
    pub fn output(&self, n: u32, max: usize) -> Result<(ShellInfo, String), String> {
        let (info, text) = self.output_now(n, max)?;
        Ok((
            ShellInfo {
                status: self.status(n).unwrap_or(info.status),
                ..info
            },
            text,
        ))
    }

    /// `output`, after waiting up to `wait` for task `n` to stop running. The wait happens with no
    /// lock held, so a long build can be followed without blocking the rest of the run.
    pub fn output_waiting(
        &self,
        n: u32,
        max: usize,
        wait: Duration,
    ) -> Result<(ShellInfo, String), String> {
        let from = Instant::now();
        loop {
            let now = self.output_now(n, max)?;
            if now.0.status != ShellStatus::Running || from.elapsed() >= wait {
                return Ok(now);
            }
            thread::sleep(Duration::from_millis(25).min(wait));
        }
    }

    /// The status of task `n`, without its output.
    pub fn status(&self, n: u32) -> Option<ShellStatus> {
        let inner = lock(&self.0);
        let task = inner.tasks.iter().find(|t| t.n == n)?;
        Some(lock(&task.state).status)
    }

    fn output_now(&self, n: u32, max: usize) -> Result<(ShellInfo, String), String> {
        let inner = lock(&self.0);
        let task = inner
            .tasks
            .iter()
            .find(|t| t.n == n)
            .ok_or_else(|| format!("error: there is no task s{n}"))?;
        let info = Self::info_of(task);
        let state = lock(&task.state);
        let from = state.tail.len().saturating_sub(max);
        let mut text = String::from_utf8_lossy(&state.tail[from..]).into_owned();
        if from > 0 || state.dropped > 0 {
            text = format!(
                "[earlier output left out]\n{}",
                text.trim_start_matches('\u{fffd}')
            );
        }
        Ok((info, text))
    }

    /// Stops task `n` and everything it started.
    pub fn stop(&self, n: u32) -> Result<(), String> {
        let inner = lock(&self.0);
        let task = inner
            .tasks
            .iter()
            .find(|t| t.n == n)
            .ok_or_else(|| format!("error: there is no task s{n}"))?;
        let mut state = lock(&task.state);
        if state.status != ShellStatus::Running {
            return Err(format!(
                "error: s{n} is not running ({})",
                state.status.text()
            ));
        }
        state.stop_asked = true;
        drop(state);
        kill_group(task.group);
        Ok(())
    }

    /// One line for each task that ended since the last call, to show once.
    pub fn take_announcements(&self) -> Vec<String> {
        let mut inner = lock(&self.0);
        let mut lines = Vec::new();
        for task in &mut inner.tasks {
            if task.announced || lock(&task.state).status == ShellStatus::Running {
                continue;
            }
            task.announced = true;
            lines.push(format!("background command {}", Self::info_of(task).line()));
        }
        lines
    }

    /// Stops everything still running.
    pub fn stop_all(&self) {
        for info in self.list() {
            let _ = self.stop(info.n);
        }
    }
}

// ---- the `task` tool ----

const SCHEMA: &str = r#"{"type":"object","properties":{"action":{"type":"string","enum":["list","output","stop"]},"id":{"type":["string","integer"],"description":"The id shown by list: s1 for a command, a1 for a subagent. For output and stop"},"wait":{"type":"integer","description":"Seconds to wait for the task to finish before reading its output. Default 0, at most 300"}},"required":["action"]}"#;

/// The longest `task output` waits for a task to finish.
const MAX_WAIT: Duration = Duration::from_secs(300);

#[derive(Deserialize)]
#[serde(untagged)]
enum RawId {
    Number(u32),
    Text(String),
}

#[derive(Deserialize)]
struct Args {
    action: String,
    id: Option<RawId>,
    #[serde(default)]
    wait: Option<u64>,
}

/// What a `task` call asks for. The id is `('s', n)` for a command, `('a', n)` for a subagent, and
/// `('?', n)` for a bare number (a command, as before subagents could be reached). `wait` is how
/// long `output` waits for the task to finish first.
#[derive(Debug, PartialEq, Eq)]
pub struct TaskCall {
    pub action: String,
    pub id: Option<(char, u32)>,
    pub wait: Duration,
}

/// `s2` or `a1` (a bare number, or `#3`, is also taken: its kind is `'?'`).
pub fn task_id(word: &str) -> Option<(char, u32)> {
    let word = word.trim().trim_start_matches('#');
    let (kind, digits) = match word.chars().next()? {
        c @ ('s' | 'a') => (c, &word[1..]),
        _ => ('?', word),
    };
    digits.parse().ok().map(|n| (kind, n))
}

/// Reads the arguments of a `task` call; the words of the error are the ones the model sees.
pub fn parse_task_call(arguments: &str) -> Result<TaskCall, String> {
    let args: Args =
        serde_json::from_str(arguments).map_err(|e| format!("error: invalid arguments: {e}"))?;
    let id = match args.id {
        None => None,
        Some(RawId::Number(n)) => Some(('?', n)),
        Some(RawId::Text(text)) => Some(
            task_id(&text)
                .ok_or_else(|| format!("error: '{text}' is not a task id (like s1 or a1)"))?,
        ),
    };
    Ok(TaskCall {
        action: args.action,
        id,
        wait: match args.wait {
            None | Some(0) => Duration::ZERO,
            Some(secs) if Duration::from_secs(secs) > MAX_WAIT => {
                return Err(format!(
                    "error: wait is at most {} seconds",
                    MAX_WAIT.as_secs()
                ));
            }
            Some(secs) => Duration::from_secs(secs),
        },
    })
}

pub struct TaskTool {
    spec: ToolSpec,
    shells: ShellTasks,
}

impl TaskTool {
    pub fn new(shells: ShellTasks) -> Self {
        Self {
            spec: ToolSpec::new(
                "task",
                "Background work: commands started with bash and background true (ids s1, s2) and subagents started with delegate (ids a1, a2). list shows them, output reads the latest output of a command or the progress and answer of a subagent, stop ends one.",
                SCHEMA,
            )
            .expect("the schema constant is valid JSON"),
            shells,
        }
    }

    fn go(&self, arguments: &str) -> Result<String, String> {
        let call = parse_task_call(arguments)?;
        let id = || match call.id {
            Some(('a', n)) => Err(format!(
                "error: a{n} is a subagent, and subagents cannot be reached from here"
            )),
            Some((_, n)) => Ok(n),
            None => Err("error: give the task id".to_owned()),
        };
        match call.action.as_str() {
            "list" => {
                let all = self.shells.list();
                if all.is_empty() {
                    return Ok("no background commands".to_owned());
                }
                Ok(all
                    .iter()
                    .map(ShellInfo::line)
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "output" => {
                let (info, text) = self.shells.output_waiting(id()?, SHOWN_BYTES, call.wait)?;
                let body = if text.trim().is_empty() {
                    "(no output yet)".to_owned()
                } else {
                    text
                };
                Ok(format!("{}\n{body}", info.line()))
            }
            "stop" => {
                let n = id()?;
                self.shells.stop(n)?;
                Ok(format!("stopping s{n}"))
            }
            other => Err(format!(
                "error: action is list, output or stop, not '{other}'"
            )),
        }
    }
}

impl Tool for TaskTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn run(&self, arguments: &str) -> String {
        self.go(arguments).unwrap_or_else(|text| text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::Scratch;

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..300 {
            if done() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn task_ids_are_read_as_text_or_number() {
        let call = |text: &str| parse_task_call(text).unwrap();
        assert_eq!(call(r#"{"action":"stop","id":"a2"}"#).id, Some(('a', 2)));
        assert_eq!(call(r#"{"action":"stop","id":"s10"}"#).id, Some(('s', 10)));
        assert_eq!(call(r#"{"action":"output","id":3}"#).id, Some(('?', 3)));
        assert_eq!(
            call(r##"{"action":"output","id":"#3"}"##).id,
            Some(('?', 3))
        );
        assert_eq!(call(r#"{"action":"list"}"#).id, None);
        assert!(
            parse_task_call(r#"{"action":"stop","id":"x"}"#)
                .unwrap_err()
                .contains("not a task id")
        );
    }

    #[test]
    fn the_tool_alone_points_a_subagent_id_to_the_agent() {
        let tool = TaskTool::new(ShellTasks::new());
        let out = tool.run(r#"{"action":"stop","id":"a1"}"#);
        assert!(out.starts_with("error: a1 is a subagent"), "{out}");
    }

    #[test]
    fn a_command_runs_in_the_background_and_its_output_and_exit_are_kept() {
        let dir = Scratch::new("shell-run");
        let shells = ShellTasks::new();
        let n = shells
            .start("echo hello; echo oops >&2; exit 3", dir.path())
            .unwrap();
        wait_until("the command to end", || shells.running() == 0);
        wait_until("its output", || {
            shells.output(n, 100).unwrap().1.contains("oops")
        });
        let (info, text) = shells.output(n, 100).unwrap();
        assert_eq!(info.status, ShellStatus::Exited(3));
        assert_eq!(text, "hello\noops\n");
        assert!(info.line().starts_with("s1  exited 3  "), "{}", info.line());
        // Said once.
        assert_eq!(shells.take_announcements().len(), 1);
        assert!(shells.take_announcements().is_empty());
    }

    #[test]
    fn stopping_kills_the_command_and_what_it_started() {
        let dir = Scratch::new("shell-stop");
        let marker = dir.path().join("alive");
        let shells = ShellTasks::new();
        let command = format!("(sleep 1; touch {}) & sleep 30", marker.display());
        let n = shells.start(&command, dir.path()).unwrap();
        assert_eq!(shells.running(), 1);
        shells.stop(n).unwrap();
        wait_until("the stop", || shells.running() == 0);
        assert_eq!(shells.list()[0].status, ShellStatus::Stopped);
        thread::sleep(Duration::from_millis(1300));
        assert!(
            !marker.exists(),
            "the child of the command was left running"
        );
        assert!(shells.stop(n).unwrap_err().contains("is not running"));
        assert!(shells.stop(9).unwrap_err().contains("no task s9"));
    }

    #[test]
    fn only_a_few_run_at_once_and_dropping_the_list_stops_them() {
        let dir = Scratch::new("shell-limit");
        let marker = dir.path().join("late");
        let shells = ShellTasks::new();
        for _ in 0..MAX_RUNNING {
            shells
                .start(&format!("sleep 1; touch {}", marker.display()), dir.path())
                .unwrap();
        }
        let refused = shells.start("true", dir.path()).unwrap_err();
        assert!(refused.contains("already running"), "{refused}");
        drop(shells);
        thread::sleep(Duration::from_millis(1400));
        assert!(!marker.exists(), "they were left running");
    }

    #[test]
    fn only_the_latest_output_is_kept() {
        let dir = Scratch::new("shell-tail");
        let shells = ShellTasks::new();
        let n = shells
            .start(
                "head -c 200000 /dev/zero | tr '\\0' x; echo END",
                dir.path(),
            )
            .unwrap();
        wait_until("the command to end", || shells.running() == 0);
        wait_until("all output", || shells.list()[0].bytes >= 200_004);
        let (info, text) = shells.output(n, 50).unwrap();
        assert_eq!(info.bytes, 200_004);
        assert!(text.starts_with("[earlier output left out]\n"), "{text}");
        assert!(text.ends_with("xxxxEND\n"), "{text:?}");
        assert!(text.len() < 100);
    }

    #[test]
    fn the_task_tool_lists_reads_and_stops() {
        let dir = Scratch::new("shell-tool");
        let shells = ShellTasks::new();
        let tool = TaskTool::new(shells.clone());
        let call = |args: serde_json::Value| tool.run(&args.to_string());
        assert_eq!(
            call(serde_json::json!({"action": "list"})),
            "no background commands"
        );
        let n = shells.start("echo up; sleep 30", dir.path()).unwrap();
        wait_until("the output", || {
            shells.output(n, 10).unwrap().1.contains("up")
        });
        let listed = call(serde_json::json!({"action": "list"}));
        assert!(listed.starts_with("s1  running"), "{listed}");
        let out = call(serde_json::json!({"action": "output", "id": n}));
        assert!(out.contains("running") && out.ends_with("up\n"), "{out:?}");
        assert_eq!(
            call(serde_json::json!({"action": "stop", "id": n})),
            "stopping s1"
        );
        assert!(call(serde_json::json!({"action": "output"})).contains("give the task id"));
        assert!(
            call(serde_json::json!({"action": "kill", "id": 1})).contains("list, output or stop")
        );
        assert!(call(serde_json::json!({"action": "stop", "id": 4})).contains("no task s4"));
    }

    #[test]
    fn output_waits_when_asked_and_parses_the_wait_range() {
        let dir = Scratch::new("shell-tool-wait");
        let shells = ShellTasks::new();
        let tool = TaskTool::new(shells.clone());
        let call = |args: serde_json::Value| tool.run(&args.to_string());
        let n = shells.start("sleep 1; echo done", dir.path()).unwrap();
        let started = Instant::now();
        let out = call(serde_json::json!({"action": "output", "id": n, "wait": 20}));
        assert!(started.elapsed() >= Duration::from_millis(500));
        assert!(
            out.starts_with(&format!("s{n}  exited 0")) && out.ends_with("done\n"),
            "{out:?}"
        );
        // With no wait it reads at once, even while the command runs.
        let m = shells.start("sleep 30", dir.path()).unwrap();
        let quick = call(serde_json::json!({"action": "output", "id": m}));
        assert!(quick.contains("running"), "{quick:?}");
        assert!(
            call(serde_json::json!({"action": "output", "id": m, "wait": 301}))
                .contains("wait is at most 300 seconds")
        );
        let parsed = parse_task_call(r#"{"action":"output","id":"s2","wait":5}"#).unwrap();
        assert_eq!(parsed.wait, Duration::from_secs(5));
        // A wait past the limit gives up and reports what there is.
        let started = Instant::now();
        let partial = call(serde_json::json!({"action": "output", "id": n, "wait": 1}));
        assert!(started.elapsed() < Duration::from_secs(10) && partial.contains("exited 0"));
    }

    #[test]
    fn the_clock_reads_short() {
        assert_eq!(clock(Duration::from_secs(5)), "5s");
        assert_eq!(clock(Duration::from_secs(65)), "1m 05s");
        assert_eq!(clock(Duration::from_secs(7380)), "2h 03m");
    }
}

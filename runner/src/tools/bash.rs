//! `bash`: run a shell command in a fixed directory, after asking for approval.
//!
//! Unix only: it relies on process groups so a timeout kills the command and
//! everything it started.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use kernel::ToolSpec;
use serde::Deserialize;

use super::truncate::OutputCapture;
use super::{Approver, Mode, SharedMode, Tool, always_for, is_free, is_looking, shared_mode};
use crate::cancel::Cancel;

/// How long a command may run before it is killed.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

const SCHEMA: &str = r#"{"type":"object","properties":{"command":{"type":"string","description":"Shell command to run with bash -c"},"timeout":{"type":"integer","description":"Seconds before the command is killed. Default 30, at most 300. For a long build, background: true is better"},"background":{"type":"boolean","description":"Start it and return at once (a server, a long build); see the task tool. It has no time limit"}},"required":["command"]}"#;

/// The longest a command may ask to run before it is killed.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Deserialize)]
struct Args {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    background: bool,
}

pub struct Bash {
    spec: ToolSpec,
    cwd: PathBuf,
    timeout: Duration,
    // Rust note: `Box<dyn Approver>` holds any type that implements `Approver`,
    // decided when the program runs: a terminal prompt in the REPL, a scripted
    // answer in tests.
    approver: Box<dyn Approver>,
    /// Where an "always" is remembered. Standalone (tests) it is a private one.
    approval: SharedMode,
    /// Set when the user cancels the turn: a running command is killed (issue 18).
    cancel: Cancel,
    /// A subagent's `bash` (issue 33): only looking commands run, nothing is ever asked.
    looking_only: bool,
    /// Where `background: true` commands go (issue 42). None: they are refused.
    shells: Option<super::ShellTasks>,
}

impl Bash {
    /// `cwd` is the fixed working directory every command runs in.
    pub fn new(cwd: impl Into<PathBuf>, timeout: Duration, approver: Box<dyn Approver>) -> Self {
        Self {
            spec: ToolSpec::new(
                "bash",
                "Run a shell command with bash -c and return its combined output and exit code.",
                SCHEMA,
            )
            .expect("the schema constant is valid JSON"),
            cwd: cwd.into(),
            timeout,
            approver,
            approval: shared_mode(Mode::Manual),
            cancel: Cancel::new(),
            looking_only: false,
            shells: None,
        }
    }

    /// Lets `background: true` start commands in this list.
    pub fn with_shells(mut self, shells: super::ShellTasks) -> Self {
        self.shells = Some(shells);
        self
    }

    /// Runs only the looking commands that need no question (and not `mkdir`); any other command
    /// is refused without asking anyone. For a subagent, which has nobody to ask.
    pub fn looking_only(mut self) -> Self {
        self.looking_only = true;
        self
    }

    /// Watches this flag while a command runs; when it is set the command's group is killed.
    pub fn with_cancel(mut self, cancel: Cancel) -> Self {
        self.cancel = cancel;
        self
    }

    /// Remembers "always" answers in this shared state, so `/approve` can list and clear them.
    pub fn with_approval(mut self, approval: SharedMode) -> Self {
        self.approval = approval;
        self
    }

    fn execute(&self, command: &str, timeout: Duration) -> String {
        // `exec 2>&1` first makes the shell send errors to the same pipe as
        // normal output, so the two come back interleaved in the order they
        // were written. `process_group(0)` puts the command in its own process
        // group so we can kill all of it at once. Stdin is closed so a command
        // that waits for input ends instead of hanging.
        let mut start = Command::new("bash");
        start
            .arg("-c")
            .arg(format!("exec 2>&1\n{command}"))
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        // The provider's key is not passed on: `env` must not print it into the chat and the logs.
        let mut child = match super::child_env::scrub(&mut start).spawn() {
            Ok(child) => child,
            Err(e) => return format!("error: could not start bash: {e}"),
        };
        let group = child.id();
        let mut stdout = child.stdout.take().expect("stdout was piped");

        // Rust note: a second thread drains the output while this one watches
        // the clock. If nobody reads the pipe, a chatty command fills it and
        // freezes. The thread keeps only the head and tail (see OutputCapture)
        // and hands that back when it finishes.
        let reader = thread::spawn(move || {
            let mut capture = OutputCapture::default();
            let mut buf = [0u8; 8192];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => capture.push(&buf[..n]),
                }
            }
            capture
        });

        let started = Instant::now();
        let mut timed_out = false;
        let mut cancelled = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if self.cancel.is_cancelled() => {
                    cancelled = true;
                    kill_group(group);
                    let _ = child.kill();
                    break child.wait().ok();
                }
                Ok(None) if started.elapsed() >= timeout => {
                    timed_out = true;
                    kill_group(group);
                    let _ = child.kill();
                    break child.wait().ok();
                }
                Ok(None) => thread::sleep(Duration::from_millis(10)),
                Err(e) => return format!("error: could not wait for the command: {e}"),
            }
        };
        // Whatever the command left running in its group is stopped too: the
        // output pipe stays open until every member exits, so this also stops
        // `sleep 30 &` from hanging the read below.
        kill_group(group);

        let output = reader.join().expect("reader thread panicked").finish();
        let summary = if cancelled {
            "[cancelled by the user and killed]".to_owned()
        } else if timed_out {
            format!("[timed out after {:?} and was killed]", timeout)
        } else {
            match status.and_then(|s| s.code()) {
                Some(code) => format!("[exit code: {code}]"),
                None => "[terminated by a signal]".to_owned(),
            }
        };
        if output.is_empty() || output.ends_with('\n') {
            format!("{output}{summary}")
        } else {
            format!("{output}\n{summary}")
        }
    }
}

/// Sends SIGKILL to a whole process group. Errors are ignored: the group may
/// already be gone, which is the goal.
pub fn kill_group(group: u32) {
    let _ = Command::new("bash")
        .arg("-c")
        .arg(format!("kill -KILL -- -{group}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

impl Tool for Bash {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn run(&self, arguments: &str) -> String {
        let args: Args = match serde_json::from_str(arguments) {
            Ok(args) => args,
            Err(e) => return format!("error: invalid arguments: {e}"),
        };
        if self.approval.planning() && !is_looking(&args.command) {
            return "plan mode is on, so only looking commands (ls, cat, grep, find, git status ...) run; put the change in your plan instead".to_owned();
        }
        if args.background && (self.looking_only || self.shells.is_none()) {
            return "error: this bash cannot run commands in the background".to_owned();
        }
        let timeout = match args.timeout {
            None => self.timeout,
            Some(0) => {
                return format!(
                    "error: timeout must be at least 1 second and at most {}",
                    MAX_TIMEOUT.as_secs()
                );
            }
            Some(secs) if Duration::from_secs(secs) > MAX_TIMEOUT => {
                return format!(
                    "error: timeout is at most {} seconds; for a longer build use background: true",
                    MAX_TIMEOUT.as_secs()
                );
            }
            Some(secs) => Duration::from_secs(secs),
        };
        if self.looking_only {
            return if is_looking(&args.command) {
                self.execute(&args.command, timeout)
            } else {
                "error: a subagent can only look (ls, cat, head, tail, wc, grep, find, git status ...): this command was not run".to_owned()
            };
        }
        // Looking commands (`ls`, `cd`, ...) run without a question; see `commands.rs`.
        if !is_free(&args.command)
            && !self.approval.confirm(
                self.approver.as_ref(),
                "run this command?",
                &args.command,
                always_for(&args.command),
                None,
            )
        {
            return self
                .approval
                .denial("denied by user: the command was not run");
        }
        if args.background
            && let Some(shells) = &self.shells
        {
            return match shells.start(&args.command, &self.cwd) {
                Ok(n) => format!(
                    "started in the background as task {n}: task(action=output, id={n}) shows its output, task(action=stop, id={n}) stops it"
                ),
                Err(why) => why,
            };
        }
        self.execute(&args.command, timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::MAX_OUTPUT_BYTES;
    use crate::tools::testutil::Scratch;
    use std::path::Path;

    struct Fixed(bool);
    impl Approver for Fixed {
        fn approve(&self, _ask: &crate::tools::Ask) -> crate::tools::Answer {
            if self.0 {
                crate::tools::Answer::Yes
            } else {
                crate::tools::Answer::No
            }
        }
    }

    fn bash(dir: &Path, timeout: Duration, approve: bool) -> Bash {
        Bash::new(dir, timeout, Box::new(Fixed(approve)))
    }

    fn run(tool: &Bash, command: &str) -> String {
        tool.run(&serde_json::json!({ "command": command }).to_string())
    }

    #[test]
    fn plan_mode_runs_only_looking_commands_and_never_asks() {
        let dir = Scratch::new("bash-plan");
        let tool = bash(dir.path(), DEFAULT_TIMEOUT, true).with_approval({
            let approval = shared_mode(Mode::Auto);
            approval.set_planning(true);
            approval
        });
        assert!(run(&tool, "touch x").starts_with("plan mode is on, so only looking commands"));
        assert!(run(&tool, "mkdir d").starts_with("plan mode is on"));
        assert!(!dir.path().join("x").exists() && !dir.path().join("d").exists());
        assert_eq!(
            run(&tool, "pwd"),
            format!("{}\n[exit code: 0]", dir.path().display())
        );
    }

    #[test]
    fn runs_in_the_fixed_working_directory() {
        let dir = Scratch::new("bash-cwd");
        let out = run(&bash(dir.path(), DEFAULT_TIMEOUT, true), "pwd");
        assert_eq!(out, format!("{}\n[exit code: 0]", dir.path().display()));
    }

    #[test]
    fn reports_exit_codes() {
        let dir = Scratch::new("bash-exit");
        let tool = bash(dir.path(), DEFAULT_TIMEOUT, true);
        assert_eq!(run(&tool, "true"), "[exit code: 0]");
        assert_eq!(run(&tool, "exit 3"), "[exit code: 3]");
    }

    #[test]
    fn stdout_and_stderr_come_back_in_order() {
        let dir = Scratch::new("bash-merge");
        let out = run(
            &bash(dir.path(), DEFAULT_TIMEOUT, true),
            "echo one; echo two >&2; echo three",
        );
        assert_eq!(out, "one\ntwo\nthree\n[exit code: 0]");
    }

    #[test]
    fn output_without_a_final_newline_gets_one_before_the_summary() {
        let dir = Scratch::new("bash-nonl");
        let out = run(&bash(dir.path(), DEFAULT_TIMEOUT, true), "printf abc");
        assert_eq!(out, "abc\n[exit code: 0]");
    }

    #[test]
    fn denied_command_is_not_run() {
        let dir = Scratch::new("bash-denied");
        let marker = dir.path().join("ran");
        let tool = bash(dir.path(), DEFAULT_TIMEOUT, false);
        let out = run(&tool, "touch ran");
        assert_eq!(out, "denied by user: the command was not run");
        assert!(!marker.exists());
    }

    #[test]
    fn approved_command_is_run() {
        let dir = Scratch::new("bash-approved");
        let marker = dir.path().join("ran");
        let tool = bash(dir.path(), DEFAULT_TIMEOUT, true);
        run(&tool, "touch ran");
        assert!(marker.exists());
    }

    #[test]
    fn the_approver_sees_the_exact_command() {
        use std::sync::{Arc, Mutex};
        struct Recorder(Arc<Mutex<Vec<String>>>);
        impl Approver for Recorder {
            fn approve(&self, ask: &crate::tools::Ask) -> crate::tools::Answer {
                self.0.lock().unwrap().push(ask.detail.to_owned());
                crate::tools::Answer::No
            }
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let dir = Scratch::new("bash-recorder");
        let tool = Bash::new(
            dir.path(),
            DEFAULT_TIMEOUT,
            Box::new(Recorder(Arc::clone(&seen))),
        );
        run(&tool, "touch  hi   there");
        assert_eq!(*seen.lock().unwrap(), vec!["touch  hi   there".to_owned()]);
    }

    /// What was asked: the detail line and the "always" label offered, if any.
    type Seen = std::sync::Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;

    /// Records every command asked about and answers as told.
    struct Asked {
        seen: Seen,
        answer: crate::tools::Answer,
    }

    impl Approver for Asked {
        fn approve(&self, ask: &crate::tools::Ask) -> crate::tools::Answer {
            self.seen
                .lock()
                .unwrap()
                .push((ask.detail.to_owned(), ask.always.map(str::to_owned)));
            self.answer
        }
    }

    fn asked_tool(tag: &str, answer: crate::tools::Answer) -> (Bash, Seen, Scratch) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let dir = Scratch::new(tag);
        let tool = Bash::new(
            dir.path(),
            DEFAULT_TIMEOUT,
            Box::new(Asked {
                seen: std::sync::Arc::clone(&seen),
                answer,
            }),
        );
        (tool, seen, dir)
    }

    #[test]
    fn looking_commands_run_without_a_question_even_when_every_answer_is_no() {
        let (tool, seen, dir) = asked_tool("bash-free", crate::tools::Answer::No);
        std::fs::write(dir.path().join("a.txt"), "text").unwrap();
        assert!(run(&tool, "ls").contains("a.txt"));
        assert!(run(&tool, "pwd; ls -a && cat a.txt").contains("text"));
        assert!(run(&tool, "mkdir -p sub/deep").ends_with("[exit code: 0]"));
        assert!(dir.path().join("sub/deep").is_dir());
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn a_command_that_goes_beyond_looking_still_asks() {
        let (tool, seen, dir) = asked_tool("bash-not-free", crate::tools::Answer::No);
        for command in [
            "touch ran",
            "ls /",
            "ls | sh",
            "ls; touch ran",
            "cd .. && ls",
        ] {
            assert_eq!(
                run(&tool, command),
                "denied by user: the command was not run"
            );
        }
        assert!(!dir.path().join("ran").exists());
        assert_eq!(seen.lock().unwrap().len(), 5);
    }

    #[test]
    fn always_covers_the_same_program_for_the_rest_of_the_run() {
        let (tool, seen, dir) = asked_tool("bash-always", crate::tools::Answer::Always);
        assert!(run(&tool, "touch one").ends_with("[exit code: 0]"));
        assert!(run(&tool, "touch two").ends_with("[exit code: 0]"));
        assert!(dir.path().join("two").exists());
        assert_eq!(
            *seen.lock().unwrap(),
            vec![("touch one".to_owned(), Some("`touch` commands".to_owned()))],
            "the second touch was not asked about"
        );
        // Another program still asks. A chain is offered "always" for that exact line only.
        run(&tool, "rm one");
        run(&tool, "touch three; touch four");
        {
            let seen = seen.lock().unwrap();
            assert_eq!(
                seen[1],
                ("rm one".to_owned(), Some("`rm` commands".to_owned()))
            );
            assert_eq!(
                seen[2],
                (
                    "touch three; touch four".to_owned(),
                    Some("this exact command (`touch three; touch four`)".to_owned())
                )
            );
        }
        // The same line is not asked again; a different one is.
        run(&tool, "touch three; touch four");
        assert_eq!(seen.lock().unwrap().len(), 3);
        run(&tool, "touch three; touch five");
        assert_eq!(seen.lock().unwrap().len(), 4);
    }

    #[test]
    fn always_is_forgotten_on_request_and_is_listed() {
        let approval = crate::tools::shared_mode(crate::tools::Mode::Manual);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let dir = Scratch::new("bash-forget");
        let tool = Bash::new(
            dir.path(),
            DEFAULT_TIMEOUT,
            Box::new(Asked {
                seen: std::sync::Arc::clone(&seen),
                answer: crate::tools::Answer::Always,
            }),
        )
        .with_approval(std::rc::Rc::clone(&approval));
        run(&tool, "touch a");
        assert_eq!(
            approval.always_labels(),
            vec!["`touch` commands".to_owned()]
        );
        assert_eq!(approval.forget_always(), 1);
        run(&tool, "touch b");
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "asked again after forgetting"
        );
    }

    #[test]
    fn long_output_is_truncated_and_does_not_hang() {
        let dir = Scratch::new("bash-big");
        let out = run(
            &bash(dir.path(), DEFAULT_TIMEOUT, true),
            "head -c 100000 /dev/zero | tr '\\0' 'a'",
        );
        // 100000 bytes in, 8 KiB head and 8 KiB tail kept.
        assert!(
            out.contains("[... 83616 bytes omitted ...]"),
            "{}",
            out.len()
        );
        assert!(out.len() < MAX_OUTPUT_BYTES + 200);
    }

    #[test]
    fn a_flood_of_output_does_not_exhaust_memory_or_hang() {
        let dir = Scratch::new("bash-flood");
        let tool = bash(dir.path(), Duration::from_secs(2), true);
        let out = run(&tool, "yes");
        assert!(
            out.contains("[timed out after 2s and was killed]"),
            "{}",
            out.len()
        );
        assert!(out.len() < MAX_OUTPUT_BYTES + 200);
    }

    #[test]
    fn timeout_kills_the_command_and_reports_it() {
        let dir = Scratch::new("bash-timeout");
        let tool = bash(dir.path(), Duration::from_secs(1), true);
        let started = Instant::now();
        let out = run(&tool, "echo before; sleep 30");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(out, "before\n[timed out after 1s and was killed]");
    }

    #[test]
    fn the_timeout_argument_raises_the_limit_and_its_range_is_checked() {
        let dir = Scratch::new("bash-timeout-arg");
        let tool = bash(dir.path(), Duration::from_secs(1), true);
        let out = tool
            .run(&serde_json::json!({"command": "sleep 2; echo done", "timeout": 20}).to_string());
        assert_eq!(out, "done\n[exit code: 0]");
        assert_eq!(
            tool.run(&serde_json::json!({"command": "true", "timeout": 0}).to_string()),
            "error: timeout must be at least 1 second and at most 300"
        );
        assert_eq!(
            tool.run(&serde_json::json!({"command": "true", "timeout": 301}).to_string()),
            "error: timeout is at most 300 seconds; for a longer build use background: true"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn timeout_leaves_no_process_running() {
        let dir = Scratch::new("bash-orphan");
        let tool = bash(dir.path(), Duration::from_secs(1), true);
        // Start a background child, record its pid, then wait forever.
        let started = Instant::now();
        run(&tool, "sleep 30 & echo $! > pid; wait");
        // If the group were not killed, the call would block until the
        // background child ends by itself, and the process check below would
        // pass for the wrong reason.
        assert!(started.elapsed() < Duration::from_secs(10));
        let pid = std::fs::read_to_string(dir.path().join("pid"))
            .unwrap()
            .trim()
            .to_owned();
        // Give the kernel a moment to finish tearing the process down.
        let gone = (0..50).any(|_| {
            thread::sleep(Duration::from_millis(100));
            !Path::new(&format!("/proc/{pid}")).exists()
        });
        assert!(gone, "process {pid} is still running");
    }

    #[test]
    fn a_cancel_kills_a_running_command_and_its_children_at_once() {
        let dir = Scratch::new("bash-cancel");
        let cancel = Cancel::new();
        let tool = bash(dir.path(), DEFAULT_TIMEOUT, true).with_cancel(cancel.clone());
        let flag = cancel.clone();
        let stopper = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            flag.cancel();
        });
        let started = Instant::now();
        let out = run(&tool, "sleep 30 & sleep 30; echo not reached");
        stopper.join().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(out, "[cancelled by the user and killed]");
    }

    #[test]
    fn a_command_that_waits_for_input_ends_instead_of_hanging() {
        let dir = Scratch::new("bash-stdin");
        let out = run(&bash(dir.path(), Duration::from_secs(5), true), "cat");
        assert_eq!(out, "[exit code: 0]");
    }

    #[test]
    fn leftover_background_jobs_do_not_hang_the_call() {
        let dir = Scratch::new("bash-bg");
        let started = Instant::now();
        let out = run(&bash(dir.path(), DEFAULT_TIMEOUT, true), "sleep 30 &");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(out, "[exit code: 0]");
    }

    #[test]
    fn bad_arguments_return_error_text_without_asking() {
        let dir = Scratch::new("bash-badargs");
        let tool = bash(dir.path(), DEFAULT_TIMEOUT, true);
        assert!(tool.run("{}").starts_with("error: invalid arguments"));
    }
}

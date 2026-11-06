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
use super::{Approver, Tool};

/// How long a command may run before it is killed.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

const SCHEMA: &str = r#"{"type":"object","properties":{"command":{"type":"string","description":"Shell command to run with bash -c"}},"required":["command"]}"#;

#[derive(Deserialize)]
struct Args {
    command: String,
}

pub struct Bash {
    spec: ToolSpec,
    cwd: PathBuf,
    timeout: Duration,
    // Rust note: `Box<dyn Approver>` holds any type that implements `Approver`,
    // decided when the program runs: a terminal prompt in the REPL, a scripted
    // answer in tests.
    approver: Box<dyn Approver>,
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
        }
    }

    fn execute(&self, command: &str) -> String {
        // `exec 2>&1` first makes the shell send errors to the same pipe as
        // normal output, so the two come back interleaved in the order they
        // were written. `process_group(0)` puts the command in its own process
        // group so we can kill all of it at once. Stdin is closed so a command
        // that waits for input ends instead of hanging.
        let mut child = match Command::new("bash")
            .arg("-c")
            .arg(format!("exec 2>&1\n{command}"))
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
        {
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
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if started.elapsed() >= self.timeout => {
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
        let summary = if timed_out {
            format!("[timed out after {:?} and was killed]", self.timeout)
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
fn kill_group(group: u32) {
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
        if !self.approver.approve(&args.command) {
            return "denied by user: the command was not run".to_owned();
        }
        self.execute(&args.command)
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
        fn approve(&self, _command: &str) -> bool {
            self.0
        }
    }

    fn bash(dir: &Path, timeout: Duration, approve: bool) -> Bash {
        Bash::new(dir, timeout, Box::new(Fixed(approve)))
    }

    fn run(tool: &Bash, command: &str) -> String {
        tool.run(&serde_json::json!({ "command": command }).to_string())
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
            fn approve(&self, command: &str) -> bool {
                self.0.lock().unwrap().push(command.to_owned());
                false
            }
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let dir = Scratch::new("bash-recorder");
        let tool = Bash::new(
            dir.path(),
            DEFAULT_TIMEOUT,
            Box::new(Recorder(Arc::clone(&seen))),
        );
        run(&tool, "echo  hi   there");
        assert_eq!(*seen.lock().unwrap(), vec!["echo  hi   there".to_owned()]);
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

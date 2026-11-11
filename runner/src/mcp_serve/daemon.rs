//! `--mcp-serve -d` and `--mcp-stop` (issue 52): the HTTP server of `http.rs` started in the
//! background, and stopped again. Three files in the log folder, named by the port:
//!
//! - `mcp-serve-PORT.json`: pid, port, mode, project and start time of the running server, written
//!   by the server itself once it listens;
//! - `mcp-serve-PORT.log`: what the detached server printed to its standard output and error;
//! - `mcp-token`: the token every request must carry (see `http.rs`).
//!
//! Unix only: the child gets a process group of its own so Ctrl-C or closing the terminal does not
//! reach it, and it is stopped with the `kill` command (no libc dependency).

use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// How long the parent waits for the child to listen.
const START_WAIT: Duration = Duration::from_secs(3);

pub fn state_path(dir: &Path, port: u16) -> PathBuf {
    dir.join(format!("mcp-serve-{port}.json"))
}

pub fn log_path(dir: &Path, port: u16) -> PathBuf {
    dir.join(format!("mcp-serve-{port}.log"))
}

/// Notes that this process is the server on `port`.
pub fn write_state(dir: &Path, port: u16, mode: &str, project: &str) -> Result<(), String> {
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let state = json!({
        "pid": std::process::id(),
        "port": port,
        "mode": mode,
        "project": project,
        "started": started,
    });
    let path = state_path(dir, port);
    std::fs::write(&path, format!("{state:#}\n"))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// The pid noted for `port`, if the file is there and says one.
fn noted_pid(dir: &Path, port: u16) -> Option<u32> {
    let text = std::fs::read_to_string(state_path(dir, port)).ok()?;
    let state: Value = serde_json::from_str(&text).ok()?;
    // Pid 0 would make `kill` signal this command's own process group.
    u32::try_from(state.get("pid")?.as_u64()?)
        .ok()
        .filter(|pid| *pid > 0)
}

/// Whether `pid` is the detached `--mcp-serve` on `port`. Where `/proc` can say what the process
/// was started with it is asked (a reused pid, or a harness's stdio server, is not ours: the
/// child always ends with `--http --port PORT`); elsewhere being alive counts.
fn is_ours(pid: u32, port: u16) -> bool {
    let alive = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !alive {
        return false;
    }
    match std::fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(cmdline) => {
            let cmdline = String::from_utf8_lossy(&cmdline);
            cmdline.contains("--mcp-serve") && cmdline.contains(&format!("--port\0{port}"))
        }
        Err(_) => true,
    }
}

/// Stops the detached server on `port`. Says what it did; an error when nothing was running (a
/// stale note is removed all the same).
pub fn stop(dir: &Path, port: u16) -> Result<String, String> {
    let state = state_path(dir, port);
    let Some(pid) = noted_pid(dir, port) else {
        return Err(format!("no detached MCP server is noted for port {port}"));
    };
    if !is_ours(pid, port) {
        let _ = std::fs::remove_file(&state);
        return Err(format!(
            "pid {pid}, noted for port {port}, is not running: removed the stale note"
        ));
    }
    let sent = Command::new("kill")
        .arg(pid.to_string())
        .status()
        .is_ok_and(|s| s.success());
    if !sent {
        return Err(format!("cannot stop pid {pid} (kill failed)"));
    }
    let until = Instant::now() + START_WAIT;
    while is_ours(pid, port) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = std::fs::remove_file(&state);
    Ok(format!("mcp: stopped pid {pid} (port {port})"))
}

/// The arguments for the detached child: the parent's own, without `-d`/`--detach`, `--http` and
/// `--port N`, and with `--http --port PORT` at the end.
pub fn child_args(args: &[String], port: u16) -> Vec<String> {
    let mut out = Vec::new();
    let mut args = args.iter().peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-d" | "--detach" | "--http" => {}
            "--port" => {
                args.next();
            }
            _ => out.push(arg.clone()),
        }
    }
    out.extend(["--http".to_owned(), "--port".to_owned(), port.to_string()]);
    out
}

/// Starts this program again in the background with `args` (see [`child_args`]), standard streams
/// in the log file, and waits until it listens on `port`. Returns the child's pid. The caller has
/// already checked that the port is free and that the database and mode are usable.
pub fn spawn(dir: &Path, args: &[String], port: u16) -> Result<u32, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this program: {e}"))?;
    let log = log_path(dir, port);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|e| format!("cannot open {}: {e}", log.display()))?;
    let again = file
        .try_clone()
        .map_err(|e| format!("cannot open {}: {e}", log.display()))?;
    let mut command = Command::new(exe);
    command
        .args(child_args(args, port))
        .stdin(Stdio::null())
        .stdout(file)
        .stderr(again);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    #[cfg(not(unix))]
    return Err(
        "-d works on Unix only: use --http and your own way of running it in the background"
            .to_owned(),
    );
    #[cfg(unix)]
    {
        let mut child = command
            .spawn()
            .map_err(|e| format!("cannot start the server: {e}"))?;
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let until = Instant::now() + START_WAIT;
        while Instant::now() < until {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!(
                    "the server exited at once ({status}): see {}",
                    log.display()
                ));
            }
            if TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok() {
                return Ok(child.id());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(format!(
            "the server did not listen on port {port} within {} seconds: see {}",
            START_WAIT.as_secs(),
            log.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_child_gets_the_same_arguments_run_in_the_foreground_on_the_port() {
        assert_eq!(
            child_args(&words(&["--mcp-serve", "-d"]), 8788),
            words(&["--mcp-serve", "--http", "--port", "8788"])
        );
        assert_eq!(
            child_args(
                &words(&[
                    "--mcp-serve",
                    "--direct",
                    "--detach",
                    "--port",
                    "9",
                    "--http"
                ]),
                9
            ),
            words(&["--mcp-serve", "--direct", "--http", "--port", "9"])
        );
        assert_eq!(
            child_args(&words(&["--mcp-serve", "--decider", "jev:x", "-d"]), 1),
            words(&["--mcp-serve", "--decider", "jev:x", "--http", "--port", "1"])
        );
    }

    #[test]
    fn stopping_with_no_note_or_a_dead_pid_says_so_and_clears_the_note() {
        let dir = crate::tools::testutil::Scratch::new("daemon-stop");
        let err = stop(dir.path(), 8788).unwrap_err();
        assert!(err.contains("no detached MCP server"), "{err}");
        // A pid that is not running: take one from a child that has ended.
        let mut done = Command::new("true").spawn().unwrap();
        let pid = done.id();
        done.wait().unwrap();
        std::fs::write(state_path(dir.path(), 8788), format!("{{\"pid\": {pid}}}")).unwrap();
        let err = stop(dir.path(), 8788).unwrap_err();
        assert!(err.contains("not running"), "{err}");
        assert!(!state_path(dir.path(), 8788).exists());
    }

    #[test]
    fn a_note_with_pid_zero_is_not_a_pid() {
        let dir = crate::tools::testutil::Scratch::new("daemon-zero");
        std::fs::write(state_path(dir.path(), 8788), "{\"pid\": 0}").unwrap();
        assert_eq!(noted_pid(dir.path(), 8788), None);
        assert!(stop(dir.path(), 8788).unwrap_err().contains("no detached"));
    }

    #[test]
    fn a_live_process_that_is_not_the_server_on_that_port_is_not_ours() {
        // This test process is alive but was not started with --mcp-serve --port.
        assert!(!is_ours(std::process::id(), 8788) || !std::path::Path::new("/proc/self").exists());
    }

    #[test]
    fn the_state_note_holds_the_pid_and_what_the_server_is() {
        let dir = crate::tools::testutil::Scratch::new("daemon-state");
        write_state(dir.path(), 8788, "buffer", "/work/app").unwrap();
        assert_eq!(noted_pid(dir.path(), 8788), Some(std::process::id()));
        let text = std::fs::read_to_string(state_path(dir.path(), 8788)).unwrap();
        assert!(
            text.contains("\"mode\": \"buffer\"") && text.contains("/work/app"),
            "{text}"
        );
    }
}

//! A client for MCP servers over standard input and output (issue 44).
//!
//! A server is a program the chat starts. The two talk in newline-delimited JSON-RPC 2.0: this
//! side sends `initialize`, `notifications/initialized`, `tools/list` and `tools/call`, and
//! answers the few requests a server may make (`ping`; anything else is "method not found").
//! Nothing here is asynchronous: a call waits on a channel that a reader thread feeds, polling
//! the [`Cancel`] flag and a deadline, the same way `bash` waits for its child.
//!
//! The transport is a pair of ends ([`Transport`]) so the protocol is tested without a process;
//! [`McpClient::start`] builds the real one. The model never sees any of this: it sees tools
//! named `mcp__{server}__{tool}` (see `tools/mcp.rs`).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::cancel::Cancel;
use crate::store::Store;
use crate::store_mcp::{McpServer, Rule, rule_of};

/// The protocol version this program asks for.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// The versions it accepts when the server answers with another one.
pub(crate) const ACCEPTED_VERSIONS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];
/// How long a server gets to start, answer `initialize` and list all its tools.
pub const START_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a server gets to exit by itself after its input is closed.
const EXIT_GRACE: Duration = Duration::from_secs(1);
/// The longest line a server may write, in bytes.
const MAX_LINE_BYTES: u64 = 8 * 1024 * 1024;
/// The most pages of `tools/list` that are followed.
const MAX_TOOL_PAGES: usize = 10;
/// How often a waiting call looks at the cancel flag.
const POLL: Duration = Duration::from_millis(25);
/// How many of the server's last error lines are kept for a message.
const STDERR_LINES: usize = 20;
/// The longest a tool name sent to the model may be (what the routers accept).
const MAX_TOOL_NAME: usize = 64;

/// One tool as the server listed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// The `inputSchema` as JSON text.
    pub schema: String,
}

impl ToolDef {
    /// A rough count of the tokens this tool costs on every model call while it is on: its
    /// name, description and argument schema as the model gets them. Only an estimate.
    pub fn tokens(&self, server: &str) -> usize {
        let name = exposed_name(server, &self.name).unwrap_or_else(|| self.name.clone());
        let chars = name.chars().count()
            + "[mcp ] ".len()
            + server.chars().count()
            + self.description.chars().count()
            + self.schema.chars().count();
        crate::store_prompts::estimated_tokens(chars)
    }
}

/// The two ends of the conversation: a writer for lines to the server, and the lines it wrote
/// (`Err` is a framing problem; a closed channel is the end of its output).
pub struct Transport {
    writer: Option<Box<dyn Write>>,
    lines: Receiver<Result<String, String>>,
}

impl Transport {
    pub fn new(writer: Box<dyn Write>, lines: Receiver<Result<String, String>>) -> Self {
        Self {
            writer: Some(writer),
            lines,
        }
    }

    /// A transport over a writer and a reader, which a thread reads line by line.
    pub fn over(writer: impl Write + 'static, reader: impl Read + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || read_lines(reader, &tx));
        Self::new(Box::new(writer), rx)
    }
}

/// Sends each line the server writes. A line over [`MAX_LINE_BYTES`] ends the conversation.
fn read_lines(reader: impl Read, tx: &mpsc::Sender<Result<String, String>>) {
    let mut reader = BufReader::new(reader);
    loop {
        let mut buf = Vec::new();
        let read = reader
            .by_ref()
            .take(MAX_LINE_BYTES + 1)
            .read_until(b'\n', &mut buf);
        match read {
            Ok(0) => return,
            Ok(_) => {}
            Err(e) => {
                let _ = tx.send(Err(e.to_string()));
                return;
            }
        }
        if buf.last() != Some(&b'\n') && buf.len() as u64 > MAX_LINE_BYTES {
            let _ = tx.send(Err(format!("it wrote a line over {MAX_LINE_BYTES} bytes")));
            return;
        }
        let text = String::from_utf8_lossy(&buf);
        let text = text.trim_end_matches(['\r', '\n']);
        if text.trim().is_empty() {
            continue;
        }
        if tx.send(Ok(text.to_owned())).is_err() {
            return;
        }
    }
}

/// Runs the server's program in `cwd` with its pipes and reader threads; no hello yet.
fn spawn(server: &McpServer, cwd: &Path) -> Result<(Transport, Process), String> {
    let mut command = Command::new(&server.command);
    command
        .args(&server.args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    crate::tools::scrub(&mut command);
    for (name, value) in &server.vars {
        command.env(name, value);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not run `{}`: {e}", server.command))?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        let _ = child.kill();
        return Err("could not open its pipes".to_owned());
    };
    let tail = Arc::new(Mutex::new(VecDeque::new()));
    let keep = Arc::clone(&tail);
    let finished = Arc::new(AtomicBool::new(false));
    let done = Arc::clone(&finished);
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let mut tail = keep.lock().unwrap_or_else(|e| e.into_inner());
            if tail.len() == STDERR_LINES {
                tail.pop_front();
            }
            tail.push_back(line);
        }
        done.store(true, Ordering::Release);
    });
    let group = child.id();
    Ok((
        Transport::over(stdin, stdout),
        Process {
            child,
            group,
            stderr: tail,
            stderr_done: finished,
        },
    ))
}

/// Why a request did not give a result.
#[derive(Debug, PartialEq, Eq)]
enum Failure {
    Timeout,
    Cancelled,
    Stopped(String),
    Rpc(String),
}

/// The process behind a real client.
struct Process {
    child: Child,
    group: u32,
    stderr: Arc<Mutex<VecDeque<String>>>,
    /// Set by the reader thread once the server's stderr has ended.
    stderr_done: Arc<AtomicBool>,
}

impl Process {
    /// Waits a moment for the stderr reader to take in everything the server wrote before it
    /// stopped; without this a slow machine can report an exit with nothing it wrote.
    fn settle_stderr(&self) {
        let until = Instant::now() + Duration::from_millis(300);
        while !self.stderr_done.load(Ordering::Acquire) && Instant::now() < until {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

struct Inner {
    transport: Transport,
    next_id: u64,
    /// Set once the server has gone away (or was shut down): why.
    stopped: Option<String>,
    process: Option<Process>,
}

/// A started server.
pub struct McpClient {
    name: String,
    /// How long one `tools/call` may take.
    timeout: Duration,
    cancel: Cancel,
    inner: RefCell<Inner>,
    /// What it takes to start the program again (`None` for a client over a test transport).
    launch: Option<(McpServer, PathBuf)>,
}

impl McpClient {
    /// A client over a transport. `timeout` is how long a call may take.
    pub fn over(name: &str, transport: Transport, timeout: Duration, cancel: Cancel) -> Self {
        Self {
            name: name.to_owned(),
            timeout,
            cancel,
            inner: RefCell::new(Inner {
                transport,
                next_id: 1,
                stopped: None,
                process: None,
            }),
            launch: None,
        }
    }

    /// Starts the server's program in `cwd` and shakes hands with it. The program's environment
    /// is passed on minus the secret-looking names, plus the owner's own variables for this
    /// server. Returns the client and the tools the server listed, or why it did not start.
    pub fn start(
        server: &McpServer,
        cwd: &Path,
        cancel: Cancel,
    ) -> Result<(Rc<McpClient>, Vec<ToolDef>), String> {
        let (transport, process) = spawn(server, cwd)?;
        let mut client = McpClient::over(
            &server.name,
            transport,
            Duration::from_secs(server.timeout_secs),
            cancel,
        );
        client.launch = Some((server.clone(), cwd.to_path_buf()));
        client.inner.borrow_mut().process = Some(process);
        match client.handshake(START_TIMEOUT) {
            Ok(tools) => Ok((Rc::new(client), tools)),
            Err(why) => Err(client.failed_start(why)),
        }
    }

    /// Why a start failed, with what the program wrote; the program is stopped.
    fn failed_start(&self, why: String) -> String {
        let tail = self.stderr_tail();
        self.shutdown();
        let text = if tail.is_empty() {
            why
        } else {
            format!("{why} (it wrote: {tail})")
        };
        self.inner.borrow_mut().stopped = Some(format!("it did not start again: {text}"));
        text
    }

    /// Why the server is not running, or `None` while it is. A program that died while no call
    /// was waiting is found here too.
    pub fn stopped(&self) -> Option<String> {
        let mut inner = self.inner.borrow_mut();
        if let Some(why) = &inner.stopped {
            return Some(why.clone());
        }
        let exited = inner
            .process
            .as_mut()
            .is_some_and(|p| matches!(p.child.try_wait(), Ok(Some(_))));
        if !exited {
            return None;
        }
        let why = self.exit_reason(&mut inner);
        inner.stopped = Some(why.clone());
        Some(why)
    }

    /// Starts the program again, in place: the tools the model already has keep working.
    /// Returns the tools the server lists now. Fails when the client was not started from a
    /// saved server, or the program does not start (it then stays stopped).
    pub fn restart(&self) -> Result<Vec<ToolDef>, String> {
        let Some((server, cwd)) = &self.launch else {
            return Err("it cannot be started again".to_owned());
        };
        self.shutdown();
        let (transport, process) = spawn(server, cwd)?;
        {
            let mut inner = self.inner.borrow_mut();
            inner.transport = transport;
            inner.next_id = 1;
            inner.stopped = None;
            inner.process = Some(process);
        }
        self.handshake(START_TIMEOUT)
            .map_err(|why| self.failed_start(why))
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// `initialize`, `notifications/initialized`, then every page of `tools/list`, all within
    /// `within`.
    pub fn handshake(&self, within: Duration) -> Result<Vec<ToolDef>, String> {
        let deadline = Instant::now() + within;
        let left = || deadline.saturating_duration_since(Instant::now());
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "aharness", "version": env!("CARGO_PKG_VERSION")},
                }),
                left(),
            )
            .map_err(|f| self.describe_start(&f, within))?;
        let version = result["protocolVersion"].as_str().unwrap_or("");
        if !ACCEPTED_VERSIONS.contains(&version) {
            return Err(format!(
                "it speaks protocol version '{version}', which this program does not"
            ));
        }
        self.notify("notifications/initialized", &json!({}));
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_TOOL_PAGES {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let page = self
                .request("tools/list", params, left())
                .map_err(|f| self.describe_start(&f, within))?;
            for tool in page["tools"].as_array().into_iter().flatten() {
                if let Some(def) = tool_def(tool) {
                    tools.push(def);
                }
            }
            match page["nextCursor"].as_str() {
                Some(next) if !next.is_empty() => cursor = Some(next.to_owned()),
                _ => return Ok(tools),
            }
        }
        Ok(tools)
    }

    /// Calls a tool with `arguments` (JSON text). The text for the model: the result's text, or
    /// `error: ...` when the server said so, did not answer, or has stopped.
    pub fn call(&self, tool: &str, arguments: &str) -> String {
        let arguments: Value = if arguments.trim().is_empty() {
            json!({})
        } else {
            match serde_json::from_str(arguments) {
                Ok(v @ Value::Object(_)) => v,
                Ok(_) => return "error: invalid arguments: they must be a JSON object".to_owned(),
                Err(e) => return format!("error: invalid arguments: {e}"),
            }
        };
        match self.request(
            "tools/call",
            json!({"name": tool, "arguments": arguments}),
            self.timeout,
        ) {
            Ok(result) => flatten_result(&result),
            Err(Failure::Timeout) => format!(
                "error: mcp {} did not answer in {} seconds",
                self.name,
                self.timeout.as_secs()
            ),
            Err(Failure::Cancelled) => format!("error: mcp {}: cancelled", self.name),
            Err(Failure::Stopped(why)) => {
                let again = if self.launch.is_some() {
                    format!(
                        "; the owner can start it again with /mcp restart {}",
                        self.name
                    )
                } else {
                    String::new()
                };
                format!("error: mcp {} has stopped: {why}{again}", self.name)
            }
            Err(Failure::Rpc(message)) => format!("error: mcp {}: {message}", self.name),
        }
    }

    /// Closes the server's input, gives it a second to exit, then kills its whole process group.
    /// Safe to call again.
    pub fn shutdown(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.transport.writer = None;
        inner
            .stopped
            .get_or_insert_with(|| "it was shut down".to_owned());
        let Some(mut process) = inner.process.take() else {
            return;
        };
        let until = Instant::now() + EXIT_GRACE;
        while Instant::now() < until {
            if matches!(process.child.try_wait(), Ok(Some(_))) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        crate::tools::kill_group(process.group);
        let _ = process.child.kill();
        let _ = process.child.wait();
    }

    /// The server's last few error lines, on one line.
    fn stderr_tail(&self) -> String {
        let inner = self.inner.borrow();
        let Some(process) = &inner.process else {
            return String::new();
        };
        process.settle_stderr();
        let tail = process.stderr.lock().unwrap_or_else(|e| e.into_inner());
        let text = tail
            .iter()
            .rev()
            .take(3)
            .rev()
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join(" | ");
        text.chars().take(300).collect()
    }

    fn describe_start(&self, failure: &Failure, within: Duration) -> String {
        match failure {
            Failure::Timeout => format!("no answer in {} seconds", within.as_secs()),
            Failure::Cancelled => "cancelled".to_owned(),
            Failure::Stopped(why) => why.clone(),
            Failure::Rpc(message) => message.clone(),
        }
    }

    fn notify(&self, method: &str, params: &Value) {
        let mut inner = self.inner.borrow_mut();
        let _ = send(
            &mut inner,
            &json!({"jsonrpc": "2.0", "method": method, "params": params}),
        );
    }

    /// Sends a request and waits for its answer, answering what the server asks meanwhile.
    fn request(&self, method: &str, params: Value, wait: Duration) -> Result<Value, Failure> {
        let mut inner = self.inner.borrow_mut();
        if let Some(why) = &inner.stopped {
            return Err(Failure::Stopped(why.clone()));
        }
        let id = inner.next_id;
        inner.next_id += 1;
        send(
            &mut inner,
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )?;
        let deadline = Instant::now() + wait;
        loop {
            if self.cancel.is_cancelled() {
                abandon(&mut inner, id, "cancelled");
                return Err(Failure::Cancelled);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                abandon(&mut inner, id, "timed out");
                return Err(Failure::Timeout);
            }
            match inner.transport.lines.recv_timeout(left.min(POLL)) {
                Ok(Ok(line)) => {
                    if let Some(answer) = self.handle(&mut inner, &line, id) {
                        return answer;
                    }
                }
                Ok(Err(why)) => return Err(self.stop(&mut inner, why)),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    let why = self.exit_reason(&mut inner);
                    return Err(self.stop(&mut inner, why));
                }
            }
        }
    }

    /// One line from the server: the answer to request `id` (`Some`), or something to answer or
    /// ignore (`None`).
    fn handle(&self, inner: &mut Inner, line: &str, id: u64) -> Option<Result<Value, Failure>> {
        let message: Value = serde_json::from_str(line).ok()?;
        if let Some(method) = message["method"].as_str() {
            // A request of the server's has an id; a notification has none and gets no answer.
            if let Some(their_id) = message.get("id") {
                let reply = if method == "ping" {
                    json!({"jsonrpc": "2.0", "id": their_id, "result": {}})
                } else {
                    json!({
                        "jsonrpc": "2.0",
                        "id": their_id,
                        "error": {"code": -32601, "message": "method not found"},
                    })
                };
                let _ = send(inner, &reply);
            }
            return None;
        }
        if message["id"].as_u64() != Some(id) {
            // The late answer to a request that was given up on.
            return None;
        }
        if let Some(error) = message.get("error") {
            let text = error["message"].as_str().unwrap_or("unknown error");
            return Some(Err(Failure::Rpc(text.to_owned())));
        }
        Some(Ok(message.get("result").cloned().unwrap_or(Value::Null)))
    }

    fn stop(&self, inner: &mut Inner, why: String) -> Failure {
        inner.stopped = Some(why.clone());
        Failure::Stopped(why)
    }

    /// What is known about why the server's output ended.
    fn exit_reason(&self, inner: &mut Inner) -> String {
        let mut text = String::from("its output ended");
        if let Some(process) = &mut inner.process {
            // The output closes a moment before the process can be reaped.
            let until = Instant::now() + Duration::from_millis(300);
            loop {
                if let Ok(Some(status)) = process.child.try_wait() {
                    text = format!("it exited ({status})");
                    break;
                }
                if Instant::now() >= until {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            process.settle_stderr();
            let tail = process.stderr.lock().unwrap_or_else(|e| e.into_inner());
            let last: Vec<&str> = tail.iter().rev().take(3).rev().map(|l| l.trim()).collect();
            if !last.is_empty() {
                let joined: String = last.join(" | ").chars().take(300).collect();
                text = format!("{text}; it wrote: {joined}");
            }
        }
        text
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Writes one JSON message and a newline.
fn send(inner: &mut Inner, message: &Value) -> Result<(), Failure> {
    let Some(writer) = inner.transport.writer.as_mut() else {
        return Err(Failure::Stopped("it was shut down".to_owned()));
    };
    let line = format!("{message}\n");
    if let Err(e) = writer
        .write_all(line.as_bytes())
        .and_then(|()| writer.flush())
    {
        let why = format!("it does not read its input any more ({e})");
        inner.stopped = Some(why.clone());
        return Err(Failure::Stopped(why));
    }
    Ok(())
}

/// Tells the server a request is no longer wanted. Its late answer, if any, is ignored by id.
fn abandon(inner: &mut Inner, id: u64, reason: &str) {
    let _ = send(
        inner,
        &json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": id, "reason": reason},
        }),
    );
}

/// A tool of a `tools/list` page, or `None` when it has no name.
fn tool_def(tool: &Value) -> Option<ToolDef> {
    let name = tool["name"].as_str()?.trim();
    if name.is_empty() {
        return None;
    }
    let mut schema = match tool.get("inputSchema") {
        Some(Value::Object(map)) => map.clone(),
        _ => serde_json::Map::new(),
    };
    // The draft marker means nothing to the model and some routers refuse it.
    schema.remove("$schema");
    schema
        .entry("type")
        .or_insert_with(|| Value::String("object".to_owned()));
    Some(ToolDef {
        name: name.to_owned(),
        description: tool["description"].as_str().unwrap_or("").trim().to_owned(),
        schema: Value::Object(schema).to_string(),
    })
}

/// The text of a `tools/call` result for the model.
fn flatten_result(result: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in result["content"].as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("text") => parts.push(block["text"].as_str().unwrap_or("").to_owned()),
            Some("image") => parts.push("[image omitted]".to_owned()),
            Some("audio") => parts.push("[audio omitted]".to_owned()),
            Some(_) => parts.push("[resource omitted]".to_owned()),
            None => {}
        }
    }
    if parts.is_empty() {
        match result.get("structuredContent") {
            Some(v) if !v.is_null() => parts.push(v.to_string()),
            _ => parts.push("(no output)".to_owned()),
        }
    }
    let text = parts.join("\n\n");
    if result["isError"].as_bool() == Some(true) {
        format!("error: {text}")
    } else {
        text
    }
}

/// The name the model sees for a server's tool, or `None` when it would be too long.
/// Characters a router would refuse become `_`.
pub fn exposed_name(server: &str, tool: &str) -> Option<String> {
    let clean: String = tool
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = format!("mcp__{server}__{clean}");
    (name.len() <= MAX_TOOL_NAME).then_some(name)
}

/// A server that started, with what the owner said about its tools.
pub struct Started {
    pub server: McpServer,
    pub rules: Vec<(String, Rule)>,
    pub client: Rc<McpClient>,
    pub tools: Vec<ToolDef>,
}

impl Started {
    /// How many tools the model is given (the ones not turned off) and about how many tokens
    /// their definitions add to every model call.
    pub fn on_cost(&self) -> (usize, usize) {
        self.tools
            .iter()
            .filter(|t| rule_of(&self.server, &self.rules, &t.name) != Rule::Off)
            .fold((0, 0), |(n, tokens), t| {
                (n + 1, tokens + t.tokens(&self.server.name))
            })
    }
}

/// The servers a chat started, and the lines to show about the ones that did not.
#[derive(Default)]
pub struct McpServers {
    pub started: Vec<Started>,
    pub notes: Vec<String>,
}

impl McpServers {
    /// No servers (the Tools page, tests).
    pub fn none() -> Self {
        Self::default()
    }

    /// Starts every enabled server, one after another, in `cwd`. A server that does not start is
    /// reported in `notes` and the others go on.
    pub fn start_all(store: &Store, cwd: &Path, cancel: &Cancel) -> Self {
        let mut out = Self::none();
        let servers = match store.list_mcp_servers() {
            Ok(servers) => servers,
            Err(why) => {
                out.notes
                    .push(format!("[mcp: could not read the servers: {why}]"));
                return out;
            }
        };
        for server in servers.into_iter().filter(|s| s.enabled) {
            let rules = store.mcp_rules(server.id).unwrap_or_default();
            match McpClient::start(&server, cwd, cancel.clone()) {
                Ok((client, tools)) => out.started.push(Started {
                    server,
                    rules,
                    client,
                    tools,
                }),
                Err(why) => out
                    .notes
                    .push(format!("[mcp: {} did not start: {why}]", server.name)),
            }
        }
        out
    }

    /// The clients of the servers that started, by name, for `/mcp restart`.
    pub fn clients(&self) -> Vec<(String, Rc<McpClient>)> {
        self.started
            .iter()
            .map(|s| (s.server.name.clone(), Rc::clone(&s.client)))
            .collect()
    }

    /// The names of the servers that started.
    pub fn names(&self) -> Vec<&str> {
        self.started
            .iter()
            .map(|s| s.server.name.as_str())
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Writes whole lines into a channel, the way a pipe to the server would.
    struct LineWriter {
        pending: Vec<u8>,
        to_server: mpsc::Sender<String>,
    }

    impl Write for LineWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.pending.extend_from_slice(buf);
            while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=end).collect();
                let text = String::from_utf8_lossy(&line).trim().to_owned();
                self.to_server
                    .send(text)
                    .map_err(|_| std::io::Error::other("closed"))?;
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A server on a thread: `answer` gets each message and returns the lines to write back.
    /// Everything it was sent is kept in `seen`.
    pub(crate) fn serve(
        answer: impl Fn(&Value) -> Vec<Value> + Send + 'static,
    ) -> (Transport, Arc<Mutex<Vec<Value>>>) {
        let (to_server, from_client) = mpsc::channel::<String>();
        let (to_client, from_server) = mpsc::channel::<Result<String, String>>();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let keep = Arc::clone(&seen);
        thread::spawn(move || {
            for line in from_client {
                let message: Value = serde_json::from_str(&line).unwrap();
                keep.lock().unwrap().push(message.clone());
                for out in answer(&message) {
                    if to_client.send(Ok(out.to_string())).is_err() {
                        return;
                    }
                }
            }
        });
        let writer = LineWriter {
            pending: Vec::new(),
            to_server,
        };
        (Transport::new(Box::new(writer), from_server), seen)
    }

    pub(crate) fn reply(message: &Value, result: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": message["id"], "result": result})
    }

    /// A well-behaved server with two tools.
    pub(crate) fn good(message: &Value) -> Vec<Value> {
        match message["method"].as_str() {
            Some("initialize") => vec![reply(
                message,
                json!({"protocolVersion": "2025-03-26", "capabilities": {}, "serverInfo": {"name": "t"}}),
            )],
            Some("tools/list") => vec![reply(
                message,
                json!({"tools": [
                    {"name": "echo", "description": "Echoes", "inputSchema": {"$schema": "x", "type": "object", "properties": {"text": {"type": "string"}}}},
                    {"name": "bare"},
                ]}),
            )],
            Some("tools/call") => {
                let name = message["params"]["name"].as_str().unwrap_or("");
                let text = message["params"]["arguments"]["text"]
                    .as_str()
                    .unwrap_or("-");
                vec![reply(
                    message,
                    json!({"content": [{"type": "text", "text": format!("{name}:{text}")}]}),
                )]
            }
            _ => vec![],
        }
    }

    fn client(transport: Transport) -> McpClient {
        McpClient::over("t", transport, Duration::from_millis(400), Cancel::new())
    }

    #[test]
    fn the_handshake_lists_tools_strips_the_draft_marker_and_fills_in_a_bare_tool() {
        let (transport, seen) = serve(good);
        let client = client(transport);
        let tools = client.handshake(Duration::from_secs(2)).unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description, "Echoes");
        let schema: Value = serde_json::from_str(&tools[0].schema).unwrap();
        assert!(schema.get("$schema").is_none());
        assert_eq!(schema["properties"]["text"]["type"], "string");
        assert_eq!(tools[1].description, "");
        assert_eq!(tools[1].schema, r#"{"type":"object"}"#);
        let methods: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|m| m["method"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            methods,
            ["initialize", "notifications/initialized", "tools/list"]
        );
        assert_eq!(
            seen.lock().unwrap()[0]["params"]["protocolVersion"],
            PROTOCOL_VERSION
        );
    }

    #[test]
    fn a_later_page_is_followed_by_its_cursor_and_the_pages_are_limited() {
        let (transport, seen) = serve(|m| match m["method"].as_str() {
            Some("initialize") => good(m),
            Some("tools/list") => match m["params"]["cursor"].as_str() {
                None => vec![reply(
                    m,
                    json!({"tools": [{"name": "a"}], "nextCursor": "p2"}),
                )],
                Some(_) => vec![reply(m, json!({"tools": [{"name": "b"}]}))],
            },
            _ => vec![],
        });
        let tools = client(transport).handshake(Duration::from_secs(2)).unwrap();
        assert_eq!(
            tools.iter().map(|t| &*t.name).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(seen.lock().unwrap().len(), 4);

        // A server that never stops giving a cursor is cut off after the page limit.
        let (transport, seen) = serve(|m| match m["method"].as_str() {
            Some("initialize") => good(m),
            Some("tools/list") => vec![reply(m, json!({"tools": [], "nextCursor": "again"}))],
            _ => vec![],
        });
        let tools = client(transport).handshake(Duration::from_secs(5)).unwrap();
        assert!(tools.is_empty());
        let lists = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m["method"] == "tools/list")
            .count();
        assert_eq!(lists, MAX_TOOL_PAGES);
    }

    #[test]
    fn every_known_protocol_version_is_accepted_and_another_is_refused() {
        for version in ACCEPTED_VERSIONS {
            let (transport, _) = serve(move |m| match m["method"].as_str() {
                Some("initialize") => vec![reply(m, json!({"protocolVersion": version}))],
                Some("tools/list") => vec![reply(m, json!({"tools": []}))],
                _ => vec![],
            });
            assert!(client(transport).handshake(Duration::from_secs(2)).is_ok());
        }
        let (transport, _) = serve(|m| match m["method"].as_str() {
            Some("initialize") => vec![reply(m, json!({"protocolVersion": "1999-01-01"}))],
            _ => vec![],
        });
        let why = client(transport)
            .handshake(Duration::from_secs(2))
            .unwrap_err();
        assert!(why.contains("1999-01-01"), "{why}");
    }

    #[test]
    fn a_call_round_trips_and_its_blocks_are_flattened() {
        let (transport, _) = serve(|m| match m["params"]["name"].as_str() {
            Some("mixed") => vec![reply(
                m,
                json!({"content": [
                    {"type": "text", "text": "one"},
                    {"type": "image", "data": "AAAA", "mimeType": "image/png"},
                    {"type": "text", "text": "two"},
                    {"type": "resource", "resource": {"uri": "file:///x"}},
                ]}),
            )],
            Some("bad") => vec![reply(
                m,
                json!({"content": [{"type": "text", "text": "no such crate"}], "isError": true}),
            )],
            Some("empty") => vec![reply(m, json!({"content": []}))],
            Some("rpc") => vec![json!({
                "jsonrpc": "2.0", "id": m["id"], "error": {"code": -32602, "message": "bad params"}
            })],
            _ => good(m),
        });
        let client = client(transport);
        client.handshake(Duration::from_secs(2)).unwrap();
        assert_eq!(client.call("echo", r#"{"text":"hi"}"#), "echo:hi");
        assert_eq!(client.call("echo", ""), "echo:-");
        assert_eq!(
            client.call("mixed", "{}"),
            "one\n\n[image omitted]\n\ntwo\n\n[resource omitted]"
        );
        assert_eq!(client.call("bad", "{}"), "error: no such crate");
        assert_eq!(client.call("empty", "{}"), "(no output)");
        assert_eq!(client.call("rpc", "{}"), "error: mcp t: bad params");
        assert!(
            client
                .call("echo", "[1]")
                .starts_with("error: invalid arguments")
        );
        assert!(
            client
                .call("echo", "{")
                .starts_with("error: invalid arguments")
        );
    }

    #[test]
    fn a_ping_is_answered_other_requests_refused_and_notifications_and_late_answers_ignored() {
        let (transport, seen) = serve(|m| {
            if m["method"] == "tools/call" {
                vec![
                    json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {}}),
                    json!({"jsonrpc": "2.0", "id": 77, "method": "ping"}),
                    json!({"jsonrpc": "2.0", "id": "s1", "method": "roots/list"}),
                    json!({"jsonrpc": "2.0", "id": 999, "result": {"content": [{"type": "text", "text": "late"}]}}),
                    reply(m, json!({"content": [{"type": "text", "text": "real"}]})),
                ]
            } else {
                good(m)
            }
        });
        let client = client(transport);
        client.handshake(Duration::from_secs(2)).unwrap();
        assert_eq!(client.call("echo", "{}"), "real");
        // The server thread reads our answers after the call has returned.
        thread::sleep(Duration::from_millis(150));
        let seen = seen.lock().unwrap();
        let ping = seen.iter().find(|m| m["id"] == 77).unwrap();
        assert_eq!(ping["result"], json!({}));
        let roots = seen.iter().find(|m| m["id"] == "s1").unwrap();
        assert_eq!(roots["error"]["code"], -32601);
    }

    #[test]
    fn a_server_that_does_not_answer_times_out_and_is_told_the_call_is_given_up() {
        let (transport, seen) = serve(|m| {
            if m["method"] == "tools/call" {
                vec![]
            } else {
                good(m)
            }
        });
        let client = client(transport);
        client.handshake(Duration::from_secs(2)).unwrap();
        let started = Instant::now();
        let text = client.call("echo", "{}");
        assert_eq!(text, "error: mcp t did not answer in 0 seconds");
        assert!(started.elapsed() < Duration::from_secs(2));
        thread::sleep(Duration::from_millis(100));
        let cancelled = seen.lock().unwrap().iter().any(|m| {
            m["method"] == "notifications/cancelled" && m["params"]["reason"] == "timed out"
        });
        assert!(cancelled);
        // It still works for the next call.
        assert!(client.call("echo", "{}").contains("did not answer"));
    }

    #[test]
    fn the_cancel_flag_ends_a_waiting_call_at_once() {
        let (transport, _) = serve(|m| {
            if m["method"] == "tools/call" {
                vec![]
            } else {
                good(m)
            }
        });
        let cancel = Cancel::new();
        let client = McpClient::over("t", transport, Duration::from_secs(30), cancel.clone());
        client.handshake(Duration::from_secs(2)).unwrap();
        let flag = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            flag.cancel();
        });
        let started = Instant::now();
        assert_eq!(client.call("echo", "{}"), "error: mcp t: cancelled");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_server_that_goes_away_is_reported_and_every_later_call_says_it_stopped() {
        let (to_server, _kept) = mpsc::channel::<String>();
        let (tx, rx) = mpsc::channel::<Result<String, String>>();
        drop(tx);
        let writer = LineWriter {
            pending: Vec::new(),
            to_server,
        };
        let closed = McpClient::over(
            "gone",
            Transport::new(Box::new(writer), rx),
            Duration::from_millis(300),
            Cancel::new(),
        );
        let first = closed.call("x", "{}");
        assert!(
            first.starts_with("error: mcp gone has stopped: its output ended"),
            "{first}"
        );
        let second = closed.call("x", "{}");
        assert!(
            second.starts_with("error: mcp gone has stopped"),
            "{second}"
        );
    }

    #[test]
    fn a_server_that_stops_reading_is_reported_as_stopped() {
        let (to_server, from_client) = mpsc::channel::<String>();
        drop(from_client);
        let (_tx, rx) = mpsc::channel::<Result<String, String>>();
        let writer = LineWriter {
            pending: Vec::new(),
            to_server,
        };
        let client = McpClient::over(
            "deaf",
            Transport::new(Box::new(writer), rx),
            Duration::from_millis(300),
            Cancel::new(),
        );
        let text = client.call("x", "{}");
        assert!(
            text.contains("has stopped: it does not read its input any more"),
            "{text}"
        );
    }

    #[test]
    fn a_framing_error_stops_the_server() {
        let (to_server, _kept) = mpsc::channel::<String>();
        let (tx, rx) = mpsc::channel::<Result<String, String>>();
        tx.send(Err("it wrote a line over 8 bytes".to_owned()))
            .unwrap();
        let writer = LineWriter {
            pending: Vec::new(),
            to_server,
        };
        let client = McpClient::over(
            "big",
            Transport::new(Box::new(writer), rx),
            Duration::from_millis(300),
            Cancel::new(),
        );
        let text = client.call("x", "{}");
        assert!(text.contains("has stopped: it wrote a line over"), "{text}");
    }

    #[test]
    fn lines_are_read_whole_blank_ones_skipped_and_an_overlong_one_ends_it() {
        let (tx, rx) = mpsc::channel();
        read_lines(&b"{\"a\":1}\n\n  \r\n{\"b\":2}\r\nlast"[..], &tx);
        let got: Vec<String> = rx.try_iter().map(Result::unwrap).collect();
        assert_eq!(got, [r#"{"a":1}"#, r#"{"b":2}"#, "last"]);

        let (tx, rx) = mpsc::channel();
        let long = vec![b'x'; usize::try_from(MAX_LINE_BYTES).unwrap() + 10];
        read_lines(&long[..], &tx);
        assert!(rx.try_recv().unwrap().is_err());
    }

    #[test]
    fn a_tool_name_for_the_model_is_cleaned_and_limited() {
        assert_eq!(
            exposed_name("rust-docs", "search_items").as_deref(),
            Some("mcp__rust-docs__search_items")
        );
        assert_eq!(
            exposed_name("a", "get.item details").as_deref(),
            Some("mcp__a__get_item_details")
        );
        assert_eq!(exposed_name("a", &"x".repeat(80)), None);
        assert!(exposed_name("a", &"x".repeat(MAX_TOOL_NAME - 8)).is_some());
        assert!(exposed_name("a", &"x".repeat(MAX_TOOL_NAME - 7)).is_none());
    }

    #[test]
    fn a_tool_costs_about_a_quarter_token_a_character_of_what_the_model_gets() {
        let def = ToolDef {
            name: "search".to_owned(),
            description: "d".repeat(400),
            schema: format!("{{\"type\":\"object\",\"x\":\"{}\"}}", "s".repeat(100)),
        };
        let tokens = def.tokens("docs");
        // 400 + 100 + the schema frame, plus the name and the "[mcp docs] " prefix.
        assert!((130..=150).contains(&tokens), "{tokens}");
        let longer = ToolDef {
            description: "d".repeat(800),
            ..def.clone()
        };
        assert!(longer.tokens("docs") > tokens + 90);
    }

    #[test]
    fn shutting_down_twice_is_harmless_and_later_calls_say_so() {
        let (transport, _) = serve(good);
        let client = client(transport);
        client.handshake(Duration::from_secs(2)).unwrap();
        client.shutdown();
        client.shutdown();
        assert_eq!(
            client.call("echo", "{}"),
            "error: mcp t has stopped: it was shut down"
        );
    }
}

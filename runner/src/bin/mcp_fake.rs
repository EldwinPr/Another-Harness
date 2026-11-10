//! Test double: a small MCP server over standard input and output (issue 44).
//!
//! `tests/mcp_stdio.rs` starts this to check the real process handling (pipes, environment,
//! timeouts, shutdown) that the in-memory tests cannot. It is not part of the agent.
//!
//! The first argument picks how it misbehaves: `good` (default), `garbage` (writes lines that are
//! not JSON between its answers), `hang` (never answers a tool call), `exit` (dies on a tool
//! call), `slow` (does not answer `initialize` for 30 seconds), `version` (claims a protocol
//! version nobody speaks).
//!
//! Tools: `echo{text}`, `env{name}` (the value of one environment variable), `pids` (its own
//! process id and that of a `sleep` it started, in its process group), `fail` (an error result),
//! `big` (200 000 characters).

use std::io::{BufRead, Write};
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

fn tool_list() -> Value {
    json!([
        {"name": "echo", "description": "Echoes text",
         "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
        {"name": "env", "description": "One environment variable",
         "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}}},
        {"name": "pids", "description": "Process ids", "inputSchema": {"type": "object"}},
        {"name": "fail", "description": "Always an error", "inputSchema": {"type": "object"}},
        {"name": "big", "description": "A long result", "inputSchema": {"type": "object"}},
    ])
}

fn text(body: &str) -> Value {
    json!({"content": [{"type": "text", "text": body}]})
}

fn call(name: &str, args: &Value) -> Value {
    match name {
        "echo" => text(args["text"].as_str().unwrap_or("")),
        "env" => {
            let var = args["name"].as_str().unwrap_or("");
            text(&std::env::var(var).unwrap_or_else(|_| "(unset)".to_owned()))
        }
        "pids" => {
            let child = Command::new("sleep")
                .arg("300")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map(|c| c.id())
                .unwrap_or(0);
            text(&format!("{} {child}", std::process::id()))
        }
        "fail" => json!({"content": [{"type": "text", "text": "it failed"}], "isError": true}),
        "big" => text(&"b".repeat(200_000)),
        _ => json!({"content": [], "isError": true}),
    }
}

fn main() -> ExitCode {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "good".to_owned());
    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(method) = message["method"].as_str() else {
            continue;
        };
        let id = message["id"].clone();
        let result = match method {
            "initialize" => {
                if mode == "slow" {
                    std::thread::sleep(Duration::from_secs(30));
                }
                let version = if mode == "version" {
                    "1999-01-01"
                } else {
                    "2025-06-18"
                };
                json!({"protocolVersion": version, "capabilities": {"tools": {}},
                       "serverInfo": {"name": "mcp_fake", "version": "0"}})
            }
            "tools/list" => json!({"tools": tool_list()}),
            "tools/call" => {
                match mode.as_str() {
                    "hang" => continue,
                    "exit" => {
                        eprintln!("boom");
                        return ExitCode::from(3);
                    }
                    _ => {}
                }
                call(
                    message["params"]["name"].as_str().unwrap_or(""),
                    &message["params"]["arguments"],
                )
            }
            _ => continue,
        };
        if id.is_null() {
            continue;
        }
        if mode == "garbage" {
            let _ = writeln!(out, "this is not json");
            let _ = writeln!(out, "{{\"half\": ");
        }
        let _ = writeln!(
            out,
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "result": result})
        );
        let _ = out.flush();
    }
    ExitCode::SUCCESS
}

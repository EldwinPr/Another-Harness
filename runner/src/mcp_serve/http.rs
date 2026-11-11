//! `--mcp-serve --http` (issue 52): the same server over HTTP on a localhost port, so it can run
//! detached from any harness (see `daemon.rs`). MCP's streamable HTTP without sessions or
//! streaming: `POST /mcp` carries one JSON-RPC message and gets the reply as `application/json`
//! (`202` with no body for a notification). The messages are answered by [`Server::handle`], so
//! tools, modes, the decider and the determinism rule are the ones of the stdio server.
//!
//! Because a localhost port can be reached by any local process and, through a browser, by any
//! web page, and the buffer and direct modes change the database, a request is refused unless:
//!
//! - its `Host` is `127.0.0.1:PORT` or `localhost:PORT` (421), which stops DNS rebinding;
//! - it has no `Origin` header (403): browsers send one, MCP clients do not;
//! - it carries `Authorization: Bearer TOKEN` (401), with the token kept in the log folder
//!   (`mcp-token`, mode 0600, made on first use and reused by every later start).
//!
//! Requests are answered one at a time, with the limits of `web.rs`: 5 seconds to send one and
//! 1 MiB of body.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{INVALID_REQUEST, Server, error};

/// The most bytes of a request head.
const MAX_HEAD: usize = 16 * 1024;
/// The most bytes of a body.
pub const MAX_BODY: usize = 1024 * 1024;
/// How long a client has to send a whole request, and to take the answer.
const READ_TIME: Duration = Duration::from_secs(5);
/// The path the server answers on.
pub const PATH: &str = "/mcp";
/// The file in the log folder that holds the token.
pub const TOKEN_FILE: &str = "mcp-token";

/// Where the token is kept.
pub fn token_path(dir: &Path) -> PathBuf {
    dir.join(TOKEN_FILE)
}

/// The token in `dir`: read from the file when there is one, made (32 random bytes as hex, mode
/// 0600) when there is not. A file that holds anything but 64 hex digits is an error, never
/// replaced, so a hand-made token is not thrown away. Another start may have just made the file
/// and not yet written the token, so the file is read again a few times (about 80 ms) first.
pub fn token(dir: &Path) -> Result<String, String> {
    let path = token_path(dir);
    match std::fs::read_to_string(&path) {
        Ok(mut text) => {
            for _ in 0..4 {
                if is_token(text.trim()) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
                if let Ok(again) = std::fs::read_to_string(&path) {
                    text = again;
                }
            }
            let text = text.trim();
            if is_token(text) {
                Ok(text.to_owned())
            } else {
                Err(format!(
                    "{} does not hold a token (64 hex digits): fix it or delete it to get a new one",
                    path.display()
                ))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => make_token(&path),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

fn make_token(path: &Path) -> Result<String, String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| format!("cannot get random bytes for the token: {e}"))?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match options.open(path) {
        Ok(mut file) => file
            .write_all(format!("{token}\n").as_bytes())
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?,
        // Another start made it first: use theirs.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return self::token(dir_of(path));
        }
        Err(e) => return Err(format!("cannot create {}: {e}", path.display())),
    }
    Ok(token)
}

fn is_token(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

fn dir_of(path: &Path) -> &Path {
    path.parent().unwrap_or_else(|| Path::new("."))
}

/// Equal without stopping at the first difference.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |left, (x, y)| left | (x ^ y))
            == 0
}

/// One HTTP answer.
fn response(status: u16, extra: &[(&str, &str)], body: &str) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Payload Too Large",
        421 => "Misdirected Request",
        _ => "Error",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if !body.is_empty() {
        head.push_str("Content-Type: application/json\r\n");
    }
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

/// A refusal with a JSON-RPC error as the body, so a client that shows it shows the reason.
fn refuse(status: u16, extra: &[(&str, &str)], why: &str) -> Vec<u8> {
    let body = serde_json::to_string(&error(&Value::Null, INVALID_REQUEST, why))
        .expect("a JSON value serialises");
    response(status, extra, &body)
}

/// What a request head says, in the parts this server looks at.
struct Head {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

impl Head {
    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.split("\r\n");
        let mut first = lines.next()?.split(' ');
        let (method, path) = (first.next()?, first.next()?);
        if !first.next()?.starts_with("HTTP/1.") {
            return None;
        }
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        Some(Self {
            method: method.to_owned(),
            path: path.to_owned(),
            headers,
        })
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// The answer to a whole request (`bytes`, head and body as read) for a server on `port` whose
/// token is `token`. Pure: no socket, so a test can ask anything.
pub fn answer(server: &Server, token: &str, port: u16, bytes: &[u8]) -> Vec<u8> {
    let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
        return refuse(400, &[], "the request head is not complete");
    };
    let Some(head) = std::str::from_utf8(&bytes[..end])
        .ok()
        .and_then(Head::parse)
    else {
        return refuse(400, &[], "the request line is not HTTP");
    };
    let host = head.get("host").unwrap_or("").to_ascii_lowercase();
    if host != format!("127.0.0.1:{port}") && host != format!("localhost:{port}") {
        return refuse(
            421,
            &[],
            "this server answers only to 127.0.0.1 or localhost",
        );
    }
    if head.get("origin").is_some() {
        return refuse(403, &[], "a request from a web page is refused");
    }
    let sent = head
        .get("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !same(sent.trim(), token) {
        return refuse(
            401,
            &[("WWW-Authenticate", "Bearer")],
            "send the token as Authorization: Bearer TOKEN (the file mcp-token in the log folder)",
        );
    }
    if head.path != PATH {
        return refuse(404, &[], &format!("the only path is {PATH}"));
    }
    if head.method != "POST" {
        return refuse(405, &[("Allow", "POST")], "POST one JSON-RPC message");
    }
    if head.get("transfer-encoding").is_some() {
        return refuse(400, &[], "send a Content-Length, not a chunked body");
    }
    let Some(length) = head
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
    else {
        return refuse(411, &[], "a POST needs a Content-Length");
    };
    if length > MAX_BODY {
        return refuse(413, &[], "the body is over 1 MiB");
    }
    let body = &bytes[end + 4..];
    if body.len() < length {
        return refuse(400, &[], "the body is shorter than its Content-Length");
    }
    let Ok(text) = std::str::from_utf8(&body[..length]) else {
        return refuse(400, &[], "the body is not UTF-8");
    };
    let text = text.trim();
    if text.starts_with('[') {
        return refuse(400, &[], "batches are not supported: send one message");
    }
    match server.handle(text) {
        Some(reply) => {
            let json = serde_json::to_string(&reply).expect("a JSON value serialises");
            response(200, &[], &json)
        }
        None => response(202, &[], ""),
    }
}

/// Reads one request from `stream`: until the head and the declared body are in, within
/// [`READ_TIME`]. A body that declares more than [`MAX_BODY`] is not read.
fn read_request(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let started = Instant::now();
    let mut request = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut wanted: Option<usize> = None;
    loop {
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            let total = *wanted.get_or_insert_with(|| {
                let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                let length = head
                    .split("\r\n")
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .filter(|n| *n <= MAX_BODY)
                    .unwrap_or(0);
                end + 4 + length
            });
            if request.len() >= total {
                return Some(request);
            }
        } else if request.len() >= MAX_HEAD {
            return Some(request);
        }
        let left = READ_TIME.saturating_sub(started.elapsed());
        if left.is_zero() {
            return None;
        }
        stream.set_read_timeout(Some(left)).ok()?;
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return (!request.is_empty()).then_some(request),
            Ok(n) => request.extend_from_slice(&chunk[..n]),
        }
    }
}

fn handle(mut stream: TcpStream, server: &Server, token: &str, port: u16) {
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    let reply = answer(server, token, port, &request);
    let _ = stream.set_write_timeout(Some(READ_TIME));
    let _ = stream.write_all(&reply);
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Write);
}

/// Binds the port on this machine only.
pub fn bind(port: u16) -> Result<TcpListener, String> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .map_err(|e| format!("cannot listen on 127.0.0.1:{port}: {e}"))
}

/// Answers requests on `listener`, one at a time, `limit` of them or forever.
pub fn serve_on(
    listener: &TcpListener,
    server: &Server,
    token: &str,
    port: u16,
    limit: Option<usize>,
) {
    for (served, stream) in listener.incoming().enumerate() {
        if let Ok(stream) = stream {
            handle(stream, server, token, port);
        }
        if limit.is_some_and(|limit| served + 1 >= limit) {
            break;
        }
    }
}

/// What a client needs to register the server, as lines for a person.
pub fn announce(port: u16, pid: u32, mode: &str, project: &str, dir: &Path) -> String {
    format!(
        "mcp: http://127.0.0.1:{port}{PATH}  (pid {pid}, {mode} mode, project {project})\n\
         mcp: send the token in {} as  Authorization: Bearer <token>\n",
        token_path(dir).display()
    )
}

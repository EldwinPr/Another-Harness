//! Tests of `--mcp-serve --http` (issue 52). Every test works on a temporary database made by
//! `seeded` and a temporary log folder; none opens the real `harness.db` or a port other than a
//! loopback one the system chose.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;

use super::http::{PATH, TOKEN_FILE, answer, bind, serve_on, token};
use super::tests::{PROJECT, seeded};
use super::*;

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const PORT: u16 = 8788;

fn server(path: &Path) -> Server {
    build(path, PROJECT, Mode::Buffer, None, false).unwrap()
}

/// A request as bytes, with the headers a good client sends unless `headers` replaces them.
fn post(headers: &[(&str, &str)], body: &str) -> Vec<u8> {
    let mut text = format!("POST {PATH} HTTP/1.1\r\n");
    for (name, value) in headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    text.into_bytes()
}

fn good() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Host", "127.0.0.1:8788"),
        (
            "Authorization",
            concat!(
                "Bearer ",
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            ),
        ),
    ]
}

fn ask(path: &Path, request: &[u8]) -> (u16, String) {
    let reply = answer(&server(path), TOKEN, PORT, request);
    let text = String::from_utf8(reply).unwrap();
    let status = text[9..12].parse().unwrap();
    let body = text.split_once("\r\n\r\n").unwrap().1.to_owned();
    (status, body)
}

const LIST: &str = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;

#[test]
fn a_message_gets_the_same_answer_as_over_standard_input() {
    let (_dir, path) = seeded("http-same");
    let (status, body) = ask(&path, &post(&good(), LIST));
    assert_eq!(status, 200);
    let mut piped = Vec::new();
    server(&path)
        .serve(std::io::Cursor::new(format!("{LIST}\n")), &mut piped)
        .unwrap();
    assert_eq!(body, String::from_utf8(piped).unwrap().trim_end());
    let call = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"context_get","arguments":{"name":"style"}}}"#;
    let (status, body) = ask(&path, &post(&good(), call));
    assert_eq!(status, 200);
    assert!(body.contains("App style: short lines."), "{body}");
}

#[test]
fn a_notification_gets_202_and_no_body() {
    let (_dir, path) = seeded("http-note");
    let (status, body) = ask(
        &path,
        &post(
            &good(),
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        ),
    );
    assert_eq!((status, body.as_str()), (202, ""));
}

#[test]
fn a_batch_a_bad_method_a_bad_path_and_a_bad_body_are_refused() {
    let (_dir, path) = seeded("http-refuse");
    let (status, body) = ask(&path, &post(&good(), &format!("[{LIST}]")));
    assert_eq!(status, 400);
    assert!(body.contains("batches"), "{body}");
    for method in ["GET", "DELETE", "PUT"] {
        let mut request = post(&good(), LIST);
        request.splice(0..4, method.bytes().chain(std::iter::once(b' ')));
        let request = String::from_utf8(request)
            .unwrap()
            .replacen("POST", method, 1);
        let request = request.replacen(&format!("{method}  "), &format!("{method} "), 1);
        assert_eq!(ask(&path, request.as_bytes()).0, 405, "{method}");
    }
    let elsewhere = String::from_utf8(post(&good(), LIST))
        .unwrap()
        .replacen(PATH, "/other", 1);
    assert_eq!(ask(&path, elsewhere.as_bytes()).0, 404);
    // No Content-Length, a chunked body, a body over the limit, a short body.
    let mut text = format!("POST {PATH} HTTP/1.1\r\n");
    for (name, value) in good() {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    assert_eq!(ask(&path, format!("{text}\r\n{LIST}").as_bytes()).0, 411);
    assert_eq!(
        ask(
            &path,
            format!("{text}Transfer-Encoding: chunked\r\n\r\n").as_bytes()
        )
        .0,
        400
    );
    assert_eq!(
        ask(
            &path,
            format!("{text}Content-Length: 2000000\r\n\r\n").as_bytes()
        )
        .0,
        413
    );
    assert_eq!(
        ask(
            &path,
            format!("{text}Content-Length: 50\r\n\r\n{LIST}").as_bytes()
        )
        .0,
        400
    );
    assert_eq!(ask(&path, b"nonsense").0, 400);
    // A message that is not JSON is a JSON-RPC parse error, answered like over standard input.
    let (status, body) = ask(&path, &post(&good(), "not json"));
    assert_eq!(status, 200);
    assert!(body.contains("-32700"), "{body}");
}

#[test]
fn a_request_without_the_token_or_from_a_web_page_or_for_another_host_is_refused() {
    let (_dir, path) = seeded("http-guard");
    let without = |name: &str| -> Vec<(&'static str, &'static str)> {
        good().into_iter().filter(|(n, _)| *n != name).collect()
    };
    // No token, the wrong token, another scheme, a token with a character changed.
    assert_eq!(ask(&path, &post(&without("Authorization"), LIST)).0, 401);
    let mut wrong = without("Authorization");
    wrong.push(("Authorization", "Bearer nope"));
    assert_eq!(ask(&path, &post(&wrong, LIST)).0, 401);
    let mut basic = without("Authorization");
    basic.push(("Authorization", "Basic abc"));
    assert_eq!(ask(&path, &post(&basic, LIST)).0, 401);
    let near = format!("Bearer {}", TOKEN.replacen('0', "1", 1));
    let mut nearly = without("Authorization");
    nearly.push(("Authorization", near.as_str()));
    assert_eq!(ask(&path, &post(&nearly, LIST)).0, 401);
    // The refusal says how to authenticate and does nothing else.
    let reply = String::from_utf8(answer(
        &server(&path),
        TOKEN,
        PORT,
        &post(&without("Authorization"), LIST),
    ))
    .unwrap();
    assert!(reply.contains("WWW-Authenticate: Bearer"), "{reply}");
    // Another host name, another port, no Host at all.
    for host in ["evil.example:8788", "127.0.0.1:9999", "127.0.0.1"] {
        let mut other = without("Host");
        other.push(("Host", host));
        assert_eq!(ask(&path, &post(&other, LIST)).0, 421, "{host}");
    }
    assert_eq!(ask(&path, &post(&without("Host"), LIST)).0, 421);
    // `localhost` and different case are the same server.
    let mut named = without("Host");
    named.push(("host", "LocalHost:8788"));
    assert_eq!(ask(&path, &post(&named, LIST)).0, 200);
    // A browser sends an Origin; a client does not.
    let mut page = good();
    page.push(("Origin", "http://evil.example"));
    assert_eq!(ask(&path, &post(&page, LIST)).0, 403);
}

#[test]
fn a_refused_request_changes_nothing_in_the_database() {
    let (_dir, path) = seeded("http-nothing");
    let submit = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"log_add","arguments":{"tag":"STATUS","title":"t","summary":"s","source":"claude-code"}}}"#;
    let mut page = good();
    page.push(("Origin", "http://evil.example"));
    assert_eq!(ask(&path, &post(&page, submit)).0, 403);
    let count: i64 = Store::open_read_only(&path)
        .unwrap()
        .conn
        .query_row("SELECT COUNT(*) FROM work_logs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
    // And with the token the same call is written, as over standard input.
    let (status, body) = ask(&path, &post(&good(), submit));
    assert_eq!(status, 200);
    assert!(body.contains("\"isError\":false"), "{body}");
}

#[test]
fn a_real_connection_on_a_loopback_port_is_answered_and_then_closed() {
    let (_dir, path) = seeded("http-socket");
    let listener = bind(0).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = server(&path);
    let client = std::thread::spawn(move || {
        let body = LIST;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        // Send the head and the body in two writes: the server waits for the declared length.
        write!(
            stream,
            "POST {PATH} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.flush().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        stream.write_all(body.as_bytes()).unwrap();
        let mut text = String::new();
        stream.read_to_string(&mut text).unwrap();
        text
    });
    serve_on(&listener, &server, TOKEN, port, Some(1));
    let text = client.join().unwrap();
    assert!(text.starts_with("HTTP/1.1 200 OK"), "{text}");
    assert!(text.contains("\"tools\""), "{text}");
}

#[test]
fn a_port_in_use_is_an_error_naming_it() {
    let held = bind(0).unwrap();
    let port = held.local_addr().unwrap().port();
    let why = bind(port).unwrap_err();
    assert!(why.contains(&port.to_string()), "{why}");
}

#[cfg(unix)]
#[test]
fn the_token_is_made_once_kept_private_and_reused() {
    use std::os::unix::fs::PermissionsExt;
    let dir = crate::tools::testutil::Scratch::new("http-token");
    let first = token(dir.path()).unwrap();
    assert_eq!(first.len(), 64);
    assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
    let file = dir.path().join(TOKEN_FILE);
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        token(dir.path()).unwrap(),
        first,
        "the next start reuses it"
    );
    // Deleted, a new one is made.
    std::fs::remove_file(&file).unwrap();
    assert_ne!(token(dir.path()).unwrap(), first);
    // A file that is not a token is an error and is left as it is.
    std::fs::write(&file, "hello").unwrap();
    assert!(
        token(dir.path())
            .unwrap_err()
            .contains("does not hold a token")
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello");
}

#[test]
fn an_empty_token_file_another_start_is_still_filling_is_read_once_it_is_full() {
    let dir = crate::tools::testutil::Scratch::new("http-token-race");
    let file = dir.path().join(TOKEN_FILE);
    std::fs::write(&file, "").unwrap();
    let wanted = "ab".repeat(32);
    let writer = {
        let (file, line) = (file.clone(), format!("{wanted}\n"));
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            std::fs::write(&file, line).unwrap();
        })
    };
    let got = token(dir.path());
    writer.join().unwrap();
    assert_eq!(got.unwrap(), wanted);
}

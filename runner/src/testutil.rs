//! Test-only: a tiny stand-in for a router that listens on loopback only.
//! No test in this crate ever contacts a real router (context/coder-only/testing.md).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread::{self, JoinHandle};

/// What the fake router received.
pub(crate) struct Captured {
    /// First line, e.g. `POST /v1/chat/completions HTTP/1.1`.
    pub request_line: String,
    /// Header names lower-cased.
    pub headers: Vec<(String, String)>,
    /// The request body, byte for byte.
    pub body: Vec<u8>,
}

impl Captured {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn read_request(stream: &mut std::net::TcpStream) -> Captured {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf).expect("read request");
        data.extend_from_slice(&buf[..n]);
        if let Some(split) = find(&data, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&data[..split]).into_owned();
            let mut lines = head.lines();
            let request_line = lines.next().unwrap_or_default().to_owned();
            let headers: Vec<(String, String)> = lines
                .filter_map(|l| l.split_once(':'))
                .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned()))
                .collect();
            let length = headers
                .iter()
                .find(|(n, _)| n == "content-length")
                .and_then(|(_, v)| v.parse::<usize>().ok())
                .unwrap_or(0);
            let body_start = split + 4;
            if data.len() >= body_start + length || n == 0 {
                return Captured {
                    request_line,
                    headers,
                    body: data[body_start..].to_vec(),
                };
            }
        } else if n == 0 {
            panic!("connection closed before a full request arrived");
        }
    }
}

/// Starts a fake router that answers exactly one request. Returns its base URL
/// (ending in `/v1`) and a handle that yields what it received.
///
/// `status_line` is e.g. `"200 OK"`; `extra_headers` are added to the reply.
pub(crate) fn fake_router(
    status_line: &'static str,
    extra_headers: &'static [(&'static str, &'static str)],
    body: &'static str,
) -> (String, JoinHandle<Captured>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let base_url = format!("http://{}/v1", listener.local_addr().expect("local addr"));
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let captured = read_request(&mut stream);
        let mut response = format!("HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\n");
        for (name, value) in extra_headers {
            response.push_str(&format!("{name}: {value}\r\n"));
        }
        response.push_str(&format!(
            "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ));
        stream.write_all(response.as_bytes()).expect("write reply");
        captured
    });
    (base_url, handle)
}

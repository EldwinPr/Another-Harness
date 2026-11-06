//! Throwaway: send one hardcoded Chat Completions request to a router and print
//! exactly what comes back.
//!
//! Not part of the agent. It exists to prove the wire path to OpenRouter and
//! 9router, and to show which response header or body field says which provider
//! and account served the request (issue 4 needs that to fill `served_by`).
//!
//! Configuration comes from the environment only:
//!
//! ```text
//! HARNESS_BASE_URL   e.g. https://openrouter.ai/api/v1   (no trailing /chat/completions)
//! HARNESS_API_KEY    optional; only if the router wants one. Never printed
//! HARNESS_MODEL      the model name as that router spells it
//! ```
//!
//! Run: `cd src && cargo run --bin hello`
//! Exit code: 0 on a 2xx reply, 1 on any other reply or a network failure, 2 on
//! missing configuration (base URL or model).

use std::process::ExitCode;
use std::time::Duration;

use serde::Deserialize;
use serde_json::value::RawValue;

const ENV_BASE_URL: &str = "HARNESS_BASE_URL";
const ENV_API_KEY: &str = "HARNESS_API_KEY";
const ENV_MODEL: &str = "HARNESS_MODEL";

struct HelloConfig {
    base_url: String,
    api_key: Option<String>,
    model: String,
}

/// Everything the router sent back, untouched.
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

// Rust note: the parameter `get` is "any function that takes a name and returns
// an optional value". `main` passes a function that reads the real environment;
// the tests pass a fake one, so no test ever touches real environment variables.
fn config_from(get: impl Fn(&str) -> Option<String>) -> Result<HelloConfig, String> {
    // A variable that is set but empty counts as missing.
    let read = |name: &str| get(name).filter(|value| !value.trim().is_empty());
    // The key is optional: a router that needs none (9router here) is fine.
    let api_key = read(ENV_API_KEY);
    match (read(ENV_BASE_URL), read(ENV_MODEL)) {
        (Some(base_url), Some(model)) => Ok(HelloConfig {
            base_url,
            api_key,
            model,
        }),
        (base_url, model) => {
            let missing: Vec<&str> = [
                (ENV_BASE_URL, base_url.is_none()),
                (ENV_MODEL, model.is_none()),
            ]
            .into_iter()
            .filter(|(_, is_missing)| *is_missing)
            .map(|(name, _)| name)
            .collect();
            Err(format!(
                "missing environment variable(s): {}",
                missing.join(", ")
            ))
        }
    }
}

fn endpoint(base_url: &str) -> String {
    format!("{}/chat/completions", base_url.trim_end_matches('/'))
}

/// The one hardcoded request. `serde_json::json!` builds it; key order does not
/// matter for this throwaway.
fn request_body(model: &str) -> String {
    serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "Reply with the single word: pong"}],
        "max_tokens": 16,
        "stream": false,
    })
    .to_string()
}

/// Replaces every occurrence of the key, in case a router echoes it back.
fn redact(text: &str, api_key: &str) -> String {
    if api_key.is_empty() {
        text.to_owned()
    } else {
        text.replace(api_key, "[REDACTED]")
    }
}

// Rust note: a struct with a single optional field. `Box<RawValue>` keeps the
// `usage` JSON as the exact text the router sent, not a re-formatted copy.
#[derive(Deserialize)]
struct UsageOnly {
    usage: Option<Box<RawValue>>,
}

fn usage_of(body: &str) -> Option<String> {
    let parsed: UsageOnly = serde_json::from_str(body).ok()?;
    parsed.usage.map(|raw| raw.get().to_owned())
}

fn report(reply: &Reply, api_key: &str) -> String {
    let mut out = format!("status: {}\n\nheaders:\n", reply.status);
    for (name, value) in &reply.headers {
        out.push_str(&format!("  {name}: {value}\n"));
    }
    out.push_str("\nbody (raw):\n");
    out.push_str(&reply.body);
    out.push_str("\n\nusage (raw):\n");
    match usage_of(&reply.body) {
        Some(usage) => out.push_str(&usage),
        None => out.push_str("(no usage object found in the body)"),
    }
    out.push('\n');
    redact(&out, api_key)
}

fn exchange(config: &HelloConfig) -> Result<Reply, String> {
    // By default ureq turns a 4xx/5xx reply into an error and drops the body.
    // We want to see error replies in full, so that is switched off.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(60)))
        .build()
        .into();
    let mut builder = agent
        .post(endpoint(&config.base_url))
        .header("Content-Type", "application/json");
    if let Some(key) = &config.api_key {
        builder = builder.header("Authorization", format!("Bearer {key}"));
    }
    let mut response = builder
        .send(request_body(&config.model))
        .map_err(|e| format!("request failed: {e}"))?;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            let value = value.to_str().unwrap_or("<not text>").to_owned();
            (name.as_str().to_owned(), value)
        })
        .collect();
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("could not read the response body: {e}"))?;
    Ok(Reply {
        status,
        headers,
        body,
    })
}

fn main() -> ExitCode {
    let config = match config_from(|name| std::env::var(name).ok()) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("error: {message}");
            return ExitCode::from(2);
        }
    };
    println!(
        "POST {}\nmodel: {}\n",
        endpoint(&config.base_url),
        config.model
    );
    match exchange(&config) {
        Ok(reply) => {
            print!(
                "{}",
                report(&reply, config.api_key.as_deref().unwrap_or(""))
            );
            if (200..300).contains(&reply.status) {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(message) => {
            eprintln!(
                "error: {}",
                redact(&message, config.api_key.as_deref().unwrap_or(""))
            );
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| {
            owned
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn complete_configuration_is_accepted() {
        let get = env_of(&[
            (ENV_BASE_URL, "http://x/v1"),
            (ENV_API_KEY, "k"),
            (ENV_MODEL, "m"),
        ]);
        let config = config_from(get).expect("should be accepted");
        assert_eq!(
            (config.base_url, config.api_key, config.model),
            ("http://x/v1".into(), Some("k".into()), "m".into())
        );
    }

    #[test]
    fn the_key_is_optional() {
        let get = env_of(&[(ENV_BASE_URL, "http://x/v1"), (ENV_MODEL, "m")]);
        let config = config_from(get).expect("no key is fine");
        assert_eq!(config.api_key, None);
    }

    #[test]
    fn missing_variables_are_named_in_the_error() {
        let get = env_of(&[(ENV_API_KEY, "k")]);
        let message = config_from(get).err().expect("should be rejected");
        assert!(message.contains(ENV_BASE_URL), "{message}");
        assert!(message.contains(ENV_MODEL), "{message}");
        assert!(!message.contains(ENV_API_KEY), "{message}");
    }

    #[test]
    fn empty_and_blank_values_count_as_missing() {
        let get = env_of(&[(ENV_BASE_URL, ""), (ENV_API_KEY, "   "), (ENV_MODEL, "m")]);
        let message = config_from(get).err().expect("should be rejected");
        assert!(message.contains(ENV_BASE_URL), "{message}");
        assert!(!message.contains(ENV_MODEL), "{message}");
    }

    #[test]
    fn the_error_never_contains_a_key_value() {
        let get = env_of(&[(ENV_API_KEY, "super-secret-key")]);
        let message = config_from(get).err().expect("should be rejected");
        assert!(!message.contains("super-secret-key"));
    }

    #[test]
    fn endpoint_joins_cleanly_with_or_without_trailing_slash() {
        assert_eq!(endpoint("http://h/v1"), "http://h/v1/chat/completions");
        assert_eq!(endpoint("http://h/v1/"), "http://h/v1/chat/completions");
    }

    #[test]
    fn request_body_is_the_expected_json() {
        let parsed: serde_json::Value = serde_json::from_str(&request_body("some/model")).unwrap();
        assert_eq!(parsed["model"], "some/model");
        assert_eq!(parsed["stream"], false);
        assert_eq!(parsed["messages"][0]["role"], "user");
    }

    #[test]
    fn usage_is_extracted_exactly_as_sent() {
        let body = r#"{"id":"x","usage": {"prompt_tokens":5,  "completion_tokens":1}}"#;
        assert_eq!(
            usage_of(body).as_deref(),
            Some(r#"{"prompt_tokens":5,  "completion_tokens":1}"#)
        );
    }

    #[test]
    fn missing_usage_or_non_json_gives_none() {
        assert_eq!(usage_of(r#"{"id":"x"}"#), None);
        assert_eq!(usage_of("<html>502 Bad Gateway</html>"), None);
        assert_eq!(usage_of(""), None);
    }

    #[test]
    fn report_never_contains_the_key_even_if_the_router_echoes_it() {
        let reply = Reply {
            status: 401,
            headers: vec![("x-echo".into(), "Bearer super-secret-key".into())],
            body: r#"{"error":"bad key super-secret-key"}"#.into(),
        };
        let text = report(&reply, "super-secret-key");
        assert!(!text.contains("super-secret-key"), "{text}");
        assert!(text.contains("[REDACTED]"));
        assert!(text.contains("status: 401"));
    }

    #[test]
    fn report_shows_status_headers_body_and_usage() {
        let reply = Reply {
            status: 200,
            headers: vec![("x-provider".into(), "FakeProvider".into())],
            body: r#"{"usage":{"prompt_tokens":3}}"#.into(),
        };
        let text = report(&reply, "test-key-123");
        assert!(text.contains("status: 200"));
        assert!(text.contains("x-provider: FakeProvider"));
        assert!(text.contains(r#"{"usage":{"prompt_tokens":3}}"#));
        assert!(text.ends_with("{\"prompt_tokens\":3}\n"), "{text}");
    }

    /// Reads one HTTP request from the socket: headers, then `Content-Length` bytes.
    fn read_request(stream: &mut std::net::TcpStream) -> String {
        let mut data = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let n = stream.read(&mut buf).unwrap();
            data.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&data).into_owned();
            if let Some(split) = text.find("\r\n\r\n") {
                let length = text[..split]
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(str::to_owned)
                    })
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if data.len() >= split + 4 + length {
                    return text;
                }
            }
            if n == 0 {
                return text;
            }
        }
    }

    /// A tiny local stand-in for a router: answers one request with `status` and
    /// `body`, and returns the raw request it received. Loopback only.
    fn fake_router(status: &str, body: &'static str) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let status = status.to_owned();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nx-provider: FakeProvider\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            request
        });
        (base_url, handle)
    }

    #[test]
    fn exchange_sends_the_request_and_returns_the_reply_untouched() {
        let body = r#"{"choices":[{"message":{"content":"pong"}}],"usage":{"prompt_tokens":9,"completion_tokens":1}}"#;
        let (base_url, server) = fake_router("200 OK", body);
        let config = HelloConfig {
            base_url,
            api_key: Some("test-key-123".into()),
            model: "fake/model".into(),
        };
        let reply = exchange(&config).expect("exchange should succeed");
        let request = server.join().unwrap();

        // What the router received.
        assert!(
            request.starts_with("POST /v1/chat/completions HTTP/1.1"),
            "{request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer test-key-123")
        );
        assert!(request.contains(r#""model":"fake/model""#), "{request}");
        // What we got back.
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, body);
        assert!(
            reply
                .headers
                .iter()
                .any(|(n, v)| n == "x-provider" && v == "FakeProvider")
        );
        assert!(usage_of(&reply.body).unwrap().contains("prompt_tokens"));
        // And the printed report hides the key.
        assert!(!report(&reply, config.api_key.as_deref().unwrap_or("")).contains("test-key-123"));
    }

    #[test]
    fn without_a_key_no_authorization_header_is_sent() {
        let (base_url, server) = fake_router("200 OK", r#"{"choices":[]}"#);
        let config = HelloConfig {
            base_url,
            api_key: None,
            model: "m".into(),
        };
        let reply = exchange(&config).expect("a router that needs no key still answers");
        let request = server.join().unwrap();
        assert_eq!(reply.status, 200);
        assert!(
            !request.to_ascii_lowercase().contains("authorization:"),
            "{request}"
        );
    }

    #[test]
    fn error_replies_are_shown_in_full_not_turned_into_failures() {
        let body = r#"{"error":{"message":"invalid api key"}}"#;
        let (base_url, server) = fake_router("401 Unauthorized", body);
        let config = HelloConfig {
            base_url,
            api_key: Some("k".into()),
            model: "m".into(),
        };
        let reply = exchange(&config).expect("a 401 is still a reply");
        server.join().unwrap();
        assert_eq!(reply.status, 401);
        assert_eq!(reply.body, body);
    }

    #[test]
    fn an_unreachable_router_is_reported_as_an_error() {
        // Bind then drop, so the port is very likely closed.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let config = HelloConfig {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            api_key: Some("k".into()),
            model: "m".into(),
        };
        let message = exchange(&config).err().expect("nothing is listening");
        assert!(message.starts_with("request failed"), "{message}");
    }
}

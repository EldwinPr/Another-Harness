//! The router call: send assembled request bytes, get a parsed reply back.
//!
//! Chat Completions over HTTP (decision 06). The reply keeps everything needed
//! for the turn log: the raw body exactly as received, the raw `usage` text, the
//! elapsed time, and what is known about who served the request.

use crate::cancel::Cancel;
use std::fmt;
use std::time::{Duration, Instant};

use kernel::{Message, ToolCall};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

/// Who served a request. Every field is optional because routers differ in what
/// they reveal: 9router returns neither provider nor account (issue 2 finding),
/// so those two are `None` on that path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ServedBy {
    /// The router's host, taken from the base URL (e.g. `localhost:20128`).
    pub router: Option<String>,
    pub provider: Option<String>,
    pub account: Option<String>,
}

/// A successful, parsed reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    /// The body exactly as received.
    pub raw_body: String,
    pub elapsed: Duration,
    pub served_by: ServedBy,
    /// The assistant's answer: text, tool calls, or both.
    pub message: Message,
    pub finish_reason: Option<String>,
    /// The model name as the router returned it (9router strips a prefix).
    pub response_model: Option<String>,
    /// The `usage` object as the exact text the router sent.
    pub usage: Option<String>,
}

/// A call that produced no usable reply. It still carries whatever came back, so
/// the turn log can record a failure as fully as a success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError {
    pub message: String,
    /// `None` when no HTTP response arrived at all.
    pub status: Option<u16>,
    pub raw_body: Option<String>,
    pub elapsed: Duration,
    pub served_by: ServedBy,
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ProviderError {}

// The shapes below mirror the JSON a router sends. Option fields accept both a
// missing key and `null`, which 9router uses for `tool_calls`.
#[derive(Deserialize)]
struct WireReply {
    #[serde(default)]
    choices: Vec<WireChoice>,
    model: Option<String>,
    usage: Option<Box<RawValue>>,
    // OpenRouter names the provider that served the call here (e.g. "Wafer").
    // Kept as a generic JSON value so an unexpected shape cannot fail the parse.
    provider: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct WireChoice {
    message: WireMessage,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct WireMessage {
    content: Option<String>,
    tool_calls: Option<Vec<WireToolCall>>,
}

#[derive(Deserialize)]
struct WireToolCall {
    id: String,
    function: WireFunction,
}

#[derive(Deserialize)]
struct WireFunction {
    name: String,
    // A JSON string holding JSON text: kept exactly as the model wrote it.
    arguments: String,
}

struct Parsed {
    message: Message,
    finish_reason: Option<String>,
    response_model: Option<String>,
    usage: Option<String>,
    provider: Option<String>,
}

fn parse_body(body: &str) -> Result<Parsed, String> {
    let wire: WireReply =
        serde_json::from_str(body).map_err(|e| format!("reply is not a chat completion: {e}"))?;
    let choice = wire
        .choices
        .into_iter()
        .next()
        .ok_or("reply has no choices")?;
    let tool_calls = choice
        .message
        .tool_calls
        .unwrap_or_default()
        .into_iter()
        .map(|call| ToolCall {
            id: call.id,
            name: call.function.name,
            arguments: call.function.arguments,
        })
        .collect();
    Ok(Parsed {
        message: Message::Assistant {
            content: choice.message.content,
            tool_calls,
        },
        finish_reason: choice.finish_reason,
        response_model: wire.model,
        usage: wire.usage.map(|raw| raw.get().to_owned()),
        provider: wire
            .provider
            .and_then(|v| v.as_str().map(str::to_owned))
            .filter(|name| !name.is_empty()),
    })
}

/// The ids in a model list: `{"data": [{"id": "..."}, ...]}`.
pub fn parse_models(body: &str) -> Result<Vec<String>, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "the model list is not JSON".to_owned())?;
    let data = value["data"]
        .as_array()
        .ok_or("the model list has no \"data\" array")?;
    Ok(data
        .iter()
        .filter_map(|entry| entry["id"].as_str().map(str::to_owned))
        .collect())
}

/// `http://localhost:20128/v1` -> `localhost:20128`.
pub fn router_host(base_url: &str) -> Option<String> {
    let rest = base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest);
    let host = rest.split('/').next().unwrap_or_default();
    (!host.is_empty()).then(|| host.to_owned())
}

/// Sends requests to one router. No `Debug` on purpose: it holds the API key.
#[derive(Clone)]
pub struct HttpProvider {
    endpoint: String,
    models_endpoint: String,
    api_key: Option<String>,
    served_by: ServedBy,
    agent: ureq::Agent,
}

impl HttpProvider {
    /// `api_key` is `None` for a router that needs none (the owner's 9router):
    /// then no `Authorization` header is sent at all.
    pub fn new(base_url: &str, api_key: Option<&str>, timeout: Duration) -> Self {
        // By default ureq turns a 4xx/5xx reply into an error and drops the
        // body; we want to keep it for the log, so that is switched off.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(timeout))
            .build()
            .into();
        Self {
            endpoint: format!("{}/chat/completions", base_url.trim_end_matches('/')),
            models_endpoint: format!("{}/models", base_url.trim_end_matches('/')),
            api_key: api_key.map(str::to_owned),
            served_by: ServedBy {
                router: router_host(base_url),
                ..ServedBy::default()
            },
            agent,
        }
    }

    /// Asks the router which models it offers (`GET {base_url}/models`, the OpenAI shape).
    /// Not every router has this endpoint; the error says what came back.
    pub fn models(&self) -> Result<Vec<String>, String> {
        let mut builder = self.agent.get(&self.models_endpoint);
        if let Some(key) = &self.api_key {
            builder = builder.header("Authorization", format!("Bearer {key}"));
        }
        let mut response = builder.call().map_err(|e| format!("request failed: {e}"))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("could not read the response body: {e}"))?;
        if !(200..300).contains(&status) {
            return Err(format!(
                "the router answered the model list with status {status}, so it may not offer one"
            ));
        }
        parse_models(&body)
    }

    /// POSTs the assembled request bytes unchanged and parses the reply.
    // Rust note: the error is boxed (`Box<..>` puts the value on the heap) because
    // it is large, and `Result` is as big as its biggest variant.
    pub fn complete(&self, request: &[u8]) -> Result<Reply, Box<ProviderError>> {
        let started = Instant::now();
        let fail = |message: String, status: Option<u16>, raw_body: Option<String>| {
            Box::new(ProviderError {
                message,
                status,
                raw_body,
                elapsed: started.elapsed(),
                served_by: self.served_by.clone(),
            })
        };

        // Rust note: `if let Some(key) = &self.api_key` runs the body only when
        // there is a key, binding it to `key`.
        let mut builder = self
            .agent
            .post(&self.endpoint)
            .header("Content-Type", "application/json");
        if let Some(key) = &self.api_key {
            builder = builder.header("Authorization", format!("Bearer {key}"));
        }
        let mut response = builder
            .send(request)
            .map_err(|e| fail(format!("request failed: {e}"), None, None))?;
        let status = response.status().as_u16();
        let raw_body = response.body_mut().read_to_string().map_err(|e| {
            fail(
                format!("could not read the response body: {e}"),
                Some(status),
                None,
            )
        })?;
        let elapsed = started.elapsed();

        if !(200..300).contains(&status) {
            return Err(fail(
                format!("router answered with status {status}"),
                Some(status),
                Some(raw_body),
            ));
        }
        match parse_body(&raw_body) {
            Ok(parsed) => Ok(Reply {
                status,
                raw_body,
                elapsed,
                served_by: ServedBy {
                    provider: parsed.provider,
                    ..self.served_by.clone()
                },
                message: parsed.message,
                finish_reason: parsed.finish_reason,
                response_model: parsed.response_model,
                usage: parsed.usage,
            }),
            Err(message) => Err(fail(message, Some(status), Some(raw_body))),
        }
    }
}

/// Anything that can answer an assembled request. `HttpProvider` is the real
/// one; `agent::ScriptedProvider` stands in for it in tests.
//
// Rust note: a trait is a promise about methods, like an interface. The agent
// loop holds a `Box<dyn Provider>`, so it works with either without knowing which.
pub trait Provider: Send + Sync {
    fn complete(&self, request: &[u8]) -> Result<Reply, Box<ProviderError>>;

    /// Like [`Provider::complete`], but gives up early with an error when `cancel` is set while
    /// waiting. The default cannot be interrupted and just calls `complete`.
    fn complete_cancellable(
        &self,
        request: &[u8],
        _cancel: &Cancel,
    ) -> Result<Reply, Box<ProviderError>> {
        self.complete(request)
    }

    /// The model ids this provider offers. Not every provider can say.
    fn models(&self) -> Result<Vec<String>, String> {
        Err("this provider cannot list its models".to_owned())
    }
}

/// Runs `job` (one request) on its own thread and watches `cancel` every 25 ms; after a cancel the
/// thread is left to finish by itself (it ends at the request timeout) and its answer is dropped.
pub(crate) fn run_cancellable(
    served_by: &ServedBy,
    cancel: &Cancel,
    job: impl FnOnce() -> Result<Reply, Box<ProviderError>> + Send + 'static,
) -> Result<Reply, Box<ProviderError>> {
    let started = Instant::now();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(job());
    });
    let gone = |message: &str| {
        Box::new(ProviderError {
            message: message.to_owned(),
            status: None,
            raw_body: None,
            elapsed: started.elapsed(),
            served_by: served_by.clone(),
        })
    };
    loop {
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(result) => return result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if cancel.is_cancelled() {
                    return Err(gone(CANCELLED));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(gone("the request thread ended without an answer"));
            }
        }
    }
}

/// Stands in until a provider is chosen (the chat starts with none the first time). The agent
/// refuses to send before then, so `complete` is not normally reached.
pub struct NoProvider;

/// The error text of a request that was given up on because of a cancel.
pub const CANCELLED: &str = "cancelled";

/// What to tell someone who has not chosen a provider yet.
pub const CHOOSE_A_PROVIDER: &str = "no provider selected: choose one with /provider <name>, or add one with /provider add <name> <base_url> <model>";

impl Provider for NoProvider {
    fn complete(&self, _request: &[u8]) -> Result<Reply, Box<ProviderError>> {
        Err(Box::new(ProviderError {
            message: CHOOSE_A_PROVIDER.to_owned(),
            status: None,
            raw_body: None,
            elapsed: Duration::ZERO,
            served_by: ServedBy::default(),
        }))
    }

    fn models(&self) -> Result<Vec<String>, String> {
        Err(CHOOSE_A_PROVIDER.to_owned())
    }
}

impl Provider for HttpProvider {
    fn complete(&self, request: &[u8]) -> Result<Reply, Box<ProviderError>> {
        // `HttpProvider::complete` names the inherent method above, not this one.
        HttpProvider::complete(self, request)
    }

    /// The request runs on its own thread and this one watches the flag every 25 ms. After a
    /// cancel the thread is left to finish by itself (it ends at the request timeout) and its
    /// answer is dropped.
    fn complete_cancellable(
        &self,
        request: &[u8],
        cancel: &Cancel,
    ) -> Result<Reply, Box<ProviderError>> {
        let me = self.clone();
        let body = request.to_vec();
        run_cancellable(&self.served_by, cancel, move || {
            HttpProvider::complete(&me, &body)
        })
    }

    fn models(&self) -> Result<Vec<String>, String> {
        HttpProvider::models(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::fake_router;
    use std::net::TcpListener;

    const REQUEST: &[u8] =
        br#"{"model":"m","messages":[{"role":"user","content":"hi"}],"stream":false}"#;
    const KEY: &str = "test-key-123";
    const TIMEOUT: Duration = Duration::from_secs(10);

    const PLAIN_REPLY: &str = r#"{"id":"x","choices":[{"finish_reason":"stop","index":0,"message":{"content":"pong","role":"assistant","tool_calls":null}}],"model":"mimo-v2.6-flash","usage": {"prompt_tokens":2770,  "completion_tokens":8,"prompt_tokens_details":{"cached_tokens":512}}}"#;

    #[test]
    fn a_cancel_gives_up_on_a_request_that_is_still_waiting() {
        // A router that accepts and then never answers.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let holder = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_secs(3));
            drop(stream);
        });
        let provider = HttpProvider::new(&base, None, Duration::from_secs(10));
        let cancel = Cancel::new();
        let flag = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            flag.cancel();
        });
        let started = Instant::now();
        let error = Provider::complete_cancellable(&provider, REQUEST, &cancel).unwrap_err();
        assert_eq!(error.message, CANCELLED);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        holder.join().unwrap();
    }

    #[test]
    fn without_a_cancel_the_cancellable_call_returns_the_answer() {
        let (base, handle) = fake_router(
            "200 OK",
            &[],
            r#"{"choices":[{"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}"#,
        );
        let provider = HttpProvider::new(&base, None, Duration::from_secs(5));
        let reply = Provider::complete_cancellable(&provider, REQUEST, &Cancel::new()).unwrap();
        assert_eq!(reply.status, 200);
        handle.join().unwrap();
    }

    #[test]
    fn sends_the_bytes_unchanged_to_the_right_path_with_the_key() {
        let (base_url, server) = fake_router("200 OK", &[], PLAIN_REPLY);
        HttpProvider::new(&base_url, Some(KEY), TIMEOUT)
            .complete(REQUEST)
            .expect("reply");
        let got = server.join().unwrap();
        assert_eq!(got.request_line, "POST /v1/chat/completions HTTP/1.1");
        assert_eq!(got.body, REQUEST);
        assert_eq!(got.header("authorization"), Some("Bearer test-key-123"));
        assert_eq!(got.header("content-type"), Some("application/json"));
    }

    #[test]
    fn without_a_key_no_authorization_header_is_sent() {
        let (base_url, server) = fake_router("200 OK", &[], PLAIN_REPLY);
        let reply = HttpProvider::new(&base_url, None, TIMEOUT)
            .complete(REQUEST)
            .expect("a router that needs no key still answers");
        let got = server.join().unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(got.header("authorization"), None);
        assert_eq!(got.body, REQUEST);
    }

    #[test]
    fn parses_the_9router_reply_shape_including_null_tool_calls() {
        let (base_url, server) = fake_router("200 OK", &[], PLAIN_REPLY);
        let reply = HttpProvider::new(&base_url, Some(KEY), TIMEOUT)
            .complete(REQUEST)
            .expect("reply");
        server.join().unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.raw_body, PLAIN_REPLY);
        assert_eq!(
            reply.message,
            Message::Assistant {
                content: Some("pong".into()),
                tool_calls: vec![]
            }
        );
        assert_eq!(reply.finish_reason.as_deref(), Some("stop"));
        assert_eq!(reply.response_model.as_deref(), Some("mimo-v2.6-flash"));
        // The usage text is kept exactly as sent, odd spacing included.
        assert_eq!(
            reply.usage.as_deref(),
            Some(
                r#"{"prompt_tokens":2770,  "completion_tokens":8,"prompt_tokens_details":{"cached_tokens":512}}"#
            )
        );
    }

    #[test]
    fn parses_tool_calls_and_keeps_arguments_verbatim() {
        let body = r#"{"choices":[{"finish_reason":"tool_calls","message":{"content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{ \"path\" :  \"Cargo.toml\" }"}}]}}]}"#;
        let (base_url, server) = fake_router("200 OK", &[], body);
        let reply = HttpProvider::new(&base_url, Some(KEY), TIMEOUT)
            .complete(REQUEST)
            .expect("reply");
        server.join().unwrap();
        assert_eq!(
            reply.message,
            Message::Assistant {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    arguments: "{ \"path\" :  \"Cargo.toml\" }".into(),
                }]
            }
        );
        assert_eq!(reply.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(reply.usage, None);
    }

    #[test]
    fn a_missing_tool_calls_field_is_also_fine() {
        let body = r#"{"choices":[{"message":{"content":"ok"}}]}"#;
        let (base_url, server) = fake_router("200 OK", &[], body);
        let reply = HttpProvider::new(&base_url, Some(KEY), TIMEOUT)
            .complete(REQUEST)
            .expect("reply");
        server.join().unwrap();
        assert!(matches!(
            reply.message,
            Message::Assistant { tool_calls, .. } if tool_calls.is_empty()
        ));
    }

    #[test]
    fn served_by_names_the_router_host_and_leaves_the_rest_empty() {
        let (base_url, server) = fake_router("200 OK", &[], PLAIN_REPLY);
        let reply = HttpProvider::new(&base_url, Some(KEY), TIMEOUT)
            .complete(REQUEST)
            .expect("reply");
        server.join().unwrap();
        let host = base_url
            .trim_start_matches("http://")
            .trim_end_matches("/v1")
            .to_owned();
        assert_eq!(
            reply.served_by,
            ServedBy {
                router: Some(host),
                provider: None,
                account: None
            }
        );
    }

    #[test]
    fn a_provider_named_in_the_reply_body_fills_served_by() {
        // OpenRouter puts the provider that served the call at the top level.
        const REPLY: &str = r#"{"provider":"Wafer","choices":[{"finish_reason":"stop","message":{"content":"ok","role":"assistant"}}],"model":"m","usage":{"prompt_tokens":1}}"#;
        let (base_url, server) = fake_router("200 OK", &[], REPLY);
        let reply = HttpProvider::new(&base_url, None, TIMEOUT)
            .complete(REQUEST)
            .expect("reply");
        server.join().unwrap();
        assert_eq!(reply.served_by.provider.as_deref(), Some("Wafer"));
        assert!(reply.served_by.router.is_some());
    }

    #[test]
    fn a_provider_field_of_the_wrong_shape_does_not_break_the_reply() {
        const REPLY: &str = r#"{"provider":{"name":"x"},"choices":[{"finish_reason":"stop","message":{"content":"ok","role":"assistant"}}],"model":"m"}"#;
        let (base_url, server) = fake_router("200 OK", &[], REPLY);
        let reply = HttpProvider::new(&base_url, None, TIMEOUT)
            .complete(REQUEST)
            .expect("reply");
        server.join().unwrap();
        assert_eq!(reply.served_by.provider, None);
    }

    #[test]
    fn an_error_status_keeps_the_status_and_the_raw_body() {
        let body = r#"{"error":{"message":"invalid api key"}}"#;
        let (base_url, server) = fake_router("401 Unauthorized", &[], body);
        let err = HttpProvider::new(&base_url, Some(KEY), TIMEOUT)
            .complete(REQUEST)
            .expect_err("a 401 is not a usable reply");
        server.join().unwrap();
        assert_eq!(err.status, Some(401));
        assert_eq!(err.raw_body.as_deref(), Some(body));
        assert!(err.message.contains("401"), "{}", err.message);
    }

    #[test]
    fn a_garbage_body_with_status_200_is_an_error_that_keeps_the_body() {
        let body = "<html>502 Bad Gateway</html>";
        let (base_url, server) = fake_router("200 OK", &[], body);
        let err = HttpProvider::new(&base_url, Some(KEY), TIMEOUT)
            .complete(REQUEST)
            .expect_err("not JSON");
        server.join().unwrap();
        assert_eq!(err.status, Some(200));
        assert_eq!(err.raw_body.as_deref(), Some(body));
    }

    #[test]
    fn json_without_choices_is_an_error_that_keeps_the_body() {
        let body = r#"{"id":"x","choices":[]}"#;
        let (base_url, server) = fake_router("200 OK", &[], body);
        let err = HttpProvider::new(&base_url, Some(KEY), TIMEOUT)
            .complete(REQUEST)
            .expect_err("no choices");
        server.join().unwrap();
        assert!(err.message.contains("no choices"), "{}", err.message);
        assert_eq!(err.raw_body.as_deref(), Some(body));
    }

    #[test]
    fn an_unreachable_router_gives_an_error_with_no_status() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let err = HttpProvider::new(&format!("http://127.0.0.1:{port}/v1"), Some(KEY), TIMEOUT)
            .complete(REQUEST)
            .expect_err("nothing is listening");
        assert_eq!(err.status, None);
        assert_eq!(err.raw_body, None);
        assert!(err.message.starts_with("request failed"), "{}", err.message);
        assert!(!err.message.contains(KEY));
    }

    #[test]
    fn router_host_handles_common_url_shapes() {
        assert_eq!(
            router_host("http://localhost:20128/v1").as_deref(),
            Some("localhost:20128")
        );
        assert_eq!(
            router_host("https://openrouter.ai/api/v1").as_deref(),
            Some("openrouter.ai")
        );
        assert_eq!(
            router_host("localhost:20128").as_deref(),
            Some("localhost:20128")
        );
        assert_eq!(router_host(""), None);
    }

    #[test]
    fn a_model_list_is_read_from_the_data_array() {
        let ids = parse_models(r#"{"data":[{"id":"a/b"},{"id":"c"},{"nope":1}]}"#).unwrap();
        assert_eq!(ids, ["a/b", "c"]);
        assert!(parse_models("not json").unwrap_err().contains("not JSON"));
        assert!(
            parse_models(r#"{"models":[]}"#)
                .unwrap_err()
                .contains("data")
        );
        assert_eq!(
            parse_models(r#"{"data":[]}"#).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn the_model_list_is_fetched_with_the_key_from_the_models_path() {
        let (base_url, handle) =
            fake_router("200 OK", &[], r#"{"data":[{"id":"m1"},{"id":"m2"}]}"#);
        let ids = HttpProvider::new(&base_url, Some(KEY), TIMEOUT)
            .models()
            .unwrap();
        assert_eq!(ids, ["m1", "m2"]);
        let seen = handle.join().unwrap();
        assert_eq!(seen.request_line, "GET /v1/models HTTP/1.1");
        assert_eq!(
            seen.header("authorization"),
            Some(format!("Bearer {KEY}").as_str())
        );
    }

    #[test]
    fn a_router_without_a_model_list_gives_a_clear_error() {
        let (base_url, handle) = fake_router("404 Not Found", &[], "{}");
        let why = HttpProvider::new(&base_url, None, TIMEOUT)
            .models()
            .unwrap_err();
        assert!(why.contains("status 404"), "{why}");
        let seen = handle.join().unwrap();
        assert_eq!(seen.header("authorization"), None);
    }
}

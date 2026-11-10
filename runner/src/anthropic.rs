//! Anthropic's own Messages API (`POST {base_url}/messages`), as a provider.
//!
//! The harness builds one request shape (OpenAI chat completions, by `kernel::assemble`) and
//! logs those bytes. This provider turns that request into a Messages request just before
//! sending, and turns the answer back into the shape the rest of the program reads, so the
//! agent loop, the turn log and the usage figures need no second path.
//!
//! Caching is asked for on the system prompt and on the last message (two breakpoints); the
//! tools come before the system prompt, so they are covered by the first one.

use crate::cancel::Cancel;
use crate::provider::{
    Provider, ProviderError, Reply, ServedBy, parse_models, router_host, run_cancellable,
};
use std::time::{Duration, Instant};

use kernel::{Message, ToolCall};
use serde_json::{Value, json};

/// The API version header Anthropic asks every caller to send.
const API_VERSION: &str = "2023-06-01";

/// The Messages API requires an output limit; used when the request names none.
const DEFAULT_MAX_TOKENS: u64 = 16_384;

/// Turns the OpenAI-shaped request bytes into the Messages request body.
pub fn to_messages_request(request: &[u8]) -> Result<Vec<u8>, String> {
    let wire: Value =
        serde_json::from_slice(request).map_err(|e| format!("the request is not JSON: {e}"))?;
    let model = wire["model"].as_str().ok_or("the request has no model")?;
    let mut system = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    for message in wire["messages"].as_array().into_iter().flatten() {
        match message["role"].as_str() {
            Some("system") => {
                if let Some(text) = message["content"].as_str() {
                    system.push(text.to_owned());
                }
            }
            Some("user") => push_blocks(
                &mut messages,
                "user",
                vec![text_block(message["content"].as_str().unwrap_or_default())],
            ),
            Some("assistant") => {
                let mut blocks = Vec::new();
                if let Some(text) = message["content"].as_str().filter(|t| !t.is_empty()) {
                    blocks.push(text_block(text));
                }
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    let arguments = call["function"]["arguments"].as_str().unwrap_or("{}");
                    // The model's own text, parsed only to put it where Anthropic wants an object.
                    let input = serde_json::from_str::<Value>(arguments)
                        .ok()
                        .filter(Value::is_object)
                        .unwrap_or_else(|| json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call["id"],
                        "name": call["function"]["name"],
                        "input": input,
                    }));
                }
                if blocks.is_empty() {
                    blocks.push(text_block("(no reply)"));
                }
                push_blocks(&mut messages, "assistant", blocks);
            }
            Some("tool") => push_blocks(
                &mut messages,
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": message["tool_call_id"],
                    "content": message["content"].as_str().unwrap_or_default(),
                })],
            ),
            _ => return Err("a message has an unknown role".to_owned()),
        }
    }
    // The cache breakpoint at the end of the conversation moves forward with it.
    if let Some(last) = messages
        .last_mut()
        .and_then(|m| m["content"].as_array_mut())
        .and_then(|blocks| blocks.last_mut())
    {
        last["cache_control"] = json!({"type": "ephemeral"});
    }
    let mut body = json!({
        "model": model,
        "max_tokens": wire["max_tokens"].as_u64().unwrap_or(DEFAULT_MAX_TOKENS),
    });
    if !system.is_empty() {
        body["system"] = json!([{
            "type": "text",
            "text": system.join("\n\n"),
            "cache_control": {"type": "ephemeral"},
        }]);
    }
    let tools: Vec<Value> = wire["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|tool| {
            json!({
                "name": tool["function"]["name"],
                "description": tool["function"]["description"],
                "input_schema": tool["function"]["parameters"],
            })
        })
        .collect();
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    body["messages"] = Value::Array(messages);
    serde_json::to_vec(&body).map_err(|e| e.to_string())
}

fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

/// Adds blocks to the conversation. The API wants roles to alternate and every tool result to
/// sit in the user message right after the tool call, so same-role neighbours are merged.
fn push_blocks(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if let Some(last) = messages.last_mut()
        && last["role"] == role
        && let Some(existing) = last["content"].as_array_mut()
    {
        // Tool results go first in a user message.
        let results_first = blocks.iter().all(|b| b["type"] == "tool_result");
        if results_first {
            let at = existing
                .iter()
                .take_while(|b| b["type"] == "tool_result")
                .count();
            for (offset, block) in blocks.into_iter().enumerate() {
                existing.insert(at + offset, block);
            }
        } else {
            existing.extend(blocks);
        }
        return;
    }
    messages.push(json!({"role": role, "content": blocks}));
}

/// A Messages reply, read as the program's usual reply.
pub struct Converted {
    pub message: Message,
    pub finish_reason: Option<String>,
    pub response_model: Option<String>,
    /// Usage in the OpenAI shape (`prompt_tokens`, `completion_tokens`, `prompt_tokens_details`).
    pub usage: Option<String>,
}

pub fn from_messages_reply(body: &str) -> Result<Converted, String> {
    let wire: Value =
        serde_json::from_str(body).map_err(|e| format!("reply is not a Messages reply: {e}"))?;
    let blocks = wire["content"]
        .as_array()
        .ok_or("reply has no content blocks")?;
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => text.push_str(block["text"].as_str().unwrap_or_default()),
            Some("tool_use") => tool_calls.push(ToolCall {
                id: block["id"].as_str().unwrap_or_default().to_owned(),
                name: block["name"].as_str().unwrap_or_default().to_owned(),
                arguments: block["input"].to_string(),
            }),
            _ => {} // thinking and other blocks are not part of the chat
        }
    }
    let finish_reason = wire["stop_reason"].as_str().map(|reason| {
        match reason {
            "end_turn" | "stop_sequence" => "stop",
            "max_tokens" => "length",
            "tool_use" => "tool_calls",
            other => other,
        }
        .to_owned()
    });
    Ok(Converted {
        message: Message::Assistant {
            content: (!text.is_empty()).then_some(text),
            tool_calls,
        },
        finish_reason,
        response_model: wire["model"].as_str().map(str::to_owned),
        usage: openai_usage(&wire["usage"]),
    })
}

/// Anthropic counts fresh, written and read input separately; the program's figures expect one
/// prompt total with the cached part named inside it.
fn openai_usage(usage: &Value) -> Option<String> {
    if !usage.is_object() {
        return None;
    }
    let number = |key: &str| usage[key].as_u64().unwrap_or(0);
    let read = number("cache_read_input_tokens");
    let written = number("cache_creation_input_tokens");
    let prompt = number("input_tokens") + read + written;
    Some(
        json!({
            "prompt_tokens": prompt,
            "completion_tokens": number("output_tokens"),
            "total_tokens": prompt + number("output_tokens"),
            "prompt_tokens_details": {"cached_tokens": read, "cache_write_tokens": written},
        })
        .to_string(),
    )
}

/// Sends requests to Anthropic. No `Debug` on purpose: it holds the API key.
#[derive(Clone)]
pub struct AnthropicProvider {
    endpoint: String,
    models_endpoint: String,
    api_key: Option<String>,
    /// Anthropic itself wants `x-api-key`; a router that speaks the same API (OpenRouter's
    /// `/messages`) wants `Authorization: Bearer`.
    bearer: bool,
    served_by: ServedBy,
    agent: ureq::Agent,
}

/// Whether a base URL is Anthropic's own API (the host `api.anthropic.com`).
fn is_anthropic_itself(base_url: &str) -> bool {
    router_host(base_url).is_some_and(|host| host == "api.anthropic.com")
}

impl AnthropicProvider {
    pub fn new(base_url: &str, api_key: Option<&str>, timeout: Duration) -> Self {
        let agent = crate::provider::http_agent(timeout);
        let base = base_url.trim_end_matches('/');
        Self {
            endpoint: format!("{base}/messages"),
            models_endpoint: format!("{base}/models?limit=1000"),
            api_key: api_key.map(str::to_owned),
            bearer: !is_anthropic_itself(base_url),
            served_by: ServedBy {
                router: router_host(base_url),
                ..ServedBy::default()
            },
            agent,
        }
    }

    fn headers<B>(&self, mut builder: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
        builder = builder.header("anthropic-version", API_VERSION);
        if let Some(key) = &self.api_key {
            builder = if self.bearer {
                builder.header("Authorization", format!("Bearer {key}"))
            } else {
                builder.header("x-api-key", key)
            };
        }
        builder
    }

    fn send(&self, request: &[u8]) -> Result<Reply, Box<ProviderError>> {
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
        let body = to_messages_request(request).map_err(|why| fail(why, None, None))?;
        let mut response = self
            .headers(self.agent.post(&self.endpoint))
            .header("Content-Type", "application/json")
            .send(&body[..])
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
                format!("Anthropic answered with status {status}"),
                Some(status),
                Some(raw_body),
            ));
        }
        match from_messages_reply(&raw_body) {
            Ok(converted) => Ok(Reply {
                status,
                raw_body,
                elapsed,
                served_by: self.served_by.clone(),
                message: converted.message,
                finish_reason: converted.finish_reason,
                response_model: converted.response_model,
                usage: converted.usage,
            }),
            Err(message) => Err(fail(message, Some(status), Some(raw_body))),
        }
    }
}

impl Provider for AnthropicProvider {
    fn complete(&self, request: &[u8]) -> Result<Reply, Box<ProviderError>> {
        self.send(request)
    }

    fn complete_cancellable(
        &self,
        request: &[u8],
        cancel: &Cancel,
    ) -> Result<Reply, Box<ProviderError>> {
        let me = self.clone();
        let body = request.to_vec();
        run_cancellable(&self.served_by, cancel, move || me.send(&body))
    }

    fn models(&self) -> Result<Vec<String>, String> {
        let mut response = self
            .headers(self.agent.get(&self.models_endpoint))
            .call()
            .map_err(|e| format!("request failed: {e}"))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("could not read the response body: {e}"))?;
        if !(200..300).contains(&status) {
            return Err(format!(
                "Anthropic answered the model list with status {status}"
            ));
        }
        parse_models(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::fake_router;

    const REQUEST: &str = r#"{"model":"claude-sonnet-5-5","messages":[
        {"role":"system","content":"be brief"},
        {"role":"user","content":"read a.txt and b.txt"},
        {"role":"assistant","content":null,"tool_calls":[
            {"id":"t1","type":"function","function":{"name":"read","arguments":"{\"path\":\"a.txt\"}"}},
            {"id":"t2","type":"function","function":{"name":"read","arguments":"{\"path\":\"b.txt\"}"}}]},
        {"role":"tool","tool_call_id":"t1","content":"AAA"},
        {"role":"tool","tool_call_id":"t2","content":"BBB"}],
        "tools":[{"type":"function","function":{"name":"read","description":"read a file","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}}],
        "stream":false}"#;

    fn converted() -> Value {
        serde_json::from_slice(&to_messages_request(REQUEST.as_bytes()).unwrap()).unwrap()
    }

    #[test]
    fn the_system_prompt_and_tools_move_to_where_anthropic_wants_them() {
        let body = converted();
        assert_eq!(body["model"], "claude-sonnet-5-5");
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(body["system"][0]["text"], "be brief");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert!(body.get("stream").is_none());
        assert!(
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m["role"] != "system")
        );
    }

    #[test]
    fn tool_calls_become_tool_use_and_both_results_share_one_user_message() {
        let body = converted();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["type"], "tool_use");
        assert_eq!(messages[1]["content"][0]["input"]["path"], "a.txt");
        assert_eq!(messages[2]["role"], "user");
        let results = messages[2]["content"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["tool_use_id"], "t1");
        assert_eq!(results[1]["content"], "BBB");
    }

    #[test]
    fn only_the_last_message_gets_a_cache_marker() {
        let body = converted();
        let messages = body["messages"].as_array().unwrap();
        assert!(messages[0]["content"][0].get("cache_control").is_none());
        assert_eq!(
            messages[2]["content"][1]["cache_control"]["type"],
            "ephemeral"
        );
    }

    #[test]
    fn a_user_text_after_tool_results_follows_them_in_the_same_message() {
        let request = r#"{"model":"m","messages":[
            {"role":"user","content":"go"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"t","function":{"name":"x","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"t","content":"ok"},
            {"role":"user","content":"and then?"}],"max_tokens":50}"#;
        let body: Value =
            serde_json::from_slice(&to_messages_request(request.as_bytes()).unwrap()).unwrap();
        assert_eq!(body["max_tokens"], 50);
        let last = &body["messages"][2]["content"];
        assert_eq!(last[0]["type"], "tool_result");
        assert_eq!(last[1]["text"], "and then?");
    }

    #[test]
    fn the_same_request_gives_the_same_bytes() {
        assert_eq!(
            to_messages_request(REQUEST.as_bytes()).unwrap(),
            to_messages_request(REQUEST.as_bytes()).unwrap()
        );
    }

    #[test]
    fn a_reply_with_text_and_a_tool_call_is_read_and_its_usage_reshaped() {
        let body = r#"{"id":"m","type":"message","role":"assistant","model":"claude-sonnet-5-5",
            "content":[{"type":"text","text":"Reading."},{"type":"tool_use","id":"t9","name":"read","input":{"path":"a.txt"}}],
            "stop_reason":"tool_use",
            "usage":{"input_tokens":10,"cache_read_input_tokens":300,"cache_creation_input_tokens":40,"output_tokens":7}}"#;
        let got = from_messages_reply(body).unwrap();
        assert_eq!(got.finish_reason.as_deref(), Some("tool_calls"));
        let Message::Assistant {
            content,
            tool_calls,
        } = got.message
        else {
            panic!("an assistant message");
        };
        assert_eq!(content.as_deref(), Some("Reading."));
        assert_eq!(tool_calls[0].id, "t9");
        assert_eq!(tool_calls[0].arguments, r#"{"path":"a.txt"}"#);
        let usage: Value = serde_json::from_str(&got.usage.unwrap()).unwrap();
        assert_eq!(usage["prompt_tokens"], 350);
        assert_eq!(usage["completion_tokens"], 7);
        assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 300);
        assert_eq!(usage["prompt_tokens_details"]["cache_write_tokens"], 40);
    }

    #[test]
    fn stop_reasons_are_named_as_the_rest_of_the_program_expects() {
        for (anthropic, ours) in [
            ("end_turn", "stop"),
            ("max_tokens", "length"),
            ("tool_use", "tool_calls"),
            ("refusal", "refusal"),
        ] {
            let body = format!(
                r#"{{"content":[{{"type":"text","text":"x"}}],"stop_reason":"{anthropic}"}}"#
            );
            assert_eq!(
                from_messages_reply(&body).unwrap().finish_reason.as_deref(),
                Some(ours)
            );
        }
    }

    #[test]
    fn a_reply_that_is_not_a_messages_reply_says_so() {
        assert!(from_messages_reply("nope").is_err());
        assert!(from_messages_reply(r#"{"choices":[]}"#).is_err());
    }

    #[test]
    fn a_router_that_is_not_anthropic_gets_the_key_as_a_bearer_token() {
        let (base, server) = fake_router(
            "200 OK",
            &[],
            r#"{"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}"#,
        );
        // The loopback test server is not api.anthropic.com.
        AnthropicProvider::new(&base, Some("sk-or-test"), Duration::from_secs(10))
            .complete(REQUEST.as_bytes())
            .expect("a reply");
        let got = server.join().unwrap();
        assert_eq!(got.header("authorization"), Some("Bearer sk-or-test"));
        assert_eq!(got.header("x-api-key"), None);
        assert_eq!(got.header("anthropic-version"), Some(API_VERSION));
    }

    #[test]
    fn only_api_anthropic_com_is_anthropic_itself() {
        assert!(is_anthropic_itself("https://api.anthropic.com/v1"));
        assert!(!is_anthropic_itself("https://openrouter.ai/api/v1"));
        assert!(!is_anthropic_itself("http://127.0.0.1:9/v1"));
    }

    #[test]
    fn it_posts_to_messages_with_the_version_header() {
        let (base, server) = fake_router(
            "200 OK",
            &[],
            r#"{"model":"c","content":[{"type":"text","text":"pong"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":1}}"#,
        );
        let provider = AnthropicProvider::new(&base, Some("sk-test"), Duration::from_secs(10));
        let reply = provider.complete(REQUEST.as_bytes()).expect("a reply");
        let got = server.join().unwrap();
        assert_eq!(got.request_line, "POST /v1/messages HTTP/1.1");
        assert_eq!(got.header("authorization"), Some("Bearer sk-test"));
        assert_eq!(got.header("anthropic-version"), Some(API_VERSION));
        assert_eq!(
            reply.message,
            Message::Assistant {
                content: Some("pong".to_owned()),
                tool_calls: vec![]
            }
        );
    }

    #[test]
    fn an_error_status_keeps_the_body() {
        let (base, server) = fake_router(
            "429 Too Many Requests",
            &[],
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
        );
        let provider = AnthropicProvider::new(&base, None, Duration::from_secs(10));
        let error = provider.complete(REQUEST.as_bytes()).unwrap_err();
        server.join().unwrap();
        assert_eq!(error.status, Some(429));
        assert!(error.raw_body.unwrap().contains("slow down"));
    }
}

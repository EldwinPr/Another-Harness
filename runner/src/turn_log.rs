//! The turn log: one JSONL file per run, one line per call to the model.
//!
//! A record, not the store: chats are saved in SQLite (`store.rs`, decision 08)
//! and this file is a secondary record next to it. A restart begins a fresh
//! log. Each line holds the full request exactly as sent, so the file shows what
//! the model saw. Lines are only ever appended.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::value::RawValue;

use crate::provider::{ProviderError, Reply, ServedBy};

// Rust note: field order here is the order in every log line, and every field
// is always present (missing values are `null`), so the shape never varies.
// `&RawValue` embeds JSON text verbatim instead of re-formatting it.
#[derive(Serialize)]
struct Line<'a> {
    turn: u32,
    /// The assembled request, exactly as sent.
    request: &'a RawValue,
    status: Option<u16>,
    /// The raw reply body as received. Always a JSON string, so even an HTML
    /// error page is stored exactly.
    response: Option<&'a str>,
    response_model: Option<&'a str>,
    finish_reason: Option<&'a str>,
    /// The raw `usage` object as the router sent it.
    usage: Option<&'a RawValue>,
    elapsed_ms: u64,
    served_by: &'a ServedBy,
    error: Option<&'a str>,
    /// How long saving this call to the database took, when a database is in use.
    store_ms: Option<u64>,
}

fn invalid_data(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

fn millis(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

pub struct TurnLog {
    path: PathBuf,
    file: File,
    turns: u32,
}

impl TurnLog {
    /// Creates `<dir>/<UTC timestamp>.jsonl`, making `dir` if needed.
    pub fn create(dir: &Path) -> io::Result<Self> {
        // Rust note: `map_or(0, f)` gives 0 if the clock is before 1970,
        // otherwise applies `f` to the duration.
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Self::create_named(dir, &utc_stamp(secs))
    }

    fn create_named(dir: &Path, stamp: &str) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        // Two runs in the same second must not share a file: `create_new` fails
        // if the name is taken, and we try `-1`, `-2`, ... instead.
        let mut attempt = 0u32;
        loop {
            let name = if attempt == 0 {
                format!("{stamp}.jsonl")
            } else {
                format!("{stamp}-{attempt}.jsonl")
            };
            let path = dir.join(name);
            match OpenOptions::new().append(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file,
                        turns: 0,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => attempt += 1,
                Err(e) => return Err(e),
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many turns have been recorded so far.
    pub fn turns(&self) -> u32 {
        self.turns
    }

    /// Appends one line for a call, successful or not. `request` is the
    /// assembled bytes that were sent; they must be valid JSON, which
    /// `kernel::assemble` always produces.
    pub fn record(
        &mut self,
        request: &[u8],
        result: &Result<Reply, Box<ProviderError>>,
    ) -> io::Result<()> {
        self.record_timed(request, result, None)
    }

    /// Like [`TurnLog::record`], also noting how many milliseconds saving the call to the
    /// database took (the line is written right after that save, so it can hold the time).
    pub fn record_timed(
        &mut self,
        request: &[u8],
        result: &Result<Reply, Box<ProviderError>>,
        store_ms: Option<u64>,
    ) -> io::Result<()> {
        let request_text = std::str::from_utf8(request).map_err(invalid_data)?;
        let request_json: &RawValue = serde_json::from_str(request_text).map_err(invalid_data)?;
        let turn = self.turns + 1;

        let line = match result {
            Ok(reply) => Line {
                turn,
                request: request_json,
                status: Some(reply.status),
                response: Some(&reply.raw_body),
                response_model: reply.response_model.as_deref(),
                finish_reason: reply.finish_reason.as_deref(),
                usage: reply
                    .usage
                    .as_deref()
                    .map(serde_json::from_str::<&RawValue>)
                    .transpose()
                    .map_err(invalid_data)?,
                elapsed_ms: millis(reply.elapsed),
                served_by: &reply.served_by,
                error: None,
                store_ms,
            },
            Err(error) => Line {
                turn,
                request: request_json,
                status: error.status,
                response: error.raw_body.as_deref(),
                response_model: None,
                finish_reason: None,
                usage: None,
                elapsed_ms: millis(error.elapsed),
                served_by: &error.served_by,
                error: Some(&error.message),
                store_ms,
            },
        };

        let mut text = serde_json::to_string(&line).map_err(invalid_data)?;
        text.push('\n');
        self.file.write_all(text.as_bytes())?;
        // Flushed per line so a crash loses at most the call in flight.
        self.file.flush()?;
        self.turns = turn;
        Ok(())
    }
}

/// Formats Unix seconds as `YYYYMMDDTHHMMSSZ` (UTC), e.g. `20261105T084944Z`.
pub fn utc_stamp(secs: u64) -> String {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (year, month, day) = civil_from_days(i64::try_from(days).unwrap_or(i64::MAX / 2));
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Formats Unix seconds for people: `2026-11-05 08:49 UTC`.
pub fn utc_label(secs: u64) -> String {
    // The stamp is `YYYYMMDDTHHMMSSZ`; cut it into pieces.
    let s = utc_stamp(secs);
    format!(
        "{}-{}-{} {}:{} UTC",
        &s[0..4],
        &s[4..6],
        &s[6..8],
        &s[9..11],
        &s[11..13]
    )
}

/// Days since 1970-01-01 to a (year, month, day) in the proleptic Gregorian
/// calendar. Howard Hinnant's well-known integer algorithm; no date library
/// is needed for one file name.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = u32::try_from(day_of_year - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::HttpProvider;
    use crate::testutil::fake_router;
    use crate::tools::testutil::Scratch;
    use kernel::Message;

    const REQUEST: &[u8] =
        br#"{"model":"m","messages":[{"role":"user","content":"hi"}],"stream":false}"#;

    fn sample_reply() -> Reply {
        Reply {
            status: 200,
            raw_body: r#"{"choices":[]}"#.into(),
            elapsed: Duration::from_millis(1234),
            served_by: ServedBy {
                router: Some("localhost:20128".into()),
                provider: None,
                account: None,
            },
            message: Message::Assistant {
                content: Some("pong".into()),
                tool_calls: vec![],
            },
            finish_reason: Some("stop".into()),
            response_model: Some("mimo".into()),
            usage: Some(r#"{"prompt_tokens":5,  "completion_tokens":1}"#.into()),
        }
    }

    fn read_lines(log: &TurnLog) -> Vec<String> {
        std::fs::read_to_string(log.path())
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn stamps_match_known_moments() {
        assert_eq!(utc_stamp(0), "19700101T000000Z");
        // From the 9router reply in issue 2: created 1793868584, Date header
        // "Thu, 05 Nov 2026 08:49:44 GMT".
        assert_eq!(utc_stamp(1_793_868_584), "20261105T084944Z");
        // A leap day, and the second before the next day.
        assert_eq!(utc_stamp(951_782_400), "20000229T000000Z");
        assert_eq!(utc_stamp(951_868_799), "20000229T235959Z");
        assert_eq!(utc_stamp(951_868_800), "20000301T000000Z");
        // A year that is divisible by 100 but not 400 has no leap day.
        assert_eq!(utc_stamp(4_107_542_400), "21000301T000000Z");
    }

    #[test]
    fn a_new_log_is_an_empty_file_with_the_stamp_as_its_name() {
        let dir = Scratch::new("log-name");
        let log = TurnLog::create_named(dir.path(), "20261105T084944Z").unwrap();
        assert_eq!(log.path(), dir.path().join("20261105T084944Z.jsonl"));
        assert_eq!(log.turns(), 0);
        assert!(read_lines(&log).is_empty());
    }

    #[test]
    fn two_logs_in_the_same_second_get_different_files() {
        let dir = Scratch::new("log-clash");
        let first = TurnLog::create_named(dir.path(), "S").unwrap();
        let second = TurnLog::create_named(dir.path(), "S").unwrap();
        let third = TurnLog::create_named(dir.path(), "S").unwrap();
        assert_eq!(first.path().file_name().unwrap(), "S.jsonl");
        assert_eq!(second.path().file_name().unwrap(), "S-1.jsonl");
        assert_eq!(third.path().file_name().unwrap(), "S-2.jsonl");
    }

    #[test]
    fn create_makes_missing_directories() {
        let dir = Scratch::new("log-mkdir");
        let log = TurnLog::create(&dir.path().join("a/b/logs")).unwrap();
        assert!(log.path().exists());
        assert!(log.path().to_string_lossy().ends_with("Z.jsonl"));
    }

    #[test]
    fn a_success_line_has_every_field_and_keeps_raw_text() {
        let dir = Scratch::new("log-success");
        let mut log = TurnLog::create_named(dir.path(), "S").unwrap();
        log.record(REQUEST, &Ok(sample_reply())).unwrap();

        // Readable while the log is still open: lines are flushed as written.
        let lines = read_lines(&log);
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        // Fixed field order, request embedded as JSON, usage text untouched.
        assert!(
            line.starts_with(r#"{"turn":1,"request":{"model":"m","#),
            "{line}"
        );
        assert!(
            line.contains(r#""usage":{"prompt_tokens":5,  "completion_tokens":1}"#),
            "{line}"
        );

        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(value["turn"], 1);
        assert_eq!(value["request"]["messages"][0]["content"], "hi");
        assert_eq!(value["status"], 200);
        assert_eq!(value["response"], r#"{"choices":[]}"#);
        assert_eq!(value["response_model"], "mimo");
        assert_eq!(value["finish_reason"], "stop");
        assert_eq!(value["usage"]["prompt_tokens"], 5);
        assert_eq!(value["elapsed_ms"], 1234);
        assert_eq!(value["served_by"]["router"], "localhost:20128");
        assert!(value["served_by"]["provider"].is_null());
        assert!(value["served_by"]["account"].is_null());
        assert!(value["error"].is_null());
        // No database in use: the field is there, and null.
        assert!(value["store_ms"].is_null());
    }

    #[test]
    fn the_database_write_time_is_noted_when_given() {
        let dir = Scratch::new("log-store-ms");
        let mut log = TurnLog::create_named(dir.path(), "S").unwrap();
        log.record_timed(REQUEST, &Ok(sample_reply()), Some(4))
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&read_lines(&log)[0]).unwrap();
        assert_eq!(value["store_ms"], 4);
    }

    #[test]
    fn a_failure_line_keeps_the_raw_body_exactly() {
        let dir = Scratch::new("log-failure");
        let mut log = TurnLog::create_named(dir.path(), "S").unwrap();
        let body = "<html>\n\"502\" Bad Gateway \\ </html>";
        let error = ProviderError {
            message: "reply is not a chat completion".into(),
            status: Some(200),
            raw_body: Some(body.into()),
            elapsed: Duration::from_millis(7),
            served_by: ServedBy::default(),
        };
        log.record(REQUEST, &Err(Box::new(error))).unwrap();
        let value: serde_json::Value = serde_json::from_str(&read_lines(&log)[0]).unwrap();
        assert_eq!(value["response"], body);
        assert_eq!(value["error"], "reply is not a chat completion");
        assert_eq!(value["status"], 200);
        assert!(value["usage"].is_null());
    }

    #[test]
    fn a_transport_failure_is_logged_with_null_status_and_response() {
        let dir = Scratch::new("log-transport");
        let mut log = TurnLog::create_named(dir.path(), "S").unwrap();
        let error = ProviderError {
            message: "request failed: Connection refused".into(),
            status: None,
            raw_body: None,
            elapsed: Duration::from_millis(1),
            served_by: ServedBy::default(),
        };
        log.record(REQUEST, &Err(Box::new(error))).unwrap();
        let value: serde_json::Value = serde_json::from_str(&read_lines(&log)[0]).unwrap();
        assert!(value["status"].is_null() && value["response"].is_null());
        assert_eq!(value["error"], "request failed: Connection refused");
    }

    #[test]
    fn turns_are_numbered_in_order_one_line_each() {
        let dir = Scratch::new("log-turns");
        let mut log = TurnLog::create_named(dir.path(), "S").unwrap();
        for _ in 0..3 {
            log.record(REQUEST, &Ok(sample_reply())).unwrap();
        }
        assert_eq!(log.turns(), 3);
        let numbers: Vec<u64> = read_lines(&log)
            .iter()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["turn"]
                    .as_u64()
                    .unwrap()
            })
            .collect();
        assert_eq!(numbers, vec![1, 2, 3]);
    }

    #[test]
    fn a_request_that_is_not_json_is_refused_and_nothing_is_written() {
        let dir = Scratch::new("log-badrequest");
        let mut log = TurnLog::create_named(dir.path(), "S").unwrap();
        let err = log.record(b"not json", &Ok(sample_reply())).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(log.turns(), 0);
        assert!(read_lines(&log).is_empty());
    }

    #[test]
    fn a_full_call_through_a_fake_router_is_logged_and_never_contains_the_key() {
        let body = r#"{"choices":[{"finish_reason":"stop","message":{"content":"pong","tool_calls":null}}],"model":"mimo","usage":{"prompt_tokens":9}}"#;
        let (base_url, server) = fake_router("200 OK", &[], body);
        let provider = HttpProvider::new(&base_url, Some("test-key-123"), Duration::from_secs(10));
        let result = provider.complete(REQUEST);
        let got = server.join().unwrap();

        let dir = Scratch::new("log-e2e");
        let mut log = TurnLog::create_named(dir.path(), "S").unwrap();
        log.record(REQUEST, &result).unwrap();

        // The server received exactly the bytes we logged as the request.
        let value: serde_json::Value = serde_json::from_str(&read_lines(&log)[0]).unwrap();
        let logged_request = serde_json::to_vec(&value["request"]).unwrap();
        let sent: serde_json::Value = serde_json::from_slice(&got.body).unwrap();
        assert_eq!(serde_json::to_vec(&sent).unwrap(), logged_request);
        assert_eq!(value["response"], body);
        assert_eq!(value["usage"]["prompt_tokens"], 9);

        let whole_file = std::fs::read_to_string(log.path()).unwrap();
        assert!(!whole_file.contains("test-key-123"));
    }

    #[test]
    fn labels_are_stamps_made_readable() {
        assert_eq!(utc_label(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc_label(1_793_868_584), "2026-11-05 08:49 UTC");
        assert_eq!(utc_label(951_868_799), "2000-02-29 23:59 UTC");
    }
}

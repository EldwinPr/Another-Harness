//! How a tool call looks on screen (issue 17): one collapsed line for the call, one for its result,
//! and the full text kept for `/expand`.
//!
//! Everything here is plain text with no colour; the REPL paints it.

use serde_json::Value;

/// What the program remembers about one tool call of this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRecord {
    pub name: String,
    /// The arguments exactly as the model wrote them.
    pub arguments: String,
    /// The tool's answer; `None` until the tool has run.
    pub result: Option<String>,
}

/// Cuts `text` to `max` characters, ending in `…` when something was dropped.
pub fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// ` (+N lines)` when `text` has more than one line, else nothing.
fn more_lines(text: &str) -> String {
    let extra = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
        .saturating_sub(1);
    match extra {
        0 => String::new(),
        1 => " (+1 line)".to_owned(),
        n => format!(" (+{n} lines)"),
    }
}

/// The call in a few words: `notes/a.txt · 5 bytes`, the first line of a command, a path.
pub fn summary(name: &str, arguments: &str, max: usize) -> String {
    let parsed: Option<Value> = serde_json::from_str(arguments).ok();
    let text = |key: &str| {
        parsed
            .as_ref()
            .and_then(|v| v.get(key))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let shown = match name {
        "bash" => text("command").map(|command| {
            let first = command.lines().next().unwrap_or("").trim().to_owned();
            format!("{first}{}", more_lines(&command))
        }),
        "read_file" | "delete_file" => text("path"),
        "glossary" | "glossary_propose" => text("term"),
        "delegate" => text("profile").zip(text("task")).map(|(profile, task)| {
            let first = task
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim();
            format!("{profile}: {first}")
        }),
        "create_file" | "write_file" => text("path")
            .zip(text("content"))
            .map(|(path, content)| format!("{path} · {} bytes", content.len())),
        "edit_file" => text("path").map(|path| {
            let all = parsed
                .as_ref()
                .and_then(|v| v.get("replace_all"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            format!("{path}{}", if all { " · all places" } else { "" })
        }),
        _ => None,
    };
    let shown = shown.unwrap_or_else(|| arguments.replace('\n', " "));
    cut(&shown, max)
}

/// Whether a tool's answer reads as a failure: an error, a refusal, or a command that exited non-zero.
pub fn failed(result: &str) -> bool {
    let result = result.trim_end();
    if result.starts_with("error:")
        || result.starts_with("denied by user")
        || result.starts_with("plan mode is on")
    {
        return true;
    }
    result
        .lines()
        .last()
        .and_then(|line| line.strip_prefix("[exit code: "))
        .and_then(|rest| rest.strip_suffix(']'))
        .is_some_and(|code| code != "0")
}

/// A command's answer ends in `[exit code: N]`: returns the text before it and N.
fn split_exit(result: &str) -> (&str, Option<&str>) {
    let trimmed = result.trim_end();
    if let Some((body, last)) = trimmed.rsplit_once('\n').or(Some(("", trimmed)))
        && let Some(code) = last
            .strip_prefix("[exit code: ")
            .and_then(|rest| rest.strip_suffix(']'))
    {
        return (body, Some(code));
    }
    (result, None)
}

/// The first line of the answer plus how many more there are. For a command, the first line of
/// its output; a non-zero exit code is added, `[exit code: 0]` is not worth a line.
pub fn result_summary(result: &str, max: usize) -> String {
    let (body, exit) = split_exit(result);
    let suffix = match exit {
        Some(code) if code != "0" => format!(" · exit {code}"),
        _ => String::new(),
    };
    let first = body
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    let first = if first.is_empty() && exit.is_some() {
        "(no output)"
    } else {
        first
    };
    let tail = format!("{}{suffix}", more_lines(body));
    let room = max.saturating_sub(tail.chars().count());
    format!("{}{tail}", cut(first, room))
}

/// Everything about a call, for `/expand`.
pub fn full(number: usize, record: &ToolRecord) -> String {
    let parsed: Option<Value> = serde_json::from_str(&record.arguments).ok();
    let text = |key: &str| {
        parsed
            .as_ref()
            .and_then(|v| v.get(key))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let mut out = format!("#{number} {}\n", record.name);
    match record.name.as_str() {
        "bash" => match text("command") {
            Some(command) => out.push_str(&format!("command:\n{command}\n")),
            None => out.push_str(&format!("arguments:\n{}\n", record.arguments)),
        },
        "create_file" | "write_file" => match (text("path"), text("content")) {
            (Some(path), Some(content)) => {
                out.push_str(&format!(
                    "path: {path}\ncontent ({} bytes):\n{content}\n",
                    content.len()
                ));
            }
            _ => out.push_str(&format!("arguments:\n{}\n", record.arguments)),
        },
        "edit_file" => match (text("path"), text("old_string"), text("new_string")) {
            (Some(path), Some(old), Some(new)) => {
                out.push_str(&format!("path: {path}\nold:\n{old}\nnew:\n{new}\n"));
            }
            _ => out.push_str(&format!("arguments:\n{}\n", record.arguments)),
        },
        "delegate" => match (text("profile"), text("task")) {
            (Some(profile), Some(task)) => {
                out.push_str(&format!("profile: {profile}\ntask:\n{task}\n"));
            }
            _ => out.push_str(&format!("arguments:\n{}\n", record.arguments)),
        },
        "read_file" | "delete_file" => match text("path") {
            Some(path) => out.push_str(&format!("path: {path}\n")),
            None => out.push_str(&format!("arguments:\n{}\n", record.arguments)),
        },
        _ => out.push_str(&format!("arguments:\n{}\n", record.arguments)),
    }
    match &record.result {
        Some(result) => out.push_str(&format!("result:\n{}\n", result.trim_end_matches('\n'))),
        None => out.push_str("result: (not finished)\n"),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edit_shows_its_path_and_the_old_and_new_text_when_expanded() {
        assert_eq!(
            summary(
                "edit_file",
                r#"{"path":"a.txt","old_string":"x","new_string":"y"}"#,
                80
            ),
            "a.txt"
        );
        assert_eq!(
            summary(
                "edit_file",
                r#"{"path":"a.txt","old_string":"x","new_string":"y","replace_all":true}"#,
                80
            ),
            "a.txt · all places"
        );
        let record = ToolRecord {
            name: "edit_file".into(),
            arguments: r#"{"path":"a.txt","old_string":"one","new_string":"two"}"#.into(),
            result: Some("edited a.txt (1 replacement(s), was 3 bytes, now 3 bytes)".into()),
        };
        let shown = full(4, &record);
        assert!(
            shown.contains("path: a.txt\nold:\none\nnew:\ntwo\n"),
            "{shown}"
        );
    }

    #[test]
    fn each_tool_gets_its_own_summary() {
        assert_eq!(
            summary(
                "create_file",
                r#"{"path":"notes/a.txt","content":"hello"}"#,
                80
            ),
            "notes/a.txt · 5 bytes"
        );
        assert_eq!(
            summary("write_file", r#"{"path":"a.txt","content":"héllo"}"#, 80),
            "a.txt · 6 bytes"
        );
        assert_eq!(summary("delete_file", r#"{"path":"a.txt"}"#, 80), "a.txt");
        assert_eq!(
            summary("read_file", r#"{"path":"src/x.rs"}"#, 80),
            "src/x.rs"
        );
        assert_eq!(summary("bash", r#"{"command":"ls -la"}"#, 80), "ls -la");
        assert_eq!(
            summary("glossary", r#"{"term":"kartu stok"}"#, 80),
            "kartu stok"
        );
        assert_eq!(
            summary(
                "glossary_propose",
                r#"{"term":"claim","definition":"d","reason":"r"}"#,
                80
            ),
            "claim"
        );
    }

    #[test]
    fn a_multi_line_command_shows_its_first_line_and_how_many_more() {
        let args = serde_json::json!({ "command": "cat > f <<'EOF'\nline\nline\nEOF" }).to_string();
        assert_eq!(summary("bash", &args, 80), "cat > f <<'EOF' (+3 lines)");
    }

    #[test]
    fn unknown_tools_and_broken_arguments_fall_back_to_the_raw_text_cut_short() {
        assert_eq!(summary("mystery", r#"{"a":1}"#, 80), r#"{"a":1}"#);
        assert_eq!(summary("bash", "not json", 80), "not json");
        assert_eq!(summary("bash", r#"{"nope":1}"#, 80), r#"{"nope":1}"#);
        let long = format!(r#"{{"command":"{}"}}"#, "x".repeat(200));
        let shown = summary("bash", &long, 20);
        assert_eq!(shown.chars().count(), 20);
        assert!(shown.ends_with('…'));
    }

    #[test]
    fn errors_refusals_and_failing_commands_read_as_failures() {
        assert!(failed("error: 'a' already exists"));
        assert!(failed("denied by user: nothing was changed"));
        assert!(failed("oops\n[exit code: 2]"));
        assert!(!failed("fine\n[exit code: 0]"));
        assert!(!failed("created a.txt (5 bytes)"));
    }

    #[test]
    fn a_result_shows_its_first_line_and_the_count_of_the_rest() {
        assert_eq!(
            result_summary("created a.txt (5 bytes)", 80),
            "created a.txt (5 bytes)"
        );
        assert_eq!(
            result_summary("total 8\nfile1\nfile2\n[exit code: 0]\n", 80),
            "total 8 (+2 lines)"
        );
        assert_eq!(result_summary("[exit code: 0]", 80), "(no output)");
        assert_eq!(result_summary("oops\n[exit code: 2]", 80), "oops · exit 2");
        assert_eq!(result_summary("hi\n[exit code: 0]", 80), "hi");
        assert_eq!(result_summary("\n\nsecond\n", 80), "second");
        let long = result_summary(&format!("{}\nmore", "y".repeat(100)), 30);
        assert_eq!(long.chars().count(), 30);
        assert!(long.ends_with("(+1 line)"), "{long}");
        assert_eq!(result_summary("", 80), "");
    }

    #[test]
    fn the_full_text_has_the_content_and_the_result() {
        let record = ToolRecord {
            name: "write_file".into(),
            arguments: r#"{"path":"a.txt","content":"one\ntwo"}"#.into(),
            result: Some("wrote a.txt (7 bytes, was 3)".into()),
        };
        assert_eq!(
            full(3, &record),
            "#3 write_file\npath: a.txt\ncontent (7 bytes):\none\ntwo\nresult:\nwrote a.txt (7 bytes, was 3)\n"
        );
        let bash = ToolRecord {
            name: "bash".into(),
            arguments: r#"{"command":"ls\npwd"}"#.into(),
            result: None,
        };
        assert_eq!(
            full(1, &bash),
            "#1 bash\ncommand:\nls\npwd\nresult: (not finished)\n"
        );
        let odd = ToolRecord {
            name: "mystery".into(),
            arguments: "{}".into(),
            result: Some("x\n".into()),
        };
        assert_eq!(full(2, &odd), "#2 mystery\narguments:\n{}\nresult:\nx\n");
    }
}

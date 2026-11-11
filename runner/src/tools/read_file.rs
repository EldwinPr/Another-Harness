//! `read_file`: read a UTF-8 text file inside the working directory.

use std::path::PathBuf;

use kernel::ToolSpec;
use serde::Deserialize;

use super::{Tool, truncate};

const SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string","description":"File path, relative to the working directory"},"start":{"type":"integer","description":"First line, from 1. Default 1"},"lines":{"type":"integer","description":"Lines to read from start. Default: all, cut to a fixed size"}},"required":["path"]}"#;

// Rust note: `Deserialize` is the reverse of `Serialize`: it lets serde build
// this struct from JSON text. Only `path` is required; `start` and `lines` are
// optional.
#[derive(Deserialize)]
struct Args {
    path: String,
    #[serde(default)]
    start: Option<u64>,
    #[serde(default)]
    lines: Option<u64>,
}

/// How many lines a range reads when only `start` is given.
const DEFAULT_LINES: u64 = 2000;

/// The lines `start..start + count` of `text` (1-based), with a note naming the lines returned and
/// the file's line count, so a model that cannot see the middle of a big file can ask for it.
fn slice(text: &str, start: u64, count: u64) -> String {
    let first = start.max(1);
    let total = text.lines().count() as u64;
    let last = total.min(first.saturating_add(count).saturating_sub(1));
    let body: Vec<&str> = if first > total {
        Vec::new()
    } else {
        text.lines()
            .skip((first - 1) as usize)
            .take(count as usize)
            .collect()
    };
    let mut out = body.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    let note = if first > total {
        format!("[lines: the file has {total}, so start={first} is past its end]")
    } else {
        format!("[lines {first}-{last} of {total}]")
    };
    format!("{out}{note}")
}

pub struct ReadFile {
    spec: ToolSpec,
    root: PathBuf,
}

impl ReadFile {
    /// `root` is the working directory; paths outside it are refused.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            spec: ToolSpec::new("read_file", "Read a UTF-8 text file.", SCHEMA)
                .expect("the schema constant is valid JSON"),
            root: root.into(),
        }
    }
}

impl Tool for ReadFile {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn run(&self, arguments: &str) -> String {
        let args: Args = match serde_json::from_str(arguments) {
            Ok(args) => args,
            Err(e) => return format!("error: invalid arguments: {e}"),
        };
        let root = match self.root.canonicalize() {
            Ok(root) => root,
            Err(e) => return format!("error: working directory is not usable: {e}"),
        };
        // `canonicalize` resolves `..` and symlinks, so the check below cannot
        // be fooled by either. It also fails if the file does not exist. An
        // absolute `path` replaces the root in `join`, and is caught too.
        let full = match root.join(&args.path).canonicalize() {
            Ok(full) => full,
            Err(e) => return format!("error: cannot read '{}': {e}", args.path),
        };
        if !full.starts_with(&root) {
            return format!("error: '{}' is outside the working directory", args.path);
        }
        let bytes = match std::fs::read(&full) {
            Ok(bytes) => bytes,
            Err(e) => return format!("error: cannot read '{}': {e}", args.path),
        };
        match String::from_utf8(bytes) {
            Ok(text) => match (args.start, args.lines) {
                // A whole file: the head and the tail are kept, as before.
                (None, None) => truncate(&text),
                // A range of lines: the middle of a big file can be reached.
                (start, count) => truncate(&slice(
                    &text,
                    start.unwrap_or(1),
                    count.unwrap_or(DEFAULT_LINES),
                )),
            },
            Err(_) => format!("error: '{}' is not valid UTF-8 text", args.path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::MAX_OUTPUT_BYTES;
    use crate::tools::testutil::Scratch;

    fn read(tool: &ReadFile, path: &str) -> String {
        let arguments = serde_json::json!({ "path": path }).to_string();
        tool.run(&arguments)
    }

    #[test]
    fn reads_a_file_inside_the_working_directory() {
        let dir = Scratch::new("read-ok");
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), "nested").unwrap();
        let tool = ReadFile::new(dir.path());
        assert_eq!(read(&tool, "a.txt"), "hello\n");
        assert_eq!(read(&tool, "sub/b.txt"), "nested");
        assert_eq!(read(&tool, "sub/../a.txt"), "hello\n");
    }

    #[test]
    fn long_files_are_truncated() {
        let dir = Scratch::new("read-long");
        std::fs::write(
            dir.path().join("big.txt"),
            "x".repeat(MAX_OUTPUT_BYTES + 500),
        )
        .unwrap();
        let out = read(&ReadFile::new(dir.path()), "big.txt");
        assert!(out.contains("[... 500 bytes omitted ...]"), "{out}");
    }

    #[test]
    fn a_line_range_reaches_the_middle_of_a_big_file() {
        let dir = Scratch::new("read-range");
        let body: String = (1..=2000).map(|n| format!("line {n}\n")).collect();
        std::fs::write(dir.path().join("big.txt"), &body).unwrap();
        let tool = ReadFile::new(dir.path());
        let call = |args: serde_json::Value| tool.run(&args.to_string());
        assert_eq!(
            call(serde_json::json!({"path": "big.txt", "start": 1000, "lines": 2})),
            "line 1000\nline 1001\n[lines 1000-1001 of 2000]"
        );
        let tail = call(serde_json::json!({"path": "big.txt", "start": 1999}));
        assert_eq!(tail, "line 1999\nline 2000\n[lines 1999-2000 of 2000]");
        let past = call(serde_json::json!({"path": "big.txt", "start": 5000}));
        assert_eq!(
            past,
            "[lines: the file has 2000, so start=5000 is past its end]"
        );
        // No range: the whole file, cut the old way when it is long.
        assert!(call(serde_json::json!({"path": "big.txt"})).contains("bytes omitted"));
    }

    #[test]
    fn missing_file_returns_error_text() {
        let dir = Scratch::new("read-missing");
        let out = read(&ReadFile::new(dir.path()), "nope.txt");
        assert!(out.starts_with("error: cannot read 'nope.txt'"), "{out}");
    }

    #[test]
    fn non_utf8_file_returns_error_text() {
        let dir = Scratch::new("read-binary");
        std::fs::write(dir.path().join("bin"), [0xff, 0xfe, 0x00, 0x80]).unwrap();
        let out = read(&ReadFile::new(dir.path()), "bin");
        assert_eq!(out, "error: 'bin' is not valid UTF-8 text");
    }

    #[test]
    fn paths_outside_the_working_directory_are_refused() {
        let outer = Scratch::new("read-outer");
        let inner = outer.path().join("inner");
        std::fs::create_dir(&inner).unwrap();
        std::fs::write(outer.path().join("secret.txt"), "secret").unwrap();
        let tool = ReadFile::new(&inner);
        let outside = outer.path().join("secret.txt");
        for path in ["../secret.txt", outside.to_str().unwrap()] {
            let out = read(&tool, path);
            assert!(
                out.contains("outside the working directory"),
                "{path}: {out}"
            );
            assert!(!out.contains("secret") || out.contains("error"), "{out}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_pointing_outside_is_refused() {
        let outer = Scratch::new("read-symlink");
        let inner = outer.path().join("inner");
        std::fs::create_dir(&inner).unwrap();
        std::fs::write(outer.path().join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(outer.path().join("secret.txt"), inner.join("link")).unwrap();
        let out = read(&ReadFile::new(&inner), "link");
        assert!(out.contains("outside the working directory"), "{out}");
    }

    #[test]
    fn bad_arguments_return_error_text() {
        let tool = ReadFile::new(".");
        assert!(tool.run("not json").starts_with("error: invalid arguments"));
        assert!(tool.run("{}").starts_with("error: invalid arguments"));
    }
}

//! `read_file`: read a UTF-8 text file inside the working directory.

use std::path::PathBuf;

use kernel::ToolSpec;
use serde::Deserialize;

use super::{Tool, truncate};

const SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string","description":"Path of the file to read, relative to the working directory"}},"required":["path"]}"#;

// Rust note: `Deserialize` is the reverse of `Serialize`: it lets serde build
// this struct from JSON text. Only `path` is expected.
#[derive(Deserialize)]
struct Args {
    path: String,
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
            Ok(text) => truncate(&text),
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

//! `grep` and `find` as tools (so a search does not need `bash`): the model asks for text or for
//! paths and gets them back as structured lines, with the results cut the same way every time.
//!
//! Neither changes anything, so neither asks. Both stay inside the working directory: absolute
//! paths and `..` are refused, a link that leads out is not followed, and `.git`, `node_modules`
//! and `target` are not walked. `grep` is a plain text search (not a regular expression: a
//! pattern that looks like one is still a literal), which is enough for finding a name and cheap
//! to reason about.

use std::path::{Component, Path, PathBuf};

use kernel::ToolSpec;
use serde::Deserialize;

use super::{Tool, truncate};

/// How many matches `grep` returns before it stops and says so.
pub const MAX_MATCHES: usize = 200;
/// How many paths `find` returns.
pub const MAX_PATHS: usize = 500;
/// How deep below the working directory either tool looks.
const MAX_DEPTH: usize = 8;
/// Folders neither tool walks.
const SKIPPED: &[&str] = &[".git", "node_modules", "target"];
/// The longest line `grep` shows: a minified file would otherwise fill the result.
const MAX_LINE_CHARS: usize = 300;

const GREP_SCHEMA: &str = r#"{"type":"object","properties":{"pattern":{"type":"string","description":"Text to find, as written"},"path":{"type":"string","description":"File or folder, relative to the working directory. Default: all of it"},"ignore_case":{"type":"boolean","description":"Ignore case"}},"required":["pattern"]}"#;
const FIND_SCHEMA: &str = r#"{"type":"object","properties":{"name":{"type":"string","description":"Part of the file name. Default: every file"},"path":{"type":"string","description":"Folder, relative to the working directory. Default: all of it"},"max":{"type":"integer","description":"Paths to return. Default 200, at most 500"}},"required":[]}"#;

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    ignore_case: bool,
}

#[derive(Deserialize)]
struct FindArgs {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    max: Option<usize>,
}

/// Checks a path argument and returns it relative to the root: no absolute path, no `..`.
fn relative(path: &str) -> Result<&Path, String> {
    if path.trim().is_empty() {
        return Ok(Path::new("."));
    }
    let rel = Path::new(path);
    if rel.is_absolute() {
        return Err(format!(
            "error: '{path}' is an absolute path; use a path relative to the working directory"
        ));
    }
    if rel.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!("error: '{path}' contains '..'"));
    }
    Ok(rel)
}

/// A line cut to [`MAX_LINE_CHARS`], marked when it was cut.
fn short_line(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_owned();
    }
    let cut: String = line.chars().take(MAX_LINE_CHARS).collect();
    format!("{cut}...")
}

/// Every path below `dir` that is a file, as paths relative to `root`, up to `max`, following no
/// link and skipping the folders in [`SKIPPED`] and dot-folders.
fn walk(root: &Path, dir: &Path, depth: usize, max: usize, found: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH || found.len() >= max {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if found.len() >= max {
            return;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        // A link is not followed: it may lead out of the working directory.
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            if !name.starts_with('.') && !SKIPPED.contains(&name.as_str()) {
                walk(root, &entry.path(), depth + 1, max, found);
            }
        } else if kind.is_file()
            && let Ok(relative) = entry.path().strip_prefix(root)
        {
            found.push(relative.to_path_buf());
        }
    }
}

/// The files to read: the one named, or every file below the folder named.
fn targets(root: &Path, path: Option<&str>, max: usize) -> Result<Vec<PathBuf>, String> {
    let rel = relative(path.unwrap_or("."))?;
    let start = root.join(rel);
    let meta = start
        .symlink_metadata()
        .map_err(|e| format!("error: cannot read '{}': {e}", rel.display()))?;
    if meta.is_symlink() {
        return Err(format!(
            "error: '{}' is a link; links are not followed",
            rel.display()
        ));
    }
    if meta.is_file() {
        return Ok(vec![rel.to_path_buf()]);
    }
    if !meta.is_dir() {
        return Err(format!(
            "error: '{}' is not a file or a folder",
            rel.display()
        ));
    }
    let mut found = Vec::new();
    walk(root, &start, 0, max, &mut found);
    Ok(found)
}

pub struct Grep {
    spec: ToolSpec,
    root: PathBuf,
}

impl Grep {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            spec: ToolSpec::new(
                "grep",
                "Search file text in the working directory; returns matching lines as path:line: text. Plain text, not a regular expression. It never asks.",
                GREP_SCHEMA,
            )
            .expect("the schema constant is valid JSON"),
            root: root.into(),
        }
    }

    fn search(&self, arguments: &str) -> Result<String, String> {
        let args: GrepArgs = serde_json::from_str(arguments)
            .map_err(|e| format!("error: invalid arguments: {e}"))?;
        if args.pattern.is_empty() {
            return Err("error: the pattern is empty".to_owned());
        }
        let root = self
            .root
            .canonicalize()
            .map_err(|e| format!("error: working directory is not usable: {e}"))?;
        let files = targets(&root, args.path.as_deref(), MAX_PATHS)?;
        let needle = if args.ignore_case {
            args.pattern.to_lowercase()
        } else {
            args.pattern.clone()
        };
        let mut out: Vec<String> = Vec::new();
        let mut cut_off = false;
        for file in files {
            let Ok(bytes) = std::fs::read(root.join(&file)) else {
                continue;
            };
            let Ok(text) = String::from_utf8(bytes) else {
                continue;
            };
            for (n, line) in text.lines().enumerate() {
                let hits = if args.ignore_case {
                    line.to_lowercase().contains(&needle)
                } else {
                    line.contains(&needle)
                };
                if !hits {
                    continue;
                }
                if out.len() >= MAX_MATCHES {
                    cut_off = true;
                    break;
                }
                out.push(format!(
                    "{}:{}: {}",
                    file.display(),
                    n + 1,
                    short_line(line.trim_end())
                ));
            }
            if cut_off {
                break;
            }
        }
        if out.is_empty() {
            return Ok(format!("no match for '{}'", args.pattern));
        }
        let mut text = out.join("\n");
        if cut_off {
            text.push_str(&format!(
                "\n[stopped at {MAX_MATCHES} matches; narrow the pattern or the path]"
            ));
        } else {
            text.push_str(&format!("\n[{} matches]", out.len()));
        }
        Ok(truncate(&text))
    }
}

impl Tool for Grep {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn run(&self, arguments: &str) -> String {
        self.search(arguments).unwrap_or_else(|text| text)
    }
}

pub struct Find {
    spec: ToolSpec,
    root: PathBuf,
}

impl Find {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            spec: ToolSpec::new(
                "find",
                "List files below a folder by part of their name. Links, dot-folders, .git, node_modules and target are skipped. It never asks.",
                FIND_SCHEMA,
            )
            .expect("the schema constant is valid JSON"),
            root: root.into(),
        }
    }

    fn list(&self, arguments: &str) -> Result<String, String> {
        let args: FindArgs = serde_json::from_str(arguments)
            .map_err(|e| format!("error: invalid arguments: {e}"))?;
        let max = args.max.unwrap_or(MAX_PATHS).min(MAX_PATHS);
        if max == 0 {
            return Err(format!(
                "error: max must be at least 1 and at most {MAX_PATHS}"
            ));
        }
        let root = self
            .root
            .canonicalize()
            .map_err(|e| format!("error: working directory is not usable: {e}"))?;
        let files = targets(&root, args.path.as_deref(), max + 1)?;
        let wanted = args.name.unwrap_or_default();
        let mut shown: Vec<String> = Vec::new();
        for file in &files {
            let text = file.display().to_string();
            if wanted.is_empty() || text.contains(&wanted) {
                shown.push(text);
                if shown.len() > max {
                    break;
                }
            }
        }
        if shown.is_empty() {
            return Ok(format!("no file matching '{wanted}'"));
        }
        let more = shown.len() > max;
        if more {
            shown.truncate(max);
        }
        let mut out = shown.join("\n");
        out.push_str(&format!(
            "\n[{} files{}]",
            shown.len(),
            if more { ", more left out" } else { "" }
        ));
        Ok(truncate(&out))
    }
}

impl Tool for Find {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn run(&self, arguments: &str) -> String {
        self.list(arguments).unwrap_or_else(|text| text)
    }
}

/// `grep` then `find`, in the order the model is told about them.
pub fn search_tools(root: impl Into<PathBuf>) -> Vec<Box<dyn Tool>> {
    let root: PathBuf = root.into();
    vec![Box::new(Grep::new(root.clone())), Box::new(Find::new(root))]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::Scratch;

    fn tree(tag: &str) -> Scratch {
        let dir = Scratch::new(tag);
        std::fs::create_dir_all(dir.path().join("src/deep")).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "fn main() {}\n// Todo here\n").unwrap();
        std::fs::write(dir.path().join("src/deep/b.rs"), "let todo = 1;\n").unwrap();
        std::fs::write(dir.path().join("README.md"), "# Read me\n").unwrap();
        std::fs::write(dir.path().join(".git/config"), "todo\n").unwrap();
        std::fs::write(dir.path().join("target/x.rs"), "todo\n").unwrap();
        dir
    }

    fn call(tool: &dyn Tool, args: serde_json::Value) -> String {
        tool.run(&args.to_string())
    }

    #[test]
    fn grep_finds_lines_ignoring_case_and_skips_git_and_target() {
        let dir = tree("search-grep");
        let grep = Grep::new(dir.path());
        let out = call(
            &grep,
            serde_json::json!({"pattern": "todo", "ignore_case": true}),
        );
        assert_eq!(
            out,
            "src/a.rs:2: // Todo here\nsrc/deep/b.rs:1: let todo = 1;\n[2 matches]"
        );
        let one = call(&grep, serde_json::json!({"pattern": "main", "path": "src"}));
        assert_eq!(one, "src/a.rs:1: fn main() {}\n[1 matches]");
        assert_eq!(
            call(&grep, serde_json::json!({"pattern": "nothing"})),
            "no match for 'nothing'"
        );
    }

    #[test]
    fn grep_is_literal_and_refuses_to_leave_the_working_directory() {
        let dir = tree("search-grep-args");
        let grep = Grep::new(dir.path());
        // A regular expression is just text here, so the dot is a dot.
        assert_eq!(
            call(&grep, serde_json::json!({"pattern": "fn.main"})),
            "no match for 'fn.main'"
        );
        for path in ["/etc", "../x"] {
            let out = call(&grep, serde_json::json!({"pattern": "a", "path": path}));
            assert!(out.starts_with("error:"), "{path}: {out}");
        }
        assert!(call(&grep, serde_json::json!({"pattern": ""})).contains("the pattern is empty"));
    }

    #[test]
    fn find_lists_by_part_of_the_name_and_says_how_many() {
        let dir = tree("search-find");
        let find = Find::new(dir.path());
        assert_eq!(
            call(&find, serde_json::json!({"name": ".rs"})),
            "src/a.rs\nsrc/deep/b.rs\n[2 files]"
        );
        assert_eq!(
            call(&find, serde_json::json!({"path": "src"})),
            "src/a.rs\nsrc/deep/b.rs\n[2 files]"
        );
        assert_eq!(
            call(&find, serde_json::json!({"name": "README"})),
            "README.md\n[1 files]"
        );
        assert_eq!(
            call(&find, serde_json::json!({"name": "nope"})),
            "no file matching 'nope'"
        );
        let capped = call(&find, serde_json::json!({"max": 1}));
        assert_eq!(capped, "README.md\n[1 files, more left out]");
    }

    #[test]
    fn a_link_is_not_followed() {
        let dir = tree("search-link");
        let outside = Scratch::new("search-outside");
        std::fs::write(outside.path().join("secret.rs"), "secret\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let find = Find::new(dir.path());
        let listed = call(&find, serde_json::json!({}));
        assert!(!listed.contains("secret"), "{listed}");
        let grep = Grep::new(dir.path());
        assert_eq!(
            call(&grep, serde_json::json!({"pattern": "secret"})),
            "no match for 'secret'"
        );
        let direct = call(
            &grep,
            serde_json::json!({"pattern": "secret", "path": "link"}),
        );
        assert!(direct.contains("is a link"), "{direct}");
    }

    #[test]
    fn the_search_tools_are_grep_then_find() {
        let dir = tree("search-order");
        let names: Vec<String> = search_tools(dir.path())
            .iter()
            .map(|t| t.spec().name.clone())
            .collect();
        assert_eq!(names, ["grep", "find"]);
    }
}

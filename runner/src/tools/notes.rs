//! `write_note` (issue 34): a subagent leaves a note (a plan, a review) in `.Aharness/tmp/` under the
//! working directory, so the next task can say "read .Aharness/tmp/plan.md".
//!
//! One plain file name, no folders. An existing note of that name is replaced: it is the
//! subagent's own scratch. Never follows or writes through a link.

use std::path::{Path, PathBuf};

use kernel::ToolSpec;
use serde::Deserialize;

use super::Tool;

/// Where notes go, below the working directory.
pub const NOTES_DIR: &str = ".Aharness/tmp";
/// The longest note name, in characters.
pub const MAX_NAME_CHARS: usize = 60;
/// The most bytes one note may hold.
pub const MAX_NOTE_BYTES: usize = 100_000;

const SCHEMA: &str = r#"{"type":"object","properties":{"name":{"type":"string","description":"One file name such as plan.md: letters, digits, . - _, at most 60 characters"},"content":{"type":"string","description":"The text of the note"}},"required":["name","content"]}"#;

#[derive(Deserialize)]
struct Args {
    name: String,
    content: String,
}

pub struct WriteNote {
    spec: ToolSpec,
    root: PathBuf,
}

impl WriteNote {
    /// `root` is the working directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            spec: ToolSpec::new(
                "write_note",
                "Save a note (a plan, a review, a long result) as a file in .Aharness/tmp/ so that it can be read later. An existing note with the same name is replaced. Returns the path.",
                SCHEMA,
            )
            .expect("the schema constant is valid JSON"),
            root: root.into(),
        }
    }
}

fn check_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "error: a note name is one file name of letters, digits, '.', '-' and '_' (at most {MAX_NAME_CHARS} characters, not starting with '.'); got '{name}'"
        ))
    }
}

/// Whether `path` exists and is a link.
fn is_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

impl Tool for WriteNote {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn run(&self, arguments: &str) -> String {
        let args: Args = match serde_json::from_str(arguments) {
            Ok(args) => args,
            Err(e) => return format!("error: invalid arguments: {e}"),
        };
        if let Err(why) = check_name(&args.name) {
            return why;
        }
        if args.content.len() > MAX_NOTE_BYTES {
            return format!(
                "error: the note is {} bytes; the limit is {MAX_NOTE_BYTES}",
                args.content.len()
            );
        }
        let aharness = self.root.join(".Aharness");
        let dir = self.root.join(NOTES_DIR);
        if is_link(&aharness) || is_link(&dir) {
            return "error: .Aharness/tmp is a link, so nothing was written".to_owned();
        }
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return format!("error: cannot create {NOTES_DIR}: {e}");
        }
        // Notes are the program's, not the project's: keep the folder out of the repository's
        // `git status` without touching the repository's own .gitignore.
        let ignore = aharness.join(".gitignore");
        if !ignore.exists() {
            let _ = std::fs::write(&ignore, "*\n");
        }
        let file = dir.join(&args.name);
        if is_link(&file) {
            return format!(
                "error: {NOTES_DIR}/{} is a link, so nothing was written",
                args.name
            );
        }
        match std::fs::write(&file, args.content.as_bytes()) {
            Ok(()) => format!(
                "saved {NOTES_DIR}/{} ({} bytes)",
                args.name,
                args.content.len()
            ),
            Err(e) => format!("error: cannot write {NOTES_DIR}/{}: {e}", args.name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::Scratch;

    fn note(tool: &WriteNote, name: &str, content: &str) -> String {
        tool.run(&serde_json::json!({ "name": name, "content": content }).to_string())
    }

    #[test]
    fn a_note_is_saved_below_aharness_tmp_and_replaced_by_the_same_name() {
        let dir = Scratch::new("notes-save");
        let tool = WriteNote::new(dir.path());
        assert_eq!(
            note(&tool, "plan.md", "one"),
            "saved .Aharness/tmp/plan.md (3 bytes)"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".Aharness/tmp/plan.md")).unwrap(),
            "one"
        );
        note(&tool, "plan.md", "two!");
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".Aharness/tmp/plan.md")).unwrap(),
            "two!"
        );
        // The folder keeps itself out of the project's git status.
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".Aharness/.gitignore")).unwrap(),
            "*\n"
        );
    }

    #[test]
    fn only_a_plain_file_name_is_taken() {
        let dir = Scratch::new("notes-names");
        let tool = WriteNote::new(dir.path());
        for bad in [
            "../x",
            "a/b",
            "",
            ".hidden",
            "sp ace",
            "é.md",
            &"n".repeat(61),
        ] {
            let out = note(&tool, bad, "x");
            assert!(out.starts_with("error: a note name"), "{bad}: {out}");
        }
        assert!(!dir.path().join(".Aharness").exists());
        assert!(note(&tool, &"n".repeat(60), "x").starts_with("saved"));
    }

    #[test]
    fn a_too_big_note_and_bad_arguments_are_refused() {
        let dir = Scratch::new("notes-size");
        let tool = WriteNote::new(dir.path());
        let big = "x".repeat(MAX_NOTE_BYTES + 1);
        assert!(note(&tool, "a.md", &big).starts_with("error: the note is"));
        assert!(tool.run("nonsense").starts_with("error: invalid arguments"));
        assert!(note(&tool, "a.md", &"x".repeat(MAX_NOTE_BYTES)).starts_with("saved"));
    }

    #[test]
    fn a_link_is_never_written_through() {
        let dir = Scratch::new("notes-link");
        let outside = Scratch::new("notes-outside");
        std::fs::create_dir_all(dir.path().join(".Aharness")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join(".Aharness/tmp")).unwrap();
        let tool = WriteNote::new(dir.path());
        assert!(note(&tool, "a.md", "x").contains("is a link"));
        assert!(!outside.path().join("a.md").exists());

        let dir = Scratch::new("notes-link-file");
        std::fs::create_dir_all(dir.path().join(".Aharness/tmp")).unwrap();
        let target = outside.path().join("target.txt");
        std::fs::write(&target, "keep").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join(".Aharness/tmp/a.md")).unwrap();
        let tool = WriteNote::new(dir.path());
        assert!(note(&tool, "a.md", "x").contains("is a link"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
    }
}

//! `create_file`, `write_file`, `edit_file` and `delete_file`: change files inside the working
//! directory.
//!
//! Paths are relative to the working directory. An absolute path or a `..` is refused at once,
//! and the deepest part of the path that exists is resolved (symlinks included) and must still
//! be inside the root, so a link cannot lead out. Every check runs before anything is asked or
//! changed. In `manual` mode the person is asked after the checks pass; a "no" changes nothing.

use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use kernel::ToolSpec;
use serde::Deserialize;

use super::{Approver, Mode, SharedMode, Tool};

/// The most text one `create_file` or `write_file` call may carry.
pub const MAX_CONTENT_BYTES: usize = 1_000_000;

const CONTENT_SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string","description":"Path of the file, relative to the working directory"},"content":{"type":"string","description":"The full UTF-8 text of the file"}},"required":["path","content"]}"#;
const PATH_SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string","description":"Path of the file, relative to the working directory"}},"required":["path"]}"#;

#[derive(Deserialize)]
struct ContentArgs {
    path: String,
    content: String,
}

const EDIT_SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string","description":"Path of the file, relative to the working directory"},"old_string":{"type":"string","description":"The exact text to replace, including spaces and line breaks. Must appear once unless replace_all is true"},"new_string":{"type":"string","description":"The text to put in its place"},"replace_all":{"type":"boolean","description":"Replace every occurrence instead of requiring exactly one"}},"required":["path","old_string","new_string"]}"#;

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[derive(Deserialize)]
struct PathArgs {
    path: String,
}

/// What the file tools share: the root, the approval mode and who to ask.
struct Shared {
    root: PathBuf,
    mode: SharedMode,
    approver: Rc<dyn Approver>,
}

impl Shared {
    /// `true` when the change may go ahead: auto mode, an earlier "always" for this tool, or the
    /// person said yes. `tool` is the name the model called.
    fn allowed(&self, tool: &str, question: &str, detail: &str, full: Option<&str>) -> bool {
        if self.mode.planning() {
            return false;
        }
        if self.mode.mode() == Mode::Auto {
            return true;
        }
        let key = Some((format!("tool:{tool}"), format!("{tool} calls")));
        self.mode
            .confirm(self.approver.as_ref(), question, detail, key, full)
    }

    fn root(&self) -> Result<PathBuf, String> {
        self.root
            .canonicalize()
            .map_err(|e| format!("error: working directory is not usable: {e}"))
    }
}

/// The file tools, in the order the model is told about them.
pub fn file_tools(
    root: impl Into<PathBuf>,
    mode: SharedMode,
    approver: Rc<dyn Approver>,
) -> Vec<Box<dyn Tool>> {
    let shared = Rc::new(Shared {
        root: root.into(),
        mode,
        approver,
    });
    vec![
        Box::new(CreateFile::new(Rc::clone(&shared))),
        Box::new(WriteFile::new(Rc::clone(&shared))),
        Box::new(EditFile::new(Rc::clone(&shared))),
        Box::new(DeleteFile::new(shared)),
    ]
}

/// Checks a path text and returns it split into the parts under the root.
fn relative(path: &str) -> Result<&Path, String> {
    if path.trim().is_empty() {
        return Err("error: the path is empty".to_owned());
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
    // Git runs programs named in .git/config, and the git commands that run unasked would run
    // them, so the file tools do not touch repository internals.
    if rel
        .components()
        .any(|c| matches!(c, Component::Normal(name) if name == ".git"))
    {
        return Err(format!(
            "error: '{path}' is inside .git; the file tools do not change repository internals"
        ));
    }
    Ok(rel)
}

fn outside(path: &str) -> String {
    format!("error: '{path}' is outside the working directory")
}

/// A path that must not exist yet. Returns where to create it: the deepest existing part,
/// resolved and checked to be inside the root, plus the rest.
fn new_path(root: &Path, path: &str) -> Result<PathBuf, String> {
    let rel = relative(path)?;
    let full = root.join(rel);
    if full.symlink_metadata().is_ok() {
        return Err(format!(
            "error: '{path}' already exists; use write_file to replace it"
        ));
    }
    let ancestor = full
        .ancestors()
        .find(|a| a.symlink_metadata().is_ok())
        .unwrap_or(root);
    let resolved = ancestor
        .canonicalize()
        .map_err(|e| format!("error: cannot use '{path}': {e}"))?;
    if !resolved.starts_with(root) {
        return Err(outside(path));
    }
    if !resolved.is_dir() {
        return Err(format!(
            "error: cannot create '{path}': a part of the path is a file"
        ));
    }
    let rest = full.strip_prefix(ancestor).unwrap_or(Path::new(""));
    Ok(resolved.join(rest))
}

/// A path that must be an existing regular file inside the root.
fn existing_file(root: &Path, path: &str, verb: &str) -> Result<PathBuf, String> {
    let rel = relative(path)?;
    let full = root
        .join(rel)
        .canonicalize()
        .map_err(|e| format!("error: cannot {verb} '{path}': {e}"))?;
    if !full.starts_with(root) {
        return Err(outside(path));
    }
    if !full.is_file() {
        return Err(format!("error: '{path}' is not a file"));
    }
    Ok(full)
}

fn too_big(content: &str) -> Option<String> {
    (content.len() > MAX_CONTENT_BYTES).then(|| {
        format!(
            "error: the content is {} bytes; the limit is {MAX_CONTENT_BYTES}",
            content.len()
        )
    })
}

pub struct CreateFile {
    spec: ToolSpec,
    shared: Rc<Shared>,
}

impl CreateFile {
    fn new(shared: Rc<Shared>) -> Self {
        Self {
            spec: ToolSpec::new(
                "create_file",
                "Create a new UTF-8 text file (and any missing folders). Fails if the file already exists.",
                CONTENT_SCHEMA,
            )
            .expect("the schema constant is valid JSON"),
            shared,
        }
    }

    fn create(&self, arguments: &str) -> Result<String, String> {
        let args: ContentArgs = serde_json::from_str(arguments)
            .map_err(|e| format!("error: invalid arguments: {e}"))?;
        if let Some(why) = too_big(&args.content) {
            return Err(why);
        }
        let root = self.shared.root()?;
        let target = new_path(&root, &args.path)?;
        let bytes = args.content.len();
        if !self.shared.allowed(
            "create_file",
            "create this file?",
            &format!("{} ({bytes} bytes)", args.path),
            Some(&args.content),
        ) {
            return Err(self
                .shared
                .mode
                .denial("denied by user: nothing was changed"));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!("error: cannot create the folders for '{}': {e}", args.path)
            })?;
        }
        // `create_new` fails if the file appeared since the check, instead of replacing it.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|e| format!("error: cannot create '{}': {e}", args.path))?;
        file.write_all(args.content.as_bytes())
            .map_err(|e| format!("error: cannot write '{}': {e}", args.path))?;
        Ok(format!("created {} ({bytes} bytes)", args.path))
    }
}

impl Tool for CreateFile {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn run(&self, arguments: &str) -> String {
        self.create(arguments).unwrap_or_else(|text| text)
    }
}

pub struct WriteFile {
    spec: ToolSpec,
    shared: Rc<Shared>,
}

impl WriteFile {
    fn new(shared: Rc<Shared>) -> Self {
        Self {
            spec: ToolSpec::new(
                "write_file",
                "Replace the whole content of an existing UTF-8 text file. Fails if the file does not exist (use create_file).",
                CONTENT_SCHEMA,
            )
            .expect("the schema constant is valid JSON"),
            shared,
        }
    }

    fn write(&self, arguments: &str) -> Result<String, String> {
        let args: ContentArgs = serde_json::from_str(arguments)
            .map_err(|e| format!("error: invalid arguments: {e}"))?;
        if let Some(why) = too_big(&args.content) {
            return Err(why);
        }
        let root = self.shared.root()?;
        let target = match existing_file(&root, &args.path, "write") {
            Ok(target) => target,
            Err(text) if text.contains("No such file") => {
                return Err(format!(
                    "error: '{}' does not exist; use create_file",
                    args.path
                ));
            }
            Err(text) => return Err(text),
        };
        let was = std::fs::metadata(&target).map_or(0, |m| m.len());
        let bytes = args.content.len();
        if !self.shared.allowed(
            "write_file",
            "overwrite this file?",
            &format!("{} ({bytes} bytes, replaces {was})", args.path),
            Some(&args.content),
        ) {
            return Err(self
                .shared
                .mode
                .denial("denied by user: nothing was changed"));
        }
        std::fs::write(&target, args.content.as_bytes())
            .map_err(|e| format!("error: cannot write '{}': {e}", args.path))?;
        Ok(format!("wrote {} ({bytes} bytes, was {was})", args.path))
    }
}

impl Tool for WriteFile {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn run(&self, arguments: &str) -> String {
        self.write(arguments).unwrap_or_else(|text| text)
    }
}

/// `1 place` or `3 places`.
fn places(n: usize) -> String {
    if n == 1 {
        "1 place".to_owned()
    } else {
        format!("{n} places")
    }
}

pub struct EditFile {
    spec: ToolSpec,
    shared: Rc<Shared>,
}

impl EditFile {
    fn new(shared: Rc<Shared>) -> Self {
        Self {
            spec: ToolSpec::new(
                "edit_file",
                "Change part of an existing UTF-8 text file by replacing exact text. Fails, changing nothing, if old_string is not found, or is found more than once and replace_all is not true. Prefer this to write_file for changes.",
                EDIT_SCHEMA,
            )
            .expect("the schema constant is valid JSON"),
            shared,
        }
    }

    fn edit(&self, arguments: &str) -> Result<String, String> {
        let args: EditArgs = serde_json::from_str(arguments)
            .map_err(|e| format!("error: invalid arguments: {e}"))?;
        if args.old_string.is_empty() {
            return Err("error: old_string is empty".to_owned());
        }
        if args.old_string == args.new_string {
            return Err("error: old_string and new_string are the same".to_owned());
        }
        let root = self.shared.root()?;
        let target = match existing_file(&root, &args.path, "edit") {
            Ok(target) => target,
            Err(text) if text.contains("No such file") => {
                return Err(format!(
                    "error: '{}' does not exist; use create_file",
                    args.path
                ));
            }
            Err(text) => return Err(text),
        };
        let bytes = std::fs::read(&target)
            .map_err(|e| format!("error: cannot read '{}': {e}", args.path))?;
        let text = String::from_utf8(bytes)
            .map_err(|_| format!("error: '{}' is not UTF-8 text", args.path))?;
        let found = text.matches(args.old_string.as_str()).count();
        if found == 0 {
            return Err(format!(
                "error: old_string was not found in '{}'; it must match the file exactly, including spaces, indentation and line breaks",
                args.path
            ));
        }
        if found > 1 && !args.replace_all {
            return Err(format!(
                "error: old_string was found in {} of '{}'; add more surrounding text to make it unique, or set replace_all to true",
                places(found),
                args.path
            ));
        }
        let changed = if args.replace_all { found } else { 1 };
        let updated = if args.replace_all {
            text.replace(args.old_string.as_str(), &args.new_string)
        } else {
            text.replacen(args.old_string.as_str(), &args.new_string, 1)
        };
        if let Some(why) = too_big(&updated) {
            return Err(why);
        }
        let (was, now) = (text.len(), updated.len());
        let removed = args.old_string.len() * changed;
        let added = args.new_string.len() * changed;
        let diff = format!("--- old\n{}\n+++ new\n{}", args.old_string, args.new_string);
        if !self.shared.allowed(
            "edit_file",
            "edit this file?",
            &format!(
                "{} ({}, -{removed} +{added} bytes)",
                args.path,
                places(changed)
            ),
            Some(&diff),
        ) {
            return Err(self
                .shared
                .mode
                .denial("denied by user: nothing was changed"));
        }
        std::fs::write(&target, updated.as_bytes())
            .map_err(|e| format!("error: cannot write '{}': {e}", args.path))?;
        Ok(format!(
            "edited {} ({changed} replacement(s), was {was} bytes, now {now} bytes)",
            args.path
        ))
    }
}

impl Tool for EditFile {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn run(&self, arguments: &str) -> String {
        self.edit(arguments).unwrap_or_else(|text| text)
    }
}

pub struct DeleteFile {
    spec: ToolSpec,
    shared: Rc<Shared>,
}

impl DeleteFile {
    fn new(shared: Rc<Shared>) -> Self {
        Self {
            spec: ToolSpec::new(
                "delete_file",
                "Delete one file. Folders are never deleted.",
                PATH_SCHEMA,
            )
            .expect("the schema constant is valid JSON"),
            shared,
        }
    }

    fn delete(&self, arguments: &str) -> Result<String, String> {
        let args: PathArgs = serde_json::from_str(arguments)
            .map_err(|e| format!("error: invalid arguments: {e}"))?;
        let root = self.shared.root()?;
        let rel = relative(&args.path)?;
        let full = root.join(rel);
        // A link is deleted itself, not what it points to, so only the folder holding it
        // is resolved.
        let (Some(parent), Some(name)) = (full.parent(), full.file_name()) else {
            return Err(format!("error: '{}' is not a file", args.path));
        };
        let parent = parent
            .canonicalize()
            .map_err(|e| format!("error: cannot delete '{}': {e}", args.path))?;
        if !parent.starts_with(&root) {
            return Err(outside(&args.path));
        }
        let target = parent.join(name);
        let meta = target
            .symlink_metadata()
            .map_err(|e| format!("error: cannot delete '{}': {e}", args.path))?;
        if meta.is_dir() {
            return Err(format!(
                "error: '{}' is a folder; only files can be deleted",
                args.path
            ));
        }
        if !self.shared.allowed(
            "delete_file",
            "delete this file?",
            &format!("{} ({} bytes)", args.path, meta.len()),
            None,
        ) {
            return Err(self
                .shared
                .mode
                .denial("denied by user: nothing was changed"));
        }
        std::fs::remove_file(&target)
            .map_err(|e| format!("error: cannot delete '{}': {e}", args.path))?;
        Ok(format!("deleted {}", args.path))
    }
}

impl Tool for DeleteFile {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn run(&self, arguments: &str) -> String {
        self.delete(arguments).unwrap_or_else(|text| text)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::tools::testutil::Scratch;
    use crate::tools::{Answer, Ask, shared_mode};

    /// Records every question and answers as told.
    struct Scripted {
        answer: Answer,
        asked: Rc<RefCell<Vec<(String, String)>>>,
    }

    impl Approver for Scripted {
        fn approve(&self, ask: &Ask) -> Answer {
            self.asked
                .borrow_mut()
                .push((ask.question.to_owned(), ask.detail.to_owned()));
            self.answer
        }
    }

    struct Rig {
        dir: Scratch,
        tools: Vec<Box<dyn Tool>>,
        mode: SharedMode,
        asked: Rc<RefCell<Vec<(String, String)>>>,
    }

    fn rig(tag: &str, mode: Mode, answer: Answer) -> Rig {
        let dir = Scratch::new(tag);
        let asked = Rc::new(RefCell::new(Vec::new()));
        let mode = shared_mode(mode);
        let tools = file_tools(
            dir.path(),
            Rc::clone(&mode),
            Rc::new(Scripted {
                answer,
                asked: Rc::clone(&asked),
            }),
        );
        Rig {
            dir,
            tools,
            mode,
            asked,
        }
    }

    impl Rig {
        fn call(&self, name: &str, args: serde_json::Value) -> String {
            let tool = self
                .tools
                .iter()
                .find(|t| t.spec().name == name)
                .expect("tool exists");
            tool.run(&args.to_string())
        }

        fn create(&self, path: &str, content: &str) -> String {
            self.call(
                "create_file",
                serde_json::json!({ "path": path, "content": content }),
            )
        }

        fn write(&self, path: &str, content: &str) -> String {
            self.call(
                "write_file",
                serde_json::json!({ "path": path, "content": content }),
            )
        }

        fn delete(&self, path: &str) -> String {
            self.call("delete_file", serde_json::json!({ "path": path }))
        }

        fn file(&self, path: &str) -> PathBuf {
            self.dir.path().join(path)
        }
    }

    #[test]
    fn the_file_tools_leave_git_internals_alone() {
        let r = rig("files-git", Mode::Auto, Answer::Yes);
        std::fs::create_dir_all(r.dir.path().join(".git")).unwrap();
        std::fs::write(r.dir.path().join(".git/config"), "[core]\n").unwrap();
        for path in [".git/config", "sub/.git/hooks/pre-commit"] {
            let made = r.call(
                "create_file",
                serde_json::json!({ "path": path, "content": "x" }),
            );
            assert!(made.contains("inside .git"), "{path}: {made}");
            let written = r.call(
                "write_file",
                serde_json::json!({ "path": path, "content": "x" }),
            );
            assert!(written.contains("inside .git"), "{path}: {written}");
            assert!(r.edit(path, "a", "b").contains("inside .git"), "{path}");
            let deleted = r.call("delete_file", serde_json::json!({ "path": path }));
            assert!(deleted.contains("inside .git"), "{path}: {deleted}");
        }
        assert_eq!(
            std::fs::read_to_string(r.dir.path().join(".git/config")).unwrap(),
            "[core]\n"
        );
        // Names that only start like it are fine.
        let ok = r.call(
            "create_file",
            serde_json::json!({ "path": ".gitignore", "content": "x" }),
        );
        assert!(!ok.starts_with("error"), "{ok}");
    }

    #[test]
    fn the_file_tools_are_named_for_the_model() {
        let r = rig("files-names", Mode::Auto, Answer::Yes);
        let names: Vec<&str> = r.tools.iter().map(|t| t.spec().name.as_str()).collect();
        assert_eq!(
            names,
            ["create_file", "write_file", "edit_file", "delete_file"]
        );
    }

    impl Rig {
        fn edit(&self, path: &str, old: &str, new: &str) -> String {
            self.call(
                "edit_file",
                serde_json::json!({ "path": path, "old_string": old, "new_string": new }),
            )
        }

        fn edit_all(&self, path: &str, old: &str, new: &str) -> String {
            self.call(
                "edit_file",
                serde_json::json!({
                    "path": path, "old_string": old, "new_string": new, "replace_all": true
                }),
            )
        }
    }

    #[test]
    fn edit_replaces_one_exact_piece_and_leaves_the_rest() {
        let r = rig("files-edit", Mode::Auto, Answer::Yes);
        r.create("a.txt", "one\ntwo\nthree\n");
        assert_eq!(
            r.edit("a.txt", "two\n", "2\n"),
            "edited a.txt (1 replacement(s), was 14 bytes, now 12 bytes)"
        );
        assert_eq!(
            std::fs::read_to_string(r.file("a.txt")).unwrap(),
            "one\n2\nthree\n"
        );
    }

    #[test]
    fn edit_needs_a_unique_match_unless_replace_all_is_set() {
        let r = rig("files-edit-many", Mode::Auto, Answer::Yes);
        r.create("a.txt", "x y x y x");
        let out = r.edit("a.txt", "x", "z");
        assert!(
            out.starts_with("error: old_string was found in 3 places of 'a.txt'"),
            "{out}"
        );
        assert_eq!(
            std::fs::read_to_string(r.file("a.txt")).unwrap(),
            "x y x y x"
        );
        assert_eq!(
            r.edit_all("a.txt", "x", "zz"),
            "edited a.txt (3 replacement(s), was 9 bytes, now 12 bytes)"
        );
        assert_eq!(
            std::fs::read_to_string(r.file("a.txt")).unwrap(),
            "zz y zz y zz"
        );
    }

    #[test]
    fn edit_that_cannot_match_or_makes_no_sense_changes_nothing() {
        let r = rig("files-edit-bad", Mode::Auto, Answer::Yes);
        r.create("a.txt", "hello");
        assert!(
            r.edit("a.txt", "bye", "x")
                .starts_with("error: old_string was not found in 'a.txt'")
        );
        assert!(r.edit("a.txt", "hello", "hello").contains("are the same"));
        assert!(r.edit("a.txt", "", "x").contains("old_string is empty"));
        assert_eq!(
            r.edit("missing.txt", "a", "b"),
            "error: 'missing.txt' does not exist; use create_file"
        );
        assert!(r.edit("../a.txt", "a", "b").contains("'..'"));
        assert!(r.edit("/etc/hostname", "a", "b").contains("absolute"));
        assert_eq!(std::fs::read_to_string(r.file("a.txt")).unwrap(), "hello");
        // Whitespace has to match exactly.
        r.create("b.txt", "    indented\n");
        assert!(r.edit("b.txt", "  indented", "x").starts_with("edited"));
        r.create("c.txt", "    indented\n");
        assert!(
            r.edit("b.txt", "\tindented", "x")
                .starts_with("error: old_string was not found")
        );
    }

    #[test]
    fn edit_refuses_a_binary_file_a_folder_a_link_out_and_a_result_that_is_too_big() {
        let r = rig("files-edit-refuse", Mode::Auto, Answer::Yes);
        std::fs::write(r.file("bin.dat"), [0xff, 0xfe, 0x00]).unwrap();
        assert!(r.edit("bin.dat", "a", "b").contains("not UTF-8"));
        std::fs::create_dir(r.file("dir")).unwrap();
        assert!(r.edit("dir", "a", "b").contains("not a file"));
        let outside = Scratch::new("files-edit-outside");
        std::fs::write(outside.path().join("o.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(outside.path().join("o.txt"), r.file("leak.txt")).unwrap();
        assert!(
            r.edit("leak.txt", "secret", "x")
                .contains("outside the working directory")
        );
        assert_eq!(
            std::fs::read_to_string(outside.path().join("o.txt")).unwrap(),
            "secret"
        );
        r.create(
            "big.txt",
            &format!("X{}", "a".repeat(MAX_CONTENT_BYTES - 2)),
        );
        let out = r.edit("big.txt", "X", &"b".repeat(10));
        assert!(out.starts_with("error: the content is"), "{out}");
    }

    #[test]
    fn edit_asks_in_manual_mode_and_a_no_changes_nothing() {
        let r = rig("files-edit-ask", Mode::Manual, Answer::No);
        std::fs::write(r.file("a.txt"), "old text").unwrap();
        assert_eq!(
            r.edit("a.txt", "old", "new"),
            "denied by user: nothing was changed"
        );
        assert_eq!(
            std::fs::read_to_string(r.file("a.txt")).unwrap(),
            "old text"
        );
        assert_eq!(
            r.asked.borrow().as_slice(),
            [(
                "edit this file?".to_owned(),
                "a.txt (1 place, -3 +3 bytes)".to_owned()
            )]
        );
    }

    #[test]
    fn edit_runs_unasked_in_auto_mode_and_always_is_per_tool() {
        let r = rig("files-edit-auto", Mode::Auto, Answer::No);
        std::fs::write(r.file("a.txt"), "old").unwrap();
        assert!(r.edit("a.txt", "old", "new").starts_with("edited"));
        assert!(r.asked.borrow().is_empty());
    }

    #[test]
    fn create_makes_a_file_and_its_folders() {
        let r = rig("files-create", Mode::Auto, Answer::Yes);
        assert_eq!(r.create("a.txt", "hello"), "created a.txt (5 bytes)");
        assert_eq!(
            r.create("notes/deep/b.txt", "x"),
            "created notes/deep/b.txt (1 bytes)"
        );
        assert_eq!(std::fs::read_to_string(r.file("a.txt")).unwrap(), "hello");
        assert!(r.file("notes/deep/b.txt").is_file());
    }

    #[test]
    fn create_refuses_an_existing_file_and_leaves_it_alone() {
        let r = rig("files-exists", Mode::Auto, Answer::Yes);
        r.create("a.txt", "first");
        let out = r.create("a.txt", "second");
        assert!(out.starts_with("error: 'a.txt' already exists"), "{out}");
        assert_eq!(std::fs::read_to_string(r.file("a.txt")).unwrap(), "first");
    }

    #[test]
    fn create_refuses_a_path_through_a_file() {
        let r = rig("files-through", Mode::Auto, Answer::Yes);
        r.create("a.txt", "x");
        let out = r.create("a.txt/b.txt", "y");
        assert!(out.starts_with("error:"), "{out}");
        assert!(r.file("a.txt").is_file());
    }

    #[test]
    fn write_replaces_an_existing_file_only() {
        let r = rig("files-write", Mode::Auto, Answer::Yes);
        let missing = r.write("a.txt", "x");
        assert_eq!(missing, "error: 'a.txt' does not exist; use create_file");
        assert!(!r.file("a.txt").exists());
        r.create("a.txt", "old text");
        assert_eq!(r.write("a.txt", "new"), "wrote a.txt (3 bytes, was 8)");
        assert_eq!(std::fs::read_to_string(r.file("a.txt")).unwrap(), "new");
    }

    #[test]
    fn write_refuses_a_folder() {
        let r = rig("files-write-dir", Mode::Auto, Answer::Yes);
        std::fs::create_dir(r.file("d")).unwrap();
        assert_eq!(r.write("d", "x"), "error: 'd' is not a file");
    }

    #[test]
    fn delete_removes_one_file() {
        let r = rig("files-delete", Mode::Auto, Answer::Yes);
        r.create("a.txt", "x");
        assert_eq!(r.delete("a.txt"), "deleted a.txt");
        assert!(!r.file("a.txt").exists());
        let again = r.delete("a.txt");
        assert!(again.starts_with("error: cannot delete 'a.txt'"), "{again}");
    }

    #[test]
    fn delete_refuses_folders_and_the_root() {
        let r = rig("files-delete-dir", Mode::Auto, Answer::Yes);
        std::fs::create_dir(r.file("d")).unwrap();
        assert_eq!(
            r.delete("d"),
            "error: 'd' is a folder; only files can be deleted"
        );
        assert!(r.file("d").is_dir());
        for path in [".", "", "./"] {
            let out = r.delete(path);
            assert!(out.starts_with("error:"), "{path:?}: {out}");
        }
        assert!(r.dir.path().is_dir());
    }

    #[test]
    fn delete_removes_a_link_not_its_target() {
        let r = rig("files-delete-link", Mode::Auto, Answer::Yes);
        r.create("real.txt", "keep");
        std::os::unix::fs::symlink(r.file("real.txt"), r.file("link")).unwrap();
        assert_eq!(r.delete("link"), "deleted link");
        assert!(r.file("real.txt").is_file());
        assert!(r.file("link").symlink_metadata().is_err());
    }

    #[test]
    fn paths_outside_the_root_are_refused_and_nothing_is_changed() {
        let r = rig("files-escape", Mode::Auto, Answer::Yes);
        let outside_dir = Scratch::new("files-escape-outside");
        let victim = outside_dir.path().join("victim.txt");
        std::fs::write(&victim, "keep").unwrap();
        let abs = victim.display().to_string();

        for path in [abs.as_str(), "../victim.txt", "a/../../victim.txt", ""] {
            for out in [r.create(path, "x"), r.write(path, "x"), r.delete(path)] {
                assert!(out.starts_with("error:"), "{path:?}: {out}");
            }
        }
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
    }

    #[test]
    fn a_link_cannot_lead_out_of_the_root() {
        let r = rig("files-link-escape", Mode::Auto, Answer::Yes);
        let outside_dir = Scratch::new("files-link-escape-outside");
        std::fs::write(outside_dir.path().join("victim.txt"), "keep").unwrap();
        std::os::unix::fs::symlink(outside_dir.path(), r.file("out")).unwrap();
        std::os::unix::fs::symlink(outside_dir.path().join("victim.txt"), r.file("lnk")).unwrap();

        let results = [
            r.create("out/new.txt", "x"),
            r.create("out/deep/new.txt", "x"),
            r.write("out/victim.txt", "x"),
            r.write("lnk", "x"),
            r.delete("out/victim.txt"),
        ];
        for out in results {
            assert!(out.contains("outside the working directory"), "{out}");
        }
        assert_eq!(
            std::fs::read_to_string(outside_dir.path().join("victim.txt")).unwrap(),
            "keep"
        );
        assert!(!outside_dir.path().join("new.txt").exists());
        assert!(!outside_dir.path().join("deep").exists());
    }

    #[test]
    fn a_refused_create_makes_no_folders() {
        let r = rig("files-no-folders", Mode::Auto, Answer::Yes);
        let big = "x".repeat(MAX_CONTENT_BYTES + 1);
        let out = r.create("new/dir/a.txt", &big);
        assert!(out.starts_with("error: the content is"), "{out}");
        assert!(!r.file("new").exists());
    }

    #[test]
    fn content_over_the_limit_is_refused_and_the_limit_itself_is_fine() {
        let r = rig("files-big", Mode::Auto, Answer::Yes);
        let at_limit = "x".repeat(MAX_CONTENT_BYTES);
        assert!(r.create("a.txt", &at_limit).starts_with("created"));
        let over = "x".repeat(MAX_CONTENT_BYTES + 1);
        assert!(r.write("a.txt", &over).starts_with("error: the content is"));
        assert_eq!(
            std::fs::metadata(r.file("a.txt")).unwrap().len() as usize,
            MAX_CONTENT_BYTES
        );
    }

    #[test]
    fn bad_arguments_come_back_as_text() {
        let r = rig("files-args", Mode::Auto, Answer::Yes);
        for name in ["create_file", "write_file", "delete_file"] {
            let out = r.call(name, serde_json::json!({ "nope": 1 }));
            assert!(out.starts_with("error: invalid arguments"), "{name}: {out}");
        }
    }

    #[test]
    fn manual_mode_asks_before_each_change_and_shows_what_it_is() {
        let r = rig("files-manual", Mode::Manual, Answer::Yes);
        r.create("a.txt", "hello");
        r.write("a.txt", "hi");
        r.delete("a.txt");
        assert_eq!(
            *r.asked.borrow(),
            vec![
                ("create this file?".to_owned(), "a.txt (5 bytes)".to_owned()),
                (
                    "overwrite this file?".to_owned(),
                    "a.txt (2 bytes, replaces 5)".to_owned()
                ),
                ("delete this file?".to_owned(), "a.txt (2 bytes)".to_owned()),
            ]
        );
        assert!(!r.file("a.txt").exists());
    }

    #[test]
    fn a_no_changes_nothing() {
        let r = rig("files-no", Mode::Manual, Answer::No);
        assert_eq!(
            r.create("a.txt", "x"),
            "denied by user: nothing was changed"
        );
        assert!(!r.file("a.txt").exists());

        std::fs::write(r.file("b.txt"), "old").unwrap();
        assert_eq!(
            r.write("b.txt", "new"),
            "denied by user: nothing was changed"
        );
        assert_eq!(r.delete("b.txt"), "denied by user: nothing was changed");
        assert_eq!(std::fs::read_to_string(r.file("b.txt")).unwrap(), "old");
    }

    #[test]
    fn auto_mode_never_asks() {
        let r = rig("files-auto", Mode::Auto, Answer::No);
        r.create("a.txt", "x");
        r.write("a.txt", "y");
        r.delete("a.txt");
        assert!(r.asked.borrow().is_empty());
        assert!(!r.file("a.txt").exists());
    }

    #[test]
    fn a_failing_call_is_not_asked_about() {
        let r = rig("files-no-ask-on-error", Mode::Manual, Answer::Yes);
        r.write("missing.txt", "x");
        r.delete("missing.txt");
        r.create("../x", "x");
        assert!(r.asked.borrow().is_empty());
    }

    #[test]
    fn switching_the_mode_takes_effect_on_the_next_call() {
        let r = rig("files-switch", Mode::Manual, Answer::Yes);
        r.create("a.txt", "x");
        assert_eq!(r.asked.borrow().len(), 1);
        r.mode.set_mode(Mode::Auto);
        r.write("a.txt", "y");
        assert_eq!(r.asked.borrow().len(), 1);
    }

    #[test]
    fn modes_parse_by_name() {
        assert_eq!(Mode::parse("manual"), Some(Mode::Manual));
        assert_eq!(Mode::parse(" Auto "), Some(Mode::Auto));
        assert_eq!(Mode::parse("tools"), None);
        assert_eq!(Mode::parse(""), None);
    }
}

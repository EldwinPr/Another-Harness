//! Subagents (issue 33): the `delegate(task, profile)` tool and the pieces around it.
//!
//! The model hands one task to a subagent that runs under one of the user's profiles, in a chat
//! of its own, in the background; only the final answer is added to the main chat later. A subagent starts with the `subagent` prompt
//! in place of its profile's `base` (see `Store::start_of_subagent`), is offered only the tools
//! that look, and has nobody to ask. The starting of it is [`crate::agent::Agent::delegate`]; this
//! file holds what does not need the agent: the tool's description, the limits, the child's tool
//! list and the wording of results.

use std::path::{Path, PathBuf};

use kernel::ToolSpec;
use serde::Deserialize;

use std::rc::Rc;

use crate::cancel::Cancel;

use crate::tools::{
    Approval, Bash, DEFAULT_TIMEOUT, Mode, ReadFile, Tool, WriteNote, context_read_tool,
    file_tools, glossary_lookup_tool, shared_mode,
};

/// The tool's name.
pub const DELEGATE: &str = "delegate";

/// Model calls a subagent may make for one task.
pub const MAX_CALLS: u32 = 15;
/// Characters of a subagent's answer that reach the main chat.
pub const MAX_ANSWER_CHARS: usize = 6_000;
/// Delegations the model may make while answering one user message.
pub const MAX_PER_MESSAGE: u32 = 4;

const SCHEMA: &str = r#"{"type":"object","properties":{"task":{"type":"string","description":"The whole task, self-contained: the subagent sees nothing of this chat. Say what to find out and what to report."},"profile":{"type":"string","description":"The name of the profile the subagent runs under"}},"required":["task","profile"]}"#;

const DESCRIPTION: &str = "Hand one task to a subagent that runs in the background: this call returns at once, the user can keep chatting, and the subagent's final answer is added to the chat as a message when the user sends their next message after it has finished. Do not wait for it: tell the user it is running and end your turn. The subagent has a chat of its own, runs under the profile you name, and cannot ask the user. It can read files, run looking commands, look up the glossary and save notes in .Aharness/tmp/; its long results are saved there, so a later task can say: read .Aharness/tmp/plan.md. It can also create, edit and delete files when the user's approval mode is auto; in manual mode it is a reader and its result says (reader). Write the task so it stands alone.";

/// The tool's description for the model. Fixed text, so the start of a chat never changes when
/// the profiles do.
pub fn delegate_spec() -> ToolSpec {
    ToolSpec::new(DELEGATE, DESCRIPTION, SCHEMA).expect("the schema constant is valid JSON")
}

#[derive(Deserialize)]
pub struct Args {
    pub task: String,
    pub profile: String,
}

/// Reads the arguments of a `delegate` call.
pub fn parse(arguments: &str) -> Result<Args, String> {
    let args: Args =
        serde_json::from_str(arguments).map_err(|e| format!("error: invalid arguments: {e}"))?;
    if args.task.trim().is_empty() {
        return Err("error: the task is empty".to_owned());
    }
    if args.profile.trim().is_empty() {
        return Err("error: no profile given".to_owned());
    }
    Ok(args)
}

/// What kind of subagent to build (issue 34), decided when it starts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Kit {
    /// May change files: the approval mode was `auto`.
    pub coder: bool,
    /// What the owner had answered "always" to in the chat, as `(identity, label)`: a coder's
    /// `bash` may run those commands too.
    pub always: Vec<(String, String)>,
}

/// The tools of a subagent. A reader has `read_file`, a `bash` that only runs looking commands,
/// `glossary` and `write_note`. A coder also has the four file tools (never asking), and its `bash`
/// also runs what the owner allowed with "always". Nothing proposes or delegates.
pub fn tools(cwd: &Path, db: &Path, cancel: &Cancel, kit: &Kit) -> Vec<Box<dyn Tool>> {
    let cwd: PathBuf = cwd.to_path_buf();
    let mut list: Vec<Box<dyn Tool>> = vec![Box::new(ReadFile::new(&cwd))];
    let bash = Bash::new(&cwd, DEFAULT_TIMEOUT, Box::new(NobodyToAsk)).with_cancel(cancel.clone());
    if kit.coder {
        list.extend(file_tools(
            &cwd,
            shared_mode(Mode::Auto),
            Rc::new(NobodyToAsk),
        ));
        list.push(Box::new(bash.with_approval(Approval::with_always(
            Mode::Manual,
            kit.always.clone(),
        ))));
    } else {
        list.push(Box::new(bash.looking_only()));
    }
    list.push(glossary_lookup_tool(db, cwd.display().to_string()));
    list.extend(context_read_tool(db, cwd.display().to_string()));
    list.push(Box::new(WriteNote::new(&cwd)));
    list
}

/// Answers every question with no. A looking-only `bash` never asks; this is the belt to that
/// pair of braces.
pub struct NobodyToAsk;
impl crate::tools::Approver for NobodyToAsk {
    fn approve(&self, _ask: &crate::tools::Ask) -> crate::tools::Answer {
        crate::tools::Answer::No
    }
}

/// The answer cut to [`MAX_ANSWER_CHARS`], with a note when something was dropped.
pub fn cut_answer(answer: &str) -> String {
    let answer = answer.trim();
    if answer.chars().count() <= MAX_ANSWER_CHARS {
        return answer.to_owned();
    }
    let kept: String = answer.chars().take(MAX_ANSWER_CHARS).collect();
    format!(
        "{kept}\n[answer cut: {} of {} characters shown]",
        MAX_ANSWER_CHARS,
        answer.chars().count()
    )
}

/// The line that ends every result: who ran, what it cost, where its chat is.
pub fn footer(
    profile: &str,
    coder: bool,
    calls: u32,
    prompt_tokens: u64,
    chat: Option<i64>,
) -> String {
    let chat = chat.map_or(String::new(), |id| format!(", chat {id}"));
    let kind = if coder { "coder" } else { "reader" };
    format!(
        "[subagent {profile} ({kind}): {calls} call{}, {} prompt tokens{chat}]",
        if calls == 1 { "" } else { "s" },
        crate::web_usage::compact(i64::try_from(prompt_tokens).unwrap_or(i64::MAX))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_arguments_need_a_task_and_a_profile() {
        assert!(parse(r#"{"task":"look","profile":"planner"}"#).is_ok());
        assert!(parse(r#"{"task":"  ","profile":"planner"}"#).is_err());
        assert!(parse(r#"{"task":"look","profile":""}"#).is_err());
        assert!(parse("nonsense").is_err());
    }

    #[test]
    fn a_long_answer_is_cut_with_a_note() {
        let long = "x".repeat(MAX_ANSWER_CHARS + 50);
        let cut = cut_answer(&long);
        assert!(cut.contains("[answer cut: 6000 of 6050 characters shown]"));
        assert_eq!(cut_answer("  short \n"), "short");
    }

    #[test]
    fn the_footer_names_the_profile_the_calls_and_the_chat() {
        assert_eq!(
            footer("planner", false, 4, 21_300, Some(14)),
            "[subagent planner (reader): 4 calls, 21.3k prompt tokens, chat 14]"
        );
        assert_eq!(
            footer("builder", true, 1, 900, None),
            "[subagent builder (coder): 1 call, 900 prompt tokens]"
        );
    }

    #[test]
    fn a_reader_has_four_tools_and_cannot_change_the_project() {
        let dir = crate::tools::testutil::Scratch::new("subagent-tools");
        let list = tools(
            dir.path(),
            &dir.path().join("x.db"),
            &Cancel::new(),
            &Kit::default(),
        );
        let names: Vec<&str> = list.iter().map(|t| t.spec().name.as_str()).collect();
        assert_eq!(names, ["read_file", "bash", "glossary", "write_note"]);
        let bash = &list[1];
        let refused = bash.run(r#"{"command":"touch made.txt"}"#);
        assert!(
            refused.starts_with("error: a subagent can only look"),
            "{refused}"
        );
        assert!(!dir.path().join("made.txt").exists());
        let refused = bash.run(r#"{"command":"mkdir made"}"#);
        assert!(refused.starts_with("error:"), "{refused}");
        assert!(!dir.path().join("made").exists());
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        let ran = bash.run(r#"{"command":"cat a.txt"}"#);
        assert!(ran.contains("hello"), "{ran}");
        // The one thing it can write is a note.
        let saved = list[3].run(r#"{"name":"n.md","content":"x"}"#);
        assert!(saved.starts_with("saved .Aharness/tmp/n.md"), "{saved}");
    }

    #[test]
    fn a_coder_changes_files_without_asking_and_runs_only_what_was_allowed() {
        let dir = crate::tools::testutil::Scratch::new("subagent-coder-tools");
        let kit = Kit {
            coder: true,
            always: vec![("bash:touch".to_owned(), "`touch` commands".to_owned())],
        };
        let list = tools(dir.path(), &dir.path().join("x.db"), &Cancel::new(), &kit);
        let names: Vec<&str> = list.iter().map(|t| t.spec().name.as_str()).collect();
        assert_eq!(
            names,
            [
                "read_file",
                "create_file",
                "write_file",
                "edit_file",
                "delete_file",
                "bash",
                "glossary",
                "write_note"
            ]
        );
        let by = |name: &str| list.iter().find(|t| t.spec().name == name).unwrap();
        let made = by("create_file").run(r#"{"path":"src/a.txt","content":"one"}"#);
        assert!(made.starts_with("created"), "{made}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/a.txt")).unwrap(),
            "one"
        );
        let edited =
            by("edit_file").run(r#"{"path":"src/a.txt","old_string":"one","new_string":"two"}"#);
        assert!(edited.starts_with("edited"), "{edited}");
        // A command nobody allowed is refused, not asked; one the owner allowed runs.
        let bash = by("bash");
        let refused = bash.run(r#"{"command":"chmod 777 src/a.txt"}"#);
        assert!(refused.starts_with("denied"), "{refused}");
        bash.run(r#"{"command":"touch made.txt"}"#);
        assert!(dir.path().join("made.txt").exists());
        let ok = bash.run(r#"{"command":"mkdir -p out && ls"}"#);
        assert!(ok.contains("src"), "{ok}");
        assert!(dir.path().join("out").is_dir());
    }
}

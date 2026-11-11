//! The list of tools (issue 38): what the model is given, for `/tools` and the Tools web page.
//!
//! The terminal reads the tools the chat really has; the web page, which has no chat, builds the
//! same sets with [`crate::tools::chat_tools`] and [`crate::subagent::tools`], so neither can list a
//! tool that does not exist or miss one that does.

use std::path::Path;
use std::rc::Rc;

use kernel::ToolSpec;

use crate::cancel::Cancel;
use crate::subagent::{self, Kit, NobodyToAsk};
use crate::tools::{Mode, shared_mode};

/// One tool as a person reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    /// The argument names; an optional one ends in `?`.
    pub arguments: Vec<String>,
}

impl ToolInfo {
    pub fn of(spec: &ToolSpec) -> Self {
        let schema: serde_json::Value =
            serde_json::from_str(spec.parameters()).unwrap_or(serde_json::Value::Null);
        let required: Vec<&str> = schema["required"]
            .as_array()
            .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        let arguments = schema["properties"]
            .as_object()
            .map(|props| {
                props
                    .keys()
                    .map(|k| {
                        if required.contains(&k.as_str()) {
                            k.clone()
                        } else {
                            format!("{k}?")
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            name: spec.name.clone(),
            description: spec.description.clone(),
            arguments,
        }
    }

    /// `name(arg, arg?)`.
    pub fn signature(&self) -> String {
        format!("{}({})", self.name, self.arguments.join(", "))
    }

    /// The first sentence or line of the description, cut to `limit` characters.
    pub fn summary(&self, limit: usize) -> String {
        let line = self.description.lines().next().unwrap_or("").trim();
        let sentence = line.split_once(". ").map_or(line, |(first, _)| first);
        let sentence = sentence.trim_end_matches('.');
        if sentence.chars().count() <= limit {
            return sentence.to_owned();
        }
        let mut cut: String = sentence.chars().take(limit).collect();
        cut.push_str("...");
        cut
    }
}

/// The tools of the main chat, and of the two kinds of subagent.
#[derive(Debug, Clone, Default)]
pub struct Catalogue {
    pub chat: Vec<ToolInfo>,
    pub reader: Vec<ToolInfo>,
    pub coder: Vec<ToolInfo>,
}

pub fn infos(specs: &[ToolSpec]) -> Vec<ToolInfo> {
    specs.iter().map(ToolInfo::of).collect()
}

/// The tools a chat started in `cwd` with the database `db` would have.
pub fn catalogue(cwd: &Path, db: &Path) -> Catalogue {
    let mut chat: Vec<ToolSpec> = crate::tools::chat_tools(
        cwd,
        db,
        shared_mode(Mode::Manual),
        Rc::new(NobodyToAsk),
        Cancel::new(),
        crate::tools::ShellTasks::new(),
    )
    .iter()
    .map(|t| t.spec().clone())
    .collect();
    chat.push(subagent::delegate_spec());
    let of_kit = |coder: bool| {
        let kit = Kit {
            coder,
            always: Vec::new(),
            allow: None,
        };
        subagent::tools(cwd, db, &Cancel::new(), &kit)
            .iter()
            .map(|t| t.spec().clone())
            .collect::<Vec<_>>()
    };
    Catalogue {
        chat: infos(&chat),
        reader: infos(&of_kit(false)),
        coder: infos(&of_kit(true)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::Scratch;

    #[test]
    fn the_catalogue_has_the_chat_tools_and_two_kinds_of_subagent() {
        let dir = Scratch::new("tool-list");
        let list = catalogue(dir.path(), &dir.path().join("harness.db"));
        let names = |l: &[ToolInfo]| l.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
        let chat = names(&list.chat);
        for name in [
            "read_file",
            "create_file",
            "edit_file",
            "bash",
            "glossary",
            "glossary_propose",
            "context_propose",
            "library",
            "profile",
            "delegate",
            "write_note",
            "grep",
            "find",
        ] {
            assert!(chat.contains(&name.to_owned()), "{name} in {chat:?}");
        }
        let reader = names(&list.reader);
        assert!(reader.contains(&"write_note".to_owned()));
        assert!(!reader.contains(&"edit_file".to_owned()), "{reader:?}");
        assert!(names(&list.coder).contains(&"edit_file".to_owned()));
        assert!(!reader.contains(&"delegate".to_owned()));
        for kit in [&list.reader, &list.coder] {
            assert!(!names(kit).iter().any(|n| n == "library" || n == "profile"));
        }
    }

    #[test]
    fn a_signature_marks_optional_arguments_and_a_summary_is_one_short_sentence() {
        let info = ToolInfo::of(
            &ToolSpec::new(
                "edit",
                "Change a file. It fails when nothing matches.",
                r#"{"type":"object","properties":{"path":{"type":"string"},"all":{"type":"boolean"}},"required":["path"]}"#,
            )
            .unwrap(),
        );
        assert_eq!(info.signature(), "edit(all?, path)");
        assert_eq!(info.summary(70), "Change a file");
        assert_eq!(info.summary(5), "Chang...");
    }
}

#[cfg(test)]
mod size_guard {
    use super::*;
    use crate::tools::testutil::Scratch;

    /// Every definition in the request is paid for on every call (issue 54): the built-in tools
    /// were 11,759 bytes before they were shortened. A new or longer definition has to make room.
    #[test]
    fn the_built_in_tool_definitions_stay_small() {
        let dir = Scratch::new("tool-size");
        let mut chat: Vec<ToolSpec> = crate::tools::chat_tools(
            dir.path(),
            &dir.path().join("harness.db"),
            shared_mode(Mode::Manual),
            Rc::new(NobodyToAsk),
            Cancel::new(),
            crate::tools::ShellTasks::new(),
        )
        .iter()
        .map(|t| t.spec().clone())
        .collect();
        chat.push(subagent::delegate_spec());
        let total: usize = chat
            .iter()
            .map(|s| s.description.len() + s.parameters().len())
            .sum();
        assert!(total <= 10_300, "the built-in tools are {total} bytes");
    }
}

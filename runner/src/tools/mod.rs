//! Tools the model can call: `read_file`, `create_file`, `write_file`, `delete_file` and `bash`.
//!
//! A tool takes the model's arguments as raw JSON text and returns text for the
//! model. Every failure (bad arguments, missing file, timeout, denied) comes
//! back as text too, never a crash, so the agent loop can hand it straight back
//! to the model.

mod bash;
mod commands;
mod context;
mod files;
mod glossary;
mod library;
mod notes;
mod read_file;
mod steps;
mod tasks;
mod truncate;

#[cfg(test)]
pub(crate) mod testutil;

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::rc::Rc;

use kernel::ToolSpec;

pub use bash::{Bash, DEFAULT_TIMEOUT};
pub use commands::{FREE_COMMANDS, always_for, is_free, is_looking};
pub use context::{context_read_tool, context_tools, kinds_text};
pub use files::{MAX_CONTENT_BYTES, file_tools};
pub use glossary::{glossary_lookup_tool, glossary_tools};
pub use library::library_tools;
pub use notes::{MAX_NOTE_BYTES, NOTES_DIR, WriteNote};
pub use read_file::ReadFile;
pub use steps::{Step, StepStatus, StepsTool, parse_steps, unfinished};
pub use tasks::{ShellInfo, ShellStatus, ShellTasks, TaskTool, clock};
pub use truncate::{HEAD_BYTES, MAX_OUTPUT_BYTES, TAIL_BYTES, truncate};

// Rust note: a `trait` is a set of methods a type promises to provide, like a
// Java interface. The agent loop (issue 7) will hold a list of
// `Box<dyn Tool>`: "boxes holding anything that implements Tool", so it can
// treat `ReadFile` and `Bash` the same way.
pub trait Tool {
    /// The name, description and argument schema sent to the model.
    fn spec(&self) -> &ToolSpec;

    /// Runs the tool. `arguments` is the raw JSON text the model produced.
    fn run(&self, arguments: &str) -> String;
}

/// What a question is about, for [`Approver::approve`].
pub struct Ask<'a> {
    /// What is being asked, for example "run this command?".
    pub question: &'a str,
    /// What it is about: the command, or the path and size.
    pub detail: &'a str,
    /// What "always" would cover, for example "`git` commands"; `None` when it is not offered.
    pub always: Option<&'a str>,
    /// The whole text behind the question (a file's content), when it is longer than the detail
    /// line. The terminal offers to show it.
    pub full: Option<&'a str>,
}

/// Hands every question to another approver (`Bash` wants its own boxed one).
struct SharedApprover(Rc<dyn Approver>);

impl Approver for SharedApprover {
    fn approve(&self, ask: &Ask) -> Answer {
        self.0.approve(ask)
    }
}

/// The tools of a chat: files, `bash`, the glossary and the context entries. One place builds them
/// for the program and for the Tools web page, so the page cannot drift from what the model gets.
/// (`delegate` is added by the agent.)
pub fn chat_tools(
    cwd: &std::path::Path,
    db: &std::path::Path,
    approval: SharedMode,
    approver: Rc<dyn Approver>,
    cancel: crate::cancel::Cancel,
    shells: ShellTasks,
) -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = vec![Box::new(ReadFile::new(cwd))];
    tools.extend(file_tools(cwd, Rc::clone(&approval), Rc::clone(&approver)));
    tools.push(Box::new(
        Bash::new(
            cwd,
            DEFAULT_TIMEOUT,
            Box::new(SharedApprover(Rc::clone(&approver))),
        )
        .with_approval(Rc::clone(&approval))
        .with_cancel(cancel)
        .with_shells(shells.clone()),
    ));
    tools.push(Box::new(TaskTool::new(shells)));
    tools.push(Box::new(StepsTool::new()));
    tools.extend(glossary_tools(
        db,
        cwd.display().to_string(),
        Rc::clone(&approver),
        Rc::clone(&approval),
    ));
    tools.extend(context_tools(
        db,
        cwd.display().to_string(),
        Rc::clone(&approver),
        Rc::clone(&approval),
    ));
    tools.extend(library_tools(db, approver, approval));
    tools
}

/// The answer to an [`Ask`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Yes,
    /// Yes, and do not ask again for the same kind of action until the program quits.
    Always,
    No,
}

/// Asked before `bash` runs a command, and before a file tool changes the disk in `manual` mode.
/// The REPL implements this as a selector on the terminal; tests use a scripted answer.
pub trait Approver {
    fn approve(&self, ask: &Ask) -> Answer;
}

/// How the file tools that change the disk are approved. `read_file` never asks, and `bash`
/// asks in both modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Every create, write and delete asks first.
    Manual,
    /// Creates, writes and deletes run without asking.
    Auto,
}

impl Mode {
    pub const ALL: [Mode; 2] = [Mode::Manual, Mode::Auto];

    pub fn name(self) -> &'static str {
        match self {
            Mode::Manual => "manual",
            Mode::Auto => "auto",
        }
    }

    /// What the mode means, in one line.
    pub fn describe(self) -> &'static str {
        match self {
            Mode::Manual => {
                "ask before every file change, saved glossary word or context entry, and bash command"
            }
            Mode::Auto => {
                "file changes, glossary words and context entries are saved without asking; bash still asks"
            }
        }
    }

    pub fn parse(text: &str) -> Option<Mode> {
        Mode::ALL
            .into_iter()
            .find(|mode| mode.name() == text.trim().to_ascii_lowercase())
    }
}

/// The approval mode and the "always" permissions given so far, shared between the tools and
/// the `/approve` command. The permissions live in memory only: they end with the program.
// Rust note: `Rc<..>` lets several owners hold one value; `Cell` and `RefCell` let them change it
// through a shared reference, which is safe here because everything runs on one thread.
pub struct Approval {
    mode: Cell<Mode>,
    /// Plan mode (issue 42): nothing is changed, whatever the mode says.
    plan: Cell<bool>,
    always: RefCell<BTreeSet<(String, String)>>,
}

pub type SharedMode = Rc<Approval>;

pub fn shared_mode(mode: Mode) -> SharedMode {
    Rc::new(Approval {
        mode: Cell::new(mode),
        plan: Cell::new(false),
        always: RefCell::new(BTreeSet::new()),
    })
}

impl Approval {
    /// A state with this mode and these "always" permissions already given, for a subagent: it
    /// starts with what the owner allowed in the chat, and what it asks for itself is refused.
    pub fn with_always(
        mode: Mode,
        always: impl IntoIterator<Item = (String, String)>,
    ) -> SharedMode {
        Rc::new(Approval {
            mode: Cell::new(mode),
            plan: Cell::new(false),
            always: RefCell::new(always.into_iter().collect()),
        })
    }

    /// The "always" permissions as `(identity, label)` pairs.
    pub fn always_pairs(&self) -> Vec<(String, String)> {
        self.always.borrow().iter().cloned().collect()
    }

    pub fn mode(&self) -> Mode {
        self.mode.get()
    }

    pub fn set_mode(&self, mode: Mode) {
        self.mode.set(mode);
    }

    /// Plan mode is on: every change is refused without asking anyone.
    pub fn planning(&self) -> bool {
        self.plan.get()
    }

    pub fn set_planning(&self, on: bool) {
        self.plan.set(on);
    }

    /// What a tool says when a change was not made: `denied` as it is, or, in plan mode, that the
    /// mode is why, so the model puts the change in its plan and does not look for a way round.
    pub(crate) fn denial(&self, denied: &str) -> String {
        if self.planning() {
            let what = denied.trim_start_matches("denied by user: ");
            format!("plan mode is on, so {what}; put the change in your plan instead")
        } else {
            denied.to_owned()
        }
    }

    /// The things allowed with "always", as the labels shown on screen.
    pub fn always_labels(&self) -> Vec<String> {
        self.always
            .borrow()
            .iter()
            .map(|(_, label)| label.clone())
            .collect()
    }

    /// Forgets every "always". Returns how many there were.
    pub fn forget_always(&self) -> usize {
        let mut always = self.always.borrow_mut();
        let count = always.len();
        always.clear();
        count
    }

    /// A proposal to save something the owner said (a glossary word, a context entry). It is not
    /// asked in `auto` mode, and "always" is offered otherwise, so a run of them is not a run of
    /// questions. `what` is the identity and the label shown for "always".
    pub(crate) fn confirm_saving(
        &self,
        approver: &dyn Approver,
        what: (&str, &str),
        ask: (&str, &str, &str),
    ) -> bool {
        if self.planning() {
            return false;
        }
        if self.mode.get() == Mode::Auto {
            return true;
        }
        let (question, detail, full) = ask;
        self.confirm(
            approver,
            question,
            detail,
            Some((format!("save:{}", what.0), what.1.to_owned())),
            Some(full),
        )
    }

    /// Asks (unless an earlier "always" already covers it) and remembers an "always" answer.
    /// `key` is `(identity, label)` of what "always" would cover, or `None` when it is not offered.
    pub(crate) fn confirm(
        &self,
        approver: &dyn Approver,
        question: &str,
        detail: &str,
        key: Option<(String, String)>,
        full: Option<&str>,
    ) -> bool {
        if self.planning() {
            return false;
        }
        if let Some((id, label)) = &key
            && self.always.borrow().contains(&(id.clone(), label.clone()))
        {
            return true;
        }
        let ask = Ask {
            question,
            detail,
            always: key.as_ref().map(|(_, label)| label.as_str()),
            full,
        };
        match approver.approve(&ask) {
            Answer::Yes => true,
            Answer::Always => {
                if let Some(key) = key {
                    self.always.borrow_mut().insert(key);
                }
                true
            }
            Answer::No => false,
        }
    }
}

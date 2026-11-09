//! Tools the model can call: `read_file`, `create_file`, `write_file`, `delete_file` and `bash`.
//!
//! A tool takes the model's arguments as raw JSON text and returns text for the
//! model. Every failure (bad arguments, missing file, timeout, denied) comes
//! back as text too, never a crash, so the agent loop can hand it straight back
//! to the model.

mod bash;
mod commands;
mod files;
mod read_file;
mod truncate;

#[cfg(test)]
pub(crate) mod testutil;

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::rc::Rc;

use kernel::ToolSpec;

pub use bash::{Bash, DEFAULT_TIMEOUT};
pub use commands::{FREE_COMMANDS, always_for, is_free};
pub use files::{MAX_CONTENT_BYTES, file_tools};
pub use read_file::ReadFile;
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
            Mode::Manual => "ask before every file change and every bash command",
            Mode::Auto => "file changes run without asking; bash still asks",
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
    always: RefCell<BTreeSet<(String, String)>>,
}

pub type SharedMode = Rc<Approval>;

pub fn shared_mode(mode: Mode) -> SharedMode {
    Rc::new(Approval {
        mode: Cell::new(mode),
        always: RefCell::new(BTreeSet::new()),
    })
}

impl Approval {
    pub fn mode(&self) -> Mode {
        self.mode.get()
    }

    pub fn set_mode(&self, mode: Mode) {
        self.mode.set(mode);
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

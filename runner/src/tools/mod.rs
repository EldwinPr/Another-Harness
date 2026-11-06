//! Tools the model can call: `read_file` and `bash`.
//!
//! A tool takes the model's arguments as raw JSON text and returns text for the
//! model. Every failure (bad arguments, missing file, timeout, denied) comes
//! back as text too, never a crash, so the agent loop can hand it straight back
//! to the model.

mod bash;
mod read_file;
mod truncate;

#[cfg(test)]
pub(crate) mod testutil;

use kernel::ToolSpec;

pub use bash::{Bash, DEFAULT_TIMEOUT};
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

/// Asked before `bash` runs a command. The REPL will implement this as a y/n
/// prompt on the terminal (issue 8); tests use a scripted answer.
pub trait Approver {
    fn approve(&self, command: &str) -> bool;
}

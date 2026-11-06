//! The pure core of the harness: provider-neutral types and `assemble()`.
//!
//! No I/O, no clock, no environment (decision 01, decision 02). Everything here
//! is a function of its arguments, so the same state always gives the same
//! bytes. Wire serialization happens in [`assemble`]; the types stay neutral so
//! another serializer can be added without touching them (decision 06).

// Rust note: this file is the "crate root" of the `kernel` library. It is the
// front door: whatever it makes `pub` is what other crates (like `runner`) can
// use. `//!` comments (above) document the whole crate; `///` comments document
// the single item that follows them.

// Rust note: `mod assemble;` says "there is a module called `assemble`, and its
// code lives in the file assemble.rs". Same for `types` and types.rs. Without
// `pub`, a module is private: only this crate can see it.
mod assemble;
mod types;

// Rust note: `pub use` re-exports names so users write `kernel::assemble(...)`
// instead of the longer `kernel::assemble::assemble(...)`. A module and a
// function can share the name `assemble` because Rust keeps modules and
// functions in separate namespaces.
pub use assemble::{assemble, message_from_json, message_to_json};
pub use types::{Config, InvalidParameters, Message, RouterOptions, ToolCall, ToolSpec};

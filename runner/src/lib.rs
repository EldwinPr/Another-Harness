//! The agent runner: everything that touches the outside world.
//!
//! `kernel` stays pure; this crate reads files, runs commands, and (in later
//! issues) talks to the network. It is a library so its tests, and later the
//! binary, can both use it.

// Rust note: a crate can be a library (this file) and a binary (main.rs) at the
// same time. main.rs then uses the library like any other user would.
pub mod agent;
pub mod cache_check;
pub mod config;
pub mod config_ui;
pub mod lineedit;
pub mod menu;
pub mod prompts_ui;
pub mod provider;
pub mod providers;
pub mod repl;
pub mod sessions_ui;
pub mod skills_ui;
pub mod store;
pub mod store_models;
pub mod store_prompts;
pub mod style;
pub mod tools;
pub mod turn_log;

#[cfg(test)]
mod testutil;

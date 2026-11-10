//! The agent runner: everything that touches the outside world.
//!
//! `kernel` stays pure; this crate reads files, runs commands, and (in later
//! issues) talks to the network. It is a library so its tests, and later the
//! binary, can both use it.

// Rust note: a crate can be a library (this file) and a binary (main.rs) at the
// same time. main.rs then uses the library like any other user would.
pub mod agent;
pub mod anthropic;
pub mod attach;
pub mod cache_check;
pub mod cancel;
pub mod config;
pub mod config_ui;
pub mod context_ui;
pub mod decider;
pub mod decider_cases;
pub mod glossary_ui;
pub mod inbox_ui;
pub mod inbox_view;
pub mod inputbox;
pub mod jev;
pub mod jobs;
pub mod lineedit;
pub mod loop_guard;
pub mod markdown;
pub mod mcp;
pub mod mcp_serve;
pub mod mcp_toolsets;
pub mod mcp_ui;
pub mod menu;
pub mod profiles_ui;
pub mod prompts_ui;
pub mod provider;
pub mod providers;
pub mod repl;
pub mod sessions_ui;
pub mod skills_ui;
pub mod store;
pub mod store_buffer;
pub mod store_context;
pub mod store_glossary;
pub mod store_mcp;
pub mod store_models;
pub mod store_profiles;
pub mod store_prompts;
pub mod style;
pub mod subagent;
pub mod tool_list;
pub mod tools;
pub mod tools_ui;
pub mod toolview;
pub mod trim;
pub mod turn_log;
pub mod usage;
pub mod web;
pub mod web_inbox;
pub mod web_pages;
pub mod web_schema;
pub mod web_ui;
pub mod web_usage;
pub mod web_write;

#[cfg(test)]
mod testutil;

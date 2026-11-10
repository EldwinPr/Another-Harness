//! Ready-made MCP toolsets (issue 44): a known server with the rules already set, so adding it is
//! one step and not an add followed by a rule for every tool.
//!
//! A toolset is only a recipe for `mcp_servers` and `mcp_tool_rules`; once installed it is an
//! ordinary server the owner can change with `/mcp`. Installing one that is already there changes
//! nothing, so the owner's own rules are never overwritten.

use crate::store::Store;
use crate::store_mcp::{ALL_TOOLS, DEFAULT_TIMEOUT_SECS, Rule};

pub struct Toolset {
    /// The name given to `--mcp` and `/mcp toolset add`; also the server's name.
    pub name: &'static str,
    /// One line for a list.
    pub about: &'static str,
    pub command: &'static str,
    pub args: &'static [&'static str],
    /// The tools that run without asking.
    pub free: &'static [&'static str],
    /// The tools that ask before each call. Every tool in neither list is off.
    pub ask: &'static [&'static str],
    /// What to do when the program is not installed.
    pub install_hint: &'static str,
}

pub const TOOLSETS: &[Toolset] = &[Toolset {
    name: "rust-docs",
    about: "Rust crate documentation: search and read items of crates already cached",
    command: "rust-docs-mcp",
    args: &["serve"],
    // Eight of its 15 tools (all 15 cost about 6,200 tokens on every model call). The reading
    // ones are free; the three that download, write and delete in the server's cache ask,
    // because the server sends no read-only hints. The rest repeat a free tool or search the
    // same index more heavily, so they are off.
    free: &[
        "list_cached_crates",
        "search_items_preview",
        "get_item_details",
        "get_item_source",
        "get_dependencies",
    ],
    ask: &["cache_crate", "remove_crate", "cache_operations"],
    install_hint: "install rust-docs-mcp first (see context/coder-only/stack.md)",
}];

impl Toolset {
    /// What the rules are, for a message: "5 tools run without asking, 3 ask first, the rest are off".
    pub fn rules_text(&self) -> String {
        format!(
            "{} tools run without asking, {} ask first, the rest are off",
            self.free.len(),
            self.ask.len()
        )
    }
}

pub fn find(name: &str) -> Option<&'static Toolset> {
    let name = name.trim();
    TOOLSETS.iter().find(|t| t.name.eq_ignore_ascii_case(name))
}

/// What installing did.
#[derive(Debug, PartialEq, Eq)]
pub enum Installed {
    Added,
    /// A server of that name was there already; nothing was changed.
    AlreadyThere,
}

/// Whether `program` can be started: a path that exists, or a name found on `PATH`.
pub fn on_path(program: &str) -> bool {
    if program.contains('/') {
        return std::path::Path::new(program).is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// Saves the toolset as a server with its rules. It is added even when the program is missing
/// (the chat then says it did not start), so the caller can warn using [`on_path`].
pub fn install(store: &mut Store, toolset: &Toolset) -> Result<Installed, String> {
    if store
        .mcp_server(toolset.name)
        .map_err(|e| e.to_string())?
        .is_some()
    {
        return Ok(Installed::AlreadyThere);
    }
    let args: Vec<String> = toolset.args.iter().map(|a| (*a).to_owned()).collect();
    store
        .add_mcp_server(
            toolset.name,
            toolset.command,
            &args,
            &[],
            DEFAULT_TIMEOUT_SECS,
        )
        .map_err(|e| e.to_string())?;
    store
        .set_mcp_rule(toolset.name, ALL_TOOLS, Rule::Off)
        .map_err(|e| e.to_string())?;
    for (tools, rule) in [(toolset.free, Rule::Free), (toolset.ask, Rule::Ask)] {
        for tool in tools {
            store
                .set_mcp_rule(toolset.name, tool, rule)
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(Installed::Added)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::Scratch;

    #[test]
    fn a_toolset_is_found_by_name_in_any_case() {
        assert!(find("rust-docs").is_some());
        assert!(find(" Rust-Docs ").is_some());
        assert!(find("nothing").is_none());
    }

    #[test]
    fn installing_saves_the_server_with_everything_off_but_the_listed_tools() {
        let dir = Scratch::new("toolset");
        let mut store = Store::open(&dir.path().join("harness.db")).unwrap();
        let toolset = find("rust-docs").unwrap();
        assert_eq!(install(&mut store, toolset), Ok(Installed::Added));
        let server = store.mcp_server("rust-docs").unwrap().unwrap();
        assert_eq!(server.command, "rust-docs-mcp");
        assert_eq!(server.args, ["serve"]);
        assert!(server.enabled);
        assert_eq!(server.default_rule, Rule::Off);
        let rules = store.mcp_rules(server.id).unwrap();
        assert_eq!(rules.len(), toolset.free.len() + toolset.ask.len());
        for tool in toolset.free {
            assert!(rules.contains(&((*tool).to_owned(), Rule::Free)), "{tool}");
        }
        for tool in toolset.ask {
            assert!(rules.contains(&((*tool).to_owned(), Rule::Ask)), "{tool}");
        }
        // The tools that write to the server's cache are never free.
        assert!(!toolset.free.contains(&"cache_crate"));
        assert!(!toolset.free.contains(&"remove_crate"));
    }

    #[test]
    fn installing_again_changes_nothing_the_owner_set() {
        let dir = Scratch::new("toolset-again");
        let mut store = Store::open(&dir.path().join("harness.db")).unwrap();
        let toolset = find("rust-docs").unwrap();
        install(&mut store, toolset).unwrap();
        store
            .set_mcp_rule("rust-docs", "get_item_details", Rule::Ask)
            .unwrap();
        assert_eq!(install(&mut store, toolset), Ok(Installed::AlreadyThere));
        let server = store.mcp_server("rust-docs").unwrap().unwrap();
        let rules = store.mcp_rules(server.id).unwrap();
        assert!(rules.contains(&("get_item_details".to_owned(), Rule::Ask)));
    }

    #[test]
    fn a_program_is_found_by_path_or_by_the_path_variable() {
        assert!(on_path("/bin/sh") || on_path("/usr/bin/sh"));
        assert!(on_path("sh"));
        assert!(!on_path("/nonexistent/program"));
        assert!(!on_path("definitely-not-a-program-xyz"));
    }
}

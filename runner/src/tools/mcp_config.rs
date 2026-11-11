//! `mcp` (issue 44): the model reads, and with the owner's yes changes, the MCP servers the chat
//! starts, so the owner can say "set up the Rust docs server" and not type `/mcp` commands.
//!
//! Reading never asks. Every change is asked every time, in both approval modes and without
//! "always": a server is a program that runs on the owner's computer each time a chat starts.
//! Plan mode refuses changes. The tool cannot give a server an environment variable (a key stays
//! with the owner: `/mcp config`). A change applies the next time the server is switched on, because
//! the tool list is fixed for a chat (decision 10), and the answer says so.

use std::path::PathBuf;
use std::rc::Rc;

use kernel::ToolSpec;
use serde::Deserialize;

use super::{Answer, Approver, Ask, SharedMode, Tool};
use crate::cancel::Cancel;
use crate::mcp::McpClient;
use crate::mcp_toolsets::{self, Installed, TOOLSETS};
use crate::store::Store;
use crate::store_mcp::{ALL_TOOLS, DEFAULT_TIMEOUT_SECS, McpServer, Rule, rule_of};

const SCHEMA: &str = r#"{"type":"object","properties":{"action":{"type":"string","enum":["list","toolsets","tools","add_toolset","add","rule","enable","disable","remove"],"description":"tools: start a saved server and list its tools. add_toolset: add a ready-made one by name. add: a program of your own. rule: ask, free or off for one tool, or * for the rest."},"name":{"type":"string","description":"Server or toolset name: lower-case letters, digits, dash, at most 20"},"command":{"type":"string","description":"add: the program, one word"},"args":{"type":"array","items":{"type":"string"},"description":"add: its arguments"},"timeout":{"type":"integer","description":"add: seconds per call, 1 to 600, default 60"},"tool":{"type":"string","description":"rule: the server's name for the tool, or *"},"rule":{"type":"string","enum":["ask","free","off"],"description":"rule: ask = owner asked each call; free = no asking; off = hidden from you"},"reason":{"type":"string","description":"Why, shown to the owner"}},"required":["action"]}"#;

/// What the answer says after a change.
const NEXT_START: &str = "It applies the next time the server is switched on in a chat (the tool list of a chat does not change); the owner switches it on with /mcp on NAME.";
/// How much of a line the question shows.
const SHOWN_CHARS: usize = 120;

#[derive(Deserialize)]
struct Args {
    action: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    command: String,
    #[serde(default)]
    args: Vec<String>,
    timeout: Option<u64>,
    #[serde(default)]
    tool: String,
    #[serde(default)]
    rule: String,
    #[serde(default)]
    reason: String,
}

pub struct McpConfig {
    spec: ToolSpec,
    db: PathBuf,
    cwd: PathBuf,
    cancel: Cancel,
    approver: Rc<dyn Approver>,
    approval: SharedMode,
}

/// The `mcp` tool.
pub fn mcp_config_tool(
    db: impl Into<PathBuf>,
    cwd: impl Into<PathBuf>,
    approver: Rc<dyn Approver>,
    approval: SharedMode,
    cancel: Cancel,
) -> Box<dyn Tool> {
    Box::new(McpConfig {
        spec: ToolSpec::new(
            "mcp",
            "Read or change the owner's MCP servers, which give you more tools (mcp__server__tool) once the owner switches one on with /mcp on NAME. Use it when the owner asks to set one up: toolsets lists ready-made ones, add_toolset adds one, add saves another program. Every change asks the owner first. You cannot set variables or keys.",
            SCHEMA,
        )
        .expect("the schema constant is valid JSON"),
        db: db.into(),
        cwd: cwd.into(),
        cancel,
        approver,
        approval,
    })
}

fn cut(text: &str, limit: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= limit {
        return one_line;
    }
    let mut shown: String = one_line.chars().take(limit).collect();
    shown.push_str("...");
    shown
}

impl McpConfig {
    fn open(&self) -> Result<Store, String> {
        Store::open(&self.db).map_err(|e| format!("error: cannot open the database: {e}"))
    }

    /// Asks the owner, every time and in both modes. False when refused (or in plan mode).
    fn allowed(&self, question: &str, detail: &str, reason: &str) -> bool {
        if self.approval.planning() {
            return false;
        }
        let full = if reason.trim().is_empty() {
            detail.to_owned()
        } else {
            format!("{detail}\n\nwhy: {}", reason.trim())
        };
        self.approver.approve(&Ask {
            question,
            detail: &cut(detail, SHOWN_CHARS),
            always: None,
            full: Some(&full),
        }) != Answer::No
    }

    fn refused(&self) -> String {
        self.approval
            .denial("denied by user: the MCP servers were not changed")
    }

    fn list(&self) -> String {
        let store = match self.open() {
            Ok(store) => store,
            Err(why) => return why,
        };
        let servers = match store.list_mcp_servers() {
            Ok(servers) => servers,
            Err(why) => return format!("error: {why}"),
        };
        if servers.is_empty() {
            return "no MCP servers are saved".to_owned();
        }
        let mut lines = Vec::new();
        for server in &servers {
            let rules = store.mcp_rules(server.id).unwrap_or_default();
            let mut line = format!(
                "{}{}: {} {} (timeout {}s; other tools: {}",
                server.name,
                if server.enabled { "" } else { " (disabled)" },
                server.command,
                server.args.join(" "),
                server.timeout_secs,
                server.default_rule.name()
            );
            if !server.vars.is_empty() {
                let names: Vec<&str> = server.vars.iter().map(|(n, _)| n.as_str()).collect();
                // Names only: a value may be a key.
                line.push_str(&format!("; variables: {}", names.join(", ")));
            }
            line.push(')');
            for (tool, rule) in &rules {
                line.push_str(&format!("\n  {tool}: {}", rule.name()));
            }
            lines.push(line);
        }
        lines.join("\n")
    }

    fn toolsets(&self) -> String {
        let have = self
            .open()
            .ok()
            .and_then(|s| s.list_mcp_servers().ok())
            .unwrap_or_default();
        TOOLSETS
            .iter()
            .map(|t| {
                format!(
                    "{}{}: {} ({} {}; {})",
                    t.name,
                    if have.iter().any(|s| s.name == t.name) {
                        " (already added)"
                    } else {
                        ""
                    },
                    t.about,
                    t.command,
                    t.args.join(" "),
                    t.rules_text()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn saved(&self, name: &str) -> Result<(Store, McpServer), String> {
        let store = self.open()?;
        match store.mcp_server(name) {
            Ok(Some(server)) => Ok((store, server)),
            Ok(None) => Err(format!(
                "error: there is no MCP server '{name}' (action list)"
            )),
            Err(why) => Err(format!("error: {why}")),
        }
    }

    fn add_toolset(&self, args: &Args) -> String {
        let Some(toolset) = mcp_toolsets::find(&args.name) else {
            return format!(
                "error: there is no toolset '{}' (action toolsets lists them)",
                args.name
            );
        };
        let mut store = match self.open() {
            Ok(store) => store,
            Err(why) => return why,
        };
        if matches!(store.mcp_server(toolset.name), Ok(Some(_))) {
            return format!(
                "{} is already added; change its rules with action rule",
                toolset.name
            );
        }
        let detail = format!(
            "{} {} (free: {}; ask first: {}; the rest off)",
            toolset.command,
            toolset.args.join(" "),
            toolset.free.join(", "),
            toolset.ask.join(", ")
        );
        if !self.allowed(
            &format!("add the MCP server {}?", toolset.name),
            &detail,
            &args.reason,
        ) {
            return self.refused();
        }
        match mcp_toolsets::install(&mut store, toolset) {
            Ok(Installed::Added) => {
                let missing = if mcp_toolsets::on_path(toolset.command) {
                    String::new()
                } else {
                    format!(
                        " {} was not found on the PATH: {}.",
                        toolset.command, toolset.install_hint
                    )
                };
                format!(
                    "added {}: {}.{missing} {NEXT_START}",
                    toolset.name,
                    toolset.rules_text()
                )
            }
            Ok(Installed::AlreadyThere) => format!("{} was already added", toolset.name),
            Err(why) => format!("error: {why}"),
        }
    }

    fn add(&self, args: &Args) -> String {
        if args.command.trim().is_empty() {
            return "error: add needs name and command".to_owned();
        }
        let mut store = match self.open() {
            Ok(store) => store,
            Err(why) => return why,
        };
        let timeout = args.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS);
        // The checks run before the owner is asked, so no one is asked about something that
        // cannot be saved.
        let line = format!("{} {}", args.command.trim(), args.args.join(" "));
        let checked = crate::store_mcp::check_name(&args.name)
            .and(crate::store_mcp::check_command(&args.command))
            .and(crate::store_mcp::check_timeout(timeout));
        if let Err(why) = checked {
            return format!("error: {why}");
        }
        if matches!(store.mcp_server(&args.name), Ok(Some(_))) {
            return format!(
                "error: there is already a server named {}",
                args.name.trim()
            );
        }
        if !self.allowed(
            &format!(
                "add the MCP server {}? It runs this program",
                args.name.trim()
            ),
            line.trim(),
            &args.reason,
        ) {
            return self.refused();
        }
        match store.add_mcp_server(&args.name, &args.command, &args.args, &[], timeout) {
            Ok(server) => format!(
                "added {}: every tool asks the owner first until you set a rule. {NEXT_START}",
                server.name
            ),
            Err(why) => format!("error: {why}"),
        }
    }

    fn rule(&self, args: &Args) -> String {
        let Some(rule) = Rule::parse(&args.rule) else {
            return "error: rule must be ask, free or off".to_owned();
        };
        let tool = args.tool.trim();
        if tool.is_empty() {
            return "error: rule needs tool (a tool's name, or *)".to_owned();
        }
        let (mut store, server) = match self.saved(&args.name) {
            Ok(found) => found,
            Err(why) => return why,
        };
        let of = if tool == ALL_TOOLS {
            "every tool with no rule of its own".to_owned()
        } else {
            format!("the tool {tool}")
        };
        if !self.allowed(
            &format!("set {} of {} to {}?", of, server.name, rule.name()),
            &format!("{}: {tool} = {}", server.name, rule.name()),
            &args.reason,
        ) {
            return self.refused();
        }
        match store.set_mcp_rule(&server.name, tool, rule) {
            Ok(true) => format!("{} {tool}: {}. {NEXT_START}", server.name, rule.name()),
            Ok(false) => format!("error: there is no MCP server '{}'", server.name),
            Err(why) => format!("error: {why}"),
        }
    }

    fn switch(&self, args: &Args, enable: bool) -> String {
        let (mut store, server) = match self.saved(&args.name) {
            Ok(found) => found,
            Err(why) => return why,
        };
        let verb = if enable { "enable" } else { "disable" };
        if !self.allowed(
            &format!("{verb} the MCP server {}?", server.name),
            &format!("{} {}", server.command, server.args.join(" ")),
            &args.reason,
        ) {
            return self.refused();
        }
        match store.set_mcp_enabled(&server.name, enable) {
            Ok(_) => format!("{} is {verb}d. {NEXT_START}", server.name),
            Err(why) => format!("error: {why}"),
        }
    }

    fn remove(&self, args: &Args) -> String {
        let (mut store, server) = match self.saved(&args.name) {
            Ok(found) => found,
            Err(why) => return why,
        };
        if !self.allowed(
            &format!("remove the MCP server {} and its rules?", server.name),
            &format!("{} {}", server.command, server.args.join(" ")),
            &args.reason,
        ) {
            return self.refused();
        }
        match store.remove_mcp_server(&server.name) {
            Ok(_) => format!("removed {}. {NEXT_START}", server.name),
            Err(why) => format!("error: {why}"),
        }
    }

    /// Starts a saved server for a moment and lists what it offers.
    fn tools(&self, args: &Args) -> String {
        let (store, server) = match self.saved(&args.name) {
            Ok(found) => found,
            Err(why) => return why,
        };
        if !self.allowed(
            &format!("start {} to list its tools?", server.name),
            &format!("{} {}", server.command, server.args.join(" ")),
            &args.reason,
        ) {
            return self.refused();
        }
        let rules = store.mcp_rules(server.id).unwrap_or_default();
        match McpClient::start(&server, &self.cwd, self.cancel.clone()) {
            Err(why) => format!("error: {} did not start: {why}", server.name),
            Ok((client, defs)) => {
                let lines: Vec<String> = defs
                    .iter()
                    .map(|def| {
                        let first = def.description.lines().next().unwrap_or("");
                        format!(
                            "{} [{}] ~{} tokens  {}",
                            def.name,
                            rule_of(&server, &rules, &def.name).name(),
                            def.tokens(&server.name),
                            cut(first, 100)
                        )
                    })
                    .collect();
                client.shutdown();
                format!(
                    "{} offers {} tools:\n{}\n{}",
                    server.name,
                    defs.len(),
                    lines.join("\n"),
                    crate::mcp_ui::cost_line(&server, &rules, &defs)
                )
            }
        }
    }
}

impl Tool for McpConfig {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn run(&self, arguments: &str) -> String {
        let args: Args = match serde_json::from_str(arguments) {
            Ok(args) => args,
            Err(e) => return format!("error: invalid arguments: {e}"),
        };
        match args.action.as_str() {
            "list" => self.list(),
            "toolsets" => self.toolsets(),
            "tools" => self.tools(&args),
            "add_toolset" => self.add_toolset(&args),
            "add" => self.add(&args),
            "rule" => self.rule(&args),
            "enable" => self.switch(&args, true),
            "disable" => self.switch(&args, false),
            "remove" => self.remove(&args),
            other => format!(
                "error: unknown action '{other}': list, toolsets, tools, add_toolset, add, rule, enable, disable or remove"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::tools::testutil::Scratch;
    use crate::tools::{Mode, shared_mode};

    struct Scripted {
        answer: Answer,
        asked: Rc<RefCell<Vec<(String, String, bool)>>>,
    }

    impl Approver for Scripted {
        fn approve(&self, ask: &Ask) -> Answer {
            self.asked.borrow_mut().push((
                ask.question.to_owned(),
                ask.detail.to_owned(),
                ask.always.is_some(),
            ));
            self.answer
        }
    }

    struct Rig {
        dir: Scratch,
        tool: Box<dyn Tool>,
        approval: SharedMode,
        asked: Rc<RefCell<Vec<(String, String, bool)>>>,
    }

    fn rig(tag: &str, mode: Mode, answer: Answer) -> Rig {
        let dir = Scratch::new(tag);
        let asked = Rc::new(RefCell::new(Vec::new()));
        let approval = shared_mode(mode);
        let tool = mcp_config_tool(
            dir.path().join("harness.db"),
            dir.path(),
            Rc::new(Scripted {
                answer,
                asked: Rc::clone(&asked),
            }),
            Rc::clone(&approval),
            Cancel::new(),
        );
        Rig {
            dir,
            tool,
            approval,
            asked,
        }
    }

    fn store(rig: &Rig) -> Store {
        Store::open(&rig.dir.path().join("harness.db")).unwrap()
    }

    #[test]
    fn a_server_is_added_only_after_the_owner_says_yes_and_the_answer_says_when_it_applies() {
        let yes = rig("mcp-tool-add", Mode::Manual, Answer::Yes);
        let out = yes.tool.run(
            r#"{"action":"add","name":"docs","command":"prog","args":["serve"],"reason":"to read crate docs"}"#,
        );
        assert!(out.starts_with("added docs"), "{out}");
        assert!(out.contains("switched on in a chat"), "{out}");
        let asked = yes.asked.borrow();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].1, "prog serve");
        assert!(!asked[0].2, "no always for running a program");
        let saved = store(&yes).mcp_server("docs").unwrap().unwrap();
        assert_eq!(saved.args, ["serve"]);
        assert!(saved.vars.is_empty());

        let no = rig("mcp-tool-add-no", Mode::Manual, Answer::No);
        let out = no
            .tool
            .run(r#"{"action":"add","name":"docs","command":"prog"}"#);
        assert_eq!(out, "denied by user: the MCP servers were not changed");
        assert!(store(&no).mcp_server("docs").unwrap().is_none());
    }

    #[test]
    fn auto_mode_still_asks_and_plan_mode_refuses_without_asking() {
        let auto = rig("mcp-tool-auto", Mode::Auto, Answer::No);
        auto.tool
            .run(r#"{"action":"add","name":"docs","command":"prog"}"#);
        assert_eq!(auto.asked.borrow().len(), 1, "asked even in auto mode");

        let plan = rig("mcp-tool-plan", Mode::Manual, Answer::Yes);
        plan.approval.set_planning(true);
        let out = plan
            .tool
            .run(r#"{"action":"add","name":"docs","command":"prog"}"#);
        assert!(out.starts_with("plan mode is on"), "{out}");
        assert!(plan.asked.borrow().is_empty());
        assert!(store(&plan).mcp_server("docs").unwrap().is_none());
    }

    #[test]
    fn a_ready_made_toolset_is_listed_and_added_with_its_rules() {
        let r = rig("mcp-tool-toolset", Mode::Manual, Answer::Yes);
        let list = r.tool.run(r#"{"action":"toolsets"}"#);
        assert!(list.contains("rust-docs"), "{list}");
        let out = r.tool.run(r#"{"action":"add_toolset","name":"rust-docs"}"#);
        assert!(
            out.starts_with("added rust-docs: 5 tools run without asking, 3 ask first"),
            "{out}"
        );
        let again = r.tool.run(r#"{"action":"add_toolset","name":"rust-docs"}"#);
        assert!(again.contains("already added"), "{again}");
        assert_eq!(r.asked.borrow().len(), 1, "no second question");
        let unknown = r.tool.run(r#"{"action":"add_toolset","name":"nothing"}"#);
        assert!(
            unknown.starts_with("error: there is no toolset"),
            "{unknown}"
        );
        let server = store(&r).mcp_server("rust-docs").unwrap().unwrap();
        assert_eq!(server.default_rule, Rule::Off);
    }

    #[test]
    fn rules_switches_and_removal_are_asked_and_applied() {
        let r = rig("mcp-tool-change", Mode::Manual, Answer::Yes);
        r.tool
            .run(r#"{"action":"add","name":"docs","command":"prog"}"#);
        let out = r
            .tool
            .run(r#"{"action":"rule","name":"docs","tool":"search","rule":"free"}"#);
        assert!(out.starts_with("docs search: free"), "{out}");
        let out = r
            .tool
            .run(r#"{"action":"rule","name":"docs","tool":"*","rule":"off"}"#);
        assert!(out.starts_with("docs *: off"), "{out}");
        r.tool.run(r#"{"action":"disable","name":"docs"}"#);
        let saved = store(&r).mcp_server("docs").unwrap().unwrap();
        assert!(!saved.enabled);
        assert_eq!(saved.default_rule, Rule::Off);
        let out = r.tool.run(r#"{"action":"remove","name":"docs"}"#);
        assert!(out.starts_with("removed docs"), "{out}");
        assert!(store(&r).mcp_server("docs").unwrap().is_none());
        assert_eq!(r.asked.borrow().len(), 5);
    }

    #[test]
    fn mistakes_are_explained_before_anyone_is_asked() {
        let r = rig("mcp-tool-bad", Mode::Manual, Answer::Yes);
        for (call, expect) in [
            (
                r#"{"action":"add","name":"docs"}"#,
                "needs name and command",
            ),
            (
                r#"{"action":"add","name":"Bad Name","command":"prog"}"#,
                "error:",
            ),
            (
                r#"{"action":"add","name":"docs","command":"prog","timeout":9999}"#,
                "error:",
            ),
            (
                r#"{"action":"rule","name":"ghost","tool":"x","rule":"free"}"#,
                "there is no MCP server 'ghost'",
            ),
            (
                r#"{"action":"rule","name":"ghost","tool":"x","rule":"maybe"}"#,
                "must be ask, free or off",
            ),
            (r#"{"action":"frobnicate"}"#, "unknown action"),
            ("not json", "invalid arguments"),
        ] {
            let out = r.tool.run(call);
            assert!(out.contains(expect), "{call} -> {out}");
        }
        assert!(r.asked.borrow().is_empty());
    }

    #[test]
    fn list_shows_variable_names_and_never_their_values() {
        let r = rig("mcp-tool-list", Mode::Manual, Answer::Yes);
        assert_eq!(
            r.tool.run(r#"{"action":"list"}"#),
            "no MCP servers are saved"
        );
        store(&r)
            .add_mcp_server(
                "docs",
                "prog",
                &["serve".to_owned()],
                &[("MY_KEY".to_owned(), "hunter2".to_owned())],
                30,
            )
            .unwrap();
        let out = r.tool.run(r#"{"action":"list"}"#);
        assert!(out.contains("docs: prog serve"), "{out}");
        assert!(out.contains("variables: MY_KEY"), "{out}");
        assert!(!out.contains("hunter2"), "{out}");
        assert!(r.asked.borrow().is_empty(), "reading never asks");
    }
}

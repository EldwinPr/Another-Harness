//! MCP servers (issue 44): the programs the owner has added for the chat to start, and the rule
//! for each of their tools.
//!
//! Part of the store (`impl Store` continues here); the table layout is owned by `docs/erd.md`.
//! Only the owner adds, changes or removes a server (`/mcp`); the model has no tool for it. A
//! change is saved at once and applies the next time the program starts. The environment values
//! may hold keys: they are stored like a provider's `api_key` and are never printed or logged.

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::store::{Store, StoreError, now_secs};

/// The longest server name, in characters. It is part of every tool name the model sees.
pub const MAX_NAME_CHARS: usize = 20;
/// The shortest and longest time a call may wait, in seconds.
pub const TIMEOUT_RANGE: std::ops::RangeInclusive<u64> = 1..=600;
/// What a server gets to answer a call when the owner says nothing else.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// The name that stands for "every tool not named" in `/mcp rule`.
pub const ALL_TOOLS: &str = "*";

/// What happens when the model calls a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// The owner is asked each time ("always" is offered).
    Ask,
    /// It runs without asking.
    Free,
    /// It is not in the model's tool list.
    Off,
}

impl Rule {
    pub const ALL: [Rule; 3] = [Rule::Ask, Rule::Free, Rule::Off];

    pub fn name(self) -> &'static str {
        match self {
            Rule::Ask => "ask",
            Rule::Free => "free",
            Rule::Off => "off",
        }
    }

    pub fn parse(text: &str) -> Option<Rule> {
        Rule::ALL
            .into_iter()
            .find(|rule| rule.name() == text.trim().to_ascii_lowercase())
    }
}

/// One server as saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    pub id: i64,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// `(NAME, value)` pairs added to the program's own environment (minus the secret-looking
    /// names of that).
    pub vars: Vec<(String, String)>,
    pub timeout_secs: u64,
    pub enabled: bool,
    /// The rule of a tool with no rule of its own.
    pub default_rule: Rule,
}

/// The rule of `tool` of `server`: its own, else the server's default.
pub fn rule_of(server: &McpServer, rules: &[(String, Rule)], tool: &str) -> Rule {
    rules
        .iter()
        .find(|(name, _)| name == tool)
        .map_or(server.default_rule, |(_, rule)| *rule)
}

fn err(text: impl Into<String>) -> StoreError {
    StoreError::new(text)
}

/// A server name as it will be stored: 1 to [`MAX_NAME_CHARS`] lower-case letters, digits or `-`.
pub fn check_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("the name is empty".to_owned());
    }
    if name.chars().count() > MAX_NAME_CHARS
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!(
            "a server name is 1 to {MAX_NAME_CHARS} lower-case letters, digits or '-'"
        ));
    }
    Ok(name.to_owned())
}

/// The program to start: not empty, one line.
pub fn check_command(command: &str) -> Result<String, String> {
    let command = command.trim();
    if command.is_empty() {
        return Err("the command is empty".to_owned());
    }
    if command.chars().any(char::is_control) {
        return Err("the command is one line".to_owned());
    }
    Ok(command.to_owned())
}

fn check_args(args: &[String]) -> Result<(), String> {
    if args.iter().any(|a| a.contains('\0')) {
        return Err("an argument cannot hold a NUL character".to_owned());
    }
    Ok(())
}

/// An environment variable name: letters, digits and `_`, not starting with a digit.
pub fn check_var_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if first_ok && chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(())
    } else {
        Err(format!(
            "'{name}' is not a variable name (letters, digits and '_')"
        ))
    }
}

/// A tool name in a rule: one line, at most 128 characters.
pub fn check_tool(tool: &str) -> Result<String, String> {
    let tool = tool.trim();
    if tool.is_empty() || tool.chars().count() > 128 || tool.chars().any(char::is_control) {
        return Err("a tool name is one line of at most 128 characters".to_owned());
    }
    Ok(tool.to_owned())
}

/// A timeout in seconds, in [`TIMEOUT_RANGE`].
pub fn check_timeout(secs: u64) -> Result<u64, String> {
    if TIMEOUT_RANGE.contains(&secs) {
        Ok(secs)
    } else {
        Err(format!(
            "the timeout is {} to {} seconds",
            TIMEOUT_RANGE.start(),
            TIMEOUT_RANGE.end()
        ))
    }
}

const COLUMNS: &str = "id, name, command, args, env, timeout_secs, enabled, default_rule";

fn read_server(row: &rusqlite::Row<'_>) -> rusqlite::Result<McpServer> {
    let bad = |column: usize, why: String| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(why)),
        )
    };
    let args: String = row.get(3)?;
    let vars: String = row.get(4)?;
    let rule: String = row.get(7)?;
    let vars: std::collections::BTreeMap<String, String> =
        serde_json::from_str(&vars).map_err(|e| bad(4, e.to_string()))?;
    Ok(McpServer {
        id: row.get(0)?,
        name: row.get(1)?,
        command: row.get(2)?,
        args: serde_json::from_str(&args).map_err(|e| bad(3, e.to_string()))?,
        vars: vars.into_iter().collect(),
        timeout_secs: u64::try_from(row.get::<_, i64>(5)?).unwrap_or(DEFAULT_TIMEOUT_SECS),
        enabled: row.get::<_, i64>(6)? == 1,
        default_rule: Rule::parse(&rule).ok_or_else(|| bad(7, format!("unknown rule {rule}")))?,
    })
}

impl Store {
    /// Every saved server, by name.
    pub fn list_mcp_servers(&self) -> Result<Vec<McpServer>, StoreError> {
        let mut statement = self
            .conn
            .prepare(&format!("SELECT {COLUMNS} FROM mcp_servers ORDER BY name"))?;
        let rows = statement.query_map([], read_server)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The server called `name`, if there is one.
    pub fn mcp_server(&self, name: &str) -> Result<Option<McpServer>, StoreError> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM mcp_servers WHERE name = ?1"),
                [name.trim()],
                read_server,
            )
            .optional()?)
    }

    /// Saves a new server, enabled, with every tool on `ask`. The name must be unused.
    pub fn add_mcp_server(
        &mut self,
        name: &str,
        command: &str,
        args: &[String],
        vars: &[(String, String)],
        timeout_secs: u64,
    ) -> Result<McpServer, StoreError> {
        let name = check_name(name).map_err(err)?;
        let command = check_command(command).map_err(err)?;
        check_args(args).map_err(err)?;
        for (key, _) in vars {
            check_var_name(key).map_err(err)?;
        }
        let timeout_secs = check_timeout(timeout_secs).map_err(err)?;
        let var_map: std::collections::BTreeMap<&str, &str> =
            vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let taken: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM mcp_servers WHERE name = ?1)",
            [&name],
            |row| row.get(0),
        )?;
        if taken {
            return Err(err(format!("there is already a server named {name}")));
        }
        let now = now_secs();
        tx.execute(
            "INSERT INTO mcp_servers \
             (name, command, args, env, timeout_secs, enabled, default_rule, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, 1, 'ask', ?6, ?6)",
            params![
                name,
                command,
                serde_json::to_string(args).map_err(|e| err(e.to_string()))?,
                serde_json::to_string(&var_map).map_err(|e| err(e.to_string()))?,
                i64::try_from(timeout_secs).unwrap_or(i64::MAX),
                now
            ],
        )?;
        tx.commit()?;
        self.mcp_server(&name)?
            .ok_or_else(|| err("the server was not saved"))
    }

    /// Removes the server and its rules. False when there was none.
    pub fn remove_mcp_server(&mut self, name: &str) -> Result<bool, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM mcp_tool_rules WHERE server_id IN (SELECT id FROM mcp_servers WHERE name = ?1)",
            [name.trim()],
        )?;
        let changed = tx.execute("DELETE FROM mcp_servers WHERE name = ?1", [name.trim()])?;
        tx.commit()?;
        Ok(changed > 0)
    }

    /// Switches a server on or off for the next start. False when there was none.
    pub fn set_mcp_enabled(&mut self, name: &str, enabled: bool) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "UPDATE mcp_servers SET enabled = ?2, updated_at = ?3 WHERE name = ?1",
            params![name.trim(), i64::from(enabled), now_secs()],
        )?;
        Ok(changed > 0)
    }

    /// Sets the rule of one tool, or of every tool with no rule of its own when `tool` is
    /// [`ALL_TOOLS`]. False when there is no such server.
    pub fn set_mcp_rule(
        &mut self,
        server: &str,
        tool: &str,
        rule: Rule,
    ) -> Result<bool, StoreError> {
        let tool = if tool.trim() == ALL_TOOLS {
            ALL_TOOLS.to_owned()
        } else {
            check_tool(tool).map_err(err)?
        };
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(id) = tx
            .query_row(
                "SELECT id FROM mcp_servers WHERE name = ?1",
                [server.trim()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
        else {
            return Ok(false);
        };
        if tool == ALL_TOOLS {
            tx.execute(
                "UPDATE mcp_servers SET default_rule = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, rule.name(), now_secs()],
            )?;
        } else {
            tx.execute(
                "INSERT INTO mcp_tool_rules (server_id, tool, rule) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (server_id, tool) DO UPDATE SET rule = excluded.rule",
                params![id, tool, rule.name()],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// The rules of the tools of this server that have one of their own, by tool name.
    pub fn mcp_rules(&self, server_id: i64) -> Result<Vec<(String, Rule)>, StoreError> {
        let mut statement = self
            .conn
            .prepare("SELECT tool, rule FROM mcp_tool_rules WHERE server_id = ?1 ORDER BY tool")?;
        let rows = statement.query_map([server_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (tool, rule) = row?;
            let rule = Rule::parse(&rule).ok_or_else(|| err(format!("unknown rule {rule}")))?;
            out.push((tool, rule));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::Scratch;

    fn store(tag: &str) -> (Scratch, Store) {
        let dir = Scratch::new(tag);
        let store = Store::open(&dir.path().join("harness.db")).unwrap();
        (dir, store)
    }

    #[test]
    fn a_server_is_added_enabled_with_every_tool_on_ask_and_found_again() {
        let (_dir, mut store) = store("mcp-add");
        let added = store
            .add_mcp_server(
                "rust-docs",
                "rust-docs-mcp",
                &["serve".to_owned()],
                &[("A_KEY".to_owned(), "v".to_owned())],
                30,
            )
            .unwrap();
        assert_eq!(added.name, "rust-docs");
        assert_eq!(added.args, ["serve"]);
        assert_eq!(added.vars, [("A_KEY".to_owned(), "v".to_owned())]);
        assert_eq!(added.timeout_secs, 30);
        assert!(added.enabled);
        assert_eq!(added.default_rule, Rule::Ask);
        assert_eq!(store.mcp_server("rust-docs").unwrap(), Some(added.clone()));
        assert_eq!(store.list_mcp_servers().unwrap(), [added]);
        assert_eq!(store.mcp_server("other").unwrap(), None);
    }

    #[test]
    fn the_name_command_variables_and_timeout_are_checked() {
        let (_dir, mut store) = store("mcp-checks");
        let add = |store: &mut Store, name: &str, command: &str, vars: &[(&str, &str)], t| {
            let vars: Vec<(String, String)> = vars
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            store.add_mcp_server(name, command, &[], &vars, t)
        };
        for name in ["", "Rust", "a b", "a_b", "x".repeat(21).as_str(), "é"] {
            assert!(add(&mut store, name, "cmd", &[], 60).is_err(), "{name:?}");
        }
        assert!(add(&mut store, "ok", "  ", &[], 60).is_err());
        assert!(add(&mut store, "ok", "a\nb", &[], 60).is_err());
        assert!(add(&mut store, "ok", "cmd", &[("1BAD", "x")], 60).is_err());
        assert!(add(&mut store, "ok", "cmd", &[("A-B", "x")], 60).is_err());
        assert!(add(&mut store, "ok", "cmd", &[], 0).is_err());
        assert!(add(&mut store, "ok", "cmd", &[], 601).is_err());
        assert!(add(&mut store, "ok", "cmd", &[], 600).is_ok());
        assert!(add(&mut store, "ok", "cmd", &[], 60).is_err(), "taken");
        assert_eq!(store.list_mcp_servers().unwrap().len(), 1);
    }

    #[test]
    fn rules_are_per_tool_with_a_default_for_the_rest() {
        let (_dir, mut store) = store("mcp-rules");
        store.add_mcp_server("docs", "cmd", &[], &[], 60).unwrap();
        assert!(store.set_mcp_rule("docs", "*", Rule::Off).unwrap());
        assert!(store.set_mcp_rule("docs", "search", Rule::Free).unwrap());
        assert!(store.set_mcp_rule("docs", "search", Rule::Ask).unwrap());
        assert!(store.set_mcp_rule("docs", "list", Rule::Free).unwrap());
        assert!(!store.set_mcp_rule("nope", "list", Rule::Free).unwrap());
        assert!(store.set_mcp_rule("docs", "a\nb", Rule::Free).is_err());
        let server = store.mcp_server("docs").unwrap().unwrap();
        assert_eq!(server.default_rule, Rule::Off);
        let rules = store.mcp_rules(server.id).unwrap();
        assert_eq!(
            rules,
            [
                ("list".to_owned(), Rule::Free),
                ("search".to_owned(), Rule::Ask)
            ]
        );
        assert_eq!(rule_of(&server, &rules, "list"), Rule::Free);
        assert_eq!(rule_of(&server, &rules, "search"), Rule::Ask);
        assert_eq!(rule_of(&server, &rules, "other"), Rule::Off);
    }

    #[test]
    fn disabling_enabling_and_removing_keep_the_rest_in_order() {
        let (_dir, mut store) = store("mcp-state");
        for name in ["b", "a"] {
            store.add_mcp_server(name, "cmd", &[], &[], 60).unwrap();
        }
        store.set_mcp_rule("a", "t", Rule::Free).unwrap();
        assert!(store.set_mcp_enabled("a", false).unwrap());
        assert!(!store.set_mcp_enabled("zz", false).unwrap());
        let list = store.list_mcp_servers().unwrap();
        assert_eq!(
            list.iter()
                .map(|s| (&*s.name, s.enabled))
                .collect::<Vec<_>>(),
            [("a", false), ("b", true)]
        );
        let a_id = list[0].id;
        assert!(store.remove_mcp_server("a").unwrap());
        assert!(!store.remove_mcp_server("a").unwrap());
        assert_eq!(store.mcp_rules(a_id).unwrap(), []);
        assert_eq!(store.list_mcp_servers().unwrap().len(), 1);
    }

    #[test]
    fn rule_names_parse_in_any_case() {
        assert_eq!(Rule::parse(" FREE "), Some(Rule::Free));
        assert_eq!(Rule::parse("ask"), Some(Rule::Ask));
        assert_eq!(Rule::parse("off"), Some(Rule::Off));
        assert_eq!(Rule::parse("yes"), None);
    }
}

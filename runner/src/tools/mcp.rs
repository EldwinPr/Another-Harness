//! The tools of the MCP servers a chat started (issue 44).
//!
//! Each tool a server listed becomes one tool for the model, named `mcp__{server}__{tool}`, with
//! the server's own description and argument schema. The owner decides per tool what happens when
//! the model calls it: `ask` (a question first, with "always" for the rest of the session), `free`
//! (no question) or `off` (the tool is not given to the model at all). Plan mode refuses `ask`
//! tools and runs `free` ones, like it does for `bash`.

use std::rc::Rc;

use kernel::ToolSpec;

use super::{Approver, SharedMode, Tool, truncate};
use crate::mcp::{McpClient, McpServers, exposed_name};
use crate::store_mcp::{Rule, rule_of};

/// The longest the arguments are shown on the question line, in characters.
const DETAIL_CHARS: usize = 200;

pub struct McpTool {
    spec: ToolSpec,
    server: String,
    tool: String,
    rule: Rule,
    client: Rc<McpClient>,
    approval: SharedMode,
    approver: Rc<dyn Approver>,
}

/// The arguments on one line, cut at [`DETAIL_CHARS`].
fn detail_line(arguments: &str) -> String {
    let one: String = arguments.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= DETAIL_CHARS {
        one
    } else {
        let cut: String = one.chars().take(DETAIL_CHARS).collect();
        format!("{cut}...")
    }
}

impl Tool for McpTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn run(&self, arguments: &str) -> String {
        if self.rule == Rule::Ask {
            let detail = detail_line(arguments);
            let full = (detail.ends_with("...") || arguments.contains('\n')).then_some(arguments);
            let allowed = self.approval.confirm(
                self.approver.as_ref(),
                &format!("call {}: {}?", self.server, self.tool),
                &detail,
                Some((
                    format!("mcp:{}:{}", self.server, self.tool),
                    format!("{}.{}", self.server, self.tool),
                )),
                full,
            );
            if !allowed {
                return self
                    .approval
                    .denial("denied by user: the tool was not called");
            }
        }
        truncate(&self.client.call(&self.tool, arguments))
    }
}

/// The tools of every server that started, one for each tool its owner did not turn off.
pub fn mcp_tools(
    servers: &McpServers,
    approval: &SharedMode,
    approver: &Rc<dyn Approver>,
) -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    for started in &servers.started {
        for def in &started.tools {
            let rule = rule_of(&started.server, &started.rules, &def.name);
            if rule == Rule::Off {
                continue;
            }
            let Some(name) = exposed_name(&started.server.name, &def.name) else {
                continue;
            };
            let text = if def.description.is_empty() {
                format!(
                    "Tool {} of the MCP server {}.",
                    def.name, started.server.name
                )
            } else {
                def.description.clone()
            };
            let description = format!("[mcp {}] {text}", started.server.name);
            let Ok(spec) = ToolSpec::new(name, description, def.schema.clone()) else {
                continue;
            };
            tools.push(Box::new(McpTool {
                spec,
                server: started.server.name.clone(),
                tool: def.name.clone(),
                rule,
                client: Rc::clone(&started.client),
                approval: Rc::clone(approval),
                approver: Rc::clone(approver),
            }));
        }
    }
    tools
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::time::Duration;

    use super::*;
    use crate::cancel::Cancel;
    use crate::mcp::{Started, Transport, tests as kit};
    use crate::store_mcp::McpServer;
    use crate::tools::{Answer, Ask, Mode, shared_mode};

    /// Question, detail, whether always was offered, whether a full text was offered.
    type Asked = Rc<RefCell<Vec<(String, String, bool, bool)>>>;

    struct Scripted {
        answer: Answer,
        asked: Asked,
    }

    impl Approver for Scripted {
        fn approve(&self, ask: &Ask) -> Answer {
            self.asked.borrow_mut().push((
                ask.question.to_owned(),
                ask.detail.to_owned(),
                ask.always.is_some(),
                ask.full.is_some(),
            ));
            self.answer
        }
    }

    fn server(default_rule: Rule) -> McpServer {
        McpServer {
            id: 1,
            name: "t".to_owned(),
            command: "x".to_owned(),
            args: Vec::new(),
            vars: Vec::new(),
            timeout_secs: 5,
            enabled: true,
            default_rule,
        }
    }

    struct Rig {
        tools: Vec<Box<dyn Tool>>,
        approval: SharedMode,
        asked: Asked,
    }

    fn rig(default_rule: Rule, rules: Vec<(String, Rule)>, answer: Answer) -> Rig {
        let (transport, _seen): (Transport, _) = kit::serve(kit::good);
        let client = McpClient::over("t", transport, Duration::from_secs(2), Cancel::new());
        let tools = client.handshake(Duration::from_secs(2)).unwrap();
        let servers = McpServers {
            started: vec![Started {
                server: server(default_rule),
                rules,
                client: Rc::new(client),
                tools,
            }],
            notes: Vec::new(),
        };
        let asked = Rc::new(RefCell::new(Vec::new()));
        let approver: Rc<dyn Approver> = Rc::new(Scripted {
            answer,
            asked: Rc::clone(&asked),
        });
        let approval = shared_mode(Mode::Manual);
        Rig {
            tools: mcp_tools(&servers, &approval, &approver),
            approval,
            asked,
        }
    }

    fn named<'a>(rig: &'a Rig, name: &str) -> &'a dyn Tool {
        rig.tools
            .iter()
            .find(|t| t.spec().name == name)
            .unwrap_or_else(|| panic!("no tool {name}"))
            .as_ref()
    }

    #[test]
    fn each_listed_tool_is_given_under_its_prefixed_name_with_its_own_description() {
        let rig = rig(Rule::Ask, Vec::new(), Answer::Yes);
        let names: Vec<String> = rig.tools.iter().map(|t| t.spec().name.clone()).collect();
        assert_eq!(names, ["mcp__t__echo", "mcp__t__bare"]);
        assert_eq!(
            named(&rig, "mcp__t__echo").spec().description,
            "[mcp t] Echoes"
        );
        assert!(
            named(&rig, "mcp__t__bare")
                .spec()
                .description
                .contains("MCP server t")
        );
    }

    #[test]
    fn a_tool_turned_off_is_not_given_and_the_default_covers_the_rest() {
        let rig = rig(Rule::Off, vec![("echo".to_owned(), Rule::Free)], Answer::No);
        let names: Vec<String> = rig.tools.iter().map(|t| t.spec().name.clone()).collect();
        assert_eq!(names, ["mcp__t__echo"]);
    }

    #[test]
    fn an_ask_tool_asks_with_its_arguments_and_runs_only_on_yes() {
        let yes = rig(Rule::Ask, Vec::new(), Answer::Yes);
        let out = named(&yes, "mcp__t__echo").run(r#"{"text":"hi"}"#);
        assert_eq!(out, "echo:hi");
        let asked = yes.asked.borrow();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].0, "call t: echo?");
        assert_eq!(asked[0].1, r#"{"text":"hi"}"#);
        assert!(asked[0].2, "always is offered");
        assert!(!asked[0].3, "a short line needs no full text");

        let no = rig(Rule::Ask, Vec::new(), Answer::No);
        assert_eq!(
            named(&no, "mcp__t__echo").run("{}"),
            "denied by user: the tool was not called"
        );
    }

    #[test]
    fn long_arguments_are_cut_on_the_question_line_and_kept_whole_behind_view() {
        let rig = rig(Rule::Ask, Vec::new(), Answer::Yes);
        let long = format!(r#"{{"text":"{}"}}"#, "y".repeat(500));
        named(&rig, "mcp__t__echo").run(&long);
        let asked = rig.asked.borrow();
        assert!(asked[0].1.ends_with("..."));
        assert!(asked[0].1.chars().count() <= DETAIL_CHARS + 3);
        assert!(asked[0].3);
    }

    #[test]
    fn always_covers_the_same_tool_for_the_session_and_not_another() {
        let rig = rig(Rule::Ask, Vec::new(), Answer::Always);
        named(&rig, "mcp__t__echo").run("{}");
        named(&rig, "mcp__t__echo").run("{}");
        assert_eq!(rig.asked.borrow().len(), 1);
        named(&rig, "mcp__t__bare").run("{}");
        assert_eq!(rig.asked.borrow().len(), 2);
        assert_eq!(rig.approval.always_labels(), ["t.bare", "t.echo"]);
    }

    #[test]
    fn a_free_tool_never_asks() {
        let rig = rig(Rule::Free, Vec::new(), Answer::No);
        assert_eq!(named(&rig, "mcp__t__echo").run(r#"{"text":"a"}"#), "echo:a");
        assert!(rig.asked.borrow().is_empty());
    }

    #[test]
    fn plan_mode_refuses_an_ask_tool_without_asking_and_runs_a_free_one() {
        let rig = rig(
            Rule::Ask,
            vec![("bare".to_owned(), Rule::Free)],
            Answer::Yes,
        );
        rig.approval.set_planning(true);
        let refused = named(&rig, "mcp__t__echo").run("{}");
        assert!(refused.starts_with("plan mode is on"), "{refused}");
        assert!(rig.asked.borrow().is_empty());
        assert_eq!(named(&rig, "mcp__t__bare").run("{}"), "bare:-");
    }

    #[test]
    fn a_long_result_is_cut_like_a_command_output() {
        let (transport, _) = kit::serve(|m| match m["method"].as_str() {
            Some("tools/call") => vec![kit::reply(
                m,
                serde_json::json!({"content": [{"type": "text", "text": "z".repeat(200_000)}]}),
            )],
            _ => kit::good(m),
        });
        let client = McpClient::over("t", transport, Duration::from_secs(2), Cancel::new());
        let tools = client.handshake(Duration::from_secs(2)).unwrap();
        let servers = McpServers {
            started: vec![Started {
                server: server(Rule::Free),
                rules: Vec::new(),
                client: Rc::new(client),
                tools,
            }],
            notes: Vec::new(),
        };
        let approver: Rc<dyn Approver> = Rc::new(Scripted {
            answer: Answer::Yes,
            asked: Rc::default(),
        });
        let tools = mcp_tools(&servers, &shared_mode(Mode::Manual), &approver);
        let out = tools[0].run("{}");
        assert!(out.len() < 100_000, "{}", out.len());
        assert!(
            out.contains("truncated") || out.contains("omitted"),
            "no marker"
        );
    }
}

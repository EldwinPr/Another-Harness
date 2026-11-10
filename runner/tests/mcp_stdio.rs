//! The MCP client against a real process (issue 44): `src/bin/mcp_fake.rs`.
//!
//! Nothing here touches the network, the real database or a real MCP server.

use std::path::Path;
use std::time::{Duration, Instant};

use runner::cancel::Cancel;
use runner::mcp::{McpClient, ToolDef};
use runner::store_mcp::{McpServer, Rule};

fn fake(mode: &str, vars: &[(&str, &str)], timeout_secs: u64) -> McpServer {
    McpServer {
        id: 1,
        name: "fake".to_owned(),
        command: env!("CARGO_BIN_EXE_mcp_fake").to_owned(),
        args: vec![mode.to_owned()],
        vars: vars
            .iter()
            .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
            .collect(),
        timeout_secs,
        enabled: true,
        default_rule: Rule::Ask,
    }
}

fn start(
    mode: &str,
    vars: &[(&str, &str)],
    timeout_secs: u64,
) -> Result<(std::rc::Rc<McpClient>, Vec<ToolDef>), String> {
    McpClient::start(
        &fake(mode, vars, timeout_secs),
        Path::new("/"),
        Cancel::new(),
    )
}

/// Whether a process with this id still exists (a zombie that nobody waited for counts as gone).
fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|s| !s.contains(") Z "))
        .unwrap_or(false)
}

fn wait_gone(pid: u32) -> bool {
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !alive(pid)
}

#[test]
fn a_real_server_starts_lists_its_tools_and_answers_calls() {
    let (client, tools) = start("good", &[], 10).unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["echo", "env", "pids", "fail", "big"]);
    assert_eq!(client.call("echo", r#"{"text":"hello"}"#), "hello");
    assert_eq!(client.call("fail", "{}"), "error: it failed");
}

#[test]
fn its_environment_has_no_secret_names_but_has_the_owners_own_variables() {
    // The test process's own variables are inherited unless they look secret.
    // SAFETY: set before the child starts, in a test that is the only one reading these names.
    unsafe {
        std::env::set_var("MCPTEST_API_KEY", "leaked");
        std::env::set_var("MCPTEST_PLAIN", "visible");
    }
    let (client, _) = start("good", &[("MCPTEST_OWNER_TOKEN", "given")], 10).unwrap();
    assert_eq!(
        client.call("env", r#"{"name":"MCPTEST_API_KEY"}"#),
        "(unset)"
    );
    assert_eq!(client.call("env", r#"{"name":"MCPTEST_PLAIN"}"#), "visible");
    assert_eq!(
        client.call("env", r#"{"name":"MCPTEST_OWNER_TOKEN"}"#),
        "given"
    );
}

#[test]
fn lines_that_are_not_json_between_answers_are_ignored() {
    let (client, tools) = start("garbage", &[], 10).unwrap();
    assert_eq!(tools.len(), 5);
    assert_eq!(client.call("echo", r#"{"text":"ok"}"#), "ok");
}

#[test]
fn a_long_result_comes_back_whole_for_the_tool_layer_to_cut() {
    let (client, _) = start("good", &[], 10).unwrap();
    assert_eq!(client.call("big", "{}").len(), 200_000);
}

#[test]
fn a_server_that_hangs_on_a_call_times_out_and_the_chat_goes_on() {
    let (client, _) = start("hang", &[], 1).unwrap();
    let started = Instant::now();
    let text = client.call("echo", "{}");
    assert_eq!(text, "error: mcp fake did not answer in 1 seconds");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_server_that_dies_on_a_call_says_so_with_what_it_wrote() {
    let (client, _) = start("exit", &[], 10).unwrap();
    let text = client.call("echo", "{}");
    assert!(
        text.starts_with("error: mcp fake has stopped: it exited"),
        "{text}"
    );
    assert!(text.contains("boom"), "{text}");
    assert!(client.call("echo", "{}").contains("has stopped"));
}

#[test]
fn a_server_that_never_answers_the_handshake_does_not_start_and_is_not_left_running() {
    let started = Instant::now();
    let err = match McpClient::start(&fake("slow", &[], 10), Path::new("/"), Cancel::new()) {
        Ok(_) => panic!("it should not have started"),
        Err(why) => why,
    };
    assert!(err.contains("no answer in"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(20));
}

#[test]
fn a_server_with_another_protocol_version_does_not_start() {
    let err = match start("version", &[], 10) {
        Ok(_) => panic!("it should not have started"),
        Err(why) => why,
    };
    assert!(err.contains("1999-01-01"), "{err}");
}

#[test]
fn a_program_that_does_not_exist_is_reported() {
    let mut server = fake("good", &[], 10);
    server.command = "/nonexistent/mcp-server".to_owned();
    let err = match McpClient::start(&server, Path::new("/"), Cancel::new()) {
        Ok(_) => panic!("it should not have started"),
        Err(why) => why,
    };
    assert!(err.contains("could not run"), "{err}");
}

#[test]
fn dropping_the_client_leaves_no_process_behind_not_even_the_servers_own_child() {
    let (client, _) = start("good", &[], 10).unwrap();
    let ids = client.call("pids", "{}");
    let (server, child) = ids.split_once(' ').unwrap();
    let (server, child): (u32, u32) = (server.parse().unwrap(), child.trim().parse().unwrap());
    assert!(alive(server) && alive(child));
    drop(client);
    assert!(wait_gone(server), "the server is still running");
    assert!(wait_gone(child), "the server's child is still running");
}

/// `/mcp config`: choosing a tool's rule from the menu, with a real server behind it.
#[test]
fn the_menu_sets_the_rule_of_one_tool_of_a_running_server() {
    use runner::agent::{Agent, ScriptedProvider, StoreContext};
    use runner::repl::Terminal;
    use runner::store::Store;
    use runner::style::Style;
    use runner::turn_log::TurnLog;

    let dir = std::env::temp_dir().join(format!(
        "another-harness-test-mcp-menu-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("harness.db");
    {
        let mut store = Store::open(&db).unwrap();
        store
            .add_mcp_server("fake", env!("CARGO_BIN_EXE_mcp_fake"), &[], &[], 10)
            .unwrap();
    }
    // The server, then "tool rules", then the first tool (echo), then free; Esc three times.
    let down = "\x1b[B";
    let typed = format!("\r{down}\r\r{down}\r\x1b\x1b\x1b");
    let term = Terminal::shared_styled(
        std::io::Cursor::new(typed.into_bytes()),
        Vec::new(),
        Style::plain(),
    );
    term.borrow_mut().set_root(dir.clone());
    term.borrow_mut().enable_menus(false);
    let log = TurnLog::create(&dir.join("logs")).unwrap();
    let mut agent = Agent::new(
        "m",
        Box::new(ScriptedProvider::new(vec![])),
        Vec::new(),
        log,
    )
    .with_target("test", "http://x/v1")
    .with_store(
        Store::open(&db).unwrap(),
        StoreContext {
            cwd: dir.display().to_string(),
        },
    );
    runner::mcp_ui::command(&mut agent, &term, "config");
    let screen = String::from_utf8(term.borrow().output().clone()).unwrap();
    let store = Store::open(&db).unwrap();
    let server = store.mcp_server("fake").unwrap().unwrap();
    let rules = store.mcp_rules(server.id).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(rules, [("echo".to_owned(), Rule::Free)], "{screen}");
    assert!(screen.contains("fake: 5 tools"), "{screen}");
}

/// The model's `mcp` tool lists the tools of a saved server by starting it (after a yes).
#[test]
fn the_mcp_tool_starts_a_saved_server_to_list_its_tools_after_a_yes() {
    use runner::tools::{Answer, Approver, Ask, Mode, mcp_config_tool, shared_mode};

    struct Yes;
    impl Approver for Yes {
        fn approve(&self, _ask: &Ask) -> Answer {
            Answer::Yes
        }
    }
    let dir = std::env::temp_dir().join(format!(
        "another-harness-test-mcp-tool-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("harness.db");
    {
        let mut store = runner::store::Store::open(&db).unwrap();
        store
            .add_mcp_server("fake", env!("CARGO_BIN_EXE_mcp_fake"), &[], &[], 10)
            .unwrap();
        store.set_mcp_rule("fake", "echo", Rule::Free).unwrap();
    }
    let tool = mcp_config_tool(
        &db,
        &dir,
        std::rc::Rc::new(Yes),
        shared_mode(Mode::Manual),
        Cancel::new(),
    );
    let out = tool.run(r#"{"action":"tools","name":"fake"}"#);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(out.starts_with("fake offers 5 tools:"), "{out}");
    assert!(
        out.contains("echo [free] ~") && out.contains("Echoes text"),
        "{out}"
    );
    assert!(out.contains("big [ask]"), "{out}");
    assert!(out.contains("on now: 5 of 5 tools"), "{out}");
}

#[test]
fn a_stopped_server_is_started_again_in_place_and_answers_again() {
    let (client, _) = start("good", &[], 10).unwrap();
    assert!(client.stopped().is_none());
    let ids = client.call("pids", "{}");
    let server: u32 = ids.split_once(' ').unwrap().0.parse().unwrap();
    std::process::Command::new("kill")
        .arg("-9")
        .arg(server.to_string())
        .status()
        .unwrap();
    assert!(wait_gone(server));
    // Found without a call having to fail first.
    let until = Instant::now() + Duration::from_secs(3);
    while client.stopped().is_none() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(client.stopped().is_some());
    let text = client.call("echo", r#"{"text":"x"}"#);
    assert!(text.contains("has stopped"), "{text}");
    assert!(text.contains("/mcp restart fake"), "{text}");

    let tools = client.restart().unwrap();
    assert_eq!(tools.len(), 5);
    assert!(client.stopped().is_none());
    assert_eq!(client.call("echo", r#"{"text":"again"}"#), "again");
    // The new program is the only one: restarting leaves no old process behind.
    let ids = client.call("pids", "{}");
    let new_server: u32 = ids.split_once(' ').unwrap().0.parse().unwrap();
    assert_ne!(new_server, server);
}

#[test]
fn the_menu_command_restarts_a_server_of_this_chat() {
    use runner::agent::{Agent, ScriptedProvider, StoreContext};
    use runner::repl::Terminal;
    use runner::store::Store;
    use runner::style::Style;
    use runner::turn_log::TurnLog;

    let dir = std::env::temp_dir().join(format!(
        "another-harness-test-mcp-restart-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("harness.db");
    {
        let mut store = Store::open(&db).unwrap();
        store
            .add_mcp_server("fake", env!("CARGO_BIN_EXE_mcp_fake"), &[], &[], 10)
            .unwrap();
    }
    let store = Store::open(&db).unwrap();
    let started = runner::mcp::McpServers::start_all(&store, &dir, &Cancel::new());
    assert_eq!(started.names(), ["fake"]);
    let term =
        Terminal::shared_styled(std::io::Cursor::new(Vec::new()), Vec::new(), Style::plain());
    term.borrow_mut().set_mcp(started.clients());
    let log = TurnLog::create(&dir.join("logs")).unwrap();
    let mut agent = Agent::new(
        "m",
        Box::new(ScriptedProvider::new(vec![])),
        Vec::new(),
        log,
    )
    .with_target("test", "http://x/v1")
    .with_store(
        store,
        StoreContext {
            cwd: dir.display().to_string(),
        },
    );
    runner::mcp_ui::command(&mut agent, &term, "restart fake");
    runner::mcp_ui::command(&mut agent, &term, "restart ghost");
    runner::mcp_ui::command(&mut agent, &term, "list");
    let screen = String::from_utf8(term.borrow().output().clone()).unwrap();
    drop(started);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        screen.contains("fake is running again (5 tools)"),
        "{screen}"
    );
    assert!(
        screen.contains("ghost is not running in this chat"),
        "{screen}"
    );
    assert!(screen.contains("running in this chat"), "{screen}");
}

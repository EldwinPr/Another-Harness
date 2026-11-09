//! `/web` (issue 29): the web page of the prompts, skills, personalities and glossary, served
//! from inside the chat.
//!
//! `/web [port]` starts the server on a thread of its own (default port 8787; `/web` alone starts it
//! there, or says where it is running), `/web stop` stops it. The thread has its own database connections and
//! prints nothing; the chat is single-threaded and shares nothing with it. It stops when the chat
//! ends (the [`Web`] handle is dropped with the prompt loop).

use std::io::{BufRead, Write};

use crate::agent::Agent;
use crate::config::DEFAULT_WEB_PORT;
use crate::config_ui::{done, error};
use crate::prompts_ui::with_store;
use crate::repl::{Shared, say_line};
use crate::web::Web;

const USAGE: &str = "usage: /web [port] | /web stop";

pub fn command<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    rest: &str,
    web: &mut Option<Web>,
) {
    let style = term.borrow().style();
    // A server whose thread ended by itself is reported once and forgotten.
    if web.as_ref().is_some_and(|w| !w.running()) {
        *web = None;
        error(term, "the web server had stopped by itself");
    }
    match rest.trim() {
        "" => match web {
            Some(w) => say_line(
                term,
                &format!("web: {} (running; /web stop ends it)", w.url()),
            ),
            None => start(agent, term, DEFAULT_WEB_PORT, web),
        },
        "stop" => match web.take() {
            Some(mut w) => {
                w.stop();
                done(term, "web stopped");
            }
            None => say_line(term, &style.dim("web: not running")),
        },
        text => {
            let Ok(port) = text.parse::<u16>() else {
                say_line(term, &style.yellow(USAGE));
                return;
            };
            if let Some(w) = web {
                say_line(
                    term,
                    &style.yellow(&format!(
                        "web is already running at {}: /web stop first",
                        w.url()
                    )),
                );
                return;
            }
            start(agent, term, port, web);
        }
    }
}

fn start<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    port: u16,
    web: &mut Option<Web>,
) {
    let Some(db) = with_store(agent, term, |s| {
        Ok(s.conn.path().map(std::path::PathBuf::from))
    })
    .flatten() else {
        error(term, "this chat has no database file to show");
        return;
    };
    match Web::start(&db, port, &agent.project()) {
        Ok(started) => {
            done(term, &format!("web: {}", started.url()));
            *web = Some(started);
        }
        Err(why) => error(term, &why),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{ScriptedProvider, StoreContext};
    use crate::repl::Terminal;
    use crate::store::Store;
    use crate::style::Style;
    use crate::tools::testutil::Scratch;
    use crate::turn_log::TurnLog;
    use std::io::{Cursor, Read};
    use std::net::{Ipv4Addr, TcpStream};

    type TestTerm = Shared<Cursor<Vec<u8>>, Vec<u8>>;

    fn rig(dir: &Scratch) -> (Agent, TestTerm) {
        let term = Terminal::shared_styled(Cursor::new(Vec::new()), Vec::new(), Style::plain());
        let log = TurnLog::create(&dir.path().join("logs")).unwrap();
        let store = Store::open(&dir.path().join("harness.db")).unwrap();
        let agent = Agent::new(
            "m",
            Box::new(ScriptedProvider::new(vec![])),
            Vec::new(),
            log,
        )
        .with_target("test", "http://x/v1")
        .with_store(
            store,
            StoreContext {
                cwd: dir.path().display().to_string(),
            },
        );
        (agent, term)
    }

    fn screen(term: &TestTerm) -> String {
        String::from_utf8(term.borrow().output().clone()).unwrap()
    }

    fn port_of(web: &Web) -> u16 {
        web.url()
            .trim_start_matches("http://127.0.0.1:")
            .trim_end_matches('/')
            .parse()
            .unwrap()
    }

    fn page(port: u16) -> String {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        stream
            .write_all(format!("GET / HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n").as_bytes())
            .unwrap();
        let mut text = String::new();
        stream.read_to_string(&mut text).unwrap();
        text
    }

    #[test]
    fn web_starts_reports_refuses_a_second_start_and_stops() {
        let dir = Scratch::new("web-ui");
        let (mut agent, term) = rig(&dir);
        let mut web = None;
        command(&mut agent, &term, "0", &mut web);
        let started = web.as_ref().expect("started on a system-chosen port");
        let port = port_of(started);
        assert!(screen(&term).contains(&format!("web: http://127.0.0.1:{port}/")));
        assert!(page(port).starts_with("HTTP/1.1 200 OK"));
        // The thread serves while the chat goes on, and the chat's own writes show up on the page.
        agent
            .store_mut()
            .unwrap()
            .set_glossary("fresh", "a new word", None)
            .unwrap();
        let glossary = {
            let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            stream
                .write_all(
                    format!("GET /glossary HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n").as_bytes(),
                )
                .unwrap();
            let mut text = String::new();
            stream.read_to_string(&mut text).unwrap();
            text
        };
        assert!(glossary.contains("fresh"));
        // Asking again says where it is, and a second port is refused.
        command(&mut agent, &term, "", &mut web);
        assert!(screen(&term).contains("(running; /web stop ends it)"));
        command(&mut agent, &term, "9", &mut web);
        assert!(screen(&term).contains("already running"));
        assert_eq!(port_of(web.as_ref().unwrap()), port);
        // Stop frees the port.
        command(&mut agent, &term, "stop", &mut web);
        assert!(web.is_none());
        assert!(screen(&term).contains("web stopped"));
        assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err());
        command(&mut agent, &term, "stop", &mut web);
        assert!(screen(&term).contains("web: not running"));
    }

    #[test]
    fn a_bad_port_or_a_taken_one_is_reported_and_starts_nothing() {
        let dir = Scratch::new("web-ui-bad");
        let (mut agent, term) = rig(&dir);
        let mut web = None;
        command(&mut agent, &term, "eighty", &mut web);
        command(&mut agent, &term, "70000", &mut web);
        assert!(web.is_none());
        assert_eq!(screen(&term).matches(USAGE).count(), 2);
        let holder = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let taken = holder.local_addr().unwrap().port();
        command(&mut agent, &term, &taken.to_string(), &mut web);
        assert!(web.is_none());
        assert!(screen(&term).contains("cannot listen"));
    }

    #[test]
    fn leaving_the_chat_drops_the_handle_and_stops_the_thread() {
        let dir = Scratch::new("web-ui-drop");
        let (mut agent, term) = rig(&dir);
        let mut web = None;
        command(&mut agent, &term, "0", &mut web);
        let port = port_of(web.as_ref().unwrap());
        drop(web);
        assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err());
    }
}

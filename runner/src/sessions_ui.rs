//! The menu behind `/sessions` (issue 12): the saved chats, and for the one picked, resume or
//! delete. Needs menus switched on ([`Terminal::menus_enabled`]); the caller prints the plain
//! list otherwise.

use std::io::{BufRead, Write};

use crate::agent::Agent;
use crate::config_ui::{done, error};
use crate::menu::MenuItem;
use crate::repl::{Shared, preview, report_resume, say_line};
use crate::turn_log::utc_label;

/// How many chats the menu offers (search narrows them).
const MENU_SESSIONS: usize = 100;

/// The rows of the chat list, and the chats they stand for. `None` after saying why there is
/// nothing to show.
fn chat_rows<R: BufRead, W: Write>(
    agent: &Agent,
    term: &Shared<R, W>,
) -> Option<(Vec<crate::store::SessionSummary>, Vec<MenuItem>)> {
    let style = term.borrow().style();
    let list = match agent.sessions(MENU_SESSIONS) {
        Ok(list) => list,
        Err(why) => {
            error(term, &why);
            return None;
        }
    };
    if list.is_empty() {
        say_line(term, &style.dim("no saved chats yet"));
        return None;
    }
    let current = agent.current_session();
    let rows = list
        .iter()
        .map(|chat| {
            let marker = if Some(chat.id) == current {
                "  (this chat)"
            } else {
                ""
            };
            MenuItem::new(format!("{:>3}", chat.id)).with_detail(format!(
                "{}  {:>3} messages  {}  {}{marker}",
                utc_label(u64::try_from(chat.updated_at).unwrap_or(0)),
                chat.message_count,
                chat.model,
                preview(chat.first_message.as_deref()),
            ))
        })
        .collect();
    Some((list, rows))
}

/// `/resume` in a terminal: pick a chat and it is resumed at once.
pub fn resume_menu<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
    let Some((list, rows)) = chat_rows(agent, term) else {
        return;
    };
    let Some(chosen) = term.borrow_mut().pick("resume which chat?", &rows) else {
        return;
    };
    let id = list[chosen].id;
    if agent.current_session() == Some(id) {
        let style = term.borrow().style();
        return say_line(term, &style.yellow("that is the chat you are in"));
    }
    let resumed = agent.resume(Some(id));
    report_resume(agent, term, resumed);
}

/// `/sessions`: pick a chat, then resume or delete it. Esc goes back a level.
pub fn sessions_menu<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
    loop {
        let style = term.borrow().style();
        let Some((list, rows)) = chat_rows(agent, term) else {
            return;
        };
        let current = agent.current_session();
        let Some(chosen) = term.borrow_mut().pick("saved chats", &rows) else {
            return;
        };
        let chat = &list[chosen];
        let id = chat.id;
        let actions = [
            MenuItem::new("resume").with_detail("continue this chat"),
            MenuItem::new("delete").with_detail("remove it and its messages for good"),
        ];
        // The borrow must end before the arms run: they print through the same terminal.
        let action = term.borrow_mut().pick(&format!("chat {id}"), &actions);
        match action {
            None => {} // back to the list
            Some(0) => {
                if Some(id) == current {
                    say_line(term, &style.yellow("that is the chat you are in"));
                    continue;
                }
                let resumed = agent.resume(Some(id));
                return report_resume(agent, term, resumed);
            }
            Some(_) => {
                if Some(id) == current {
                    error(
                        term,
                        "that chat is the one in use; resume another chat first",
                    );
                    continue;
                }
                let question = format!("delete chat {id} ({} messages)? [y/N]", chat.message_count);
                let sure = term.borrow_mut().ask(&question, None);
                if !matches!(
                    sure.as_deref().map(str::to_ascii_lowercase).as_deref(),
                    Some("y" | "yes")
                ) {
                    say_line(term, &style.yellow("[not deleted]"));
                    continue;
                }
                match agent.delete_session(id) {
                    Ok(()) => done(term, &format!("deleted chat {id}")),
                    Err(why) => error(term, &why),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{NoObserver, ScriptedProvider, StoreContext};
    use crate::repl::Terminal;
    use crate::store::Store;
    use crate::style::Style;
    use crate::tools::testutil::Scratch;
    use crate::turn_log::TurnLog;
    use std::cell::RefCell;
    use std::io::Cursor;
    use std::rc::Rc;

    const DOWN: &str = "\x1b[B";
    const ENTER: &str = "\r";
    const ESC: &str = "\x1b";

    type TestTerm = Shared<Cursor<Vec<u8>>, Vec<u8>>;
    type Requests = Rc<RefCell<Vec<Vec<u8>>>>;

    fn screen(term: &TestTerm) -> String {
        String::from_utf8(term.borrow().output().clone()).unwrap()
    }

    /// An agent saving into the scratch folder; menus on, keys read from `typed`.
    fn rig(dir: &Scratch, typed: &str, replies: &[&str]) -> (Agent, TestTerm, Requests) {
        let term = Terminal::shared_styled(
            Cursor::new(typed.as_bytes().to_vec()),
            Vec::new(),
            Style::plain(),
        );
        term.borrow_mut().enable_menus(false);
        let provider =
            ScriptedProvider::new(replies.iter().map(|r| ScriptedProvider::text(r)).collect());
        let requests = provider.requests();
        let log = TurnLog::create(&dir.path().join("logs")).unwrap();
        let store = Store::open(&dir.path().join("harness.db")).unwrap();
        let agent = Agent::new("m", Box::new(provider), Vec::new(), log)
            .with_target("test", "http://x/v1")
            .with_store(
                store,
                StoreContext {
                    cwd: dir.path().display().to_string(),
                },
            );
        (agent, term, requests)
    }

    /// Saves one chat per question (each with the answer "ok"), as separate program runs.
    fn seed(dir: &Scratch, questions: &[&str]) {
        for question in questions {
            let (mut agent, _term, _) = rig(dir, "", &["ok"]);
            agent.send(question, &mut NoObserver);
        }
    }

    fn count(dir: &Scratch, sql: &str, id: i64) -> i64 {
        Store::open(&dir.path().join("harness.db"))
            .unwrap()
            .conn
            .query_row(sql, [id], |row| row.get(0))
            .unwrap()
    }

    fn chats(dir: &Scratch) -> Vec<i64> {
        let (agent, _term, _) = rig(dir, "", &[]);
        agent.sessions(50).unwrap().iter().map(|c| c.id).collect()
    }

    #[test]
    fn picking_a_chat_and_resume_continues_it_and_shows_it() {
        let dir = Scratch::new("sessions-resume");
        seed(&dir, &["first question", "second question"]);
        // Newest first: chat 2, then chat 1. Pick chat 1, then "resume".
        let typed = [DOWN, ENTER, ENTER].concat();
        let (mut agent, term, requests) = rig(&dir, &typed, &["three"]);
        sessions_menu(&mut agent, &term);
        let out = screen(&term);
        assert!(out.contains("saved chats"), "{out}");
        assert!(out.contains("resumed chat 1, last used "), "{out}");
        assert!(
            out.contains("> first question\nok\n[end of saved chat]\n"),
            "{out}"
        );
        let shown = out.split("resumed chat").nth(1).unwrap();
        assert!(!shown.contains("second question"), "{out}");
        assert_eq!(agent.current_session(), Some(1));
        // The next message carries the resumed chat.
        agent.send("and more", &mut NoObserver);
        let requests = requests.borrow();
        let body: serde_json::Value = serde_json::from_slice(&requests[0]).unwrap();
        assert_eq!(body["messages"][1]["content"], "first question");
        assert_eq!(body["messages"][3]["content"], "and more");
    }

    #[test]
    fn delete_asks_first_and_a_yes_removes_the_chat_and_only_that_chat() {
        let dir = Scratch::new("sessions-delete");
        seed(&dir, &["keep me", "remove me"]);
        // Chat 2 is first: pick it, "delete", answer y; the list comes back; Esc leaves.
        let typed = [ENTER, DOWN, ENTER, "y\n", ESC].concat();
        let (mut agent, term, _) = rig(&dir, &typed, &[]);
        sessions_menu(&mut agent, &term);
        let out = screen(&term);
        assert!(out.contains("delete chat 2 (2 messages)? [y/N]"), "{out}");
        assert!(out.contains("deleted chat 2"), "{out}");
        assert_eq!(chats(&dir), [1]);
        for table in ["messages", "calls"] {
            let sql = format!("SELECT COUNT(*) FROM {table} WHERE session_id = ?1");
            assert_eq!(count(&dir, &sql, 2), 0, "{table}");
            assert!(count(&dir, &sql, 1) > 0, "{table} of the other chat");
        }
    }

    #[test]
    fn anything_but_a_yes_keeps_the_chat() {
        let dir = Scratch::new("sessions-delete-no");
        seed(&dir, &["keep me"]);
        let typed = [
            ENTER, DOWN, ENTER, "
", ENTER, DOWN, ENTER, "n
", ESC,
        ]
        .concat();
        let (mut agent, term, _) = rig(&dir, &typed, &[]);
        sessions_menu(&mut agent, &term);
        assert_eq!(chats(&dir), [1]);
        assert_eq!(screen(&term).matches("[not deleted]").count(), 2);
    }

    #[test]
    fn the_chat_in_use_cannot_be_deleted_or_resumed_over_itself() {
        let dir = Scratch::new("sessions-current");
        let typed = [
            ENTER, DOWN, ENTER, // delete the chat in use: refused
            ENTER, ENTER, // resume it: says so
            ESC,
        ]
        .concat();
        let (mut agent, term, _) = rig(&dir, &typed, &["ok"]);
        agent.send("hello", &mut NoObserver);
        sessions_menu(&mut agent, &term);
        let out = screen(&term);
        assert!(out.contains("(this chat)"), "{out}");
        assert!(out.contains("that chat is the one in use"), "{out}");
        assert!(out.contains("that is the chat you are in"), "{out}");
        assert!(
            !out.contains("delete chat 1"),
            "no question was asked: {out}"
        );
        assert_eq!(chats(&dir), [1]);
        // And the agent itself refuses, whatever the menu does.
        assert!(agent.delete_session(1).is_err());
    }

    #[test]
    fn typing_searches_the_chats() {
        let dir = Scratch::new("sessions-search");
        seed(&dir, &["apple pie", "banana split", "cherry tart"]);
        let typed = ["BANANA", ENTER, ENTER].concat();
        let (mut agent, term, _) = rig(&dir, &typed, &[]);
        sessions_menu(&mut agent, &term);
        assert!(
            screen(&term).contains("> banana split\n"),
            "{}",
            screen(&term)
        );
        assert_eq!(agent.current_session(), Some(2));
    }

    #[test]
    fn escape_goes_back_a_level_and_end_of_input_cancels() {
        let dir = Scratch::new("sessions-cancel");
        seed(&dir, &["one"]);
        let typed = [ENTER, ESC, ESC].concat(); // chat -> actions -> back -> out
        let (mut agent, term, _) = rig(&dir, &typed, &[]);
        sessions_menu(&mut agent, &term);
        assert!(agent.current_session().is_none());
        assert!(!screen(&term).contains("resumed"));
        let (mut agent, term, _) = rig(&dir, "", &[]);
        sessions_menu(&mut agent, &term);
        assert_eq!(chats(&dir), [1]);
    }

    #[test]
    fn with_no_saved_chats_it_says_so() {
        let dir = Scratch::new("sessions-none");
        let (mut agent, term, _) = rig(&dir, "", &[]);
        sessions_menu(&mut agent, &term);
        assert!(screen(&term).contains("no saved chats yet"));
    }

    #[test]
    fn the_store_refuses_an_unknown_chat_and_changes_nothing() {
        let dir = Scratch::new("sessions-unknown");
        seed(&dir, &["one"]);
        let mut store = Store::open(&dir.path().join("harness.db")).unwrap();
        assert!(
            store
                .delete_session(99)
                .unwrap_err()
                .to_string()
                .contains("no saved chat with id 99")
        );
        assert_eq!(chats(&dir), [1]);
        store.delete_session(1).unwrap();
        assert!(chats(&dir).is_empty());
    }

    #[test]
    fn a_chat_is_resumed_from_the_menu_when_it_shows_in_a_colour_terminal_too() {
        let dir = Scratch::new("sessions-colour");
        seed(&dir, &["first question"]);
        let term = Terminal::shared_styled(
            Cursor::new([ENTER, ENTER].concat().into_bytes()),
            Vec::new(),
            Style::colored(),
        );
        term.borrow_mut().enable_menus(false);
        let (mut agent, _plain, _) = rig(&dir, "", &[]);
        sessions_menu(&mut agent, &term);
        let out = screen(&term);
        assert!(out.contains("\x1b[32mresumed chat 1,"), "{out}");
        assert!(out.contains("\x1b[1;36m> \x1b[0mfirst question"), "{out}");
    }

    #[test]
    fn the_resume_picker_resumes_on_enter_with_no_second_step() {
        let dir = Scratch::new("resume-picker");
        seed(&dir, &["first question", "second question"]);
        // Newest first: Down to chat 1, Enter: resumed, nothing else asked.
        let typed = [DOWN, ENTER].concat();
        let (mut agent, term, requests) = rig(&dir, &typed, &["three"]);
        resume_menu(&mut agent, &term);
        let out = screen(&term);
        assert!(out.contains("resume which chat?"), "{out}");
        assert!(!out.contains("delete"), "no delete step: {out}");
        assert!(out.contains("resumed chat 1, last used "), "{out}");
        assert_eq!(agent.current_session(), Some(1));
        agent.send("more", &mut NoObserver);
        assert_eq!(requests.borrow().len(), 1);
    }

    #[test]
    fn the_resume_picker_searches_and_cancels_and_knows_the_chat_in_use() {
        let dir = Scratch::new("resume-picker-more");
        seed(&dir, &["apple pie", "banana split"]);
        let (mut agent, term, _) = rig(&dir, &["apple", ENTER].concat(), &[]);
        resume_menu(&mut agent, &term);
        assert_eq!(agent.current_session(), Some(1));
        // Esc and end of input change nothing.
        let (mut agent, term, _) = rig(&dir, ESC, &[]);
        resume_menu(&mut agent, &term);
        assert!(agent.current_session().is_none());
        let (mut agent, term, _) = rig(&dir, "", &[]);
        resume_menu(&mut agent, &term);
        assert!(agent.current_session().is_none());
        // The chat in use says so.
        let (mut agent, term, _) = rig(&dir, ENTER, &["ok"]);
        agent.send("fresh", &mut NoObserver);
        resume_menu(&mut agent, &term);
        assert!(screen(&term).contains("that is the chat you are in"));
        assert_eq!(agent.current_session(), Some(3));
    }

    #[test]
    fn the_resume_picker_with_no_chats_says_so() {
        let dir = Scratch::new("resume-picker-none");
        let (mut agent, term, _) = rig(&dir, "", &[]);
        resume_menu(&mut agent, &term);
        assert!(screen(&term).contains("no saved chats yet"));
    }

    #[test]
    fn resume_typed_in_the_chat_opens_the_picker_in_a_terminal_and_an_id_skips_it() {
        let dir = Scratch::new("resume-typed");
        seed(&dir, &["first question", "second question"]);
        // Keys for the picker follow the typed line: Down, Enter picks chat 1.
        let typed = format!("/resume\n{DOWN}{ENTER}/exit\n");
        let (mut agent, term, _) = rig(&dir, &typed, &[]);
        crate::repl::run(&mut agent, &term).unwrap();
        assert_eq!(agent.current_session(), Some(1), "{}", screen(&term));
        let (mut agent, term, _) = rig(&dir, "/resume 2\n/exit\n", &[]);
        crate::repl::run(&mut agent, &term).unwrap();
        assert_eq!(agent.current_session(), Some(2));
        assert!(!screen(&term).contains("resume which chat?"));
    }
}

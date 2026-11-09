//! `/skills` and `/personality` (issue 14): the prompts that are added after the user's message
//! instead of going into the system prompt.
//!
//! - A **skill** is off, on for the next message only, or on, per chat. In a terminal `/skills`
//!   is a checklist where Enter cycles off, next message, on.
//! - A **personality** is one prompt selected at a time, or none. Changing it in a chat that
//!   already has replies shows a warning (see [`crate::agent::PERSONALITY_WARNING`]).
//!
//! Both are typed too, terminal or not: `/skills on|off|use <name>`, `/personality <name|none>`.
//! The state is saved with the chat. A new chat starts with no skill and no personality.
//!
//! Each command also manages its own prompts, with the screens of [`crate::prompts_ui`]:
//! `config` (terminal), `add [name [file]]`, `edit <name>`, `delete <name>`.

use std::io::{BufRead, Write};

use crate::agent::Agent;
use crate::config_ui::{done, error};
use crate::menu::MenuItem;
use crate::repl::{Shared, say_line};
use crate::store_prompts::{Prompt, PromptType, SkillState, size_label};

const NO_SKILLS: &str = "no skills yet: /skills add <name> (or /skills config in a terminal)";
const NO_PERSONALITIES: &str =
    "no personalities yet: /personality add <name> (or /personality config in a terminal)";

fn mark(state: SkillState) -> &'static str {
    match state {
        SkillState::Off => "[ ]",
        SkillState::Once => "[1]",
        SkillState::On => "[x]",
    }
}

fn next(state: SkillState) -> SkillState {
    match state {
        SkillState::Off => SkillState::Once,
        SkillState::Once => SkillState::On,
        SkillState::On => SkillState::Off,
    }
}

fn detail(prompt: &Prompt) -> String {
    let mut text = size_label(prompt.chars());
    if !prompt.description.is_empty() {
        text.push_str("  ");
        text.push_str(&prompt.description);
    }
    text
}

/// What is active in this chat, in words.
fn active_line(agent: &Agent) -> String {
    let state = agent.chat_prompts();
    let mut parts = Vec::new();
    if !state.on.is_empty() {
        parts.push(format!("on: {}", state.on.join(", ")));
    }
    if !state.once.is_empty() {
        parts.push(format!("next message only: {}", state.once.join(", ")));
    }
    if parts.is_empty() {
        "no skill is on".to_owned()
    } else {
        format!("skills {}", parts.join("; "))
    }
}

// ---- /skills ----

pub fn skills_command<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, rest: &str) {
    let style = term.borrow().style();
    let menus = term.borrow().menus_enabled();
    let (first, tail) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, ""), |(first, tail)| (first, tail.trim()));
    if crate::prompts_ui::manage(agent, term, PromptType::Skill, first, tail) {
        return;
    }
    let mut words = rest.split_whitespace();
    let (word, name, extra) = (words.next(), words.next(), words.next());
    match (word, name, extra) {
        (None, ..) if menus => skills_checklist(agent, term),
        (None, ..) => show_skills(agent, term),
        (Some(w @ ("on" | "off" | "use")), Some(name), None) => {
            let state = match w {
                "on" => SkillState::On,
                "off" => SkillState::Off,
                _ => SkillState::Once,
            };
            agent.refresh_chat_prompts();
            match agent.set_skill(name, state) {
                Err(why) => error(term, &why),
                Ok(()) => {
                    done(
                        term,
                        &match state {
                            SkillState::Off => format!("{name} is off in this chat"),
                            SkillState::Once => {
                                format!("{name} will be used for your next message only")
                            }
                            SkillState::On => format!("{name} is on in this chat"),
                        },
                    );
                    say_line(term, &style.dim(&active_line(agent)));
                }
            }
        }
        _ => say_line(
            term,
            &style.yellow(
                "usage: /skills [on <name> | off <name> | use <name> | config | add [name [file]] | edit <name> | delete <name>]",
            ),
        ),
    }
}

/// The plain list: one line per skill and what it is doing in this chat.
fn show_skills<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
    let style = term.borrow().style();
    agent.refresh_chat_prompts();
    let skills = match agent.skills() {
        Ok(skills) => skills,
        Err(why) => return error(term, &why),
    };
    if skills.is_empty() {
        return say_line(term, &style.yellow(NO_SKILLS));
    }
    let width = skills
        .iter()
        .map(|(p, _)| p.name.chars().count())
        .max()
        .unwrap_or(0);
    let mut text = format!("{}\n", style.bold("skills in this chat:"));
    for (prompt, state) in &skills {
        let word = match state {
            SkillState::Off => style.dim("off "),
            SkillState::Once => style.yellow("next"),
            SkillState::On => style.green("on  "),
        };
        text.push_str(&format!(
            "  {word}  {:<width$}  {:>11}  {}\n",
            prompt.name,
            size_label(prompt.chars()),
            style.dim(&prompt.description)
        ));
    }
    text.push_str(
        &style.dim("/skills on|off|use <name>: use switches one on for the next message only"),
    );
    say_line(term, &text);
}

/// `/skills` in a terminal: Enter cycles the highlighted skill off, next message, on.
fn skills_checklist<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
    let style = term.borrow().style();
    agent.refresh_chat_prompts();
    let mut start = 0;
    let mut changed = false;
    loop {
        let skills = match agent.skills() {
            Ok(skills) => skills,
            Err(why) => return error(term, &why),
        };
        if skills.is_empty() {
            return say_line(term, &style.yellow(NO_SKILLS));
        }
        let rows: Vec<MenuItem> = skills
            .iter()
            .map(|(p, state)| {
                MenuItem::new(format!("{} {}", mark(*state), p.name)).with_detail(detail(p))
            })
            .collect();
        let chosen = term.borrow_mut().pick_from(
            "skills: Enter cycles off, [1] next message only, [x] on; Esc finishes",
            &rows,
            start,
        );
        let Some(index) = chosen else {
            break;
        };
        let (prompt, state) = &skills[index];
        if let Err(why) = agent.set_skill(&prompt.name, next(*state)) {
            return error(term, &why);
        }
        changed = true;
        start = index;
    }
    if changed {
        say_line(term, &style.green(&active_line(agent)));
    }
}

// ---- /personality ----

pub fn personality_command<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    rest: &str,
) {
    let style = term.borrow().style();
    let menus = term.borrow().menus_enabled();
    let (first, tail) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, ""), |(first, tail)| (first, tail.trim()));
    if crate::prompts_ui::manage(agent, term, PromptType::Personality, first, tail) {
        return;
    }
    let mut words = rest.split_whitespace();
    let (name, extra) = (words.next(), words.next());
    match (name, extra) {
        (None, _) if menus => personality_menu(agent, term),
        (None, _) => show_personalities(agent, term),
        (Some("none"), None) => choose(agent, term, None),
        (Some(name), None) => choose(agent, term, Some(name)),
        _ => say_line(
            term,
            &style.yellow(
                "usage: /personality [<name> | none | config | add [name [file]] | edit <name> | delete <name>]",
            ),
        ),
    }
}

/// Selects the personality (or none) and says so, with the warning when it is a change in a chat
/// that already has replies.
fn choose<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, name: Option<&str>) {
    let style = term.borrow().style();
    agent.refresh_chat_prompts();
    if agent.chat_prompts().personality.as_deref() == name {
        return say_line(
            term,
            &style.dim(&format!(
                "personality is already {}",
                name.unwrap_or("none")
            )),
        );
    }
    match agent.set_personality(name) {
        Err(why) => error(term, &why),
        Ok(warning) => {
            done(
                term,
                &match name {
                    Some(name) => format!("personality: {name}, used from your next message on"),
                    None => "personality: none".to_owned(),
                },
            );
            if let Some(warning) = warning {
                say_line(term, &style.yellow(&format!("warning: {warning}")));
            }
        }
    }
}

fn show_personalities<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
    let style = term.borrow().style();
    agent.refresh_chat_prompts();
    let all = match agent.personalities() {
        Ok(all) => all,
        Err(why) => return error(term, &why),
    };
    if all.is_empty() {
        return say_line(term, &style.yellow(NO_PERSONALITIES));
    }
    let current = agent.chat_prompts().personality.clone();
    let width = all
        .iter()
        .map(|p| p.name.chars().count())
        .max()
        .unwrap_or(0);
    let mut text = format!(
        "{} {}\n",
        style.bold("personality of this chat:"),
        current.as_deref().unwrap_or("none")
    );
    for p in &all {
        let word = if current.as_deref() == Some(p.name.as_str()) {
            style.green("current")
        } else {
            style.dim("       ")
        };
        text.push_str(&format!(
            "  {word}  {:<width$}  {:>11}  {}\n",
            p.name,
            size_label(p.chars()),
            style.dim(&p.description)
        ));
    }
    text.push_str(&style.dim("/personality <name> selects one, /personality none clears it"));
    say_line(term, &text);
}

/// `/personality` in a terminal: pick one, or none.
fn personality_menu<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
    let style = term.borrow().style();
    agent.refresh_chat_prompts();
    let all = match agent.personalities() {
        Ok(all) => all,
        Err(why) => return error(term, &why),
    };
    if all.is_empty() {
        return say_line(term, &style.yellow(NO_PERSONALITIES));
    }
    let current = agent.chat_prompts().personality.clone();
    let radio = |on: bool| if on { "(x)" } else { "( )" };
    let mut rows = vec![MenuItem::new(format!("{} none", radio(current.is_none())))];
    rows.extend(all.iter().map(|p| {
        MenuItem::new(format!(
            "{} {}",
            radio(current.as_deref() == Some(p.name.as_str())),
            p.name
        ))
        .with_detail(detail(p))
    }));
    let start = current
        .as_deref()
        .and_then(|name| all.iter().position(|p| p.name == name))
        .map_or(0, |i| i + 1);
    let title = if agent.has_reply() {
        "personality: this chat already has replies, so a change can be troublesome; Esc keeps it"
    } else {
        "personality of this chat (Esc keeps it)"
    };
    let chosen = term.borrow_mut().pick_from(title, &rows, start);
    match chosen {
        None => {}
        Some(0) => choose(agent, term, None),
        Some(i) => choose(agent, term, Some(&all[i - 1].name)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{NoObserver, ScriptedProvider, StoreContext};
    use crate::repl::{Terminal, run};
    use crate::store::Store;
    use crate::store_prompts::PromptType;
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

    /// An agent saving into the scratch folder with a skill `a`, a skill `b` and the
    /// personalities `pirate` and `caveman`.
    fn rig(
        dir: &Scratch,
        typed: &str,
        replies: &[&str],
        menus: bool,
    ) -> (Agent, TestTerm, Requests) {
        let term = Terminal::shared_styled(
            Cursor::new(typed.as_bytes().to_vec()),
            Vec::new(),
            Style::plain(),
        );
        if menus {
            term.borrow_mut().enable_menus(false);
        }
        let provider =
            ScriptedProvider::new(replies.iter().map(|r| ScriptedProvider::text(r)).collect());
        let requests = provider.requests();
        let log = TurnLog::create(&dir.path().join("logs")).unwrap();
        let mut store = Store::open(&dir.path().join("harness.db")).unwrap();
        for (name, kind) in [
            ("a", PromptType::Skill),
            ("b", PromptType::Skill),
            ("pirate", PromptType::Personality),
            ("caveman", PromptType::Personality),
        ] {
            if store.prompt(name).is_err() {
                store
                    .add_prompt_of_type(name, "", &format!("{name} text"), None, kind)
                    .unwrap();
            }
        }
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

    fn last_message(request: &[u8]) -> String {
        let v: serde_json::Value = serde_json::from_slice(request).unwrap();
        v["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn the_skills_checklist_cycles_off_next_message_on_and_off_again() {
        let dir = Scratch::new("su-checklist");
        // a: Enter (next message). b: Down, Enter (next message), Enter (on). Esc.
        let typed = [ENTER, DOWN, ENTER, ENTER, ESC].concat();
        let (mut agent, term, _) = rig(&dir, &typed, &[], true);
        skills_command(&mut agent, &term, "");
        assert_eq!(agent.chat_prompts().once, ["a"]);
        assert_eq!(agent.chat_prompts().on, ["b"]);
        let out = screen(&term);
        assert!(out.contains("[1] a"), "{out}");
        assert!(out.contains("[x] b"), "{out}");
        assert!(out.contains("skills on: b; next message only: a"), "{out}");
        // Another full turn of the cycle on b comes back to off.
        let (mut agent, term, _) = rig(&dir, &[ENTER, ENTER, ENTER, ESC].concat(), &[], true);
        skills_command(&mut agent, &term, "");
        assert!(agent.chat_prompts().is_empty());
    }

    #[test]
    fn leaving_the_skills_checklist_untouched_says_nothing() {
        let dir = Scratch::new("su-checklist-esc");
        let (mut agent, term, _) = rig(&dir, ESC, &[], true);
        skills_command(&mut agent, &term, "");
        assert!(!screen(&term).contains("no skill is on"));
        assert!(agent.chat_prompts().is_empty());
    }

    #[test]
    fn no_skills_points_to_where_they_are_made() {
        let dir = Scratch::new("su-none");
        let (mut agent, term, _) = rig(&dir, "", &[], true);
        for name in ["a", "b"] {
            agent.store_mut().unwrap().delete_prompt(name).unwrap();
        }
        skills_command(&mut agent, &term, "");
        assert!(screen(&term).contains("no skills yet: /skills add <name>"));
    }

    #[test]
    fn typed_skill_commands_work_without_a_terminal() {
        let dir = Scratch::new("su-typed");
        let typed = "/skills\n\
            /skills on a\n\
            /skills use b\n\
            /skills\n\
            /skills off a\n\
            /skills on nope\n\
            /skills on pirate\n\
            /skills on\n\
            /skills sideways a\n\
            /exit\n";
        let (mut agent, term, _) = rig(&dir, typed, &[], false);
        run(&mut agent, &term).unwrap();
        let out = screen(&term);
        assert!(out.contains("skills in this chat:\n  off   a "), "{out}");
        assert!(out.contains("a is on in this chat"), "{out}");
        assert!(
            out.contains("b will be used for your next message only"),
            "{out}"
        );
        assert!(out.contains("  on    a "), "{out}");
        assert!(out.contains("  next  b "), "{out}");
        assert!(out.contains("a is off in this chat"), "{out}");
        assert!(out.contains("no prompt named nope"), "{out}");
        assert!(
            out.contains("pirate is a personality prompt, not a skill"),
            "{out}"
        );
        assert_eq!(out.matches("usage: /skills").count(), 2, "{out}");
        assert!(agent.chat_prompts().on.is_empty());
        assert_eq!(agent.chat_prompts().once, ["b"]);
    }

    #[test]
    fn a_skill_reaches_the_next_message_after_the_users_text() {
        let dir = Scratch::new("su-e2e");
        let typed = "/skills use a\nfirst\nsecond\n/exit\n";
        let (mut agent, term, requests) = rig(&dir, typed, &["one", "two"], false);
        run(&mut agent, &term).unwrap();
        let requests = requests.borrow();
        assert!(last_message(&requests[0]).contains("## Skill: a\na text"));
        // Used up by the first message.
        assert_eq!(last_message(&requests[1]), "second");
    }

    #[test]
    fn the_personality_menu_selects_one_and_none_and_marks_the_current() {
        let dir = Scratch::new("su-pmenu");
        // Rows: none, pirate, caveman. Down to pirate, Enter.
        let (mut agent, term, _) = rig(&dir, &[DOWN, ENTER].concat(), &[], true);
        personality_command(&mut agent, &term, "");
        assert_eq!(agent.chat_prompts().personality.as_deref(), Some("pirate"));
        let out = screen(&term);
        assert!(out.contains("(x) none"), "{out}");
        assert!(
            out.contains("personality: pirate, used from your next message on"),
            "{out}"
        );
        // The menu opens on the current one (pirate), so Down moves to caveman.
        let (mut agent, term, _) = rig(&dir, &[DOWN, ENTER].concat(), &[], true);
        agent.set_personality(Some("pirate")).unwrap();
        personality_command(&mut agent, &term, "");
        assert_eq!(agent.chat_prompts().personality.as_deref(), Some("caveman"));
        assert!(screen(&term).contains("(x) pirate"), "{}", screen(&term));
        // Esc keeps it; the none row clears it.
        let (mut agent, term, _) = rig(&dir, ESC, &[], true);
        agent.set_personality(Some("pirate")).unwrap();
        personality_command(&mut agent, &term, "");
        assert_eq!(agent.chat_prompts().personality.as_deref(), Some("pirate"));
        let (mut agent, term, _) = rig(&dir, &["\x1b[A", ENTER].concat(), &[], true);
        agent.set_personality(Some("pirate")).unwrap();
        personality_command(&mut agent, &term, "");
        assert_eq!(agent.chat_prompts().personality, None);
        assert!(screen(&term).contains("personality: none"));
    }

    #[test]
    fn changing_the_personality_mid_chat_warns_and_the_first_choice_does_not() {
        let dir = Scratch::new("su-warn");
        let typed = "/personality pirate\nhello\n/personality pirate\n/personality caveman\n/personality none\n/exit\n";
        let (mut agent, term, _) = rig(&dir, typed, &["hi"], false);
        run(&mut agent, &term).unwrap();
        let out = screen(&term);
        assert_eq!(
            out.matches("warning: changing the personality").count(),
            2,
            "{out}"
        );
        assert!(out.contains("personality is already pirate"), "{out}");
        assert!(out.contains("can be troublesome"), "{out}");
        // The first choice came before any reply: no warning in front of "hello".
        let first = out.find("personality: pirate").unwrap();
        let hello = out.find("hi\n").unwrap();
        assert!(!out[first..hello].contains("warning"), "{out}");
    }

    #[test]
    fn the_menu_title_warns_when_the_chat_has_replies() {
        let dir = Scratch::new("su-warn-title");
        let (mut agent, term, _) = rig(&dir, ESC, &["hi"], true);
        agent.send("hello", &mut NoObserver);
        personality_command(&mut agent, &term, "");
        assert!(
            screen(&term).contains("already has replies"),
            "{}",
            screen(&term)
        );
    }

    #[test]
    fn typed_personality_commands_list_set_and_refuse() {
        let dir = Scratch::new("su-ptyped");
        let typed = "/personality\n\
            /personality caveman\n\
            /personality\n\
            /personality a\n\
            /personality nope\n\
            /personality one two\n\
            /exit\n";
        let (mut agent, term, _) = rig(&dir, typed, &[], false);
        run(&mut agent, &term).unwrap();
        let out = screen(&term);
        assert!(out.contains("personality of this chat: none"), "{out}");
        assert!(out.contains("personality of this chat: caveman"), "{out}");
        assert!(out.contains("current  caveman"), "{out}");
        assert!(
            out.contains("a is a skill prompt, not a personality"),
            "{out}"
        );
        assert!(out.contains("no prompt named nope"), "{out}");
        assert!(out.contains("usage: /personality"), "{out}");
    }

    #[test]
    fn a_new_chat_starts_with_no_skill_and_no_personality_and_says_what_it_cleared() {
        let dir = Scratch::new("su-new");
        let typed = "/new\n/skills on b\n/personality caveman\n/new\n/exit\n";
        let (mut agent, term, _) = rig(&dir, typed, &[], false);
        run(&mut agent, &term).unwrap();
        let out = screen(&term);
        // The first /new had nothing to clear, the second did.
        assert!(
            out.contains(&format!(
                "new chat; system prompt: 1 on (base); {}, about {} tokens (estimate)\n",
                crate::store_prompts::size_label(
                    crate::store_prompts::BUILTIN_PROMPT.chars().count()
                ),
                crate::store_prompts::BUILTIN_PROMPT
                    .chars()
                    .count()
                    .div_ceil(4)
            )),
            "{out}"
        );
        assert!(
            out.contains("(estimate); personality and skills cleared"),
            "{out}"
        );
        assert!(agent.chat_prompts().is_empty());
    }

    #[test]
    fn renaming_or_deleting_a_prompt_drops_it_from_the_chat_in_memory_too() {
        let dir = Scratch::new("su-refresh");
        let (mut agent, _term, _) = rig(&dir, "", &["hi"], false);
        agent.set_skill("a", SkillState::On).unwrap();
        agent.set_skill("b", SkillState::Once).unwrap();
        agent.set_personality(Some("pirate")).unwrap();
        agent.send("hello", &mut NoObserver);
        agent.set_skill("b", SkillState::Once).unwrap();
        {
            let store = agent.store_mut().unwrap();
            store.rename_prompt("a", "a2").unwrap();
            store.delete_prompt("b").unwrap();
            store.delete_prompt("pirate").unwrap();
        }
        agent.refresh_chat_prompts();
        // The saved chat follows the prompt by id, so the rename is kept.
        assert_eq!(agent.chat_prompts().on, ["a2"]);
        assert!(agent.chat_prompts().once.is_empty());
        assert_eq!(agent.chat_prompts().personality, None);
    }
}

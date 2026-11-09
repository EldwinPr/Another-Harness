//! `/profile` (issue 31): the starting setups of a chat.
//!
//! A profile holds system prompts, skills that are on and a personality. A new chat starts under
//! the selected profile (`use`); the chat in progress keeps what it began with. In a terminal
//! `/profile` is a menu (pick a profile, then use, edit, copy, rename or delete it; editing is a
//! checklist of prompts, a checklist of skills and a pick of the personality). Typed forms work
//! everywhere: `/profile [list | show <name> | use <name> | add <name> | edit <name> [field ...] |
//! copy <name> <new> | rename <name> <new> | delete <name>]`.

use std::io::{BufRead, Write};

use crate::agent::Agent;
use crate::config_ui::done;
use crate::menu::MenuItem;
use crate::prompts_ui::with_store;
use crate::repl::{Shared, say_line};
use crate::store_profiles::Profile;
use crate::store_prompts::{Prompt, PromptType};

const USAGE: &str = "usage: /profile [list | show <name> | use <name> | add <name> | edit <name> [description <text> | prompts <names> | skills <names> | personality <name|none>] | copy <name> <new> | rename <name> <new> | delete <name>]";

pub fn command<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, rest: &str) {
    let style = term.borrow().style();
    let menus = term.borrow().menus_enabled();
    let words: Vec<&str> = rest.split_whitespace().collect();
    match words.as_slice() {
        [] if menus => menu(agent, term),
        [] | ["list"] => show_list(agent, term),
        ["show", name] => {
            if let Some(profile) = find(agent, term, name) {
                show_profile(agent, term, &profile);
            }
        }
        ["use", name] => use_profile(agent, term, name),
        ["add", name] => add(agent, term, name),
        ["edit", name] if menus => {
            if let Some(profile) = find(agent, term, name) {
                edit_menu(agent, term, &profile.name);
            }
        }
        ["edit", name] => say_line(
            term,
            &style.yellow(&format!(
                "usage: /profile edit {name} description <text> | prompts <names> | skills <names> | personality <name|none>"
            )),
        ),
        ["edit", name, field, values @ ..] => edit_typed(agent, term, name, field, values),
        ["copy", name, new] => {
            if with_store(agent, term, |s| s.copy_profile(name, new)).is_some() {
                done(term, &format!("copied {name} to {new}"));
            }
        }
        ["rename", name, new] => {
            if with_store(agent, term, |s| s.rename_profile(name, new)).is_some() {
                done(term, &format!("renamed {name} to {new}"));
            }
        }
        ["delete", name] => delete(agent, term, name),
        _ => say_line(term, &style.yellow(USAGE)),
    }
}

fn profiles<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) -> Option<Vec<Profile>> {
    with_store(agent, term, |s| s.list_profiles())
}

fn find<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    name: &str,
) -> Option<Profile> {
    with_store(agent, term, |s| s.profile(name))
}

fn selected(agent: &Agent) -> String {
    agent.selected_profile().unwrap_or_default()
}

fn is_selected(selected: &str, profile: &Profile) -> bool {
    selected.eq_ignore_ascii_case(&profile.name)
}

/// "2 prompts, 1 skill, personality pirate", for a list line.
fn counts(profile: &Profile) -> String {
    let plural = |n: usize, one: &str| format!("{n} {one}{}", if n == 1 { "" } else { "s" });
    let mut text = format!(
        "{}, {}",
        plural(profile.prompts.len(), "prompt"),
        plural(profile.skills.len(), "skill")
    );
    if let Some(personality) = &profile.personality {
        text.push_str(&format!(", personality {personality}"));
    }
    text
}

fn show_list<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
    let style = term.borrow().style();
    let Some(all) = profiles(agent, term) else {
        return;
    };
    let current = selected(agent);
    let mut text = format!("{}\n", style.bold("profiles (* starts new chats):"));
    for profile in &all {
        let mark = if is_selected(&current, profile) {
            "*"
        } else {
            " "
        };
        text.push_str(&format!(
            "{mark} {}  {}\n",
            style.bold(&profile.name),
            style.dim(&format!("{} ({})", profile.description, counts(profile)))
        ));
    }
    text.push_str(&style.dim("/profile use <name> picks the one for new chats\n"));
    term.borrow_mut().say(&text);
}

fn list_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_owned()
    } else {
        items.join(", ")
    }
}

fn show_profile<R: BufRead, W: Write>(agent: &Agent, term: &Shared<R, W>, profile: &Profile) {
    let style = term.borrow().style();
    let note = if is_selected(&selected(agent), profile) {
        "  (new chats start under it)"
    } else {
        ""
    };
    let text = format!(
        "{}{note}\n{}\nsystem prompts: {}\nskills on: {}\npersonality: {}\n",
        style.bold(&profile.name),
        profile.description,
        list_or_none(&profile.prompts),
        list_or_none(&profile.skills),
        profile.personality.as_deref().unwrap_or("none"),
    );
    term.borrow_mut().say(&text);
}

fn use_profile<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, name: &str) {
    if with_store(agent, term, |s| s.select_profile(name)).is_none() {
        return;
    }
    let now = selected(agent);
    done(
        term,
        &format!(
            "new chats start under {now}; /new starts one (this chat keeps what it began with)"
        ),
    );
}

fn add<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, name: &str) {
    let Some(description) = term
        .borrow_mut()
        .ask("description (one line, Enter for none)", None)
    else {
        return;
    };
    if with_store(agent, term, |s| s.add_profile(name, description.trim())).is_some() {
        done(
            term,
            &format!("added {name}, empty: /profile edit {name} fills it"),
        );
    }
}

fn delete<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, name: &str) {
    let Some(profile) = find(agent, term, name) else {
        return;
    };
    let style = term.borrow().style();
    let question = format!("delete the profile '{}'? [y/N]", profile.name);
    let sure = term.borrow_mut().ask(&question, None);
    if !matches!(
        sure.as_deref().map(str::to_ascii_lowercase).as_deref(),
        Some("y" | "yes")
    ) {
        return say_line(term, &style.yellow("[not deleted]"));
    }
    if with_store(agent, term, |s| s.delete_profile(&profile.name)).is_some() {
        done(term, &format!("deleted {}", profile.name));
    }
}

/// The names in `values` that are not repeats, in the order given.
fn distinct(values: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for value in values {
        if !out.iter().any(|v| v == value) {
            out.push((*value).to_owned());
        }
    }
    out
}

fn edit_typed<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    name: &str,
    field: &str,
    values: &[&str],
) {
    let style = term.borrow().style();
    let ok = match field {
        "description" => {
            let text = values.join(" ");
            with_store(agent, term, |s| s.set_profile_description(name, &text)).is_some()
        }
        "prompts" => {
            let names = distinct(values);
            with_store(agent, term, |s| s.set_profile_prompts(name, &names)).is_some()
        }
        "skills" => {
            let names = distinct(values);
            with_store(agent, term, |s| s.set_profile_skills(name, &names)).is_some()
        }
        "personality" => {
            let chosen = match values {
                ["none"] => None,
                [one] => Some(*one),
                _ => {
                    say_line(
                        term,
                        &style.yellow(&format!(
                            "usage: /profile edit {name} personality <name|none>"
                        )),
                    );
                    return;
                }
            };
            with_store(agent, term, |s| s.set_profile_personality(name, chosen)).is_some()
        }
        _ => {
            say_line(term, &style.yellow(USAGE));
            return;
        }
    };
    if ok {
        done(term, &format!("saved the changes to {name}"));
    }
}

/// `/profile` in a terminal: the profiles, with "+ add new" at the end; Esc leaves.
fn menu<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
    let mut start = 0;
    loop {
        let Some(all) = profiles(agent, term) else {
            return;
        };
        let current = selected(agent);
        let mut items: Vec<MenuItem> = all
            .iter()
            .map(|p| {
                let mark = if is_selected(&current, p) { "* " } else { "  " };
                MenuItem::new(format!("{mark}{}", p.name)).with_detail(format!(
                    "{} ({})",
                    p.description,
                    counts(p)
                ))
            })
            .collect();
        items.push(MenuItem::action("+ add new"));
        let chosen = term.borrow_mut().pick_from(
            "profiles (* starts new chats): Enter opens one, Esc leaves",
            &items,
            start,
        );
        let Some(chosen) = chosen else {
            return;
        };
        start = chosen;
        if chosen == all.len() {
            let Some(name) = term.borrow_mut().ask("name of the new profile", None) else {
                continue;
            };
            if !name.trim().is_empty() {
                add(agent, term, name.trim());
            }
            continue;
        }
        let profile = &all[chosen];
        show_profile(agent, term, profile);
        let actions = [
            MenuItem::new("use").with_detail("new chats start under it"),
            MenuItem::new("edit").with_detail("prompts, skills, personality, description"),
            MenuItem::new("copy").with_detail("make a new profile like it"),
            MenuItem::new("rename"),
            MenuItem::new("delete"),
        ];
        let action = term.borrow_mut().pick(&profile.name, &actions);
        match action {
            Some(0) => use_profile(agent, term, &profile.name),
            Some(1) => edit_menu(agent, term, &profile.name),
            Some(2) => {
                if let Some(new) = term.borrow_mut().ask("name of the copy", None) {
                    command(
                        agent,
                        term,
                        &format!("copy {} {}", profile.name, new.trim()),
                    );
                }
            }
            Some(3) => {
                if let Some(new) = term.borrow_mut().ask("new name", Some(&profile.name)) {
                    command(
                        agent,
                        term,
                        &format!("rename {} {}", profile.name, new.trim()),
                    );
                }
            }
            Some(_) => delete(agent, term, &profile.name),
            None => {}
        }
    }
}

fn prompts_of<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    kind: PromptType,
) -> Option<Vec<Prompt>> {
    let all = with_store(agent, term, |s| s.list_prompts())?;
    Some(all.into_iter().filter(|p| p.prompt_type == kind).collect())
}

/// What editing a profile offers; Esc goes back.
fn edit_menu<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, name: &str) {
    let mut start = 0;
    loop {
        let Some(profile) = find(agent, term, name) else {
            return;
        };
        let items = [
            MenuItem::new("description").with_detail(profile.description.clone()),
            MenuItem::new("system prompts").with_detail(list_or_none(&profile.prompts)),
            MenuItem::new("skills on").with_detail(list_or_none(&profile.skills)),
            MenuItem::new("personality").with_detail(
                profile
                    .personality
                    .clone()
                    .unwrap_or_else(|| "none".to_owned()),
            ),
        ];
        let title = format!("edit {}: Esc goes back", profile.name);
        let Some(chosen) = term.borrow_mut().pick_from(&title, &items, start) else {
            return;
        };
        start = chosen;
        match chosen {
            0 => {
                if let Some(text) = term
                    .borrow_mut()
                    .ask("description (one line)", Some(&profile.description))
                {
                    let text = text.trim().to_owned();
                    if with_store(agent, term, |s| {
                        s.set_profile_description(&profile.name, &text)
                    })
                    .is_some()
                    {
                        done(term, "saved the description");
                    }
                }
            }
            1 => checklist(agent, term, &profile, PromptType::System),
            2 => checklist(agent, term, &profile, PromptType::Skill),
            _ => pick_personality(agent, term, &profile),
        }
    }
}

/// A checklist of the prompts of one kind: Enter puts one in the profile or takes it out; Esc finishes.
fn checklist<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    profile: &Profile,
    kind: PromptType,
) {
    let style = term.borrow().style();
    let mut start = 0;
    loop {
        let Some(now) = find(agent, term, &profile.name) else {
            return;
        };
        let Some(all) = prompts_of(agent, term, kind) else {
            return;
        };
        if all.is_empty() {
            return say_line(
                term,
                &style.yellow(&format!(
                    "there are no {}s yet: /{}s add",
                    kind.as_str(),
                    if kind == PromptType::System {
                        "prompt"
                    } else {
                        "skill"
                    }
                )),
            );
        }
        let members = if kind == PromptType::System {
            &now.prompts
        } else {
            &now.skills
        };
        let rows: Vec<MenuItem> = all
            .iter()
            .map(|p| {
                let mark = if members.contains(&p.name) {
                    "[x]"
                } else {
                    "[ ]"
                };
                MenuItem::new(format!("{mark} {}", p.name)).with_detail(p.description.clone())
            })
            .collect();
        let title = format!(
            "{} in {}: Enter puts one in or takes it out, Esc finishes",
            kind.as_str(),
            now.name
        );
        let Some(index) = term.borrow_mut().pick_from(&title, &rows, start) else {
            return;
        };
        start = index;
        let name = &all[index].name;
        let mut next: Vec<String> = members.clone();
        if let Some(at) = next.iter().position(|n| n == name) {
            next.remove(at);
        } else {
            next.push(name.clone());
        }
        let saved = if kind == PromptType::System {
            with_store(agent, term, |s| s.set_profile_prompts(&now.name, &next))
        } else {
            with_store(agent, term, |s| s.set_profile_skills(&now.name, &next))
        };
        if saved.is_none() {
            return;
        }
    }
}

fn pick_personality<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    profile: &Profile,
) {
    let style = term.borrow().style();
    let Some(all) = prompts_of(agent, term, PromptType::Personality) else {
        return;
    };
    if all.is_empty() {
        return say_line(
            term,
            &style.yellow("there are no personalities yet: /personality add"),
        );
    }
    let mut rows = vec![MenuItem::new("none")];
    rows.extend(all.iter().map(|p| {
        let mark = if profile.personality.as_deref() == Some(p.name.as_str()) {
            "* "
        } else {
            "  "
        };
        MenuItem::new(format!("{mark}{}", p.name)).with_detail(p.description.clone())
    }));
    let Some(index) = term.borrow_mut().pick("personality of the profile", &rows) else {
        return;
    };
    let chosen = index.checked_sub(1).map(|i| all[i].name.clone());
    if with_store(agent, term, |s| {
        s.set_profile_personality(&profile.name, chosen.as_deref())
    })
    .is_some()
    {
        done(
            term,
            &format!(
                "personality of {} is {}",
                profile.name,
                chosen.as_deref().unwrap_or("none")
            ),
        );
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
    use std::io::Cursor;

    const DOWN: &str = "\x1b[B";
    const ENTER: &str = "\r";
    const ESC: &str = "\x1b";

    type TestTerm = Shared<Cursor<Vec<u8>>, Vec<u8>>;

    fn screen(term: &TestTerm) -> String {
        String::from_utf8(term.borrow().output().clone()).unwrap()
    }

    fn rig(dir: &Scratch, typed: &str, menus: bool) -> (Agent, TestTerm) {
        let term = Terminal::shared_styled(
            Cursor::new(typed.as_bytes().to_vec()),
            Vec::new(),
            Style::plain(),
        );
        if menus {
            term.borrow_mut().enable_menus(false);
        }
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

    fn store(dir: &Scratch) -> Store {
        Store::open(&dir.path().join("harness.db")).unwrap()
    }

    fn run(dir: &Scratch, typed: &str, menus: bool, text: &str) -> String {
        let (mut agent, term) = rig(dir, typed, menus);
        command(&mut agent, &term, text);
        screen(&term)
    }

    #[test]
    fn the_plain_list_marks_the_selected_profile() {
        let dir = Scratch::new("pu-list");
        {
            let mut s = store(&dir);
            s.add_profile("work", "for work").unwrap();
        }
        let out = run(&dir, "", false, "");
        assert!(out.contains("* default"), "{out}");
        assert!(
            out.contains("  work  for work (0 prompts, 0 skills)"),
            "{out}"
        );
        assert!(out.contains("* starts new chats"), "{out}");
        let same = run(&dir, "", false, "list");
        assert_eq!(out, same);
    }

    #[test]
    fn show_says_what_is_in_a_profile() {
        let dir = Scratch::new("pu-show");
        {
            let mut s = store(&dir);
            s.add_prompt_of_type("tdd", "", "T", None, PromptType::Skill)
                .unwrap();
            s.set_profile_skills("default", &["tdd".to_owned()])
                .unwrap();
        }
        let out = run(&dir, "", false, "show DEFAULT");
        assert!(out.contains("(new chats start under it)"), "{out}");
        assert!(out.contains("system prompts: base"), "{out}");
        assert!(out.contains("skills on: tdd"), "{out}");
        assert!(out.contains("personality: none"), "{out}");
        let out = run(&dir, "", false, "show nope");
        assert!(out.contains("there is no profile named nope"), "{out}");
    }

    #[test]
    fn use_selects_for_new_chats_and_a_bad_name_changes_nothing() {
        let dir = Scratch::new("pu-use");
        store(&dir).add_profile("work", "").unwrap();
        let out = run(&dir, "", false, "use work");
        assert!(out.contains("new chats start under work"), "{out}");
        assert_eq!(store(&dir).selected_profile().unwrap(), "work");
        let out = run(&dir, "", false, "use nope");
        assert!(out.contains("there is no profile named nope"), "{out}");
        assert_eq!(store(&dir).selected_profile().unwrap(), "work");
    }

    #[test]
    fn add_asks_for_a_description_and_the_usual_mistakes_are_explained() {
        let dir = Scratch::new("pu-add");
        let out = run(&dir, "plan things\n", false, "add planner2");
        assert!(out.contains("added planner2, empty"), "{out}");
        assert_eq!(
            store(&dir).profile("planner2").unwrap().description,
            "plan things"
        );
        let out = run(&dir, "\n", false, "add planner2");
        assert!(
            out.contains("there is already a profile named planner2"),
            "{out}"
        );
        let out = run(&dir, "\n", false, "add list");
        assert!(out.contains("cannot be a name"), "{out}");
    }

    #[test]
    fn typed_edit_sets_each_field_and_refuses_the_wrong_kind() {
        let dir = Scratch::new("pu-edit");
        {
            let mut s = store(&dir);
            s.add_prompt("extra", "", "E", None).unwrap();
            s.add_prompt_of_type("tdd", "", "T", None, PromptType::Skill)
                .unwrap();
            s.add_prompt_of_type("pirate", "", "A", None, PromptType::Personality)
                .unwrap();
            s.add_profile("work", "").unwrap();
        }
        for (text, expected) in [
            (
                "edit work description for the week",
                "saved the changes to work",
            ),
            (
                "edit work prompts base extra extra",
                "saved the changes to work",
            ),
            ("edit work skills tdd", "saved the changes to work"),
            ("edit work personality pirate", "saved the changes to work"),
        ] {
            let out = run(&dir, "", false, text);
            assert!(out.contains(expected), "{text}: {out}");
        }
        let work = store(&dir).profile("work").unwrap();
        assert_eq!(work.description, "for the week");
        assert_eq!(work.prompts, ["base", "extra"]);
        assert_eq!(work.skills, ["tdd"]);
        assert_eq!(work.personality.as_deref(), Some("pirate"));
        let out = run(&dir, "", false, "edit work personality none");
        assert!(out.contains("saved the changes"), "{out}");
        assert!(store(&dir).profile("work").unwrap().personality.is_none());
        // Wrong kinds and unknown names are refused and change nothing.
        for text in [
            "edit work prompts tdd",
            "edit work skills base",
            "edit work personality tdd",
            "edit work personality a b",
            "edit nope prompts base",
            "edit work colour red",
            "edit work",
        ] {
            let out = run(&dir, "", false, text);
            assert!(!out.contains("saved the changes"), "{text}: {out}");
        }
        assert_eq!(
            store(&dir).profile("work").unwrap().prompts,
            ["base", "extra"]
        );
    }

    #[test]
    fn copy_rename_and_delete_work_and_the_selected_profile_is_kept() {
        let dir = Scratch::new("pu-crud");
        let out = run(&dir, "", false, "copy default second");
        assert!(out.contains("copied default to second"), "{out}");
        assert_eq!(store(&dir).profile("second").unwrap().prompts, ["base"]);
        let out = run(&dir, "", false, "rename second third");
        assert!(out.contains("renamed second to third"), "{out}");
        let out = run(&dir, "n\n", false, "delete third");
        assert!(out.contains("delete the profile 'third'? [y/N]"), "{out}");
        assert!(out.contains("[not deleted]"));
        assert!(store(&dir).profile("third").is_ok());
        let out = run(&dir, "y\n", false, "delete third");
        assert!(out.contains("deleted third"), "{out}");
        assert!(store(&dir).profile("third").is_err());
        let out = run(&dir, "y\n", false, "delete default");
        assert!(out.contains("/profile use another one first"), "{out}");
        assert!(store(&dir).profile("default").is_ok());
        let out = run(&dir, "", false, "frobnicate");
        assert!(out.contains("usage: /profile"), "{out}");
    }

    #[test]
    fn the_menu_opens_a_profile_and_uses_it() {
        let dir = Scratch::new("pu-menu-use");
        store(&dir).add_profile("work", "").unwrap();
        // default, work, + add new: go down to work, Enter, then the first action (use).
        let typed = format!("{DOWN}{ENTER}{ENTER}{ESC}");
        let out = run(&dir, &typed, true, "");
        assert!(out.contains("new chats start under work"), "{out}");
        assert_eq!(store(&dir).selected_profile().unwrap(), "work");
    }

    #[test]
    fn the_edit_menu_flips_a_prompt_and_leaving_changes_nothing_more() {
        let dir = Scratch::new("pu-menu-edit");
        store(&dir).add_prompt("extra", "", "E", None).unwrap();
        // The profile list: Enter on default; actions: down to edit, Enter; edit menu: down to
        // "system prompts", Enter; checklist: down to extra (second), Enter flips it; Esc x3.
        let typed = format!("{ENTER}{DOWN}{ENTER}{DOWN}{ENTER}{DOWN}{ENTER}{ESC}{ESC}{ESC}{ESC}");
        run(&dir, &typed, true, "");
        assert_eq!(
            store(&dir).profile("default").unwrap().prompts,
            ["base", "extra"]
        );
        // Escaping straight out changes nothing.
        let typed = format!("{ENTER}{ESC}{ESC}");
        run(&dir, &typed, true, "");
        assert_eq!(
            store(&dir).profile("default").unwrap().prompts,
            ["base", "extra"]
        );
    }
}

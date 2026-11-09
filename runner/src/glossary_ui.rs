//! `/glossary` (issue 27): the words and definitions the owner keeps.
//!
//! In a terminal it is a menu: pick a word to view, edit or delete it, or add a new one. Typed
//! forms work everywhere: `/glossary [show|add|edit|delete] <term>`, `import <file>` and
//! `export <file>`. A definition is typed as lines ended by a lone `.`, or written in the
//! owner's editor. Nothing here asks the model anything; the model's own way in is the
//! `glossary_propose` tool, which asks the owner (`tools/glossary.rs`).

use std::io::{BufRead, Write};
use std::path::{Component, Path, PathBuf};

use crate::agent::Agent;
use crate::config_ui::{done, error};
use crate::menu::MenuItem;
use crate::prompts_ui::with_store;
use crate::repl::{Shared, say_line};
use crate::store_glossary::{Entry, Saved, check_area, check_term, parse_tables, to_table};
use crate::turn_log::utc_label;

/// How much of a definition a list line shows.
const LIST_CHARS: usize = 80;

const USAGE: &str = "usage: /glossary [show <term> | add <term> | edit <term> | delete <term> | import <file> | export <file>]";

pub fn command<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, rest: &str) {
    let style = term.borrow().style();
    let menus = term.borrow().menus_enabled();
    let (word, tail) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, ""), |(word, tail)| (word, tail.trim()));
    match word {
        "" if menus => menu(agent, term),
        "" => show_list(agent, term),
        "show" | "add" | "edit" | "delete" if tail.is_empty() => say_line(
            term,
            &style.yellow(&format!("usage: /glossary {word} <term>")),
        ),
        "show" => show_one(agent, term, tail),
        "add" => add(agent, term, tail),
        "edit" => edit(agent, term, tail),
        "delete" => delete(agent, term, tail),
        "import" | "export" if tail.is_empty() => say_line(
            term,
            &style.yellow(&format!("usage: /glossary {word} <file>")),
        ),
        "import" => import(agent, term, tail),
        "export" => export(agent, term, tail),
        _ => say_line(term, &style.yellow(USAGE)),
    }
}

/// One line of a definition, cut.
fn first_line(text: &str, limit: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= limit {
        return one_line;
    }
    let mut shown: String = one_line.chars().take(limit).collect();
    shown.push_str("...");
    shown
}

fn entries<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) -> Option<Vec<Entry>> {
    with_store(agent, term, |s| s.list_glossary())
}

fn day(secs: i64) -> String {
    utc_label(u64::try_from(secs).unwrap_or(0))
}

/// The plain list: one line per word.
fn show_list<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
    let style = term.borrow().style();
    let Some(all) = entries(agent, term) else {
        return;
    };
    if all.is_empty() {
        return say_line(
            term,
            &style
                .yellow("the glossary is empty: /glossary add <term>, or /glossary import <file>"),
        );
    }
    let mut text = format!(
        "{}\n",
        style.bold(&format!("glossary, {} words:", all.len()))
    );
    for entry in &all {
        text.push_str(&format!(
            "  {}{}  {}\n",
            style.bold(&entry.term),
            if entry.project.is_empty() {
                " (all)"
            } else {
                ""
            },
            style.dim(&first_line(&entry.definition, LIST_CHARS))
        ));
    }
    term.borrow_mut().say(&text);
}

fn find<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, name: &str) -> Option<Entry> {
    let found = with_store(agent, term, |s| s.glossary_entry(name))?;
    if found.is_none() {
        let style = term.borrow().style();
        say_line(
            term,
            &style.yellow(&format!(
                "there is no word '{name}' in the glossary (see /glossary)"
            )),
        );
    }
    found
}

fn show_entry<R: BufRead, W: Write>(term: &Shared<R, W>, entry: &Entry) {
    let style = term.borrow().style();
    let area = if entry.area.is_empty() {
        String::new()
    } else {
        format!("  [{}]", entry.area)
    };
    let text = format!(
        "{}{area}\n{}\n{}\n",
        style.bold(&entry.term),
        entry.definition,
        style.dim(&format!(
            "{}; added {}, changed {}",
            if entry.project.is_empty() {
                "for every project"
            } else {
                "for this project"
            },
            day(entry.created_at),
            day(entry.updated_at)
        ))
    );
    term.borrow_mut().say(&text);
}

fn show_one<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, name: &str) {
    if let Some(entry) = find(agent, term, name) {
        show_entry(term, &entry);
    }
}

/// Reads a definition from the owner, starting from `initial`. `None` when they back out.
fn read_definition<R: BufRead, W: Write>(
    term: &Shared<R, W>,
    name: &str,
    initial: &str,
) -> Option<String> {
    match term
        .borrow_mut()
        .read_text(&format!("the definition of {name}"), initial)
    {
        Ok(Some(text)) if !text.trim().is_empty() => Some(text),
        Ok(_) => {
            let style = term.borrow().style();
            say_line(term, &style.yellow("[nothing saved]"));
            None
        }
        Err(why) => {
            error(term, &why);
            None
        }
    }
}

/// Asks for the area (Enter keeps `current`). `None` when the input ended or the area is not usable.
fn read_area<R: BufRead, W: Write>(term: &Shared<R, W>, current: &str) -> Option<String> {
    let answer = term
        .borrow_mut()
        .ask("area (a short group name, Enter for none)", Some(current))?;
    let area = if answer.is_empty() {
        current.to_owned()
    } else {
        answer
    };
    match check_area(&area) {
        Ok(area) => Some(area),
        Err(why) => {
            error(term, &why);
            None
        }
    }
}

fn save<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    name: &str,
    scope: &str,
    definition: &str,
    area: &str,
) {
    let saved = with_store(agent, term, |s| {
        s.set_glossary_in(scope, name, definition, Some(area))
    });
    match saved {
        Some(Saved::Added) => done(term, &format!("added {name}")),
        Some(Saved::Changed) => done(term, &format!("saved the changes to {name}")),
        Some(Saved::Same) => done(term, &format!("{name} is unchanged")),
        None => {}
    }
}

fn add<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, typed: &str) {
    let name = if typed.is_empty() {
        let Some(asked) = term.borrow_mut().ask("word", None) else {
            return;
        };
        asked
    } else {
        typed.to_owned()
    };
    let name = match check_term(&name) {
        Ok(name) => name,
        Err(why) => return error(term, &why),
    };
    let Some(definition) = read_definition(term, &name, "") else {
        return;
    };
    let Some(area) = read_area(term, "") else {
        return;
    };
    let Some(everywhere) = term.borrow_mut().ask("for every project? [y/N]", Some("n")) else {
        return;
    };
    let scope = if matches!(everywhere.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        String::new()
    } else {
        agent.project()
    };
    let there = with_store(agent, term, |s| s.list_all_glossary())
        .unwrap_or_default()
        .iter()
        .any(|e| e.project == scope && e.term.eq_ignore_ascii_case(&name));
    if there {
        return error(
            term,
            &format!("'{name}' is already in the glossary for that: /glossary edit {name}"),
        );
    }
    save(agent, term, &name, &scope, &definition, &area);
}

fn edit<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, name: &str) {
    let Some(entry) = find(agent, term, name) else {
        return;
    };
    edit_entry(agent, term, &entry);
}

fn edit_entry<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, entry: &Entry) {
    let Some(definition) = read_definition(term, &entry.term, &entry.definition) else {
        return;
    };
    let Some(area) = read_area(term, &entry.area) else {
        return;
    };
    save(agent, term, &entry.term, &entry.project, &definition, &area);
}

fn delete<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, name: &str) {
    let Some(entry) = find(agent, term, name) else {
        return;
    };
    delete_entry(agent, term, &entry);
}

fn delete_entry<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, entry: &Entry) {
    let style = term.borrow().style();
    let question = format!("delete '{}' from the glossary? [y/N]", entry.term);
    let sure = term.borrow_mut().ask(&question, None);
    if !matches!(
        sure.as_deref().map(str::to_ascii_lowercase).as_deref(),
        Some("y" | "yes")
    ) {
        return say_line(term, &style.yellow("[not deleted]"));
    }
    if with_store(agent, term, |s| {
        s.delete_glossary_in(&entry.project, &entry.term)
    })
    .is_some()
    {
        done(term, &format!("deleted {}", entry.term));
    }
}

/// `/glossary` in a terminal: the words, with "+ add new" at the end; Esc leaves.
fn menu<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) {
    let mut start = 0;
    loop {
        let Some(all) = entries(agent, term) else {
            return;
        };
        let mut items: Vec<MenuItem> = all
            .iter()
            .map(|e| {
                let area = if e.area.is_empty() {
                    String::new()
                } else {
                    format!("[{}] ", e.area)
                };
                MenuItem::new(&e.term)
                    .with_detail(format!("{area}{}", first_line(&e.definition, LIST_CHARS)))
            })
            .collect();
        items.push(MenuItem::action("+ add new"));
        let title = format!("glossary, {} words", all.len());
        let chosen = term.borrow_mut().pick_from(&title, &items, start);
        let Some(chosen) = chosen else {
            return;
        };
        start = chosen;
        if chosen == all.len() {
            add(agent, term, "");
            continue;
        }
        let entry = &all[chosen];
        show_entry(term, entry);
        let actions = [
            MenuItem::new("edit").with_detail("change the definition or the area"),
            MenuItem::new("delete").with_detail("remove it for good"),
        ];
        let action = term.borrow_mut().pick(&entry.term, &actions);
        match action {
            Some(0) => edit_entry(agent, term, entry),
            Some(_) => delete_entry(agent, term, entry),
            None => {}
        }
    }
}

/// A path typed by the owner, below the working directory: relative, no `..`.
fn below_root(root: &Path, typed: &str) -> Result<PathBuf, String> {
    let path = Path::new(typed.trim_matches(|c| c == '"' || c == '\''));
    if path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!(
            "'{typed}' must be a path below the working directory (no absolute path, no ..)"
        ));
    }
    Ok(root.join(path))
}

fn import<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, typed: &str) {
    let style = term.borrow().style();
    let root = term.borrow().root().to_path_buf();
    let path = match below_root(&root, typed) {
        Ok(path) => path,
        Err(why) => return error(term, &why),
    };
    // A link must not lead out of the folder either.
    let inside = path
        .canonicalize()
        .ok()
        .zip(root.canonicalize().ok())
        .is_some_and(|(file, root)| file.starts_with(root) && file.is_file());
    if !inside {
        return error(
            term,
            &format!("'{typed}' is not a file below the working directory"),
        );
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => return error(term, &format!("cannot read '{typed}': {e}")),
    };
    let (pairs, skipped) = parse_tables(&text);
    let (mut added, mut changed, mut kept) = (0, 0, 0);
    let project = agent.project();
    for (name, definition) in pairs {
        let Some(old) = with_store(agent, term, |s| s.glossary_entry(&name)) else {
            return;
        };
        // A word that is new here, or only exists for every project: this project gets its own.
        match old.filter(|e| e.project == project) {
            None => {
                if with_store(agent, term, |s| {
                    s.set_glossary_in(&project, &name, &definition, None)
                })
                .is_none()
                {
                    return;
                }
                added += 1;
            }
            Some(entry) if entry.definition == definition => kept += 1,
            Some(entry) => {
                let question = format!(
                    "replace the definition of '{}'?\n  now: {}\n  file: {}\n[y/N]",
                    entry.term,
                    first_line(&entry.definition, LIST_CHARS),
                    first_line(&definition, LIST_CHARS)
                );
                let yes = term
                    .borrow_mut()
                    .ask(&question, None)
                    .is_some_and(|a| matches!(a.to_ascii_lowercase().as_str(), "y" | "yes"));
                if !yes {
                    kept += 1;
                } else if with_store(agent, term, |s| {
                    s.set_glossary_in(&project, &name, &definition, None)
                })
                .is_none()
                {
                    return;
                } else {
                    changed += 1;
                }
            }
        }
    }
    say_line(
        term,
        &style.green(&format!(
            "[imported from {typed}: {added} added, {changed} changed, {kept} left as they were, {skipped} table rows skipped]"
        )),
    );
}

fn export<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, typed: &str) {
    let root = term.borrow().root().to_path_buf();
    let path = match below_root(&root, typed) {
        Ok(path) => path,
        Err(why) => return error(term, &why),
    };
    let Some(all) = entries(agent, term) else {
        return;
    };
    // The folder must be inside the working directory once links are followed.
    let parent_ok = path
        .parent()
        .and_then(|p| p.canonicalize().ok())
        .zip(root.canonicalize().ok())
        .is_some_and(|(parent, root)| parent.starts_with(root));
    if !parent_ok {
        return error(
            term,
            &format!("'{typed}' is not below the working directory"),
        );
    }
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut file| file.write_all(to_table(&all).as_bytes()));
    match written {
        Ok(()) => done(term, &format!("wrote {} words to {typed}", all.len())),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            error(
                term,
                &format!("'{typed}' already exists; choose a new file name"),
            );
        }
        Err(e) => error(term, &format!("cannot write '{typed}': {e}")),
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
    use std::io::Cursor;

    const DOWN: &str = "\x1b[B";
    const ENTER: &str = "\r";
    const ESC: &str = "\x1b";

    type TestTerm = Shared<Cursor<Vec<u8>>, Vec<u8>>;

    fn screen(term: &TestTerm) -> String {
        String::from_utf8(term.borrow().output().clone()).unwrap()
    }

    /// An agent saving into the scratch folder, a terminal reading `typed`. `menus` switches
    /// the arrow-key menus on.
    fn rig(dir: &Scratch, typed: &str, menus: bool) -> (Agent, TestTerm) {
        let term = Terminal::shared_styled(
            Cursor::new(typed.as_bytes().to_vec()),
            Vec::new(),
            Style::plain(),
        );
        term.borrow_mut().set_root(dir.path().to_path_buf());
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

    /// A store working for the folder the agent works in.
    fn store(dir: &Scratch) -> Store {
        Store::open(&dir.path().join("harness.db"))
            .unwrap()
            .with_project(dir.path().display().to_string())
    }

    fn run(dir: &Scratch, typed: &str, menus: bool, command_text: &str) -> String {
        let (mut agent, term) = rig(dir, typed, menus);
        let _ = NoObserver;
        command(&mut agent, &term, command_text);
        screen(&term)
    }

    #[test]
    fn typed_add_reads_the_definition_until_a_lone_dot_then_the_area() {
        let dir = Scratch::new("glossary-ui-add");
        let out = run(
            &dir,
            "the stock ledger\nof one item\n.\ninventory\n\n",
            false,
            "add kartu stok",
        );
        assert!(out.contains("added kartu stok"), "{out}");
        let entry = store(&dir).glossary_entry("kartu stok").unwrap().unwrap();
        assert_eq!(entry.definition, "the stock ledger\nof one item");
        assert_eq!(entry.area, "inventory");
        // A second add of the same word is refused.
        let out = run(&dir, "x\n.\n\n\n", false, "add KARTU STOK");
        assert!(out.contains("already in the glossary"), "{out}");
    }

    #[test]
    fn a_word_is_for_this_project_or_for_every_project_and_a_project_word_wins() {
        let dir = Scratch::new("glossary-ui-scope");
        // Asked "for every project?", y: the word has no project.
        let out = run(&dir, "shared meaning\n.\n\ny\n", false, "add claim");
        assert!(out.contains("added claim"), "{out}");
        let all = store(&dir).list_all_glossary().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].project, "");
        // Another folder sees it; so does this one, marked as shared.
        let elsewhere = Store::open(&dir.path().join("harness.db"))
            .unwrap()
            .with_project("/another/repo");
        assert_eq!(
            elsewhere
                .glossary_entry("claim")
                .unwrap()
                .unwrap()
                .definition,
            "shared meaning"
        );
        let out = run(&dir, "", false, "");
        assert!(out.contains("claim (all)  shared meaning"), "{out}");
        // This project makes its own version: it wins here, and the other folder keeps the shared one.
        let out = run(&dir, "our meaning\n.\n\n\n", false, "add claim");
        assert!(out.contains("added claim"), "{out}");
        assert_eq!(
            store(&dir)
                .glossary_entry("claim")
                .unwrap()
                .unwrap()
                .definition,
            "our meaning"
        );
        assert_eq!(
            elsewhere
                .glossary_entry("claim")
                .unwrap()
                .unwrap()
                .definition,
            "shared meaning"
        );
        assert_eq!(store(&dir).list_glossary().unwrap().len(), 1);
        assert_eq!(store(&dir).list_all_glossary().unwrap().len(), 2);
        // Deleting the project's word brings the shared one back.
        run(&dir, "y\n", false, "delete claim");
        assert_eq!(
            store(&dir)
                .glossary_entry("claim")
                .unwrap()
                .unwrap()
                .definition,
            "shared meaning"
        );
    }

    #[test]
    fn typed_edit_show_and_the_plain_list() {
        let dir = Scratch::new("glossary-ui-edit");
        store(&dir)
            .set_glossary("claim", "old", Some("billing"))
            .unwrap();
        let out = run(&dir, "new meaning\n.\n\n", false, "edit claim");
        assert!(out.contains("saved the changes to claim"), "{out}");
        let entry = store(&dir).glossary_entry("claim").unwrap().unwrap();
        assert_eq!(entry.definition, "new meaning");
        assert_eq!(entry.area, "billing", "Enter keeps the area");
        let out = run(&dir, "", false, "show claim");
        assert!(out.contains("claim  [billing]\nnew meaning"), "{out}");
        let out = run(&dir, "", false, "");
        assert!(out.contains("glossary, 1 words:"), "{out}");
        assert!(out.contains("claim  new meaning"), "{out}");
        let out = run(&dir, "", false, "show nope");
        assert!(out.contains("there is no word 'nope'"), "{out}");
        let out = run(&dir, "", false, "show");
        assert!(out.contains("usage: /glossary show <term>"), "{out}");
    }

    #[test]
    fn delete_asks_and_only_a_yes_removes_it() {
        let dir = Scratch::new("glossary-ui-delete");
        store(&dir).set_glossary("claim", "x", None).unwrap();
        let out = run(&dir, "n\n", false, "delete claim");
        assert!(
            out.contains("delete 'claim' from the glossary? [y/N]"),
            "{out}"
        );
        assert!(out.contains("[not deleted]"), "{out}");
        assert!(store(&dir).glossary_entry("claim").unwrap().is_some());
        let out = run(&dir, "y\n", false, "delete claim");
        assert!(out.contains("deleted claim"), "{out}");
        assert!(store(&dir).glossary_entry("claim").unwrap().is_none());
    }

    #[test]
    fn the_menu_lists_the_words_and_deletes_one_after_a_yes() {
        let dir = Scratch::new("glossary-ui-menu");
        {
            let mut s = store(&dir);
            s.set_glossary("alpha", "first", None).unwrap();
            s.set_glossary("beta", "second", None).unwrap();
        }
        // Pick beta, "delete" (the second action), answer y, then Esc leaves.
        let typed = [DOWN, ENTER, DOWN, ENTER, "y\n", ESC].concat();
        let out = run(&dir, &typed, true, "");
        assert!(out.contains("glossary, 2 words"), "{out}");
        assert!(out.contains("deleted beta"), "{out}");
        let s = store(&dir);
        assert!(s.glossary_entry("beta").unwrap().is_none());
        assert!(s.glossary_entry("alpha").unwrap().is_some());
    }

    #[test]
    fn import_adds_new_words_asks_before_replacing_and_reports_the_counts() {
        let dir = Scratch::new("glossary-ui-import");
        std::fs::write(
            dir.path().join("vocab.md"),
            "| Term | Meaning |\n|---|---|\n| claim | new claim text |\n| order | a purchase |\n| same | as before |\n| _{x}_ | _{y}_ |\n",
        )
        .unwrap();
        {
            let mut s = store(&dir);
            s.set_glossary("claim", "old claim text", None).unwrap();
            s.set_glossary("same", "as before", None).unwrap();
        }
        // The one conflict (claim) is answered n.
        let out = run(&dir, "n\n", false, "import vocab.md");
        assert!(out.contains("replace the definition of 'claim'?"), "{out}");
        assert!(
            out.contains("1 added, 0 changed, 2 left as they were, 1 table rows skipped"),
            "{out}"
        );
        assert_eq!(
            store(&dir)
                .glossary_entry("claim")
                .unwrap()
                .unwrap()
                .definition,
            "old claim text"
        );
        assert!(store(&dir).glossary_entry("order").unwrap().is_some());
        // Again, answering y this time.
        let out = run(&dir, "y\n", false, "import vocab.md");
        assert!(
            out.contains("0 added, 1 changed, 2 left as they were"),
            "{out}"
        );
        assert_eq!(
            store(&dir)
                .glossary_entry("claim")
                .unwrap()
                .unwrap()
                .definition,
            "new claim text"
        );
    }

    #[test]
    fn import_and_export_stay_below_the_working_directory() {
        let dir = Scratch::new("glossary-ui-paths");
        let out = run(&dir, "", false, "import /etc/passwd");
        assert!(
            out.contains("must be a path below the working directory"),
            "{out}"
        );
        let out = run(&dir, "", false, "import ../x.md");
        assert!(
            out.contains("must be a path below the working directory"),
            "{out}"
        );
        let out = run(&dir, "", false, "import missing.md");
        assert!(
            out.contains("is not a file below the working directory"),
            "{out}"
        );
        let out = run(&dir, "", false, "export ../out.md");
        assert!(
            out.contains("must be a path below the working directory"),
            "{out}"
        );
        // A link that leads out is refused too.
        let outside = Scratch::new("glossary-ui-outside");
        std::fs::write(
            outside.path().join("v.md"),
            "| a | b |\n|---|---|\n| t | d |\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let out = run(&dir, "", false, "import link/v.md");
        assert!(
            out.contains("is not a file below the working directory"),
            "{out}"
        );
        let out = run(&dir, "", false, "export link/out.md");
        assert!(out.contains("is not below the working directory"), "{out}");
        assert!(!outside.path().join("out.md").exists());
    }

    #[test]
    fn export_writes_a_new_table_and_refuses_an_existing_file() {
        let dir = Scratch::new("glossary-ui-export");
        store(&dir)
            .set_glossary("claim", "a request", None)
            .unwrap();
        let out = run(&dir, "", false, "export out/../v.md");
        assert!(out.contains("must be a path below"), "{out}");
        let out = run(&dir, "", false, "export v.md");
        assert!(out.contains("wrote 1 words to v.md"), "{out}");
        let text = std::fs::read_to_string(dir.path().join("v.md")).unwrap();
        assert!(text.contains("| claim | a request |"), "{text}");
        let out = run(&dir, "", false, "export v.md");
        assert!(out.contains("already exists"), "{out}");
    }

    #[test]
    fn an_unknown_word_after_the_command_prints_the_usage() {
        let dir = Scratch::new("glossary-ui-usage");
        let out = run(&dir, "", false, "frobnicate");
        assert!(out.contains("usage: /glossary"), "{out}");
        let out = run(&dir, "", false, "");
        assert!(out.contains("the glossary is empty"), "{out}");
    }
}

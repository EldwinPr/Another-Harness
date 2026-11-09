//! `library` and `profile` (issue 41): the model reads, and with the owner's yes changes, the
//! owner's skills, personalities, system prompts and profiles.
//!
//! Reading never asks. A new item is added switched off (a profile is added without being
//! selected): nothing changes in any chat until the owner switches it on, so adding is asked like
//! a glossary word: in `manual` mode with "always" offered, not asked in `auto`. Changing or
//! deleting something that exists, and any change to a profile that exists, is asked every time,
//! in both modes and without "always": these shape the model's own instructions. The tools
//! cannot switch anything on, select a profile, or move a prompt; that stays with the owner.

use std::path::PathBuf;
use std::rc::Rc;

use kernel::ToolSpec;
use serde::Deserialize;

use super::{Answer, Approver, Ask, SharedMode, Tool};
use crate::store::{Edit, Store};
use crate::store_profiles::{Profile, check_description, check_profile_name};
use crate::store_prompts::{Prompt, PromptType, check_prompt_name, check_prompt_text};

const LIBRARY_SCHEMA: &str = r#"{"type":"object","properties":{"action":{"type":"string","enum":["list","show","add","edit","delete"]},"type":{"type":"string","enum":["skill","personality","prompt"],"description":"prompt is a system prompt"},"name":{"type":"string"},"description":{"type":"string","description":"One line: what it is for"},"text":{"type":"string","description":"The whole text"},"new_name":{"type":"string","description":"edit only: rename"},"reason":{"type":"string","description":"Why, shown to the owner"}},"required":["action","type"]}"#;
const PROFILE_SCHEMA: &str = r#"{"type":"object","properties":{"action":{"type":"string","enum":["list","show","add","edit","delete"]},"name":{"type":"string"},"new_name":{"type":"string","description":"edit only: rename"},"description":{"type":"string"},"prompts":{"type":"array","items":{"type":"string"},"description":"Names of system prompts. edit: replaces the list"},"skills":{"type":"array","items":{"type":"string"},"description":"Names of skills switched on when a chat starts. edit: replaces the list"},"personality":{"type":"string","description":"Name of a personality; empty for none"},"reason":{"type":"string","description":"Why, shown to the owner"}},"required":["action"]}"#;

/// How much of a text the one-line question shows, and the most of a text `show` gives back.
const SHOWN_CHARS: usize = 70;
const SHOWN_TEXT_CHARS: usize = 20_000;
/// The most names `list` gives back.
const LISTED: usize = 100;

/// The two tools, in the order the model is told about them.
pub fn library_tools(
    db: impl Into<PathBuf>,
    approver: Rc<dyn Approver>,
    approval: SharedMode,
) -> Vec<Box<dyn Tool>> {
    let db = db.into();
    vec![
        Box::new(Library {
            spec: ToolSpec::new(
                "library",
                "Read or change the owner's skills, personalities and system prompts (type prompt). Actions: list, show, add, edit, delete. A new one is saved switched off; you cannot switch anything on. The owner is asked before a change.",
                LIBRARY_SCHEMA,
            )
            .expect("the schema constant is valid JSON"),
            guard: Guard::new(db.clone(), approver.clone(), approval.clone()),
        }),
        Box::new(Profiles {
            spec: ToolSpec::new(
                "profile",
                "Read or change the owner's profiles: the setup a chat starts with (system prompts, skills that are on, a personality). Actions: list, show, add, edit, delete. You cannot select a profile. The owner is asked before a change.",
                PROFILE_SCHEMA,
            )
            .expect("the schema constant is valid JSON"),
            guard: Guard::new(db, approver, approval),
        }),
    ]
}

/// What both tools share: the database and how the owner is asked.
struct Guard {
    db: PathBuf,
    approver: Rc<dyn Approver>,
    approval: SharedMode,
}

impl Guard {
    fn new(db: PathBuf, approver: Rc<dyn Approver>, approval: SharedMode) -> Self {
        Self {
            db,
            approver,
            approval,
        }
    }

    fn open(&self) -> Result<Store, String> {
        Store::open(&self.db).map_err(|e| format!("error: cannot open the database: {e}"))
    }

    /// For adding something new: not asked in auto mode, "always" offered otherwise.
    fn ask_to_add(&self, what: (&str, &str), question: &str, detail: &str, full: &str) -> bool {
        self.approval
            .confirm_saving(&*self.approver, what, (question, detail, full))
    }

    /// For changing or deleting what exists: asked every time, in both modes.
    fn ask_each_time(&self, question: &str, detail: &str, full: &str) -> bool {
        if self.approval.planning() {
            return false;
        }
        self.approver.approve(&Ask {
            question,
            detail,
            always: None,
            full: Some(full),
        }) != Answer::No
    }
}

/// A text on one line, cut to `limit` characters.
fn cut(text: &str, limit: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= limit {
        return one_line;
    }
    let mut shown: String = one_line.chars().take(limit).collect();
    shown.push_str("...");
    shown
}

fn stale(what: &str) -> String {
    format!("error: the {what} changed while you were asking; read it again")
}

// ---- library ----

struct Library {
    spec: ToolSpec,
    guard: Guard,
}

#[derive(Deserialize)]
struct LibraryArgs {
    action: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    name: String,
    description: Option<String>,
    text: Option<String>,
    new_name: Option<String>,
    #[serde(default)]
    reason: String,
}

fn kind_of(word: &str) -> Result<PromptType, String> {
    match word {
        "skill" => Ok(PromptType::Skill),
        "personality" => Ok(PromptType::Personality),
        "prompt" => Ok(PromptType::System),
        other => Err(format!(
            "error: type is skill, personality or prompt, not '{other}'"
        )),
    }
}

/// What the owner calls this kind.
fn noun(kind: PromptType) -> &'static str {
    match kind {
        PromptType::System => "system prompt",
        PromptType::Skill => "skill",
        PromptType::Personality => "personality",
    }
}

/// The existing item of this kind, or why there is none.
fn find(store: &Store, name: &str, kind: PromptType) -> Result<Prompt, String> {
    if name.trim().is_empty() {
        return Err("error: give a name".to_owned());
    }
    let prompt = store.prompt(name).map_err(|e| format!("error: {e}"))?;
    if prompt.prompt_type != kind {
        return Err(format!(
            "error: {name} is a {}, not a {}",
            noun(prompt.prompt_type),
            noun(kind)
        ));
    }
    Ok(prompt)
}

impl Library {
    fn go(&self, arguments: &str) -> Result<String, String> {
        let args: LibraryArgs = serde_json::from_str(arguments)
            .map_err(|e| format!("error: invalid arguments: {e}"))?;
        let kind = kind_of(&args.kind)?;
        let mut store = self.guard.open()?;
        match args.action.as_str() {
            "list" => Ok(list(&store, kind)?),
            "show" => {
                let p = find(&store, &args.name, kind)?;
                Ok(show(&p))
            }
            "add" => self.add(&mut store, &args, kind),
            "edit" => self.edit(&mut store, &args, kind),
            "delete" => self.delete(&mut store, &args, kind),
            other => Err(format!(
                "error: action is list, show, add, edit or delete, not '{other}'"
            )),
        }
    }

    fn add(
        &self,
        store: &mut Store,
        args: &LibraryArgs,
        kind: PromptType,
    ) -> Result<String, String> {
        let name = args.name.trim();
        check_prompt_name(name).map_err(|e| format!("error: {e}"))?;
        let text = args.text.as_deref().unwrap_or_default();
        check_prompt_text(text).map_err(|e| format!("error: {e}"))?;
        if store.prompt(name).is_ok() {
            return Err(format!(
                "error: there is already a prompt named {name}; use edit to change it"
            ));
        }
        let description = cut(args.description.as_deref().unwrap_or_default(), 200);
        let what = noun(kind);
        let detail = format!("{what} {name}: {}", cut(text, SHOWN_CHARS * 2));
        let full = format!(
            "{what}: {name}\nDescription: {description}\n\n{text}\n\nReason: {}",
            args.reason.trim()
        );
        if !self.guard.ask_to_add(
            ("library", "new skills, prompts and personalities"),
            &format!("add this {what}? (it is saved switched off)"),
            &detail,
            &full,
        ) {
            return Err(self
                .guard
                .approval
                .denial("denied by user: nothing was changed"));
        }
        store
            .add_prompt_of_type(name, &description, text, None, kind)
            .map_err(|e| format!("error: {e}"))?;
        Ok(format!(
            "added {what} '{name}', switched off; the owner turns it on"
        ))
    }

    fn edit(
        &self,
        store: &mut Store,
        args: &LibraryArgs,
        kind: PromptType,
    ) -> Result<String, String> {
        let old = find(store, &args.name, kind)?;
        let name = args
            .new_name
            .as_deref()
            .map_or(old.name.as_str(), str::trim);
        let description = args
            .description
            .as_deref()
            .map_or(old.description.clone(), |d| cut(d, 200));
        let text = args.text.as_deref().unwrap_or(&old.text);
        check_prompt_name(name).map_err(|e| format!("error: {e}"))?;
        check_prompt_text(text).map_err(|e| format!("error: {e}"))?;
        if name == old.name && description == old.description && text == old.text {
            return Ok(format!("no change: '{}' already says this", old.name));
        }
        let what = noun(kind);
        let mut changes = Vec::new();
        if name != old.name {
            changes.push(format!("name \"{}\" -> \"{name}\"", old.name));
        }
        if description != old.description {
            changes.push("description".to_owned());
        }
        if text != old.text {
            changes.push(format!(
                "text \"{}\" -> \"{}\"",
                cut(&old.text, SHOWN_CHARS / 2),
                cut(text, SHOWN_CHARS / 2)
            ));
        }
        let full = format!(
            "{what}: {}\n\nNow: {}\n\nProposed: {text}\n\nReason: {}",
            old.name,
            old.text,
            args.reason.trim()
        );
        if !self.guard.ask_each_time(
            &format!("change this {what}?"),
            &format!("{}: {}", old.name, changes.join("; ")),
            &full,
        ) {
            return Err(self
                .guard
                .approval
                .denial("denied by user: nothing was changed"));
        }
        match store
            .edit_prompt(old.id, &old.fingerprint(), name, &description, text)
            .map_err(|e| format!("error: {e}"))?
        {
            Edit::Done => Ok(format!("changed {what} '{}'", old.name)),
            Edit::Stale => Err(stale(what)),
            Edit::Gone => Err(format!("error: {what} '{}' is gone", old.name)),
        }
    }

    fn delete(
        &self,
        store: &mut Store,
        args: &LibraryArgs,
        kind: PromptType,
    ) -> Result<String, String> {
        let old = find(store, &args.name, kind)?;
        let what = noun(kind);
        let full = format!(
            "{what}: {}\n\n{}\n\nReason: {}",
            old.name,
            old.text,
            args.reason.trim()
        );
        if !self.guard.ask_each_time(
            &format!("delete this {what}?"),
            &format!("{}: {}", old.name, cut(&old.text, SHOWN_CHARS)),
            &full,
        ) {
            return Err(self
                .guard
                .approval
                .denial("denied by user: nothing was changed"));
        }
        match store
            .delete_prompt_checked(old.id, &old.fingerprint())
            .map_err(|e| format!("error: {e}"))?
        {
            Edit::Done => Ok(format!("deleted {what} '{}'", old.name)),
            Edit::Stale => Err(stale(what)),
            Edit::Gone => Err(format!("error: {what} '{}' is already gone", old.name)),
        }
    }
}

fn list(store: &Store, kind: PromptType) -> Result<String, String> {
    let all: Vec<Prompt> = store
        .list_prompts()
        .map_err(|e| format!("error: {e}"))?
        .into_iter()
        .filter(|p| p.prompt_type == kind)
        .collect();
    if all.is_empty() {
        let plural = if kind == PromptType::Personality {
            "personalities".to_owned()
        } else {
            format!("{}s", noun(kind))
        };
        return Ok(format!("no {plural} saved"));
    }
    let mut lines: Vec<String> = all
        .iter()
        .take(LISTED)
        .map(|p| {
            let on = if kind == PromptType::System {
                if p.enabled { " [on]" } else { " [off]" }
            } else {
                ""
            };
            let about = if p.description.is_empty() {
                cut(&p.text, SHOWN_CHARS)
            } else {
                p.description.clone()
            };
            format!("{}{on}: {about} ({} chars)", p.name, p.chars())
        })
        .collect();
    if all.len() > LISTED {
        lines.push(format!("... and {} more", all.len() - LISTED));
    }
    Ok(lines.join("\n"))
}

fn show(p: &Prompt) -> String {
    let text: String = p.text.chars().take(SHOWN_TEXT_CHARS).collect();
    let cut_note = if p.chars() > SHOWN_TEXT_CHARS {
        format!("\n[cut: {} of {} characters]", SHOWN_TEXT_CHARS, p.chars())
    } else {
        String::new()
    };
    let on = if p.prompt_type == PromptType::System {
        if p.enabled { ", on" } else { ", off" }
    } else {
        ""
    };
    format!(
        "{} ({}{on})\nDescription: {}\n\n{text}{cut_note}",
        p.name,
        noun(p.prompt_type),
        p.description
    )
}

impl Tool for Library {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn run(&self, arguments: &str) -> String {
        self.go(arguments).unwrap_or_else(|text| text)
    }
}

// ---- profile ----

struct Profiles {
    spec: ToolSpec,
    guard: Guard,
}

#[derive(Deserialize)]
struct ProfileArgs {
    action: String,
    #[serde(default)]
    name: String,
    new_name: Option<String>,
    description: Option<String>,
    prompts: Option<Vec<String>>,
    skills: Option<Vec<String>>,
    personality: Option<String>,
    #[serde(default)]
    reason: String,
}

/// A personality given as an empty string means none.
fn personality_of(given: Option<&str>) -> Option<Option<String>> {
    given.map(|p| Some(p.trim().to_owned()).filter(|p| !p.is_empty()))
}

/// Every name must be an existing item of the right kind; says which one is not.
fn check_members(
    store: &Store,
    prompts: &[String],
    skills: &[String],
    personality: Option<&str>,
) -> Result<(), String> {
    let groups: [(&[String], PromptType); 2] =
        [(prompts, PromptType::System), (skills, PromptType::Skill)];
    for (names, kind) in groups {
        for name in names {
            find(store, name, kind)?;
        }
    }
    if let Some(name) = personality {
        find(store, name, PromptType::Personality)?;
    }
    Ok(())
}

fn find_profile(store: &Store, name: &str) -> Result<Profile, String> {
    if name.trim().is_empty() {
        return Err("error: give a name".to_owned());
    }
    store.profile(name).map_err(|e| format!("error: {e}"))
}

fn describe(p: &Profile) -> String {
    format!(
        "{}\nDescription: {}\nSystem prompts: {}\nSkills on: {}\nPersonality: {}",
        p.name,
        p.description,
        or_none(&p.prompts.join(", ")),
        or_none(&p.skills.join(", ")),
        p.personality.as_deref().unwrap_or("none")
    )
}

fn or_none(text: &str) -> &str {
    if text.is_empty() { "none" } else { text }
}

impl Profiles {
    fn go(&self, arguments: &str) -> Result<String, String> {
        let args: ProfileArgs = serde_json::from_str(arguments)
            .map_err(|e| format!("error: invalid arguments: {e}"))?;
        let mut store = self.guard.open()?;
        match args.action.as_str() {
            "list" => {
                let selected = store.selected_profile().unwrap_or_default();
                let all = store.list_profiles().map_err(|e| format!("error: {e}"))?;
                Ok(all
                    .iter()
                    .map(|p| {
                        let mark = if p.name.eq_ignore_ascii_case(&selected) {
                            " (new chats start under it)"
                        } else {
                            ""
                        };
                        format!("{}{mark}: {}", p.name, cut(&p.description, SHOWN_CHARS))
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "show" => Ok(describe(&find_profile(&store, &args.name)?)),
            "add" => self.add(&mut store, &args),
            "edit" => self.edit(&mut store, &args),
            "delete" => self.delete(&mut store, &args),
            other => Err(format!(
                "error: action is list, show, add, edit or delete, not '{other}'"
            )),
        }
    }

    fn add(&self, store: &mut Store, args: &ProfileArgs) -> Result<String, String> {
        let name = args.name.trim();
        check_profile_name(name).map_err(|e| format!("error: {e}"))?;
        let description = check_description(args.description.as_deref().unwrap_or_default())
            .map_err(|e| format!("error: {e}"))?;
        if store.profile(name).is_ok() {
            return Err(format!(
                "error: there is already a profile named {name}; use edit to change it"
            ));
        }
        let prompts = args.prompts.clone().unwrap_or_default();
        let skills = args.skills.clone().unwrap_or_default();
        let personality = personality_of(args.personality.as_deref()).flatten();
        check_members(store, &prompts, &skills, personality.as_deref())?;
        let full = format!(
            "Profile: {name}\nDescription: {description}\nSystem prompts: {}\nSkills on: {}\nPersonality: {}\n\nReason: {}",
            or_none(&prompts.join(", ")),
            or_none(&skills.join(", ")),
            personality.as_deref().unwrap_or("none"),
            args.reason.trim()
        );
        if !self.guard.ask_to_add(
            ("profile", "new profiles"),
            "add this profile? (it is not selected)",
            &format!("{name}: {}", cut(&description, SHOWN_CHARS)),
            &full,
        ) {
            return Err(self
                .guard
                .approval
                .denial("denied by user: nothing was changed"));
        }
        store
            .add_profile(name, &description)
            .map_err(|e| format!("error: {e}"))?;
        let filled = store
            .set_profile_prompts(name, &prompts)
            .and_then(|()| store.set_profile_skills(name, &skills))
            .and_then(|()| store.set_profile_personality(name, personality.as_deref()));
        if let Err(why) = filled {
            // A half-made profile is worse than none.
            let _ = store.delete_profile(name);
            return Err(format!("error: {why}; the profile was not kept"));
        }
        Ok(format!(
            "added profile '{name}'; it is not selected: the owner chooses it with /profile use"
        ))
    }

    fn edit(&self, store: &mut Store, args: &ProfileArgs) -> Result<String, String> {
        let old = find_profile(store, &args.name)?;
        let name = args
            .new_name
            .as_deref()
            .map_or(old.name.as_str(), str::trim);
        check_profile_name(name).map_err(|e| format!("error: {e}"))?;
        let description =
            check_description(args.description.as_deref().unwrap_or(&old.description))
                .map_err(|e| format!("error: {e}"))?;
        let prompts = args.prompts.clone().unwrap_or_else(|| old.prompts.clone());
        let skills = args.skills.clone().unwrap_or_else(|| old.skills.clone());
        let personality = match personality_of(args.personality.as_deref()) {
            Some(given) => given,
            None => old.personality.clone(),
        };
        check_members(store, &prompts, &skills, personality.as_deref())?;
        let mut changes = Vec::new();
        if name != old.name {
            changes.push(format!("name \"{}\" -> \"{name}\"", old.name));
        }
        if description != old.description {
            changes.push("description".to_owned());
        }
        if prompts != old.prompts {
            changes.push(format!("system prompts: {}", or_none(&prompts.join(", "))));
        }
        if skills != old.skills {
            changes.push(format!("skills: {}", or_none(&skills.join(", "))));
        }
        if personality != old.personality {
            changes.push(format!(
                "personality: {}",
                personality.as_deref().unwrap_or("none")
            ));
        }
        if changes.is_empty() {
            return Ok(format!("no change: '{}' already is this", old.name));
        }
        let full = format!(
            "Now:\n{}\n\nProposed:\nName: {name}\nDescription: {description}\nSystem prompts: {}\nSkills on: {}\nPersonality: {}\n\nReason: {}",
            describe(&old),
            or_none(&prompts.join(", ")),
            or_none(&skills.join(", ")),
            personality.as_deref().unwrap_or("none"),
            args.reason.trim()
        );
        if !self.guard.ask_each_time(
            "change this profile?",
            &format!("{}: {}", old.name, changes.join("; ")),
            &full,
        ) {
            return Err(self
                .guard
                .approval
                .denial("denied by user: nothing was changed"));
        }
        match store
            .edit_profile(
                old.id,
                &old.fingerprint(),
                name,
                &description,
                &prompts,
                &skills,
                personality.as_deref(),
            )
            .map_err(|e| format!("error: {e}"))?
        {
            Edit::Done => Ok(format!("changed profile '{}'", old.name)),
            Edit::Stale => Err(stale("profile")),
            Edit::Gone => Err(format!("error: profile '{}' is gone", old.name)),
        }
    }

    fn delete(&self, store: &mut Store, args: &ProfileArgs) -> Result<String, String> {
        let old = find_profile(store, &args.name)?;
        if store
            .selected_profile()
            .is_ok_and(|s| s.eq_ignore_ascii_case(&old.name))
        {
            return Err(format!(
                "error: {} is the profile new chats start under: the owner selects another with /profile use first",
                old.name
            ));
        }
        if !self.guard.ask_each_time(
            "delete this profile?",
            &old.name,
            &format!("{}\n\nReason: {}", describe(&old), args.reason.trim()),
        ) {
            return Err(self
                .guard
                .approval
                .denial("denied by user: nothing was changed"));
        }
        match store
            .delete_profile_checked(old.id, &old.fingerprint())
            .map_err(|e| format!("error: {e}"))?
        {
            Edit::Done => Ok(format!("deleted profile '{}'", old.name)),
            Edit::Stale => Err(stale("profile")),
            Edit::Gone => Err(format!("error: profile '{}' is already gone", old.name)),
        }
    }
}

impl Tool for Profiles {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn run(&self, arguments: &str) -> String {
        self.go(arguments).unwrap_or_else(|text| text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::Scratch;
    use crate::tools::{Mode, shared_mode};
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// What was asked: the question, whether "always" was offered.
    type Asked = Rc<RefCell<Vec<(String, bool)>>>;

    struct Scripted {
        answers: RefCell<VecDeque<Answer>>,
        asked: Asked,
    }

    impl Approver for Scripted {
        fn approve(&self, ask: &Ask) -> Answer {
            self.asked
                .borrow_mut()
                .push((ask.question.to_owned(), ask.always.is_some()));
            self.answers.borrow_mut().pop_front().unwrap_or(Answer::No)
        }
    }

    fn rig(tag: &str, mode: Mode, answers: &[Answer]) -> (Scratch, Vec<Box<dyn Tool>>, Asked) {
        let dir = Scratch::new(tag);
        let asked: Asked = Rc::default();
        let approver = Rc::new(Scripted {
            answers: RefCell::new(answers.iter().copied().collect()),
            asked: Rc::clone(&asked),
        });
        let tools = library_tools(dir.path().join("harness.db"), approver, shared_mode(mode));
        (dir, tools, asked)
    }

    fn call(tools: &[Box<dyn Tool>], tool: &str, args: serde_json::Value) -> String {
        tools
            .iter()
            .find(|t| t.spec().name == tool)
            .unwrap()
            .run(&args.to_string())
    }

    fn lib(action: &str, kind: &str, name: &str) -> serde_json::Value {
        serde_json::json!({"action": action, "type": kind, "name": name, "text": "Do the thing.", "description": "for things"})
    }

    fn store_of(dir: &Scratch) -> Store {
        Store::open(&dir.path().join("harness.db")).unwrap()
    }

    #[test]
    fn a_new_skill_is_asked_with_always_offered_and_saved_switched_off() {
        let (dir, tools, asked) = rig("lib-add", Mode::Manual, &[Answer::Yes]);
        let said = call(&tools, "library", lib("add", "skill", "tidy"));
        assert!(said.starts_with("added skill 'tidy'"), "{said}");
        assert_eq!(asked.borrow().len(), 1);
        assert!(asked.borrow()[0].1, "always is offered for an addition");
        let saved = store_of(&dir).prompt("tidy").unwrap();
        assert_eq!(saved.prompt_type, PromptType::Skill);
        assert!(!saved.enabled);
        assert_eq!(saved.description, "for things");
        // Listing and showing never ask.
        let listed = call(&tools, "library", lib("list", "skill", ""));
        assert!(listed.contains("tidy: for things"), "{listed}");
        assert!(call(&tools, "library", lib("show", "skill", "tidy")).contains("Do the thing."));
        assert_eq!(asked.borrow().len(), 1);
    }

    #[test]
    fn a_no_adds_nothing_and_a_name_in_use_or_a_bad_text_is_refused_before_asking() {
        let (dir, tools, asked) = rig("lib-refuse", Mode::Manual, &[Answer::No]);
        let said = call(&tools, "library", lib("add", "personality", "calm"));
        assert!(said.starts_with("denied by user"), "{said}");
        assert!(store_of(&dir).prompt("calm").is_err());
        let again = call(&tools, "library", lib("add", "prompt", "base"));
        assert!(again.contains("already a prompt named base"), "{again}");
        let bad = call(&tools, "library", lib("add", "skill", "bad name"));
        assert!(bad.starts_with("error:"), "{bad}");
        let empty = call(
            &tools,
            "library",
            serde_json::json!({"action": "add", "type": "skill", "name": "x", "text": " "}),
        );
        assert!(empty.starts_with("error:"), "{empty}");
        assert_eq!(
            asked.borrow().len(),
            1,
            "only the first one reached the owner"
        );
    }

    #[test]
    fn auto_mode_adds_without_asking_but_a_change_and_a_delete_always_ask_without_always() {
        let (dir, tools, asked) = rig(
            "lib-auto",
            Mode::Auto,
            &[Answer::Yes, Answer::Always, Answer::No],
        );
        assert!(call(&tools, "library", lib("add", "skill", "one")).starts_with("added"));
        assert!(
            asked.borrow().is_empty(),
            "auto mode adds without a question"
        );
        let edit = serde_json::json!({"action": "edit", "type": "skill", "name": "one", "text": "New text."});
        assert_eq!(call(&tools, "library", edit.clone()), "changed skill 'one'");
        // "Always" is not a way out of the next question.
        let edit2 = serde_json::json!({"action": "edit", "type": "skill", "name": "one", "new_name": "uno"});
        assert_eq!(call(&tools, "library", edit2), "changed skill 'one'");
        assert_eq!(store_of(&dir).prompt("uno").unwrap().text, "New text.");
        let gone = call(&tools, "library", lib("delete", "skill", "uno"));
        assert!(gone.starts_with("denied"), "{gone}");
        assert!(store_of(&dir).prompt("uno").is_ok());
        let asked = asked.borrow();
        assert_eq!(asked.len(), 3);
        assert!(asked.iter().all(|(_, always)| !always));
        assert_eq!(asked[0].0, "change this skill?");
        assert_eq!(asked[2].0, "delete this skill?");
    }

    #[test]
    fn plan_mode_refuses_every_change_even_in_auto_and_asks_nobody() {
        let dir = Scratch::new("lib-plan");
        let asked: Asked = Rc::default();
        let approval = shared_mode(Mode::Auto);
        let approver = Rc::new(Scripted {
            answers: RefCell::new([Answer::Yes, Answer::Yes].into()),
            asked: Rc::clone(&asked),
        });
        let tools = library_tools(dir.path().join("harness.db"), approver, approval.clone());
        assert!(call(&tools, "library", lib("add", "skill", "one")).starts_with("added"));
        approval.set_planning(true);
        let add = call(&tools, "library", lib("add", "skill", "two"));
        assert!(
            add.starts_with("plan mode is on, so nothing was changed"),
            "{add}"
        );
        let edit =
            serde_json::json!({"action": "edit", "type": "skill", "name": "one", "text": "New."});
        assert!(call(&tools, "library", edit).starts_with("plan mode is on"));
        let gone = call(
            &tools,
            "profile",
            serde_json::json!({"action": "delete", "name": "x"}),
        );
        assert!(
            gone.contains("no profile") || gone.starts_with("error"),
            "{gone}"
        );
        assert!(asked.borrow().is_empty());
        assert!(store_of(&dir).prompt("two").is_err());
        assert_eq!(store_of(&dir).prompt("one").unwrap().text, "Do the thing.");
        // Reading still works.
        assert!(call(&tools, "library", lib("show", "skill", "one")).contains("Do the thing."));
    }

    #[test]
    fn the_kind_must_match_and_nothing_is_switched_on_by_the_tool() {
        let (dir, tools, _) = rig("lib-kind", Mode::Auto, &[]);
        call(&tools, "library", lib("add", "prompt", "style"));
        let wrong = call(&tools, "library", lib("show", "skill", "style"));
        assert!(wrong.contains("is a system prompt, not a skill"), "{wrong}");
        assert!(!store_of(&dir).prompt("style").unwrap().enabled);
        let listed = call(&tools, "library", lib("list", "prompt", ""));
        assert!(listed.contains("style [off]"), "{listed}");
        let none = call(&tools, "library", lib("list", "personality", ""));
        assert_eq!(none, "no personalities saved");
        let nonsense = call(&tools, "library", lib("burn", "skill", "x"));
        assert!(
            nonsense.contains("action is list, show, add, edit or delete"),
            "{nonsense}"
        );
        let kind = call(&tools, "library", lib("list", "recipe", ""));
        assert!(
            kind.contains("type is skill, personality or prompt"),
            "{kind}"
        );
    }

    #[test]
    fn a_profile_is_added_not_selected_and_its_names_must_exist() {
        let (dir, tools, asked) = rig("prof-add", Mode::Auto, &[]);
        call(&tools, "library", lib("add", "skill", "tidy"));
        call(&tools, "library", lib("add", "personality", "calm"));
        let missing = call(
            &tools,
            "profile",
            serde_json::json!({"action": "add", "name": "work", "skills": ["nope"]}),
        );
        assert!(
            missing.contains("there is no prompt named nope"),
            "{missing}"
        );
        let said = call(
            &tools,
            "profile",
            serde_json::json!({"action": "add", "name": "work", "description": "daily", "prompts": ["base"], "skills": ["tidy"], "personality": "calm"}),
        );
        assert!(said.starts_with("added profile 'work'"), "{said}");
        let made = store_of(&dir).profile("work").unwrap();
        assert_eq!(made.prompts, ["base"]);
        assert_eq!(made.skills, ["tidy"]);
        assert_eq!(made.personality.as_deref(), Some("calm"));
        assert_eq!(store_of(&dir).selected_profile().unwrap(), "default");
        let shown = call(
            &tools,
            "profile",
            serde_json::json!({"action": "show", "name": "work"}),
        );
        assert!(
            shown.contains("Skills on: tidy") && shown.contains("Personality: calm"),
            "{shown}"
        );
        let listed = call(&tools, "profile", serde_json::json!({"action": "list"}));
        assert!(
            listed.contains("default (new chats start under it)"),
            "{listed}"
        );
        assert!(asked.borrow().is_empty());
    }

    #[test]
    fn a_profile_change_asks_each_time_and_a_list_is_replaced_whole() {
        let (dir, tools, asked) = rig(
            "prof-edit",
            Mode::Auto,
            &[Answer::Yes, Answer::Yes, Answer::No],
        );
        call(&tools, "library", lib("add", "skill", "tidy"));
        call(
            &tools,
            "profile",
            serde_json::json!({"action": "add", "name": "work", "skills": ["tidy"]}),
        );
        let same = call(
            &tools,
            "profile",
            serde_json::json!({"action": "edit", "name": "work", "skills": ["tidy"]}),
        );
        assert!(same.starts_with("no change"), "{same}");
        assert!(asked.borrow().is_empty());
        let cleared = call(
            &tools,
            "profile",
            serde_json::json!({"action": "edit", "name": "work", "skills": [], "new_name": "job"}),
        );
        assert_eq!(cleared, "changed profile 'work'");
        let job = store_of(&dir).profile("job").unwrap();
        assert!(job.skills.is_empty());
        let removed = call(
            &tools,
            "profile",
            serde_json::json!({"action": "delete", "name": "job"}),
        );
        assert_eq!(removed, "deleted profile 'job'");
        assert!(store_of(&dir).profile("job").is_err());
        // The profile new chats start under cannot be deleted, and the owner is not asked.
        let before = asked.borrow().len();
        let refused = call(
            &tools,
            "profile",
            serde_json::json!({"action": "delete", "name": "default"}),
        );
        assert!(refused.contains("new chats start under"), "{refused}");
        assert_eq!(asked.borrow().len(), before);
        let asked = asked.borrow();
        assert!(asked.iter().all(|(_, always)| !always));
        assert_eq!(asked[0].0, "change this profile?");
    }
}

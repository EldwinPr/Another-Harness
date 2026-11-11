//! The write tools of `--mcp-serve --direct` (issue 46).
//!
//! Offered only when the owner started the server with `--direct`; without it this file is never
//! reached (a call to one of these is an unknown tool). With `--direct` the server offers no
//! `submit` (`submit.rs`, the buffer mode). They write straight into the database with
//! the store's own checks, the ones the chat and the web page obey, so there is one set of rules
//! for what may be stored. Three things are kept on purpose:
//!
//! - **An edit or delete reads the item first and hands its fingerprint to the store's checked
//!   function**, so a change that landed in between is reported, not overwritten.
//! - **Nothing here can turn anything on.** A new skill, personality or system prompt is saved
//!   switched off, and no tool switches one on, selects the profile new chats start under, or
//!   reorders: what the next chat sends stays the owner's call.
//! - **A failure is whole.** A profile's members are checked before it is added, and the store
//!   functions are transactions.
//!
//! Every answer says what happened in fixed words: `added`, `replaced`, `changed`, `no change`,
//! `deleted`.

use serde_json::Value;

use super::{
    Def, Server, failed, find_item, find_profile, kind_arg, quotes_arg, required, string_arg,
};
use crate::store::{Edit, Store};
use crate::store_buffer::scope;
use crate::store_glossary::Saved;
use crate::store_profiles::{Profile, check_description, check_profile_name};
use crate::store_prompts::{Prompt, PromptType, check_prompt_name, check_prompt_text};

/// The write tools, after the read tools in `tools/list`.
pub(super) const WRITE_TOOLS: [Def; 10] = [
    Def {
        name: "context_set",
        description: "Writes. Add a context entry, or replace the one with the same name and project. A new shape is preloaded. A shape needs the owner's own words (quote or quotes) or a '## Where the mechanism lives' section; keep their words when you have them.",
        schema: r#"{"type":"object","properties":{"name":{"type":"string","description":"A short name: letters, digits, . - _"},"kind":{"type":"string","enum":["rule","decision","shape","guide"],"description":"rule: how to work here. decision: a choice and why. shape: how an area is thought of. guide: a longer how-to."},"text":{"type":"string","description":"What to remember"},"quote":{"type":"string","description":"The owner's own words this comes from (optional; an empty quote keeps the stored one)"},"area":{"type":"string","description":"The business area (optional; omitted keeps the stored one)"},"quotes":{"type":"array","description":"A shape: the owner's words it rests on, each with the day said and the claim it supports. Added to the ones stored.","items":{"type":"object","properties":{"quote":{"type":"string"},"said_on":{"type":"string","description":"YYYY-MM-DD"},"claim":{"type":"string"},"replaces":{"type":"string"}},"required":["quote","said_on"]}},"project":{"type":"string","description":"A project's folder path. Default: the folder the server was started in. Empty: for every project."}},"required":["name","kind","text"]}"#,
    },
    Def {
        name: "context_delete",
        description: "Writes. Delete the context entry with this name in exactly this project (default: the folder the server was started in; pass an empty project to delete an every-project entry).",
        schema: r#"{"type":"object","properties":{"name":{"type":"string","description":"The name of the entry"},"project":{"type":"string","description":"A project's folder path. Default: the folder the server was started in. Empty: the every-project entry."}},"required":["name"]}"#,
    },
    Def {
        name: "glossary_set",
        description: "Writes. Add a project word to the glossary, or replace the definition of the one with the same term and project.",
        schema: r#"{"type":"object","properties":{"term":{"type":"string","description":"The project word"},"definition":{"type":"string","description":"What it means in this project, in a few sentences"},"area":{"type":"string","description":"The business area (optional; omitted keeps the stored one)"},"project":{"type":"string","description":"A project's folder path. Default: the folder the server was started in. Empty: for every project."}},"required":["term","definition"]}"#,
    },
    Def {
        name: "glossary_delete",
        description: "Writes. Delete the glossary term in exactly this project (default: the folder the server was started in; pass an empty project to delete an every-project word).",
        schema: r#"{"type":"object","properties":{"term":{"type":"string","description":"The project word"},"project":{"type":"string","description":"A project's folder path. Default: the folder the server was started in. Empty: the every-project word."}},"required":["term"]}"#,
    },
    Def {
        name: "library_add",
        description: "Writes. Save a new skill, personality or system prompt. It is saved switched off; only the owner turns it on.",
        schema: r#"{"type":"object","properties":{"type":{"type":"string","enum":["skill","personality","prompt"],"description":"prompt is a system prompt"},"name":{"type":"string","description":"The name of the new item"},"text":{"type":"string","description":"The whole text"},"description":{"type":"string","description":"One line: what it is for"}},"required":["type","name","text"]}"#,
    },
    Def {
        name: "library_edit",
        description: "Writes. Change a skill, personality or system prompt: give any of new_name, text and description; the rest stays. Whether it is on does not change.",
        schema: r#"{"type":"object","properties":{"type":{"type":"string","enum":["skill","personality","prompt"],"description":"prompt is a system prompt"},"name":{"type":"string","description":"The name of the item"},"new_name":{"type":"string","description":"Rename it"},"text":{"type":"string","description":"The whole new text"},"description":{"type":"string","description":"One line: what it is for"}},"required":["type","name"]}"#,
    },
    Def {
        name: "library_delete",
        description: "Writes. Delete a skill, personality or system prompt, and take it out of the profiles and chats that used it.",
        schema: r#"{"type":"object","properties":{"type":{"type":"string","enum":["skill","personality","prompt"],"description":"prompt is a system prompt"},"name":{"type":"string","description":"The name of the item"}},"required":["type","name"]}"#,
    },
    Def {
        name: "profile_add",
        description: "Writes. Add a profile (a named starting setup). Every prompt, skill and personality named has to exist already, with the right type.",
        schema: r#"{"type":"object","properties":{"name":{"type":"string","description":"The name of the new profile"},"description":{"type":"string","description":"One line: what it is for"},"prompts":{"type":"array","items":{"type":"string"},"description":"Names of system prompts"},"skills":{"type":"array","items":{"type":"string"},"description":"Names of skills switched on when a chat starts"},"personality":{"type":"string","description":"Name of a personality"}},"required":["name"]}"#,
    },
    Def {
        name: "profile_edit",
        description: "Writes. Change a profile: give any of new_name, description, prompts, skills and personality; a list given replaces the whole list, and an empty personality means none. The rest stays. The prompts, skills and personality of the profile new chats start under cannot be changed here (its new_name and description can).",
        schema: r#"{"type":"object","properties":{"name":{"type":"string","description":"The name of the profile"},"new_name":{"type":"string","description":"Rename it"},"description":{"type":"string","description":"One line: what it is for"},"prompts":{"type":"array","items":{"type":"string"},"description":"Names of system prompts; replaces the list"},"skills":{"type":"array","items":{"type":"string"},"description":"Names of skills; replaces the list"},"personality":{"type":"string","description":"Name of a personality; empty for none"}},"required":["name"]}"#,
    },
    Def {
        name: "profile_delete",
        description: "Writes. Delete a profile. The one new chats start under cannot be deleted.",
        schema: r#"{"type":"object","properties":{"name":{"type":"string","description":"The name of the profile"}},"required":["name"]}"#,
    },
];

impl Server {
    /// Runs one write tool. Only called when the server is direct.
    pub(super) fn call_write(&self, name: &str, arguments: &Value) -> Result<String, String> {
        match name {
            "context_set" => self.context_set(arguments),
            "context_delete" => self.context_delete(arguments),
            "glossary_set" => self.glossary_set(arguments),
            "glossary_delete" => self.glossary_delete(arguments),
            "library_add" => self.library_add(arguments),
            "library_edit" => self.library_edit(arguments),
            "library_delete" => self.library_delete(arguments),
            "profile_add" => self.profile_add(arguments),
            "profile_edit" => self.profile_edit(arguments),
            "profile_delete" => self.profile_delete(arguments),
            other => Err(format!("error: unknown tool '{other}'")),
        }
    }

    /// A fresh connection that may write: nothing is created or migrated, and a database of
    /// another schema version is refused.
    pub(super) fn writer(&self) -> Result<Store, String> {
        Store::open_for_writing(&self.db)
            .map_err(|e| format!("error: cannot write the database: {e}"))
    }

    fn context_set(&self, arguments: &Value) -> Result<String, String> {
        let name = required(arguments, "name")?;
        let kind = required(arguments, "kind")?;
        let text = required(arguments, "text")?;
        let quote = string_arg(arguments, "quote")?.unwrap_or_default();
        let area = string_arg(arguments, "area")?;
        let quotes = quotes_arg(arguments)?;
        let project = self.project_of(arguments)?;
        // Evidence is asked of what arrives from outside, as `submit` does; the owner's own
        // screens do not ask it.
        if kind.trim().eq_ignore_ascii_case("shape") {
            crate::store_context::check_shape_evidence(&text, &quote, &quotes)
                .map_err(|e| format!("error: {e}"))?;
        }
        let saved = self
            .writer()?
            .set_context_with_quotes(
                &project,
                &name,
                &kind,
                &text,
                &quote,
                area.as_deref(),
                &quotes,
            )
            .map_err(failed)?;
        let what = format!("context entry '{name}' for {}", scope(&project));
        Ok(saved_words(saved, &what))
    }

    fn context_delete(&self, arguments: &Value) -> Result<String, String> {
        let name = required(arguments, "name")?;
        let project = self.project_of(arguments)?;
        let what = format!("context entry '{name}' for {}", scope(&project));
        if self
            .writer()?
            .delete_context(&project, &name)
            .map_err(failed)?
        {
            Ok(format!("deleted {what}"))
        } else {
            Err(format!("error: no {what}"))
        }
    }

    fn glossary_set(&self, arguments: &Value) -> Result<String, String> {
        let term = required(arguments, "term")?;
        let definition = required(arguments, "definition")?;
        let area = string_arg(arguments, "area")?;
        let project = self.project_of(arguments)?;
        let saved = self
            .writer()?
            .set_glossary_in(&project, &term, &definition, area.as_deref())
            .map_err(failed)?;
        let what = format!("glossary term '{term}' for {}", scope(&project));
        Ok(saved_words(saved, &what))
    }

    fn glossary_delete(&self, arguments: &Value) -> Result<String, String> {
        let term = required(arguments, "term")?;
        let project = self.project_of(arguments)?;
        let what = format!("glossary term '{term}' for {}", scope(&project));
        if self
            .writer()?
            .delete_glossary_in(&project, &term)
            .map_err(failed)?
        {
            Ok(format!("deleted {what}"))
        } else {
            Err(format!("error: no {what}"))
        }
    }

    fn library_add(&self, arguments: &Value) -> Result<String, String> {
        let kind = kind_arg(arguments)?;
        let name = required(arguments, "name")?;
        let text = required(arguments, "text")?;
        let description =
            check_description(&string_arg(arguments, "description")?.unwrap_or_default())
                .map_err(|e| format!("error: {e}"))?;
        check_prompt_name(&name).map_err(|e| format!("error: {e}"))?;
        check_prompt_text(&text).map_err(|e| format!("error: {e}"))?;
        self.writer()?
            .add_prompt_of_type(&name, &description, &text, None, kind)
            .map_err(failed)?;
        Ok(format!(
            "added {} '{name}', switched off",
            super::noun(kind)
        ))
    }

    fn library_edit(&self, arguments: &Value) -> Result<String, String> {
        let kind = kind_arg(arguments)?;
        let wanted = required(arguments, "name")?;
        let new_name = string_arg(arguments, "new_name")?.filter(|n| !n.is_empty());
        let new_text = string_arg(arguments, "text")?;
        let new_description = string_arg(arguments, "description")?;
        if new_name.is_none() && new_text.is_none() && new_description.is_none() {
            return Err("error: give new_name, text or description".to_owned());
        }
        let mut store = self.writer()?;
        let all = store.list_prompts().map_err(failed)?;
        let old = find_item(&all, kind, &wanted)?;
        let name = new_name.unwrap_or_else(|| old.name.clone());
        let text = new_text.unwrap_or_else(|| old.text.clone());
        let description = match new_description {
            Some(given) => check_description(&given).map_err(|e| format!("error: {e}"))?,
            None => old.description.clone(),
        };
        if name == old.name && text == old.text && description == old.description {
            return Ok(format!(
                "no change: {} '{}' already says this",
                super::noun(kind),
                old.name
            ));
        }
        apply_prompt_edit(&mut store, old, &name, &description, &text)
    }

    fn library_delete(&self, arguments: &Value) -> Result<String, String> {
        let kind = kind_arg(arguments)?;
        let wanted = required(arguments, "name")?;
        let mut store = self.writer()?;
        let all = store.list_prompts().map_err(failed)?;
        let old = find_item(&all, kind, &wanted)?;
        apply_prompt_delete(&mut store, old)
    }

    fn profile_add(&self, arguments: &Value) -> Result<String, String> {
        let name = required(arguments, "name")?;
        let description =
            check_description(&string_arg(arguments, "description")?.unwrap_or_default())
                .map_err(|e| format!("error: {e}"))?;
        let prompts = list_arg(arguments, "prompts")?.unwrap_or_default();
        let skills = list_arg(arguments, "skills")?.unwrap_or_default();
        let personality = string_arg(arguments, "personality")?.filter(|p| !p.is_empty());
        check_profile_name(&name).map_err(|e| format!("error: {e}"))?;
        let mut store = self.writer()?;
        // Every member is checked before anything is written, so a typo leaves no half profile.
        let all = store.list_prompts().map_err(failed)?;
        let prompts = resolve(&all, PromptType::System, &prompts)?;
        let skills = resolve(&all, PromptType::Skill, &skills)?;
        let personality = personality
            .map(|member| find_item(&all, PromptType::Personality, &member).map(|p| p.name.clone()))
            .transpose()?;
        store.add_profile(&name, &description).map_err(failed)?;
        let filled = store
            .set_profile_prompts(&name, &prompts)
            .and_then(|()| store.set_profile_skills(&name, &skills))
            .and_then(|()| store.set_profile_personality(&name, personality.as_deref()));
        if let Err(why) = filled {
            // Best effort: take the empty profile back out rather than leave it half made.
            let _ = store.delete_profile(&name);
            return Err(failed(why));
        }
        Ok(format!("added profile '{name}'"))
    }

    fn profile_edit(&self, arguments: &Value) -> Result<String, String> {
        let wanted = required(arguments, "name")?;
        let new_name = string_arg(arguments, "new_name")?.filter(|n| !n.is_empty());
        let new_description = string_arg(arguments, "description")?;
        let new_prompts = list_arg(arguments, "prompts")?;
        let new_skills = list_arg(arguments, "skills")?;
        let new_personality = string_arg(arguments, "personality")?;
        if new_name.is_none()
            && new_description.is_none()
            && new_prompts.is_none()
            && new_skills.is_none()
            && new_personality.is_none()
        {
            return Err(
                "error: give new_name, description, prompts, skills or personality".to_owned(),
            );
        }
        let mut store = self.writer()?;
        let all = store.list_profiles().map_err(failed)?;
        let old = find_profile(&all, &wanted)?;
        // Which system prompts are on is the prompts of the profile new chats start under, and its
        // skills and personality are what the next chat starts with: only the owner changes them.
        // Refused whatever the values, before anything is read further or written.
        if (new_prompts.is_some() || new_skills.is_some() || new_personality.is_some())
            && store
                .selected_profile()
                .map_err(failed)?
                .eq_ignore_ascii_case(&old.name)
        {
            return Err(format!(
                "error: {} is the profile new chats start under: its prompts, skills and personality decide what the next chat sends, so only the owner changes them (new_name and description are allowed)",
                old.name
            ));
        }
        let library = store.list_prompts().map_err(failed)?;
        let name = new_name.unwrap_or_else(|| old.name.clone());
        let description = new_description.unwrap_or_else(|| old.description.clone());
        let prompts = match new_prompts {
            Some(given) => resolve(&library, PromptType::System, &given)?,
            None => old.prompts.clone(),
        };
        let skills = match new_skills {
            Some(given) => resolve(&library, PromptType::Skill, &given)?,
            None => old.skills.clone(),
        };
        let personality = match new_personality.as_deref() {
            None => old.personality.clone(),
            Some("") => None,
            Some(given) => Some(
                find_item(&library, PromptType::Personality, given)?
                    .name
                    .clone(),
            ),
        };
        let description = check_description(&description).map_err(|e| format!("error: {e}"))?;
        if name == old.name
            && description == old.description
            && same_members(&prompts, &old.prompts)
            && same_members(&skills, &old.skills)
            && personality == old.personality
        {
            return Ok(format!(
                "no change: profile '{}' already says this",
                old.name
            ));
        }
        apply_profile_edit(
            &mut store,
            old,
            &name,
            &description,
            &prompts,
            &skills,
            personality.as_deref(),
        )
    }

    fn profile_delete(&self, arguments: &Value) -> Result<String, String> {
        let wanted = required(arguments, "name")?;
        let mut store = self.writer()?;
        let all = store.list_profiles().map_err(failed)?;
        let old = find_profile(&all, &wanted)?;
        apply_profile_delete(&mut store, old)
    }
}

/// Changes the prompt `old` as it was read: the store makes the change only if the row still has
/// the fingerprint `old` had, so an edit that landed in between is reported and not overwritten.
pub(super) fn apply_prompt_edit(
    store: &mut Store,
    old: &Prompt,
    name: &str,
    description: &str,
    text: &str,
) -> Result<String, String> {
    let kind = old.prompt_type;
    match store
        .edit_prompt(old.id, &old.fingerprint(), name, description, text)
        .map_err(failed)?
    {
        Edit::Done if name == old.name => {
            Ok(format!("changed {} '{}'", super::noun(kind), old.name))
        }
        Edit::Done => Ok(format!(
            "changed {} '{}', now named '{name}'",
            super::noun(kind),
            old.name
        )),
        Edit::Stale => Err(stale(super::noun(kind))),
        Edit::Gone => Err(format!(
            "error: {} '{}' is gone",
            super::noun(kind),
            old.name
        )),
    }
}

/// Deletes the prompt `old` as it was read, if it is still as it was.
pub(super) fn apply_prompt_delete(store: &mut Store, old: &Prompt) -> Result<String, String> {
    let kind = old.prompt_type;
    match store
        .delete_prompt_checked(old.id, &old.fingerprint())
        .map_err(failed)?
    {
        Edit::Done => Ok(format!("deleted {} '{}'", super::noun(kind), old.name)),
        Edit::Stale => Err(stale(super::noun(kind))),
        Edit::Gone => Err(format!(
            "error: {} '{}' is already gone",
            super::noun(kind),
            old.name
        )),
    }
}

/// Changes the profile `old` as it was read, if it is still as it was.
pub(super) fn apply_profile_edit(
    store: &mut Store,
    old: &Profile,
    name: &str,
    description: &str,
    prompts: &[String],
    skills: &[String],
    personality: Option<&str>,
) -> Result<String, String> {
    match store
        .edit_profile(
            old.id,
            &old.fingerprint(),
            name,
            description,
            prompts,
            skills,
            personality,
        )
        .map_err(failed)?
    {
        Edit::Done if name == old.name => Ok(format!("changed profile '{}'", old.name)),
        Edit::Done => Ok(format!(
            "changed profile '{}', now named '{name}'",
            old.name
        )),
        Edit::Stale => Err(stale("profile")),
        Edit::Gone => Err(format!("error: profile '{}' is gone", old.name)),
    }
}

/// Deletes the profile `old` as it was read, if it is still as it was.
pub(super) fn apply_profile_delete(store: &mut Store, old: &Profile) -> Result<String, String> {
    match store
        .delete_profile_checked(old.id, &old.fingerprint())
        .map_err(failed)?
    {
        Edit::Done => Ok(format!("deleted profile '{}'", old.name)),
        Edit::Stale => Err(stale("profile")),
        Edit::Gone => Err(format!("error: profile '{}' is already gone", old.name)),
    }
}

/// The stored names of members given by name (any case): each has to be in the library with
/// this kind. A name given twice counts once.
fn resolve(all: &[Prompt], kind: PromptType, given: &[String]) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = Vec::new();
    for member in given {
        let name = find_item(all, kind, member)?.name.clone();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    Ok(names)
}

/// Whether two lists name the same members: the store keeps them in the library's order, so the
/// order a caller gave them in is not a change.
fn same_members(given: &[String], stored: &[String]) -> bool {
    given.len() == stored.len() && given.iter().all(|name| stored.contains(name))
}

fn saved_words(saved: Saved, what: &str) -> String {
    match saved {
        Saved::Added => format!("added {what}"),
        Saved::Changed => format!("replaced {what}"),
        Saved::Same => format!("no change: {what} already says this"),
    }
}

fn stale(what: &str) -> String {
    format!("error: the {what} changed since it was read; read it again and retry")
}

/// A list of names: absent is `None`; anything but an array of strings is an error.
fn list_arg(arguments: &Value, key: &str) -> Result<Option<Vec<String>>, String> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(|s| s.trim().to_owned()))
            .collect::<Option<Vec<_>>>()
            .map(Some)
            .ok_or_else(|| format!("error: '{key}' must be a list of names")),
        Some(_) => Err(format!("error: '{key}' must be a list of names")),
    }
}

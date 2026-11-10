//! Chat profiles (issue 31): a named starting setup for a chat.
//!
//! A profile holds system prompts (joined, in the library's order, into the system prompt of a new
//! chat), skills that are on when the chat starts, and at most one personality. A new chat starts
//! under the selected profile (the setting [`SELECTED_KEY`]) and then belongs to itself: `/skills`
//! and `/personality` change that chat only, and editing a profile never changes a chat that has
//! begun. Part of the store (`impl Store` continues here); the table layout is owned by
//! `docs/erd.md`.
//!
//! `Prompt::enabled` (see `store_prompts.rs`) now means "in the selected profile", so everything
//! that reads the prompts sees the selected profile without knowing profiles exist; the column
//! `prompts.enabled` is no longer read.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::store::{Edit, Store, StoreError, now_secs};
use crate::store_prompts::{
    Assembled, BASE_NAME, ChatPrompts, PromptType, SUBAGENT_CODER_NAME, SUBAGENT_CODER_PROMPT,
    SUBAGENT_NAME, SUBAGENT_PROMPT, assemble,
};

/// The `settings` key holding the name of the profile new chats start under.
pub const SELECTED_KEY: &str = "profile";
/// The profile every database has, and the one used when the setting names none.
pub const DEFAULT_NAME: &str = "default";
/// The longest description of a profile, in characters.
pub const MAX_DESCRIPTION_CHARS: usize = 200;

/// Words `/profile` takes as its first argument, so they cannot be profile names.
const RESERVED: &[&str] = &[
    "list", "show", "use", "add", "edit", "copy", "delete", "rename",
];

/// A profile and everything in it, by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub id: i64,
    pub name: String,
    pub description: String,
    /// The personality chosen when a chat starts, if any.
    pub personality: Option<String>,
    /// The system prompts, in the library's order.
    pub prompts: Vec<String>,
    /// The skills that are on when a chat starts, in the library's order.
    pub skills: Vec<String>,
}

impl Profile {
    /// A stamp of everything an edit form shows: two readings of an untouched profile agree, and a
    /// profile changed in between (by the chat, another tab) does not.
    pub fn fingerprint(&self) -> String {
        let personality = self.personality.as_deref().unwrap_or("");
        let prompts = self.prompts.join("\n");
        let skills = self.skills.join("\n");
        crate::store::fingerprint(&[
            &self.name,
            &self.description,
            personality,
            &prompts,
            &skills,
        ])
    }
}

/// What a new chat starts with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Start {
    pub assembled: Assembled,
    /// The name of the selected profile.
    pub profile: String,
    /// The skills that are on and the personality chosen.
    pub chat: ChatPrompts,
}

/// A profile that is created with the program, with the one prompt that makes it what it is. The
/// profile is `base` plus that prompt. Nothing is created if either name is already used.
struct Seed {
    prompt: &'static str,
    profile: &'static str,
    description: &'static str,
    text: &'static str,
}

const SEEDS: [Seed; 2] = [
    Seed {
        prompt: "planner",
        profile: "planner",
        description: "plan the work, do not build it",
        text: "Your job is to plan, not to build. Read what you need, then write a plan: the goal, the files to change, the steps in order, and how each will be checked. Do not create or change files except the plan the user asks for. Say what you are unsure about.",
    },
    Seed {
        prompt: "builder",
        profile: "builder",
        description: "build what was asked, in small steps",
        text: "Your job is to build what the plan or the user's request says, in small steps. After each change run the project's own test command. If the request and the code disagree, stop and say how. Do not add anything that was not asked for.",
    },
];

fn err(text: impl Into<String>) -> StoreError {
    StoreError::new(text)
}

/// A profile's name: the provider-name rules, without the words `/profile` takes.
pub fn check_profile_name(name: &str) -> Result<(), String> {
    crate::providers::check_name_among(name, RESERVED, "/profile")
}

/// A description as it will be stored: trimmed, one line, at most [`MAX_DESCRIPTION_CHARS`].
pub fn check_description(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.chars().any(char::is_control) {
        return Err("a description is one line of plain text".to_owned());
    }
    if text.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(format!(
            "a description is at most {MAX_DESCRIPTION_CHARS} characters"
        ));
    }
    Ok(text.to_owned())
}

/// The migration to schema 12: creates `default` from the system prompts that are on now, and
/// (with `examples`) the seeded profiles `planner` and `builder`, and selects
/// `default`. Everything is created only if missing, so running it again changes nothing that is
/// there. The migration passes `examples` as `!cfg!(test)`: the many tests that count the prompts
/// of a fresh database would otherwise all have to know the two seeded ones; the seeded content
/// is tested by calling this with `true`.
pub(crate) fn seed(conn: &Connection, examples: bool) -> Result<(), StoreError> {
    let now = now_secs();
    let taken = |table: &str, name: &str| -> Result<bool, StoreError> {
        let n: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE name = ?1"),
            [name],
            |row| row.get(0),
        )?;
        Ok(n > 0)
    };
    if !taken("profiles", DEFAULT_NAME)? {
        conn.execute(
            "INSERT INTO profiles (name, description, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
            params![DEFAULT_NAME, "the system prompt of a chat as it was", now],
        )?;
        let id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO profile_prompts (profile_id, prompt_id) \
             SELECT ?1, id FROM prompts WHERE enabled = 1 AND type = 'system'",
            [id],
        )?;
    }
    for seed in SEEDS.iter().filter(|_| examples) {
        if taken("prompts", seed.prompt)? || taken("profiles", seed.profile)? {
            continue;
        }
        let position: i64 = conn.query_row(
            "SELECT COALESCE(MAX(position), 0) + 1 FROM prompts",
            [],
            |row| row.get(0),
        )?;
        conn.execute(
            "INSERT INTO prompts (name, description, text, enabled, position, created_at, updated_at, type) \
             VALUES (?1, ?2, ?3, 0, ?4, ?5, ?5, 'system')",
            params![seed.prompt, seed.description, seed.text, position, now],
        )?;
        let prompt_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO profiles (name, description, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
            params![seed.profile, seed.description, now],
        )?;
        let profile_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO profile_prompts (profile_id, prompt_id) \
             SELECT ?1, id FROM prompts WHERE name IN (?2, ?3)",
            params![profile_id, BASE_NAME, seed.prompt],
        )?;
        debug_assert!(prompt_id > 0);
    }
    conn.execute(
        "INSERT OR IGNORE INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3)",
        params![SELECTED_KEY, DEFAULT_NAME, now],
    )?;
    Ok(())
}

impl Store {
    /// The name of the profile new chats start under.
    pub fn selected_profile(&self) -> Result<String, StoreError> {
        Ok(self
            .setting(SELECTED_KEY)?
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_NAME.to_owned()))
    }

    /// Every profile with its prompts, skills and personality, by name (ignoring case).
    pub fn list_profiles(&self) -> Result<Vec<Profile>, StoreError> {
        profiles_on(&self.conn)
    }

    /// One profile by name (ignoring case).
    pub fn profile(&self, name: &str) -> Result<Profile, StoreError> {
        self.list_profiles()?
            .into_iter()
            .find(|p| p.name.eq_ignore_ascii_case(name.trim()))
            .ok_or_else(|| err(format!("there is no profile named {name} (see /profile)")))
    }

    /// Makes new chats start under this profile. A chat in progress keeps what it began with.
    pub fn select_profile(&mut self, name: &str) -> Result<(), StoreError> {
        let profile = self.profile(name)?;
        self.set_setting(SELECTED_KEY, &profile.name)
    }

    /// What a chat started now would get: the system prompt of the selected profile (the built-in
    /// text when it has none), the skills that are on and the personality.
    pub fn start_of_selected_profile(&self) -> Result<Start, StoreError> {
        let assembled = assemble(&self.list_prompts()?);
        let name = self.selected_profile()?;
        let profile = self
            .list_profiles()?
            .into_iter()
            .find(|p| p.name.eq_ignore_ascii_case(&name));
        Ok(Start {
            assembled,
            profile: profile.as_ref().map_or(name, |p| p.name.clone()),
            chat: profile
                .map(|p| ChatPrompts {
                    personality: p.personality,
                    on: p.skills,
                    once: Vec::new(),
                })
                .unwrap_or_default(),
        })
    }

    /// What a subagent running under profile `name` starts with (issue 33): the `subagent` prompt
    /// (`subagent-coder` for one that may change files, issue 34; the built-in text when there is
    /// none by that name) in place of the profile's `base`, then the profile's other system
    /// prompts in the library's order; the profile's skills and personality. The profile's own
    /// start is not changed.
    pub fn start_of_subagent(&self, name: &str, coder: bool) -> Result<Start, StoreError> {
        let (first_name, built_in) = if coder {
            (SUBAGENT_CODER_NAME, SUBAGENT_CODER_PROMPT)
        } else {
            (SUBAGENT_NAME, SUBAGENT_PROMPT)
        };
        let profile = self.profile(name)?;
        let prompts = self.list_prompts()?;
        let first = prompts
            .iter()
            .find(|p| p.name == first_name && p.prompt_type == PromptType::System)
            .map_or_else(|| built_in.to_owned(), |p| p.text.trim().to_owned());
        let mut parts = vec![first];
        let mut names = vec![first_name.to_owned()];
        for p in &prompts {
            if p.prompt_type == PromptType::System
                && p.name != BASE_NAME
                && p.name != SUBAGENT_NAME
                && p.name != SUBAGENT_CODER_NAME
                && profile.prompts.contains(&p.name)
            {
                parts.push(p.text.trim().to_owned());
                names.push(p.name.clone());
            }
        }
        Ok(Start {
            assembled: Assembled {
                text: parts.join("\n\n"),
                names,
                fallback: false,
            },
            profile: profile.name,
            chat: ChatPrompts {
                personality: profile.personality,
                on: profile.skills,
                once: Vec::new(),
            },
        })
    }

    /// Adds an empty profile.
    pub fn add_profile(&mut self, name: &str, description: &str) -> Result<(), StoreError> {
        check_profile_name(name).map_err(err)?;
        let description = check_description(description).map_err(err)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        taken(&tx, name)?;
        let now = now_secs();
        tx.execute(
            "INSERT INTO profiles (name, description, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
            params![name, description, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Makes `new` a copy of `from`: description, prompts, skills and personality.
    pub fn copy_profile(&mut self, from: &str, new: &str) -> Result<(), StoreError> {
        check_profile_name(new).map_err(err)?;
        let source = self.profile(from)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        taken(&tx, new)?;
        let now = now_secs();
        tx.execute(
            "INSERT INTO profiles (name, description, personality_id, created_at, updated_at) \
             SELECT ?2, description, personality_id, ?3, ?3 FROM profiles WHERE id = ?1",
            params![source.id, new, now],
        )?;
        let id = tx.last_insert_rowid();
        for table in ["profile_prompts", "profile_skills"] {
            tx.execute(
                &format!(
                    "INSERT INTO {table} (profile_id, prompt_id) \
                     SELECT ?2, prompt_id FROM {table} WHERE profile_id = ?1"
                ),
                params![source.id, id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Renames a profile; if it is the selected one it stays selected.
    pub fn rename_profile(&mut self, old: &str, new: &str) -> Result<(), StoreError> {
        check_profile_name(new).map_err(err)?;
        let profile = self.profile(old)?;
        let selected = self.selected_profile()?.eq_ignore_ascii_case(&profile.name);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let other: i64 = tx.query_row(
            "SELECT COUNT(*) FROM profiles WHERE name = ?1 AND id <> ?2",
            params![new, profile.id],
            |row| row.get(0),
        )?;
        if other > 0 {
            return Err(err(format!("there is already a profile named {new}")));
        }
        let now = now_secs();
        tx.execute(
            "UPDATE profiles SET name = ?2, updated_at = ?3 WHERE id = ?1",
            params![profile.id, new, now],
        )?;
        if selected {
            tx.execute(
                "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                params![SELECTED_KEY, new, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn set_profile_description(
        &mut self,
        name: &str,
        description: &str,
    ) -> Result<(), StoreError> {
        let description = check_description(description).map_err(err)?;
        let profile = self.profile(name)?;
        self.conn.execute(
            "UPDATE profiles SET description = ?2, updated_at = ?3 WHERE id = ?1",
            params![profile.id, description, now_secs()],
        )?;
        Ok(())
    }

    /// Deletes a profile. The selected one cannot be deleted: a new chat would have none.
    pub fn delete_profile(&mut self, name: &str) -> Result<(), StoreError> {
        let profile = self.profile(name)?;
        if self.selected_profile()?.eq_ignore_ascii_case(&profile.name) {
            return Err(err(format!(
                "{} is the profile new chats start under: /profile use another one first",
                profile.name
            )));
        }
        self.conn
            .execute("DELETE FROM profiles WHERE id = ?1", [profile.id])?;
        Ok(())
    }

    /// Sets the system prompts of a profile (names of system prompts).
    pub fn set_profile_prompts(
        &mut self,
        name: &str,
        prompts: &[String],
    ) -> Result<(), StoreError> {
        self.set_members(name, "profile_prompts", PromptType::System, prompts)
    }

    /// Sets the skills a profile turns on (names of skills).
    pub fn set_profile_skills(&mut self, name: &str, skills: &[String]) -> Result<(), StoreError> {
        self.set_members(name, "profile_skills", PromptType::Skill, skills)
    }

    /// Sets the personality of a profile (the name of a personality), or none.
    pub fn set_profile_personality(
        &mut self,
        name: &str,
        personality: Option<&str>,
    ) -> Result<(), StoreError> {
        let profile = self.profile(name)?;
        let id = match personality {
            None => None,
            Some(wanted) => Some(self.prompt_of_type(wanted, PromptType::Personality)?),
        };
        self.conn.execute(
            "UPDATE profiles SET personality_id = ?2, updated_at = ?3 WHERE id = ?1",
            params![profile.id, id, now_secs()],
        )?;
        Ok(())
    }

    /// Changes the profile `id` (name, description, system prompts, skills, personality) in one
    /// transaction, if it still has the stamp `expected`. A rename keeps the profile selected.
    #[allow(clippy::too_many_arguments)]
    pub fn edit_profile(
        &mut self,
        id: i64,
        expected: &str,
        name: &str,
        description: &str,
        prompts: &[String],
        skills: &[String],
        personality: Option<&str>,
    ) -> Result<Edit, StoreError> {
        check_profile_name(name).map_err(err)?;
        let description = check_description(description).map_err(err)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(there) = profiles_on(&tx)?.into_iter().find(|p| p.id == id) else {
            return Ok(Edit::Gone);
        };
        if there.fingerprint() != expected {
            return Ok(Edit::Stale);
        }
        let other: i64 = tx.query_row(
            "SELECT COUNT(*) FROM profiles WHERE name = ?1 AND id <> ?2",
            params![name, id],
            |row| row.get(0),
        )?;
        if other > 0 {
            return Err(err(format!("there is already a profile named {name}")));
        }
        let prompt_ids = prompts
            .iter()
            .map(|n| member_id(&tx, n, PromptType::System))
            .collect::<Result<Vec<_>, _>>()?;
        let skill_ids = skills
            .iter()
            .map(|n| member_id(&tx, n, PromptType::Skill))
            .collect::<Result<Vec<_>, _>>()?;
        let personality_id = personality
            .map(|n| member_id(&tx, n, PromptType::Personality))
            .transpose()?;
        let now = now_secs();
        let selected: Option<String> = tx
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                [SELECTED_KEY],
                |row| row.get(0),
            )
            .optional()?;
        tx.execute(
            "UPDATE profiles SET name = ?2, description = ?3, personality_id = ?4, updated_at = ?5 WHERE id = ?1",
            params![id, name, description, personality_id, now],
        )?;
        for (table, ids) in [
            ("profile_prompts", &prompt_ids),
            ("profile_skills", &skill_ids),
        ] {
            tx.execute(&format!("DELETE FROM {table} WHERE profile_id = ?1"), [id])?;
            for prompt_id in ids {
                tx.execute(
                    &format!(
                        "INSERT OR IGNORE INTO {table} (profile_id, prompt_id) VALUES (?1, ?2)"
                    ),
                    params![id, prompt_id],
                )?;
            }
        }
        if selected.is_some_and(|s| s.eq_ignore_ascii_case(&there.name)) {
            tx.execute(
                "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                params![SELECTED_KEY, name, now],
            )?;
        }
        tx.commit()?;
        Ok(Edit::Done)
    }

    /// Deletes the profile `id` if it still has the stamp `expected`. The selected one is refused.
    pub fn delete_profile_checked(&mut self, id: i64, expected: &str) -> Result<Edit, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(there) = profiles_on(&tx)?.into_iter().find(|p| p.id == id) else {
            return Ok(Edit::Gone);
        };
        if there.fingerprint() != expected {
            return Ok(Edit::Stale);
        }
        let selected: Option<String> = tx
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                [SELECTED_KEY],
                |row| row.get(0),
            )
            .optional()?;
        if selected
            .unwrap_or_else(|| DEFAULT_NAME.to_owned())
            .eq_ignore_ascii_case(&there.name)
        {
            return Err(err(format!(
                "{} is the profile new chats start under: use another one first",
                there.name
            )));
        }
        tx.execute("DELETE FROM profiles WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(Edit::Done)
    }

    /// Adds a system prompt to the selected profile, or takes it out (`/prompts on` and `off`, and
    /// the web page's switch).
    pub(crate) fn set_in_selected_profile(
        &mut self,
        prompt_id: i64,
        member: bool,
    ) -> Result<(), StoreError> {
        let selected = self.selected_profile()?;
        let profile = self.profile(&selected).map_err(|_| {
            err(format!(
                "the profile {selected} does not exist: choose one with /profile use <name>"
            ))
        })?;
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if member {
            tx.execute(
                "INSERT OR IGNORE INTO profile_prompts (profile_id, prompt_id) VALUES (?1, ?2)",
                params![profile.id, prompt_id],
            )?;
        } else {
            tx.execute(
                "DELETE FROM profile_prompts WHERE profile_id = ?1 AND prompt_id = ?2",
                params![profile.id, prompt_id],
            )?;
        }
        tx.execute(
            "UPDATE profiles SET updated_at = ?2 WHERE id = ?1",
            params![profile.id, now_secs()],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn prompt_of_type(&self, name: &str, kind: PromptType) -> Result<i64, StoreError> {
        let prompt = self.prompt(name)?;
        if prompt.prompt_type != kind {
            return Err(err(format!(
                "{name} is a {}, not a {}",
                prompt.prompt_type.as_str(),
                kind.as_str()
            )));
        }
        Ok(prompt.id)
    }

    fn set_members(
        &mut self,
        name: &str,
        table: &str,
        kind: PromptType,
        names: &[String],
    ) -> Result<(), StoreError> {
        let profile = self.profile(name)?;
        let ids = names
            .iter()
            .map(|n| self.prompt_of_type(n, kind))
            .collect::<Result<Vec<_>, _>>()?;
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            &format!("DELETE FROM {table} WHERE profile_id = ?1"),
            [profile.id],
        )?;
        for id in ids {
            tx.execute(
                &format!("INSERT OR IGNORE INTO {table} (profile_id, prompt_id) VALUES (?1, ?2)"),
                params![profile.id, id],
            )?;
        }
        tx.execute(
            "UPDATE profiles SET updated_at = ?2 WHERE id = ?1",
            params![profile.id, now_secs()],
        )?;
        tx.commit()?;
        Ok(())
    }
}

/// Every profile as `conn` (or a transaction on it) sees it, by name (ignoring case).
fn profiles_on(conn: &Connection) -> Result<Vec<Profile>, StoreError> {
    let names = |sql: &str, id: i64| -> Result<Vec<String>, StoreError> {
        let mut statement = conn.prepare(sql)?;
        let rows = statement.query_map([id], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    };
    let mut statement = conn.prepare(
        "SELECT p.id, p.name, p.description, \
                (SELECT q.name FROM prompts q WHERE q.id = p.personality_id) \
         FROM profiles p ORDER BY p.name COLLATE NOCASE",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let heads = rows.collect::<Result<Vec<_>, _>>()?;
    let mut out = Vec::with_capacity(heads.len());
    for (id, name, description, personality) in heads {
        out.push(Profile {
            id,
            name,
            description,
            personality,
            prompts: names(
                "SELECT q.name FROM profile_prompts m JOIN prompts q ON q.id = m.prompt_id \
                 WHERE m.profile_id = ?1 ORDER BY q.position, q.id",
                id,
            )?,
            skills: names(
                "SELECT q.name FROM profile_skills m JOIN prompts q ON q.id = m.prompt_id \
                 WHERE m.profile_id = ?1 ORDER BY q.position, q.id",
                id,
            )?,
        });
    }
    Ok(out)
}

/// The id of the prompt `name` if it is of type `kind`.
fn member_id(conn: &Connection, name: &str, kind: PromptType) -> Result<i64, StoreError> {
    let found: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, type FROM prompts WHERE name = ?1",
            [name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match found {
        None => Err(err(format!(
            "there is no prompt named {name} (see /prompts)"
        ))),
        Some((id, found_kind)) if found_kind == kind.as_str() => Ok(id),
        Some((_, found_kind)) => Err(err(format!(
            "{name} is a {found_kind}, not a {}",
            kind.as_str()
        ))),
    }
}

/// An error if a profile of this name (ignoring case) exists.
fn taken(conn: &Connection, name: &str) -> Result<(), StoreError> {
    let found: Option<i64> = conn
        .query_row("SELECT id FROM profiles WHERE name = ?1", [name], |row| {
            row.get(0)
        })
        .optional()?;
    if found.is_some() {
        return Err(err(format!("there is already a profile named {name}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::Scratch;

    fn open(dir: &Scratch) -> Store {
        Store::open(&dir.path().join("harness.db")).unwrap()
    }

    fn names(profiles: &[Profile]) -> Vec<&str> {
        profiles.iter().map(|p| p.name.as_str()).collect()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_new_database_has_default_selected_and_holding_base() {
        let dir = Scratch::new("profiles-fresh");
        let store = open(&dir);
        let all = store.list_profiles().unwrap();
        assert_eq!(names(&all), ["default"]);
        assert_eq!(all[0].prompts, ["base"]);
        assert!(all[0].skills.is_empty() && all[0].personality.is_none());
        assert_eq!(store.selected_profile().unwrap(), "default");
        let start = store.start_of_selected_profile().unwrap();
        assert_eq!(start.profile, "default");
        assert_eq!(start.assembled.names, ["base"]);
        assert!(!start.assembled.fallback);
        assert!(start.chat.is_empty());
    }

    #[test]
    fn the_seeded_profiles_are_base_plus_one_prompt_and_never_overwritten() {
        let dir = Scratch::new("profiles-seed");
        let mut store = open(&dir);
        seed(store.conn(), true).unwrap();
        let all = store.list_profiles().unwrap();
        assert_eq!(names(&all), ["builder", "default", "planner"]);
        for (profile, prompt) in [("planner", "planner"), ("builder", "builder")] {
            let p = store.profile(profile).unwrap();
            assert_eq!(p.prompts, ["base", prompt], "{profile}");
            assert!(store.prompt(prompt).unwrap().text.len() > 100);
        }
        // The owner edits one and removes another; running the seed again changes none of it.
        store
            .set_prompt_text("planner", "my planner", None)
            .unwrap();
        store.delete_profile("builder").unwrap();
        store
            .add_prompt("builder", "mine", "my own prompt called builder", None)
            .ok();
        seed(store.conn(), true).unwrap();
        assert_eq!(store.prompt("planner").unwrap().text, "my planner");
        assert_eq!(
            names(&store.list_profiles().unwrap()),
            ["default", "planner"]
        );
        assert_eq!(
            store.prompt("builder").unwrap().description,
            "build what was asked, in small steps"
        );
    }

    #[test]
    fn a_checked_edit_replaces_everything_at_once_and_a_rename_keeps_the_selection() {
        let dir = Scratch::new("profiles-edit-checked");
        let mut store = open(&dir);
        store.add_prompt("extra", "", "E", None).unwrap();
        store
            .add_prompt_of_type("tdd", "", "T", None, PromptType::Skill)
            .unwrap();
        store
            .add_prompt_of_type("pirate", "", "P", None, PromptType::Personality)
            .unwrap();
        let before = store.profile("default").unwrap();
        let done = store
            .edit_profile(
                before.id,
                &before.fingerprint(),
                "main",
                " the main one ",
                &strings(&["base", "extra"]),
                &strings(&["tdd"]),
                Some("pirate"),
            )
            .unwrap();
        assert_eq!(done, Edit::Done);
        let after = store.profile("main").unwrap();
        assert_eq!(after.description, "the main one");
        assert_eq!(after.prompts, ["base", "extra"]);
        assert_eq!(
            (after.skills.clone(), after.personality.clone()),
            (strings(&["tdd"]), Some("pirate".to_owned()))
        );
        assert_eq!(
            store.selected_profile().unwrap(),
            "main",
            "renaming the selected one keeps it selected"
        );
        assert_ne!(before.fingerprint(), after.fingerprint());
        // The old stamp is now stale and nothing is written; an unknown id is gone.
        let stale = store
            .edit_profile(before.id, &before.fingerprint(), "x", "", &[], &[], None)
            .unwrap();
        assert_eq!(stale, Edit::Stale);
        assert_eq!(store.profile("main").unwrap(), after);
        let gone = store
            .edit_profile(9999, "x", "x", "", &[], &[], None)
            .unwrap();
        assert_eq!(gone, Edit::Gone);
    }

    #[test]
    fn a_checked_edit_that_is_refused_changes_nothing() {
        let dir = Scratch::new("profiles-edit-refused");
        let mut store = open(&dir);
        store
            .add_prompt_of_type("tdd", "", "T", None, PromptType::Skill)
            .unwrap();
        store.add_profile("other", "").unwrap();
        let default = store.profile("default").unwrap();
        type Attempt<'a> = (&'a str, Vec<String>, Vec<String>, Option<&'a str>);
        let attempts: [Attempt; 5] = [
            ("other", vec![], vec![], None),
            ("default", strings(&["tdd"]), vec![], None),
            ("default", vec![], strings(&["base"]), None),
            ("default", vec![], vec![], Some("base")),
            ("default", strings(&["ghost"]), vec![], None),
        ];
        for (name, prompts, skills, personality) in attempts {
            let result = store.edit_profile(
                default.id,
                &default.fingerprint(),
                name,
                "changed",
                &prompts,
                &skills,
                personality,
            );
            assert!(
                result.is_err(),
                "{name} {prompts:?} {skills:?} {personality:?}"
            );
            assert_eq!(store.profile("default").unwrap(), default);
        }
        assert!(
            store
                .edit_profile(
                    default.id,
                    &default.fingerprint(),
                    "list",
                    "",
                    &[],
                    &[],
                    None
                )
                .is_err()
        );
    }

    #[test]
    fn a_checked_delete_needs_the_current_stamp_and_never_takes_the_selected_profile() {
        let dir = Scratch::new("profiles-delete-checked");
        let mut store = open(&dir);
        store.add_profile("other", "").unwrap();
        let other = store.profile("other").unwrap();
        assert_eq!(
            store.delete_profile_checked(other.id, "stale").unwrap(),
            Edit::Stale
        );
        assert!(store.profile("other").is_ok());
        let default = store.profile("default").unwrap();
        let why = store
            .delete_profile_checked(default.id, &default.fingerprint())
            .unwrap_err()
            .to_string();
        assert!(why.contains("use another one first"), "{why}");
        assert_eq!(
            store
                .delete_profile_checked(other.id, &other.fingerprint())
                .unwrap(),
            Edit::Done
        );
        assert!(store.profile("other").is_err());
        assert_eq!(
            store
                .delete_profile_checked(other.id, &other.fingerprint())
                .unwrap(),
            Edit::Gone
        );
    }

    #[test]
    fn a_name_the_owner_already_uses_is_not_seeded_over() {
        let dir = Scratch::new("profiles-seed-taken");
        let mut store = open(&dir);
        store
            .add_prompt("planner", "mine", "my planner prompt", None)
            .unwrap();
        store.add_profile("builder", "mine").unwrap();
        seed(store.conn(), true).unwrap();
        assert_eq!(store.prompt("planner").unwrap().text, "my planner prompt");
        assert!(
            store.profile("planner").is_err(),
            "no planner profile: its prompt name was taken"
        );
        assert_eq!(store.profile("builder").unwrap().description, "mine");
        assert!(store.profile("planner").is_err());
        assert!(
            store.profile("sub-agent").is_err(),
            "no profile is made only for subagents"
        );
    }

    #[test]
    fn switching_a_prompt_on_or_off_edits_the_selected_profile_only() {
        let dir = Scratch::new("profiles-toggle");
        let mut store = open(&dir);
        store.add_prompt("extra", "", "Extra rules.", None).unwrap();
        store.add_profile("other", "").unwrap();
        store
            .set_profile_prompts("other", &strings(&["base"]))
            .unwrap();
        store.set_prompt_enabled("extra", true).unwrap();
        assert_eq!(store.profile("default").unwrap().prompts, ["base", "extra"]);
        assert_eq!(store.profile("other").unwrap().prompts, ["base"]);
        assert!(store.prompt("extra").unwrap().enabled);
        assert_eq!(
            store.start_of_selected_profile().unwrap().assembled.names,
            ["base", "extra"]
        );
        // Another profile selected: the same prompt now reads as off, and the chat gets its set.
        store.select_profile("OTHER").unwrap();
        assert_eq!(store.selected_profile().unwrap(), "other");
        assert!(!store.prompt("extra").unwrap().enabled);
        assert_eq!(
            store.start_of_selected_profile().unwrap().assembled.names,
            ["base"]
        );
        store.set_prompt_enabled("base", false).unwrap();
        let start = store.start_of_selected_profile().unwrap();
        assert!(
            start.assembled.fallback,
            "nothing in the profile: the built-in text"
        );
        assert_eq!(store.profile("default").unwrap().prompts, ["base", "extra"]);
    }

    #[test]
    fn a_new_chat_gets_the_skills_and_personality_of_the_profile() {
        let dir = Scratch::new("profiles-start");
        let mut store = open(&dir);
        store
            .add_prompt_of_type("tdd", "", "Test first.", None, PromptType::Skill)
            .unwrap();
        store
            .add_prompt_of_type("lint", "", "Lint.", None, PromptType::Skill)
            .unwrap();
        store
            .add_prompt_of_type("pirate", "", "Arr.", None, PromptType::Personality)
            .unwrap();
        store.add_profile("work", "for work").unwrap();
        store
            .set_profile_skills("work", &strings(&["lint", "tdd"]))
            .unwrap();
        store
            .set_profile_personality("work", Some("pirate"))
            .unwrap();
        let work = store.profile("work").unwrap();
        // Library order, not the order they were given.
        assert_eq!(
            (work.skills.clone(), work.personality.clone()),
            (strings(&["tdd", "lint"]), Some("pirate".to_owned()))
        );
        store.select_profile("work").unwrap();
        let start = store.start_of_selected_profile().unwrap();
        assert_eq!(start.profile, "work");
        assert_eq!(start.chat.on, ["tdd", "lint"]);
        assert_eq!(start.chat.personality.as_deref(), Some("pirate"));
        assert!(start.chat.once.is_empty());
        assert!(
            start.assembled.fallback,
            "the profile holds no system prompt"
        );
        store.set_profile_personality("work", None).unwrap();
        assert!(store.profile("work").unwrap().personality.is_none());
    }

    #[test]
    fn only_prompts_of_the_right_type_can_be_put_in_a_profile() {
        let dir = Scratch::new("profiles-kinds");
        let mut store = open(&dir);
        store
            .add_prompt_of_type("tdd", "", "T", None, PromptType::Skill)
            .unwrap();
        store
            .add_prompt_of_type("pirate", "", "A", None, PromptType::Personality)
            .unwrap();
        for result in [
            store.set_profile_prompts("default", &strings(&["tdd"])),
            store.set_profile_prompts("default", &strings(&["nope"])),
            store.set_profile_skills("default", &strings(&["base"])),
            store.set_profile_skills("default", &strings(&["pirate"])),
            store.set_profile_personality("default", Some("tdd")),
            store.set_profile_personality("default", Some("nope")),
            store.set_profile_prompts("nope", &[]),
        ] {
            assert!(result.is_err());
        }
        assert_eq!(
            store.profile("default").unwrap().prompts,
            ["base"],
            "a refused set changes nothing"
        );
    }

    #[test]
    fn profiles_can_be_renamed_copied_and_deleted_with_the_selected_one_protected() {
        let dir = Scratch::new("profiles-crud");
        let mut store = open(&dir);
        store
            .add_prompt_of_type("tdd", "", "T", None, PromptType::Skill)
            .unwrap();
        store
            .add_prompt_of_type("pirate", "", "A", None, PromptType::Personality)
            .unwrap();
        store
            .set_profile_skills("default", &strings(&["tdd"]))
            .unwrap();
        store
            .set_profile_personality("default", Some("pirate"))
            .unwrap();
        store
            .set_profile_description("default", "  the usual  ")
            .unwrap();
        // Copy: everything but the name.
        store.copy_profile("default", "second").unwrap();
        let second = store.profile("second").unwrap();
        let first = store.profile("default").unwrap();
        assert_eq!(
            (
                second.description.as_str(),
                &second.prompts,
                &second.skills,
                &second.personality
            ),
            (
                "the usual",
                &first.prompts,
                &first.skills,
                &first.personality
            )
        );
        assert!(store.copy_profile("default", "SECOND").is_err());
        assert!(store.copy_profile("nope", "x").is_err());
        // Renaming the selected profile keeps it selected.
        store.rename_profile("default", "main").unwrap();
        assert_eq!(store.selected_profile().unwrap(), "main");
        assert!(store.rename_profile("second", "MAIN").is_err());
        assert!(store.rename_profile("second", "bad name").is_err());
        assert!(store.profile("default").is_err());
        // The selected one is not deleted; another is, and its memberships go with it.
        assert!(store.delete_profile("main").is_err());
        store.delete_profile("second").unwrap();
        let rows = |sql: &str| -> i64 { store.conn().query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(
            rows(
                "SELECT COUNT(*) FROM profile_prompts WHERE profile_id NOT IN (SELECT id FROM profiles)"
            ),
            0
        );
        assert_eq!(rows("SELECT COUNT(*) FROM profiles"), 1);
        assert!(store.select_profile("second").is_err());
        assert_eq!(store.selected_profile().unwrap(), "main");
    }

    #[test]
    fn deleting_a_prompt_takes_it_out_of_every_profile() {
        let dir = Scratch::new("profiles-prompt-delete");
        let mut store = open(&dir);
        store.add_prompt("extra", "", "E", None).unwrap();
        store
            .add_prompt_of_type("tdd", "", "T", None, PromptType::Skill)
            .unwrap();
        store
            .add_prompt_of_type("pirate", "", "A", None, PromptType::Personality)
            .unwrap();
        store
            .set_profile_prompts("default", &strings(&["base", "extra"]))
            .unwrap();
        store
            .set_profile_skills("default", &strings(&["tdd"]))
            .unwrap();
        store
            .set_profile_personality("default", Some("pirate"))
            .unwrap();
        store.delete_prompt("extra").unwrap();
        store.delete_prompt("tdd").unwrap();
        store.delete_prompt("pirate").unwrap();
        let p = store.profile("default").unwrap();
        assert_eq!(
            (p.prompts, p.skills, p.personality),
            (strings(&["base"]), vec![], None)
        );
    }

    #[test]
    fn names_and_descriptions_follow_the_rules() {
        let dir = Scratch::new("profiles-rules");
        let mut store = open(&dir);
        for bad in ["", "has space", "list", "use", ".hidden", &"x".repeat(61)] {
            assert!(store.add_profile(bad, "").is_err(), "{bad:?}");
        }
        assert!(
            store.add_profile("Default", "").is_err(),
            "names are unique ignoring case"
        );
        assert!(store.add_profile("ok", "two\nlines").is_err());
        assert!(store.add_profile("ok", &"d".repeat(201)).is_err());
        store.add_profile("ok", &"d".repeat(200)).unwrap();
        assert_eq!(store.profile("OK").unwrap().name, "ok");
    }

    #[test]
    fn a_subagent_starts_with_the_subagent_prompt_in_place_of_base() {
        let dir = Scratch::new("profiles-subagent");
        let mut store = open(&dir);
        seed(store.conn(), true).unwrap();
        crate::store_prompts::seed_subagent(store.conn(), true).unwrap();
        store
            .add_prompt_of_type("tdd", "", "Test first.", None, PromptType::Skill)
            .unwrap();
        store
            .add_prompt_of_type("terse", "", "Few words.", None, PromptType::Personality)
            .unwrap();
        let planner = store.profile("planner").unwrap();
        store
            .edit_profile(
                planner.id,
                &planner.fingerprint(),
                "planner",
                &planner.description,
                &strings(&["base", "planner"]),
                &strings(&["tdd"]),
                Some("terse"),
            )
            .unwrap();

        let start = store.start_of_subagent("Planner", false).unwrap();
        assert_eq!(start.profile, "planner");
        assert_eq!(start.assembled.names, ["subagent", "planner"]);
        let text = &start.assembled.text;
        assert!(text.starts_with("You are a subagent."), "{text}");
        assert!(
            text.contains("Your job is to plan, not to build."),
            "{text}"
        );
        assert!(!text.contains("You are a coding assistant"), "{text}");
        assert_eq!(start.chat.on, ["tdd"]);
        assert_eq!(start.chat.personality.as_deref(), Some("terse"));
        // The profile's own start is not touched.
        let main = store.profile("planner").unwrap();
        assert_eq!(main.prompts, ["base", "planner"]);
        // A profile of `base` alone gives the subagent prompt alone.
        let only = store.start_of_subagent("default", false).unwrap();
        assert_eq!(only.assembled.names, ["subagent"]);
        assert!(store.start_of_subagent("nope", false).is_err());
    }

    #[test]
    fn the_subagent_prompt_is_the_owners_to_edit_and_has_a_built_in_copy() {
        let dir = Scratch::new("profiles-subagent-text");
        let mut store = open(&dir);
        // Nothing is seeded in tests, so the built-in text stands in.
        assert!(store.prompt("subagent").is_err());
        let start = store.start_of_subagent("default", false).unwrap();
        assert_eq!(start.assembled.text, crate::store_prompts::SUBAGENT_PROMPT);
        // Seeding adds it once, switched off and in no profile; a copy the owner has is kept.
        crate::store_prompts::seed_subagent(store.conn(), true).unwrap();
        let seeded = store.prompt("subagent").unwrap();
        assert_eq!(seeded.text, crate::store_prompts::SUBAGENT_PROMPT);
        assert!(!seeded.enabled);
        assert!(
            store
                .list_profiles()
                .unwrap()
                .iter()
                .all(|p| !p.prompts.contains(&"subagent".to_owned()))
        );
        store
            .edit_prompt(
                seeded.id,
                &seeded.fingerprint(),
                "subagent",
                "mine",
                "Look only.",
            )
            .unwrap();
        crate::store_prompts::seed_subagent(store.conn(), true).unwrap();
        assert_eq!(store.prompt("subagent").unwrap().text, "Look only.");
        assert_eq!(
            store
                .start_of_subagent("default", false)
                .unwrap()
                .assembled
                .text,
            "Look only."
        );
    }

    #[test]
    fn a_coder_subagent_starts_with_the_coder_prompt_and_both_are_seeded_once() {
        let dir = Scratch::new("profiles-coder");
        let mut store = open(&dir);
        seed(store.conn(), true).unwrap();
        crate::store_prompts::seed_subagent(store.conn(), true).unwrap();
        crate::store_prompts::seed_subagent(store.conn(), true).unwrap();
        for name in ["subagent", "subagent-coder"] {
            let p = store.prompt(name).unwrap();
            assert!(!p.enabled, "{name}");
        }
        assert_eq!(
            store
                .list_prompts()
                .unwrap()
                .iter()
                .filter(|p| p.name.starts_with("subagent"))
                .count(),
            2
        );
        let start = store.start_of_subagent("builder", true).unwrap();
        assert_eq!(start.assembled.names, ["subagent-coder", "builder"]);
        assert!(start.assembled.text.starts_with("You are a subagent."));
        assert!(start.assembled.text.contains("you may change files"));
        assert!(start.assembled.text.contains("Your job is to build"));
        // A reader of the same profile gets the other prompt.
        let reader = store.start_of_subagent("builder", false).unwrap();
        assert_eq!(reader.assembled.names, ["subagent", "builder"]);
        assert!(reader.assembled.text.contains("you can only look"));
        // The owner's own copy is kept.
        let coder = store.prompt("subagent-coder").unwrap();
        store
            .edit_prompt(
                coder.id,
                &coder.fingerprint(),
                "subagent-coder",
                "mine",
                "Code it.",
            )
            .unwrap();
        crate::store_prompts::seed_subagent(store.conn(), true).unwrap();
        assert_eq!(
            store
                .start_of_subagent("default", true)
                .unwrap()
                .assembled
                .text,
            "Code it."
        );
    }

    #[test]
    fn upgrading_from_schema_11_builds_default_from_the_prompts_that_were_on() {
        let dir = Scratch::new("profiles-migrate");
        let path = dir.path().join("harness.db");
        let mut store = Store::open(&path).unwrap();
        store.add_prompt("extra", "", "Extra rules.", None).unwrap();
        store.add_prompt("unused", "", "Not on.", None).unwrap();
        store
            .add_prompt_of_type("tdd", "", "T", None, PromptType::Skill)
            .unwrap();
        store.set_prompt_enabled("extra", true).unwrap();
        let before = store.assembled_prompt().unwrap();
        assert_eq!(before.names, ["base", "extra"]);
        drop(store);
        {
            // Put the file back as schema 11 left it: flags on the prompts, no profile tables.
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "DROP TABLE profile_prompts; DROP TABLE profile_skills; DROP TABLE profiles; \
                 DELETE FROM settings WHERE key = 'profile'; \
                 UPDATE prompts SET enabled = 0; \
                 UPDATE prompts SET enabled = 1 WHERE name IN ('base', 'extra'); \
                 PRAGMA user_version = 11;",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let default = store.profile("default").unwrap();
        assert_eq!(default.prompts, ["base", "extra"]);
        assert!(default.skills.is_empty() && default.personality.is_none());
        assert_eq!(store.selected_profile().unwrap(), "default");
        let after = store.assembled_prompt().unwrap();
        assert_eq!(
            after.text, before.text,
            "the first chat after the upgrade has the same system prompt"
        );
        let copy = dir.path().join("harness.db.before-25");
        assert!(copy.exists());
        let old = rusqlite::Connection::open(&copy).unwrap();
        let version: i64 = old
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 11);
        assert!(old.prepare("SELECT 1 FROM profiles").is_err());
    }

    #[test]
    fn the_selected_profile_names_a_missing_one_gracefully() {
        let dir = Scratch::new("profiles-missing");
        let mut store = open(&dir);
        store.set_setting(SELECTED_KEY, "gone").unwrap();
        let start = store.start_of_selected_profile().unwrap();
        assert_eq!(start.profile, "gone");
        assert!(start.assembled.fallback && start.chat.is_empty());
        assert!(store.set_prompt_enabled("base", true).is_err());
        store.select_profile("default").unwrap();
        assert!(
            !store
                .start_of_selected_profile()
                .unwrap()
                .assembled
                .fallback
        );
    }
}

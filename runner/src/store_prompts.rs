//! The prompts the owner keeps (issue 13): named texts, some switched on. The ones switched on, in
//! order, are joined into the system prompt when a chat starts.
//!
//! Part of the store (`impl Store` continues here); the table layout is owned by `docs/erd.md`.
//! The texts the program ships with (the built-in prompt and the prompt-writing guide) live here
//! too, because the first row of the table is seeded from the built-in one.

use rusqlite::{OptionalExtension, params};

use crate::store::{Store, StoreError, sha256_hex};

/// The built-in text before issue 16 listed the tools. A database that still holds exactly this as
/// `base` gets the new text in schema 8; an edited `base` is left alone.
pub const PREVIOUS_BUILTIN_PROMPT: &str = "You are a coding assistant running in a terminal. You can read files and run shell commands with the tools provided. Use a tool when you need information you do not have, then answer concisely.";

/// The built-in text of schema 8 (the five tools). A database that still holds exactly this as
/// `base` gets the new text in schema 9.
pub const BUILTIN_PROMPT_V8: &str = "You are a coding assistant running in a terminal, working in one folder (the working directory). You have these tools:\n- read_file(path): read a UTF-8 text file.\n- create_file(path, content): create a new file, with any missing folders. Fails if the file exists.\n- write_file(path, content): replace the whole content of an existing file.\n- delete_file(path): delete one file. Folders are never deleted.\n- bash(command): run a shell command with bash -c in the working directory; it stops after 30 seconds.\nFile paths are relative to the working directory; absolute paths and .. are refused. Prefer the file tools to bash for reading and changing files. Looking commands such as ls, cd, pwd, cat and mkdir run without asking the user; any other command, and file changes in manual mode, need the user's approval. If a result says \"denied by user\", do not retry it: say what you wanted to do and ask. Use a tool when you need information you do not have, then answer concisely.";

/// The built-in text of schema 9 (six tools). A database that still holds exactly this as `base`
/// gets the new text in schema 10.
pub const BUILTIN_PROMPT_V9: &str = "You are a coding assistant running in a terminal, working in one folder (the working directory). You have these tools:\n- read_file(path): read a UTF-8 text file.\n- create_file(path, content): create a new file, with any missing folders. Fails if the file exists.\n- write_file(path, content): replace the whole content of an existing file.\n- edit_file(path, old_string, new_string, replace_all): change part of an existing file by replacing exact text; fails if old_string is not found or is not unique (unless replace_all is true).\n- delete_file(path): delete one file. Folders are never deleted.\n- bash(command): run a shell command with bash -c in the working directory; it stops after 30 seconds.\nFile paths are relative to the working directory; absolute paths and .. are refused. Prefer the file tools to bash for reading and changing files, and edit_file to write_file for changes: use write_file only for a full rewrite. Looking commands such as ls, cd, pwd, cat and mkdir run without asking the user; any other command, and file changes in manual mode, need the user's approval. If a result says \"denied by user\", do not retry it: say what you wanted to do and ask. Use a tool when you need information you do not have, then answer concisely.";

/// The system prompt the program had before prompts were stored. It seeds the `base` row, is used
/// when no prompt is switched on, and is what "restore built-in text" writes back.
pub const BUILTIN_PROMPT: &str = "You are a coding assistant running in a terminal, working in one folder (the working directory). You have these tools:\n- read_file(path): read a UTF-8 text file.\n- create_file(path, content): create a new file, with any missing folders. Fails if the file exists.\n- write_file(path, content): replace the whole content of an existing file.\n- edit_file(path, old_string, new_string, replace_all): change part of an existing file by replacing exact text; fails if old_string is not found or is not unique (unless replace_all is true).\n- delete_file(path): delete one file. Folders are never deleted.\n- bash(command): run a shell command with bash -c in the working directory; it stops after 30 seconds. Every call starts in the working directory again, so cd does not carry over: use paths like dir/file.\nRules:\n- Stay inside the working directory. File paths are relative; absolute paths and .. are refused. Do not write to /tmp or look through the rest of the computer (find /, ~).\n- Change files with edit_file for small changes, create_file for new files and write_file only for a full rewrite. Do not change files with shell redirects, heredocs or patch scripts.\n- To look, use ls, grep, find, head, tail, wc, cat or sed -n 'N,Mp' file. These, and pipes between them, run without asking the user; any other command, and file changes in manual mode, need the user's approval.\n- When you need several independent things, ask for them together in one response: every call re-sends the whole conversation, and a message is limited to 20 model calls.\n- If a result says \"denied by user\", do not retry it or find a way around it: say what you wanted to do and ask.\n- If something is not found after two tries (a file, a script, a command), stop searching and tell the user. Do not guess paths or scan the filesystem.\nUse a tool when you need information you do not have, then answer concisely.";

/// The name of the seeded prompt.
pub const BASE_NAME: &str = "base";
pub const BASE_DESCRIPTION: &str = "The built-in instructions";

/// What the "improve with AI" request is told. Sent as the system part of one request, never saved
/// in a chat and never part of a chat's own system prompt.
pub const PROMPT_WRITER_GUIDE: &str = "You rewrite system prompts for an AI coding assistant. You will receive a draft system prompt, and sometimes a note saying what to change. Return an improved version of the draft.

Rules:
- Keep the author's intent, facts, names, paths and commands exactly. Do not add requirements the draft does not imply. If something is unclear, keep it as written instead of guessing.
- Be specific. Replace vague wording such as \"be good\" or \"try to\" with instructions that can be followed and checked.
- State what output is expected: format, length and tone, and what to do when information is missing or a request is unclear.
- Say what to do, and name the few things never to do. Remove contradictions and repetition.
- Put the most important rules first. Use short sentences and plain lists. Use headings only when the prompt is long.
- Keep it as short as it can be without losing a rule.
- Write the prompt itself, addressed to the assistant (\"You ...\"). Do not describe or explain it.

Reply with the improved prompt only: no introduction, no comments about your changes, no code fences. If a note is given, follow it for this rewrite.";

/// Largest text one prompt may hold.
pub const MAX_PROMPT_BYTES: usize = 200_000;

/// Words a prompt may not be called, because `/prompts` uses them.
const RESERVED: &[&str] = &[
    "add", "edit", "delete", "on", "off", "show", "config", "none", "use", "type",
];

/// What a prompt is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptType {
    /// Joined into the system prompt when a chat starts.
    System,
    /// Added to requests after the user's message while it is on, or for one message.
    Skill,
    /// One of these is selected at a time; added like a skill, for every message.
    Personality,
}

impl PromptType {
    pub fn as_str(self) -> &'static str {
        match self {
            PromptType::System => "system",
            PromptType::Skill => "skill",
            PromptType::Personality => "personality",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "system" => Some(PromptType::System),
            "skill" => Some(PromptType::Skill),
            "personality" => Some(PromptType::Personality),
            _ => None,
        }
    }
}

/// What a skill is doing in a chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillState {
    Off,
    /// For the next message only.
    Once,
    On,
}

/// The skills and personality of one chat. Names, not ids: a prompt that is renamed or deleted
/// simply stops matching. `once` lives in memory only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatPrompts {
    pub personality: Option<String>,
    pub on: Vec<String>,
    pub once: Vec<String>,
}

impl ChatPrompts {
    pub fn is_empty(&self) -> bool {
        self.personality.is_none() && self.on.is_empty() && self.once.is_empty()
    }

    pub fn skill_state(&self, name: &str) -> SkillState {
        if self.once.iter().any(|n| n == name) {
            SkillState::Once
        } else if self.on.iter().any(|n| n == name) {
            SkillState::On
        } else {
            SkillState::Off
        }
    }

    /// One line for the screen, empty when nothing is active.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(personality) = &self.personality {
            parts.push(format!("personality {personality}"));
        }
        if !self.on.is_empty() {
            parts.push(format!("skills on: {}", self.on.join(", ")));
        }
        if !self.once.is_empty() {
            parts.push(format!("this message: {}", self.once.join(", ")));
        }
        parts.join("; ")
    }
}

/// The start of every saved "active instructions" message. A resumed chat shows these as one
/// line, and the agent finds the newest one to see whether the instructions changed.
pub const ACTIVE_MARK: &str =
    "Active instructions (set in the program's settings, not written by the user)";

/// The start of the message a compacted chat begins with (issue 24). A resumed chat shows it as
/// one line.
pub const SUMMARY_MARK: &str = "Summary of the earlier conversation in this chat (written by the model at the user's request; the full chat is saved separately). Continue from it.";

/// The text of the first message of a compacted chat: the fixed first sentence, then (when the
/// old chat was saved) `[earlier chat: N]`, then the model's summary. The number lets the chat
/// list warn before the chat it came from is deleted.
pub fn summary_message(earlier: Option<i64>, summary: &str) -> String {
    match earlier {
        Some(id) => format!("{SUMMARY_MARK}\n\n{}\n\n{summary}", earlier_line(id)),
        None => format!("{SUMMARY_MARK}\n\n{summary}"),
    }
}

/// `[earlier chat: N]`, the line that names the chat a summary was made from.
pub fn earlier_line(id: i64) -> String {
    format!("[earlier chat: {id}]")
}

/// The summary text itself, without the fixed sentence and without the `[earlier chat: N]` line.
pub fn summary_body(message: &str) -> &str {
    let rest = message.strip_prefix(SUMMARY_MARK).unwrap_or(message).trim();
    match rest.strip_prefix("[earlier chat: ") {
        Some(after) => match after.split_once(']') {
            Some((number, tail)) if number.parse::<i64>().is_ok() => tail.trim(),
            _ => rest,
        },
        None => rest,
    }
}

/// Added to the chat when nothing is active any more but an earlier block is.
pub const NONE_ACTIVE: &str = "Active instructions (set in the program's settings, not written by the user): none are active now. Ignore any earlier \"Active instructions\" message.";

fn sections(
    prompts: &[Prompt],
    state: &ChatPrompts,
    personality: bool,
    skills: &[String],
) -> Vec<String> {
    let mut sections: Vec<String> = Vec::new();
    if personality
        && let Some(name) = &state.personality
        && let Some(p) = prompts
            .iter()
            .find(|p| &p.name == name && p.prompt_type == PromptType::Personality)
    {
        sections.push(format!("## Personality: {}\n{}", p.name, p.text.trim()));
    }
    let mut chosen: Vec<&Prompt> = prompts
        .iter()
        .filter(|p| p.prompt_type == PromptType::Skill && skills.contains(&p.name))
        .collect();
    chosen.sort_by_key(|p| (p.position, p.id));
    for p in chosen {
        sections.push(format!("## Skill: {}\n{}", p.name, p.text.trim()));
    }
    sections
}

/// The personality and the skills that are on, as one message to put in the chat before the user's
/// message when it differs from the newest such message already there (issue 20). `None` when
/// nothing is active. A name with no matching prompt of the right type is skipped.
pub fn compose_persistent(prompts: &[Prompt], state: &ChatPrompts) -> Option<String> {
    let sections = sections(prompts, state, true, &state.on);
    if sections.is_empty() {
        return None;
    }
    Some(format!(
        "{ACTIVE_MARK}. They replace any earlier \"Active instructions\" message and stay in force until a newer one:\n\n{}",
        sections.join("\n\n")
    ))
}

/// The skills asked for one message only: added after the newest message on each call of that
/// message and never kept in the chat. `None` when there are none.
pub fn compose_once(prompts: &[Prompt], state: &ChatPrompts) -> Option<String> {
    let sections = sections(prompts, state, false, &state.once);
    if sections.is_empty() {
        return None;
    }
    Some(format!(
        "Active instructions for this message only (they come from the program's settings, not from the user's message):\n\n{}",
        sections.join("\n\n")
    ))
}

/// One stored prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub text: String,
    /// The file it was imported from, if it was.
    pub source_path: Option<String>,
    pub enabled: bool,
    pub position: i64,
    pub prompt_type: PromptType,
}

impl Prompt {
    pub fn chars(&self) -> usize {
        self.text.chars().count()
    }
}

/// What the switched-on prompts add up to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assembled {
    pub text: String,
    /// The names that went into it, in order. Empty when the built-in text was used.
    pub names: Vec<String>,
    /// Nothing was switched on, so the built-in text stands in.
    pub fallback: bool,
}

impl Assembled {
    pub fn chars(&self) -> usize {
        self.text.chars().count()
    }
}

/// The switched-on prompts, in position order, each trimmed, joined by one blank line. With none
/// switched on, the built-in text.
pub fn assemble(prompts: &[Prompt]) -> Assembled {
    let mut on: Vec<&Prompt> = prompts
        .iter()
        .filter(|p| p.enabled && p.prompt_type == PromptType::System)
        .collect();
    on.sort_by_key(|p| (p.position, p.id));
    if on.is_empty() {
        return Assembled {
            text: BUILTIN_PROMPT.to_owned(),
            names: Vec::new(),
            fallback: true,
        };
    }
    Assembled {
        text: on
            .iter()
            .map(|p| p.text.trim())
            .collect::<Vec<_>>()
            .join("\n\n"),
        names: on.iter().map(|p| p.name.clone()).collect(),
        fallback: false,
    }
}

/// A prompt's name: the provider-name rules, without the words `/prompts` uses.
pub fn check_prompt_name(name: &str) -> Result<(), String> {
    crate::providers::check_name_among(name, RESERVED, "/prompts")
}

/// A prompt's text: something, and not enormous.
pub fn check_prompt_text(text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("the prompt is empty".to_owned());
    }
    if text.len() > MAX_PROMPT_BYTES {
        return Err(format!(
            "the prompt is {} bytes; the limit is {MAX_PROMPT_BYTES}",
            text.len()
        ));
    }
    Ok(())
}

/// A size for the screen: `170 chars`, `4.2k chars`.
pub fn size_label(chars: usize) -> String {
    if chars < 1000 {
        format!("{chars} chars")
    } else {
        format!("{:.1}k chars", chars as f64 / 1000.0)
    }
}

/// A rough token count: characters divided by four, rounded up. Only an estimate.
pub fn estimated_tokens(chars: usize) -> usize {
    chars.div_ceil(4)
}

/// A short hash of a text, to tell two prompts apart on screen.
pub fn short_hash(text: &str) -> String {
    sha256_hex(text.as_bytes()).chars().take(12).collect()
}

fn err(text: impl Into<String>) -> StoreError {
    StoreError::new(text)
}

fn read_prompt(row: &rusqlite::Row<'_>) -> rusqlite::Result<Prompt> {
    Ok(Prompt {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        text: row.get(3)?,
        source_path: row.get(4)?,
        enabled: row.get::<_, i64>(5)? == 1,
        position: row.get(6)?,
        prompt_type: PromptType::parse(&row.get::<_, String>(7)?).unwrap_or(PromptType::System),
    })
}

const COLUMNS: &str = "id, name, description, text, source_path, enabled, position, type";

impl Store {
    /// Every prompt, in position order.
    pub fn list_prompts(&self) -> Result<Vec<Prompt>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM prompts ORDER BY position, id"
        ))?;
        let rows = statement.query_map([], read_prompt)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// One prompt by name.
    pub fn prompt(&self, name: &str) -> Result<Prompt, StoreError> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM prompts WHERE name = ?1"),
                [name],
                read_prompt,
            )
            .optional()?
            .ok_or_else(|| err(format!("there is no prompt named {name} (see /prompts)")))
    }

    /// What a chat started now would get as its system prompt.
    pub fn assembled_prompt(&self) -> Result<Assembled, StoreError> {
        Ok(assemble(&self.list_prompts()?))
    }

    /// Saves a new prompt at the end of the list, switched off: nothing is sent to the model
    /// until it is switched on.
    pub fn add_prompt(
        &mut self,
        name: &str,
        description: &str,
        text: &str,
        source_path: Option<&str>,
    ) -> Result<(), StoreError> {
        self.add_prompt_of_type(name, description, text, source_path, PromptType::System)
    }

    /// Like [`Store::add_prompt`], for a prompt of this type.
    pub fn add_prompt_of_type(
        &mut self,
        name: &str,
        description: &str,
        text: &str,
        source_path: Option<&str>,
        prompt_type: PromptType,
    ) -> Result<(), StoreError> {
        check_prompt_name(name).map_err(err)?;
        check_prompt_text(text).map_err(err)?;
        let tx = self.conn.transaction()?;
        let taken: i64 = tx.query_row(
            "SELECT COUNT(*) FROM prompts WHERE name = ?1",
            [name],
            |row| row.get(0),
        )?;
        if taken > 0 {
            return Err(err(format!("there is already a prompt named {name}")));
        }
        let position: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position), 0) + 1 FROM prompts",
            [],
            |row| row.get(0),
        )?;
        let now = crate::store::now_secs();
        tx.execute(
            "INSERT INTO prompts (name, description, text, source_path, enabled, position, created_at, updated_at, type) \
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?6, ?7)",
            params![
                name,
                description,
                text,
                source_path,
                position,
                now,
                prompt_type.as_str()
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Replaces a prompt's text (and, for a reload, where it came from).
    pub fn set_prompt_text(
        &mut self,
        name: &str,
        text: &str,
        source_path: Option<&str>,
    ) -> Result<(), StoreError> {
        check_prompt_text(text).map_err(err)?;
        let changed = self.conn.execute(
            "UPDATE prompts SET text = ?2, source_path = COALESCE(?3, source_path), updated_at = ?4 WHERE name = ?1",
            params![name, text, source_path, crate::store::now_secs()],
        )?;
        self.exists(changed, name)
    }

    pub fn set_prompt_description(
        &mut self,
        name: &str,
        description: &str,
    ) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE prompts SET description = ?2, updated_at = ?3 WHERE name = ?1",
            params![name, description, crate::store::now_secs()],
        )?;
        self.exists(changed, name)
    }

    /// Switches a system prompt on or off. Skills and personalities are never on in the library:
    /// a chat chooses them (`ChatPrompts`), and a new chat starts with none.
    pub fn set_prompt_enabled(&mut self, name: &str, enabled: bool) -> Result<(), StoreError> {
        let prompt = self.prompt(name)?;
        if prompt.prompt_type != PromptType::System {
            return Err(err(format!(
                "{name} is a {}: it is chosen in a chat, not switched on here",
                prompt.prompt_type.as_str()
            )));
        }
        let changed = self.conn.execute(
            "UPDATE prompts SET enabled = ?2, updated_at = ?3 WHERE name = ?1",
            params![name, i64::from(enabled), crate::store::now_secs()],
        )?;
        self.exists(changed, name)
    }

    /// The skills and personality a saved chat had.
    pub fn chat_prompts(&self, session_id: i64) -> Result<ChatPrompts, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT p.name, p.type, s.state FROM chat_prompt_state s \
             JOIN prompts p ON p.id = s.prompt_id WHERE s.session_id = ?1 \
             ORDER BY p.position, p.id",
        )?;
        let rows = statement.query_map([session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut state = ChatPrompts::default();
        for row in rows {
            let (name, kind, how) = row?;
            match (kind.as_str(), how.as_str()) {
                ("personality", "selected") => state.personality = Some(name),
                ("skill", "on") => state.on.push(name),
                _ => {}
            }
        }
        Ok(state)
    }

    /// Saves the skills that are on and the personality of a chat (what is only for one message
    /// is not saved). A name that matches no prompt is left out.
    pub fn set_chat_prompts(
        &mut self,
        session_id: i64,
        state: &ChatPrompts,
    ) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM chat_prompt_state WHERE session_id = ?1",
            [session_id],
        )?;
        let entries = state
            .personality
            .iter()
            .map(|n| (n, "selected"))
            .chain(state.on.iter().map(|n| (n, "on")));
        for (name, how) in entries {
            tx.execute(
                "INSERT OR IGNORE INTO chat_prompt_state (session_id, prompt_id, state) \
                 SELECT ?1, id, ?3 FROM prompts WHERE name = ?2",
                params![session_id, name, how],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn rename_prompt(&mut self, old: &str, new: &str) -> Result<(), StoreError> {
        check_prompt_name(new).map_err(err)?;
        if old != new {
            let taken: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM prompts WHERE name = ?1",
                [new],
                |row| row.get(0),
            )?;
            if taken > 0 {
                return Err(err(format!("there is already a prompt named {new}")));
            }
        }
        let changed = self.conn.execute(
            "UPDATE prompts SET name = ?2, updated_at = ?3 WHERE name = ?1",
            params![old, new, crate::store::now_secs()],
        )?;
        self.exists(changed, old)
    }

    /// Moves a prompt one place earlier (`-1`) or later (`1`) by swapping places with its
    /// neighbour. At either end nothing happens and `false` comes back.
    pub fn move_prompt(&mut self, name: &str, delta: i32) -> Result<bool, StoreError> {
        let all = self.list_prompts()?;
        let at = all
            .iter()
            .position(|p| p.name == name)
            .ok_or_else(|| err(format!("there is no prompt named {name} (see /prompts)")))?;
        let to = match delta {
            d if d < 0 => at.checked_sub(1),
            _ => (at + 1 < all.len()).then_some(at + 1),
        };
        let Some(to) = to else {
            return Ok(false);
        };
        let tx = self.conn.transaction()?;
        // Positions are renumbered 1..n so they stay distinct whatever came before.
        let mut order: Vec<i64> = all.iter().map(|p| p.id).collect();
        order.swap(at, to);
        for (index, id) in order.iter().enumerate() {
            tx.execute(
                "UPDATE prompts SET position = ?2 WHERE id = ?1",
                params![id, i64::try_from(index + 1).unwrap_or(i64::MAX)],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    pub fn delete_prompt(&mut self, name: &str) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM chat_prompt_state WHERE prompt_id IN (SELECT id FROM prompts WHERE name = ?1)",
            [name],
        )?;
        let changed = tx.execute("DELETE FROM prompts WHERE name = ?1", [name])?;
        if changed == 0 {
            return Err(err(format!(
                "there is no prompt named {name} (see /prompts)"
            )));
        }
        tx.commit()?;
        Ok(())
    }

    fn exists(&self, changed: usize, name: &str) -> Result<(), StoreError> {
        if changed == 0 {
            return Err(err(format!(
                "there is no prompt named {name} (see /prompts)"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::Scratch;

    fn open(dir: &Scratch) -> Store {
        Store::open(&dir.path().join("harness.db")).unwrap()
    }

    fn prompt(name: &str, text: &str, enabled: bool, position: i64) -> Prompt {
        Prompt {
            id: position,
            name: name.to_owned(),
            description: String::new(),
            text: text.to_owned(),
            source_path: None,
            enabled,
            position,
            prompt_type: PromptType::System,
        }
    }

    #[test]
    fn the_summary_message_names_its_chat_and_the_body_leaves_that_out() {
        let with = summary_message(Some(7), "the text");
        assert_eq!(
            with,
            format!("{SUMMARY_MARK}\n\n[earlier chat: 7]\n\nthe text")
        );
        assert_eq!(summary_body(&with), "the text");
        let without = summary_message(None, "the text");
        assert_eq!(without, format!("{SUMMARY_MARK}\n\nthe text"));
        assert_eq!(summary_body(&without), "the text");
        // A summary that merely starts with something like the line is left alone.
        let odd = summary_message(None, "[earlier chat: x] more");
        assert_eq!(summary_body(&odd), "[earlier chat: x] more");
    }

    #[test]
    fn a_new_database_starts_with_the_built_in_prompt_switched_on() {
        let dir = Scratch::new("prompts-seed");
        let store = open(&dir);
        let all = store.list_prompts().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, BASE_NAME);
        assert_eq!(all[0].text, BUILTIN_PROMPT);
        assert!(all[0].enabled);
        let assembled = store.assembled_prompt().unwrap();
        assert_eq!(assembled.text, BUILTIN_PROMPT);
        assert_eq!(assembled.names, ["base"]);
        assert!(!assembled.fallback);
    }

    #[test]
    fn assembly_orders_trims_and_joins_with_one_blank_line() {
        let all = [
            prompt("c", "third\n", true, 3),
            prompt("a", "  first  ", true, 1),
            prompt("off", "never", false, 2),
            prompt("b", "\nsecond\n\n", true, 2),
        ];
        let assembled = assemble(&all);
        assert_eq!(assembled.text, "first\n\nsecond\n\nthird");
        assert_eq!(assembled.names, ["a", "b", "c"]);
        // The same input gives the same bytes.
        assert_eq!(assemble(&all), assembled);
    }

    #[test]
    fn with_nothing_on_the_built_in_text_stands_in() {
        let assembled = assemble(&[prompt("a", "x", false, 1)]);
        assert_eq!(assembled.text, BUILTIN_PROMPT);
        assert!(assembled.fallback && assembled.names.is_empty());
        assert!(assemble(&[]).fallback);
    }

    #[test]
    fn prompts_are_added_off_at_the_end_and_switched_on_and_off() {
        let dir = Scratch::new("prompts-add");
        let mut store = open(&dir);
        store
            .add_prompt("review", "check tests", "Review the diff.", None)
            .unwrap();
        let added = store.prompt("review").unwrap();
        assert!(!added.enabled);
        assert_eq!(added.position, 2);
        assert_eq!(added.description, "check tests");
        assert_eq!(store.assembled_prompt().unwrap().text, BUILTIN_PROMPT);
        store.set_prompt_enabled("review", true).unwrap();
        assert_eq!(
            store.assembled_prompt().unwrap().text,
            format!("{BUILTIN_PROMPT}\n\nReview the diff.")
        );
        store.set_prompt_enabled("base", false).unwrap();
        assert_eq!(store.assembled_prompt().unwrap().text, "Review the diff.");
    }

    #[test]
    fn bad_names_texts_and_duplicates_are_refused() {
        let dir = Scratch::new("prompts-bad");
        let mut store = open(&dir);
        for name in ["", "has space", "../x", "on", "delete", ".hidden"] {
            assert!(store.add_prompt(name, "", "text", None).is_err(), "{name}");
        }
        assert!(store.add_prompt("x", "", "   \n", None).is_err());
        let huge = "x".repeat(MAX_PROMPT_BYTES + 1);
        assert!(
            store
                .add_prompt("x", "", &huge, None)
                .unwrap_err()
                .to_string()
                .contains("the limit is")
        );
        assert!(
            store
                .add_prompt("base", "", "text", None)
                .unwrap_err()
                .to_string()
                .contains("already a prompt named base")
        );
        assert_eq!(store.list_prompts().unwrap().len(), 1);
        assert!(
            store
                .prompt("nope")
                .unwrap_err()
                .to_string()
                .contains("no prompt named nope")
        );
        assert!(store.set_prompt_enabled("nope", true).is_err());
        assert!(store.delete_prompt("nope").is_err());
    }

    #[test]
    fn text_description_rename_and_source_are_updated() {
        let dir = Scratch::new("prompts-edit");
        let mut store = open(&dir);
        store.add_prompt("a", "", "one", Some("/x/a.md")).unwrap();
        store.set_prompt_text("a", "two", None).unwrap();
        let p = store.prompt("a").unwrap();
        assert_eq!(p.text, "two");
        assert_eq!(
            p.source_path.as_deref(),
            Some("/x/a.md"),
            "kept when not given"
        );
        store
            .set_prompt_text("a", "three", Some("/y/b.md"))
            .unwrap();
        assert_eq!(
            store.prompt("a").unwrap().source_path.as_deref(),
            Some("/y/b.md")
        );
        store.set_prompt_description("a", "about a").unwrap();
        store.rename_prompt("a", "renamed").unwrap();
        let p = store.prompt("renamed").unwrap();
        assert_eq!(
            (p.description.as_str(), p.text.as_str()),
            ("about a", "three")
        );
        assert!(store.rename_prompt("renamed", "base").is_err());
        assert!(store.rename_prompt("renamed", "on").is_err());
        assert!(store.set_prompt_text("renamed", "", None).is_err());
    }

    #[test]
    fn prompts_move_one_place_and_stop_at_the_ends() {
        let dir = Scratch::new("prompts-move");
        let mut store = open(&dir);
        store.add_prompt("a", "", "A", None).unwrap();
        store.add_prompt("b", "", "B", None).unwrap();
        let names = |s: &Store| -> Vec<String> {
            s.list_prompts()
                .unwrap()
                .into_iter()
                .map(|p| p.name)
                .collect()
        };
        assert_eq!(names(&store), ["base", "a", "b"]);
        assert!(store.move_prompt("b", -1).unwrap());
        assert_eq!(names(&store), ["base", "b", "a"]);
        assert!(store.move_prompt("base", 1).unwrap());
        assert_eq!(names(&store), ["b", "base", "a"]);
        assert!(!store.move_prompt("b", -1).unwrap(), "already first");
        assert!(!store.move_prompt("a", 1).unwrap(), "already last");
        assert!(store.move_prompt("nope", 1).is_err());
        let positions: Vec<i64> = store
            .list_prompts()
            .unwrap()
            .iter()
            .map(|p| p.position)
            .collect();
        assert_eq!(positions, [1, 2, 3]);
    }

    #[test]
    fn a_deleted_prompt_is_gone_and_the_rest_stay() {
        let dir = Scratch::new("prompts-delete");
        let mut store = open(&dir);
        store.add_prompt("a", "", "A", None).unwrap();
        store.delete_prompt("a").unwrap();
        assert!(store.prompt("a").is_err());
        assert!(store.prompt("base").is_ok());
        store.add_prompt("a", "", "A again", None).unwrap();
        assert_eq!(store.prompt("a").unwrap().position, 2);
    }

    #[test]
    fn a_version_four_database_gains_the_prompts_table_and_keeps_its_rows() {
        let dir = Scratch::new("prompts-migrate");
        let path = dir.path().join("harness.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(&format!(
                "{} {} {} {} PRAGMA user_version = 4;
                 INSERT INTO providers (name, base_url, model, created_at)
                     VALUES ('kept', 'http://k/v1', 'm', 1);",
                crate::store::SCHEMA_V1_FOR_TESTS,
                crate::store::SCHEMA_V2_FOR_TESTS,
                crate::store::SCHEMA_V3_FOR_TESTS,
                crate::store::SCHEMA_V4_FOR_TESTS,
            ))
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, crate::store::SCHEMA_VERSION);
        assert_eq!(store.list_providers().unwrap().len(), 1);
        assert_eq!(store.list_prompts().unwrap().len(), 1);
        drop(store);
        // Opening again seeds nothing more.
        let store = Store::open(&path).unwrap();
        assert_eq!(store.list_prompts().unwrap().len(), 1);
    }

    // ---- the base prompt lists the tools (issue 16) ----

    fn base_text_after_reopening(tag: &str, text: &str) -> String {
        base_text_after_reopening_at(tag, text, 7)
    }

    fn base_text_after_reopening_at(tag: &str, text: &str, version: i64) -> String {
        let dir = Scratch::new(tag);
        let path = dir.path().join("harness.db");
        drop(Store::open(&path).unwrap());
        {
            // Back to how an owner's database looked at an older schema.
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute("UPDATE prompts SET text = ?1 WHERE name = 'base'", [text])
                .unwrap();
            conn.pragma_update(None, "user_version", version).unwrap();
        }
        let store = Store::open(&path).unwrap();
        store.prompt(BASE_NAME).unwrap().text
    }

    #[test]
    fn an_unedited_base_gets_the_text_that_lists_the_tools() {
        let text = base_text_after_reopening("prompt-v8-unedited", PREVIOUS_BUILTIN_PROMPT);
        assert_eq!(text, BUILTIN_PROMPT);
        assert_ne!(PREVIOUS_BUILTIN_PROMPT, BUILTIN_PROMPT);
    }

    #[test]
    fn a_base_with_the_schema_8_text_gets_the_text_that_lists_edit_file() {
        let text = base_text_after_reopening_at("prompt-v9-unedited", BUILTIN_PROMPT_V8, 8);
        assert_eq!(text, BUILTIN_PROMPT);
        assert_ne!(BUILTIN_PROMPT_V8, BUILTIN_PROMPT);
        assert!(BUILTIN_PROMPT.contains("- edit_file("));
        // An old database goes through both steps and ends on the newest text.
        assert_eq!(
            base_text_after_reopening("prompt-v9-from-7", PREVIOUS_BUILTIN_PROMPT),
            BUILTIN_PROMPT
        );
        assert_eq!(
            base_text_after_reopening_at("prompt-v9-edited", "Mine.", 8),
            "Mine."
        );
    }

    #[test]
    fn a_base_with_the_schema_9_text_gets_the_rules_from_the_logs() {
        let text = base_text_after_reopening_at("prompt-v10-unedited", BUILTIN_PROMPT_V9, 9);
        assert_eq!(text, BUILTIN_PROMPT);
        assert_ne!(BUILTIN_PROMPT_V9, BUILTIN_PROMPT);
        assert_eq!(
            base_text_after_reopening_at("prompt-v10-edited", "Mine.", 9),
            "Mine."
        );
    }

    #[test]
    fn the_built_in_text_states_the_real_call_limit_and_stays_short() {
        assert!(BUILTIN_PROMPT.contains(&format!(
            "limited to {} model calls",
            crate::agent::MAX_CALLS_PER_MESSAGE
        )));
        // Every call carries it: keep it under about 600 tokens.
        assert!(estimated_tokens(BUILTIN_PROMPT.chars().count()) < 600);
    }

    #[test]
    fn an_edited_base_is_left_alone() {
        let text = base_text_after_reopening("prompt-v8-edited", "My own rules.");
        assert_eq!(text, "My own rules.");
    }

    #[test]
    fn the_built_in_text_names_every_tool_the_program_gives_the_model() {
        use crate::tools::{
            Answer, Approver, Ask, Bash, DEFAULT_TIMEOUT, Mode, ReadFile, Tool, file_tools,
            shared_mode,
        };
        struct Never;
        impl Approver for Never {
            fn approve(&self, _ask: &Ask) -> Answer {
                Answer::No
            }
        }
        let dir = Scratch::new("prompt-names-tools");
        let mut tools: Vec<Box<dyn Tool>> = vec![Box::new(ReadFile::new(dir.path()))];
        tools.extend(file_tools(
            dir.path(),
            shared_mode(Mode::Manual),
            std::rc::Rc::new(Never),
        ));
        tools.push(Box::new(Bash::new(
            dir.path(),
            DEFAULT_TIMEOUT,
            Box::new(Never),
        )));
        assert_eq!(tools.len(), 6);
        for tool in &tools {
            let name = &tool.spec().name;
            assert!(
                BUILTIN_PROMPT.contains(&format!("- {name}(")),
                "the built-in prompt does not list {name}"
            );
        }
    }

    // ---- types, chat state and the injected text (issue 14) ----

    fn typed(name: &str, text: &str, kind: PromptType, position: i64) -> Prompt {
        Prompt {
            prompt_type: kind,
            ..prompt(name, text, false, position)
        }
    }

    fn new_session(store: &mut Store) -> i64 {
        store
            .create_session(&crate::store::NewSession {
                kind: crate::store::Kind::Worker,
                model: "m",
                base_url: "http://x/v1",
                router: &kernel::RouterOptions::default(),
                system_prompt: "p",
                cwd: "/w",
            })
            .unwrap()
    }

    #[test]
    fn a_version_five_database_gains_types_and_chat_state_and_keeps_its_prompts() {
        let dir = Scratch::new("prompts-migrate-6");
        let path = dir.path().join("harness.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(&format!(
                "{} {} {} {} {} PRAGMA user_version = 5;
                 INSERT INTO prompts (name, description, text, enabled, position, created_at, updated_at)
                     VALUES ('kept', '', 'text', 1, 1, 1, 1);",
                crate::store::SCHEMA_V1_FOR_TESTS,
                crate::store::SCHEMA_V2_FOR_TESTS,
                crate::store::SCHEMA_V3_FOR_TESTS,
                crate::store::SCHEMA_V4_FOR_TESTS,
                crate::store::SCHEMA_V5_FOR_TESTS,
            ))
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let kept = store.prompt("kept").unwrap();
        assert_eq!(kept.prompt_type, PromptType::System);
        assert!(kept.enabled);
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM chat_prompt_state", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        drop(store);
        // Opening again changes nothing.
        let store = Store::open(&path).unwrap();
        assert_eq!(store.list_prompts().unwrap().len(), 1);
    }

    #[test]
    fn a_version_six_database_loses_the_defaults_of_skills_and_personalities() {
        let dir = Scratch::new("prompts-migrate-7");
        let path = dir.path().join("harness.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(&format!(
                "{} {} {} {} {} {} PRAGMA user_version = 6;
                 INSERT INTO prompts (name, description, text, enabled, position, created_at, updated_at, type)
                     VALUES ('sys', '', 't', 1, 1, 1, 1, 'system'),
                            ('skill', '', 't', 1, 2, 1, 1, 'skill'),
                            ('pers', '', 't', 1, 3, 1, 1, 'personality');",
                crate::store::SCHEMA_V1_FOR_TESTS,
                crate::store::SCHEMA_V2_FOR_TESTS,
                crate::store::SCHEMA_V3_FOR_TESTS,
                crate::store::SCHEMA_V4_FOR_TESTS,
                crate::store::SCHEMA_V5_FOR_TESTS,
                crate::store::SCHEMA_V6_FOR_TESTS,
            ))
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert!(store.prompt("sys").unwrap().enabled);
        assert!(!store.prompt("skill").unwrap().enabled);
        assert!(!store.prompt("pers").unwrap().enabled);
    }

    #[test]
    fn only_system_prompts_go_into_the_system_prompt() {
        let all = [
            typed("rules", "rules text", PromptType::System, 1),
            typed("tdd", "skill text", PromptType::Skill, 2),
            typed("pirate", "arr", PromptType::Personality, 3),
        ]
        .map(|mut p| {
            p.enabled = true;
            p
        });
        let assembled = assemble(&all);
        assert_eq!(assembled.text, "rules text");
        assert_eq!(assembled.names, ["rules"]);
    }

    #[test]
    fn a_prompt_is_added_with_its_type_and_only_system_prompts_are_switched() {
        let dir = Scratch::new("prompts-type");
        let mut store = open(&dir);
        store
            .add_prompt_of_type("tdd", "", "tests first", None, PromptType::Skill)
            .unwrap();
        store
            .add_prompt_of_type("pirate", "", "arr", None, PromptType::Personality)
            .unwrap();
        assert_eq!(store.prompt("tdd").unwrap().prompt_type, PromptType::Skill);
        assert!(!store.prompt("tdd").unwrap().enabled);
        // A plain add is a system prompt, and only those switch on and off.
        store.add_prompt("rules", "", "r", None).unwrap();
        assert_eq!(
            store.prompt("rules").unwrap().prompt_type,
            PromptType::System
        );
        store.set_prompt_enabled("rules", true).unwrap();
        for name in ["tdd", "pirate"] {
            let why = store
                .set_prompt_enabled(name, true)
                .unwrap_err()
                .to_string();
            assert!(why.contains("chosen in a chat"), "{why}");
            assert!(!store.prompt(name).unwrap().enabled);
        }
        // Names are shared by every kind.
        assert!(
            store
                .add_prompt_of_type("tdd", "", "x", None, PromptType::Personality)
                .is_err()
        );
    }

    #[test]
    fn the_state_of_a_chat_is_saved_and_read_back_without_the_one_message_skills() {
        let dir = Scratch::new("prompts-chat-state");
        let mut store = open(&dir);
        store
            .add_prompt_of_type("tdd", "", "t", None, PromptType::Skill)
            .unwrap();
        store
            .add_prompt_of_type("pirate", "", "t", None, PromptType::Personality)
            .unwrap();
        let id = new_session(&mut store);
        let state = ChatPrompts {
            personality: Some("pirate".into()),
            on: vec!["tdd".into(), "ghost".into()],
            once: vec!["tdd".into()],
        };
        store.set_chat_prompts(id, &state).unwrap();
        let back = store.chat_prompts(id).unwrap();
        assert_eq!(back.personality.as_deref(), Some("pirate"));
        // A name with no prompt is left out; the one-message state is not saved.
        assert_eq!(back.on, ["tdd"]);
        assert!(back.once.is_empty());
        // Saving again replaces.
        store.set_chat_prompts(id, &ChatPrompts::default()).unwrap();
        assert!(store.chat_prompts(id).unwrap().is_empty());
    }

    #[test]
    fn deleting_a_prompt_or_a_chat_cleans_up_the_saved_state() {
        let dir = Scratch::new("prompts-state-cleanup");
        let mut store = open(&dir);
        store
            .add_prompt_of_type("tdd", "", "t", None, PromptType::Skill)
            .unwrap();
        store
            .add_prompt_of_type("pirate", "", "t", None, PromptType::Personality)
            .unwrap();
        let id = new_session(&mut store);
        let state = ChatPrompts {
            personality: Some("pirate".into()),
            on: vec!["tdd".into()],
            once: vec![],
        };
        store.set_chat_prompts(id, &state).unwrap();
        let count = |store: &Store| {
            store
                .conn
                .query_row("SELECT COUNT(*) FROM chat_prompt_state", [], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap()
        };
        store.delete_prompt("tdd").unwrap();
        assert_eq!(count(&store), 1);
        store.delete_session(id).unwrap();
        assert_eq!(count(&store), 0);
    }

    #[test]
    fn the_kept_text_has_the_personality_first_then_the_skills_that_are_on_by_position() {
        let all = [
            typed("late", "late skill\n", PromptType::Skill, 5),
            typed("early", "  early skill ", PromptType::Skill, 2),
            typed("off", "never", PromptType::Skill, 3),
            typed("pirate", "talk like a pirate", PromptType::Personality, 4),
            typed("rules", "not a skill", PromptType::System, 1),
        ];
        let state = ChatPrompts {
            personality: Some("pirate".into()),
            on: vec!["late".into(), "early".into(), "rules".into()],
            once: vec!["off".into(), "gone".into()],
        };
        let text = compose_persistent(&all, &state).unwrap();
        assert!(text.starts_with(ACTIVE_MARK));
        assert!(text.contains("stay in force until a newer one"));
        let pirate = text
            .find("## Personality: pirate\ntalk like a pirate")
            .unwrap();
        let early = text.find("## Skill: early\nearly skill").unwrap();
        let late = text.find("## Skill: late\nlate skill").unwrap();
        assert!(pirate < early && early < late);
        // The skill for this message only and the system prompt are not in it.
        assert!(!text.contains("never") && !text.contains("not a skill"));
        // The same input gives the same bytes.
        assert_eq!(compose_persistent(&all, &state).unwrap(), text);
        // Nothing on, or only names that match nothing: no text.
        assert_eq!(compose_persistent(&all, &ChatPrompts::default()), None);
        let only_gone = ChatPrompts {
            on: vec!["gone".into(), "rules".into()],
            ..ChatPrompts::default()
        };
        assert_eq!(compose_persistent(&all, &only_gone), None);
    }

    #[test]
    fn the_text_for_one_message_holds_only_those_skills() {
        let all = [
            typed("a", "skill a", PromptType::Skill, 1),
            typed("b", "skill b", PromptType::Skill, 2),
            typed("pirate", "arr", PromptType::Personality, 3),
        ];
        let state = ChatPrompts {
            personality: Some("pirate".into()),
            on: vec!["a".into()],
            once: vec!["b".into()],
        };
        let text = compose_once(&all, &state).unwrap();
        assert!(text.starts_with("Active instructions for this message only"));
        assert!(text.contains("## Skill: b\nskill b"));
        assert!(!text.contains("skill a") && !text.contains("arr"));
        assert!(!text.starts_with(ACTIVE_MARK));
        assert_eq!(compose_once(&all, &ChatPrompts::default()), None);
    }

    #[test]
    fn the_chat_state_reads_for_the_screen() {
        let state = ChatPrompts {
            personality: Some("pirate".into()),
            on: vec!["tdd".into()],
            once: vec!["review".into()],
        };
        assert_eq!(state.skill_state("tdd"), SkillState::On);
        assert_eq!(state.skill_state("review"), SkillState::Once);
        assert_eq!(state.skill_state("x"), SkillState::Off);
        assert_eq!(
            state.summary(),
            "personality pirate; skills on: tdd; this message: review"
        );
        assert_eq!(ChatPrompts::default().summary(), "");
    }

    #[test]
    fn the_new_command_words_are_not_usable_as_names() {
        for word in ["none", "use", "type"] {
            assert!(check_prompt_name(word).is_err(), "{word}");
        }
    }

    #[test]
    fn the_short_hash_tells_texts_apart() {
        assert_eq!(short_hash("a").len(), 12);
        assert_ne!(short_hash("a"), short_hash("b"));
        assert_eq!(short_hash("a"), short_hash("a"));
    }
}

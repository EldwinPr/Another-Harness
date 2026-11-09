//! The agent loop for the normal session harness (decision 07).
//!
//! The conversation is a plain list of messages that only ever grows. For each
//! thing you type, the loop sends the whole list, runs any tools the model asks
//! for, appends their results, and sends again, until the model answers in text
//! or the call cap is hit. Because history is never rewritten, each request
//! starts with exactly the messages of the one before it.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use kernel::{Config, Message, RouterOptions, ToolCall, assemble};

use crate::cancel::Cancel;
use crate::loop_guard::{self, LoopGuard, Verdict};
use crate::provider::{CHOOSE_A_PROVIDER, Provider, ProviderError, Reply, ServedBy};
use crate::providers::{self, Catalog, Listed, ProviderDef, Source};
use crate::store::{
    APPROVE_MODE, AUTO_COMPACT, Kind, LAST_PROVIDER, NewSession, Resumed, SaveCall, SessionSummary,
    Store,
};
use crate::store_models::MergeReport;
use crate::store_prompts::{
    ACTIVE_MARK, Assembled, ChatPrompts, NONE_ACTIVE, PROMPT_WRITER_GUIDE, Prompt, PromptType,
    SUMMARY_MARK, SkillState, assemble as assemble_prompts, compose_once, compose_persistent,
    estimated_tokens, size_label, summary_message,
};
use crate::tools::{Mode, SharedMode, Tool, shared_mode};
use crate::turn_log::{TurnLog, utc_label};
use crate::usage::{ChatUsage, Tokens};

/// The system prompt. A constant on purpose: no dates, paths or other volatile
/// data, so every run starts with the same bytes (decision 01).
pub const SYSTEM_PROMPT: &str = crate::store_prompts::BUILTIN_PROMPT;

/// What the system prompt of a chat is made of, for the banner and for `/new`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptInfo {
    /// The prompts that went into it, in order; empty when the built-in text stood in.
    pub names: Vec<String>,
    pub chars: usize,
    pub fallback: bool,
}

impl PromptInfo {
    fn of(assembled: &Assembled) -> Self {
        Self {
            names: assembled.names.clone(),
            chars: assembled.chars(),
            fallback: assembled.fallback,
        }
    }

    /// One line: what is in it and how big it is (the token count is an estimate).
    pub fn summary(&self) -> String {
        let size = format!(
            "{}, about {} tokens (estimate)",
            size_label(self.chars),
            estimated_tokens(self.chars)
        );
        if self.fallback {
            format!("the built-in text, no prompt is switched on; {size}")
        } else {
            format!(
                "{} on ({}); {size}",
                self.names.len(),
                self.names.join(", ")
            )
        }
    }
}

/// Removes one pair of code fences around a whole answer, which models add although told not to.
fn strip_fence(text: &str) -> String {
    let Some(inner) = text.strip_prefix("```").and_then(|t| t.strip_suffix("```")) else {
        return text.to_owned();
    };
    // Drop the language tag on the opening line.
    let inner = inner.split_once('\n').map_or(inner, |(_, rest)| rest);
    inner.trim().to_owned()
}

/// The answer when a chat command needs the database and none is attached.
const NOT_SAVING: &str = "chats are not being saved, so there is nothing to resume";

/// The answer when the program was started without a place to find providers.
const NO_SWITCHING: &str = "this run cannot switch providers";

/// The warning that goes with every switch.
const COLD_CACHE: &str =
    "the next request differs from the earlier ones, so the router's cache starts over";

/// Most calls to the model allowed for one thing the user types.
pub const MAX_CALLS_PER_MESSAGE: u32 = 20;

/// The prompt-token limit for automatic compaction until the owner sets another (issue 24).
pub const DEFAULT_AUTO_COMPACT_AT: u64 = 80_000;
/// The lowest limit that can be set: below this a chat would be compacted almost at once.
pub const MIN_AUTO_COMPACT_AT: u64 = 10_000;

/// The last message of the request that asks for a summary.
const COMPACT_ASK: &str = "Write a summary of this conversation so far. It will be the only thing a new chat starts with, so another assistant must be able to carry on from it. Keep: what the user wants, what they said to do or not to do, decisions and the reasons for them, the files created or changed and what state they are in, what is done and what is left. Leave out tool output and file text that can be read again. Do not call any tools. Write plain text, under 600 words.";

/// What a compaction did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactReport {
    /// Messages of the chat that was summarised (not counting the system prompt).
    pub messages: usize,
    pub summary_chars: usize,
}

/// How one user message ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The model answered in text.
    Answer(String),
    /// The model kept asking for tools until the cap.
    CapReached { calls: u32 },
    /// The call failed, or the chat could not be saved. The message that
    /// caused it has been removed from the in-memory history.
    Failed(String),
    /// The model kept making failing or repeated calls (see [`crate::loop_guard`]) and the
    /// program ended the message. What was done is kept, like at the call cap.
    LoopStopped { calls: u32 },
    /// The user cancelled while it worked. Handled like a failure: the message has been removed
    /// from the history and its turn is marked failed in the database.
    Cancelled,
}

/// Told about things as they happen, so the REPL can print them.
pub trait Observer {
    fn tool_call(&mut self, _call: &ToolCall) {}
    /// A tool has run; `result` is the text the model gets back.
    fn tool_result(&mut self, _call: &ToolCall, _result: &str) {}
    /// Something went wrong that did not stop the chat (the turn log could not be written).
    fn warning(&mut self, _text: &str) {}
}

/// An observer that ignores everything.
pub struct NoObserver;
impl Observer for NoObserver {}

/// What the saved chat records about where it ran. The model, the router and its options come
/// from the agent itself.
pub struct StoreContext {
    pub cwd: String,
}

/// Which provider the next request goes to. `name` is the profile or saved provider it came
/// from (the router's host when launched without one).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Target {
    pub name: String,
    pub base_url: String,
}

/// What [`Agent::resume`] did, for the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeReport {
    pub id: i64,
    pub updated_at: i64,
    pub message_count: usize,
    /// How the saved chat's setup differs from the current one (model, router, directory).
    pub differences: Vec<String>,
    /// The saved chat's last turn never finished, so it was left out.
    pub dropped_unfinished_turn: bool,
}

impl ResumeReport {
    /// One line saying which chat was loaded.
    pub fn headline(&self) -> String {
        format!(
            "resumed chat {}, last used {} ({} messages)",
            self.id,
            utc_label(u64::try_from(self.updated_at).unwrap_or(0)),
            self.message_count
        )
    }

    /// Warnings and notes, one per line with a trailing newline. Empty when there are none.
    pub fn notes(&self) -> String {
        let mut text = String::new();
        for difference in &self.differences {
            text.push_str(&format!("warning: {difference}\n"));
        }
        if !self.differences.is_empty() {
            text.push_str(
                "         (a different setup means the router's cache starts over and answers may change)\n",
            );
        }
        if self.dropped_unfinished_turn {
            text.push_str(
                "note: the last message of that chat never finished, so it was left out\n",
            );
        }
        text
    }
}

/// Where a chat is saved, once a store is attached ([`Agent::with_store`]).
struct Stored {
    store: Store,
    /// `None` until the first call is saved: a chat nobody typed into leaves no trace.
    session_id: Option<i64>,
    context: StoreContext,
    /// How many history messages are saved (the system message counts as saved, since it
    /// belongs to the session). Everything after this index is waiting for the next save.
    saved: usize,
    /// The number of the current (or last) user message, counting from 1.
    turn: u32,
}

pub struct Agent {
    config: Config,
    history: Vec<Message>,
    provider: Box<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    log: TurnLog,
    stored: Option<Stored>,
    max_calls: u32,
    target: Target,
    catalog: Option<Catalog>,
    /// The skills and personality of the chat in use (issue 14).
    chat_prompts: ChatPrompts,
    /// How the file tools are approved; shared with those tools (issue 16).
    approval: SharedMode,
    /// What the chat in use has cost so far (issue 17).
    usage: ChatUsage,
    cancel: Cancel,
}

/// Said when the personality changes in a chat that already has replies.
pub const PERSONALITY_WARNING: &str = "changing the personality in the middle of a chat can be troublesome: earlier replies keep the old style and the model may keep following them. /new starts a clean chat.";

impl Agent {
    pub fn new(
        model: impl Into<String>,
        provider: Box<dyn Provider>,
        tools: Vec<Box<dyn Tool>>,
        log: TurnLog,
    ) -> Self {
        let config = Config {
            model: model.into(),
            tools: tools.iter().map(|t| t.spec().clone()).collect(),
            ..Config::default()
        };
        Self {
            config,
            history: vec![Message::System(SYSTEM_PROMPT.to_owned())],
            provider,
            tools,
            log,
            stored: None,
            max_calls: MAX_CALLS_PER_MESSAGE,
            target: Target::default(),
            catalog: None,
            chat_prompts: ChatPrompts::default(),
            approval: shared_mode(Mode::Manual),
            usage: ChatUsage::default(),
            cancel: Cancel::new(),
        }
    }

    /// What the chat in use has cost so far, from the replies' `usage`.
    pub fn usage(&self) -> &ChatUsage {
        &self.usage
    }

    /// Uses this shared mode, the one the file tools were built with.
    pub fn with_approval(mut self, approval: SharedMode) -> Self {
        self.approval = approval;
        self
    }

    /// The "always" permissions given in this run, as labels.
    pub fn always_labels(&self) -> Vec<String> {
        self.approval.always_labels()
    }

    /// Forgets the "always" permissions; returns how many there were.
    pub fn forget_always(&self) -> usize {
        self.approval.forget_always()
    }

    pub fn approval_mode(&self) -> Mode {
        self.approval.mode()
    }

    /// Switches the approval mode from the next tool call on and remembers it for later runs.
    /// The mode is not part of any chat. Without a database it lasts for this run only.
    pub fn set_approval_mode(&mut self, mode: Mode) -> Result<(), String> {
        self.approval.set_mode(mode);
        match &mut self.stored {
            Some(stored) => stored
                .store
                .set_setting(APPROVE_MODE, mode.name())
                .map_err(|e| {
                    format!(
                        "the mode is {} for this run but could not be saved: {e}",
                        mode.name()
                    )
                }),
            None => Ok(()),
        }
    }

    /// At start-up: the saved mode, or `manual` when none is saved or the saved text is unknown.
    pub fn load_approval_mode(&mut self) {
        let saved = self
            .stored
            .as_ref()
            .and_then(|s| s.store.setting(APPROVE_MODE).ok().flatten())
            .and_then(|text| Mode::parse(&text));
        self.approval.set_mode(saved.unwrap_or(Mode::Manual));
    }

    /// Says which provider the first request goes to. The base URL is saved with the chat and
    /// compared on resume.
    pub fn with_target(mut self, name: impl Into<String>, base_url: impl Into<String>) -> Self {
        self.target = Target {
            name: name.into(),
            base_url: base_url.into(),
        };
        self
    }

    /// Lets the chat switch provider and add providers (`/provider`).
    pub fn with_catalog(mut self, catalog: Catalog) -> Self {
        self.catalog = Some(catalog);
        self
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }

    /// Whether a provider has been chosen. The chat starts without one the first time; nothing
    /// is sent until there is one.
    pub fn ready(&self) -> bool {
        !self.target.base_url.is_empty()
    }

    pub fn target(&self) -> &Target {
        &self.target
    }

    /// The pinning options in use.
    pub fn router(&self) -> &RouterOptions {
        &self.config.router
    }

    /// Saves the chat in SQLite as it goes (decision 08). The database is the truth: a
    /// message whose save fails fails. The turn log then becomes a secondary record, and a
    /// failure to write it is only a warning.
    pub fn with_store(mut self, store: Store, context: StoreContext) -> Self {
        self.stored = Some(Stored {
            store,
            session_id: None,
            context,
            saved: self.history.len(),
            turn: 0,
        });
        self
    }

    /// Continues a saved chat: its messages become the history, behind the system prompt it
    /// was saved with (not the current one, so the request bytes stay what they were). Call
    /// after [`Agent::with_store`].
    pub fn restore(&mut self, resumed: Resumed) {
        let resumed_id = resumed.info.id;
        if let Some(stored) = &self.stored {
            self.chat_prompts = stored
                .store
                .chat_prompts(resumed.info.id)
                .unwrap_or_default();
        }
        self.history = std::iter::once(Message::System(resumed.info.system_prompt))
            .chain(resumed.messages)
            .collect();
        if let Some(stored) = &mut self.stored {
            stored.session_id = Some(resumed.info.id);
            stored.saved = self.history.len();
            stored.turn = resumed.last_turn;
        }
        self.usage = self
            .stored
            .as_ref()
            .and_then(|s| s.store.chat_usage(resumed_id).ok())
            .unwrap_or_default();
    }

    /// The router options in use, as the text the store compares against a saved chat.
    pub fn router_json(&self) -> String {
        crate::store::router_json(&self.config.router)
    }

    /// What a chat started now would get as its system prompt, without changing anything.
    fn fresh_prompt(&self) -> Result<Assembled, String> {
        match &self.stored {
            Some(stored) => stored.store.assembled_prompt().map_err(|e| e.to_string()),
            None => Ok(assemble_prompts(&[])),
        }
    }

    /// Sets the system prompt from the prompts switched on. Only before the first message of a
    /// chat: a chat in progress keeps the prompt it started with (use [`Agent::new_chat`]).
    pub fn load_prompt(&mut self) -> Result<PromptInfo, String> {
        if self.current_session().is_some() || self.history.len() > 1 {
            return Err(
                "a chat keeps the prompt it started with; /new starts a new chat".to_owned(),
            );
        }
        let assembled = self.fresh_prompt()?;
        self.history[0] = Message::System(assembled.text.clone());
        self.chat_prompts = ChatPrompts::default();
        Ok(PromptInfo::of(&assembled))
    }

    /// Starts a new chat: the history is dropped (the old chat stays saved) and the system prompt
    /// is assembled again from the prompts switched on now. The next message starts a new saved chat.
    pub fn new_chat(&mut self) -> Result<PromptInfo, String> {
        let assembled = self.fresh_prompt()?;
        self.history = vec![Message::System(assembled.text.clone())];
        self.chat_prompts = ChatPrompts::default();
        self.usage = ChatUsage::default();
        if let Some(stored) = &mut self.stored {
            stored.session_id = None;
            stored.saved = 1;
            stored.turn = 0;
        }
        Ok(PromptInfo::of(&assembled))
    }

    /// The skills and personality of the chat in use.
    pub fn chat_prompts(&self) -> &ChatPrompts {
        &self.chat_prompts
    }

    /// Every skill, with what it is doing in this chat.
    pub fn skills(&self) -> Result<Vec<(Prompt, SkillState)>, String> {
        let stored = self.stored.as_ref().ok_or(NOT_SAVING)?;
        Ok(stored
            .store
            .list_prompts()
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|p| p.prompt_type == PromptType::Skill)
            .map(|p| {
                let state = self.chat_prompts.skill_state(&p.name);
                (p, state)
            })
            .collect())
    }

    /// Switches a skill off, on for the next message only, or on, in this chat.
    pub fn set_skill(&mut self, name: &str, state: SkillState) -> Result<(), String> {
        let stored = self.stored.as_ref().ok_or(NOT_SAVING)?;
        let prompt = stored.store.prompt(name).map_err(|e| e.to_string())?;
        if prompt.prompt_type != PromptType::Skill {
            return Err(format!(
                "{name} is a {} prompt, not a skill (/prompts type {name} skill changes that)",
                prompt.prompt_type.as_str()
            ));
        }
        self.chat_prompts.on.retain(|n| n != name);
        self.chat_prompts.once.retain(|n| n != name);
        match state {
            SkillState::Off => {}
            SkillState::Once => self.chat_prompts.once.push(name.to_owned()),
            SkillState::On => self.chat_prompts.on.push(name.to_owned()),
        }
        self.persist_chat_prompts()
    }

    /// Every personality.
    pub fn personalities(&self) -> Result<Vec<Prompt>, String> {
        let stored = self.stored.as_ref().ok_or(NOT_SAVING)?;
        Ok(stored
            .store
            .list_prompts()
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|p| p.prompt_type == PromptType::Personality)
            .collect())
    }

    /// Whether the chat in use already has a reply from the model.
    pub fn has_reply(&self) -> bool {
        self.history
            .iter()
            .any(|m| matches!(m, Message::Assistant { .. }))
    }

    /// Selects the personality of this chat, or none. Returns the warning to show when the
    /// personality changes in a chat that already has a reply.
    pub fn set_personality(&mut self, name: Option<&str>) -> Result<Option<&'static str>, String> {
        if let Some(name) = name {
            let stored = self.stored.as_ref().ok_or(NOT_SAVING)?;
            let prompt = stored.store.prompt(name).map_err(|e| e.to_string())?;
            if prompt.prompt_type != PromptType::Personality {
                return Err(format!(
                    "{name} is a {} prompt, not a personality (/prompts type {name} personality changes that)",
                    prompt.prompt_type.as_str()
                ));
            }
        }
        let changed = self.chat_prompts.personality.as_deref() != name;
        let warn = changed && self.has_reply();
        self.chat_prompts.personality = name.map(str::to_owned);
        self.persist_chat_prompts()?;
        Ok(warn.then_some(PERSONALITY_WARNING))
    }

    /// Brings the skills and personality of this chat in line with the prompts after one was
    /// renamed, deleted or given another type: a saved chat reads its state again (it follows
    /// the prompt by id, so a rename keeps it); names that match no prompt of the right type are
    /// dropped.
    pub fn refresh_chat_prompts(&mut self) {
        let Some(stored) = &self.stored else {
            return;
        };
        let Ok(prompts) = stored.store.list_prompts() else {
            return;
        };
        let mut next = stored
            .session_id
            .and_then(|id| stored.store.chat_prompts(id).ok())
            .unwrap_or_else(|| self.chat_prompts.clone());
        next.once = self.chat_prompts.once.clone();
        let exists = |name: &str, kind: PromptType| {
            prompts
                .iter()
                .any(|p| p.name == name && p.prompt_type == kind)
        };
        if next
            .personality
            .as_deref()
            .is_some_and(|n| !exists(n, PromptType::Personality))
        {
            next.personality = None;
        }
        next.on.retain(|n| exists(n, PromptType::Skill));
        next.once.retain(|n| exists(n, PromptType::Skill));
        self.chat_prompts = next;
    }

    /// Saves the skills and personality of the chat once the chat is saved (a chat nobody typed
    /// into is not saved; the state goes in with its first call).
    fn persist_chat_prompts(&mut self) -> Result<(), String> {
        if let Some(stored) = &mut self.stored
            && let Some(id) = stored.session_id
        {
            stored
                .store
                .set_chat_prompts(id, &self.chat_prompts)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// What the skills and the personality add to the next message: the text that is kept in the
    /// chat (personality and the skills that are on) and the text for this message only.
    pub fn injections(&self) -> Result<(Option<String>, Option<String>), String> {
        let Some(stored) = &self.stored else {
            return Ok((None, None));
        };
        // A chat that never had any needs no look at the prompts.
        if self.chat_prompts.is_empty() && self.current_block().is_none() {
            return Ok((None, None));
        }
        let prompts = stored.store.list_prompts().map_err(|e| e.to_string())?;
        Ok((
            compose_persistent(&prompts, &self.chat_prompts),
            compose_once(&prompts, &self.chat_prompts),
        ))
    }

    /// The newest "active instructions" message in the chat.
    fn current_block(&self) -> Option<&str> {
        self.history.iter().rev().find_map(|m| match m {
            Message::User(text) if text.starts_with(ACTIVE_MARK) => Some(text.as_str()),
            _ => None,
        })
    }

    /// The message to add before the user's, if the wanted instructions differ from the newest
    /// block in the chat.
    fn block_to_add(&self, wanted: Option<&str>) -> Option<String> {
        match (wanted, self.current_block()) {
            (Some(wanted), Some(current)) if wanted == current => None,
            (Some(wanted), _) => Some(wanted.to_owned()),
            (None, Some(current)) if current != NONE_ACTIVE => Some(NONE_ACTIVE.to_owned()),
            (None, _) => None,
        }
    }

    /// What a chat started now would get as its system prompt (nothing is changed).
    pub fn assembled_prompt(&self) -> Result<Assembled, String> {
        self.fresh_prompt()
    }

    /// The same, as a summary for the screen.
    pub fn prompt_preview(&self) -> Result<PromptInfo, String> {
        self.fresh_prompt()
            .map(|assembled| PromptInfo::of(&assembled))
    }

    /// How long the system prompt in use is, in characters.
    pub fn prompt_chars(&self) -> usize {
        match self.history.first() {
            Some(Message::System(text)) => text.chars().count(),
            _ => 0,
        }
    }

    /// Asks the model in use to rewrite a draft prompt, with the built-in prompt-writing guide as
    /// the system part. One request; nothing is saved or logged. Returns the improved text.
    pub fn improve_prompt(&self, draft: &str, note: &str) -> Result<String, String> {
        if !self.ready() {
            return Err(CHOOSE_A_PROVIDER.to_owned());
        }
        let config = Config {
            model: self.config.model.clone(),
            router: self.config.router.clone(),
            ..Config::default()
        };
        let note = note.trim();
        let ask = if note.is_empty() {
            format!("Draft prompt:\n\n{draft}")
        } else {
            format!("Draft prompt:\n\n{draft}\n\nNote: {note}")
        };
        let history = [
            Message::System(PROMPT_WRITER_GUIDE.to_owned()),
            Message::User(ask),
        ];
        let reply = self
            .provider
            .complete(&assemble(&config, &history))
            .map_err(|error| describe(&error))?;
        let said = match &reply.message {
            Message::Assistant { content, .. } => content.clone().unwrap_or_default(),
            _ => String::new(),
        };
        let improved = strip_fence(said.trim());
        if improved.is_empty() {
            return Err("the model answered with nothing".to_owned());
        }
        Ok(improved)
    }

    /// The prompt-token limit for automatic compaction: `None` when it is off.
    pub fn auto_compact_at(&self) -> Option<u64> {
        let saved = self
            .stored
            .as_ref()
            .and_then(|s| s.store.setting(AUTO_COMPACT).ok().flatten());
        match saved.as_deref() {
            Some("off") => None,
            Some(text) => Some(
                text.parse::<u64>()
                    .ok()
                    .filter(|n| *n >= MIN_AUTO_COMPACT_AT)
                    .unwrap_or(DEFAULT_AUTO_COMPACT_AT),
            ),
            None => Some(DEFAULT_AUTO_COMPACT_AT),
        }
    }

    /// Sets the limit (`None` turns it off) and remembers it for later runs.
    pub fn set_auto_compact(&mut self, at: Option<u64>) -> Result<(), String> {
        if let Some(n) = at
            && n < MIN_AUTO_COMPACT_AT
        {
            return Err(format!(
                "the limit must be at least {MIN_AUTO_COMPACT_AT} tokens"
            ));
        }
        let stored = self.stored.as_mut().ok_or(NOT_SAVING)?;
        let value = at.map_or("off".to_owned(), |n| n.to_string());
        stored
            .store
            .set_setting(AUTO_COMPACT, &value)
            .map_err(|e| e.to_string())
    }

    /// Whether the newest call's prompt reached the automatic limit.
    pub fn needs_compacting(&self) -> bool {
        let prompt = self.usage.last.and_then(|t| t.prompt);
        match (self.auto_compact_at(), prompt) {
            (Some(limit), Some(prompt)) => prompt >= limit && self.has_messages(),
            _ => false,
        }
    }

    fn has_messages(&self) -> bool {
        self.history
            .iter()
            .any(|m| matches!(m, Message::User(t) if !t.starts_with(ACTIVE_MARK) && !t.starts_with(SUMMARY_MARK)))
    }

    /// Asks the model for a summary of the chat and starts a new chat that begins with it. The
    /// chat left behind stays saved. The skills and personality of the chat carry over. On any
    /// failure, a cancel included, nothing changes.
    pub fn compact(&mut self) -> Result<CompactReport, String> {
        if !self.ready() {
            return Err(CHOOSE_A_PROVIDER.to_owned());
        }
        if !self.has_messages() {
            return Err("there is nothing to compact yet".to_owned());
        }
        self.cancel.reset();
        let shortened = crate::trim::trimmed(&self.history);
        let sent: &[Message] = shortened.as_deref().unwrap_or(&self.history);
        let mut messages = sent.to_vec();
        messages.push(Message::User(COMPACT_ASK.to_owned()));
        let request = assemble(&self.config, &messages);
        let result = self.provider.complete_cancellable(&request, &self.cancel);
        let _ = self.log.record_timed(&request, &result, None);
        if self.cancel.is_cancelled() {
            return Err("cancelled".to_owned());
        }
        let reply = result.map_err(|error| describe(&error))?;
        let summary = match &reply.message {
            Message::Assistant {
                content,
                tool_calls,
            } if tool_calls.is_empty() => content.clone().unwrap_or_default(),
            Message::Assistant { .. } => {
                return Err("the model asked for a tool instead of writing a summary".to_owned());
            }
            _ => return Err("the reply was not an assistant message".to_owned()),
        };
        let summary = summary.trim();
        if summary.is_empty() {
            return Err("the model answered with nothing".to_owned());
        }
        let before = self.history.len() - 1;
        let earlier = self.current_session();
        let kept = self.chat_prompts.clone();
        self.new_chat()?;
        self.chat_prompts = kept;
        self.history
            .push(Message::User(summary_message(earlier, summary)));
        Ok(CompactReport {
            messages: before,
            summary_chars: summary.chars().count(),
        })
    }

    /// The id of the chat being saved into, once its first message has been saved.
    pub fn current_session(&self) -> Option<i64> {
        self.stored.as_ref().and_then(|s| s.session_id)
    }

    /// The most recent saved chats, newest first.
    pub fn sessions(&self, limit: usize) -> Result<Vec<SessionSummary>, String> {
        let stored = self.stored.as_ref().ok_or(NOT_SAVING)?;
        stored
            .store
            .list_sessions(Kind::Worker, limit)
            .map_err(|e| e.to_string())
    }

    /// The saved chats that began from a summary of chat `id` (empty when none is known).
    pub fn chats_continuing(&self, id: i64) -> Result<Vec<i64>, String> {
        let stored = self.stored.as_ref().ok_or(NOT_SAVING)?;
        stored.store.chats_continuing(id).map_err(|e| e.to_string())
    }

    /// Deletes a saved chat. The chat being saved into right now is refused: its next message
    /// would have nowhere to go.
    pub fn delete_session(&mut self, id: i64) -> Result<(), String> {
        if self.current_session() == Some(id) {
            return Err("that chat is the one in use; resume another chat first".to_owned());
        }
        let stored = self.stored.as_mut().ok_or(NOT_SAVING)?;
        stored.store.delete_session(id).map_err(|e| e.to_string())
    }

    /// Switches to a saved chat: the latest one, or the one with this id. The chat being left
    /// stays saved. The model and router stay as they are now (the report says how the saved
    /// chat differed), but the system prompt is the saved chat's own, so its bytes do not change.
    pub fn resume(&mut self, id: Option<i64>) -> Result<ResumeReport, String> {
        let stored = self.stored.as_mut().ok_or(NOT_SAVING)?;
        let id = match id {
            Some(id) => id,
            None => {
                stored
                    .store
                    .latest_session(Kind::Worker)
                    .map_err(|e| e.to_string())?
                    .ok_or("there is no saved chat to resume")?
                    .id
            }
        };
        let resumed = stored.store.load(id).map_err(|e| e.to_string())?;
        let ready = !self.target.base_url.is_empty();
        let prompt_now = stored
            .store
            .assembled_prompt()
            .map(|assembled| assembled.text)
            .ok();
        let mut differences = if ready {
            resumed.info.differences(
                &self.config.model,
                &self.target.base_url,
                &crate::store::router_json(&self.config.router),
            )
        } else {
            vec![format!(
                "no provider is selected (that chat used {} at {}): choose one with /provider",
                resumed.info.model, resumed.info.base_url
            )]
        };
        if prompt_now.is_some_and(|now| now != resumed.info.system_prompt) {
            differences.push(
                "the chat keeps its own system prompt, which differs from the prompts switched on now"
                    .to_owned(),
            );
        }
        if resumed.info.cwd != stored.context.cwd {
            differences.push(format!(
                "working directory was {}, now {}",
                resumed.info.cwd, stored.context.cwd
            ));
        }
        let report = ResumeReport {
            id: resumed.info.id,
            updated_at: resumed.info.updated_at,
            message_count: resumed.messages.len(),
            differences,
            dropped_unfinished_turn: resumed.dropped_unfinished_turn,
        };
        self.restore(resumed);
        Ok(report)
    }

    /// Sets the router options (provider pinning, session key) sent with every
    /// request. They stay the same for the whole run, so the bytes stay stable.
    pub fn with_router(mut self, router: RouterOptions) -> Self {
        self.config.router = router;
        self
    }

    /// Overrides the call cap (tests use a small one).
    /// Uses this flag to cancel a message in progress (see [`Cancel`]); share it with the tools
    /// that wait, such as `bash`.
    pub fn with_cancel(mut self, cancel: Cancel) -> Self {
        self.cancel = cancel;
        self
    }

    /// The flag a cancel sets.
    pub fn cancel_handle(&self) -> Cancel {
        self.cancel.clone()
    }

    pub fn with_max_calls(mut self, max_calls: u32) -> Self {
        self.max_calls = max_calls;
        self
    }

    pub fn history(&self) -> &[Message] {
        &self.history
    }

    pub fn log_path(&self) -> &Path {
        self.log.path()
    }

    /// Handles one thing the user typed. The personality and the skills that are on are put in
    /// the chat once, as a message before the user's, when they differ from the newest such
    /// message already there; from then on they are part of the history, so the router's cache
    /// covers them. Skills asked for one message ride along as one more message at the end of
    /// every request of this message (after the user's message and after any tool results) and
    /// are not kept; they are used up when it is answered or stopped by the cap, not when it
    /// fails.
    pub fn send(&mut self, text: &str, observer: &mut dyn Observer) -> Outcome {
        if !self.ready() {
            return Outcome::Failed(CHOOSE_A_PROVIDER.to_owned());
        }
        let (kept, extra) = match self.injections() {
            Ok(both) => both,
            Err(why) => return Outcome::Failed(format!("could not read the skills: {why}")),
        };
        self.cancel.reset();
        let block = self.block_to_add(kept.as_deref());
        let outcome = self.send_turn(text, block, extra.as_deref(), observer);
        if !matches!(outcome, Outcome::Failed(_) | Outcome::Cancelled) {
            self.chat_prompts.once.clear();
        }
        outcome
    }

    fn send_turn(
        &mut self,
        text: &str,
        block: Option<String>,
        extra: Option<&str>,
        observer: &mut dyn Observer,
    ) -> Outcome {
        // If anything fails we go back to this length, so a failed message
        // leaves no half-finished exchange in memory (the log still has it, and
        // the database keeps it marked as failed).
        let rollback = self.history.len();
        if let Some(block) = block {
            self.history.push(Message::User(block));
        }
        self.history.push(Message::User(text.to_owned()));
        if let Some(stored) = &mut self.stored {
            stored.turn += 1;
        }

        let mut guard = LoopGuard::default();
        for round in 1..=self.max_calls {
            // Old file text is left out of what is sent (the history itself stays whole).
            let shortened = crate::trim::trimmed(&self.history);
            let sent: &[Message] = shortened.as_deref().unwrap_or(&self.history);
            let request = match extra {
                None => assemble(&self.config, sent),
                Some(extra) => {
                    let mut messages = sent.to_vec();
                    messages.push(Message::User(extra.to_owned()));
                    assemble(&self.config, &messages)
                }
            };
            let result = self.provider.complete_cancellable(&request, &self.cancel);
            if self.cancel.is_cancelled() {
                // The log keeps what was asked; nothing of this call is saved or used.
                let _ = self.log.record_timed(&request, &result, None);
                self.abandon(rollback);
                return Outcome::Cancelled;
            }
            // A failed call, or a reply that is not an assistant message, fails the turn.
            let turn_failed = match &result {
                Ok(reply) => !matches!(reply.message, Message::Assistant { .. }),
                Err(_) => true,
            };

            let store_ms = match self.save(&request, &result, turn_failed) {
                Ok(ms) => ms,
                Err(why) => {
                    self.abandon(rollback);
                    return Outcome::Failed(format!("could not save the chat: {why}"));
                }
            };
            if let Err(e) = self.log.record_timed(&request, &result, store_ms) {
                if self.stored.is_some() {
                    observer.warning(&format!("could not write the turn log: {e}"));
                } else {
                    self.history.truncate(rollback);
                    return Outcome::Failed(format!("could not write the turn log: {e}"));
                }
            }
            let reply = match result {
                Ok(reply) => reply,
                Err(error) => {
                    self.rewind(rollback);
                    return Outcome::Failed(describe(&error));
                }
            };

            let Message::Assistant {
                content,
                tool_calls,
            } = &reply.message
            else {
                self.rewind(rollback);
                return Outcome::Failed("the reply was not an assistant message".to_owned());
            };
            let (content, tool_calls) = (content.clone(), tool_calls.clone());
            if let Some(raw) = &reply.usage {
                self.usage.add(Tokens::from_usage(raw));
            } else {
                self.usage.add(Tokens::default());
            }
            self.history.push(reply.message);
            if let Some(stored) = &mut self.stored {
                // The save above already included this reply.
                stored.saved = self.history.len();
            }

            if tool_calls.is_empty() {
                return Outcome::Answer(content.unwrap_or_default());
            }
            let mut stop = false;
            for call in &tool_calls {
                if self.cancel.is_cancelled() {
                    self.abandon(rollback);
                    return Outcome::Cancelled;
                }
                observer.tool_call(call);
                let mut output = self.run_tool(call);
                if self.cancel.is_cancelled() {
                    self.abandon(rollback);
                    return Outcome::Cancelled;
                }
                match guard.record(call, crate::toolview::failed(&output)) {
                    Verdict::Fine => {}
                    Verdict::Warn => {
                        output.push('\n');
                        output.push_str(loop_guard::NOTE);
                    }
                    Verdict::Stop => {
                        output.push('\n');
                        output.push_str(loop_guard::NOTE);
                        stop = true;
                    }
                }
                observer.tool_result(call, &output);
                self.history.push(Message::Tool {
                    tool_call_id: call.id.clone(),
                    content: output,
                });
            }
            if stop {
                if let Err(why) = self.flush() {
                    self.abandon(rollback);
                    return Outcome::Failed(format!("could not save the chat: {why}"));
                }
                return Outcome::LoopStopped { calls: round };
            }
        }
        // The last call's tool results are not carried by any further call, so save them now:
        // a chat that stopped on a tool call with no result could not be resumed.
        if let Err(why) = self.flush() {
            self.abandon(rollback);
            return Outcome::Failed(format!("could not save the chat: {why}"));
        }
        Outcome::CapReached {
            calls: self.max_calls,
        }
    }

    /// Saves one call to the database, creating the chat on the first one. Returns how long it
    /// took in milliseconds, or `None` when no database is attached.
    fn save(
        &mut self,
        request: &[u8],
        result: &Result<Reply, Box<ProviderError>>,
        turn_failed: bool,
    ) -> Result<Option<u64>, String> {
        let Some(stored) = &mut self.stored else {
            return Ok(None);
        };
        let started = Instant::now();
        let session_id = match stored.session_id {
            Some(id) => id,
            None => {
                let Some(Message::System(prompt)) = self.history.first() else {
                    return Err("the history does not start with the system message".to_owned());
                };
                let id = stored
                    .store
                    .create_session(&NewSession {
                        kind: Kind::Worker,
                        model: &self.config.model,
                        base_url: &self.target.base_url,
                        router: &self.config.router,
                        system_prompt: prompt,
                        cwd: &stored.context.cwd,
                    })
                    .map_err(|e| e.to_string())?;
                stored.session_id = Some(id);
                stored
                    .store
                    .set_chat_prompts(id, &self.chat_prompts)
                    .map_err(|e| e.to_string())?;
                id
            }
        };
        let reply = match result {
            Ok(r) if !turn_failed => Some(&r.message),
            _ => None,
        };
        stored
            .store
            .save_call(&SaveCall {
                session_id,
                turn: stored.turn,
                pending: &self.history[stored.saved..],
                reply,
                turn_failed,
                // The system message is not counted: it belongs to the session.
                message_count: self.history.len() - 1,
                request,
                model: &self.config.model,
                base_url: &self.target.base_url,
                router: &self.config.router,
                result,
            })
            .map_err(|e| e.to_string())?;
        Ok(Some(
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        ))
    }

    /// Saves messages no call carried yet (see the call cap in [`Agent::send`]).
    fn flush(&mut self) -> Result<(), String> {
        let Some(stored) = &mut self.stored else {
            return Ok(());
        };
        let (Some(session_id), pending) = (stored.session_id, &self.history[stored.saved..]) else {
            return Ok(());
        };
        if pending.is_empty() {
            return Ok(());
        }
        stored
            .store
            .save_messages(session_id, stored.turn, pending)
            .map_err(|e| e.to_string())?;
        stored.saved = self.history.len();
        Ok(())
    }

    /// Goes back to before the user's message. The database already has the turn marked failed.
    fn rewind(&mut self, rollback: usize) {
        self.history.truncate(rollback);
        if let Some(stored) = &mut self.stored {
            stored.saved = rollback;
        }
    }

    /// Like [`Agent::rewind`] for when the save itself failed: earlier calls of this turn may
    /// be in the database as good, so mark them failed too, as far as the database allows.
    fn abandon(&mut self, rollback: usize) {
        self.rewind(rollback);
        if let Some(stored) = &mut self.stored
            && let Some(session_id) = stored.session_id
        {
            // Nothing more can be done if this fails too; the next resume drops an
            // unfinished turn anyway.
            let _ = stored.store.mark_turn_failed(session_id, stored.turn);
        }
    }

    /// Every provider that can be switched to: the saved ones, then the profile files that are
    /// not saved yet, by name. A profile file whose name is saved or was imported before is not
    /// listed. A profile that cannot be read is listed with the reason.
    pub fn providers(&self) -> Result<Vec<Listed>, String> {
        let catalog = self.catalog.as_ref().ok_or(NO_SWITCHING)?;
        let mut all = Vec::new();
        let mut hidden: Vec<String> = Vec::new();
        if let Some(stored) = &self.stored {
            for def in stored.store.list_providers().map_err(|e| e.to_string())? {
                hidden.push(def.name.clone());
                all.push(Listed {
                    name: def.name.clone(),
                    source: Source::Saved,
                    def: Ok(def),
                });
            }
            hidden.extend(
                stored
                    .store
                    .imported_profiles()
                    .map_err(|e| e.to_string())?,
            );
        }
        if let Some(dir) = &catalog.profile_dir {
            all.extend(
                providers::read_profiles(dir)
                    .into_iter()
                    .filter(|l| !hidden.contains(&l.name)),
            );
        }
        all.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(all)
    }

    /// Copies every readable profile file that has not been imported into the database (key
    /// included) and remembers its name, so deleting the provider later is not undone. The files
    /// are not touched. Returns how many were copied.
    pub fn import_profiles(&mut self) -> Result<usize, String> {
        let dir = match self.catalog.as_ref().and_then(|c| c.profile_dir.clone()) {
            Some(dir) => dir,
            None => return Ok(0),
        };
        let stored = self.stored.as_mut().ok_or(NOT_SAVING)?;
        let done = stored
            .store
            .imported_profiles()
            .map_err(|e| e.to_string())?;
        let saved: Vec<String> = stored
            .store
            .list_providers()
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|d| d.name)
            .collect();
        let (mut copied, mut seen) = (0, Vec::new());
        for entry in providers::read_profiles(&dir) {
            if done.contains(&entry.name) {
                continue;
            }
            let Ok(def) = entry.def else { continue };
            if !saved.contains(&def.name) {
                stored.store.add_provider(&def).map_err(|e| e.to_string())?;
                copied += 1;
            }
            seen.push(entry.name);
        }
        stored
            .store
            .mark_imported(&seen)
            .map_err(|e| e.to_string())?;
        Ok(copied)
    }

    /// The store, for the menus that edit providers and models.
    pub fn store_mut(&mut self) -> Result<&mut Store, String> {
        self.stored
            .as_mut()
            .map(|s| &mut s.store)
            .ok_or_else(|| "chats are not being saved, so providers cannot be edited".to_owned())
    }

    /// A provider's full definition by name.
    pub fn provider_def(&self, name: &str) -> Result<ProviderDef, String> {
        self.providers()?
            .into_iter()
            .find(|l| l.name == name)
            .ok_or_else(|| format!("no provider named {name}"))?
            .def
            .map_err(|why| format!("{name} cannot be used: {why}"))
    }

    /// Asks a saved provider's router for its models and stores the ones not seen before.
    pub fn refresh_models(&mut self, name: &str) -> Result<MergeReport, String> {
        let def = self.provider_def(name)?;
        let catalog = self.catalog.as_ref().ok_or(NO_SWITCHING)?;
        let ids = (catalog.build)(&def).models()?;
        self.store_mut()?
            .merge_fetched(name, &ids)
            .map_err(|e| e.to_string())
    }

    /// Sends one tiny request to a provider with a model and says how it went. Nothing is saved
    /// or logged.
    pub fn test_model(&self, name: &str, model: &str) -> Result<String, String> {
        let def = self.provider_def(name)?;
        let catalog = self.catalog.as_ref().ok_or(NO_SWITCHING)?;
        let provider = (catalog.build)(&def);
        let config = Config {
            model: model.to_owned(),
            router: def.router.clone(),
            ..Config::default()
        };
        let history = [
            Message::System("You are a connection test.".to_owned()),
            Message::User("Reply with the single word: ok".to_owned()),
        ];
        match provider.complete(&assemble(&config, &history)) {
            Ok(reply) => {
                let said = match &reply.message {
                    Message::Assistant { content, .. } => content.clone().unwrap_or_default(),
                    _ => String::new(),
                };
                let said: String = said.trim().chars().take(120).collect();
                Ok(format!(
                    "status {} in {} ms, the model said: {said}",
                    reply.status,
                    reply.elapsed.as_millis()
                ))
            }
            Err(error) => Err(describe(&error)),
        }
    }

    /// Saves changes to a saved provider (the name may change). If it is the one in use, the
    /// change applies to the next request, with the usual warning.
    pub fn edit_provider(
        &mut self,
        old_name: &str,
        def: ProviderDef,
    ) -> Result<Vec<String>, String> {
        providers::check_name(&def.name)?;
        providers::check_base_url(&def.base_url)?;
        providers::check_model(&def.model)?;
        if def.name != old_name && self.providers()?.iter().any(|l| l.name == def.name) {
            return Err(format!("the name {} is already used", def.name));
        }
        let new_name = def.name.clone();
        self.store_mut()?
            .update_provider(old_name, &def)
            .map_err(|e| e.to_string())?;
        if self.target.name == old_name {
            self.select(def)?;
            let _ = self.remember(&new_name);
            return Ok(vec![COLD_CACHE.to_owned()]);
        }
        Ok(Vec::new())
    }

    /// Changes the model for the next message. History, provider and pinning stay. Returns what
    /// the user should be warned about.
    pub fn switch_model(&mut self, model: &str) -> Result<Vec<String>, String> {
        if !self.ready() {
            return Err(CHOOSE_A_PROVIDER.to_owned());
        }
        providers::check_model(model)?;
        if model == self.config.model {
            return Err(format!("already using {model}"));
        }
        let mut warnings = vec![COLD_CACHE.to_owned()];
        if !self.config.router.provider_order.is_empty() {
            warnings.push(format!(
                "provider pinning ({}) stays and may not serve {model}",
                self.config.router.provider_order.join(", ")
            ));
        }
        self.config.model = model.to_owned();
        Ok(warnings)
    }

    /// Switches to a profile or saved provider by name, taking its model and pinning too. History
    /// stays. Nothing changes when the name is unknown or its definition cannot be read.
    pub fn switch_provider(&mut self, name: &str) -> Result<Vec<String>, String> {
        self.switch_provider_with(name, None)
    }

    /// Like [`Agent::switch_provider`], but with this model, which also becomes the provider's
    /// default when the provider is a saved one.
    pub fn switch_provider_with(
        &mut self,
        name: &str,
        model: Option<&str>,
    ) -> Result<Vec<String>, String> {
        let listed = self.providers()?;
        let entry = listed
            .into_iter()
            .find(|l| l.name == name)
            .ok_or_else(|| format!("no provider named {name} (see /provider)"))?;
        let mut def = entry
            .def
            .map_err(|why| format!("{name} cannot be used: {why}"))?;
        if let Some(model) = model {
            providers::check_model(model)?;
            def.model = model.to_owned();
        }
        let mut warnings = if self.ready() {
            vec![COLD_CACHE.to_owned()]
        } else {
            Vec::new()
        };
        let name = def.name.clone();
        let default_model =
            (entry.source == Source::Saved && model.is_some()).then(|| def.model.clone());
        self.select(def)?;
        if let Some(model) = default_model
            && let Some(stored) = &mut self.stored
            && let Err(e) = stored.store.set_default_model(&name, &model)
        {
            warnings.push(format!("could not save the default model: {e}"));
        }
        // Remember the choice for the next start. Failing to is a warning, not a failed switch.
        if let Err(why) = self.remember(&name) {
            warnings.push(format!(
                "could not remember this choice for next time: {why}"
            ));
        }
        Ok(warnings)
    }

    /// Makes `def` the provider for the next request.
    fn select(&mut self, def: ProviderDef) -> Result<(), String> {
        let catalog = self.catalog.as_ref().ok_or(NO_SWITCHING)?;
        self.provider = (catalog.build)(&def);
        self.config.model = def.model;
        self.config.router = def.router;
        self.target = Target {
            name: def.name,
            base_url: def.base_url,
        };
        Ok(())
    }

    fn remember(&mut self, name: &str) -> Result<(), String> {
        match &mut self.stored {
            Some(stored) => stored
                .store
                .set_setting(LAST_PROVIDER, name)
                .map_err(|e| e.to_string()),
            None => Ok(()),
        }
    }

    /// At start-up with no provider given: the one switched to last time, if it still exists
    /// and can be read. Returns a note for the screen, if there is one to show.
    pub fn use_remembered_provider(&mut self) -> Option<String> {
        let name = self.stored.as_ref()?.store.setting(LAST_PROVIDER).ok()??;
        let def = match self
            .providers()
            .ok()?
            .into_iter()
            .find(|l| l.name == name)?
            .def
        {
            Ok(def) => def,
            Err(why) => {
                return Some(format!(
                    "note: the provider you used last, {name}, cannot be used: {why}"
                ));
            }
        };
        self.select(def).err()
    }

    /// Saves a new provider. The name must not belong to a profile or another saved provider.
    pub fn add_provider(&mut self, def: ProviderDef) -> Result<(), String> {
        providers::check_name(&def.name)?;
        providers::check_base_url(&def.base_url)?;
        providers::check_model(&def.model)?;
        if self.providers()?.iter().any(|l| l.name == def.name) {
            return Err(format!("the name {} is already used", def.name));
        }
        let stored = self
            .stored
            .as_mut()
            .ok_or("chats are not being saved, so a provider cannot be saved either")?;
        stored.store.add_provider(&def).map_err(|e| e.to_string())
    }

    /// Deletes a saved provider (profile files are never touched). The one in use stays in use
    /// until you switch.
    pub fn remove_provider(&mut self, name: &str) -> Result<(), String> {
        let listed = self.providers()?;
        match listed.iter().find(|l| l.name == name) {
            None => return Err(format!("no provider named {name}")),
            Some(l) if l.source == Source::Profile => {
                return Err(format!(
                    "{name} is a profile file; delete the file to remove it"
                ));
            }
            Some(_) => {}
        }
        let stored = self.stored.as_mut().ok_or(NOT_SAVING)?;
        stored
            .store
            .remove_provider(name)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// The model ids the current provider offers, sorted.
    pub fn list_models(&self) -> Result<Vec<String>, String> {
        let mut models = self.provider.models()?;
        models.sort();
        models.dedup();
        Ok(models)
    }

    fn run_tool(&self, call: &ToolCall) -> String {
        match self.tools.iter().find(|t| t.spec().name == call.name) {
            Some(tool) => tool.run(&call.arguments),
            None => format!("error: unknown tool '{}'", call.name),
        }
    }
}

/// A one-line description of a failed call, with the start of the router's
/// answer when there is one (the full body is in the log).
fn describe(error: &ProviderError) -> String {
    match (&error.raw_body, error.status) {
        (Some(body), Some(_)) => {
            let snippet: String = body.chars().take(300).collect();
            format!("{}: {snippet}", error.message)
        }
        _ => error.message.clone(),
    }
}

/// A provider that plays back prepared replies and records every request it
/// receives. For tests; it never touches the network.
//
// Rust note: `Rc<RefCell<..>>` lets two owners (the provider, which moves into
// the agent, and the test) share one list. `RefCell` allows changing it through
// a shared reference, checked while the program runs.
pub struct ScriptedProvider {
    replies: RefCell<VecDeque<Result<Reply, Box<ProviderError>>>>,
    requests: Rc<RefCell<Vec<Vec<u8>>>>,
    models: Option<Result<Vec<String>, String>>,
}

impl ScriptedProvider {
    pub fn new(replies: Vec<Result<Reply, Box<ProviderError>>>) -> Self {
        Self {
            replies: RefCell::new(replies.into()),
            requests: Rc::new(RefCell::new(Vec::new())),
            models: None,
        }
    }

    /// What `models()` answers; without this the provider cannot list models.
    pub fn with_models(mut self, models: Result<Vec<String>, String>) -> Self {
        self.models = Some(models);
        self
    }

    /// Every request received so far, in order. Keep this before handing the
    /// provider to an agent.
    pub fn requests(&self) -> Rc<RefCell<Vec<Vec<u8>>>> {
        Rc::clone(&self.requests)
    }

    fn reply(message: Message, finish_reason: &str, body: serde_json::Value) -> Reply {
        Reply {
            status: 200,
            raw_body: body.to_string(),
            elapsed: Duration::from_millis(1),
            served_by: ServedBy::default(),
            message,
            finish_reason: Some(finish_reason.to_owned()),
            response_model: Some("scripted".to_owned()),
            usage: Some(r#"{"prompt_tokens":1}"#.to_owned()),
        }
    }

    /// A reply that answers in text.
    pub fn text(content: &str) -> Result<Reply, Box<ProviderError>> {
        let message = Message::Assistant {
            content: Some(content.to_owned()),
            tool_calls: vec![],
        };
        Ok(Self::reply(
            message,
            "stop",
            serde_json::json!({ "scripted": true, "content": content }),
        ))
    }

    /// A reply that asks for several tool calls, given as `(id, name, arguments)`.
    pub fn tool_calls(calls: &[(&str, &str, &str)]) -> Result<Reply, Box<ProviderError>> {
        let tool_calls: Vec<ToolCall> = calls
            .iter()
            .map(|(id, name, arguments)| ToolCall {
                id: (*id).to_owned(),
                name: (*name).to_owned(),
                arguments: (*arguments).to_owned(),
            })
            .collect();
        let message = Message::Assistant {
            content: None,
            tool_calls,
        };
        Ok(Self::reply(
            message,
            "tool_calls",
            serde_json::json!({ "scripted": true, "tool_calls": calls.len() }),
        ))
    }

    /// A reply that asks for one tool call.
    pub fn tool_call(id: &str, name: &str, arguments: &str) -> Result<Reply, Box<ProviderError>> {
        Self::tool_calls(&[(id, name, arguments)])
    }

    /// The same reply, reporting this `usage` object.
    pub fn with_usage(
        reply: Result<Reply, Box<ProviderError>>,
        usage: &str,
    ) -> Result<Reply, Box<ProviderError>> {
        reply.map(|mut reply| {
            reply.usage = Some(usage.to_owned());
            reply
        })
    }

    /// A call that fails.
    pub fn failure(message: &str) -> Result<Reply, Box<ProviderError>> {
        Err(Box::new(ProviderError {
            message: message.to_owned(),
            status: None,
            raw_body: None,
            elapsed: Duration::from_millis(1),
            served_by: ServedBy::default(),
        }))
    }
}

impl Provider for ScriptedProvider {
    fn complete(&self, request: &[u8]) -> Result<Reply, Box<ProviderError>> {
        self.requests.borrow_mut().push(request.to_vec());
        self.replies
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| Self::failure("the scripted provider has no more replies"))
    }

    fn models(&self) -> Result<Vec<String>, String> {
        self.models
            .clone()
            .unwrap_or_else(|| Err("this provider cannot list its models".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ReadFile;
    use crate::tools::testutil::Scratch;

    type Requests = Rc<RefCell<Vec<Vec<u8>>>>;

    fn agent_in(
        dir: &Scratch,
        replies: Vec<Result<Reply, Box<ProviderError>>>,
    ) -> (Agent, Requests) {
        let provider = ScriptedProvider::new(replies);
        let requests = provider.requests();
        let log = TurnLog::create(&dir.path().join("logs")).unwrap();
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(ReadFile::new(dir.path()))];
        let agent =
            Agent::new("m", Box::new(provider), tools, log).with_target("test", "http://x/v1");
        (agent, requests)
    }

    fn messages_of(request: &[u8]) -> Vec<serde_json::Value> {
        let value: serde_json::Value = serde_json::from_slice(request).unwrap();
        value["messages"].as_array().unwrap().clone()
    }

    fn log_lines(agent: &Agent) -> Vec<serde_json::Value> {
        std::fs::read_to_string(agent.log_path())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    const READ_A: &str = r#"{"path":"a.txt"}"#;

    #[test]
    fn a_tool_call_then_its_result_then_a_final_answer() {
        let dir = Scratch::new("agent-basic");
        std::fs::write(dir.path().join("a.txt"), "hello from a\n").unwrap();
        let (mut agent, requests) = agent_in(
            &dir,
            vec![
                ScriptedProvider::tool_call("call_1", "read_file", READ_A),
                ScriptedProvider::text("The file says hello."),
            ],
        );
        let outcome = agent.send("what is in a.txt?", &mut NoObserver);
        assert_eq!(outcome, Outcome::Answer("The file says hello.".into()));

        // The history is system, user, assistant(tool call), tool result, assistant.
        let history = agent.history();
        assert_eq!(history.len(), 5);
        assert!(matches!(history[0], Message::System(_)));
        assert_eq!(history[1], Message::User("what is in a.txt?".into()));
        assert!(
            matches!(&history[2], Message::Assistant { tool_calls, .. } if tool_calls.len() == 1)
        );
        assert_eq!(
            history[3],
            Message::Tool {
                tool_call_id: "call_1".into(),
                content: "hello from a\n".into()
            }
        );

        // The second request carries the tool result back to the model.
        let requests = requests.borrow();
        assert_eq!(requests.len(), 2);
        let second = messages_of(&requests[1]);
        assert_eq!(second[3]["role"], "tool");
        assert_eq!(second[3]["tool_call_id"], "call_1");
        assert_eq!(second[3]["content"], "hello from a\n");
    }

    #[test]
    fn each_request_starts_with_exactly_the_previous_requests_messages() {
        let dir = Scratch::new("agent-prefix");
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let (mut agent, requests) = agent_in(
            &dir,
            vec![
                ScriptedProvider::tool_call("c1", "read_file", READ_A),
                ScriptedProvider::text("first answer"),
                ScriptedProvider::text("second answer"),
            ],
        );
        agent.send("one", &mut NoObserver);
        agent.send("two", &mut NoObserver);
        let requests = requests.borrow();
        assert_eq!(requests.len(), 3);
        for pair in requests.windows(2) {
            let (before, after) = (messages_of(&pair[0]), messages_of(&pair[1]));
            assert!(after.len() > before.len());
            assert_eq!(&after[..before.len()], &before[..]);
        }
    }

    #[test]
    fn old_file_text_is_left_out_of_requests_but_stays_in_the_history() {
        let dir = Scratch::new("agent-trim");
        let big = "z".repeat(1000);
        let args = format!("{{\"path\":\"a.txt\",\"content\":\"{big}\"}}");
        let mut replies = Vec::new();
        for i in 0..crate::trim::KEEP + crate::trim::STEP {
            replies.push(ScriptedProvider::tool_call(
                &format!("c{i}"),
                "write_file",
                &args,
            ));
            replies.push(ScriptedProvider::text("done"));
        }
        let (mut agent, requests) = agent_in(&dir, replies);
        for i in 0..crate::trim::KEEP + crate::trim::STEP {
            agent.send(&format!("message {i}"), &mut NoObserver);
        }
        let requests = requests.borrow();
        let last = String::from_utf8(requests.last().unwrap().clone()).unwrap();
        let first_of_last_message =
            String::from_utf8(requests[requests.len() - 2].clone()).unwrap();
        // The newest message's own write is not in its first request yet.
        for (request, whole) in [
            (&last, crate::trim::KEEP),
            (&first_of_last_message, crate::trim::KEEP - 1),
        ] {
            assert_eq!(
                request.matches("bytes of text left out").count(),
                crate::trim::STEP
            );
            assert_eq!(request.matches(&big).count(), whole);
        }
        // The earlier requests were sent whole.
        let early = String::from_utf8(requests[0].clone()).unwrap();
        assert_eq!(early.matches("bytes of text left out").count(), 0);
    }

    #[test]
    fn compacting_starts_a_new_chat_with_the_summary_and_keeps_the_skills() {
        let dir = Scratch::new("agent-compact");
        let (mut agent, requests) = stored_agent(
            &dir,
            vec![
                ScriptedProvider::text("one"),
                ScriptedProvider::text("The user wants X. Done: Y."),
                ScriptedProvider::text("two"),
            ],
        );
        with_prompts(&mut agent);
        agent.set_skill("tdd", SkillState::On).unwrap();
        agent.send("a", &mut NoObserver);
        let report = agent.compact().unwrap();
        assert_eq!(report.messages, 3);
        assert_eq!(report.summary_chars, "The user wants X. Done: Y.".len());
        assert_eq!(agent.history().len(), 2);
        assert!(matches!(&agent.history()[1], Message::User(t)
            if t.starts_with(SUMMARY_MARK) && t.ends_with("Done: Y.")
                && t.contains("\n\n[earlier chat: 1]\n\n")));
        assert!(agent.chat_prompts().on.contains(&"tdd".to_owned()));
        assert!(agent.usage().last.is_none());

        // The request for the summary carried the whole chat and ended with the question.
        {
            let requests = requests.borrow();
            let asked = texts(&requests[1]);
            assert!(asked.iter().any(|t| t == "a"));
            assert_eq!(asked.last().unwrap(), COMPACT_ASK);
        }

        // The next message goes into a new chat: summary, then the skill block, then the message.
        agent.send("b", &mut NoObserver);
        let requests = requests.borrow();
        let sent = texts(&requests[2]);
        assert!(sent[1].starts_with(SUMMARY_MARK));
        assert!(sent[2].starts_with(ACTIVE_MARK));
        assert_eq!(sent[3], "b");
        assert_eq!(sent.len(), 4);
        let store = peek(&dir);
        assert_eq!(rows(&store, "SELECT COUNT(*) FROM sessions"), 2);
        // The summary was saved with the new chat.
        assert_eq!(
            rows(
                &store,
                &format!(
                    "SELECT COUNT(*) FROM messages WHERE text LIKE '{}%'",
                    &SUMMARY_MARK[..20]
                )
            ),
            1
        );
    }

    #[test]
    fn a_compaction_that_fails_changes_nothing() {
        let dir = Scratch::new("agent-compact-fails");
        let (mut agent, requests) = stored_agent(&dir, vec![]);
        // Nothing to compact: no request is made.
        assert!(agent.compact().unwrap_err().contains("nothing to compact"));
        assert!(requests.borrow().is_empty());

        let (mut agent, _) = stored_agent(
            &dir,
            vec![
                ScriptedProvider::text("one"),
                ScriptedProvider::failure("boom"),
                ScriptedProvider::tool_call("c1", "read_file", READ_A),
                ScriptedProvider::text("   "),
            ],
        );
        agent.send("a", &mut NoObserver);
        let before = agent.history().to_vec();
        assert!(agent.compact().unwrap_err().contains("boom"));
        assert!(agent.compact().unwrap_err().contains("asked for a tool"));
        assert!(agent.compact().unwrap_err().contains("nothing"));
        assert_eq!(agent.history(), &before[..]);
    }

    #[test]
    fn automatic_compaction_has_a_remembered_limit_and_fires_only_when_it_is_reached() {
        let dir = Scratch::new("agent-compact-auto");
        let (mut agent, _) = stored_agent(
            &dir,
            vec![
                ScriptedProvider::with_usage(
                    ScriptedProvider::text("one"),
                    r#"{"prompt_tokens":30000}"#,
                ),
                ScriptedProvider::with_usage(
                    ScriptedProvider::text("two"),
                    r#"{"prompt_tokens":90000}"#,
                ),
            ],
        );
        assert_eq!(agent.auto_compact_at(), Some(DEFAULT_AUTO_COMPACT_AT));
        assert!(!agent.needs_compacting());
        agent.send("a", &mut NoObserver);
        assert!(!agent.needs_compacting());
        assert!(agent.set_auto_compact(Some(500)).is_err());
        agent.set_auto_compact(Some(20_000)).unwrap();
        assert_eq!(agent.auto_compact_at(), Some(20_000));
        assert!(agent.needs_compacting());
        agent.set_auto_compact(None).unwrap();
        assert_eq!(agent.auto_compact_at(), None);
        assert!(!agent.needs_compacting());
        agent
            .set_auto_compact(Some(DEFAULT_AUTO_COMPACT_AT))
            .unwrap();
        assert!(!agent.needs_compacting());
        agent.send("b", &mut NoObserver);
        assert!(agent.needs_compacting());
        // A second look at the database sees the saved limit.
        assert_eq!(
            peek(&dir)
                .setting(crate::store::AUTO_COMPACT)
                .unwrap()
                .as_deref(),
            Some("80000")
        );
    }

    #[test]
    fn a_model_that_keeps_failing_is_warned_then_stopped() {
        let dir = Scratch::new("agent-loop");
        let replies = (0..10)
            .map(|i| {
                ScriptedProvider::tool_call(
                    &format!("c{i}"),
                    "read_file",
                    &format!("{{\"path\":\"missing{i}.txt\"}}"),
                )
            })
            .collect();
        let (mut agent, requests) = agent_in(&dir, replies);
        let outcome = agent.send("find it", &mut NoObserver);
        assert_eq!(outcome, Outcome::LoopStopped { calls: 5 });
        assert_eq!(requests.borrow().len(), 5);
        // The 2nd call's result has no note, the 3rd and later ones do.
        let sent = messages_of(&requests.borrow()[4]);
        let notes = sent
            .iter()
            .filter(|m| m.to_string().contains(crate::loop_guard::NOTE))
            .count();
        assert_eq!(notes, 2);
    }

    #[test]
    fn the_cap_stops_a_model_that_always_asks_for_a_tool() {
        let dir = Scratch::new("agent-cap");
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let replies = (0..6)
            .map(|i| ScriptedProvider::tool_call(&format!("c{i}"), "read_file", READ_A))
            .collect();
        let (agent, requests) = agent_in(&dir, replies);
        let mut agent = agent.with_max_calls(3);
        let outcome = agent.send("loop forever", &mut NoObserver);
        assert_eq!(outcome, Outcome::CapReached { calls: 3 });
        assert_eq!(requests.borrow().len(), 3);
        assert_eq!(log_lines(&agent).len(), 3);
    }

    #[test]
    fn the_cap_counts_per_message_and_starts_again_for_the_next() {
        let dir = Scratch::new("agent-cap-reset");
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let (agent, requests) = agent_in(
            &dir,
            vec![
                ScriptedProvider::tool_call("c1", "read_file", READ_A),
                ScriptedProvider::text("a1"),
                ScriptedProvider::tool_call("c2", "read_file", READ_A),
                ScriptedProvider::text("a2"),
            ],
        );
        let mut agent = agent.with_max_calls(2);
        assert_eq!(
            agent.send("m1", &mut NoObserver),
            Outcome::Answer("a1".into())
        );
        assert_eq!(
            agent.send("m2", &mut NoObserver),
            Outcome::Answer("a2".into())
        );
        assert_eq!(requests.borrow().len(), 4);
    }

    #[test]
    fn an_unknown_tool_becomes_an_error_result_and_the_loop_continues() {
        let dir = Scratch::new("agent-unknown");
        let (mut agent, requests) = agent_in(
            &dir,
            vec![
                ScriptedProvider::tool_call("c1", "teleport", "{}"),
                ScriptedProvider::text("sorry"),
            ],
        );
        assert_eq!(
            agent.send("go", &mut NoObserver),
            Outcome::Answer("sorry".into())
        );
        let second = messages_of(&requests.borrow()[1]);
        assert_eq!(second[3]["content"], "error: unknown tool 'teleport'");
    }

    #[test]
    fn bad_tool_arguments_become_an_error_result_too() {
        let dir = Scratch::new("agent-badargs");
        let (mut agent, requests) = agent_in(
            &dir,
            vec![
                ScriptedProvider::tool_call("c1", "read_file", "not json"),
                ScriptedProvider::text("ok"),
            ],
        );
        agent.send("go", &mut NoObserver);
        let second = messages_of(&requests.borrow()[1]);
        assert!(
            second[3]["content"]
                .as_str()
                .unwrap()
                .starts_with("error: invalid arguments")
        );
    }

    #[test]
    fn several_tool_calls_in_one_reply_run_in_order_with_matching_ids() {
        let dir = Scratch::new("agent-multi");
        std::fs::write(dir.path().join("a.txt"), "AAA").unwrap();
        std::fs::write(dir.path().join("b.txt"), "BBB").unwrap();
        let (mut agent, requests) = agent_in(
            &dir,
            vec![
                ScriptedProvider::tool_calls(&[
                    ("c1", "read_file", READ_A),
                    ("c2", "read_file", r#"{"path":"b.txt"}"#),
                ]),
                ScriptedProvider::text("both read"),
            ],
        );
        agent.send("read both", &mut NoObserver);
        let second = messages_of(&requests.borrow()[1]);
        assert_eq!(
            (
                second[3]["tool_call_id"].as_str(),
                second[3]["content"].as_str()
            ),
            (Some("c1"), Some("AAA"))
        );
        assert_eq!(
            (
                second[4]["tool_call_id"].as_str(),
                second[4]["content"].as_str()
            ),
            (Some("c2"), Some("BBB"))
        );
    }

    #[test]
    fn the_observer_hears_each_tool_call_in_order() {
        struct Recorder(Vec<String>);
        impl Observer for Recorder {
            fn tool_call(&mut self, call: &ToolCall) {
                self.0.push(format!("{} {}", call.name, call.arguments));
            }
        }
        let dir = Scratch::new("agent-observer");
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let (mut agent, _) = agent_in(
            &dir,
            vec![
                ScriptedProvider::tool_call("c1", "read_file", READ_A),
                ScriptedProvider::text("done"),
            ],
        );
        let mut recorder = Recorder(vec![]);
        agent.send("go", &mut recorder);
        assert_eq!(recorder.0, vec![format!("read_file {READ_A}")]);
    }

    #[test]
    fn a_failed_call_leaves_no_trace_in_memory_but_is_logged() {
        let dir = Scratch::new("agent-fail");
        let (mut agent, requests) = agent_in(
            &dir,
            vec![
                ScriptedProvider::failure("request failed: Connection refused"),
                ScriptedProvider::text("back again"),
            ],
        );
        let before = agent.history().len();
        let outcome = agent.send("hello?", &mut NoObserver);
        assert_eq!(
            outcome,
            Outcome::Failed("request failed: Connection refused".into())
        );
        assert_eq!(agent.history().len(), before);

        let lines = log_lines(&agent);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["error"], "request failed: Connection refused");

        // The next message works, and the failed one is not in its request.
        assert_eq!(
            agent.send("again", &mut NoObserver),
            Outcome::Answer("back again".into())
        );
        let second = messages_of(&requests.borrow()[1]);
        assert_eq!(second.len(), 2);
        assert_eq!(second[1]["content"], "again");
    }

    #[test]
    fn every_call_to_the_model_is_logged_in_order() {
        let dir = Scratch::new("agent-log");
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let (mut agent, _) = agent_in(
            &dir,
            vec![
                ScriptedProvider::tool_call("c1", "read_file", READ_A),
                ScriptedProvider::text("done"),
            ],
        );
        agent.send("go", &mut NoObserver);
        let lines = log_lines(&agent);
        let turns: Vec<u64> = lines.iter().map(|l| l["turn"].as_u64().unwrap()).collect();
        assert_eq!(turns, vec![1, 2]);
        assert_eq!(lines[1]["request"]["messages"][3]["role"], "tool");
    }

    #[test]
    fn the_same_input_gives_byte_identical_requests_in_two_runs() {
        let run = |tag: &str| {
            let dir = Scratch::new(tag);
            std::fs::write(dir.path().join("a.txt"), "same file\n").unwrap();
            let (mut agent, requests) = agent_in(
                &dir,
                vec![
                    ScriptedProvider::tool_call("c1", "read_file", READ_A),
                    ScriptedProvider::text("answer"),
                ],
            );
            agent.send("read a.txt", &mut NoObserver);
            requests.borrow().clone()
        };
        let (first, second) = (run("agent-det-1"), run("agent-det-2"));
        assert_eq!(first, second);
    }

    #[test]
    fn router_options_ride_along_in_every_request_of_a_run() {
        let dir = Scratch::new("agent-router");
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let (agent, requests) = agent_in(
            &dir,
            vec![
                ScriptedProvider::tool_call("c1", "read_file", READ_A),
                ScriptedProvider::text("done"),
            ],
        );
        let mut agent = agent.with_router(RouterOptions {
            provider_order: vec!["deepseek".into()],
            allow_fallbacks: Some(false),
            session_id: Some("s-1".into()),
        });
        agent.send("go", &mut NoObserver);
        for request in requests.borrow().iter() {
            let v: serde_json::Value = serde_json::from_slice(request).unwrap();
            assert_eq!(v["provider"]["order"][0], "deepseek");
            assert_eq!(v["provider"]["allow_fallbacks"], false);
            assert_eq!(v["session_id"], "s-1");
        }
    }

    #[test]
    fn the_tool_descriptions_and_system_prompt_are_in_every_request() {
        let dir = Scratch::new("agent-spec");
        let (mut agent, requests) = agent_in(&dir, vec![ScriptedProvider::text("hi")]);
        agent.send("hello", &mut NoObserver);
        let value: serde_json::Value = serde_json::from_slice(&requests.borrow()[0]).unwrap();
        assert_eq!(value["messages"][0]["content"], SYSTEM_PROMPT);
        assert_eq!(value["tools"][0]["function"]["name"], "read_file");
        assert_eq!(value["stream"], false);
    }

    // ---- saving to the database (issue 9) ----

    fn db_path(dir: &Scratch) -> std::path::PathBuf {
        dir.path().join("harness.db")
    }

    fn context() -> StoreContext {
        StoreContext { cwd: "/w".into() }
    }

    /// An agent that saves into `<dir>/harness.db`.
    fn stored_agent(
        dir: &Scratch,
        replies: Vec<Result<Reply, Box<ProviderError>>>,
    ) -> (Agent, Requests) {
        let (agent, requests) = agent_in(dir, replies);
        let store = Store::open(&db_path(dir)).unwrap();
        (agent.with_store(store, context()), requests)
    }

    /// A second look at the same database, as another program would have (WAL allows it).
    fn peek(dir: &Scratch) -> Store {
        Store::open(&db_path(dir)).unwrap()
    }

    fn rows(store: &Store, sql: &str) -> i64 {
        store.conn().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn a_chat_is_saved_as_it_goes() {
        let dir = Scratch::new("agent-saved");
        std::fs::write(dir.path().join("a.txt"), "hello from a\n").unwrap();
        let (mut agent, _) = stored_agent(
            &dir,
            vec![
                ScriptedProvider::tool_call("c1", "read_file", READ_A),
                ScriptedProvider::text("The file says hello."),
            ],
        );
        agent.send("what is in a.txt?", &mut NoObserver);

        let mut store = peek(&dir);
        let saved = store.latest_session(Kind::Worker).unwrap().unwrap();
        assert_eq!((saved.model.as_str(), saved.cwd.as_str()), ("m", "/w"));
        assert_eq!(saved.system_prompt, SYSTEM_PROMPT);
        let loaded = store.load(saved.id).unwrap();
        // Everything after the system message is what was saved, exactly.
        assert_eq!(loaded.messages, agent.history()[1..]);
        assert_eq!(rows(&store, "SELECT COUNT(*) FROM calls"), 2);
        assert_eq!(
            rows(
                &store,
                "SELECT message_count FROM calls ORDER BY id DESC LIMIT 1"
            ),
            3
        );
    }

    #[test]
    fn nothing_is_saved_until_the_first_message() {
        let dir = Scratch::new("agent-lazy");
        let (_agent, _) = stored_agent(&dir, vec![]);
        assert_eq!(rows(&peek(&dir), "SELECT COUNT(*) FROM sessions"), 0);
    }

    #[test]
    fn a_resumed_chat_sends_the_same_bytes_as_one_that_never_stopped() {
        // Chat A answers "one", then the program stops.
        let dir = Scratch::new("agent-resume");
        let (mut first, _) = stored_agent(&dir, vec![ScriptedProvider::text("first answer")]);
        first.send("one", &mut NoObserver);
        drop(first);

        // Chat B starts again, loads the saved chat, and sends "two".
        let (mut second, second_requests) =
            stored_agent(&dir, vec![ScriptedProvider::text("second answer")]);
        let mut store = peek(&dir);
        let id = store.latest_session(Kind::Worker).unwrap().unwrap().id;
        second.restore(store.load(id).unwrap());
        second.send("two", &mut NoObserver);

        // Chat C never stopped: it sends "one" and then "two" in one run.
        let other = Scratch::new("agent-resume-control");
        let (mut control, control_requests) = agent_in(
            &other,
            vec![
                ScriptedProvider::text("first answer"),
                ScriptedProvider::text("second answer"),
            ],
        );
        control.send("one", &mut NoObserver);
        control.send("two", &mut NoObserver);

        assert_eq!(second_requests.borrow()[0], control_requests.borrow()[1]);
        assert_eq!(second.history(), control.history());
    }

    #[test]
    fn a_resumed_chat_keeps_numbering_and_saves_into_the_same_session() {
        let dir = Scratch::new("agent-resume-numbers");
        let (mut first, _) = stored_agent(&dir, vec![ScriptedProvider::text("a1")]);
        first.send("one", &mut NoObserver);
        drop(first);

        let (mut second, _) = stored_agent(&dir, vec![ScriptedProvider::text("a2")]);
        let mut store = peek(&dir);
        let id = store.latest_session(Kind::Worker).unwrap().unwrap().id;
        second.restore(store.load(id).unwrap());
        second.send("two", &mut NoObserver);

        assert_eq!(rows(&store, "SELECT COUNT(*) FROM sessions"), 1);
        assert_eq!(rows(&store, "SELECT MAX(turn) FROM messages"), 2);
        assert_eq!(rows(&store, "SELECT MAX(seq) FROM messages"), 3);
        assert_eq!(store.load(id).unwrap().messages.len(), 4);
    }

    #[test]
    fn the_saved_system_prompt_wins_over_the_current_one() {
        let dir = Scratch::new("agent-old-prompt");
        let (mut agent, requests) = stored_agent(&dir, vec![ScriptedProvider::text("hi")]);
        let mut store = peek(&dir);
        // A chat saved long ago, with a different prompt.
        let id = store
            .create_session(&crate::store::NewSession {
                kind: Kind::Worker,
                model: "m",
                base_url: "http://x/v1",
                router: &RouterOptions::default(),
                system_prompt: "an older prompt",
                cwd: "/w",
            })
            .unwrap();
        let user = [Message::User("earlier".into())];
        let answer = Message::Assistant {
            content: Some("ok".into()),
            tool_calls: vec![],
        };
        let result = ScriptedProvider::text("ok");
        store
            .save_call(&SaveCall {
                session_id: id,
                turn: 1,
                pending: &user,
                reply: Some(&answer),
                turn_failed: false,
                message_count: 1,
                request: b"{}",
                model: "m",
                base_url: "u",
                router: &RouterOptions::default(),
                result: &result,
            })
            .unwrap();
        agent.restore(store.load(id).unwrap());
        agent.send("now", &mut NoObserver);
        let first = messages_of(&requests.borrow()[0]);
        assert_eq!(first[0]["content"], "an older prompt");
        assert_ne!(first[0]["content"], SYSTEM_PROMPT);
    }

    #[test]
    fn a_failed_message_is_kept_as_failed_and_skipped_on_resume() {
        let dir = Scratch::new("agent-failed-saved");
        let (mut agent, _) = stored_agent(
            &dir,
            vec![
                ScriptedProvider::failure("request failed: Connection refused"),
                ScriptedProvider::text("back again"),
            ],
        );
        agent.send("hello?", &mut NoObserver);
        agent.send("again", &mut NoObserver);

        let mut store = peek(&dir);
        assert_eq!(
            rows(
                &store,
                "SELECT COUNT(*) FROM messages WHERE status = 'failed'"
            ),
            1
        );
        assert_eq!(
            rows(&store, "SELECT COUNT(*) FROM calls WHERE error IS NOT NULL"),
            1
        );
        let id = store.latest_session(Kind::Worker).unwrap().unwrap().id;
        let loaded = store.load(id).unwrap();
        assert_eq!(loaded.messages, agent.history()[1..]);
        assert_eq!(loaded.messages.len(), 2);
        // The failed turn kept its number, so the next turn is 3 after a resume.
        assert_eq!(loaded.last_turn, 2);
    }

    #[test]
    fn a_failure_after_a_saved_call_marks_the_whole_turn_failed() {
        let dir = Scratch::new("agent-failed-midturn");
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let (mut agent, _) = stored_agent(
            &dir,
            vec![
                ScriptedProvider::tool_call("c1", "read_file", READ_A),
                ScriptedProvider::failure("boom"),
            ],
        );
        let outcome = agent.send("go", &mut NoObserver);
        assert_eq!(outcome, Outcome::Failed("boom".into()));
        assert_eq!(agent.history().len(), 1);

        let store = peek(&dir);
        assert_eq!(rows(&store, "SELECT COUNT(*) FROM messages"), 3);
        assert_eq!(
            rows(
                &store,
                "SELECT COUNT(*) FROM messages WHERE status = 'failed'"
            ),
            3
        );
    }

    /// Cancels the message while the request is in flight, as Esc would.
    struct CancelsInFlight(Cancel);
    impl Provider for CancelsInFlight {
        fn complete(&self, _request: &[u8]) -> Result<Reply, Box<ProviderError>> {
            self.0.cancel();
            ScriptedProvider::failure("cancelled")
        }
    }

    /// Cancels the message when the first tool call is announced, before the tool runs.
    struct CancelsAtToolCall(Cancel);
    impl Observer for CancelsAtToolCall {
        fn tool_call(&mut self, _call: &ToolCall) {
            self.0.cancel();
        }
    }

    #[test]
    fn a_cancel_during_the_request_drops_the_message_and_the_chat_goes_on() {
        let dir = Scratch::new("agent-cancel-request");
        let cancel = Cancel::new();
        let log = TurnLog::create(&dir.path().join("logs")).unwrap();
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(ReadFile::new(dir.path()))];
        let mut agent = Agent::new("m", Box::new(CancelsInFlight(cancel.clone())), tools, log)
            .with_target("test", "http://x/v1")
            .with_cancel(cancel)
            .with_store(Store::open(&db_path(&dir)).unwrap(), context());
        let before = agent.history().len();
        assert_eq!(agent.send("slow one", &mut NoObserver), Outcome::Cancelled);
        assert_eq!(agent.history().len(), before);
        // The call was asked for, so the log shows it; nothing of it is in the database.
        assert_eq!(log_lines(&agent).len(), 1);
        assert_eq!(
            rows(
                &peek(&dir),
                "SELECT COUNT(*) FROM messages WHERE status = 'ok'"
            ),
            0
        );
    }

    #[test]
    fn a_cancel_before_a_tool_call_runs_it_not_and_marks_the_turn_failed() {
        let dir = Scratch::new("agent-cancel-tool");
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let (agent, requests) = stored_agent(
            &dir,
            vec![
                ScriptedProvider::tool_call("c1", "read_file", READ_A),
                ScriptedProvider::text("never asked"),
                ScriptedProvider::text("fresh start"),
            ],
        );
        let mut agent = agent;
        let mut observer = CancelsAtToolCall(agent.cancel_handle());
        let before = agent.history().len();
        assert_eq!(agent.send("go", &mut observer), Outcome::Cancelled);
        assert_eq!(agent.history().len(), before);
        // The first call was saved and is now marked failed with the rest of the turn.
        let store = peek(&dir);
        assert_eq!(
            rows(&store, "SELECT COUNT(*) FROM messages WHERE status = 'ok'"),
            0
        );
        assert!(
            rows(
                &store,
                "SELECT COUNT(*) FROM messages WHERE status = 'failed'"
            ) >= 2
        );
        // Only one request went out; the next message starts clean and clears the flag.
        assert_eq!(requests.borrow().len(), 1);
        assert_eq!(
            agent.send("again", &mut NoObserver),
            Outcome::Answer("never asked".into())
        );
        let second = messages_of(&requests.borrow()[1]);
        assert_eq!(
            second.len(),
            2,
            "the dropped message is not in the next request"
        );
    }

    #[test]
    fn hitting_the_call_cap_saves_the_last_tool_results_too() {
        let dir = Scratch::new("agent-cap-saved");
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let replies = (0..4)
            .map(|i| ScriptedProvider::tool_call(&format!("c{i}"), "read_file", READ_A))
            .collect();
        let (agent, _) = stored_agent(&dir, replies);
        let mut agent = agent.with_max_calls(2);
        assert_eq!(
            agent.send("loop", &mut NoObserver),
            Outcome::CapReached { calls: 2 }
        );

        // user, assistant, tool, assistant, tool.
        let mut store = peek(&dir);
        let id = store.latest_session(Kind::Worker).unwrap().unwrap().id;
        let loaded = store.load(id).unwrap();
        assert_eq!(loaded.messages.len(), 5);
        assert!(!loaded.dropped_unfinished_turn);
        assert_eq!(loaded.messages, agent.history()[1..]);
    }

    #[test]
    fn a_save_that_fails_fails_the_message_and_rolls_the_history_back() {
        let dir = Scratch::new("agent-save-fails");
        let (mut agent, _) = stored_agent(
            &dir,
            vec![
                ScriptedProvider::text("fine"),
                ScriptedProvider::text("never saved"),
                ScriptedProvider::text("saved after the repair"),
            ],
        );
        agent.send("one", &mut NoObserver);
        let before = agent.history().to_vec();

        // Break the database from outside.
        let other = peek(&dir);
        other
            .conn()
            .execute_batch("ALTER TABLE calls RENAME TO calls_gone")
            .unwrap();
        let outcome = agent.send("two", &mut NoObserver);
        let Outcome::Failed(why) = outcome else {
            panic!("expected a failure, got {outcome:?}");
        };
        assert!(why.starts_with("could not save the chat"), "{why}");
        assert_eq!(agent.history(), before);

        // After the repair the chat carries on, and the failed message is not in it.
        other
            .conn()
            .execute_batch("ALTER TABLE calls_gone RENAME TO calls")
            .unwrap();
        assert_eq!(
            agent.send("three", &mut NoObserver),
            Outcome::Answer("saved after the repair".into())
        );
        let mut store = peek(&dir);
        let id = store.latest_session(Kind::Worker).unwrap().unwrap().id;
        let texts: Vec<String> = store
            .load(id)
            .unwrap()
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::User(text) => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["one", "three"]);
    }

    #[test]
    fn the_log_line_holds_the_database_write_time() {
        let dir = Scratch::new("agent-store-ms");
        let (mut agent, _) = stored_agent(&dir, vec![ScriptedProvider::text("hi")]);
        agent.send("hello", &mut NoObserver);
        assert!(log_lines(&agent)[0]["store_ms"].is_u64());
        // Without a database the field is null.
        let plain = Scratch::new("agent-no-store");
        let (mut agent, _) = agent_in(&plain, vec![ScriptedProvider::text("hi")]);
        agent.send("hello", &mut NoObserver);
        assert!(log_lines(&agent)[0]["store_ms"].is_null());
    }

    // ---- switching chats ----

    /// Saves a finished two-message chat into `<dir>/harness.db` and returns its id.
    fn seed(dir: &Scratch, question: &str, answer: &str) -> i64 {
        let (mut agent, _) = stored_agent(dir, vec![ScriptedProvider::text(answer)]);
        agent.send(question, &mut NoObserver);
        agent.current_session().unwrap()
    }

    #[test]
    fn resume_with_no_id_loads_the_latest_chat_and_reports_it() {
        let dir = Scratch::new("agent-resume-latest");
        seed(&dir, "older", "a1");
        let newest = seed(&dir, "newest", "a2");
        let (mut agent, requests) = stored_agent(&dir, vec![ScriptedProvider::text("a3")]);
        let report = agent.resume(None).unwrap();
        assert_eq!(report.id, newest);
        assert_eq!(report.message_count, 2);
        assert!(report.differences.is_empty() && !report.dropped_unfinished_turn);
        assert!(
            report
                .headline()
                .starts_with(&format!("resumed chat {newest}, last used "))
        );
        assert_eq!(report.notes(), "");
        assert_eq!(agent.current_session(), Some(newest));

        agent.send("go on", &mut NoObserver);
        let sent = messages_of(&requests.borrow()[0]);
        assert_eq!(sent[1]["content"], "newest");
        assert_eq!(sent.len(), 4);
    }

    #[test]
    fn resume_with_an_id_picks_that_chat_and_keeps_saving_into_it() {
        let dir = Scratch::new("agent-resume-id");
        let older = seed(&dir, "the older chat", "a1");
        let newer = seed(&dir, "the newer chat", "a2");
        let (mut agent, _) = stored_agent(&dir, vec![ScriptedProvider::text("a3")]);
        agent.resume(Some(older)).unwrap();
        agent.send("more", &mut NoObserver);

        let mut store = peek(&dir);
        // The older chat grew; the newer one is untouched.
        assert_eq!(store.load(older).unwrap().messages.len(), 4);
        assert_eq!(store.load(newer).unwrap().messages.len(), 2);
        assert_eq!(rows(&store, "SELECT COUNT(*) FROM sessions"), 2);
    }

    #[test]
    fn switching_chats_in_one_run_leaves_the_chat_you_were_in_saved() {
        let dir = Scratch::new("agent-switch");
        let old = seed(&dir, "old chat", "a0");
        let (mut agent, _) = stored_agent(
            &dir,
            vec![ScriptedProvider::text("a1"), ScriptedProvider::text("a2")],
        );
        agent.send("current chat", &mut NoObserver);
        let current = agent.current_session().unwrap();
        assert_ne!(current, old);

        agent.resume(Some(old)).unwrap();
        agent.send("back in the old one", &mut NoObserver);
        // Going back to the current chat finds everything that was saved in it.
        agent.resume(Some(current)).unwrap();
        let texts: Vec<_> = agent
            .history()
            .iter()
            .filter_map(|m| match m {
                Message::User(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["current chat"]);
        assert_eq!(peek(&dir).load(old).unwrap().messages.len(), 4);
    }

    #[test]
    fn resume_reports_a_different_model_and_directory() {
        let dir = Scratch::new("agent-resume-diff");
        seed(&dir, "q", "a");
        let (agent, _) = agent_in(&dir, vec![]);
        let store = Store::open(&db_path(&dir)).unwrap();
        let other = Agent::new(
            "another-model",
            Box::new(ScriptedProvider::new(vec![])),
            vec![],
            TurnLog::create(&dir.path().join("logs2")).unwrap(),
        )
        .with_target("test", "http://x/v1")
        .with_store(
            store,
            StoreContext {
                cwd: "/elsewhere".into(),
            },
        );
        drop(agent);
        let mut other = other;
        let report = other.resume(None).unwrap();
        assert_eq!(report.differences.len(), 2, "{:?}", report.differences);
        assert!(report.differences[0].contains("another-model"));
        assert!(report.differences[1].contains("/elsewhere"));
        assert!(
            report
                .notes()
                .starts_with("warning: model was m, now another-model\n")
        );
    }

    #[test]
    fn resume_fails_clearly_when_there_is_nothing_to_load() {
        let dir = Scratch::new("agent-resume-none");
        let (mut agent, _) = stored_agent(&dir, vec![]);
        assert!(agent.resume(None).unwrap_err().contains("no saved chat"));
        assert!(
            agent
                .resume(Some(999))
                .unwrap_err()
                .contains("no saved chat with id 999")
        );

        let plain = Scratch::new("agent-resume-nostore");
        let (mut agent, _) = agent_in(&plain, vec![]);
        assert!(agent.resume(None).unwrap_err().contains("not being saved"));
        assert!(agent.sessions(10).unwrap_err().contains("not being saved"));
    }

    #[test]
    fn the_agent_lists_saved_chats_and_knows_which_is_current() {
        let dir = Scratch::new("agent-sessions");
        let first = seed(&dir, "first", "a");
        let (mut agent, _) = stored_agent(&dir, vec![ScriptedProvider::text("b")]);
        assert_eq!(agent.current_session(), None);
        agent.send("second", &mut NoObserver);
        let list = agent.sessions(10).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(Some(list[0].id), agent.current_session());
        assert_eq!(list[1].id, first);
    }

    // ---- switching model and provider (issue 10) ----

    /// The providers a scripted catalog built: (base URL, the requests each received).
    type Built = Rc<RefCell<Vec<(String, Requests)>>>;

    fn text_of(request: &[u8], field: &str) -> String {
        let value: serde_json::Value = serde_json::from_slice(request).unwrap();
        value[field].as_str().unwrap_or_default().to_owned()
    }

    /// A catalog whose providers are scripted: every provider built answers "from <name>" and
    /// is recorded as (base_url, its requests).
    fn scripted_catalog(profile_dir: Option<std::path::PathBuf>, built: Built) -> Catalog {
        Catalog {
            profile_dir,
            build: Box::new(move |def| {
                let provider = ScriptedProvider::new(vec![ScriptedProvider::text(&format!(
                    "from {}",
                    def.name
                ))]);
                built
                    .borrow_mut()
                    .push((def.base_url.clone(), provider.requests()));
                Box::new(provider)
            }),
        }
    }

    fn write_profile(dir: &Scratch, name: &str, text: &str) {
        let profiles = dir.path().join("profiles");
        std::fs::create_dir_all(&profiles).unwrap();
        std::fs::write(profiles.join(format!("{name}.profile")), text).unwrap();
    }

    fn switching_agent(
        dir: &Scratch,
        replies: Vec<Result<Reply, Box<ProviderError>>>,
    ) -> (Agent, Requests, Built) {
        let (agent, requests) = stored_agent(dir, replies);
        let built = Rc::new(RefCell::new(Vec::new()));
        let catalog = scripted_catalog(Some(dir.path().join("profiles")), Rc::clone(&built));
        (agent.with_catalog(catalog), requests, built)
    }

    #[test]
    fn a_model_switch_changes_only_the_requests_after_it() {
        let dir = Scratch::new("agent-switch-model");
        let (mut agent, requests, _) = switching_agent(
            &dir,
            vec![ScriptedProvider::text("one"), ScriptedProvider::text("two")],
        );
        agent.send("first", &mut NoObserver);
        let notes = agent.switch_model("other-model").unwrap();
        assert!(notes[0].contains("cache starts over"));
        agent.send("second", &mut NoObserver);

        let requests = requests.borrow();
        assert_eq!(text_of(&requests[0], "model"), "m");
        assert_eq!(text_of(&requests[1], "model"), "other-model");
        // The history is untouched: the second request starts with the first one's messages.
        let (first, second) = (messages_of(&requests[0]), messages_of(&requests[1]));
        assert_eq!(&second[..first.len()], &first[..]);

        // Each call records what it was sent to, and the chat row shows the latest.
        let store = peek(&dir);
        let models: Vec<String> = store
            .conn()
            .prepare("SELECT model FROM calls ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(models, ["m", "other-model"]);
        let session: String = store
            .conn()
            .query_row("SELECT model FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(session, "other-model");
        let base: String = store
            .conn()
            .query_row("SELECT DISTINCT base_url FROM calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(base, "http://x/v1");
    }

    #[test]
    fn a_model_switch_is_checked_and_warns_about_pinning() {
        let dir = Scratch::new("agent-switch-model-checks");
        let (agent, _) = agent_in(&dir, vec![]);
        let mut agent = agent.with_router(RouterOptions {
            provider_order: vec!["relace/fp4".into()],
            ..RouterOptions::default()
        });
        assert!(agent.switch_model("two words").is_err());
        assert!(agent.switch_model("").is_err());
        assert!(
            agent
                .switch_model("m")
                .unwrap_err()
                .contains("already using")
        );
        assert_eq!(agent.model(), "m");
        let notes = agent.switch_model("x").unwrap();
        assert_eq!(notes.len(), 2);
        assert!(notes[1].contains("relace/fp4"));
    }

    #[test]
    fn a_provider_switch_sends_the_next_request_to_the_new_provider() {
        let dir = Scratch::new("agent-switch-provider");
        write_profile(
            &dir,
            "other",
            "HARNESS_BASE_URL=http://other/v1\nHARNESS_MODEL=other-model\nHARNESS_PROVIDER_ORDER=relace/fp4\nHARNESS_ALLOW_FALLBACKS=false\n",
        );
        let (mut agent, old_requests, built) =
            switching_agent(&dir, vec![ScriptedProvider::text("from old")]);
        agent.send("first", &mut NoObserver);
        agent.switch_provider("other").unwrap();
        assert_eq!(agent.model(), "other-model");
        assert_eq!(agent.target().name, "other");
        assert_eq!(agent.target().base_url, "http://other/v1");
        assert_eq!(agent.router().provider_order, ["relace/fp4"]);
        let outcome = agent.send("second", &mut NoObserver);
        assert_eq!(outcome, Outcome::Answer("from other".into()));

        assert_eq!(
            old_requests.borrow().len(),
            1,
            "the old provider got no more"
        );
        let built = built.borrow();
        assert_eq!(built.len(), 1);
        assert_eq!(built[0].0, "http://other/v1");
        let sent = built[0].1.borrow();
        assert_eq!(sent.len(), 1);
        let value: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
        assert_eq!(value["model"], "other-model");
        assert_eq!(value["provider"]["order"][0], "relace/fp4");
        // The whole earlier conversation went along.
        assert_eq!(value["messages"].as_array().unwrap().len(), 4);

        let store = peek(&dir);
        let urls: Vec<String> = store
            .conn()
            .prepare("SELECT base_url FROM calls ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(urls, ["http://x/v1", "http://other/v1"]);
    }

    #[test]
    fn a_resume_compares_with_what_the_chat_used_last() {
        let dir = Scratch::new("agent-switch-resume");
        write_profile(
            &dir,
            "other",
            "HARNESS_BASE_URL=http://other/v1\nHARNESS_MODEL=other-model\n",
        );
        let (mut agent, _, _) = switching_agent(&dir, vec![ScriptedProvider::text("a")]);
        agent.send("q", &mut NoObserver);
        agent.switch_provider("other").unwrap();
        agent.send("q2", &mut NoObserver);
        drop(agent);

        // A new program on the first provider resumes the chat: both differences are named.
        let (mut again, _) = stored_agent(&dir, vec![]);
        let report = again.resume(None).unwrap();
        assert_eq!(report.differences.len(), 2, "{:?}", report.differences);
        assert!(report.differences[0].contains("was other-model, now m"));
        assert!(report.differences[1].contains("http://other/v1"));
    }

    #[test]
    fn a_failed_provider_switch_changes_nothing() {
        let dir = Scratch::new("agent-switch-provider-fails");
        write_profile(&dir, "broken", "HARNESS_MODEL=m\n");
        let (mut agent, _, built) = switching_agent(&dir, vec![]);
        let before = (agent.model().to_owned(), agent.target().clone());
        let unknown = agent.switch_provider("nope").unwrap_err();
        assert!(unknown.contains("no provider named nope"), "{unknown}");
        let broken = agent.switch_provider("broken").unwrap_err();
        assert!(broken.contains("HARNESS_BASE_URL is missing"), "{broken}");
        assert_eq!((agent.model().to_owned(), agent.target().clone()), before);
        assert!(built.borrow().is_empty(), "no provider was built");
    }

    #[test]
    fn without_a_catalog_nothing_can_be_switched() {
        let dir = Scratch::new("agent-no-catalog");
        let (mut agent, _) = agent_in(&dir, vec![]);
        assert!(agent.providers().err().unwrap().contains("cannot switch"));
        assert!(agent.switch_provider("x").is_err());
        // The model can still be changed.
        assert!(agent.switch_model("y").is_ok());
    }

    fn saved(name: &str, key: Option<&str>) -> ProviderDef {
        ProviderDef {
            name: name.into(),
            base_url: format!("http://{name}/v1"),
            model: format!("{name}-model"),
            api_key: key.map(str::to_owned),
            router: RouterOptions::default(),
        }
    }

    #[test]
    fn a_saved_provider_is_listed_switched_to_and_removed() {
        let dir = Scratch::new("agent-saved-provider");
        write_profile(
            &dir,
            "fileprofile",
            "HARNESS_BASE_URL=http://f/v1\nHARNESS_MODEL=fm\n",
        );
        let (mut agent, _, built) = switching_agent(&dir, vec![]);
        agent.add_provider(saved("zeta", Some("sk-1"))).unwrap();
        agent.add_provider(saved("alpha", None)).unwrap();
        let listed = agent.providers().unwrap();
        let summary: Vec<(String, Source)> =
            listed.iter().map(|l| (l.name.clone(), l.source)).collect();
        assert_eq!(
            summary,
            [
                ("alpha".to_owned(), Source::Saved),
                ("fileprofile".to_owned(), Source::Profile),
                ("zeta".to_owned(), Source::Saved)
            ]
        );
        agent.switch_provider("zeta").unwrap();
        assert_eq!(agent.model(), "zeta-model");
        assert_eq!(built.borrow().len(), 1);
        // Another program sees the saved ones too (they live in the database).
        assert_eq!(peek(&dir).list_providers().unwrap().len(), 2);

        agent.remove_provider("alpha").unwrap();
        assert_eq!(agent.providers().unwrap().len(), 2);
        assert!(
            agent
                .remove_provider("alpha")
                .unwrap_err()
                .contains("no provider")
        );
        assert!(
            agent
                .remove_provider("fileprofile")
                .unwrap_err()
                .contains("profile file")
        );
    }

    #[test]
    fn a_provider_name_cannot_be_reused_or_made_unsafe() {
        let dir = Scratch::new("agent-provider-names");
        write_profile(
            &dir,
            "taken",
            "HARNESS_BASE_URL=http://f/v1\nHARNESS_MODEL=fm\n",
        );
        let (mut agent, _, _) = switching_agent(&dir, vec![]);
        assert!(
            agent
                .add_provider(saved("taken", None))
                .unwrap_err()
                .contains("already used")
        );
        agent.add_provider(saved("mine", None)).unwrap();
        assert!(
            agent
                .add_provider(saved("mine", None))
                .unwrap_err()
                .contains("already used")
        );
        assert!(agent.add_provider(saved("../x", None)).is_err());
        assert!(agent.add_provider(saved("add", None)).is_err());
        let mut bad = saved("ok", None);
        bad.base_url = "nonsense".into();
        assert!(agent.add_provider(bad).unwrap_err().contains("http"));
    }

    #[test]
    fn a_saved_key_never_reaches_a_request_the_log_or_the_chat_tables() {
        let dir = Scratch::new("agent-key-stays-put");
        let (mut agent, _, built) = switching_agent(&dir, vec![]);
        agent
            .add_provider(saved("keyed", Some("SENTINEL-KEY-12345")))
            .unwrap();
        agent.switch_provider("keyed").unwrap();
        agent.send("hello", &mut NoObserver);
        let built = built.borrow();
        for request in built[0].1.borrow().iter() {
            assert!(!String::from_utf8_lossy(request).contains("SENTINEL"));
        }
        let log = std::fs::read_to_string(agent.log_path()).unwrap();
        assert!(!log.contains("SENTINEL"));
        let store = peek(&dir);
        for (table, columns) in [
            ("messages", ["json", "text"].as_slice()),
            (
                "calls",
                [
                    "response",
                    "error",
                    "model",
                    "base_url",
                    "served_by_provider",
                ]
                .as_slice(),
            ),
            (
                "sessions",
                ["title", "model", "base_url", "router_options", "cwd"].as_slice(),
            ),
            ("system_prompts", ["text"].as_slice()),
        ] {
            let test = columns
                .iter()
                .map(|c| format!("COALESCE({c}, '') LIKE '%SENTINEL%'"))
                .collect::<Vec<_>>()
                .join(" OR ");
            assert_eq!(
                rows(
                    &store,
                    &format!("SELECT COUNT(*) FROM {table} WHERE {test}")
                ),
                0,
                "{table}"
            );
        }
        // It is stored only where it belongs: in the provider row.
        let key: String = store
            .conn()
            .query_row(
                "SELECT api_key FROM providers WHERE name = 'keyed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(key, "SENTINEL-KEY-12345");
    }

    #[test]
    fn the_models_come_from_the_provider_sorted() {
        let dir = Scratch::new("agent-models");
        let provider =
            ScriptedProvider::new(vec![]).with_models(Ok(vec!["b".into(), "a".into(), "b".into()]));
        let log = TurnLog::create(&dir.path().join("logs")).unwrap();
        let agent = Agent::new("m", Box::new(provider), vec![], log);
        assert_eq!(agent.list_models().unwrap(), ["a", "b"]);
        let (silent, _) = agent_in(&dir, vec![]);
        assert!(silent.list_models().unwrap_err().contains("cannot list"));
    }

    // ---- starting without a provider, and remembering the last one (issue 10) ----

    /// An agent that has no provider yet, saving into the scratch folder, able to switch.
    fn unconfigured_agent(dir: &Scratch) -> (Agent, Built) {
        let log = TurnLog::create(&dir.path().join("logs")).unwrap();
        let built = Rc::new(RefCell::new(Vec::new()));
        let agent = Agent::new("", Box::new(crate::provider::NoProvider), vec![], log)
            .with_catalog(scripted_catalog(
                Some(dir.path().join("profiles")),
                Rc::clone(&built),
            ))
            .with_store(Store::open(&db_path(dir)).unwrap(), context());
        (agent, built)
    }

    #[test]
    fn nothing_is_sent_or_saved_before_a_provider_is_chosen() {
        let dir = Scratch::new("agent-no-provider");
        let (mut agent, _) = unconfigured_agent(&dir);
        assert!(!agent.ready());
        let outcome = agent.send("hello", &mut NoObserver);
        let Outcome::Failed(why) = outcome else {
            panic!("expected a failure, got {outcome:?}")
        };
        assert!(why.contains("no provider selected"), "{why}");
        assert!(agent.history().len() == 1, "only the system message");
        assert_eq!(rows(&peek(&dir), "SELECT COUNT(*) FROM sessions"), 0);
        assert!(
            agent
                .switch_model("x")
                .unwrap_err()
                .contains("no provider selected")
        );
        assert!(
            agent
                .list_models()
                .unwrap_err()
                .contains("no provider selected")
        );
    }

    #[test]
    fn the_first_switch_makes_the_agent_ready_without_a_cache_warning() {
        let dir = Scratch::new("agent-first-switch");
        write_profile(
            &dir,
            "first",
            "HARNESS_BASE_URL=http://first/v1\nHARNESS_MODEL=fm\n",
        );
        let (mut agent, _) = unconfigured_agent(&dir);
        let notes = agent.switch_provider("first").unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        assert!(agent.ready());
        assert_eq!(agent.model(), "fm");
        assert_eq!(
            agent.send("hello", &mut NoObserver),
            Outcome::Answer("from first".into())
        );
        // A second switch is a real change and warns.
        write_profile(
            &dir,
            "second",
            "HARNESS_BASE_URL=http://second/v1\nHARNESS_MODEL=sm\n",
        );
        assert_eq!(agent.switch_provider("second").unwrap().len(), 1);
    }

    #[test]
    fn the_last_provider_switched_to_is_the_one_a_new_start_uses() {
        let dir = Scratch::new("agent-remembered");
        write_profile(
            &dir,
            "one",
            "HARNESS_BASE_URL=http://one/v1\nHARNESS_MODEL=m1\n",
        );
        write_profile(
            &dir,
            "two",
            "HARNESS_BASE_URL=http://two/v1\nHARNESS_MODEL=m2\n",
        );
        let (mut agent, _) = unconfigured_agent(&dir);
        assert_eq!(
            agent.use_remembered_provider(),
            None,
            "nothing remembered yet"
        );
        assert!(!agent.ready());
        agent.switch_provider("one").unwrap();
        agent.switch_provider("two").unwrap();
        drop(agent);

        let (mut again, built) = unconfigured_agent(&dir);
        assert_eq!(again.use_remembered_provider(), None);
        assert!(again.ready());
        assert_eq!(again.target().name, "two");
        assert_eq!(again.model(), "m2");
        assert_eq!(built.borrow().len(), 1);
    }

    #[test]
    fn a_remembered_provider_that_is_gone_or_broken_starts_with_none() {
        let dir = Scratch::new("agent-remembered-gone");
        write_profile(
            &dir,
            "gone",
            "HARNESS_BASE_URL=http://g/v1\nHARNESS_MODEL=gm\n",
        );
        let (mut agent, _) = unconfigured_agent(&dir);
        agent.switch_provider("gone").unwrap();
        drop(agent);

        std::fs::remove_file(dir.path().join("profiles/gone.profile")).unwrap();
        let (mut again, _) = unconfigured_agent(&dir);
        assert_eq!(again.use_remembered_provider(), None);
        assert!(!again.ready());

        write_profile(&dir, "gone", "HARNESS_MODEL=gm\n");
        let note = again.use_remembered_provider().unwrap();
        assert!(
            note.contains("gone") && note.contains("HARNESS_BASE_URL is missing"),
            "{note}"
        );
        assert!(!again.ready());
    }

    #[test]
    fn resuming_with_no_provider_says_which_one_the_chat_used() {
        let dir = Scratch::new("agent-resume-no-provider");
        seed(&dir, "q", "a");
        let (mut agent, _) = unconfigured_agent(&dir);
        let report = agent.resume(None).unwrap();
        assert_eq!(report.differences.len(), 1);
        assert!(report.differences[0].contains("no provider is selected"));
        assert!(report.differences[0].contains("that chat used m at http://x/v1"));
        // The saved messages are loaded even so.
        assert_eq!(agent.history().len(), 3);
    }

    // ---- skills and personality (issue 14) ----

    fn with_prompts(agent: &mut Agent) {
        let store = agent.store_mut().unwrap();
        store
            .add_prompt_of_type("tdd", "", "Write the test first.", None, PromptType::Skill)
            .unwrap();
        store
            .add_prompt_of_type("review", "", "Review for bugs.", None, PromptType::Skill)
            .unwrap();
        store
            .add_prompt_of_type(
                "pirate",
                "",
                "Talk like a pirate.",
                None,
                PromptType::Personality,
            )
            .unwrap();
        store
            .add_prompt_of_type("caveman", "", "Few words.", None, PromptType::Personality)
            .unwrap();
    }

    fn last_message(request: &[u8]) -> serde_json::Value {
        messages_of(request).last().unwrap().clone()
    }

    #[test]
    fn nothing_is_added_when_no_skill_or_personality_is_active() {
        let dir = Scratch::new("agent-extra-none");
        let (mut agent, requests) = stored_agent(&dir, vec![ScriptedProvider::text("hi")]);
        with_prompts(&mut agent);
        agent.send("hello", &mut NoObserver);
        let sent = messages_of(&requests.borrow()[0]);
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1]["content"], "hello");
    }

    fn texts(request: &[u8]) -> Vec<String> {
        messages_of(request)
            .iter()
            .map(|m| m["content"].as_str().unwrap_or("").to_owned())
            .collect()
    }

    #[test]
    fn skills_that_are_on_go_into_the_chat_once_before_the_users_message() {
        let dir = Scratch::new("agent-block-once");
        let (mut agent, requests) = stored_agent(
            &dir,
            vec![ScriptedProvider::text("one"), ScriptedProvider::text("two")],
        );
        with_prompts(&mut agent);
        agent.set_personality(Some("pirate")).unwrap();
        agent.set_skill("tdd", SkillState::On).unwrap();
        agent.send("first", &mut NoObserver);

        let first = messages_of(&requests.borrow()[0]);
        // system, the instructions, the user's message: nothing after it.
        assert_eq!(first.len(), 3);
        assert_eq!(first[1]["role"], "user");
        let block = first[1]["content"].as_str().unwrap();
        assert!(block.starts_with(ACTIVE_MARK));
        assert!(block.contains("## Personality: pirate\nTalk like a pirate."));
        assert!(block.contains("## Skill: tdd\nWrite the test first."));
        assert_eq!(first[2]["content"], "first");
        // It is part of the chat now: in memory and in the database.
        assert_eq!(agent.history().len(), 4);
        assert_eq!(
            rows(
                &peek(&dir),
                "SELECT COUNT(*) FROM messages WHERE json LIKE '%Active instructions%'"
            ),
            1
        );

        // The next message adds no second copy, and everything sent before is a prefix of what
        // is sent now (so the router's cache covers it).
        agent.send("second", &mut NoObserver);
        let requests = requests.borrow();
        let second = messages_of(&requests[1]);
        assert_eq!(second.len(), 5);
        assert_eq!(second[..3], first[..]);
        assert_eq!(second[4]["content"], "second");
        assert_eq!(
            texts(&requests[1])
                .iter()
                .filter(|t| t.starts_with(ACTIVE_MARK))
                .count(),
            1
        );
    }

    #[test]
    fn changing_or_clearing_the_skills_adds_a_newer_block_and_a_failed_message_leaves_none() {
        let dir = Scratch::new("agent-block-changes");
        let (mut agent, requests) = stored_agent(
            &dir,
            vec![
                ScriptedProvider::text("one"),
                ScriptedProvider::text("two"),
                ScriptedProvider::text("three"),
                ScriptedProvider::failure("boom"),
                ScriptedProvider::text("five"),
            ],
        );
        with_prompts(&mut agent);
        agent.set_skill("tdd", SkillState::On).unwrap();
        agent.send("a", &mut NoObserver);
        // A second skill: a new block with both.
        agent.set_skill("review", SkillState::On).unwrap();
        agent.send("b", &mut NoObserver);
        // All off: a block that says so.
        agent.set_skill("tdd", SkillState::Off).unwrap();
        agent.set_skill("review", SkillState::Off).unwrap();
        agent.send("c", &mut NoObserver);
        // Nothing changed, nothing added; a failure leaves no block behind.
        let before = agent.history().len();
        agent.set_skill("tdd", SkillState::On).unwrap();
        assert!(matches!(
            agent.send("d", &mut NoObserver),
            Outcome::Failed(_)
        ));
        assert_eq!(agent.history().len(), before);
        agent.send("e", &mut NoObserver);

        let requests = requests.borrow();
        let blocks = |i: usize| -> Vec<String> {
            texts(&requests[i])
                .into_iter()
                .filter(|t| t.starts_with(ACTIVE_MARK))
                .collect()
        };
        assert_eq!(blocks(0).len(), 1);
        assert_eq!(blocks(1).len(), 2);
        assert!(blocks(1)[1].contains("## Skill: review"));
        assert_eq!(blocks(2).len(), 3);
        assert_eq!(blocks(2)[2], NONE_ACTIVE);
        // The retry after the failure has the same blocks as the failed try plus the new one.
        assert_eq!(blocks(3).len(), 4);
        assert_eq!(blocks(4).len(), 4);
        assert!(blocks(4)[3].contains("## Skill: tdd"));
        // The whole chat from an earlier request is the start of the later one.
        let earlier = texts(&requests[1]);
        assert_eq!(texts(&requests[2])[..earlier.len()], earlier[..]);
    }

    #[test]
    fn a_skill_for_one_message_still_rides_last_and_is_not_kept() {
        let dir = Scratch::new("agent-block-once-only");
        std::fs::write(dir.path().join("a.txt"), "hello from a\n").unwrap();
        let (mut agent, requests) = stored_agent(
            &dir,
            vec![
                ScriptedProvider::tool_call("c1", "read_file", READ_A),
                ScriptedProvider::text("done"),
                ScriptedProvider::text("next"),
            ],
        );
        with_prompts(&mut agent);
        agent.set_skill("review", SkillState::Once).unwrap();
        agent.send("look", &mut NoObserver);
        {
            let requests = requests.borrow();
            let (first, second) = (messages_of(&requests[0]), messages_of(&requests[1]));
            // system, user, then the text for this message only.
            assert_eq!(first.len(), 3);
            assert!(
                first[2]["content"]
                    .as_str()
                    .unwrap()
                    .starts_with("Active instructions for this message only")
            );
            // system, user, assistant call, tool result, then the same text again.
            assert_eq!(second.len(), 5);
            assert_eq!(second[3]["role"], "tool");
            assert_eq!(second[4]["content"], first[2]["content"]);
        }
        // Not in the chat, so not in the database, and gone from the next message.
        assert_eq!(agent.history().len(), 5);
        assert_eq!(
            rows(
                &peek(&dir),
                "SELECT COUNT(*) FROM messages WHERE json LIKE '%Active instructions%'"
            ),
            0
        );
        agent.send("again", &mut NoObserver);
        assert_eq!(last_message(&requests.borrow()[2])["content"], "again");
    }

    #[test]
    fn a_skill_for_one_message_is_used_up_when_it_is_answered_but_not_when_it_fails() {
        let dir = Scratch::new("agent-extra-once");
        let (mut agent, requests) = stored_agent(
            &dir,
            vec![
                ScriptedProvider::failure("boom"),
                ScriptedProvider::text("one"),
                ScriptedProvider::text("two"),
            ],
        );
        with_prompts(&mut agent);
        agent.set_skill("review", SkillState::Once).unwrap();
        assert!(matches!(
            agent.send("a", &mut NoObserver),
            Outcome::Failed(_)
        ));
        assert_eq!(agent.chat_prompts().once, ["review"]);
        agent.send("a", &mut NoObserver);
        assert!(agent.chat_prompts().once.is_empty());
        agent.send("b", &mut NoObserver);
        let requests = requests.borrow();
        let has = |i: usize| {
            last_message(&requests[i])["content"]
                .as_str()
                .unwrap()
                .contains("Skill: review")
        };
        assert!(has(0) && has(1));
        assert_eq!(last_message(&requests[2])["content"], "b");
    }

    #[test]
    fn the_skills_that_are_on_are_saved_with_the_chat_and_come_back_on_resume() {
        let dir = Scratch::new("agent-extra-resume");
        let (mut first, _) = stored_agent(&dir, vec![ScriptedProvider::text("one")]);
        with_prompts(&mut first);
        first.set_personality(Some("caveman")).unwrap();
        first.set_skill("tdd", SkillState::On).unwrap();
        first.set_skill("review", SkillState::Once).unwrap();
        // Nothing is saved before the first message; the state goes in with it.
        assert_eq!(
            rows(&peek(&dir), "SELECT COUNT(*) FROM chat_prompt_state"),
            0
        );
        first.send("hi", &mut NoObserver);
        assert_eq!(
            rows(&peek(&dir), "SELECT COUNT(*) FROM chat_prompt_state"),
            2
        );
        // A change after the chat exists is saved at once.
        first.set_skill("tdd", SkillState::Off).unwrap();
        assert_eq!(
            rows(&peek(&dir), "SELECT COUNT(*) FROM chat_prompt_state"),
            1
        );
        first.set_skill("tdd", SkillState::On).unwrap();
        drop(first);

        let (mut second, requests) = stored_agent(&dir, vec![ScriptedProvider::text("two")]);
        let id = peek(&dir).latest_session(Kind::Worker).unwrap().unwrap().id;
        second.resume(Some(id)).unwrap();
        let state = second.chat_prompts();
        assert_eq!(state.personality.as_deref(), Some("caveman"));
        assert_eq!(state.on, ["tdd"]);
        assert!(state.once.is_empty());
        second.send("again", &mut NoObserver);
        // The first chat put the instructions in when it was sent; nothing is added again.
        let sent = texts(&requests.borrow()[0]);
        let blocks: Vec<&String> = sent.iter().filter(|t| t.starts_with(ACTIVE_MARK)).collect();
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].contains("Personality: caveman") && blocks[0].contains("Skill: tdd"));
        assert!(!blocks[0].contains("Skill: review"));
    }

    #[test]
    fn a_new_chat_starts_clean_and_the_old_chat_keeps_what_it_had() {
        let dir = Scratch::new("agent-extra-new");
        let (mut agent, _) = stored_agent(&dir, vec![ScriptedProvider::text("one")]);
        with_prompts(&mut agent);
        agent.load_prompt().unwrap();
        assert!(agent.chat_prompts().is_empty());
        agent.set_personality(Some("caveman")).unwrap();
        agent.set_skill("review", SkillState::On).unwrap();
        agent.send("hi", &mut NoObserver);
        agent.new_chat().unwrap();
        assert!(agent.chat_prompts().is_empty());
        // The old chat kept what it had.
        let id = peek(&dir).latest_session(Kind::Worker).unwrap().unwrap().id;
        let old = peek(&dir).chat_prompts(id).unwrap();
        assert_eq!(old.personality.as_deref(), Some("caveman"));
        assert_eq!(old.on, ["review"]);
    }

    #[test]
    fn changing_the_personality_warns_only_when_a_reply_exists_and_it_changes() {
        let dir = Scratch::new("agent-extra-warn");
        let (mut agent, _) = stored_agent(&dir, vec![ScriptedProvider::text("one")]);
        with_prompts(&mut agent);
        // Before any reply there is nothing to be troubled by.
        assert_eq!(agent.set_personality(Some("pirate")).unwrap(), None);
        agent.send("hi", &mut NoObserver);
        // The same personality again is not a change.
        assert_eq!(agent.set_personality(Some("pirate")).unwrap(), None);
        assert_eq!(
            agent.set_personality(Some("caveman")).unwrap(),
            Some(PERSONALITY_WARNING)
        );
        assert_eq!(
            agent.set_personality(None).unwrap(),
            Some(PERSONALITY_WARNING)
        );
        assert_eq!(agent.set_personality(None).unwrap(), None);
        assert!(PERSONALITY_WARNING.contains("troublesome"));
    }

    #[test]
    fn a_skill_or_personality_name_must_match_a_prompt_of_that_type() {
        let dir = Scratch::new("agent-extra-names");
        let (mut agent, _) = stored_agent(&dir, vec![]);
        with_prompts(&mut agent);
        assert!(agent.set_skill("nope", SkillState::On).is_err());
        assert!(agent.set_skill("pirate", SkillState::On).is_err());
        assert!(agent.set_skill("base", SkillState::On).is_err());
        assert!(agent.set_personality(Some("tdd")).is_err());
        assert!(agent.set_personality(Some("nope")).is_err());
        assert!(agent.chat_prompts().is_empty());
        let skills = agent.skills().unwrap();
        assert_eq!(
            skills
                .iter()
                .map(|(p, _)| p.name.as_str())
                .collect::<Vec<_>>(),
            ["tdd", "review"]
        );
        assert_eq!(agent.personalities().unwrap().len(), 2);
    }
}

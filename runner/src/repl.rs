//! The terminal side: read a line, run the agent, print what happened.
//!
//! Input and output are passed in (not hard-wired to the keyboard and screen),
//! so tests drive the same code with scripted text.

use std::cell::RefCell;
use std::io::{self, BufRead, Write};
use std::rc::Rc;

use kernel::{Message, ToolCall};

use crate::agent::{Agent, Observer, Outcome};
use crate::cancel::{Cancel, asks_to_cancel};
use crate::config::ApiStyle;
use crate::menu::{DEFAULT_WIDTH, Key, MenuItem, MenuState, Step, parse_key};
use crate::provider::router_host;
use crate::providers::ProviderDef;
use crate::store_prompts::{ACTIVE_MARK, SUMMARY_MARK, summary_body};
use crate::style::Style;
use crate::tools::{Answer, Approver, Ask, Mode, task_id};
use crate::turn_log::utc_label;

/// How wide a collapsed tool line may be when the terminal's width is not known.
const DEFAULT_LINE_WIDTH: usize = 100;

/// How many of the user's messages a resumed chat shows again (with everything after each).
const REPLAYED_USER_MESSAGES: usize = 10;

/// How many saved chats `/sessions` lists.
const LISTED_SESSIONS: usize = 10;

/// Longest start of a chat's first message shown in the list.
const SHOWN_PREVIEW_CHARS: usize = 40;

/// How many model ids `/models` prints before saying how many more there are.
const SHOWN_MODELS: usize = 30;

/// Words that run a command listed under another name; Tab offers them too.
pub(crate) const ALIASES: &[&str] = &["/quit"];

/// What `/init` sends to the model in place of the command. It names no file path with an at-sign,
/// so nothing in it is taken for an attachment.
const INIT_MESSAGE: &str = "Draft context for this project from its own files. Check the context tool first so you do not repeat an entry that is already saved. Read the README (or the nearest thing to one), the build and package files and how the folders are laid out; read_file does it, and bash only to list files. Then propose up to 5 entries with context_propose, kind guide, each about one thing a newcomer needs: how to build and test, where the main code lives, conventions the files state, what the files say must not be done. The quote is the file the text comes from, written as From README.md: and a short excerpt, because these come from the project and not from my words. Propose nothing you did not read in a file. Propose a glossary word with glossary_propose only for a term the files define. When you are done say in one or two lines what you proposed.";

/// The first words of the commands, once each, for the completion list of the input box: the
/// listed commands in their order, then the aliases.
pub fn command_words() -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for (usage, _) in COMMANDS {
        let word = usage.split_whitespace().next().unwrap_or(usage);
        if !words.iter().any(|w| w == word) {
            words.push(word.to_owned());
        }
    }
    for alias in ALIASES {
        if !words.iter().any(|w| w == alias) {
            words.push((*alias).to_owned());
        }
    }
    words
}

/// The chat commands, as `/help` lists them: the command, then what it does.
pub(crate) const COMMANDS: &[(&str, &str)] = &[
    ("/help", "show this list"),
    (
        "/model",
        "show the model in use (a menu of its provider's models in a terminal)",
    ),
    (
        "/model <name>",
        "use another model from the next message on",
    ),
    (
        "/models [text]",
        "list the models the provider offers (only those containing text)",
    ),
    (
        "/provider",
        "list the providers (a menu in a terminal: provider, then model)",
    ),
    (
        "/provider config",
        "manage providers and models: refresh, test, edit, delete (terminal only)",
    ),
    (
        "/provider <name>",
        "switch to a provider (its model and pinning come with it)",
    ),
    (
        "/provider add <name> <base_url> <model> [openai|anthropic]",
        "save a new provider, ask for its API key, switch to it",
    ),
    (
        "/provider remove <name>",
        "delete a saved provider (provider files stay)",
    ),
    (
        "/prompts",
        "the system prompts of the selected profile: put them in or out (a checklist in a terminal; /profile has the rest)",
    ),
    (
        "/prompts config",
        "add, edit, reorder and delete prompts, improve one with the model (terminal only)",
    ),
    (
        "/prompts <on|off|edit|delete> <name>",
        "the same by typing (also without a terminal); changes apply to the next chat",
    ),
    (
        "/prompts add [name [file]]",
        "save a new prompt, typed here or copied from a file",
    ),
    (
        "/prompts show",
        "print the system prompt a new chat would start with",
    ),
    (
        "/profile",
        "the starting setups of a chat (system prompts, skills, personality): pick one for new chats, edit, copy, delete (a menu in a terminal)",
    ),
    (
        "/profile <list|show|use|add|edit|copy|rename|delete> ...",
        "the same by typing (also without a terminal); use applies to the next chat",
    ),
    (
        "/glossary",
        "the words and definitions you keep (a menu in a terminal); the model can look them up and propose changes, which you confirm",
    ),
    (
        "/glossary <show|add|edit|delete> <term>",
        "the same by typing; add and edit read the definition as lines ended by a lone . or in your editor",
    ),
    (
        "/glossary <import|export> <file>",
        "read the word/definition tables of a markdown file, or write them to a new file (below the working directory)",
    ),
    (
        "/context",
        "the rules, decisions and shapes you have said about this project (a menu in a terminal); the model can fetch one by name and propose new ones, which you confirm",
    ),
    (
        "/context <show|add|edit|delete> <name>",
        "the same by typing; add and edit read the text as lines ended by a lone . or in your editor",
    ),
    (
        "/context preload <name> on|off",
        "put an entry in the start of every subagent (shapes are on by default)",
    ),
    (
        "/mcp",
        "switch MCP servers on or off in this chat, like /skills (a terminal shows a checklist, Enter toggles; typed: /mcp on NAME, /mcp off NAME). A chat starts with none on. Once the chat has replies, turning one on makes the router read the chat again, and turning one off keeps its tools listed but refuses the calls",
    ),
    (
        "/mcp config",
        "a menu of the servers: add one by answering questions (name, program, arguments, variables such as an API key), enable, disable, remove, and ask, free or off for each tool (terminal only)",
    ),
    (
        "/mcp toolset [add NAME]",
        "list the ready-made MCP toolsets (rust-docs) and add one with its rules already set",
    ),
    (
        "/mcp <on|off|add|remove|enable|disable|tools|restart|rule> ...",
        "the same by typing (also without a terminal): on NAME, off NAME, add NAME COMMAND [ARGS] [--env N=v] [--timeout S], tools NAME, rule NAME TOOL|* ask|free|off",
    ),
    (
        "/inbox",
        "list what other harnesses have submitted through the MCP server (its default buffer mode) and waits for your decision; nothing is stored until you accept it",
    ),
    (
        "/inbox <show|accept|reject> N [note]",
        "show a submission next to what is stored now with a line diff; accept it (asks first; a stored item that changed meanwhile is reported, not overwritten); or reject it with an optional note",
    ),
    (
        "/tools [name]",
        "switch built-in tools on or off in this chat, like /skills (a terminal shows a checklist; typed: /tools on|off NAME, /tools save for the profile). A tool that is off has no definition in the request. With a name, show that tool in full",
    ),
    (
        "/web [port]",
        "show and edit the prompts, skills, personalities, glossary and context entries, and see the tools, in a browser at http://127.0.0.1:8787/ (port optional); runs on a thread until /web stop or the chat ends",
    ),
    (
        "/skills",
        "the skills of this chat: off, next message only, or on (a checklist in a terminal)",
    ),
    (
        "/skills <on|off|use> <name>",
        "the same by typing; use is for the next message only",
    ),
    (
        "/skills config",
        "add, edit, reorder and delete skills (terminal only); typed: /skills add [name [file]], edit <name>, delete <name>",
    ),
    (
        "/personality [name|none]",
        "choose the personality of this chat, one at a time (a menu in a terminal)",
    ),
    (
        "/personality config",
        "add, edit, reorder and delete personalities (terminal only); typed: /personality add [name [file]], edit <name>, delete <name>",
    ),
    (
        "/approve [manual|auto|forget]",
        "show or choose how file changes are approved: manual asks every time, auto lets the file tools run (bash still asks); remembered (a menu in a terminal); forget clears the \"always\" answers given in this run",
    ),
    (
        "/expand [N|list]",
        "show a tool call in full (the last one, call #N, or a list of this run's calls)",
    ),
    (
        "/info",
        "show the line under the input box: model, approval mode, tokens and context",
    ),
    (
        "/init",
        "have the model read this project (README, build files, layout) and propose context entries and glossary words for it; you confirm each one",
    ),
    (
        "/compact [auto [tokens|off]]",
        "have the model summarise this chat and continue in a new chat that starts with the summary (the old chat stays saved); it also happens before a message when the last request was 80,000 prompt tokens or more: /compact auto shows the limit, /compact auto <tokens> or off changes it",
    ),
    (
        "/new",
        "start a new chat, with the system prompt made from the prompts switched on now",
    ),
    (
        "/resume [id]",
        "continue a saved chat: pick one in a terminal, else the latest or the one with this id",
    ),
    (
        "/sessions",
        "list the saved chats (a menu in a terminal: resume or delete one)",
    ),
    (
        "/subagent [profile] <task>",
        "start a subagent in the background and keep chatting; its answer is added to the chat at your next message. It changes files only when /approve is auto",
    ),
    (
        "/subagents [list|show N|stop N]",
        "the subagents of this run: what they are doing, their calls and answers, or stop one",
    ),
    (
        "/plan [on|off]",
        "plan mode: the model can only look and must answer with a plan; nothing is changed. When it has answered you choose to build it here, build it with a subagent, or discuss it",
    ),
    (
        "/calls [all|short]",
        "short (the default) shows one line for each tool call and its result only when it failed; while the model has a list of steps, it shows the steps instead of the calls. all shows every call and result",
    ),
    (
        "/tasks [show ID|stop ID]",
        "everything running in the background: commands the model started (s1, s2) and subagents (a1, a2); show gives the latest output, stop ends one",
    ),
    ("/exit", "quit (also /quit; Ctrl-D works too)"),
];

/// Whether `word` is shaped like a chat command: a slash and then letters, digits, `-` or `_`
/// only. A path (`/etc/hosts`) has another slash and is a message.
fn looks_like_command(word: &str) -> bool {
    word.strip_prefix('/').is_some_and(|name| {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    })
}

/// One collapsed tool line, the same live and in a replay: `▸ #3 write_file notes/a.txt · 5 bytes`
/// (a replay has no number). `width` is how many columns the line may use.
fn tool_line(
    style: Style,
    number: Option<usize>,
    name: &str,
    arguments: &str,
    width: usize,
) -> String {
    let head = match number {
        Some(n) => format!("▸ #{n} {name}"),
        None => format!("▸ {name}"),
    };
    let room = width.saturating_sub(head.chars().count() + 1).max(20);
    format!(
        "{}\n",
        style.dim(&format!(
            "{head} {}",
            crate::toolview::summary(name, arguments, room)
        ))
    )
}

/// The line after a tool has run: `  ✓ created a.txt (5 bytes)` or, for a failure, `  ✗ error: ...`.
fn result_line(style: Style, result: &str, width: usize) -> String {
    let text = crate::toolview::result_summary(result, width.saturating_sub(4).max(20));
    if crate::toolview::failed(result) {
        format!("{}\n", style.red(&format!("  ✗ {text}")))
    } else {
        format!("{}\n", style.dim(&format!("  ✓ {text}")))
    }
}

/// What runs in the background, as a copy that stays current: the commands the model started and
/// the subagents. It lists them for under the box (issue 42).
#[derive(Clone)]
pub struct TaskView {
    pub(crate) shells: crate::tools::ShellTasks,
    pub(crate) runs: Vec<crate::jobs::RunView>,
}

/// The most rows the list under the box takes.
const TASK_ROWS: usize = 4;

impl TaskView {
    /// One line for each task that is running, such as `● s1  npm run dev · 1m 05s`, at most
    /// [`TASK_ROWS`] and then how many more. Empty when nothing runs.
    pub fn lines(&self, width: usize) -> Vec<String> {
        let room = width.saturating_sub(6).max(20);
        let mut rows: Vec<String> = Vec::new();
        for info in self.shells.list() {
            if info.status != crate::tools::ShellStatus::Running {
                continue;
            }
            let command = info
                .command
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let tail = format!(" · {}", crate::tools::clock(info.elapsed));
            let head = format!("● s{}  ", info.n);
            let space = room.saturating_sub(head.chars().count() + tail.chars().count());
            rows.push(format!(
                "{head}{}{tail}",
                crate::toolview::cut(&command, space.max(8))
            ));
        }
        for run in &self.runs {
            let (running, calls, seconds) = run.progress();
            if !running {
                continue;
            }
            let task = run.task.lines().next().unwrap_or("").trim();
            let tail = format!(
                " · {calls} call{} · {}",
                if calls == 1 { "" } else { "s" },
                crate::tools::clock(std::time::Duration::from_secs(seconds))
            );
            let head = format!("● a{}  {}: ", run.n, run.profile);
            let space = room.saturating_sub(head.chars().count() + tail.chars().count());
            rows.push(format!(
                "{head}{}{tail}",
                crate::toolview::cut(task, space.max(8))
            ));
        }
        if rows.len() > TASK_ROWS {
            let more = rows.len() - TASK_ROWS + 1;
            rows.truncate(TASK_ROWS - 1);
            rows.push(format!("… {more} more (/tasks)"));
        }
        rows
    }
}

/// The checklist as lines to draw over the working box: a header with how far it has got, then each
/// step. At most `max` rows; the rest are counted.
fn steps_block(style: Style, steps: &[crate::tools::Step], max: usize) -> Vec<String> {
    use crate::tools::StepStatus;
    if steps.is_empty() {
        return Vec::new();
    }
    let done = steps
        .iter()
        .filter(|s| s.status == StepStatus::Done)
        .count();
    let mut lines = vec![style.dim(&format!(" steps {done}/{}", steps.len()))];
    let room = max.saturating_sub(1);
    let shown = if steps.len() > room {
        room.saturating_sub(1)
    } else {
        steps.len()
    };
    // When it does not all fit, the finished steps give way first.
    let first_open = steps
        .iter()
        .position(|s| s.status != StepStatus::Done)
        .unwrap_or(0);
    let from = if steps.len() > shown {
        first_open.min(steps.len() - shown)
    } else {
        0
    };
    for step in &steps[from..from + shown] {
        lines.push(match step.status {
            StepStatus::Done => style.green(&format!("  ✓ {}", step.text)),
            StepStatus::Doing => style.bold(&format!("  ▸ {}", step.text)),
            StepStatus::Todo => style.dim(&format!("  ○ {}", step.text)),
        });
    }
    let hidden = steps.len() - shown;
    if hidden > 0 {
        lines.push(style.dim(&format!("  … {hidden} more")));
    }
    lines
}

/// The checklist as it changes: all of it when the steps are new or reworded, else only the steps
/// whose state changed, with how far the list has got.
fn steps_text(style: Style, before: &[crate::tools::Step], now: &[crate::tools::Step]) -> String {
    use crate::tools::StepStatus;
    let line = |step: &crate::tools::Step| match step.status {
        StepStatus::Done => style.green(&format!("  ✓ {}", step.text)),
        StepStatus::Doing => style.bold(&format!("  ▸ {}", step.text)),
        StepStatus::Todo => style.dim(&format!("  ○ {}", step.text)),
    };
    let same_texts =
        before.len() == now.len() && before.iter().zip(now).all(|(a, b)| a.text == b.text);
    let done = now.iter().filter(|s| s.status == StepStatus::Done).count();
    let mut text = String::new();
    if same_texts {
        for (old, new) in before.iter().zip(now) {
            if old.status != new.status {
                text.push_str(&format!("{}\n", line(new)));
            }
        }
        if text.is_empty() {
            return text;
        }
        text.push_str(&format!(
            "{}\n",
            style.dim(&format!("  {done}/{} done", now.len()))
        ));
    } else {
        text.push_str(&format!("{}\n", style.dim("steps:")));
        for step in now {
            text.push_str(&format!("{}\n", line(step)));
        }
    }
    text
}

pub const PROMPT: &str = "> ";

/// The mark at the left of what the owner sent, so it can be told from the answer without colour.
const SENT_BAR: &str = "┃";

/// A message the owner sent, as it stays in the scrollback: every line behind a bar, in cyan,
/// (the callers put a blank line before and after). The model's answer has no bar, so the two
/// differ by shape and spacing and not only by colour.
fn sent_block(style: Style, text: &str) -> String {
    text.lines()
        .map(|line| format!("{} {}", style.cyan(SENT_BAR), style.cyan(line)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One input and one output, shared by the REPL, the observer and the approver
/// so that the approval question and the next prompt read from the same stream.
pub struct Terminal<R, W> {
    input: R,
    output: W,
    style: Style,
    /// Turn echo off while a secret is typed. Only the real program sets this.
    hide_secrets: bool,
    /// Arrow-key menus are on (see [`Terminal::enable_menus`]).
    menus: bool,
    /// Menus switch the terminal to raw mode with `stty` (only on a real terminal).
    menu_stty: bool,
    /// The program that edits a longer text (`$VISUAL` or `$EDITOR`); `None` types it in lines.
    editor: Option<String>,
    /// How many columns of the current row are already printed (the prompt), so a line being
    /// edited is drawn after it.
    col: usize,
    /// The lines typed at the main prompt, oldest first, for Up and Down.
    history: Vec<String>,
    /// The tool calls of this run, for `/expand` (issue 17).
    tools: Vec<crate::toolview::ToolRecord>,
    /// The checklist the model set with `steps` in the message being answered (issue 42).
    steps: Vec<crate::tools::Step>,
    /// `/calls all`: every call and every result on screen, as before issue 42.
    calls_all: bool,
    /// The call line of the call in progress was not printed (a list of steps is showing).
    call_hidden: bool,
    /// The box kept at the bottom while the model works (issue 17).
    sticky: Option<Sticky>,
    /// What runs in the background, listed under the working box (issue 42).
    tasks: Option<TaskView>,
    /// The steps are drawn over the working box (not printed as they change).
    steps_above: bool,
    /// A cancelled message, put back in the box at the next prompt to be edited and sent again.
    restore: Option<String>,
    /// The flag Esc and Ctrl-C set while the model works (issue 18); `None` until the REPL gives it.
    cancel: Option<Cancel>,
    /// Reads the keyboard while the model works, to see a cancel.
    watcher: Option<Watcher>,
    /// The working directory: the `@file` list is made from it and `@path` words are read from it.
    root: std::path::PathBuf,
}

/// The box at the bottom of the screen while the model works: output is printed above it.
struct Sticky {
    /// The info line, made when the work started.
    info: String,
    /// How many rows the box takes on screen.
    rows: usize,
    /// Whether it is on screen now (it is taken off while an approval question is shown).
    visible: bool,
}

/// How many columns the cursor is at after printing `text` from column `col`: escape sequences
/// take none, a newline or carriage return starts the row again.
fn advance_col(mut col: usize, text: &str) -> usize {
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for next in chars.by_ref() {
                        if next.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            }
            '\n' | '\r' => col = 0,
            c if c.is_control() => {}
            _ => col += 1,
        }
    }
    col
}

// Rust note: `Rc<RefCell<..>>` = shared ownership (`Rc`) of something that can be
// changed through a shared handle (`RefCell`, checked at run time). Never hold a
// `borrow_mut()` across a call that might borrow again, or it panics.
pub type Shared<R, W> = Rc<RefCell<Terminal<R, W>>>;

impl<R: BufRead, W: Write> Terminal<R, W> {
    /// A terminal that prints plain text (what tests use).
    pub fn shared(input: R, output: W) -> Shared<R, W> {
        Self::shared_styled(input, output, Style::plain())
    }

    /// A terminal that colours its output with `style`.
    pub fn shared_styled(input: R, output: W, style: Style) -> Shared<R, W> {
        Rc::new(RefCell::new(Self {
            input,
            output,
            style,
            hide_secrets: false,
            menus: false,
            menu_stty: false,
            editor: None,
            col: 0,
            history: Vec::new(),
            tools: Vec::new(),
            steps: Vec::new(),
            tasks: None,
            steps_above: false,
            calls_all: false,
            call_hidden: false,
            sticky: None,
            restore: None,
            cancel: None,
            watcher: None,
            root: std::env::current_dir().unwrap_or_default(),
        }))
    }

    /// Turns on arrow-key menus. `stty` is true on a real terminal, where raw mode is needed to
    /// see single key presses; tests pass false and feed the key bytes through the input.
    pub fn enable_menus(&mut self, stty: bool) {
        self.menus = true;
        self.menu_stty = stty;
    }

    /// The program [`Terminal::read_text`] opens for a longer text; `None` types it in lines.
    pub fn set_editor(&mut self, editor: Option<String>) {
        self.editor = editor.filter(|e| !e.trim().is_empty());
    }

    pub fn menus_enabled(&self) -> bool {
        self.menus
    }

    /// How many columns a collapsed tool line may use.
    fn line_width(&self) -> usize {
        if self.menu_stty {
            terminal_width()
        } else {
            DEFAULT_LINE_WIDTH
        }
    }

    /// From now on [`Terminal::ask_secret`] tries to keep what is typed off the screen.
    pub fn hide_secrets(&mut self) {
        self.hide_secrets = true;
    }

    pub fn style(&self) -> Style {
        self.style
    }

    pub fn output(&self) -> &W {
        &self.output
    }

    /// Prints text and flushes, so a prompt without a newline appears at once.
    pub fn say(&mut self, text: &str) {
        // While the model works, the box stays at the bottom: take it off, print, put it back.
        if let Some(rows) = self.sticky.as_ref().filter(|s| s.visible).map(|s| s.rows) {
            let _ = self
                .output
                .write_all(crate::inputbox::erase(rows).as_bytes());
            let _ = self.output.write_all(text.as_bytes());
            if !text.ends_with('\n') {
                let _ = self.output.write_all(b"\r\n");
            }
            self.draw_sticky();
            self.col = 0;
            return;
        }
        // A closed screen is not worth stopping for here.
        let _ = self.output.write_all(text.as_bytes());
        let _ = self.output.flush();
        self.col = advance_col(self.col, text);
    }

    /// Draws the working box where the cursor is (a fresh row) and leaves the cursor below it.
    /// The steps the model set are over it, and what runs in the background under it.
    fn draw_sticky(&mut self) {
        let size = terminal_size();
        let mut info = match &self.sticky {
            Some(sticky) => sticky.info.clone(),
            None => return,
        };
        if let Some(view) = &self.tasks {
            for line in view.lines(size.1) {
                info.push('\n');
                info.push_str(&line);
            }
        }
        let above = if self.steps_above {
            steps_block(self.style, &self.steps, size.0.saturating_sub(12).max(3))
        } else {
            Vec::new()
        };
        let (shown, rows) =
            crate::inputbox::static_box(self.style, size, "working…", &info, &above);
        if let Some(sticky) = &mut self.sticky {
            sticky.rows = rows;
            sticky.visible = true;
        }
        let _ = self.output.write_all(shown.as_bytes());
        let _ = self.output.flush();
    }

    /// Draws the working box again (the steps or the tasks changed).
    fn refresh_sticky(&mut self) {
        let Some(rows) = self.sticky.as_ref().filter(|s| s.visible).map(|s| s.rows) else {
            return;
        };
        let _ = self
            .output
            .write_all(crate::inputbox::erase(rows).as_bytes());
        self.draw_sticky();
    }

    /// Gives the list of what runs in the background, shown under the working box.
    pub fn set_tasks(&mut self, view: TaskView) {
        self.tasks = Some(view);
    }

    /// The model's steps are over the working box: true while it is drawn.
    fn pin_steps(&self) -> bool {
        self.sticky.is_some() && self.steps_above
    }

    /// The work is ending: the steps that were over the box are printed once, as they stand.
    fn unpin_steps(&mut self) {
        if self.pin_steps() && !self.steps.is_empty() {
            let text = steps_text(self.style, &[], &self.steps);
            self.steps_above = false;
            self.say(&text);
        }
        self.steps_above = false;
    }

    /// Sets the folder `@file` works in (the program's working directory unless told otherwise).
    /// The working directory: files named in a command must be below it.
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    pub fn set_root(&mut self, root: std::path::PathBuf) {
        self.root = root;
    }

    /// Gives the flag that Esc and Ctrl-C set while the model works.
    pub fn set_cancel(&mut self, cancel: Cancel) {
        self.cancel = Some(cancel);
    }

    fn start_watching(&mut self) {
        if let Some(cancel) = &self.cancel {
            self.watcher = Watcher::start(cancel.clone());
        }
    }

    /// Starts keeping a box at the bottom while the model works (a real terminal only).
    pub fn begin_working(&mut self, info: impl Fn(Option<usize>) -> String) {
        if !self.menu_stty {
            return;
        }
        let info = info(Some(terminal_size().1.saturating_sub(3)));
        self.steps_above = true;
        self.sticky = Some(Sticky {
            info,
            rows: 0,
            visible: false,
        });
        self.draw_sticky();
        self.start_watching();
    }

    /// Gives the working box new info text (a model call was answered) and draws it again.
    pub fn refresh_working(&mut self, info: impl Fn(Option<usize>) -> String) {
        if self.sticky.is_none() {
            return;
        }
        let text = info(Some(terminal_size().1.saturating_sub(3)));
        if let Some(sticky) = &mut self.sticky {
            sticky.info = text;
        }
        self.refresh_sticky();
    }

    /// Takes the working box off the screen for good.
    pub fn end_working(&mut self) {
        self.watcher = None;
        if let Some(sticky) = self.sticky.take()
            && sticky.visible
        {
            let _ = self
                .output
                .write_all(crate::inputbox::erase(sticky.rows).as_bytes());
            let _ = self.output.flush();
        }
    }

    /// Takes the working box off the screen until [`Terminal::resume_sticky`] (a question is shown).
    fn suspend_sticky(&mut self) {
        if let Some(sticky) = &mut self.sticky
            && sticky.visible
        {
            sticky.visible = false;
            let _ = self
                .output
                .write_all(crate::inputbox::erase(sticky.rows).as_bytes());
            let _ = self.output.flush();
        }
    }

    fn resume_sticky(&mut self) {
        if self.sticky.as_ref().is_some_and(|s| !s.visible) {
            self.draw_sticky();
        }
    }

    /// Reads one line without its line ending. `None` at end of input. On a real terminal the
    /// line can be edited (arrow keys, word deletion); see [`crate::lineedit`].
    pub fn read_line(&mut self) -> Option<String> {
        self.read_edited_line(false)
    }

    /// Like [`Terminal::read_line`] for the main prompt: Up and Down also bring back the lines
    /// typed there earlier in this run.
    pub fn read_command(&mut self) -> Option<String> {
        self.read_edited_line(true)
    }

    /// Draws the main prompt and reads the next line. At a real terminal the line is typed inside
    /// a box with `info` under it (issue 17); otherwise it follows a plain `> `.
    pub fn read_prompt(
        &mut self,
        info: impl Fn(Option<usize>) -> String,
        wake: impl Fn() -> bool,
    ) -> Prompted {
        let raw = if self.menu_stty {
            // Reads give up after a fifth of a second with nothing typed, so the prompt can notice
            // that a subagent finished.
            RawMode::enter_with(0, 2)
        } else {
            None
        };
        let Some(raw) = raw else {
            let accent = self.style.accent(PROMPT);
            self.say(&accent);
            return self.read_command().map_or(Prompted::Eof, Prompted::Line);
        };
        let root = self.root.clone();
        let restored = self.restore.take().unwrap_or_default();
        let (line, row, woke) = crate::inputbox::read_boxed_waking(
            &restored,
            &mut self.input,
            &mut self.output,
            self.style,
            terminal_size(),
            &self.history.clone(),
            &info,
            &mut || crate::attach::files_under(&root),
            Some(&wake),
        );
        drop(raw);
        // The box goes; what was sent stays in the scrollback as a line of its own, in cyan.
        let _ = self
            .output
            .write_all(crate::inputbox::erase(row).as_bytes());
        let _ = self.output.flush();
        self.col = 0;
        if let Some(typed) = woke {
            // The text typed so far comes back in the next box.
            if !typed.is_empty() {
                self.restore = Some(typed);
            }
            return Prompted::Wake;
        }
        if let Some(sent) = &line
            && !sent.trim().is_empty()
        {
            let echoed = format!("\n{}\n\n", sent_block(self.style, sent));
            self.say(&echoed);
        }
        if let Some(line) = &line
            && !line.trim().is_empty()
            && self.history.last() != Some(line)
        {
            self.history.push(line.clone());
        }
        line.map_or(Prompted::Eof, Prompted::Line)
    }

    /// Keeps what is typed while the model works off the screen (a real terminal only).
    fn quiet(&self) -> Option<EchoOff> {
        if self.menu_stty && self.watcher.is_none() {
            EchoOff::enter()
        } else {
            None
        }
    }

    /// Throws away keys that were pressed before now, so a stray Enter typed while the model
    /// worked cannot answer the question about to be shown (a real terminal only).
    fn discard_pending(&mut self) {
        if !self.menu_stty {
            return;
        }
        let Some(raw) = RawMode::enter_with_min(0) else {
            return;
        };
        while let Ok(buffer) = self.input.fill_buf() {
            let n = buffer.len();
            if n == 0 {
                break;
            }
            self.input.consume(n);
        }
        drop(raw);
    }

    fn read_edited_line(&mut self, with_history: bool) -> Option<String> {
        let raw = if self.menu_stty {
            RawMode::enter()
        } else {
            None
        };
        let Some(raw) = raw else {
            return self.read_plain_line();
        };
        let history = if with_history {
            self.history.clone()
        } else {
            Vec::new()
        };
        let width = terminal_width();
        // A prompt longer than the row has wrapped: the line starts on its last row.
        let line = crate::lineedit::read_edited(
            &mut self.input,
            &mut self.output,
            self.col % width,
            width,
            &history,
        );
        drop(raw);
        self.col = 0;
        if with_history
            && let Some(line) = &line
            && !line.trim().is_empty()
            && self.history.last() != Some(line)
        {
            self.history.push(line.clone());
        }
        line
    }

    /// One line from the terminal's own line input (also used for a secret, which is not drawn).
    fn read_plain_line(&mut self) -> Option<String> {
        self.col = 0;
        let mut line = String::new();
        match self.input.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim_end_matches(['\n', '\r']).to_owned()),
        }
    }
}

/// How many rows of a menu show at once.
const MENU_ROWS: usize = 12;

/// What the main prompt gave back.
pub enum Prompted {
    Line(String),
    /// The end of input.
    Eof,
    /// A subagent finished while nothing was being sent.
    Wake,
}

/// The terminal in raw mode (single keys, no echo) until this is dropped, which puts it back as
/// it was, also when a panic unwinds. Ctrl-C arrives as a key (`-isig`) so it cannot kill the
/// program with the terminal left raw.
struct RawMode {
    saved: String,
}

impl RawMode {
    fn enter() -> Option<Self> {
        Self::enter_with_min(1)
    }

    /// `min` is how many bytes a read waits for: 1 waits for a key, 0 returns at once.
    fn enter_with_min(min: u8) -> Option<Self> {
        Self::enter_with(min, 0)
    }

    /// `time` is the wait of a read in tenths of a second when `min` is 0.
    fn enter_with(min: u8, time: u8) -> Option<Self> {
        let output = std::process::Command::new("stty")
            .arg("-g")
            .stdin(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let saved = String::from_utf8(output.stdout).ok()?.trim().to_owned();
        let set = std::process::Command::new("stty")
            .args([
                "-icanon",
                "-echo",
                "-isig",
                "min",
                &min.to_string(),
                "time",
                &time.to_string(),
            ])
            .stdin(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?;
        set.success().then_some(Self { saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = std::process::Command::new("stty")
            .arg(&self.saved)
            .stdin(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Watches the keyboard while the model works (issue 18). The terminal is in raw mode, so Ctrl-C
/// arrives as a key and nothing typed is drawn; Esc on its own or Ctrl-C sets the cancel flag.
/// Dropping it stops the thread and puts the terminal back.
struct Watcher {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    // Dropped after the thread has been joined (fields drop after `drop` runs).
    _raw: RawMode,
}

impl Watcher {
    fn start(cancel: Cancel) -> Option<Self> {
        use std::sync::atomic::Ordering;
        let raw = RawMode::enter_with_min(0)?;
        let mut tty = std::fs::File::open("/dev/tty").ok()?;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = std::sync::Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            use std::io::Read;
            let mut buffer = [0u8; 64];
            while !seen.load(Ordering::SeqCst) {
                match tty.read(&mut buffer) {
                    Ok(n) if n > 0 => {
                        if asks_to_cancel(&buffer[..n]) {
                            cancel.cancel();
                        }
                    }
                    _ => std::thread::sleep(std::time::Duration::from_millis(20)),
                }
            }
        });
        Some(Self {
            stop,
            thread: Some(thread),
            _raw: raw,
        })
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// No echo of what is typed while this is alive (the model is working). The keys are not lost: they
/// wait to be read. Puts the terminal back as it was when dropped.
struct EchoOff {
    saved: String,
}

impl EchoOff {
    fn enter() -> Option<Self> {
        let output = std::process::Command::new("stty")
            .arg("-g")
            .stdin(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let saved = String::from_utf8(output.stdout).ok()?.trim().to_owned();
        let set = std::process::Command::new("stty")
            .arg("-echo")
            .stdin(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?;
        set.success().then_some(Self { saved })
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        let _ = std::process::Command::new("stty")
            .arg(&self.saved)
            .stdin(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Runs `editor` (a command, possibly with arguments) on a private temporary file holding
/// `initial` and returns the file's text afterwards. The file is deleted.
fn edit_in_editor(editor: &str, initial: &str) -> Result<String, String> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "harness-prompt-{}-{}.md",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("cannot create a temporary file: {e}"))?;
    file.write_all(initial.as_bytes())
        .map_err(|e| format!("cannot write the temporary file: {e}"))?;
    drop(file);
    // The path is passed as an argument, never spliced into the command text.
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(&path)
        .status();
    let text = std::fs::read_to_string(&path);
    let _ = std::fs::remove_file(&path);
    match status {
        Err(e) => Err(format!("cannot run the editor {editor}: {e}")),
        Ok(status) if !status.success() => Err(format!("the editor {editor} exited with {status}")),
        Ok(_) => text.map_err(|e| format!("cannot read the edited text: {e}")),
    }
}

/// The terminal's width in columns, from `stty size`.
fn terminal_width() -> usize {
    terminal_size().1
}

/// The terminal's `(rows, columns)`; 24 by [`DEFAULT_WIDTH`] when it cannot say.
fn terminal_size() -> (usize, usize) {
    let numbers: Vec<usize> = std::process::Command::new("stty")
        .arg("size")
        .stdin(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|text| {
            text.split_whitespace()
                .filter_map(|n| n.parse::<usize>().ok())
                .collect()
        })
        .unwrap_or_default();
    // A terminal that does not know its size reports 0.
    match numbers[..] {
        [rows, cols] if rows > 0 && cols > 0 => (rows, cols),
        _ => (24, DEFAULT_WIDTH),
    }
}

impl<R: BufRead, W: Write> Terminal<R, W> {
    /// The next key from the input, one at a time so the bytes after it stay unread (a menu is
    /// followed by typed lines). `None` at end of input.
    fn read_key(&mut self) -> Option<Key> {
        loop {
            let buffer = self.input.fill_buf().ok()?;
            if buffer.is_empty() {
                return None;
            }
            let (key, used) = parse_key(buffer);
            self.input.consume(used);
            if key.is_some() {
                return key;
            }
        }
    }

    /// Shows a menu and returns the index of the row chosen, or `None` when it was cancelled
    /// (Esc, Ctrl-C, end of input). The menu is erased afterwards, so the caller says what
    /// happened. Call only when [`Terminal::menus_enabled`].
    pub fn pick(&mut self, title: &str, items: &[MenuItem]) -> Option<usize> {
        self.pick_from(title, items, 0)
    }

    /// Like [`Terminal::pick`], with row `start` highlighted first.
    pub fn pick_from(&mut self, title: &str, items: &[MenuItem], start: usize) -> Option<usize> {
        let raw = self.menu_stty.then(RawMode::enter).flatten();
        let width = if raw.is_some() {
            terminal_width()
        } else {
            DEFAULT_WIDTH
        };
        let mut state = MenuState::new(items.to_vec(), MENU_ROWS)
            .with_width(width)
            .starting_at(start);
        let mut drawn = 0;
        self.say("\x1b[?25l");
        let chosen = loop {
            let lines = state.render(title, self.style);
            let mut frame = String::new();
            if drawn > 0 {
                frame.push_str(&format!("\x1b[{drawn}A\r\x1b[J"));
            }
            for line in &lines {
                frame.push_str(line);
                frame.push_str("\r\n");
            }
            self.say(&frame);
            drawn = lines.len();
            let Some(key) = self.read_key() else {
                break None;
            };
            match state.key(key) {
                Step::Continue => {}
                Step::Chosen(index) => break Some(index),
                Step::Cancelled => break None,
            }
        };
        self.say(&format!("\x1b[{drawn}A\r\x1b[J\x1b[?25h"));
        drop(raw);
        chosen
    }

    /// Asks one question on a line and returns the answer without surrounding spaces. `current`
    /// is shown in brackets (Enter alone keeps it, which is the caller's rule). `None` at end of
    /// input.
    pub fn ask(&mut self, question: &str, current: Option<&str>) -> Option<String> {
        let shown = match current {
            Some(value) if !value.is_empty() => {
                format!("{question} [{}]: ", self.style.dim(value))
            }
            _ => format!("{question}: "),
        };
        self.say(&shown);
        self.read_line().map(|line| line.trim().to_owned())
    }

    /// Gets a longer text from the owner. With an editor set, the editor opens on a temporary file
    /// holding `initial` and the saved file is the answer. Without one, the text is typed in lines,
    /// ended by a line holding only `.`. `Ok(None)` when the input ended (a cancel); `Err` when the
    /// editor could not be run or failed.
    pub fn read_text(&mut self, what: &str, initial: &str) -> Result<Option<String>, String> {
        if let Some(editor) = self.editor.clone() {
            self.say(&format!(
                "{}\n",
                self.style.dim(&format!("opening {editor} for {what}..."))
            ));
            return edit_in_editor(&editor, initial).map(Some);
        }
        if !initial.is_empty() {
            self.say(&format!(
                "{}\n{initial}\n{}\n",
                self.style.dim(&format!("current text of {what}:")),
                self.style.dim("(typing replaces it)")
            ));
        }
        self.say(&format!(
            "{}\n",
            self.style.dim(&format!(
                "{what}: type the text, then a line holding only . (the end of input cancels)"
            ))
        ));
        let mut lines: Vec<String> = Vec::new();
        loop {
            match self.read_line() {
                None => return Ok(None),
                Some(line) if line.trim_end() == "." => break,
                Some(line) => lines.push(line),
            }
        }
        Ok(Some(lines.join("\n")))
    }

    /// Asks for something that must not be shown, such as an API key. The typed text is
    /// returned to the caller only: it is never printed, logged or sent to the model. Echo is
    /// turned off with `stty` when this is a real terminal; if that is not possible the prompt
    /// says the input is visible. `None` at end of input.
    pub fn ask_secret(&mut self, question: &str) -> Option<String> {
        let hidden = self.hide_secrets && set_echo(false);
        let note = if hidden { "hidden" } else { "visible" };
        self.say(&format!("{question} ({note}): "));
        let answer = self.read_plain_line();
        if hidden {
            set_echo(true);
            // The Enter key was not echoed either.
            self.say("\n");
        }
        answer
    }
}

/// Turns terminal echo on or off with `stty`. `false` when it could not be done (not a
/// terminal, or no `stty`).
fn set_echo(on: bool) -> bool {
    std::process::Command::new("stty")
        .arg(if on { "echo" } else { "-echo" })
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Asks on the terminal before `bash` runs a command or a file tool changes a file. In a terminal
/// the answer is picked from a menu (yes, always, no; Esc is no); typed, `y` or `yes` approves,
/// `a` or `always` approves and remembers (only when offered), and anything else, or end of input,
/// denies.
pub struct TerminalApprover<R, W>(pub Shared<R, W>);

impl<R: BufRead, W: Write> Approver for TerminalApprover<R, W> {
    fn approve(&self, ask: &Ask) -> Answer {
        // The working box is out of the way while the question is on screen, and the key watcher
        // stops so that the question gets every key.
        let watching = {
            let mut term = self.0.borrow_mut();
            term.suspend_sticky();
            term.watcher.take().is_some()
        };
        let answer = self.ask_inner(ask);
        let mut term = self.0.borrow_mut();
        term.resume_sticky();
        if watching {
            term.start_watching();
        }
        answer
    }
}

impl<R: BufRead, W: Write> TerminalApprover<R, W> {
    fn ask_inner(&self, ask: &Ask) -> Answer {
        let mut term = self.0.borrow_mut();
        let style = term.style();
        term.say(&format!(
            "\n{}\n  {}\n",
            style.bold(ask.question),
            style.yellow(ask.detail)
        ));
        if term.menus_enabled() {
            term.discard_pending();
            loop {
                let mut rows = vec![(Some(Answer::Yes), MenuItem::new("yes"))];
                if let Some(what) = ask.always {
                    rows.push((
                        Some(Answer::Always),
                        MenuItem::new("always").with_detail(format!("allow {what} until you quit")),
                    ));
                }
                if ask.full.is_some() {
                    rows.push((
                        None,
                        MenuItem::new("view").with_detail("show the whole text first"),
                    ));
                }
                rows.push((Some(Answer::No), MenuItem::new("no")));
                let items: Vec<MenuItem> = rows.iter().map(|(_, item)| item.clone()).collect();
                // The cursor starts on "yes", so Enter alone approves; Esc is still "no".
                let picked = term.pick_from("allow it?", &items, 0);
                let answer = match picked {
                    None => Some(Answer::No),
                    Some(index) => rows[index].0,
                };
                let Some(answer) = answer else {
                    // "view": show the text, then ask again.
                    let text = ask.full.unwrap_or("");
                    term.say(&format!("{}\n", style.dim(text.trim_end_matches('\n'))));
                    continue;
                };
                let shown = match answer {
                    Answer::Yes => style.green("  yes"),
                    Answer::Always => style.green(&format!(
                        "  always: {} are allowed until you quit",
                        ask.always.unwrap_or("these")
                    )),
                    Answer::No => style.red("  no"),
                };
                term.say(&format!("{shown}\n"));
                return answer;
            }
        }
        if let Some(what) = ask.always {
            term.say(&format!(
                "{}\n",
                style.dim(&format!("  a = always allow {what} until you quit"))
            ));
        }
        term.say(&format!(
            "{} ",
            style.bold(if ask.always.is_some() {
                "[y/a/N]"
            } else {
                "[y/N]"
            })
        ));
        let line = term.read_line().unwrap_or_default();
        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => Answer::Yes,
            "a" | "always" if ask.always.is_some() => Answer::Always,
            _ => Answer::No,
        }
    }
}

/// Prints each tool call as the agent starts it.
pub struct PrintObserver<R, W>(pub Shared<R, W>);

impl<R: BufRead, W: Write> Observer for PrintObserver<R, W> {
    fn call_answered(&mut self, agent: &Agent) {
        self.0
            .borrow_mut()
            .refresh_working(|width| info_text(agent, width));
    }

    fn tool_call(&mut self, call: &ToolCall) {
        let mut term = self.0.borrow_mut();
        let style = term.style();
        term.tools.push(crate::toolview::ToolRecord {
            name: call.name.clone(),
            arguments: call.arguments.clone(),
            result: None,
        });
        term.call_hidden = false;
        if call.name == "steps"
            && let Ok(now) = crate::tools::parse_steps(&call.arguments)
        {
            term.call_hidden = true;
            if term.pin_steps() {
                // Over the working box, not in the scrollback.
                term.steps = now;
                term.refresh_sticky();
                return;
            }
            let text = steps_text(style, &term.steps, &now);
            term.steps = now;
            term.say(&text);
            return;
        }
        // While a list of steps is unfinished its calls are not shown, unless one fails.
        if !term.calls_all && crate::tools::unfinished(&term.steps) {
            term.call_hidden = true;
            return;
        }
        let line = tool_line(
            style,
            Some(term.tools.len()),
            &call.name,
            &call.arguments,
            term.line_width(),
        );
        term.say(&line);
    }

    fn tool_result(&mut self, call: &ToolCall, result: &str) {
        let mut term = self.0.borrow_mut();
        let style = term.style();
        if let Some(record) = term.tools.last_mut() {
            record.result = Some(result.to_owned());
        }
        let failed = crate::toolview::failed(result);
        // A call that went well is one line (the call). Its result is shown when it failed, or
        // with `/calls all`.
        if !failed && !term.calls_all {
            return;
        }
        let mut text = String::new();
        if term.call_hidden {
            text.push_str(&tool_line(
                style,
                Some(term.tools.len()),
                &call.name,
                &call.arguments,
                term.line_width(),
            ));
        }
        text.push_str(&result_line(style, result, term.line_width()));
        term.say(&text);
    }

    fn warning(&mut self, text: &str) {
        let style = self.0.borrow().style();
        self.0.borrow_mut().say(&format!(
            "{}\n",
            style.yellow(&format!("[warning: {text}]"))
        ));
    }

    /// A background subagent finished, or its answer was added to the chat.
    fn subagent_note(&mut self, text: &str) {
        let mut term = self.0.borrow_mut();
        let style = term.style();
        term.say(&format!("{}\n", style.dim(text)));
    }
}

/// A message the program added with the answers of subagents (`jobs::delivery_text`): one short
/// line for each answer, so a replay does not print their whole text as if the owner had typed it.
fn delivery_lines(content: &str) -> Option<Vec<String>> {
    const MARK: &str = ". This text is from a subagent, not from the user:";
    if !content.starts_with("Subagent #") {
        return None;
    }
    let mut lines = Vec::new();
    for part in content.split("\n\n---\n\n") {
        let (header, body) = part.split_once("\n\n").unwrap_or((part, ""));
        let header = header.strip_suffix(MARK)?;
        lines.push(format!(
            "[{}: answer added to the chat, {} characters]",
            header.replacen("Subagent", "subagent", 1),
            body.chars().count()
        ));
    }
    Some(lines)
}

/// A saved "active instructions" message as one line: which personality and skills it holds.
fn instructions_line(block: &str) -> String {
    let personality = block
        .lines()
        .find_map(|l| l.strip_prefix("## Personality: "));
    let skills: Vec<&str> = block
        .lines()
        .filter_map(|l| l.strip_prefix("## Skill: "))
        .collect();
    let mut parts = Vec::new();
    if let Some(name) = personality {
        parts.push(format!("personality {name}"));
    }
    if !skills.is_empty() {
        parts.push(format!("skills {}", skills.join(", ")));
    }
    if parts.is_empty() {
        "[instructions: none active]".to_owned()
    } else {
        format!("[instructions: {}]", parts.join("; "))
    }
}

/// Shows a resumed chat on screen the way the live chat showed it: your lines after `> `, the
/// model's answers, and a collapsed `▸` line for each tool call. Tool results and the system prompt
/// are not shown (they were not on screen live either). Only the last
/// [`REPLAYED_USER_MESSAGES`] of your messages, with what followed each, are shown.
pub fn show_history<R: BufRead, W: Write>(term: &Shared<R, W>, history: &[Message]) {
    let messages: Vec<&Message> = history
        .iter()
        .filter(|m| !matches!(m, Message::System(_)))
        .collect();
    if messages.is_empty() {
        return;
    }
    let user_positions: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            matches!(m, Message::User(t) if !t.starts_with(ACTIVE_MARK) && !t.starts_with(SUMMARY_MARK))
        })
        .map(|(position, _)| position)
        .collect();
    let start = user_positions
        .len()
        .checked_sub(REPLAYED_USER_MESSAGES)
        .map_or(0, |first| user_positions[first]);

    let style = term.borrow().style();
    let width = term.borrow().line_width();
    let mut text = String::new();
    if start > 0 {
        text.push_str(&format!(
            "{}\n",
            style.dim(&format!("[{start} earlier messages not shown]"))
        ));
    }
    for message in &messages[start..] {
        match message {
            Message::User(content) if content.starts_with(ACTIVE_MARK) => {
                text.push_str(&format!("{}\n", style.dim(&instructions_line(content))));
            }
            Message::User(content) if content.starts_with(SUMMARY_MARK) => {
                let chars = summary_body(content).chars().count();
                text.push_str(&format!(
                    "{}\n",
                    style.dim(&format!("[summary of an earlier chat: {chars} characters]"))
                ));
            }
            Message::User(content) if delivery_lines(content).is_some() => {
                for line in delivery_lines(content).unwrap_or_default() {
                    text.push_str(&format!("{}\n", style.dim(&line)));
                }
            }
            Message::User(content) if content == crate::agent::WAKE_TEXT => {}
            Message::User(content) => {
                text.push_str(&format!("\n{}\n\n", sent_block(style, content)));
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                // Text that comes with a tool call was not shown live, so it is not shown now.
                if tool_calls.is_empty()
                    && let Some(content) = content
                {
                    text.push_str(&format!(
                        "{}\n",
                        crate::markdown::render(style, content, width)
                    ));
                }
                for call in tool_calls {
                    text.push_str(&tool_line(style, None, &call.name, &call.arguments, width));
                }
            }
            Message::Tool { .. } | Message::System(_) => {}
        }
    }
    text.push_str(&format!("{}\n", style.dim("[end of saved chat]")));
    term.borrow_mut().say(&text);
}

/// `/compact`, `/compact auto`, `/compact auto <tokens>` and `/compact auto off` (issue 24).
fn compact_command<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, rest: &str) {
    let style = term.borrow().style();
    let mut words = rest.split_whitespace();
    match (words.next(), words.next(), words.next()) {
        (None, ..) => {
            run_compact(agent, term, true);
        }
        (Some("auto"), None, _) => {
            let said = match agent.auto_compact_at() {
                Some(limit) => format!(
                    "automatic compaction: before a message when the last request was {} prompt tokens or more",
                    crate::usage::thousands(limit)
                ),
                None => "automatic compaction: off".to_owned(),
            };
            say_line(term, &said);
        }
        (Some("auto"), Some(value), None) => {
            let limit = if value == "off" {
                Ok(None)
            } else {
                value
                    .replace(',', "")
                    .parse::<u64>()
                    .map(Some)
                    .map_err(|_| format!("'{value}' is not a number of tokens or off"))
            };
            let done = limit.and_then(|limit| {
                agent.set_auto_compact(limit)?;
                Ok(limit)
            });
            match done {
                Ok(Some(limit)) => say_line(
                    term,
                    &style.green(&format!(
                        "automatic compaction at {} prompt tokens",
                        crate::usage::thousands(limit)
                    )),
                ),
                Ok(None) => say_line(term, &style.green("automatic compaction is off")),
                Err(why) => say_line(term, &style.yellow(&why)),
            }
        }
        _ => say_line(
            term,
            &style.yellow(
                "usage: /compact | /compact auto | /compact auto <tokens> | /compact auto off",
            ),
        ),
    }
}

/// Runs one compaction with the working box up and says what happened. `Esc` cancels it.
/// Returns `true` when the user cancelled it.
fn run_compact<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, asked: bool) -> bool {
    let style = term.borrow().style();
    term.borrow_mut()
        .begin_working(|width| info_text(agent, width));
    let quiet = term.borrow().quiet();
    let done = agent.compact();
    drop(quiet);
    term.borrow_mut().end_working();
    match done {
        Ok(report) => {
            say_line(
                term,
                &style.green(&format!(
                    "[compacted: {} messages became a summary of {} characters; the earlier chat is saved]",
                    report.messages, report.summary_chars
                )),
            );
            false
        }
        Err(why) => {
            let cancelled = why == "cancelled";
            let note = if asked {
                format!("[not compacted: {why}]")
            } else if cancelled {
                format!("[not compacted: {why}; the message was not sent]")
            } else {
                format!("[not compacted: {why}; sending the message as it is]")
            };
            say_line(term, &style.yellow(&note));
            cancelled
        }
    }
}

/// The start of a chat's first message on one line, in quotes, to recognise it by.
pub(crate) fn preview(first_message: Option<&str>) -> String {
    let Some(text) = first_message else {
        return String::new();
    };
    // Rust note: `split_whitespace` then `join(" ")` turns any run of spaces and newlines
    // into a single space.
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut shown: String = one_line.chars().take(SHOWN_PREVIEW_CHARS).collect();
    if one_line.chars().count() > SHOWN_PREVIEW_CHARS {
        shown.push_str("...");
    }
    format!("\"{shown}\"")
}

/// What identifies a saved chat in a list: the first thing the user typed, in quotes, with
/// `(continued)` first when the chat began from a compaction summary.
pub(crate) fn chat_label(chat: &crate::store::SessionSummary) -> String {
    let text = preview(chat.first_message.as_deref());
    match (chat.continued, text.is_empty()) {
        (false, _) => text,
        (true, true) => "(continued)".to_owned(),
        (true, false) => format!("(continued) {text}"),
    }
}

/// `/sessions`: lists the newest saved chats.
fn show_sessions<R: BufRead, W: Write>(agent: &Agent, term: &Shared<R, W>) {
    let style = term.borrow().style();
    let text = match agent.sessions(LISTED_SESSIONS) {
        Err(why) => format!("{}\n", style.red(&format!("[error: {why}]"))),
        Ok(list) if list.is_empty() => format!("{}\n", style.dim("no saved chats yet")),
        Ok(list) => {
            let current = agent.current_session();
            let mut text = format!("{}\n", style.bold("saved chats, newest first:"));
            for chat in &list {
                let this_chat = if Some(chat.id) == current {
                    format!("  {}", style.green("(this chat)"))
                } else {
                    String::new()
                };
                text.push_str(&format!(
                    "  {}  {}  {:>3} messages  {}  {}{this_chat}\n",
                    style.bold(&format!("{:>3}", chat.id)),
                    style.dim(&utc_label(u64::try_from(chat.updated_at).unwrap_or(0))),
                    chat.message_count,
                    chat.model,
                    chat_label(chat),
                ));
            }
            text.push_str(&format!(
                "{}\n",
                style.dim("continue one with /resume <id>, or /resume for the latest")
            ));
            text
        }
    };
    term.borrow_mut().say(&text);
}

/// `/resume` or `/resume <id>`: switches to a saved chat and shows it. With no id, a terminal
/// gets a picker and anything else gets the latest chat.
fn resume_command<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, line: &str) {
    let style = term.borrow().style();
    let argument = line.strip_prefix("/resume").unwrap_or_default().trim();
    let id = if argument.is_empty() {
        if term.borrow().menus_enabled() {
            crate::sessions_ui::resume_menu(agent, term);
            return;
        }
        None
    } else if let Ok(id) = argument.parse::<i64>() {
        Some(id)
    } else {
        term.borrow_mut().say(&format!(
            "{}\n",
            style.yellow("usage: /resume [id]   (the ids are listed by /sessions)")
        ));
        return;
    };
    let resumed = agent.resume(id);
    report_resume(agent, term, resumed);
}

/// Says what a resume did: the headline, what differs, and the saved chat shown again; or why
/// it did not happen.
pub(crate) fn report_resume<R: BufRead, W: Write>(
    agent: &Agent,
    term: &Shared<R, W>,
    resumed: Result<crate::agent::ResumeReport, String>,
) {
    let style = term.borrow().style();
    match resumed {
        Err(why) => term
            .borrow_mut()
            .say(&format!("{}\n", style.red(&format!("[error: {why}]")))),
        Ok(report) => {
            let mut text = format!("{}\n", style.green(&report.headline()));
            let notes = report.notes();
            if !notes.is_empty() {
                text.push_str(&format!("{}\n", style.yellow(notes.trim_end_matches('\n'))));
            }
            term.borrow_mut().say(&text);
            show_history(term, agent.history());
        }
    }
}

/// `/help`: every command with one line each.
fn help_text(style: Style) -> String {
    let width = COMMANDS.iter().map(|(c, _)| c.len()).max().unwrap_or(0);
    let mut text = format!("{}\n", style.bold("commands:"));
    for (command, what) in COMMANDS {
        text.push_str(&format!(
            "  {}  {}\n",
            style.accent(&format!("{command:<width$}")),
            style.dim(what)
        ));
    }
    text.push_str(&format!(
        "{}\n",
        style.dim(
            "anything else you type goes to the model; a word like /foo that is not a command is refused (a path such as /etc/hosts is fine)"
        )
    ));
    text
}

pub(crate) fn say_line<R: BufRead, W: Write>(term: &Shared<R, W>, text: &str) {
    term.borrow_mut().say(&format!("{text}\n"));
}

/// "name (host)" for the provider in use.
pub(crate) fn target_label(agent: &Agent) -> String {
    let target = agent.target();
    let host = router_host(&target.base_url).unwrap_or_default();
    match (target.name.is_empty(), host.is_empty()) {
        (false, false) if target.name != host => format!("{} ({host})", target.name),
        (false, _) => target.name.clone(),
        (true, _) => host,
    }
}

pub(crate) fn warnings<R: BufRead, W: Write>(
    term: &Shared<R, W>,
    style: Style,
    warnings: &[String],
) {
    for warning in warnings {
        say_line(term, &style.yellow(&format!("warning: {warning}")));
    }
}

/// `/model` shows the model in use; `/model <name>` changes it for the next message.
fn model_command<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, argument: &str) {
    let style = term.borrow().style();
    if argument.is_empty() && term.borrow().menus_enabled() {
        crate::config_ui::model_menu(agent, term);
        return;
    }
    if argument.is_empty() {
        if !agent.ready() {
            say_line(term, &style.yellow(crate::provider::CHOOSE_A_PROVIDER));
            return;
        }
        say_line(
            term,
            &format!(
                "{}{}\n{}{}",
                style.dim("model:    "),
                style.bold(agent.model()),
                style.dim("provider: "),
                target_label(agent)
            ),
        );
        return;
    }
    if argument.split_whitespace().count() > 1 {
        say_line(
            term,
            &style.yellow("usage: /model <name>   (a model id has no spaces)"),
        );
        return;
    }
    let switched = agent.switch_model(argument);
    report_model_switch(agent, term, switched);
}

/// The info line under the input box (also `/info`): model, provider, approval mode, tokens.
fn info_text(agent: &Agent, width: Option<usize>) -> String {
    let mode = agent.approval_mode();
    let approve = if agent.planning() {
        format!("{} · plan mode", mode.name())
    } else {
        mode.name().to_owned()
    };
    crate::usage::info_line(
        &crate::usage::Info {
            model: agent.ready().then(|| agent.model()),
            provider: &agent.target().name,
            approve: &approve,
            profile: agent.chat_profile(),
            always: agent.always_labels().len(),
            subagents: agent.running_subagents(),
            shells: agent.shells().running(),
            usage: agent.usage(),
        },
        width,
    )
}

/// What is under the input box: the info line, then what runs in the background (issue 42).
fn box_text(agent: &Agent, width: Option<usize>) -> String {
    let mut text = info_text(agent, width);
    for line in agent.task_view().lines(width.map_or(80, |w| w + 3)) {
        text.push('\n');
        text.push_str(&line);
    }
    text
}

/// Before leaving: running subagents stop with the program, so the owner is asked first. True when
/// there is nothing running, the answer is yes, or the input has ended (no one to ask).
fn quit_confirmed<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) -> bool {
    let subagents = agent.running_subagents();
    let commands = agent.shells().running();
    if subagents + commands == 0 {
        return true;
    }
    let style = term.borrow().style();
    let mut parts = Vec::new();
    if subagents > 0 {
        parts.push(format!(
            "{subagents} subagent{}",
            if subagents == 1 { "" } else { "s" }
        ));
    }
    if commands > 0 {
        parts.push(format!(
            "{commands} background command{}",
            if commands == 1 { "" } else { "s" }
        ));
    }
    let what = parts.join(" and ");
    let total = subagents + commands;
    let ask = format!("{what} still running; quitting stops {total}. Quit anyway? [y/N] ");
    term.borrow_mut().say(&style.yellow(&ask));
    // Input that has ended cannot answer, and asking again would never stop: that quits.
    let yes = term
        .borrow_mut()
        .read_line()
        .is_none_or(|a| matches!(a.trim().to_lowercase().as_str(), "y" | "yes"));
    if !yes {
        say_line(term, &style.dim("not quitting; /tasks shows them"));
    }
    yes
}

/// `/subagent [profile] <task>` (issue 35): starts a subagent on a thread and returns at once.
fn subagent_command<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, rest: &str) {
    let style = term.borrow().style();
    match agent.start_subagent(rest) {
        Ok(run) => {
            let kind = if run.coder {
                "coder: it may change files"
            } else {
                "reader: it only looks"
            };
            say_line(
                term,
                &style.green(&format!(
                    "subagent #{} started ({}, {kind}); you can keep chatting, its answer is added at your next message after it finishes (/subagents shows progress)",
                    run.n, run.profile
                )),
            );
        }
        Err(why) => say_line(term, &style.yellow(&why)),
    }
}

/// `/subagents [list | show N | stop N]`.
fn subagents_command<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, rest: &str) {
    let style = term.borrow().style();
    let (sub, arg) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, ""), |(a, b)| (a, b.trim()));
    let number = || arg.trim_start_matches('#').parse::<u32>().ok();
    match sub {
        "" | "list" if arg.is_empty() => {
            let runs = agent.subagent_runs();
            if runs.is_empty() {
                say_line(
                    term,
                    &style.dim("no subagents started in this run (/subagent <task>)"),
                );
                return;
            }
            let width = term.borrow().line_width();
            for run in &runs {
                let kind = if run.coder { "coder" } else { "reader" };
                let head = format!(
                    "#{} {} ({kind}) {} · {} call{} · {}s · ",
                    run.n,
                    run.profile,
                    run.status.name(),
                    run.calls,
                    if run.calls == 1 { "" } else { "s" },
                    run.seconds
                );
                let room = width.saturating_sub(head.chars().count()).max(20);
                let task: String = run
                    .task
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(room)
                    .collect();
                say_line(term, &format!("{}{task}", style.bold(&head)));
            }
        }
        "show" => {
            let Some(n) = number() else {
                say_line(term, &style.yellow("usage: /subagents show <number>"));
                return;
            };
            let Some(run) = agent.subagent_runs().into_iter().find(|r| r.n == n) else {
                say_line(
                    term,
                    &style.yellow(&format!("there is no subagent #{n} (see /subagents)")),
                );
                return;
            };
            let kind = if run.coder { "coder" } else { "reader" };
            say_line(
                term,
                &style.bold(&format!(
                    "#{} {} ({kind}) {} · {} calls · {}s",
                    run.n,
                    run.profile,
                    run.status.name(),
                    run.calls,
                    run.seconds
                )),
            );
            say_line(term, &format!("task: {}", run.task));
            if !run.recent.is_empty() {
                say_line(term, &style.dim("latest calls:"));
                for line in &run.recent {
                    say_line(term, &style.dim(&format!("  ↳ {line}")));
                }
            }
            if let Some(result) = &run.result {
                say_line(term, "answer:");
                let width = term.borrow().line_width();
                say_line(term, &crate::markdown::render(style, result, width));
            }
        }
        "stop" => {
            let Some(n) = number() else {
                say_line(term, &style.yellow("usage: /subagents stop <number>"));
                return;
            };
            match agent.stop_subagent(n) {
                Ok(()) => say_line(
                    term,
                    &style.green(&format!(
                        "stopping subagent #{n}; its answer is not added to the chat"
                    )),
                ),
                Err(why) => say_line(term, &style.yellow(&why)),
            }
        }
        _ => say_line(
            term,
            &style.yellow("usage: /subagents [list | show <number> | stop <number>]"),
        ),
    }
}

/// `/calls [all | short]`: how much of each tool call is shown (issue 42).
fn calls_command<R: BufRead, W: Write>(term: &Shared<R, W>, rest: &str) {
    let style = term.borrow().style();
    match rest.trim() {
        "" => {
            let now = if term.borrow().calls_all {
                "all"
            } else {
                "short"
            };
            say_line(
                term,
                &format!("tool calls are shown: {now} (/calls all, /calls short)"),
            );
        }
        "all" | "short" => {
            let all = rest.trim() == "all";
            term.borrow_mut().calls_all = all;
            say_line(
                term,
                &style.green(if all {
                    "every tool call and result is shown"
                } else {
                    "one line for each call; a result only when it failed; steps instead of calls"
                }),
            );
        }
        _ => say_line(term, &style.yellow("usage: /calls [all | short]")),
    }
}

/// `/tasks [show ID | stop ID]`: the background commands and subagents in one list (issue 42).
fn tasks_command<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, rest: &str) {
    let style = term.borrow().style();
    let (sub, arg) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, ""), |(a, b)| (a, b.trim()));
    let usage = || {
        say_line(
            term,
            &style.yellow("usage: /tasks [show <id> | stop <id>]   (ids: s1 for a command, a1 for a subagent)"),
        );
    };
    match sub {
        "" | "list" if arg.is_empty() => {
            let shells = agent.shells().list();
            let runs = agent.subagent_runs();
            if shells.is_empty() && runs.is_empty() {
                return say_line(
                    term,
                    &style.dim("nothing has run in the background in this run"),
                );
            }
            let width = term.borrow().line_width();
            for info in &shells {
                let line = info.line();
                say_line(term, &cut_line(&line, width));
            }
            for run in &runs {
                let head = format!(
                    "a{}  {}  {}  {}: ",
                    run.n,
                    run.status.name(),
                    crate::tools::clock(std::time::Duration::from_secs(run.seconds)),
                    run.profile
                );
                let room = width.saturating_sub(head.chars().count()).max(20);
                let task: String = run
                    .task
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(room)
                    .collect();
                say_line(term, &format!("{head}{task}"));
            }
        }
        "show" | "stop" => match task_id(arg) {
            Some(('a', n)) => subagents_command(agent, term, &format!("{sub} {n}")),
            Some(('s', n)) => shell_task(agent, term, sub == "stop", n),
            Some((_, n)) => {
                // A bare number: the command of that number if there is one, else the subagent.
                if agent.shells().list().iter().any(|t| t.n == n) {
                    shell_task(agent, term, sub == "stop", n);
                } else {
                    subagents_command(agent, term, &format!("{sub} {n}"));
                }
            }
            None => usage(),
        },
        _ => usage(),
    }
}

fn cut_line(line: &str, width: usize) -> String {
    crate::toolview::cut(line, width.max(20))
}

fn shell_task<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, stop: bool, n: u32) {
    let style = term.borrow().style();
    if stop {
        match agent.shells().stop(n) {
            Ok(()) => say_line(term, &style.green(&format!("stopping s{n}"))),
            Err(why) => say_line(term, &style.yellow(&why)),
        }
        return;
    }
    match agent.shells().output(n, 6000) {
        Ok((info, text)) => {
            say_line(term, &style.bold(&info.line()));
            let text = if text.trim().is_empty() {
                style.dim("(no output yet)")
            } else {
                text.trim_end().to_owned()
            };
            say_line(term, &text);
        }
        Err(why) => say_line(term, &style.yellow(&why)),
    }
}

/// `/expand [N|list]`: the full text of a tool call of this run.
fn expand_command<R: BufRead, W: Write>(term: &Shared<R, W>, argument: &str) {
    let style = term.borrow().style();
    let count = term.borrow().tools.len();
    if count == 0 {
        say_line(
            term,
            &style.yellow(
                "no tool calls in this run yet (calls from before a /resume cannot be expanded)",
            ),
        );
        return;
    }
    let text = if argument == "list" {
        let width = term.borrow().line_width();
        let lines: Vec<String> = term
            .borrow()
            .tools
            .iter()
            .enumerate()
            .map(|(i, record)| {
                let head = format!("#{} {}", i + 1, record.name);
                let room = width.saturating_sub(head.chars().count() + 1).max(20);
                format!(
                    "{head} {}",
                    crate::toolview::summary(&record.name, &record.arguments, room)
                )
            })
            .collect();
        format!("{}\n", lines.join("\n"))
    } else {
        let number = if argument.is_empty() {
            Some(count)
        } else {
            argument.parse::<usize>().ok()
        };
        match number {
            Some(n) if (1..=count).contains(&n) => {
                crate::toolview::full(n, &term.borrow().tools[n - 1])
            }
            Some(_) => {
                say_line(
                    term,
                    &style.yellow(&format!(
                        "there is no call #{argument}; this run has #1 to #{count}"
                    )),
                );
                return;
            }
            None => {
                say_line(term, &style.yellow("usage: /expand [N|list]"));
                return;
            }
        }
    };
    term.borrow_mut().say(&text);
}

/// `/approve`: shows or chooses how the file tools are approved (issue 16).
fn approve_command<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, argument: &str) {
    let style = term.borrow().style();
    let current = agent.approval_mode();
    if argument == "forget" {
        let count = agent.forget_always();
        say_line(
            term,
            &format!(
                "forgot {count} \"always\" permission{}",
                if count == 1 { "" } else { "s" }
            ),
        );
        return;
    }
    let chosen = if argument.is_empty() {
        let always = agent.always_labels();
        if !always.is_empty() {
            say_line(
                term,
                &format!(
                    "{} {} (/approve forget clears them)",
                    style.dim("allowed until you quit:"),
                    always.join(", ")
                ),
            );
        }
        if !term.borrow().menus_enabled() {
            say_line(
                term,
                &format!("approval mode: {} ({})", current.name(), current.describe()),
            );
            return;
        }
        let items: Vec<MenuItem> = Mode::ALL
            .iter()
            .map(|mode| {
                let marker = if *mode == current { " (now)" } else { "" };
                MenuItem::new(format!("{}{marker}", mode.name())).with_detail(mode.describe())
            })
            .collect();
        let start = Mode::ALL.iter().position(|m| *m == current).unwrap_or(0);
        let picked = term.borrow_mut().pick_from("approval mode", &items, start);
        match picked {
            Some(index) => Mode::ALL[index],
            None => {
                say_line(term, &style.dim("unchanged"));
                return;
            }
        }
    } else {
        match Mode::parse(argument) {
            Some(mode) => mode,
            None => {
                say_line(term, &style.yellow("usage: /approve [manual|auto|forget]"));
                return;
            }
        }
    };
    if chosen == current {
        say_line(
            term,
            &format!(
                "approval mode stays {}: {}",
                current.name(),
                current.describe()
            ),
        );
        return;
    }
    match agent.set_approval_mode(chosen) {
        Ok(()) => say_line(
            term,
            &style.green(&format!(
                "approval mode is now {}: {}",
                chosen.name(),
                chosen.describe()
            )),
        ),
        Err(why) => say_line(term, &style.yellow(&format!("[{why}]"))),
    }
}

/// Says what a model switch did: the new model and the warnings, or why it did not happen.
pub(crate) fn report_model_switch<R: BufRead, W: Write>(
    agent: &Agent,
    term: &Shared<R, W>,
    switched: Result<Vec<String>, String>,
) {
    let style = term.borrow().style();
    match switched {
        Ok(notes) => {
            say_line(
                term,
                &style.green(&format!("model is now {}", agent.model())),
            );
            warnings(term, style, &notes);
        }
        Err(why) => say_line(term, &style.yellow(&format!("[{why}]"))),
    }
}

/// `/models [text]`: the model ids the provider offers.
fn models_command<R: BufRead, W: Write>(agent: &Agent, term: &Shared<R, W>, filter: &str) {
    let style = term.borrow().style();
    let models = match agent.list_models() {
        Ok(models) => models,
        Err(why) => {
            say_line(term, &style.red(&format!("[error: {why}]")));
            return;
        }
    };
    let wanted = filter.to_lowercase();
    let matching: Vec<&String> = models
        .iter()
        .filter(|id| id.to_lowercase().contains(&wanted))
        .collect();
    if matching.is_empty() {
        let what = if filter.is_empty() {
            "the provider listed no models".to_owned()
        } else {
            format!(
                "none of the provider's {} models contain '{filter}'",
                models.len()
            )
        };
        say_line(term, &style.dim(&what));
        return;
    }
    let mut text = String::new();
    for id in matching.iter().take(SHOWN_MODELS) {
        let mark = if id.as_str() == agent.model() {
            format!("  {}", style.green("(in use)"))
        } else {
            String::new()
        };
        text.push_str(&format!("  {id}{mark}\n"));
    }
    let shown = matching.len().min(SHOWN_MODELS);
    let summary = if matching.len() > shown {
        format!(
            "{shown} of {} matching models shown; add text to narrow the list, e.g. /models claude",
            matching.len()
        )
    } else {
        let count = matching.len();
        format!("{count} model{}", if count == 1 { "" } else { "s" })
    };
    text.push_str(&format!("{}\n", style.dim(&summary)));
    term.borrow_mut().say(&text);
}

/// `/provider` with nothing after it: the list.
fn show_providers<R: BufRead, W: Write>(agent: &Agent, term: &Shared<R, W>) {
    let style = term.borrow().style();
    let listed = match agent.providers() {
        Ok(listed) => listed,
        Err(why) => {
            say_line(term, &style.red(&format!("[error: {why}]")));
            return;
        }
    };
    let in_use = &agent.target().name;
    let mut text = format!("{}\n", style.bold("providers:"));
    if listed.is_empty() {
        text.push_str(&format!(
            "  {}\n",
            style.dim("none yet; add one with /provider add <name> <base_url> <model>")
        ));
    }
    let width = listed.iter().map(|l| l.name.len()).max().unwrap_or(0);
    for entry in &listed {
        let mark = if &entry.name == in_use {
            format!("  {}", style.green("(in use)"))
        } else {
            String::new()
        };
        let what = match &entry.def {
            Ok(def) => format!(
                "{}  {}",
                router_host(&def.base_url).unwrap_or_else(|| def.base_url.clone()),
                def.model
            ),
            Err(why) => style.red(&format!("cannot be used: {why}")),
        };
        text.push_str(&format!(
            "  {}  {}  {what}{mark}\n",
            style.bold(&format!("{:<width$}", entry.name)),
            style.dim(&format!("{:<7}", entry.source.label())),
        ));
    }
    text.push_str(&format!(
        "{}\n",
        style.dim(
            "switch with /provider <name>; add one with /provider add <name> <base_url> <model>"
        )
    ));
    term.borrow_mut().say(&text);
}

/// `/provider add <name> <base_url> <model> [openai|anthropic]`: asks for the key, saves, switches.
fn add_provider_command<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    arguments: &[&str],
) {
    let style = term.borrow().style();
    let (name, base_url, model, api) = match arguments {
        [name, base_url, model] => (name, base_url, model, Ok(ApiStyle::OpenAi)),
        [name, base_url, model, api] => (name, base_url, model, ApiStyle::parse(api)),
        _ => {
            say_line(
                term,
                &style.yellow("usage: /provider add <name> <base_url> <model> [openai|anthropic]"),
            );
            return;
        }
    };
    let api = match api {
        Ok(api) => api,
        Err(why) => {
            say_line(term, &style.red(&format!("[error: {why}]")));
            return;
        }
    };
    // Check everything that can be checked before asking for a key.
    if let Err(why) = crate::providers::check_name(name)
        .and_then(|()| crate::providers::check_base_url(base_url))
        .and_then(|()| crate::providers::check_model(model))
    {
        say_line(term, &style.red(&format!("[error: {why}]")));
        return;
    }
    if agent
        .providers()
        .is_ok_and(|all| all.iter().any(|l| l.name == *name))
    {
        say_line(
            term,
            &style.red(&format!("[error: the name {name} is already used]")),
        );
        return;
    }
    // The key goes from here straight into the database: not on the command line, not on
    // the screen, not to the model.
    let asked = term
        .borrow_mut()
        .ask_secret(&format!("API key for {name}, Enter for none"));
    let Some(answer) = asked else {
        say_line(term, &style.yellow("[cancelled: nothing was saved]"));
        return;
    };
    let key = answer.trim();
    let def = ProviderDef {
        name: (*name).to_owned(),
        base_url: (*base_url).to_owned(),
        model: (*model).to_owned(),
        api_key: (!key.is_empty()).then(|| key.to_owned()),
        router: kernel::RouterOptions::default(),
        api,
    };
    if let Err(why) = agent.add_provider(def) {
        say_line(term, &style.red(&format!("[error: {why}]")));
        return;
    }
    say_line(term, &style.green(&format!("saved provider {name}")));
    switch_provider_command(agent, term, name);
}

fn switch_provider_command<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    name: &str,
) {
    let switched = agent.switch_provider(name);
    report_switch(agent, term, switched);
}

/// Says what a provider switch did: the provider and model now in use and the warnings, or why
/// it did not happen.
pub(crate) fn report_switch<R: BufRead, W: Write>(
    agent: &Agent,
    term: &Shared<R, W>,
    switched: Result<Vec<String>, String>,
) {
    let style = term.borrow().style();
    match switched {
        Ok(notes) => {
            say_line(
                term,
                &style.green(&format!(
                    "now using {} with model {}",
                    target_label(agent),
                    agent.model()
                )),
            );
            warnings(term, style, &notes);
        }
        Err(why) => say_line(term, &style.red(&format!("[error: {why}]"))),
    }
}

/// `/provider ...`: list, switch, add, remove.
fn provider_command<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>, rest: &str) {
    let style = term.borrow().style();
    let words: Vec<&str> = rest.split_whitespace().collect();
    let menus = term.borrow().menus_enabled();
    match words.as_slice() {
        [] if menus => crate::config_ui::provider_menu(agent, term),
        [] => show_providers(agent, term),
        ["config"] if menus => crate::config_ui::config_menu(agent, term),
        ["config"] => say_line(
            term,
            &style.yellow("/provider config needs a terminal: it is an arrow-key menu"),
        ),
        ["add", arguments @ ..] => add_provider_command(agent, term, arguments),
        ["remove", name] => match agent.remove_provider(name) {
            Ok(()) => say_line(term, &style.green(&format!("removed provider {name}"))),
            Err(why) => say_line(term, &style.red(&format!("[error: {why}]"))),
        },
        ["remove", ..] => say_line(term, &style.yellow("usage: /provider remove <name>")),
        [name] => switch_provider_command(agent, term, name),
        _ => say_line(
            term,
            &style.yellow(
                "usage: /provider [<name> | config | add <name> <base_url> <model> | remove <name>]",
            ),
        ),
    }
}

/// Runs the chat until `/exit`, `/quit` or end of input.
pub fn run<R: BufRead, W: Write>(agent: &mut Agent, term: &Shared<R, W>) -> io::Result<()> {
    let mut observer = PrintObserver(Rc::clone(term));
    let style = term.borrow().style();
    term.borrow_mut().set_cancel(agent.cancel_handle());
    // The `/web` server, if it was started: it stops when this function returns (issue 29).
    let mut web: Option<crate::web::Web> = None;
    // A message the program sends by itself because of a choice the owner made (after a plan).
    let mut queued: Option<String> = None;
    loop {
        // Each borrow ends at the semicolon, before `agent.send` runs: the
        // approver and observer borrow the terminal again while it works.
        let typed: String;
        let woke_up = queued.is_none() && agent.wake_ready();
        if let Some(text) = queued.take() {
            // The owner chose "build" after a plan: the message is not typed.
            typed = text;
        } else if woke_up {
            // A subagent finished: the model gets its answer and goes on without waiting for the
            // owner to type (issue 35).
            agent.note_wake();
            say_line(
                term,
                &style.dim("[a subagent finished: its answer goes to the model]"),
            );
            typed = crate::agent::WAKE_TEXT.to_owned();
        } else {
            let read = term
                .borrow_mut()
                .read_prompt(|width| box_text(agent, width), || agent.wake_ready());
            match read {
                Prompted::Wake => continue,
                Prompted::Eof => {
                    term.borrow_mut().say("\n");
                    if quit_confirmed(agent, term) {
                        return Ok(());
                    }
                    continue;
                }
                Prompted::Line(line) => {
                    agent.reset_wakes();
                    typed = line;
                }
            }
        }
        let line = typed.trim();
        if line.is_empty() {
            continue;
        }
        if line == "/exit" || line == "/quit" {
            if quit_confirmed(agent, term) {
                return Ok(());
            }
            continue;
        }
        if line == "/sessions" {
            if term.borrow().menus_enabled() {
                crate::sessions_ui::sessions_menu(agent, term);
            } else {
                show_sessions(agent, term);
            }
            continue;
        }
        if line == "/resume" || line.starts_with("/resume ") {
            resume_command(agent, term, line);
            continue;
        }
        let (word, rest) = line
            .split_once(char::is_whitespace)
            .map_or((line, ""), |(word, rest)| (word, rest.trim()));
        match word {
            "/help" => {
                term.borrow_mut().say(&help_text(style));
                continue;
            }
            "/model" => {
                model_command(agent, term, rest);
                continue;
            }
            "/models" => {
                models_command(agent, term, rest);
                continue;
            }
            "/provider" => {
                provider_command(agent, term, rest);
                continue;
            }
            "/approve" => {
                approve_command(agent, term, rest);
                continue;
            }
            "/expand" => {
                expand_command(term, rest);
                continue;
            }
            "/info" if rest.is_empty() => {
                say_line(term, &info_text(agent, None));
                continue;
            }
            "/subagent" => {
                subagent_command(agent, term, rest);
                continue;
            }
            "/subagents" => {
                subagents_command(agent, term, rest);
                continue;
            }
            "/tasks" => {
                tasks_command(agent, term, rest);
                continue;
            }
            "/calls" => {
                calls_command(term, rest);
                continue;
            }
            "/plan" => {
                plan_command(agent, term, rest);
                continue;
            }
            "/prompts" => {
                crate::prompts_ui::command(agent, term, rest);
                continue;
            }
            "/skills" => {
                crate::skills_ui::skills_command(agent, term, rest);
                continue;
            }
            "/profile" => {
                crate::profiles_ui::command(agent, term, rest);
                continue;
            }
            "/context" => {
                crate::context_ui::command(agent, term, rest);
                continue;
            }
            "/tools" => {
                crate::tools_ui::command(agent, term, rest);
                continue;
            }
            "/glossary" => {
                crate::glossary_ui::command(agent, term, rest);
                continue;
            }
            "/mcp" => {
                crate::mcp_ui::command(agent, term, rest);
                continue;
            }
            "/inbox" => {
                crate::inbox_ui::command(agent, term, rest);
                continue;
            }
            "/web" => {
                crate::web_ui::command(agent, term, rest, &mut web);
                continue;
            }
            "/personality" => {
                crate::skills_ui::personality_command(agent, term, rest);
                continue;
            }
            "/compact" => {
                compact_command(agent, term, rest);
                continue;
            }
            "/new" if rest.is_empty() => {
                let cleared = !agent.chat_prompts().is_empty();
                match agent.new_chat() {
                    Ok(info) => say_line(
                        term,
                        &style.green(&format!(
                            "new chat; system prompt: {}{}",
                            info.summary(),
                            if cleared {
                                "; personality and skills cleared"
                            } else {
                                ""
                            }
                        )),
                    ),
                    Err(why) => say_line(term, &style.red(&format!("[error: {why}]"))),
                }
                continue;
            }
            _ => {}
        }
        // `/init` is a message to the model that the program writes.
        let is_init = word == "/init" && rest.is_empty();
        let line = if is_init { INIT_MESSAGE } else { line };
        // A command that is misspelled or has arguments it does not take is not a message: sending
        // it would cost a model call for nothing.
        if !is_init && looks_like_command(word) {
            let known = COMMANDS
                .iter()
                .filter_map(|(command, _)| command.split_whitespace().next())
                .any(|command| command == word);
            let said = if known {
                format!("{word} does not take that; /help shows how to use it")
            } else {
                format!("unknown command {word}; /help lists the commands")
            };
            say_line(term, &style.yellow(&said));
            continue;
        }
        // `@path` words bring the files' text along; what was typed is what the screen showed.
        let root = term.borrow().root.clone();
        let attachment = crate::attach::attach(&root, line);
        for file in &attachment.files {
            let cut = if file.cut { ", cut" } else { "" };
            let what = match file.listed {
                Some(count) => format!("{count} files listed{cut}"),
                None => format!("{} bytes{cut}", file.bytes),
            };
            say_line(term, &style.dim(&format!("  + {} ({what})", file.path)));
        }
        for warning in &attachment.warnings {
            say_line(term, &style.yellow(&format!("[{warning}]")));
        }
        if agent.needs_compacting() {
            let used = agent
                .usage()
                .last
                .and_then(|t| t.prompt)
                .unwrap_or_default();
            say_line(
                term,
                &style.yellow(&format!(
                    "[the last request was {} prompt tokens; compacting first]",
                    crate::usage::thousands(used)
                )),
            );
            if run_compact(agent, term, false) {
                // Esc while compacting: the message is not sent. At the box its text comes back.
                if term.borrow().menu_stty && !woke_up {
                    term.borrow_mut().restore = Some(line.to_owned());
                }
                continue;
            }
        }
        term.borrow_mut().steps.clear();
        term.borrow_mut().set_tasks(agent.task_view());
        term.borrow_mut()
            .begin_working(|width| info_text(agent, width));
        let quiet = term.borrow().quiet();
        let sent = agent.send(&attachment.message, &mut observer);
        drop(quiet);
        if woke_up && matches!(sent, Outcome::Failed(_) | Outcome::Cancelled) {
            // A message the owner did not type is not tried again and again.
            agent.stop_wakes();
        }
        let plan_text = match &sent {
            Outcome::Answer(text) if agent.planning() && !woke_up => Some(text.clone()),
            _ => None,
        };
        let report = match sent {
            Outcome::Answer(text) => {
                let width = term.borrow().line_width();
                format!("{}\n", crate::markdown::render(style, &text, width))
            }
            Outcome::CapReached { calls } => format!(
                "{}\n",
                style.yellow(&format!(
                    "[stopped: reached the limit of {calls} model calls for this message]"
                ))
            ),
            Outcome::LoopStopped { calls } => format!(
                "{}\n",
                style.yellow(&format!(
                    "[stopped: the last tool calls failed or repeated one another (after {calls} model calls); say what to do next]"
                ))
            ),
            Outcome::Failed(why) => format!("{}\n", style.red(&format!("[error: {why}]"))),
            Outcome::Cancelled => format!(
                "{}\n",
                style.yellow("[cancelled: the transcript was kept; say what to do next]")
            ),
        };
        term.borrow_mut().unpin_steps();
        term.borrow_mut().say(&report);
        term.borrow_mut().end_working();
        if let Some(plan) = plan_text {
            queued = after_plan(agent, term, &plan);
        }
    }
}

/// What to do with a plan the model just wrote (issue 42). Returns the message to send next, if
/// the owner chose to build it here.
fn after_plan<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    plan: &str,
) -> Option<String> {
    let style = term.borrow().style();
    let choice = if term.borrow().menus_enabled() {
        let items = [
            MenuItem::new("Build").with_detail("leave plan mode and do the plan here"),
            MenuItem::new("Build with agent")
                .with_detail("a subagent does it in the background (needs /approve auto)"),
            MenuItem::new("Discuss").with_detail("stay in plan mode and talk it over"),
        ];
        term.borrow_mut()
            .pick("The plan is ready. What next?", &items)
            .unwrap_or(2)
    } else {
        term.borrow_mut().say(&style.bold(
            "The plan is ready: [b]uild it here, build with an [a]gent, or [d]iscuss it? [b/a/D] ",
        ));
        match term
            .borrow_mut()
            .read_line()
            .unwrap_or_default()
            .trim()
            .to_lowercase()
            .as_str()
        {
            "b" | "build" => 0,
            "a" | "agent" => 1,
            _ => 2,
        }
    };
    match choice {
        0 => {
            agent.set_planning(false);
            say_line(term, &style.green("plan mode is off: building"));
            Some("Carry out the plan you just wrote, step by step.".to_owned())
        }
        1 => {
            if agent.approval_mode() != Mode::Auto {
                say_line(
                    term,
                    &style.yellow(
                        "a subagent changes files only in /approve auto; still in plan mode (/approve auto, then ask again, or choose Build)",
                    ),
                );
                return None;
            }
            agent.set_planning(false);
            say_line(term, &style.green("plan mode is off"));
            subagent_command(agent, term, &format!("Carry out this plan:\n\n{plan}"));
            None
        }
        _ => {
            say_line(
                term,
                &style.dim("still in plan mode: type to discuss, /plan off to leave"),
            );
            None
        }
    }
}

/// `/plan [on | off]`.
fn plan_command<R: BufRead, W: Write>(agent: &Agent, term: &Shared<R, W>, rest: &str) {
    let style = term.borrow().style();
    match rest.trim() {
        "" | "on" => {
            agent.set_planning(true);
            say_line(
                term,
                &style.green(
                    "plan mode is on: the model can only look and answer with a plan (/plan off leaves it)",
                ),
            );
        }
        "off" => {
            agent.set_planning(false);
            say_line(term, &style.green("plan mode is off"));
        }
        _ => say_line(term, &style.yellow("usage: /plan [on | off]")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ScriptedProvider;
    use crate::tools::testutil::Scratch;
    use crate::tools::{Bash, DEFAULT_TIMEOUT, Mode, ReadFile, Tool, file_tools, shared_mode};
    use crate::turn_log::TurnLog;
    use std::io::Cursor;

    type TestTerm = Shared<Cursor<Vec<u8>>, Vec<u8>>;

    #[test]
    fn the_prompt_width_ignores_colour_codes_and_restarts_after_a_newline() {
        assert_eq!(advance_col(0, "> "), 2);
        assert_eq!(advance_col(0, "\x1b[36m> \x1b[0m"), 2);
        assert_eq!(advance_col(5, "abc\nxy"), 2);
        assert_eq!(advance_col(5, "abc\r\n"), 0);
        assert_eq!(advance_col(2, "\x1b[3A\r\x1b[J"), 0);
        assert_eq!(advance_col(0, "name (empty cancels): "), 22);
        assert_eq!(advance_col(0, "é日"), 2);
    }

    struct Rig {
        agent: Agent,
        term: TestTerm,
        requests: crate::agent::Shared<Vec<Vec<u8>>>,
    }

    fn rig(
        dir: &Scratch,
        typed: &str,
        replies: Vec<Result<crate::provider::Reply, Box<crate::provider::ProviderError>>>,
    ) -> Rig {
        rig_styled(dir, typed, replies, Style::plain())
    }

    fn rig_styled(
        dir: &Scratch,
        typed: &str,
        replies: Vec<Result<crate::provider::Reply, Box<crate::provider::ProviderError>>>,
        style: Style,
    ) -> Rig {
        rig_with_provider(dir, typed, ScriptedProvider::new(replies), style)
    }

    fn rig_with_provider(
        dir: &Scratch,
        typed: &str,
        provider: ScriptedProvider,
        style: Style,
    ) -> Rig {
        let term =
            Terminal::shared_styled(Cursor::new(typed.as_bytes().to_vec()), Vec::new(), style);
        let requests = provider.requests();
        let approval = shared_mode(Mode::Manual);
        let mut tools: Vec<Box<dyn Tool>> = vec![Box::new(ReadFile::new(dir.path()))];
        tools.extend(file_tools(
            dir.path(),
            Rc::clone(&approval),
            Rc::new(TerminalApprover(Rc::clone(&term))),
        ));
        tools.push(Box::new(
            Bash::new(
                dir.path(),
                DEFAULT_TIMEOUT,
                Box::new(TerminalApprover(Rc::clone(&term))),
            )
            .with_approval(Rc::clone(&approval)),
        ));
        tools.push(Box::new(crate::tools::StepsTool::new()));
        let log = TurnLog::create(&dir.path().join("logs")).unwrap();
        let agent = Agent::new("m", Box::new(provider), tools, log)
            .with_target("test", "http://x/v1")
            .with_approval(approval);
        Rig {
            agent,
            term,
            requests,
        }
    }

    fn screen(term: &TestTerm) -> String {
        String::from_utf8(term.borrow().output().clone()).unwrap()
    }

    fn tool_message(request: &[u8], index: usize) -> String {
        let v: serde_json::Value = serde_json::from_slice(request).unwrap();
        v["messages"][index]["content"].as_str().unwrap().to_owned()
    }

    #[test]
    fn compact_summarises_the_chat_and_the_next_message_goes_into_the_new_one() {
        let dir = Scratch::new("repl-compact");
        let mut r = rig_stored(
            &dir,
            "hello\n/compact\n/compact\nnext\n/exit\n",
            vec![
                ScriptedProvider::text("hi"),
                ScriptedProvider::text("The user said hello."),
                ScriptedProvider::text("answer"),
            ],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(
            out.contains("[compacted: 2 messages became a summary of 20 characters; the earlier chat is saved]"),
            "{out}"
        );
        assert!(
            out.contains("[not compacted: there is nothing to compact yet]"),
            "{out}"
        );
        let requests = r.requests.borrow();
        assert_eq!(requests.len(), 3);
        let sent: serde_json::Value = serde_json::from_slice(&requests[2]).unwrap();
        let messages = sent["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert!(
            messages[1]["content"]
                .as_str()
                .unwrap()
                .ends_with("The user said hello.")
        );
        assert_eq!(messages[2]["content"], "next");
    }

    #[test]
    fn compact_auto_shows_sets_and_refuses() {
        let dir = Scratch::new("repl-compact-auto");
        let mut r = rig_stored(
            &dir,
            "/compact auto\n/compact auto 20,000\n/compact auto\n/compact auto 5\n/compact auto many\n/compact auto off\n/compact auto\n/compact now\n/exit\n",
            vec![],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        for expected in [
            "before a message when the last request was 80,000 prompt tokens or more",
            "automatic compaction at 20,000 prompt tokens",
            "was 20,000 prompt tokens or more",
            "the limit must be at least 10000 tokens",
            "'many' is not a number of tokens or off",
            "automatic compaction is off",
            "automatic compaction: off",
            "usage: /compact | /compact auto",
        ] {
            assert!(out.contains(expected), "missing {expected:?} in {out}");
        }
    }

    #[test]
    fn a_large_context_is_compacted_before_the_next_message_is_sent() {
        let dir = Scratch::new("repl-compact-before-send");
        let mut r = rig_stored(
            &dir,
            "first\nsecond\n/exit\n",
            vec![
                ScriptedProvider::with_usage(
                    ScriptedProvider::text("one"),
                    r#"{"prompt_tokens":90000}"#,
                ),
                ScriptedProvider::text("Summary text."),
                ScriptedProvider::text("two"),
            ],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(
            out.contains("[the last request was 90,000 prompt tokens; compacting first]"),
            "{out}"
        );
        assert!(
            out.contains("[compacted: 2 messages became a summary"),
            "{out}"
        );
        let requests = r.requests.borrow();
        assert_eq!(requests.len(), 3);
        let sent: serde_json::Value = serde_json::from_slice(&requests[2]).unwrap();
        let texts: Vec<&str> = sent["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["content"].as_str().unwrap_or(""))
            .collect();
        assert!(texts[1].starts_with(SUMMARY_MARK));
        assert_eq!(texts[2], "second");
    }

    #[test]
    fn a_failed_automatic_compaction_still_sends_the_message() {
        let dir = Scratch::new("repl-compact-fails");
        let mut r = rig_stored(
            &dir,
            "first\nsecond\n/exit\n",
            vec![
                ScriptedProvider::with_usage(
                    ScriptedProvider::text("one"),
                    r#"{"prompt_tokens":90000}"#,
                ),
                ScriptedProvider::failure("boom"),
                ScriptedProvider::text("two"),
            ],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(
            out.contains("[not compacted: boom; sending the message as it is]"),
            "{out}"
        );
        assert!(out.contains("two"), "{out}");
    }

    #[test]
    fn esc_during_automatic_compaction_does_not_send_the_message() {
        let dir = Scratch::new("repl-compact-esc");
        let slot = crate::agent::Shared::new(None);
        let provider = ScriptedProvider::new(vec![
            ScriptedProvider::with_usage(
                ScriptedProvider::text("one"),
                r#"{"prompt_tokens":90000}"#,
            ),
            ScriptedProvider::text("Summary text."),
            ScriptedProvider::text("two"),
        ])
        .cancelling_at(2, slot.clone());
        let mut r = rig_with_provider(&dir, "first\nsecond\n/exit\n", provider, Style::plain());
        *slot.borrow_mut() = Some(r.agent.cancel_handle());
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(
            out.contains("[not compacted: cancelled; the message was not sent]"),
            "{out}"
        );
        assert!(!out.contains("sending the message as it is"), "{out}");
        // "first" and the summary request were sent; "second" was not.
        assert_eq!(r.requests.borrow().len(), 2);
        assert!(!out.contains("two"), "{out}");
    }

    #[test]
    fn a_saved_summary_is_replayed_as_one_line() {
        let dir = Scratch::new("repl-summary-line");
        let r = rig(&dir, "", vec![]);
        let history = vec![
            Message::System("s".to_owned()),
            Message::User(format!("{SUMMARY_MARK}\n\nlong text of the summary")),
            Message::User("go on".to_owned()),
        ];
        show_history(&r.term, &history);
        let out = screen(&r.term);
        assert!(
            out.contains("[summary of an earlier chat: 24 characters]"),
            "{out}"
        );
        assert!(!out.contains("long text"), "{out}");
        assert!(out.contains("go on"), "{out}");
    }

    #[test]
    fn a_multi_turn_chat_with_an_approved_command() {
        let dir = Scratch::new("repl-e2e");
        std::fs::write(dir.path().join("a.txt"), "file text\n").unwrap();
        // The approval answer "y" is the line right after the request that needs it.
        let mut r = rig(
            &dir,
            "run echo hi && touch hi.txt\ny\nnow read a.txt\n/exit\n",
            vec![
                ScriptedProvider::tool_call(
                    "c1",
                    "bash",
                    r#"{"command":"echo hi && touch hi.txt"}"#,
                ),
                ScriptedProvider::text("It printed hi."),
                ScriptedProvider::tool_call("c2", "read_file", r#"{"path":"a.txt"}"#),
                ScriptedProvider::text("The file says: file text."),
            ],
        );
        run(&mut r.agent, &r.term).unwrap();

        let out = screen(&r.term);
        assert!(out.contains("▸ #1 bash echo hi && touch hi.txt\n"), "{out}");
        assert!(
            !out.contains("  ✓ hi\n"),
            "a result that went well is not shown: {out}"
        );
        assert!(
            out.contains("run this command?\n  echo hi && touch hi.txt\n"),
            "{out}"
        );
        assert!(out.contains("[y/a/N] "), "{out}");
        assert!(out.contains("It printed hi.\n"), "{out}");
        assert!(out.contains("▸ #2 read_file a.txt\n"), "{out}");
        assert!(out.contains("The file says: file text.\n"), "{out}");

        let requests = r.requests.borrow();
        assert_eq!(requests.len(), 4);
        // Request 2 carries the bash result: system, user, assistant, tool.
        assert_eq!(tool_message(&requests[1], 3), "hi\n[exit code: 0]");
        // Request 4 carries the file contents.
        assert_eq!(tool_message(&requests[3], 7), "file text\n");

        let log = std::fs::read_to_string(r.agent.log_path()).unwrap();
        assert_eq!(log.lines().count(), 4);
    }

    #[test]
    fn an_at_file_in_a_message_is_attached_and_listed_under_the_echo() {
        let dir = Scratch::new("repl-at-file");
        std::fs::write(dir.path().join("a.txt"), "file text\n").unwrap();
        let mut r = rig(
            &dir,
            "explain @a.txt and @missing.txt\nplain words\n/exit\n",
            vec![
                ScriptedProvider::text("It says file text."),
                ScriptedProvider::text("ok"),
            ],
        );
        r.term.borrow_mut().set_root(dir.path().to_path_buf());
        run(&mut r.agent, &r.term).unwrap();

        let out = screen(&r.term);
        assert!(out.contains("  + a.txt (10 bytes)\n"), "{out}");
        assert!(
            !out.contains("missing.txt (") && !out.contains("not attached"),
            "{out}"
        );
        let requests = r.requests.borrow();
        let first: serde_json::Value = serde_json::from_slice(&requests[0]).unwrap();
        assert_eq!(
            first["messages"][1]["content"],
            "explain @a.txt and @missing.txt\n\nAttached files:\n\n--- a.txt ---\nfile text\n--- end of a.txt ---\n"
        );
        // A message without @ words goes out exactly as typed.
        let second: serde_json::Value = serde_json::from_slice(&requests[1]).unwrap();
        assert_eq!(second["messages"][3]["content"], "plain words");
    }

    #[test]
    fn a_file_that_cannot_be_attached_is_said_and_the_message_still_goes() {
        let dir = Scratch::new("repl-at-bad");
        std::fs::write(dir.path().join("bin.dat"), [0xff, 0xfe, 0x01]).unwrap();
        let mut r = rig(
            &dir,
            "look at @bin.dat\n/exit\n",
            vec![ScriptedProvider::text("ok")],
        );
        r.term.borrow_mut().set_root(dir.path().to_path_buf());
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(
            out.contains("[@bin.dat: not attached (it is not UTF-8 text)]"),
            "{out}"
        );
        let first: serde_json::Value = serde_json::from_slice(&r.requests.borrow()[0]).unwrap();
        assert_eq!(first["messages"][1]["content"], "look at @bin.dat");
    }

    #[test]
    fn a_saved_instructions_block_is_shown_as_one_dim_line_in_a_replay() {
        let dir = Scratch::new("repl-block-replay");
        let r = rig(&dir, "", vec![]);
        let block = format!(
            "{ACTIVE_MARK}. They replace any earlier message:\n\n## Personality: pirate\nArr.\n\n## Skill: tdd\nTest.\n\n## Skill: ui\nPretty."
        );
        show_history(
            &r.term,
            &[
                Message::System("s".into()),
                Message::User(block),
                Message::User("hello".into()),
                Message::Assistant {
                    content: Some("hi".into()),
                    tool_calls: vec![],
                },
                Message::User(crate::store_prompts::NONE_ACTIVE.into()),
                Message::User("bye".into()),
            ],
        );
        let out = screen(&r.term);
        assert!(
            out.contains("[instructions: personality pirate; skills tdd, ui]\n"),
            "{out}"
        );
        assert!(out.contains("[instructions: none active]\n"), "{out}");
        assert!(
            out.contains("┃ hello\n\nhi\n") && out.contains("┃ bye\n"),
            "{out}"
        );
        assert!(!out.contains("Arr.") && !out.contains("Pretty."), "{out}");
    }

    #[test]
    fn a_denied_command_is_not_run_and_the_model_is_told() {
        let dir = Scratch::new("repl-deny");
        let mut r = rig(
            &dir,
            "make a file\nn\n/exit\n",
            vec![
                ScriptedProvider::tool_call(
                    "c1",
                    "bash",
                    r#"{"command":"touch should-not-exist"}"#,
                ),
                ScriptedProvider::text("Understood, I did not run it."),
            ],
        );
        run(&mut r.agent, &r.term).unwrap();
        assert!(!dir.path().join("should-not-exist").exists());
        let requests = r.requests.borrow();
        assert!(tool_message(&requests[1], 3).contains("denied by user"));
    }

    #[test]
    fn end_of_input_at_the_approval_question_means_no() {
        let dir = Scratch::new("repl-eof-approval");
        let mut r = rig(
            &dir,
            "make a file\n",
            vec![
                ScriptedProvider::tool_call("c1", "bash", r#"{"command":"touch nope"}"#),
                ScriptedProvider::text("ok"),
            ],
        );
        run(&mut r.agent, &r.term).unwrap();
        assert!(!dir.path().join("nope").exists());
    }

    #[test]
    fn empty_lines_are_skipped_and_slash_exit_quits_before_reading_more() {
        let dir = Scratch::new("repl-exit");
        let mut r = rig(&dir, "\n   \n/exit\nnever sent\n", vec![]);
        run(&mut r.agent, &r.term).unwrap();
        assert!(r.requests.borrow().is_empty());
    }

    #[test]
    fn end_of_input_quits_cleanly() {
        let dir = Scratch::new("repl-eof");
        let mut r = rig(&dir, "", vec![]);
        run(&mut r.agent, &r.term).unwrap();
        assert!(screen(&r.term).starts_with(PROMPT));
    }

    #[test]
    fn a_failed_call_prints_an_error_and_the_chat_goes_on() {
        let dir = Scratch::new("repl-error");
        let mut r = rig(
            &dir,
            "hello\nagain\n",
            vec![
                ScriptedProvider::failure("request failed: Connection refused"),
                ScriptedProvider::text("back"),
            ],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(
            out.contains("[error: request failed: Connection refused]\n"),
            "{out}"
        );
        assert!(out.contains("back\n"), "{out}");
    }

    #[test]
    fn hitting_the_call_cap_is_reported() {
        let dir = Scratch::new("repl-cap");
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let replies = (0..5)
            .map(|i| {
                ScriptedProvider::tool_call(&format!("c{i}"), "read_file", r#"{"path":"a.txt"}"#)
            })
            .collect();
        let mut r = rig(&dir, "loop\n", replies);
        r.agent = r.agent.with_max_calls(2);
        run(&mut r.agent, &r.term).unwrap();
        assert!(screen(&r.term).contains("[stopped: reached the limit of 2 model calls"));
    }

    #[test]
    fn long_tool_arguments_are_shortened_on_screen_only() {
        let dir = Scratch::new("repl-long");
        let long = "a".repeat(500);
        let args = format!(r#"{{"command":"echo {long}"}}"#);
        let mut r = rig(
            &dir,
            "go\nn\n",
            vec![
                ScriptedProvider::tool_call("c1", "bash", &args),
                ScriptedProvider::text("done"),
            ],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        // (A real terminal echoes the typed newline; this scripted one does not,
        // so the prompt shares the line.)
        let tool_line = out
            .lines()
            .find_map(|l| l.find("▸").map(|i| &l[i..]))
            .unwrap();
        assert!(tool_line.ends_with('…'), "{tool_line}");
        assert!(
            tool_line.chars().count() <= DEFAULT_LINE_WIDTH,
            "{tool_line}"
        );
        // The model still got the full command.
        let requests = r.requests.borrow();
        assert!(String::from_utf8_lossy(&requests[1]).contains(&long));
    }

    fn blank_term() -> TestTerm {
        Terminal::shared(Cursor::new(Vec::new()), Vec::new())
    }

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    fn said(text: &str) -> Message {
        Message::Assistant {
            content: Some(text.into()),
            tool_calls: vec![],
        }
    }

    #[test]
    fn a_saved_chat_is_shown_the_way_the_live_chat_showed_it() {
        let term = blank_term();
        let history = vec![
            Message::System("SECRET SYSTEM PROMPT".into()),
            Message::User("hi".into()),
            said("Hello!"),
            Message::User("read it".into()),
            Message::Assistant {
                content: Some("let me look".into()),
                tool_calls: vec![call("read_file", r#"{"path":"a.txt"}"#)],
            },
            Message::Tool {
                tool_call_id: "c".into(),
                content: "FILE CONTENTS".into(),
            },
            said("It says x."),
        ];
        show_history(&term, &history);
        assert_eq!(
            screen(&term),
            "\n┃ hi\n\nHello!\n\n┃ read it\n\n▸ read_file a.txt\nIt says x.\n[end of saved chat]\n"
        );
    }

    #[test]
    fn only_the_last_ten_user_messages_are_shown_and_the_rest_are_counted() {
        let term = blank_term();
        let mut history = vec![Message::System("p".into())];
        for n in 0..12 {
            history.push(Message::User(format!("q{n}")));
            history.push(said(&format!("a{n}")));
        }
        show_history(&term, &history);
        let out = screen(&term);
        // q0, a0, q1, a1 are left out.
        assert!(
            out.starts_with("[4 earlier messages not shown]\n\n┃ q2\n\na2\n"),
            "{out}"
        );
        assert!(!out.contains("q1\n") && !out.contains("a1\n"), "{out}");
        assert!(
            out.ends_with("┃ q11\n\na11\n[end of saved chat]\n"),
            "{out}"
        );
    }

    #[test]
    fn a_short_chat_shows_everything_without_a_count() {
        let term = blank_term();
        let history = vec![Message::System("p".into()), Message::User("only".into())];
        show_history(&term, &history);
        assert_eq!(screen(&term), "\n┃ only\n\n[end of saved chat]\n");
    }

    #[test]
    fn long_tool_arguments_are_shortened_in_a_replay_too() {
        let term = blank_term();
        let long = "b".repeat(500);
        let history = vec![
            Message::System("p".into()),
            Message::User("go".into()),
            Message::Assistant {
                content: None,
                tool_calls: vec![call("bash", &long)],
            },
        ];
        show_history(&term, &history);
        let out = screen(&term);
        let tool_line = out.lines().find(|l| l.starts_with('▸')).unwrap();
        assert!(
            tool_line.ends_with('…') && tool_line.chars().count() <= DEFAULT_LINE_WIDTH,
            "{tool_line}"
        );
    }

    #[test]
    fn a_chat_with_no_messages_shows_nothing() {
        let term = blank_term();
        show_history(&term, &[Message::System("p".into())]);
        show_history(&term, &[]);
        assert_eq!(screen(&term), "");
    }

    /// Same typed input, two separate runs: the first request is identical, byte
    /// for byte, in memory and in the log (decision 01).
    #[test]
    fn identical_input_gives_a_byte_identical_first_request() {
        let first_request = |tag: &str| {
            let dir = Scratch::new(tag);
            let mut r = rig(&dir, "hello there\n", vec![ScriptedProvider::text("hi")]);
            run(&mut r.agent, &r.term).unwrap();
            let sent = r.requests.borrow()[0].clone();
            let log = std::fs::read_to_string(r.agent.log_path()).unwrap();
            // Rust note: `RawValue` keeps the request's text exactly as written;
            // parsing it into a `Value` and printing it again would reorder keys.
            #[derive(serde::Deserialize)]
            struct Line {
                request: Box<serde_json::value::RawValue>,
            }
            let line: Line = serde_json::from_str(log.lines().next().unwrap()).unwrap();
            (sent, line.request.get().to_owned())
        };
        let (a, b) = (first_request("repl-det-1"), first_request("repl-det-2"));
        assert_eq!(a.0, b.0);
        assert_eq!(a.1, b.1);
        // The log embeds the request as the same JSON that was sent.
        assert_eq!(a.0, a.1.as_bytes());
    }

    // ---- /sessions and /resume ----

    use crate::agent::StoreContext;
    use crate::store::Store;

    /// Like `rig`, but the chat is saved in `<dir>/harness.db`.
    fn rig_stored(
        dir: &Scratch,
        typed: &str,
        replies: Vec<Result<crate::provider::Reply, Box<crate::provider::ProviderError>>>,
    ) -> Rig {
        rig_stored_styled(dir, typed, replies, Style::plain())
    }

    fn rig_stored_styled(
        dir: &Scratch,
        typed: &str,
        replies: Vec<Result<crate::provider::Reply, Box<crate::provider::ProviderError>>>,
        style: Style,
    ) -> Rig {
        let mut r = rig_styled(dir, typed, replies, style);
        let store = Store::open(&dir.path().join("harness.db")).unwrap();
        r.agent = r.agent.with_store(
            store,
            StoreContext {
                cwd: dir.path().display().to_string(),
            },
        );
        r
    }

    /// Saves a chat with these (question, answer) pairs, then forgets the program.
    fn seed(dir: &Scratch, pairs: &[(&str, &str)]) {
        let typed: String = pairs.iter().map(|(q, _)| format!("{q}\n")).collect();
        let replies = pairs
            .iter()
            .map(|(_, a)| ScriptedProvider::text(a))
            .collect();
        let mut r = rig_stored(dir, &typed, replies);
        run(&mut r.agent, &r.term).unwrap();
    }

    #[test]
    fn sessions_lists_the_saved_chats_and_marks_the_current_one() {
        let dir = Scratch::new("repl-sessions");
        seed(&dir, &[("what is rust?", "a language")]);
        seed(
            &dir,
            &[(
                "a very long first question that goes on and on and on forever",
                "ok",
            )],
        );
        let mut r = rig_stored(
            &dir,
            "/sessions\nhello\n/sessions\n/exit\n",
            vec![ScriptedProvider::text("hi")],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(out.contains("saved chats, newest first:\n"), "{out}");
        // Newest first, one line each, the first message on one line and cut short.
        // The scripted terminal does not echo what is typed, so the reply "hi" marks the boundary.
        let first_list = out.split("> hi\n").next().unwrap();
        let lines: Vec<&str> = first_list
            .lines()
            .filter(|l| l.contains("messages"))
            .collect();
        assert_eq!(lines.len(), 2, "{out}");
        assert!(
            lines[0].contains("\"a very long first question that goes on ...\""),
            "{}",
            lines[0]
        );
        assert!(lines[1].contains("\"what is rust?\""), "{}", lines[1]);
        assert!(
            lines[0].contains("  2 messages") && lines[0].contains(" UTC "),
            "{}",
            lines[0]
        );
        assert!(out.contains("continue one with /resume <id>"), "{out}");
        // After the first message, the new chat shows up at the top and is marked.
        let second_list = out.split("> hi\n").nth(1).unwrap();
        let marked: Vec<&str> = second_list
            .lines()
            .filter(|l| l.contains("(this chat)"))
            .collect();
        assert_eq!(marked.len(), 1, "{out}");
        assert!(marked[0].contains("\"hello\""), "{}", marked[0]);
        // Nothing was sent to the model for the two commands: only "hello" was.
        assert_eq!(r.requests.borrow().len(), 1);
    }

    #[test]
    fn sessions_with_nothing_saved_says_so() {
        let dir = Scratch::new("repl-sessions-none");
        let mut r = rig_stored(&dir, "/sessions\n/exit\n", vec![]);
        run(&mut r.agent, &r.term).unwrap();
        assert!(screen(&r.term).contains("no saved chats yet\n"));
    }

    #[test]
    fn resume_shows_the_saved_chat_and_the_next_message_carries_it() {
        let dir = Scratch::new("repl-resume");
        seed(
            &dir,
            &[("first question", "one"), ("second question", "two")],
        );
        let mut r = rig_stored(
            &dir,
            "/resume\nand a third\n/exit\n",
            vec![ScriptedProvider::text("three")],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(out.contains("resumed chat 1, last used "), "{out}");
        assert!(out.contains("(4 messages)"), "{out}");
        assert!(
            out.contains(
                "┃ first question\n\none\n\n┃ second question\n\ntwo\n[end of saved chat]\n"
            ),
            "{out}"
        );
        assert!(out.contains("three\n"), "{out}");
        // The model got the whole old chat plus the new message.
        let requests = r.requests.borrow();
        assert_eq!(requests.len(), 1);
        assert_eq!(tool_message(&requests[0], 1), "first question");
        assert_eq!(tool_message(&requests[0], 4), "two");
        assert_eq!(tool_message(&requests[0], 5), "and a third");
    }

    #[test]
    fn resume_with_an_id_picks_that_chat() {
        let dir = Scratch::new("repl-resume-id");
        seed(&dir, &[("the first chat", "a")]);
        seed(&dir, &[("the second chat", "b")]);
        let mut r = rig_stored(&dir, "/resume 1\n/exit\n", vec![]);
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(out.contains("resumed chat 1,"), "{out}");
        assert!(out.contains("┃ the first chat\n"), "{out}");
        assert!(!out.contains("the second chat"), "{out}");
    }

    #[test]
    fn a_bad_resume_prints_why_and_the_chat_goes_on() {
        let dir = Scratch::new("repl-resume-bad");
        seed(&dir, &[("keep", "kept")]);
        let mut r = rig_stored(
            &dir,
            "/resume abc\n/resume 99\n/resume 1 2\nstill here\n/exit\n",
            vec![ScriptedProvider::text("yes")],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(out.contains("usage: /resume [id]"), "{out}");
        assert!(out.contains("[error: no saved chat with id 99]"), "{out}");
        // "1 2" is not a number either.
        assert_eq!(out.matches("usage: /resume [id]").count(), 2, "{out}");
        assert!(out.contains("yes\n"), "{out}");
        // No command line was sent to the model; only "still here" was.
        assert_eq!(r.requests.borrow().len(), 1);
    }

    #[test]
    fn resume_with_nothing_saved_prints_an_error() {
        let dir = Scratch::new("repl-resume-none");
        let mut r = rig_stored(&dir, "/resume\n/exit\n", vec![]);
        run(&mut r.agent, &r.term).unwrap();
        assert!(screen(&r.term).contains("[error: there is no saved chat to resume]"));
    }

    #[test]
    fn without_a_database_the_commands_explain_themselves() {
        let dir = Scratch::new("repl-commands-nostore");
        let mut r = rig(&dir, "/sessions\n/resume\n/exit\n", vec![]);
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert_eq!(out.matches("not being saved").count(), 2, "{out}");
    }

    #[test]
    fn previews_are_one_line_and_cut_short() {
        assert_eq!(preview(None), "");
        assert_eq!(preview(Some("  two\n lines  ")), "\"two lines\"");
        let long = "x".repeat(60);
        assert_eq!(preview(Some(&long)), format!("\"{}...\"", "x".repeat(40)));
    }

    // ---- colour ----

    /// Removes colour codes (`ESC [ ... m`), leaving the text.
    fn strip_ansi(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for skipped in chars.by_ref() {
                    if skipped == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn a_coloured_chat_is_the_plain_chat_plus_colour_codes() {
        let typed =
            "run echo hi && touch hi.txt\ny\nnow read a.txt\nfail please\n/sessions\n/exit\n";
        let replies = || {
            vec![
                ScriptedProvider::tool_call(
                    "c1",
                    "bash",
                    r#"{"command":"echo hi && touch hi.txt"}"#,
                ),
                ScriptedProvider::text("It printed hi."),
                ScriptedProvider::tool_call("c2", "read_file", r#"{"path":"a.txt"}"#),
                ScriptedProvider::text("The file says: file text."),
                ScriptedProvider::failure("request failed: Connection refused"),
            ]
        };
        let run_with = |style: Style, tag: &str| {
            let dir = Scratch::new(tag);
            std::fs::write(dir.path().join("a.txt"), "file text\n").unwrap();
            let mut r = rig_styled(&dir, typed, replies(), style);
            run(&mut r.agent, &r.term).unwrap();
            screen(&r.term)
        };
        let plain = run_with(Style::plain(), "repl-colour-plain");
        let coloured = run_with(Style::colored(), "repl-colour-on");

        assert!(!plain.contains('\x1b'), "{plain}");
        assert_eq!(strip_ansi(&coloured), plain);
        // The pieces that should stand out do: prompt, tool lines, the question, errors.
        assert!(coloured.contains("\x1b[1;36m> \x1b[0m"), "{coloured}");
        assert!(
            coloured.contains("\x1b[2m▸ #1 bash echo hi && touch hi.txt\x1b[0m"),
            "{coloured}"
        );
        assert!(
            coloured.contains("\x1b[1mrun this command?\x1b[0m"),
            "{coloured}"
        );
        assert!(coloured.contains("\x1b[31m[error: "), "{coloured}");
    }

    #[test]
    fn a_coloured_chat_list_and_replay_are_the_plain_ones_plus_colour_codes() {
        let dir = Scratch::new("repl-colour-lists");
        seed(
            &dir,
            &[("first question", "one"), ("second question", "two")],
        );
        let typed = "/sessions\n/resume\n/resume 99\n/resume x\n/exit\n";
        let mut plain = rig_stored_styled(&dir, typed, vec![], Style::plain());
        run(&mut plain.agent, &plain.term).unwrap();
        let mut coloured = rig_stored_styled(&dir, typed, vec![], Style::colored());
        run(&mut coloured.agent, &coloured.term).unwrap();

        let (plain, coloured) = (screen(&plain.term), screen(&coloured.term));
        assert_eq!(strip_ansi(&coloured), plain);
        assert!(
            coloured.contains("\x1b[1msaved chats, newest first:\x1b[0m"),
            "{coloured}"
        );
        assert!(coloured.contains("\x1b[32mresumed chat 1,"), "{coloured}");
        assert!(
            coloured.contains("\x1b[36m┃\x1b[0m \x1b[36mfirst question"),
            "{coloured}"
        );
        assert!(
            coloured.contains("\x1b[2m[end of saved chat]\x1b[0m"),
            "{coloured}"
        );
        assert!(
            coloured.contains("\x1b[31m[error: no saved chat with id 99]"),
            "{coloured}"
        );
    }

    #[test]
    fn strip_ansi_removes_only_colour_codes() {
        assert_eq!(
            strip_ansi("\x1b[1;36m> \x1b[0mhi \x1b[2mthere\x1b[0m"),
            "> hi there"
        );
        assert_eq!(strip_ansi("plain"), "plain");
    }

    // ---- /help, /model, /models, /provider (issue 10) ----

    #[test]
    fn help_lists_every_command_once_and_says_what_else_happens() {
        let dir = Scratch::new("repl-help");
        let mut r = rig(&dir, "/help\n/exit\n", vec![]);
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        for (command, what) in COMMANDS {
            assert!(out.contains(command), "{command}");
            assert!(out.contains(what), "{what}");
        }
        assert!(out.contains("goes to the model"));
        assert!(
            r.requests.borrow().is_empty(),
            "/help is not sent to the model"
        );
    }

    #[test]
    fn init_sends_the_prepared_message_and_takes_no_arguments() {
        let dir = Scratch::new("repl-init");
        let mut r = rig(
            &dir,
            "/init\n/init now\n/exit\n",
            vec![ScriptedProvider::text("proposed nothing")],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(out.contains("/init does not take that"), "{out}");
        let requests = r.requests.borrow();
        assert_eq!(requests.len(), 1, "only the bare /init reaches the model");
        let sent = String::from_utf8_lossy(&requests[0]);
        assert!(sent.contains("Draft context for this project"), "{sent}");
        assert!(sent.contains("context_propose"), "{sent}");
    }

    #[test]
    fn approve_shows_and_sets_the_mode_by_typing() {
        let dir = Scratch::new("repl-approve-typed");
        let mut r = rig(
            &dir,
            "/approve\n/approve auto\n/approve auto\n/approve tools\n/approve manual\n/exit\n",
            vec![],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(
            out.contains("approval mode: manual (ask before every file change"),
            "{out}"
        );
        assert!(
            out.contains(
                "approval mode is now auto: file changes, glossary words and context entries are saved without asking; bash still asks"
            ),
            "{out}"
        );
        assert!(out.contains("approval mode stays auto"), "{out}");
        assert!(
            out.contains("usage: /approve [manual|auto|forget]"),
            "{out}"
        );
        assert!(out.contains("approval mode is now manual"), "{out}");
        assert_eq!(r.agent.approval_mode(), Mode::Manual);
        assert!(r.requests.borrow().is_empty());
    }

    #[test]
    fn approve_menu_picks_a_mode_and_escape_leaves_it() {
        let dir = Scratch::new("repl-approve-menu");
        // Down, Enter picks "auto"; then Esc leaves it alone.
        let mut r = rig(&dir, "/approve\n\x1b[B\r/approve\n\x1b/exit\n", vec![]);
        r.term.borrow_mut().enable_menus(false);
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(out.contains("approval mode is now auto"), "{out}");
        assert!(out.contains("unchanged"), "{out}");
        assert_eq!(r.agent.approval_mode(), Mode::Auto);
    }

    #[test]
    fn the_mode_is_remembered_between_runs_and_unknown_text_means_manual() {
        let dir = Scratch::new("repl-approve-saved");
        let mut r = rig_stored(&dir, "/approve auto\n/exit\n", vec![]);
        run(&mut r.agent, &r.term).unwrap();
        drop(r);

        let mut again = rig_stored(&dir, "/exit\n", vec![]);
        assert_eq!(again.agent.approval_mode(), Mode::Manual, "before loading");
        again.agent.load_approval_mode();
        assert_eq!(again.agent.approval_mode(), Mode::Auto);
        drop(again);

        let mut store = Store::open(&dir.path().join("harness.db")).unwrap();
        store
            .set_setting(crate::store::APPROVE_MODE, "tools")
            .unwrap();
        drop(store);
        let mut third = rig_stored(&dir, "/exit\n", vec![]);
        third.agent.load_approval_mode();
        assert_eq!(third.agent.approval_mode(), Mode::Manual);
    }

    #[test]
    fn manual_asks_before_a_file_change_and_auto_does_not() {
        let dir = Scratch::new("repl-approve-e2e");
        let create = |path: &str| {
            ScriptedProvider::tool_call(
                "c1",
                "create_file",
                &format!(r#"{{"path":"{path}","content":"hi"}}"#),
            )
        };
        let mut r = rig(
            &dir,
            "make a\ny\nmake b\nn\n/approve auto\nmake c\n/exit\n",
            vec![
                create("a.txt"),
                ScriptedProvider::text("done a"),
                create("b.txt"),
                ScriptedProvider::text("done b"),
                create("c.txt"),
                ScriptedProvider::text("done c"),
            ],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert_eq!(out.matches("create this file?").count(), 2, "{out}");
        assert!(out.contains("a.txt (2 bytes)"), "{out}");
        assert!(dir.path().join("a.txt").is_file());
        assert!(!dir.path().join("b.txt").exists(), "a no changes nothing");
        assert!(
            dir.path().join("c.txt").is_file(),
            "auto runs without asking"
        );
    }

    fn touch(path: &str) -> Result<crate::provider::Reply, Box<crate::provider::ProviderError>> {
        ScriptedProvider::tool_call("c1", "bash", &format!(r#"{{"command":"touch {path}"}}"#))
    }

    #[test]
    fn typed_answers_are_yes_always_or_no_and_always_is_listed_and_forgotten() {
        let dir = Scratch::new("repl-always-typed");
        let mut r = rig(
            &dir,
            "m1\na\nm2\nm3\n/approve\n/approve forget\n/approve forget\nm4\nn\n/exit\n",
            vec![
                touch("one"),
                ScriptedProvider::text("ok1"),
                touch("two"),
                ScriptedProvider::text("ok2"),
                touch("three"),
                ScriptedProvider::text("ok3"),
                touch("four"),
                ScriptedProvider::text("ok4"),
            ],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        // m1 is asked and answered "a"; m2 and m3 are not asked; after /approve forget m4 is.
        assert_eq!(out.matches("run this command?").count(), 2, "{out}");
        assert!(out.contains("[y/a/N] "), "{out}");
        assert!(
            out.contains("a = always allow `touch` commands until you quit"),
            "{out}"
        );
        assert!(
            out.contains("allowed until you quit: `touch` commands"),
            "{out}"
        );
        assert!(out.contains("forgot 1 \"always\" permission\n"), "{out}");
        assert!(out.contains("forgot 0 \"always\" permissions"), "{out}");
        assert!(dir.path().join("three").exists());
        assert!(
            !dir.path().join("four").exists(),
            "the last question was answered no"
        );
    }

    #[test]
    fn a_chain_is_offered_always_for_that_exact_line_and_a_typed_a_allows_it() {
        let dir = Scratch::new("repl-always-chain");
        let mut r = rig(
            &dir,
            "go\na\n/exit\n",
            vec![
                ScriptedProvider::tool_call("c1", "bash", r#"{"command":"touch x; touch y"}"#),
                ScriptedProvider::text("ok"),
            ],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(out.contains("[y/a/N] "), "{out}");
        assert!(
            out.contains("a = always allow this exact command (`touch x; touch y`) until you quit"),
            "{out}"
        );
        assert!(dir.path().join("x").exists() && dir.path().join("y").exists());
    }

    #[test]
    fn the_selector_answers_yes_always_and_no_and_escape_is_no() {
        let dir = Scratch::new("repl-selector");
        // The cursor starts on "yes", so Enter alone is yes. Keys after each question:
        // Enter = yes; Down Down Enter = no (rows: yes, always, no); Down Enter = always; Esc = no.
        let mut r = rig(
            &dir,
            "m1\n\rm2\n\x1b[B\x1b[B\rm3\n\x1b[B\rm4\nm5\n\x1b/exit\n",
            vec![
                touch("one"),
                ScriptedProvider::text("ok1"),
                ScriptedProvider::tool_call("c1", "bash", r#"{"command":"touch two; true"}"#),
                ScriptedProvider::text("ok2"),
                touch("three"),
                ScriptedProvider::text("ok3"),
                touch("four"),
                ScriptedProvider::text("ok4"),
                ScriptedProvider::tool_call("c1", "bash", r#"{"command":"cp one five"}"#),
                ScriptedProvider::text("ok5"),
            ],
        );
        r.term.borrow_mut().enable_menus(false);
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(dir.path().join("one").exists(), "Enter alone is yes: {out}");
        assert!(
            !dir.path().join("two").exists(),
            "Down Down Enter is no: {out}"
        );
        assert!(dir.path().join("three").exists(), "always: {out}");
        assert!(
            dir.path().join("four").exists(),
            "covered by the earlier always: {out}"
        );
        assert!(!dir.path().join("five").exists(), "Esc is no: {out}");
        assert!(out.contains("allow it?"), "{out}");
        assert!(
            out.contains("always: `touch` commands are allowed until you quit"),
            "{out}"
        );
        assert_eq!(out.matches("run this command?").count(), 4, "{out}");
    }

    #[test]
    fn the_view_row_shows_what_would_be_written_and_then_asks_again() {
        let dir = Scratch::new("repl-view");
        let create = |path: &str| {
            ScriptedProvider::tool_call(
                "c1",
                "create_file",
                &format!(r#"{{"path":"{path}","content":"SECRET\nBODY"}}"#),
            )
        };
        // Rows: yes, always, view, no (the cursor starts on yes).
        // m1: Down Down Enter = view, then Esc = no. m2: view, then Enter = yes.
        let mut r = rig(
            &dir,
            "m1\n\x1b[B\x1b[B\r\x1bm2\n\x1b[B\x1b[B\r\r/exit\n",
            vec![
                create("one.txt"),
                ScriptedProvider::text("ok1"),
                create("two.txt"),
                ScriptedProvider::text("ok2"),
            ],
        );
        r.term.borrow_mut().enable_menus(false);
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert_eq!(out.matches("SECRET\nBODY").count(), 2, "{out}");
        assert!(
            !out.contains("▸ #1 create_file one.txt · 11 bytes\n  SECRET"),
            "{out}"
        );
        assert!(!dir.path().join("one.txt").exists(), "{out}");
        assert!(dir.path().join("two.txt").exists(), "{out}");
        assert!(out.matches("allow it?").count() > 2, "{out}");
    }

    #[test]
    fn manual_remembers_always_per_file_tool() {
        let dir = Scratch::new("repl-always-files");
        let create = |path: &str| {
            ScriptedProvider::tool_call(
                "c1",
                "create_file",
                &format!(r#"{{"path":"{path}","content":"x"}}"#),
            )
        };
        let mut r = rig(
            &dir,
            "m1\na\nm2\ny\nm3\nn\n/exit\n",
            vec![
                create("a.txt"),
                ScriptedProvider::text("1"),
                create("b.txt"),
                ScriptedProvider::text("2"),
                ScriptedProvider::tool_call("c1", "delete_file", r#"{"path":"a.txt"}"#),
                ScriptedProvider::text("3"),
            ],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        // "always" covers one file now, so the second create is asked again.
        assert_eq!(out.matches("create this file?").count(), 2, "{out}");
        assert!(
            out.contains("a = always allow create_file on a.txt until you quit"),
            "{out}"
        );
        assert!(dir.path().join("b.txt").exists());
        assert!(
            dir.path().join("a.txt").exists(),
            "delete asked and was refused"
        );
    }

    #[test]
    fn the_working_prompt_stays_under_the_output_and_goes_when_the_work_ends() {
        let term = blank_term();
        term.borrow_mut().enable_menus(true);
        term.borrow_mut().begin_working(|_| "the info".to_owned());
        let after_begin = screen(&term);
        assert!(after_begin.contains("> working…"), "{after_begin:?}");
        assert!(after_begin.ends_with(" the info\r\n"), "{after_begin:?}");

        term.borrow_mut().say("a tool line\n");
        term.borrow_mut().say("no newline");
        let out = screen(&term);
        let added = &out[after_begin.len()..];
        // Each line: the box is erased, the line printed, the box drawn again below it.
        let erase = crate::inputbox::erase(4);
        assert!(
            added.starts_with(&format!("{erase}a tool line\n─")),
            "{added:?}"
        );
        assert!(
            added.contains(&format!("{erase}no newline\r\n─")),
            "{added:?}"
        );

        // While a question is shown the box is off the screen; afterwards it is back.
        term.borrow_mut().suspend_sticky();
        let mark = screen(&term).len();
        term.borrow_mut().say("question\n");
        assert_eq!(&screen(&term)[mark..], "question\n");
        term.borrow_mut().resume_sticky();
        assert!(screen(&term).ends_with(" the info\r\n"));

        let before_end = screen(&term).len();
        term.borrow_mut().end_working();
        assert_eq!(&screen(&term)[before_end..], erase);
        // With nothing kept at the bottom, output is printed as it always was.
        term.borrow_mut().say("plain\n");
        assert!(screen(&term).ends_with(&format!("{erase}plain\n")));
    }

    #[test]
    fn steps_are_over_the_working_prompt_and_tasks_under_it_and_the_steps_are_printed_once_at_the_end()
     {
        let dir = Scratch::new("repl-pinned");
        let shells = crate::tools::ShellTasks::new();
        shells.start("sleep 30", dir.path()).unwrap();
        let term = blank_term();
        term.borrow_mut().enable_menus(true);
        term.borrow_mut().set_tasks(TaskView {
            shells: shells.clone(),
            runs: Vec::new(),
        });
        term.borrow_mut().begin_working(|_| "the info".to_owned());
        let mut observer = PrintObserver(Rc::clone(&term));
        let first =
            r#"{"steps":[{"text":"read","status":"doing"},{"text":"write","status":"todo"}]}"#;
        observer.tool_call(&call("steps", first));
        let shown = screen(&term);
        // The checklist is the first thing of the drawing, then the prompt, the info, the task.
        let drawn = shown.rsplit("\x1b[J").next().unwrap();
        let rows: Vec<&str> = drawn.split("\r\n").collect();
        assert_eq!(rows[0], " steps 0/2");
        assert_eq!(rows[1], "  ▸ read");
        assert_eq!(rows[2], "  ○ write");
        assert!(rows[3].starts_with('─'), "{rows:?}");
        assert!(rows[5].starts_with('─'), "{rows:?}");
        assert_eq!(rows[6], " the info");
        assert!(rows[7].starts_with(" ● s1  sleep 30 · "), "{rows:?}");
        let second =
            r#"{"steps":[{"text":"read","status":"done"},{"text":"write","status":"doing"}]}"#;
        observer.tool_call(&call("steps", second));
        let drawn = screen(&term);
        let drawn = drawn.rsplit("\x1b[J").next().unwrap().to_owned();
        assert!(
            drawn.starts_with(" steps 1/2\r\n  ✓ read\r\n  ▸ write\r\n"),
            "{drawn:?}"
        );
        // Nothing of the steps went to the scrollback while the work went on.
        assert!(!screen(&term).contains("steps:\n"));
        term.borrow_mut().unpin_steps();
        term.borrow_mut().say("the answer\n");
        term.borrow_mut().end_working();
        let out = screen(&term);
        assert_eq!(out.matches("  ✓ read\n").count(), 1, "{out:?}");
        assert!(out.contains("the answer\n"));
        shells.stop_all();
    }

    #[test]
    fn the_task_list_names_each_running_task_and_counts_the_rest() {
        let dir = Scratch::new("repl-task-lines");
        let shells = crate::tools::ShellTasks::new();
        let view = |shells: &crate::tools::ShellTasks| TaskView {
            shells: shells.clone(),
            runs: Vec::new(),
        };
        assert!(view(&shells).lines(80).is_empty());
        for _ in 0..5 {
            shells.start("sleep 30", dir.path()).unwrap();
        }
        let lines = view(&shells).lines(80);
        assert_eq!(lines.len(), TASK_ROWS);
        assert!(lines[0].starts_with("● s1  sleep 30 · "), "{lines:?}");
        assert_eq!(lines[TASK_ROWS - 1], "… 2 more (/tasks)");
        shells.stop_all();
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(
            view(&shells).lines(80).is_empty(),
            "an ended task is not listed"
        );
    }

    #[test]
    fn a_long_checklist_is_cut_with_the_finished_steps_giving_way() {
        use crate::tools::{Step, StepStatus};
        let step = |text: &str, status| Step {
            text: text.to_owned(),
            status,
        };
        let steps: Vec<Step> = (0..8)
            .map(|i| {
                step(
                    &format!("s{i}"),
                    if i < 5 {
                        StepStatus::Done
                    } else {
                        StepStatus::Todo
                    },
                )
            })
            .collect();
        let lines = steps_block(Style::plain(), &steps, 6);
        assert_eq!(lines.len(), 6, "{lines:?}");
        assert_eq!(lines[0], " steps 5/8");
        assert_eq!(lines[1], "  ✓ s4");
        assert_eq!(lines[4], "  ○ s7");
        assert_eq!(lines[5], "  … 4 more");
        assert!(steps_block(Style::plain(), &[], 6).is_empty());
    }

    #[test]
    fn without_a_real_terminal_no_box_is_kept() {
        let term = blank_term();
        term.borrow_mut().begin_working(|_| "info".to_owned());
        term.borrow_mut().say("x\n");
        term.borrow_mut().end_working();
        assert_eq!(screen(&term), "x\n");
    }

    #[test]
    fn expand_shows_a_call_in_full_and_lists_and_refuses_bad_numbers() {
        let dir = Scratch::new("repl-expand");
        let mut r = rig(
            &dir,
            "go\ny\n/expand\n/expand 1\n/expand list\n/expand 9\n/expand x\n/exit\n",
            vec![
                ScriptedProvider::tool_call(
                    "c1",
                    "create_file",
                    r#"{"path":"a.txt","content":"hello\nworld"}"#,
                ),
                ScriptedProvider::text("done"),
            ],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        // The call itself is one collapsed line; the content is not on screen until expanded.
        assert!(out.contains("▸ #1 create_file a.txt · 11 bytes\n"), "{out}");
        assert!(!out.contains("  ✓ created a.txt (11 bytes)\n"), "{out}");
        let full = "#1 create_file\npath: a.txt\ncontent (11 bytes):\nhello\nworld\nresult:\ncreated a.txt (11 bytes)\n";
        assert_eq!(out.matches(full).count(), 2, "{out}");
        assert!(out.contains("#1 create_file a.txt · 11 bytes\n"), "{out}");
        assert!(
            out.contains("there is no call #9; this run has #1 to #1"),
            "{out}"
        );
        assert!(out.contains("usage: /expand [N|list]"), "{out}");
    }

    #[test]
    fn expand_before_any_call_says_so_and_calls_are_numbered_across_messages() {
        let dir = Scratch::new("repl-expand-none");
        let mut r = rig(
            &dir,
            "/expand\none\ntwo\n/expand 1\n/expand\n/exit\n",
            vec![
                ScriptedProvider::tool_call("c1", "read_file", r#"{"path":"a.txt"}"#),
                ScriptedProvider::text("1"),
                ScriptedProvider::tool_call("c2", "read_file", r#"{"path":"b.txt"}"#),
                ScriptedProvider::text("2"),
            ],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(out.contains("no tool calls in this run yet"), "{out}");
        assert!(out.contains("▸ #1 read_file a.txt"), "{out}");
        assert!(out.contains("▸ #2 read_file b.txt"), "{out}");
        assert!(
            out.contains("#1 read_file\npath: a.txt\nresult:\nerror:"),
            "{out}"
        );
        assert!(
            out.contains("#2 read_file\npath: b.txt\nresult:\nerror:"),
            "{out}"
        );
        assert!(out.contains("  ✗ error: cannot read 'a.txt'"), "{out}");
    }

    #[test]
    fn info_shows_the_numbers_of_the_calls_made_so_far() {
        let dir = Scratch::new("repl-info");
        let usage1 = r#"{"prompt_tokens":1200,"completion_tokens":30,"prompt_tokens_details":{"cached_tokens":1000}}"#;
        let usage2 = r#"{"prompt_tokens":1300,"completion_tokens":20}"#;
        let mut r = rig(
            &dir,
            "/info\none\ntwo\n/info\n/approve auto\n/info\n/info extra\n/exit\n",
            vec![
                ScriptedProvider::with_usage(ScriptedProvider::text("a"), usage1),
                ScriptedProvider::with_usage(ScriptedProvider::text("b"), usage2),
            ],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(
            out.contains("m · test │ approve: manual │ last call: none yet │ chat: 0 in · 0 out │ context: not sent yet\n"),
            "{out}"
        );
        assert!(
            out.contains("m · test │ approve: manual │ last call: 1,300 in · 20 out │ chat: 2,500 in · 50 out │ context ≈ 1,300\n"),
            "{out}"
        );
        assert!(out.contains("approve: auto │"), "{out}");
        assert!(out.contains("/info does not take that"), "{out}");
    }

    #[test]
    fn the_totals_start_again_with_a_new_chat_and_come_back_with_a_resume() {
        let dir = Scratch::new("repl-info-resume");
        let usage = r#"{"prompt_tokens":10,"completion_tokens":2}"#;
        let mut r = rig_stored(
            &dir,
            "hello\n/new\n/info\n/resume\n/info\n/exit\n",
            vec![ScriptedProvider::with_usage(
                ScriptedProvider::text("a"),
                usage,
            )],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        let infos: Vec<&str> = out.lines().filter(|l| l.contains("m · test │")).collect();
        assert_eq!(infos.len(), 2, "{out}");
        assert!(infos[0].contains("chat: 0 in · 0 out"), "{out}");
        assert!(infos[1].contains("chat: 10 in · 2 out"), "{out}");
        assert!(infos[1].contains("last call: 10 in · 2 out"), "{out}");
    }

    #[test]
    fn bash_still_asks_in_auto_mode() {
        let dir = Scratch::new("repl-approve-bash");
        let mut r = rig(
            &dir,
            "/approve auto\nrun it\nn\n/exit\n",
            vec![
                ScriptedProvider::tool_call("c1", "bash", r#"{"command":"touch ran"}"#),
                ScriptedProvider::text("ok"),
            ],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(out.contains("run this command?"), "{out}");
        assert!(!dir.path().join("ran").exists());
    }

    #[test]
    fn a_misspelled_command_is_refused_and_not_sent_to_the_model() {
        let dir = Scratch::new("repl-unknown-slash");
        let mut r = rig(
            &dir,
            "/foo bar\n/skill\n/new extra\n/exit\n",
            vec![ScriptedProvider::text("ok")],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(
            out.contains("unknown command /foo; /help lists the commands"),
            "{out}"
        );
        assert!(out.contains("unknown command /skill;"), "{out}");
        assert!(out.contains("/new does not take that"), "{out}");
        assert!(r.requests.borrow().is_empty());
    }

    #[test]
    fn a_path_or_a_plain_message_still_goes_to_the_model() {
        let dir = Scratch::new("repl-slash-path");
        let mut r = rig(
            &dir,
            "/etc/hosts what is this\n//x\nhello /foo\n/exit\n",
            vec![
                ScriptedProvider::text("1"),
                ScriptedProvider::text("2"),
                ScriptedProvider::text("3"),
            ],
        );
        run(&mut r.agent, &r.term).unwrap();
        assert_eq!(r.requests.borrow().len(), 3);
    }

    #[test]
    fn looking_like_a_command_means_a_slash_and_a_plain_word() {
        for yes in ["/foo", "/a-b_c1", "/Skill"] {
            assert!(looks_like_command(yes), "{yes}");
        }
        for no in ["/", "//x", "/etc/hosts", "foo", "/a.b", ""] {
            assert!(!looks_like_command(no), "{no}");
        }
    }

    #[test]
    fn model_shows_the_model_and_switches_it_with_a_warning() {
        let dir = Scratch::new("repl-model");
        let mut r = rig(
            &dir,
            "/model\n/model other\n/model other\n/model a b\nhi\n/exit\n",
            vec![ScriptedProvider::text("hello")],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(out.contains("model:    m\nprovider: test (x)"), "{out}");
        assert!(
            out.contains("model is now other\nwarning: the next request differs"),
            "{out}"
        );
        assert!(out.contains("[already using other]"), "{out}");
        assert!(out.contains("usage: /model <name>"), "{out}");
        let sent = r.requests.borrow();
        let value: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
        assert_eq!(value["model"], "other");
    }

    fn many_models() -> Vec<String> {
        let mut ids: Vec<String> = (0..35).map(|n| format!("vendor/model-{n:02}")).collect();
        ids.push("anthropic/claude-x".into());
        ids.push("m".into());
        ids
    }

    #[test]
    fn models_are_listed_filtered_capped_and_the_one_in_use_marked() {
        let dir = Scratch::new("repl-models");
        let provider = ScriptedProvider::new(vec![]).with_models(Ok(many_models()));
        let mut r = rig_with_provider(
            &dir,
            "/models\n/models CLAUDE\n/models zzz\n/models model-3\n/models m\n/exit\n",
            provider,
            Style::plain(),
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        // No filter: the first 30 of 37, and a hint.
        assert!(out.contains("30 of 37 matching models shown"), "{out}");
        assert!(out.contains("  anthropic/claude-x\n"), "{out}");
        let first = out.split("30 of 37").next().unwrap();
        assert!(first.contains("vendor/model-27"), "{first}");
        assert!(
            !first.contains("vendor/model-28"),
            "cut off by the cap: {first}"
        );
        // Filter is case-insensitive and shows a plain count.
        assert!(out.contains("  anthropic/claude-x\n1 model\n"), "{out}");
        assert!(
            out.contains("none of the provider's 37 models contain 'zzz'"),
            "{out}"
        );
        assert!(
            out.contains("vendor/model-30") && out.contains("vendor/model-34"),
            "{out}"
        );
        assert!(out.contains("  m  (in use)\n"), "{out}");
    }

    #[test]
    fn models_says_so_when_the_provider_cannot_list_them() {
        let dir = Scratch::new("repl-models-error");
        let mut r = rig(&dir, "/models\n/exit\n", vec![]);
        run(&mut r.agent, &r.term).unwrap();
        assert!(screen(&r.term).contains("[error: this provider cannot list its models]"));
        let provider = ScriptedProvider::new(vec![]).with_models(Err(
            "the router answered the model list with status 404".into(),
        ));
        let mut r = rig_with_provider(&dir, "/models\n/exit\n", provider, Style::plain());
        run(&mut r.agent, &r.term).unwrap();
        assert!(screen(&r.term).contains("status 404"));
    }

    type Built = Rc<RefCell<Vec<(String, crate::agent::Shared<Vec<Vec<u8>>>)>>>;

    /// An agent that saves into the scratch folder and can switch among scripted providers:
    /// each provider built answers "from <name>".
    fn switching_rig(dir: &Scratch, typed: &str, style: Style) -> (Rig, Built) {
        let mut r = rig_stored_styled(
            dir,
            typed,
            vec![ScriptedProvider::text("from start")],
            style,
        );
        let built: Built = Rc::new(RefCell::new(Vec::new()));
        let seen = Rc::clone(&built);
        let catalog = crate::providers::Catalog {
            provider_dir: Some(dir.path().join("providers")),
            build: Box::new(move |def| {
                let provider = ScriptedProvider::new(vec![ScriptedProvider::text(&format!(
                    "from {}",
                    def.name
                ))]);
                seen.borrow_mut()
                    .push((def.name.clone(), provider.requests()));
                Box::new(provider)
            }),
        };
        r.agent = r.agent.with_catalog(catalog);
        (r, built)
    }

    fn provider_file(dir: &Scratch, name: &str, text: &str) {
        let folder = dir.path().join("providers");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join(format!("{name}.provider")), text).unwrap();
    }

    #[test]
    fn providers_are_listed_switched_added_and_removed() {
        let dir = Scratch::new("repl-providers");
        provider_file(
            &dir,
            "other",
            "HARNESS_BASE_URL=http://other:9/v1\nHARNESS_MODEL=other-model\n",
        );
        provider_file(&dir, "broken", "HARNESS_MODEL=m\n");
        let typed = "/provider\n/provider add mine http://mine.example/v1 mine-model\nSECRET-KEY-777\n\
                     /provider\n/provider other\n/provider remove mine\n/provider remove other\n\
                     /provider nope\n/provider broken\nhello\n/exit\n";
        let (mut r, built) = switching_rig(&dir, typed, Style::plain());
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);

        // The first list: both provider files, the broken one explained, nothing saved yet.
        assert!(out.contains("providers:\n  broken  file     cannot be used: HARNESS_BASE_URL is missing\n  other   file     other:9  other-model\n"), "{out}");
        // Adding asks for the key on its own prompt, says where the input shows, saves, switches.
        assert!(
            out.contains("API key for mine, Enter for none (visible): "),
            "{out}"
        );
        assert!(out.contains("saved provider mine\nnow using mine (mine.example) with model mine-model\nwarning: the next request differs"), "{out}");
        // The second list marks the saved one in use.
        assert!(
            out.contains("  mine    saved    mine.example  mine-model  (in use)\n"),
            "{out}"
        );
        // Switching to a provider file, removing, and the failure messages.
        assert!(
            out.contains("now using other (other:9) with model other-model"),
            "{out}"
        );
        assert!(out.contains("removed provider mine"), "{out}");
        assert!(
            out.contains("[error: other is a provider file; delete the file to remove it]"),
            "{out}"
        );
        assert!(
            out.contains("[error: no provider named nope (see /provider)]"),
            "{out}"
        );
        assert!(
            out.contains("[error: broken cannot be used: HARNESS_BASE_URL is missing]"),
            "{out}"
        );
        // The chat went on with the provider that was current: "other".
        assert!(out.contains("from other\n"), "{out}");
        assert_eq!(
            built
                .borrow()
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>(),
            ["mine", "other"]
        );
        // The key is not on the screen and not in the log; it is in the database row until removed.
        assert!(!out.contains("SECRET-KEY"), "{out}");
        let log = std::fs::read_to_string(r.agent.log_path()).unwrap_or_default();
        assert!(!log.contains("SECRET-KEY"));
    }

    #[test]
    fn an_added_provider_keeps_its_key_in_the_database_only() {
        let dir = Scratch::new("repl-provider-key");
        let typed = "/provider add mine http://mine/v1 mm\n  SECRET-KEY-777  \n/provider add free http://free/v1 ff\n\n/exit\n";
        let (mut r, _) = switching_rig(&dir, typed, Style::plain());
        run(&mut r.agent, &r.term).unwrap();
        let store = Store::open(&dir.path().join("harness.db")).unwrap();
        let saved = store.list_providers().unwrap();
        assert_eq!(saved.len(), 2);
        let mine = saved.iter().find(|p| p.name == "mine").unwrap();
        assert_eq!(mine.api_key.as_deref(), Some("SECRET-KEY-777"), "trimmed");
        let free = saved.iter().find(|p| p.name == "free").unwrap();
        assert_eq!(free.api_key, None, "an empty answer means no key");
        assert!(!screen(&r.term).contains("SECRET-KEY"));
    }

    #[test]
    fn a_provider_is_added_with_the_api_it_speaks() {
        let dir = Scratch::new("repl-provider-api");
        let typed = "/provider add claude https://api.anthropic.com/v1 claude-sonnet-5-5 anthropic\nKEY\n/provider add plain http://p/v1 m openai\n\n/provider add odd http://o/v1 m gemini\n/exit\n";
        let (mut r, _) = switching_rig(&dir, typed, Style::plain());
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(out.contains("openai or anthropic"), "{out}");
        let store = Store::open(&dir.path().join("harness.db")).unwrap();
        let saved = store.list_providers().unwrap();
        let api = |name: &str| saved.iter().find(|p| p.name == name).map(|p| p.api);
        assert_eq!(api("claude"), Some(crate::config::ApiStyle::Anthropic));
        assert_eq!(api("plain"), Some(crate::config::ApiStyle::OpenAi));
        assert_eq!(api("odd"), None, "a bad api saves nothing");
    }

    #[test]
    fn a_bad_add_is_refused_before_a_key_is_asked_for() {
        let dir = Scratch::new("repl-provider-bad-add");
        let typed = "/provider add x ftp://bad m\n/provider add ../x http://h/v1 m\n/provider add x http://h/v1\n/provider add add http://h/v1 m\nhello\n/exit\n";
        let (mut r, _) = switching_rig(&dir, typed, Style::plain());
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(!out.contains("API key for"), "{out}");
        assert!(
            out.contains("[error: the base URL must start with http:// or https://]"),
            "{out}"
        );
        assert!(
            out.contains("usage: /provider add <name> <base_url> <model>"),
            "{out}"
        );
        // Nothing was swallowed as a key: "hello" reached the model.
        assert_eq!(r.requests.borrow().len(), 1);
        let store = Store::open(&dir.path().join("harness.db")).unwrap();
        assert!(store.list_providers().unwrap().is_empty());
    }

    #[test]
    fn ending_the_input_at_the_key_prompt_cancels_the_add() {
        let dir = Scratch::new("repl-provider-cancel");
        let (mut r, _) = switching_rig(&dir, "/provider add x http://h/v1 m\n", Style::plain());
        run(&mut r.agent, &r.term).unwrap();
        assert!(screen(&r.term).contains("[cancelled: nothing was saved]"));
        let store = Store::open(&dir.path().join("harness.db")).unwrap();
        assert!(store.list_providers().unwrap().is_empty());
    }

    #[test]
    fn provider_commands_without_a_catalog_say_so() {
        let dir = Scratch::new("repl-provider-none");
        let mut r = rig(&dir, "/provider\n/provider x\n/exit\n", vec![]);
        run(&mut r.agent, &r.term).unwrap();
        assert_eq!(
            screen(&r.term)
                .matches("this run cannot switch providers")
                .count(),
            2
        );
    }

    #[test]
    fn the_new_commands_look_the_same_with_and_without_colour() {
        let script = "/help\n/model\n/model other\n/models\n/provider\n/provider add mine http://mine/v1 mm\nk\n/provider\n/provider nope\n/exit\n";
        let run_with = |style: Style, tag: &str| {
            let dir = Scratch::new(tag);
            provider_file(
                &dir,
                "other",
                "HARNESS_BASE_URL=http://other/v1\nHARNESS_MODEL=om\n",
            );
            let provider = ScriptedProvider::new(vec![]).with_models(Ok(many_models()));
            let mut r = rig_with_provider(&dir, script, provider, style);
            let store = Store::open(&dir.path().join("harness.db")).unwrap();
            r.agent = r.agent.with_store(store, StoreContext { cwd: "/w".into() });
            r.agent = r.agent.with_catalog(crate::providers::Catalog {
                provider_dir: Some(dir.path().join("providers")),
                build: Box::new(|_| Box::new(ScriptedProvider::new(vec![]))),
            });
            run(&mut r.agent, &r.term).unwrap();
            screen(&r.term)
        };
        let plain = run_with(Style::plain(), "repl-new-plain");
        let coloured = run_with(Style::colored(), "repl-new-coloured");
        assert_ne!(plain, coloured, "colour was applied");
        assert_eq!(strip_ansi(&coloured), plain);
    }

    #[test]
    fn a_secret_is_read_as_one_line_and_never_echoed_by_the_terminal_type() {
        let term = Terminal::shared(Cursor::new(b"abc\n".to_vec()), Vec::new());
        let answer = term.borrow_mut().ask_secret("key");
        assert_eq!(answer.as_deref(), Some("abc"));
        assert_eq!(screen(&term), "key (visible): ");
        let ended = Terminal::shared(Cursor::new(Vec::new()), Vec::new());
        assert_eq!(ended.borrow_mut().ask_secret("key"), None);
    }

    #[test]
    fn with_no_provider_a_message_is_refused_and_the_commands_explain() {
        let dir = Scratch::new("repl-no-provider");
        let log = TurnLog::create(&dir.path().join("logs")).unwrap();
        let term = Terminal::shared(
            Cursor::new(b"hello\n/model\n/model x\n/models\n/exit\n".to_vec()),
            Vec::new(),
        );
        let mut agent = Agent::new("", Box::new(crate::provider::NoProvider), vec![], log);
        run(&mut agent, &term).unwrap();
        let out = screen(&term);
        assert_eq!(out.matches("no provider selected").count(), 4, "{out}");
        assert!(out.contains("[error: no provider selected"), "{out}");
    }
    #[test]
    fn a_replay_shows_a_subagent_answer_as_one_line_not_as_typed_text() {
        let delivery = crate::jobs::delivery_text(&[crate::jobs::Delivery {
            n: 3,
            profile: "reviewer".to_owned(),
            coder: false,
            status: crate::jobs::Status::Done,
            text: "**A - CONFLICT.** and more".to_owned(),
            prompt_tokens: 0,
            completion_tokens: 0,
        }]);
        let lines = delivery_lines(&delivery).unwrap();
        assert_eq!(
            lines,
            ["[subagent #3 (reviewer, reader) finished: answer added to the chat, 26 characters]"]
        );
        assert!(delivery_lines("Subagent #3 is a fine name").is_none());
        assert!(delivery_lines("hello").is_none());
    }

    // ---- short calls and steps (issue 42) ----

    fn steps_call(
        id: &str,
        list: &str,
    ) -> Result<crate::provider::Reply, Box<crate::provider::ProviderError>> {
        ScriptedProvider::tool_call(id, "steps", list)
    }

    #[test]
    fn calls_all_shows_the_results_again_and_calls_with_no_argument_says_which() {
        let dir = Scratch::new("repl-calls-all");
        let mut r = rig(
            &dir,
            "/calls\n/calls all\n/calls\nmake it\ny\n/calls short\n/calls what\n/exit\n",
            vec![
                ScriptedProvider::tool_call(
                    "c1",
                    "create_file",
                    r#"{"path":"a.txt","content":"x"}"#,
                ),
                ScriptedProvider::text("done"),
            ],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(out.contains("tool calls are shown: short"), "{out}");
        assert!(out.contains("every tool call and result is shown"), "{out}");
        assert!(out.contains("tool calls are shown: all"), "{out}");
        assert!(out.contains("  ✓ created a.txt (1 bytes)\n"), "{out}");
        assert!(out.contains("one line for each call"), "{out}");
        assert!(out.contains("usage: /calls [all | short]"), "{out}");
    }

    #[test]
    fn while_steps_are_unfinished_calls_are_hidden_but_a_failure_shows_itself() {
        let dir = Scratch::new("repl-steps");
        std::fs::write(dir.path().join("a.txt"), "text").unwrap();
        let first =
            r#"{"steps":[{"text":"read","status":"doing"},{"text":"write","status":"todo"}]}"#;
        let second =
            r#"{"steps":[{"text":"read","status":"done"},{"text":"write","status":"doing"}]}"#;
        let last =
            r#"{"steps":[{"text":"read","status":"done"},{"text":"write","status":"done"}]}"#;
        let mut r = rig(
            &dir,
            "go\ngo again\n/exit\n",
            vec![
                steps_call("s1", first),
                ScriptedProvider::tool_call("c1", "read_file", r#"{"path":"a.txt"}"#),
                ScriptedProvider::tool_call("c2", "read_file", r#"{"path":"missing.txt"}"#),
                steps_call("s2", second),
                steps_call("s3", last),
                ScriptedProvider::tool_call("c3", "read_file", r#"{"path":"a.txt"}"#),
                ScriptedProvider::text("done"),
                ScriptedProvider::text("again"),
            ],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(out.contains("steps:\n  ▸ read\n  ○ write\n"), "{out}");
        assert!(
            !out.contains("▸ #2 read_file a.txt"),
            "a call is hidden while the steps are unfinished: {out}"
        );
        assert!(
            out.contains("▸ #3 read_file missing.txt"),
            "a failure shows its call: {out}"
        );
        assert!(out.contains("  ✗ error:"), "{out}");
        assert!(out.contains("  ✓ read\n  ▸ write\n  1/2 done\n"), "{out}");
        assert!(out.contains("  ✓ write\n  2/2 done\n"), "{out}");
        assert!(
            out.contains("▸ #6 read_file a.txt"),
            "with every step done the calls show again: {out}"
        );
    }

    // ---- plan mode (issue 42) ----

    #[test]
    fn plan_mode_refuses_changes_adds_its_text_and_build_leaves_it_and_goes_on() {
        let dir = Scratch::new("repl-plan-build");
        let mut r = rig(
            &dir,
            "/plan\nadd a file\nb\n/exit\n",
            vec![
                ScriptedProvider::tool_call(
                    "c1",
                    "create_file",
                    r#"{"path":"a.txt","content":"x"}"#,
                ),
                ScriptedProvider::tool_call("c2", "bash", r#"{"command":"touch b.txt"}"#),
                ScriptedProvider::text("1. create a.txt"),
                ScriptedProvider::text("built"),
            ],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(
            out.contains("plan mode is on: the model can only look"),
            "{out}"
        );
        assert!(
            out.contains(
                "plan mode is on, so nothing was changed; put the change in your plan instead"
            ),
            "{out}"
        );
        assert!(out.contains("only looking commands"), "{out}");
        assert!(!dir.path().join("a.txt").exists() && !dir.path().join("b.txt").exists());
        assert!(out.contains("The plan is ready"), "{out}");
        assert!(out.contains("plan mode is off: building"), "{out}");
        assert!(!r.agent.planning());
        let requests = r.requests.borrow();
        let text = |i: usize| String::from_utf8_lossy(&requests[i]).into_owned();
        assert!(
            text(0).contains("Plan mode is on."),
            "the plan text rides along"
        );
        assert!(text(2).contains("Plan mode is on."));
        assert!(
            !text(3).contains("Plan mode is on."),
            "it is gone once plan mode is off"
        );
        assert!(text(3).contains("Carry out the plan you just wrote"));
    }

    #[test]
    fn discussing_keeps_plan_mode_and_an_agent_needs_auto_mode() {
        let dir = Scratch::new("repl-plan-discuss");
        let mut r = rig(
            &dir,
            "/plan\none\n\ntwo\na\n/plan what\n/plan off\n/info\n/exit\n",
            vec![
                ScriptedProvider::text("plan one"),
                ScriptedProvider::text("plan two"),
            ],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(out.contains("still in plan mode: type to discuss"), "{out}");
        assert!(
            out.contains("a subagent changes files only in /approve auto"),
            "{out}"
        );
        assert!(out.contains("usage: /plan [on | off]"), "{out}");
        assert!(out.contains("plan mode is off\n"), "{out}");
        assert!(!r.agent.planning());
        assert_eq!(r.requests.borrow().len(), 2);
    }

    #[test]
    fn the_info_line_says_when_plan_mode_is_on() {
        let dir = Scratch::new("repl-plan-info");
        let r = rig(&dir, "", vec![]);
        assert!(!info_text(&r.agent, None).contains("plan mode"));
        r.agent.set_planning(true);
        assert!(info_text(&r.agent, None).contains("approve: manual · plan mode"));
    }

    // ---- /tasks (issue 42) ----

    #[test]
    fn tasks_lists_shows_and_stops_background_commands_and_quitting_asks() {
        let dir = Scratch::new("repl-tasks-run");
        let mut r = rig(
            &dir,
            "/tasks\n/tasks show s1\n/tasks stop s1\n/tasks show s9\n/tasks stop\n/tasks show 1\n/exit\nn\n/tasks stop s2\n/exit\n",
            vec![],
        );
        assert_eq!(r.agent.shells().running(), 0);
        let first = r.agent.shells().start("echo up", dir.path()).unwrap();
        assert_eq!(first, 1);
        let second = r.agent.shells().start("sleep 30", dir.path()).unwrap();
        assert_eq!(second, 2);
        std::thread::sleep(std::time::Duration::from_millis(500));
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(out.contains("s2  running"), "{out}");
        assert!(out.contains("up\n"), "{out}");
        assert!(out.contains("s1 is not running"), "{out}");
        assert!(out.contains("there is no task s9"), "{out}");
        assert!(
            out.contains("usage: /tasks [show <id> | stop <id>]"),
            "{out}"
        );
        assert!(
            out.contains("1 background command still running; quitting stops 1"),
            "{out}"
        );
        assert!(out.contains("not quitting; /tasks shows them"), "{out}");
    }

    #[test]
    fn tasks_says_so_when_nothing_ran() {
        let dir = Scratch::new("repl-tasks-none");
        let mut r = rig(&dir, "/tasks\n/exit\n", vec![]);
        run(&mut r.agent, &r.term).unwrap();
        assert!(screen(&r.term).contains("nothing has run in the background"));
    }

    // ---- /subagent and /subagents (issue 35) ----

    /// An agent that can start subagents and a terminal that types `typed`.
    fn background_rig(
        dir: &Scratch,
        typed: &str,
        split: crate::agent::background_tests::Split,
    ) -> (Agent, TestTerm) {
        let agent = crate::agent::background_tests::background_agent(dir, split);
        let term = Terminal::shared(Cursor::new(typed.as_bytes().to_vec()), Vec::new());
        (agent, term)
    }

    #[test]
    fn the_commands_start_list_show_and_stop_a_run_and_quitting_asks() {
        use crate::agent::background_tests::Split;
        let dir = Scratch::new("repl-subagents");
        let (split, h) = Split::new(vec![], vec![ScriptedProvider::text("the plan")]);
        let (mut agent, term) = background_rig(
            &dir,
            "/subagents\n/subagent\n/subagent planner look into x\nsecond line\n/subagents\n/subagents show 1\n/subagents show 7\n/subagents stop x\n/exit\nn\n/subagents stop 1\n/exit\ny\n",
            split,
        );
        run(&mut agent, &term).unwrap();
        let out = screen(&term);
        assert!(out.contains("no subagents started in this run"), "{out}");
        assert!(out.contains("usage: /subagent [profile] <task>"), "{out}");
        assert!(
            out.contains("subagent #1 started (planner, reader: it only looks)"),
            "{out}"
        );
        assert!(out.contains("#1 planner (reader) running"), "{out}");
        assert!(out.contains("task: look into x"), "{out}");
        assert!(out.contains("there is no subagent #7"), "{out}");
        assert!(out.contains("usage: /subagents stop <number>"), "{out}");
        assert!(
            out.contains("1 subagent still running; quitting stops 1. Quit anyway?"),
            "{out}"
        );
        assert!(out.contains("not quitting"), "{out}");
        assert!(
            out.contains("stopping subagent #1; its answer is not added to the chat"),
            "{out}"
        );
        drop(h);
    }

    #[test]
    fn the_end_of_input_asks_once_and_quits() {
        use crate::agent::background_tests::Split;
        let dir = Scratch::new("repl-subagents-eof");
        let (split, h) = Split::new(vec![], vec![ScriptedProvider::text("the plan")]);
        let (mut agent, term) = background_rig(&dir, "/subagent planner look\n", split);
        run(&mut agent, &term).unwrap();
        let out = screen(&term);
        assert_eq!(
            out.matches("still running; quitting stops 1").count(),
            1,
            "{out}"
        );
        drop(h);
    }

    #[test]
    fn the_info_line_counts_the_runs_that_are_going() {
        use crate::agent::background_tests::Split;
        let dir = Scratch::new("repl-subagents-info");
        let (split, h) = Split::new(vec![], vec![ScriptedProvider::text("the plan")]);
        let (mut agent, _term) = background_rig(&dir, "", split);
        assert!(!info_text(&agent, None).contains("subagents"));
        agent.start_subagent("planner look").unwrap();
        assert!(info_text(&agent, None).ends_with("subagents: 1 running"));
        drop(h);
    }

    #[test]
    fn a_finished_subagent_wakes_the_model_without_the_owner_typing() {
        use crate::agent::background_tests::{Split, wait_for};
        let dir = Scratch::new("repl-wake");
        let (split, h) = Split::new(
            vec![ScriptedProvider::text("Noted the plan.")],
            vec![ScriptedProvider::text("the plan")],
        );
        let (mut agent, term) = background_rig(&dir, "/exit\n", split);
        agent.start_subagent("planner plan it").unwrap();
        h.release();
        wait_for(&agent, 1, crate::jobs::Status::Done);
        run(&mut agent, &term).unwrap();
        let out = screen(&term);
        assert!(
            out.contains("[a subagent finished: its answer goes to the model]"),
            "{out}"
        );
        assert!(out.contains("Noted the plan."), "{out}");
        let seen = h.main_seen.borrow();
        assert_eq!(seen.len(), 1, "one message, sent once");
        let text = String::from_utf8_lossy(&seen[0]).into_owned();
        assert!(
            text.contains("Subagent #1 (planner, reader) finished."),
            "{text}"
        );
        assert!(text.contains("the plan"), "{text}");
        assert!(
            text.contains("A subagent you started has finished"),
            "{text}"
        );
    }

    #[test]
    fn a_woken_message_that_fails_is_not_tried_again() {
        use crate::agent::background_tests::{Split, wait_for};
        let dir = Scratch::new("repl-wake-fail");
        let (split, h) = Split::new(
            vec![
                ScriptedProvider::failure("boom"),
                ScriptedProvider::text("never asked for"),
            ],
            vec![ScriptedProvider::text("the plan")],
        );
        let (mut agent, term) = background_rig(&dir, "/exit\n", split);
        agent.start_subagent("planner plan it").unwrap();
        h.release();
        wait_for(&agent, 1, crate::jobs::Status::Done);
        run(&mut agent, &term).unwrap();
        assert_eq!(h.main_seen.borrow().len(), 1);
    }

    #[test]
    fn automatic_messages_are_capped_until_the_owner_types() {
        use crate::agent::background_tests::{Split, wait_for};
        let dir = Scratch::new("repl-wake-count");
        let (split, h) = Split::new(vec![], vec![ScriptedProvider::text("the plan")]);
        let (mut agent, _term) = background_rig(&dir, "", split);
        agent.start_subagent("planner plan it").unwrap();
        h.release();
        wait_for(&agent, 1, crate::jobs::Status::Done);
        assert!(agent.wake_ready());
        for _ in 0..crate::agent::MAX_AUTO_WAKES {
            agent.note_wake();
        }
        assert!(!agent.wake_ready());
        agent.reset_wakes();
        assert!(agent.wake_ready());
        agent.stop_wakes();
        assert!(!agent.wake_ready());
    }
    #[test]
    fn context_entries_are_added_listed_shown_switched_and_deleted_by_typing() {
        let dir = Scratch::new("repl-context");
        let mut r = rig_stored(
            &dir,
            "/context\n/context add stay-small\nrule\nKeep every change small.\n.\nsmall changes please\n\n\n/context\n/context show STAY-SMALL\n/context preload stay-small on\n/context\n/context add stay-small\n/context delete stay-small\ny\n/context show stay-small\n/context bogus\n/exit\n",
            vec![],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(out.contains("no context entries here yet"), "{out}");
        assert!(out.contains("added stay-small"), "{out}");
        assert!(
            out.contains("stay-small  rule · here · ~6 tokens  Keep every change small."),
            "{out}"
        );
        assert!(
            out.contains("stay-small (rule) · this project · preload off"),
            "{out}"
        );
        assert!(
            out.contains("the owner said: small changes please"),
            "{out}"
        );
        assert!(
            out.contains("stay-small will be in every subagent's start"),
            "{out}"
        );
        assert!(out.contains("rule · here · ~6 tokens preload"), "{out}");
        assert!(out.contains("'stay-small' is here already"), "{out}");
        assert!(out.contains("deleted stay-small"), "{out}");
        assert!(out.contains("there is no entry 'stay-small' here"), "{out}");
        assert!(out.contains("usage: /context [list"), "{out}");
        // The entry belongs to this project's folder.
        let store = Store::open(&dir.path().join("harness.db")).unwrap();
        assert!(store.list_all_context().unwrap().is_empty());
    }

    #[test]
    fn the_context_menu_adds_an_entry_switches_its_preload_and_deletes_it() {
        let dir = Scratch::new("repl-context-menu");
        // Enter on "+ add new", then the questions; Enter on the entry, down + Enter on "preload on";
        // Enter on the entry again, down twice + Enter on "delete" and "y"; Esc leaves the menu.
        let mut r = rig_stored(
            &dir,
            "/context\n\rkeep-small\ndecision\nPrefer small steps.\n.\n\n\n\n\r\x1b[B\r\r\x1b[B\x1b[B\ry\n\x1b/exit\n",
            vec![],
        );
        r.term.borrow_mut().enable_menus(false);
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(out.contains("added keep-small"), "{out}");
        assert!(out.contains("context, 1 entries"), "{out}");
        assert!(
            out.contains("keep-small will be in every subagent's start"),
            "{out}"
        );
        assert!(out.contains("deleted keep-small"), "{out}");
        let store = Store::open(&dir.path().join("harness.db")).unwrap();
        assert!(store.list_all_context().unwrap().is_empty());
    }

    // ---- /inbox (issue 47) ----

    use crate::store_buffer::{Checked, Status as InboxStatus, Submission, Submitted};

    fn inbox_sub(kind: &str, action: &str, name: &str, text: &str) -> Submission {
        Submission {
            kind: kind.to_owned(),
            action: action.to_owned(),
            name: name.to_owned(),
            text: text.to_owned(),
            context_kind: if kind == "context" && action == "set" {
                "rule".to_owned()
            } else {
                String::new()
            },
            ..Submission::default()
        }
    }

    /// Queues a submission in `<dir>/harness.db` as another harness would; the row's id.
    fn inbox_queue(dir: &Scratch, sub: &Submission, why: &str, source: &str) -> i64 {
        let mut store = Store::open(&dir.path().join("harness.db")).unwrap();
        match store
            .queue_submission(&Checked::new(sub, why, source).unwrap())
            .unwrap()
        {
            Submitted::Queued(id) => id,
            other => panic!("{other:?}"),
        }
    }

    fn inbox_row(dir: &Scratch, id: i64) -> crate::store_buffer::BufferEntry {
        Store::open(&dir.path().join("harness.db"))
            .unwrap()
            .buffer_entry(id)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn inbox_says_so_when_empty_and_lists_what_waits_with_the_sender_marked_as_claimed() {
        let dir = Scratch::new("repl-inbox-list");
        let mut r = rig_stored(&dir, "/inbox\n/exit\n", vec![]);
        run(&mut r.agent, &r.term).unwrap();
        assert!(
            screen(&r.term).contains("the inbox is empty: nothing waits for your decision"),
            "{}",
            screen(&r.term)
        );
        drop(r);
        let first = inbox_queue(
            &dir,
            &inbox_sub("glossary", "set", "sku", "a stock unit"),
            "the docs use it\nsecond line",
            "claude-code",
        );
        let second = inbox_queue(
            &dir,
            &inbox_sub("context", "delete", "old-rule", ""),
            "outdated",
            "codex",
        );
        let mut r = rig_stored(&dir, "/inbox\n/inbox list\n/exit\n", vec![]);
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(
            out.contains(&format!(
                "#{first}  glossary set sku · every project · from claude-code (claimed) · just now"
            )),
            "{out}"
        );
        assert!(out.contains("why: the docs use it..."), "{out}");
        assert!(out.contains(&format!("#{second}  context delete old-rule")));
        assert!(out.find(&format!("#{first}")).unwrap() < out.find(&format!("#{second}")).unwrap());
        assert!(out.contains("2 waiting: /inbox show N"), "{out}");
        assert!(
            r.requests.borrow().is_empty(),
            "/inbox is not sent to the model"
        );
    }

    #[test]
    fn inbox_show_prints_the_submission_next_to_the_stored_item_with_a_line_diff() {
        let dir = Scratch::new("repl-inbox-show");
        Store::open(&dir.path().join("harness.db"))
            .unwrap()
            .set_context("", "style", "rule", "one\ntwo\nthree", "plain please", None)
            .unwrap();
        let mut sub = inbox_sub("context", "set", "style", "one\n2\nthree");
        sub.quote = "newer words".to_owned();
        let id = inbox_queue(&dir, &sub, "clearer", "claude-code");
        let new = inbox_queue(
            &dir,
            &inbox_sub("skill", "set", "tidy", "Tidy up."),
            "useful",
            "codex",
        );
        let gone = inbox_queue(
            &dir,
            &inbox_sub("context", "delete", "style", ""),
            "obsolete",
            "codex",
        );
        let mut r = rig_stored(
            &dir,
            &format!(
                "/inbox show {id}\n/inbox show #{new}\n/inbox show {gone}\n/inbox show 99\n/inbox show\n/inbox show x\n/exit\n"
            ),
            vec![],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        for text in [
            &format!("#{id} context set 'style' · every project · pending"),
            "from claude-code (as the sender claims, not checked)",
            "why (the sender's words):",
            "clearer",
            "accepting replaces the stored item:",
            "quote: plain please  ->  newer words",
            "  one",
            "- two",
            "+ 2",
            "  three",
            "nothing is stored under this name: accepting adds it; a new item is saved switched off",
            "+ Tidy up.",
            "accepting deletes this:",
            "there is no submission #99",
            "usage: /inbox show N",
        ] {
            assert!(out.contains(text), "{text}: {out}");
        }
        assert!(!out.contains("This system prompt is on"));
    }

    #[test]
    fn inbox_accept_shows_the_diff_asks_and_applies_only_after_a_yes() {
        let dir = Scratch::new("repl-inbox-accept");
        let id = inbox_queue(
            &dir,
            &inbox_sub("glossary", "set", "sku", "a stock unit"),
            "docs",
            "claude-code",
        );
        let mut r = rig_stored(
            &dir,
            &format!(
                "/inbox accept {id}\nn\n/inbox accept {id}\ny\n/inbox accept {id}\n/inbox accept\n/exit\n"
            ),
            vec![],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(out.contains(&format!("accept #{id}? [y/N]")), "{out}");
        assert!(out.contains("+ a stock unit"));
        assert!(out.contains("[not accepted]"), "{out}");
        assert!(
            out.contains("added glossary term 'sku' for every project"),
            "{out}"
        );
        assert!(out.contains(&format!("#{id} is applied already")), "{out}");
        assert!(out.contains("usage: /inbox accept N"));
        let store = Store::open(&dir.path().join("harness.db")).unwrap();
        let word = store.glossary_entry("sku").unwrap().unwrap();
        assert_eq!(word.definition, "a stock unit");
        assert_eq!(inbox_row(&dir, id).status, InboxStatus::Applied);
    }

    #[test]
    fn inbox_accept_of_a_library_item_that_is_on_shows_the_banner_and_a_no_applies_nothing() {
        let dir = Scratch::new("repl-inbox-on");
        {
            let mut store = Store::open(&dir.path().join("harness.db")).unwrap();
            store
                .add_prompt_of_type(
                    "house",
                    "",
                    "Be brief.",
                    None,
                    crate::store_prompts::PromptType::System,
                )
                .unwrap();
            store.set_prompt_enabled("house", true).unwrap();
        }
        let id = inbox_queue(
            &dir,
            &inbox_sub("prompt", "set", "house", "Be very brief."),
            "shorter",
            "claude-code",
        );
        let wrong = inbox_queue(
            &dir,
            &inbox_sub("skill", "set", "house", "x"),
            "oops",
            "claude-code",
        );
        let mut r = rig_stored(
            &dir,
            &format!(
                "/inbox accept {id}\nno\n/inbox accept {wrong}\n/inbox reject {wrong} wrong type\n/exit\n"
            ),
            vec![],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(
            out.contains("! This system prompt is on: accepting changes what the next chat sends."),
            "{out}"
        );
        assert!(out.contains("[not accepted]"));
        let store = Store::open(&dir.path().join("harness.db")).unwrap();
        assert_eq!(store.prompt("house").unwrap().text, "Be brief.");
        // A library item of another type: the error is shown and only reject is left.
        assert!(
            out.contains("this cannot be accepted: 'house' is a system, not a skill"),
            "{out}"
        );
        assert!(
            out.contains(&format!("[nothing accepted] /inbox reject {wrong}")),
            "{out}"
        );
        assert!(!out.contains(&format!("accept #{wrong}?")));
        assert!(out.contains(&format!("rejected #{wrong}")));
        assert_eq!(inbox_row(&dir, wrong).decision_note, "wrong type");
    }

    #[test]
    fn inbox_apply_reports_a_stored_item_that_changed_and_leaves_the_row_pending() {
        let dir = Scratch::new("repl-inbox-stale");
        Store::open(&dir.path().join("harness.db"))
            .unwrap()
            .set_glossary_in("", "sku", "old meaning", None)
            .unwrap();
        let id = inbox_queue(
            &dir,
            &inbox_sub("glossary", "set", "sku", "newer meaning"),
            "better",
            "claude-code",
        );
        let shown = Store::open(&dir.path().join("harness.db"))
            .unwrap()
            .glossary_entry("sku")
            .unwrap()
            .unwrap()
            .fingerprint();
        // The word is edited after the reviewer saw it.
        Store::open(&dir.path().join("harness.db"))
            .unwrap()
            .set_glossary_in("", "sku", "my own meaning", None)
            .unwrap();
        let mut r = rig_stored(&dir, "/exit\n", vec![]);
        crate::inbox_ui::apply_shown(&mut r.agent, &r.term, id, Some(&shown));
        let out = screen(&r.term);
        assert!(
            out.contains(&format!(
                "the stored item changed while you were reading; check it again: /inbox show {id}"
            )),
            "{out}"
        );
        let store = Store::open(&dir.path().join("harness.db")).unwrap();
        assert_eq!(
            store.glossary_entry("sku").unwrap().unwrap().definition,
            "my own meaning"
        );
        assert_eq!(inbox_row(&dir, id).status, InboxStatus::Pending);
    }

    #[test]
    fn inbox_reject_keeps_the_note_and_changes_nothing_stored() {
        let dir = Scratch::new("repl-inbox-reject");
        let id = inbox_queue(
            &dir,
            &inbox_sub("glossary", "set", "sku", "a stock unit"),
            "docs",
            "claude-code",
        );
        let long = "x".repeat(crate::inbox_view::MAX_NOTE_CHARS + 1);
        let mut r = rig_stored(
            &dir,
            &format!(
                "/inbox reject {id} {long}\n/inbox reject {id}   not our word  \n/inbox reject {id}\n/inbox reject 99\n/inbox reject\n/inbox bogus\n/inbox show {id} extra\n/exit\n"
            ),
            vec![],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(out.contains("a note is at most 1000 characters"), "{out}");
        assert!(out.contains(&format!("rejected #{id}")));
        assert!(out.contains(&format!("#{id} is rejected already")), "{out}");
        assert!(out.contains("there is no submission #99"));
        assert!(out.contains("usage: /inbox reject N [note]"));
        assert!(out.contains("usage: /inbox [show N | accept N | reject N [note]]"));
        assert!(out.contains("usage: /inbox show N"));
        let row = inbox_row(&dir, id);
        assert_eq!(row.status, InboxStatus::Rejected);
        assert_eq!(row.decision_note, "not our word");
        let store = Store::open(&dir.path().join("harness.db")).unwrap();
        assert!(store.glossary_entry("sku").unwrap().is_none());
    }

    #[test]
    fn inbox_prints_what_a_sender_wrote_without_terminal_control_characters() {
        let dir = Scratch::new("repl-inbox-control");
        let evil = "bad\x1b[2J\x1b]0;pwned\x07text\rmore\x1b[31m";
        let id = inbox_queue(
            &dir,
            &inbox_sub("context", "set", "evil", "Line one\nCLEANTEXT"),
            "fine",
            "claude-code",
        );
        {
            // `submit` now refuses such text, but a row written by something else may hold anything
            // in its text, `why`, `source` and note, and the screen must still print it safely.
            let store = Store::open(&dir.path().join("harness.db")).unwrap();
            let json_evil = serde_json::to_string(evil).unwrap();
            let json_evil = &json_evil[1..json_evil.len() - 1];
            store
                .conn
                .execute(
                    "UPDATE buffer_entries SET why = ?1, source = ?2, decision_note = ?3, \
                     entry_json = replace(entry_json, 'CLEANTEXT', ?5) WHERE id = ?4",
                    rusqlite::params![evil, evil, evil, id, json_evil],
                )
                .unwrap();
        }
        let mut r = rig_stored(
            &dir,
            &format!("/inbox\n/inbox show {id}\n/inbox accept {id}\nn\n/exit\n"),
            vec![],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(!out.contains('\x1b'), "{out:?}");
        assert!(!out.contains('\x07') && !out.contains('\r'), "{out:?}");
        assert!(out.contains("bad[2J]0;pwnedtextmore[31m"), "{out:?}");
        assert!(out.contains("from bad[2J]0;pwnedtextmore[31m"), "{out:?}");
    }

    #[test]
    fn inbox_prints_the_suggestion_or_none_without_control_characters_and_decides_without_it() {
        use crate::decider::{Opinion, Verdict};
        let dir = Scratch::new("repl-inbox-suggestion");
        let judged = inbox_queue(
            &dir,
            &inbox_sub("glossary", "set", "sku", "a stock unit"),
            "docs",
            "claude-code",
        );
        let plain = inbox_queue(
            &dir,
            &inbox_sub("glossary", "set", "lot", "a batch"),
            "docs",
            "claude-code",
        );
        {
            let mut store = Store::open(&dir.path().join("harness.db")).unwrap();
            let hostile = Opinion {
                verdict: Verdict::Reject,
                confidence: Some(0.95),
                reason: "bad\x1b[2J\x1b]0;pwned\x07 <b>reason</b>\rmore".to_owned(),
                raw: String::new(),
            };
            assert!(store.add_suggestion(judged, "llm:p:m", &hostile).unwrap());
        }
        let mut r = rig_stored(
            &dir,
            &format!(
                "/inbox\n/inbox show {judged}\n/inbox show {plain}\n/inbox accept {judged}\ny\n/inbox reject {plain}\n/exit\n"
            ),
            vec![],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(
            !out.contains('\x1b') && !out.contains('\x07') && !out.contains('\r'),
            "{out:?}"
        );
        assert!(
            out.contains("suggestion: Reject (said reject, confidence 0.95)"),
            "{out}"
        );
        assert!(
            out.contains(
                "suggestion (from llm:p:m, not a decision): Reject (said reject, confidence 0.95)"
            ),
            "{out}"
        );
        assert!(
            out.contains("reason: bad[2J]0;pwned <b>reason</b>more"),
            "{out:?}"
        );
        assert!(out.contains("suggestion: none"), "{out}");
        // Accepting went ahead against the suggestion, and an entry with none can be rejected.
        assert_eq!(inbox_row(&dir, judged).status, InboxStatus::Applied);
        assert_eq!(inbox_row(&dir, plain).status, InboxStatus::Rejected);
        assert!(r.requests.borrow().is_empty(), "no model was called");
    }

    #[test]
    fn tools_lists_what_the_model_has_and_shows_one_in_full() {
        let dir = Scratch::new("repl-tools");
        let mut r = rig_stored(
            &dir,
            "/tools\n/tools READ_FILE\n/tools nope\n/exit\n",
            vec![],
        );
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);
        assert!(out.contains("built-in tools,"), "{out}");
        assert!(out.contains("read_file(lines?, path, start?)"), "{out}");
        assert!(out.contains("/tools <name> shows one in full"), "{out}");
        assert!(out.contains("there is no tool 'nope'"), "{out}");
    }
}

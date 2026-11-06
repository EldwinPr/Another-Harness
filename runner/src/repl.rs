//! The terminal side: read a line, run the agent, print what happened.
//!
//! Input and output are passed in (not hard-wired to the keyboard and screen),
//! so tests drive the same code with scripted text.

use std::cell::RefCell;
use std::io::{self, BufRead, Write};
use std::rc::Rc;

use kernel::{Message, ToolCall};

use crate::agent::{Agent, Observer, Outcome};
use crate::menu::{DEFAULT_WIDTH, Key, MenuItem, MenuState, Step, parse_key};
use crate::provider::router_host;
use crate::providers::ProviderDef;
use crate::style::Style;
use crate::tools::Approver;
use crate::turn_log::utc_label;

/// Longest tool-call arguments shown on screen (the log keeps them whole).
const SHOWN_ARGUMENT_CHARS: usize = 200;

/// How many of the user's messages a resumed chat shows again (with everything after each).
const REPLAYED_USER_MESSAGES: usize = 10;

/// How many saved chats `/sessions` lists.
const LISTED_SESSIONS: usize = 10;

/// Longest start of a chat's first message shown in the list.
const SHOWN_PREVIEW_CHARS: usize = 40;

/// How many model ids `/models` prints before saying how many more there are.
const SHOWN_MODELS: usize = 30;

/// The chat commands, as `/help` lists them: the command, then what it does.
const COMMANDS: &[(&str, &str)] = &[
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
        "/provider add <name> <base_url> <model>",
        "save a new provider, ask for its API key, switch to it",
    ),
    (
        "/provider remove <name>",
        "delete a saved provider (profile files stay)",
    ),
    (
        "/prompts",
        "the system prompts: switch them on or off (a checklist in a terminal; skills and personalities are in /prompts config)",
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
        "/prompts type <name> <system|skill|personality>",
        "change what a prompt is for (it starts the new type switched off)",
    ),
    (
        "/prompts show",
        "print the system prompt a new chat would start with",
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
        "/personality [name|none]",
        "choose the personality of this chat, one at a time (a menu in a terminal)",
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
    ("/exit", "quit (also /quit; Ctrl-D works too)"),
];

/// Tool-call arguments as shown on screen: cut off after [`SHOWN_ARGUMENT_CHARS`].
fn shorten(arguments: &str) -> String {
    let mut shown: String = arguments.chars().take(SHOWN_ARGUMENT_CHARS).collect();
    if arguments.chars().count() > SHOWN_ARGUMENT_CHARS {
        shown.push_str("...");
    }
    shown
}

/// One `[tool] name arguments` line, the same live and in a replay.
fn tool_line(style: Style, name: &str, arguments: &str) -> String {
    format!(
        "{} {name} {}\n",
        style.yellow("[tool]"),
        style.dim(&shorten(arguments))
    )
}

pub const PROMPT: &str = "> ";

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
        // A closed screen is not worth stopping for here.
        let _ = self.output.write_all(text.as_bytes());
        let _ = self.output.flush();
        self.col = advance_col(self.col, text);
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

/// The terminal in raw mode (single keys, no echo) until this is dropped, which puts it back as
/// it was, also when a panic unwinds. Ctrl-C arrives as a key (`-isig`) so it cannot kill the
/// program with the terminal left raw.
struct RawMode {
    saved: String,
}

impl RawMode {
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
            .args(["-icanon", "-echo", "-isig", "min", "1", "time", "0"])
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
    std::process::Command::new("stty")
        .arg("size")
        .stdin(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|text| text.split_whitespace().nth(1)?.parse::<usize>().ok())
        // A terminal that does not know its size reports 0.
        .filter(|width| *width > 0)
        .unwrap_or(DEFAULT_WIDTH)
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

/// Asks on the terminal before `bash` runs a command. Only `y` or `yes` approves;
/// anything else, or end of input, denies.
pub struct TerminalApprover<R, W>(pub Shared<R, W>);

impl<R: BufRead, W: Write> Approver for TerminalApprover<R, W> {
    fn approve(&self, command: &str) -> bool {
        let mut term = self.0.borrow_mut();
        let style = term.style();
        term.say(&format!(
            "\n{}\n  {}\n{} ",
            style.bold("run this command?"),
            style.yellow(command),
            style.bold("[y/N]")
        ));
        let answer = term.read_line().unwrap_or_default();
        matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    }
}

/// Prints each tool call as the agent starts it.
pub struct PrintObserver<R, W>(pub Shared<R, W>);

impl<R: BufRead, W: Write> Observer for PrintObserver<R, W> {
    fn tool_call(&mut self, call: &ToolCall) {
        let style = self.0.borrow().style();
        self.0
            .borrow_mut()
            .say(&tool_line(style, &call.name, &call.arguments));
    }

    fn warning(&mut self, text: &str) {
        let style = self.0.borrow().style();
        self.0.borrow_mut().say(&format!(
            "{}\n",
            style.yellow(&format!("[warning: {text}]"))
        ));
    }
}

/// Shows a resumed chat on screen the way the live chat showed it: your lines after `> `, the
/// model's answers, and a `[tool]` line for each tool call. Tool results and the system prompt
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
        .filter(|(_, m)| matches!(m, Message::User(_)))
        .map(|(position, _)| position)
        .collect();
    let start = user_positions
        .len()
        .checked_sub(REPLAYED_USER_MESSAGES)
        .map_or(0, |first| user_positions[first]);

    let style = term.borrow().style();
    let mut text = String::new();
    if start > 0 {
        text.push_str(&format!(
            "{}\n",
            style.dim(&format!("[{start} earlier messages not shown]"))
        ));
    }
    for message in &messages[start..] {
        match message {
            Message::User(content) => {
                text.push_str(&format!("{}{content}\n", style.accent(PROMPT)));
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                // Text that comes with a tool call was not shown live, so it is not shown now.
                if tool_calls.is_empty()
                    && let Some(content) = content
                {
                    text.push_str(&format!("{content}\n"));
                }
                for call in tool_calls {
                    text.push_str(&tool_line(style, &call.name, &call.arguments));
                }
            }
            Message::Tool { .. } | Message::System(_) => {}
        }
    }
    text.push_str(&format!("{}\n", style.dim("[end of saved chat]")));
    term.borrow_mut().say(&text);
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
                    preview(chat.first_message.as_deref()),
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
            "anything else you type, including other lines that start with /, goes to the model"
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

/// `/provider add <name> <base_url> <model>`: asks for the key, saves, switches.
fn add_provider_command<R: BufRead, W: Write>(
    agent: &mut Agent,
    term: &Shared<R, W>,
    arguments: &[&str],
) {
    let style = term.borrow().style();
    let [name, base_url, model] = arguments else {
        say_line(
            term,
            &style.yellow("usage: /provider add <name> <base_url> <model>"),
        );
        return;
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
    loop {
        // Each borrow ends at the semicolon, before `agent.send` runs: the
        // approver and observer borrow the terminal again while it works.
        term.borrow_mut().say(&style.accent(PROMPT));
        let Some(line) = term.borrow_mut().read_command() else {
            term.borrow_mut().say("\n");
            return Ok(());
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "/exit" || line == "/quit" {
            return Ok(());
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
            "/prompts" => {
                crate::prompts_ui::command(agent, term, rest);
                continue;
            }
            "/skills" => {
                crate::skills_ui::skills_command(agent, term, rest);
                continue;
            }
            "/personality" => {
                crate::skills_ui::personality_command(agent, term, rest);
                continue;
            }
            "/new" if rest.is_empty() => {
                match agent.new_chat() {
                    Ok(info) => {
                        let extras = agent.chat_prompts().summary();
                        let extras = if extras.is_empty() {
                            String::new()
                        } else {
                            format!("; {extras}")
                        };
                        say_line(
                            term,
                            &style.green(&format!(
                                "new chat; system prompt: {}{extras}",
                                info.summary()
                            )),
                        );
                    }
                    Err(why) => say_line(term, &style.red(&format!("[error: {why}]"))),
                }
                continue;
            }
            _ => {}
        }
        let report = match agent.send(line, &mut observer) {
            Outcome::Answer(text) => format!("{text}\n"),
            Outcome::CapReached { calls } => format!(
                "{}\n",
                style.yellow(&format!(
                    "[stopped: reached the limit of {calls} model calls for this message]"
                ))
            ),
            Outcome::Failed(why) => format!("{}\n", style.red(&format!("[error: {why}]"))),
        };
        term.borrow_mut().say(&report);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ScriptedProvider;
    use crate::tools::testutil::Scratch;
    use crate::tools::{Bash, DEFAULT_TIMEOUT, ReadFile, Tool};
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
        requests: Rc<RefCell<Vec<Vec<u8>>>>,
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
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(ReadFile::new(dir.path())),
            Box::new(Bash::new(
                dir.path(),
                DEFAULT_TIMEOUT,
                Box::new(TerminalApprover(Rc::clone(&term))),
            )),
        ];
        let log = TurnLog::create(&dir.path().join("logs")).unwrap();
        let agent =
            Agent::new("m", Box::new(provider), tools, log).with_target("test", "http://x/v1");
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
    fn a_multi_turn_chat_with_an_approved_command() {
        let dir = Scratch::new("repl-e2e");
        std::fs::write(dir.path().join("a.txt"), "file text\n").unwrap();
        // The approval answer "y" is the line right after the request that needs it.
        let mut r = rig(
            &dir,
            "run echo hi\ny\nnow read a.txt\n/exit\n",
            vec![
                ScriptedProvider::tool_call("c1", "bash", r#"{"command":"echo hi"}"#),
                ScriptedProvider::text("It printed hi."),
                ScriptedProvider::tool_call("c2", "read_file", r#"{"path":"a.txt"}"#),
                ScriptedProvider::text("The file says: file text."),
            ],
        );
        run(&mut r.agent, &r.term).unwrap();

        let out = screen(&r.term);
        assert!(
            out.contains(r#"[tool] bash {"command":"echo hi"}"#),
            "{out}"
        );
        assert!(
            out.contains("run this command?\n  echo hi\n[y/N] "),
            "{out}"
        );
        assert!(out.contains("It printed hi.\n"), "{out}");
        assert!(out.contains("[tool] read_file"), "{out}");
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
            .find_map(|l| l.find("[tool]").map(|i| &l[i..]))
            .unwrap();
        assert!(tool_line.ends_with("..."));
        assert!(tool_line.len() < 300);
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
            "> hi\nHello!\n> read it\n[tool] read_file {\"path\":\"a.txt\"}\nIt says x.\n[end of saved chat]\n"
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
            out.starts_with("[4 earlier messages not shown]\n> q2\na2\n"),
            "{out}"
        );
        assert!(!out.contains("q1\n") && !out.contains("a1\n"), "{out}");
        assert!(out.ends_with("> q11\na11\n[end of saved chat]\n"), "{out}");
    }

    #[test]
    fn a_short_chat_shows_everything_without_a_count() {
        let term = blank_term();
        let history = vec![Message::System("p".into()), Message::User("only".into())];
        show_history(&term, &history);
        assert_eq!(screen(&term), "> only\n[end of saved chat]\n");
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
        let tool_line = out.lines().find(|l| l.starts_with("[tool]")).unwrap();
        assert!(
            tool_line.ends_with("...") && tool_line.len() < 300,
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
            out.contains("> first question\none\n> second question\ntwo\n[end of saved chat]\n"),
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
        assert!(out.contains("> the first chat\n"), "{out}");
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
        let typed = "run echo hi\ny\nnow read a.txt\nfail please\n/sessions\n/exit\n";
        let replies = || {
            vec![
                ScriptedProvider::tool_call("c1", "bash", r#"{"command":"echo hi"}"#),
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
        assert!(coloured.contains("\x1b[33m[tool]\x1b[0m"), "{coloured}");
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
            coloured.contains("\x1b[1;36m> \x1b[0mfirst question"),
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
    fn an_unknown_slash_line_still_goes_to_the_model() {
        let dir = Scratch::new("repl-unknown-slash");
        let mut r = rig(
            &dir,
            "/foo bar\n/exit\n",
            vec![ScriptedProvider::text("ok")],
        );
        run(&mut r.agent, &r.term).unwrap();
        assert_eq!(r.requests.borrow().len(), 1);
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

    type Built = Rc<RefCell<Vec<(String, Rc<RefCell<Vec<Vec<u8>>>>)>>>;

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
            profile_dir: Some(dir.path().join("profiles")),
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

    fn profile(dir: &Scratch, name: &str, text: &str) {
        let profiles = dir.path().join("profiles");
        std::fs::create_dir_all(&profiles).unwrap();
        std::fs::write(profiles.join(format!("{name}.profile")), text).unwrap();
    }

    #[test]
    fn providers_are_listed_switched_added_and_removed() {
        let dir = Scratch::new("repl-providers");
        profile(
            &dir,
            "other",
            "HARNESS_BASE_URL=http://other:9/v1\nHARNESS_MODEL=other-model\n",
        );
        profile(&dir, "broken", "HARNESS_MODEL=m\n");
        let typed = "/provider\n/provider add mine http://mine.example/v1 mine-model\nSECRET-KEY-777\n\
                     /provider\n/provider other\n/provider remove mine\n/provider remove other\n\
                     /provider nope\n/provider broken\nhello\n/exit\n";
        let (mut r, built) = switching_rig(&dir, typed, Style::plain());
        run(&mut r.agent, &r.term).unwrap();
        let out = screen(&r.term);

        // The first list: both profiles, the broken one explained, nothing saved yet.
        assert!(out.contains("providers:\n  broken  profile  cannot be used: HARNESS_BASE_URL is missing\n  other   profile  other:9  other-model\n"), "{out}");
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
        // Switching to a profile, removing, and the failure messages.
        assert!(
            out.contains("now using other (other:9) with model other-model"),
            "{out}"
        );
        assert!(out.contains("removed provider mine"), "{out}");
        assert!(
            out.contains("[error: other is a profile file; delete the file to remove it]"),
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
            profile(
                &dir,
                "other",
                "HARNESS_BASE_URL=http://other/v1\nHARNESS_MODEL=om\n",
            );
            let provider = ScriptedProvider::new(vec![]).with_models(Ok(many_models()));
            let mut r = rig_with_provider(&dir, script, provider, style);
            let store = Store::open(&dir.path().join("harness.db")).unwrap();
            r.agent = r.agent.with_store(store, StoreContext { cwd: "/w".into() });
            r.agent = r.agent.with_catalog(crate::providers::Catalog {
                profile_dir: Some(dir.path().join("profiles")),
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
}

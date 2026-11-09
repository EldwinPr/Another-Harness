//! The box around the main prompt (issue 17), drawn only at a real terminal:
//!
//! ```text
//! ╭──────────────────╮
//! │ what you type    │
//! │ and it wraps     │
//! ╰──────────────────╯
//!  model · provider │ approve: manual │ ...
//! ```
//!
//! The box is as wide as the terminal and grows taller as the text wraps; when it would not fit
//! the screen, the rows around the cursor are shown. Every key redraws the whole box: the cursor
//! goes back up to the top, everything below is cleared and the box is printed again. After Enter
//! the box is erased and what was typed is printed as a line of its own; while the model works a
//! box with `working…` stays at the bottom and the output scrolls above it.
//! Each character counts as one column (wide characters are not measured).

use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

use crate::attach::{PICKER_ROWS, matches, mention};
use crate::lineedit::{Edit, LineEditor, Step, parse_edit};
use crate::style::Style;

/// Rows the command list shows.
const COMMAND_ROWS: usize = 8;

/// The `/command` being typed: the line starts with `/` and the cursor is still inside that first
/// word. Gives where the word ends (the cursor may be in the middle of it) and the text of the
/// word up to the cursor, slash included.
pub fn command_word(chars: &[char], cursor: usize) -> Option<(usize, String)> {
    let cursor = cursor.min(chars.len());
    if chars.first() != Some(&'/') || chars[..cursor].iter().any(|c| c.is_whitespace()) {
        return None;
    }
    let end = chars[cursor..]
        .iter()
        .position(|c| c.is_whitespace())
        .map_or(chars.len(), |at| cursor + at);
    Some((end, chars[..cursor].iter().collect()))
}

/// The commands that start with what was typed, in the order they are listed, at most `limit`.
/// Nothing for a path such as `/etc/hosts`.
pub fn command_matches(words: &[String], typed: &str, limit: usize) -> Vec<String> {
    let typed = typed.to_lowercase();
    words
        .iter()
        .filter(|w| w.starts_with(&typed))
        .take(limit)
        .cloned()
        .collect()
}

/// The narrowest box worth drawing.
const MIN_WIDTH: usize = 20;

/// Rows of the screen the box leaves free: the two rules, the info line and one spare.
const SCREEN_MARGIN: usize = 4;

/// One drawing of the box.
pub struct Frame {
    /// What to print: it starts at the top-left of the box and ends with the cursor in place.
    pub text: String,
    /// The cursor's row counted from the top rule (0): the next redraw goes up this far.
    pub cursor_row: usize,
}

/// Draws the box for `chars` with the cursor at `cursor`. `scroll` is the first content row shown
/// (kept between calls, moved only when the cursor would leave the window). `redraw_from` is the
/// cursor row of the previous drawing, or `None` the first time.
// Everything the drawing depends on is passed in, so a test can draw without a terminal.
#[allow(clippy::too_many_arguments)]
pub fn frame(
    style: Style,
    size: (usize, usize),
    chars: &[char],
    cursor: usize,
    info: &str,
    scroll: &mut usize,
    redraw_from: Option<usize>,
    picker: (&[String], usize),
) -> Frame {
    let (choices, selected) = picker;
    let (height, width) = size;
    // The first line of `info` goes under the box; any more are a list under the choices.
    let (info, panel) = info.split_once('\n').unwrap_or((info, ""));
    let panel: Vec<&str> = panel.lines().collect();
    // One column short of the edge, so a full row never depends on how the terminal wraps.
    let width = width.max(MIN_WIDTH).saturating_sub(1).max(MIN_WIDTH);
    let inner = width - 4;
    let total = chars.len() / inner + 1;
    let (cursor_row, cursor_col) = (cursor / inner, cursor % inner);
    let visible = total.min(
        height
            .saturating_sub(SCREEN_MARGIN + choices.len() + panel.len())
            .max(1),
    );
    if cursor_row < *scroll {
        *scroll = cursor_row;
    }
    if cursor_row >= *scroll + visible {
        *scroll = cursor_row + 1 - visible;
    }
    *scroll = (*scroll).min(total - visible);

    let rule = "─".repeat(width - 2);
    let mut lines = vec![style.cyan(&format!("╭{rule}╮"))];
    for row in *scroll..*scroll + visible {
        let start = (row * inner).min(chars.len());
        let end = ((row + 1) * inner).min(chars.len());
        let text: String = chars[start..end].iter().collect();
        let pad = " ".repeat(inner - (end - start));
        lines.push(style.cyan(&format!("│ {text}{pad} │")));
    }
    lines.push(style.cyan(&format!("╰{rule}╯")));
    lines.push(style.dim(&format!(
        " {}",
        crate::toolview::cut(info, width.saturating_sub(2))
    )));

    for (i, choice) in choices.iter().enumerate() {
        let shown = crate::toolview::cut(choice, width.saturating_sub(5));
        lines.push(if i == selected {
            style.cyan(&format!(" ▸ {shown}"))
        } else {
            style.dim(&format!("   {shown}"))
        });
    }
    for line in &panel {
        lines.push(style.dim(&format!(
            " {}",
            crate::toolview::cut(line, width.saturating_sub(2))
        )));
    }

    let target = 1 + cursor_row - *scroll;
    let last = visible + 2 + choices.len() + panel.len();
    let mut text = String::new();
    if let Some(from) = redraw_from
        && from > 0
    {
        text.push_str(&format!("\x1b[{from}A"));
    }
    if redraw_from.is_some() {
        text.push_str("\r\x1b[J");
    }
    text.push_str(&lines.join("\r\n"));
    if last > target {
        text.push_str(&format!("\x1b[{}A", last - target));
    }
    text.push_str(&format!("\r\x1b[{}C", 2 + cursor_col));
    Frame {
        text,
        cursor_row: target,
    }
}

/// The box left at the bottom of the screen while the model works: `text` in it, the info line
/// under it (and, if `info` has more lines, a list under that), `above` (already styled lines, such
/// as a checklist) over it, and the cursor on the row below. Returns what to print and how many
/// rows it has, which [`erase`] needs.
pub fn static_box(
    style: Style,
    size: (usize, usize),
    text: &str,
    info: &str,
    above: &[String],
) -> (String, usize) {
    let width = size.1.max(MIN_WIDTH).saturating_sub(1).max(MIN_WIDTH);
    let inner = width - 4;
    let rule = "─".repeat(width - 2);
    let shown = crate::toolview::cut(text, inner);
    let pad = " ".repeat(inner - shown.chars().count());
    let mut lines: Vec<String> = above.to_vec();
    lines.push(style.cyan(&format!("╭{rule}╮")));
    lines.push(format!(
        "{}{}{}",
        style.cyan("│ "),
        style.dim(&format!("{shown}{pad}")),
        style.cyan(" │")
    ));
    lines.push(style.cyan(&format!("╰{rule}╯")));
    for line in info.lines() {
        lines.push(style.dim(&format!(
            " {}",
            crate::toolview::cut(line, width.saturating_sub(2))
        )));
    }
    (format!("{}\r\n", lines.join("\r\n")), lines.len())
}

/// What takes a drawn box off the screen: the cursor goes up `rows` rows to the top-left corner
/// and everything below is cleared.
pub fn erase(rows: usize) -> String {
    if rows > 0 {
        format!("\x1b[{rows}A\r\x1b[J")
    } else {
        "\r\x1b[J".to_owned()
    }
}

/// Reads one line typed inside the box. The terminal must already be in raw mode. `info` is the
/// text for under the box, made for the width it may use. `files` makes the list of files the
/// `@` picker offers; it is called the first time an `@` word is typed and only then. With a
/// list open, Up and Down move the highlight, Tab or Enter put the file in the line, Esc closes it. Returns the line (`None` at the end
/// of the input: Ctrl-D or Ctrl-C on an empty line, or the stream ended) and the cursor's row in
/// the box, which is still on screen: the caller erases it with [`erase`].
pub fn read_boxed<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    style: Style,
    size: (usize, usize),
    history: &[String],
    info: &dyn Fn(Option<usize>) -> String,
    files: &mut dyn FnMut() -> Vec<String>,
) -> (Option<String>, usize) {
    let (line, row, _) =
        read_boxed_waking("", input, output, style, size, history, info, files, None);
    (line, row)
}

/// [`read_boxed`] with `initial` already in the box and the cursor after it (a cancelled message
/// comes back this way, to be edited and sent again), for a terminal whose reads give up after a
/// short wait (issue 35): whenever a read finds nothing, `wake` is asked, and when it says yes the
/// box ends with `None` and the text typed so far handed back as the third value, so the caller can
/// act and bring the text back.
#[allow(clippy::too_many_arguments)]
pub fn read_boxed_waking<R: BufRead, W: Write>(
    initial: &str,
    input: &mut R,
    output: &mut W,
    style: Style,
    size: (usize, usize),
    history: &[String],
    info_for: &dyn Fn(Option<usize>) -> String,
    files: &mut dyn FnMut() -> Vec<String>,
    wake: Option<&dyn Fn() -> bool>,
) -> (Option<String>, usize, Option<String>) {
    let mut info = info_for(Some(size.1.saturating_sub(3)));
    let mut woke: Option<String> = None;
    let mut editor = LineEditor::new(history);
    editor.replace(0, 0, initial);
    let commands = crate::repl::command_words();
    let (mut scroll, mut from) = (0, None);
    let mut all: Option<Vec<String>> = None;
    // The `@` word the list was closed for with Esc, and the highlighted row.
    let mut closed_at: Option<usize> = None;
    let (mut selected, mut last_query) = (0usize, String::new());
    // True while more bytes are waiting after the key just read: a paste, which is drawn once at
    // its end instead of once per character.
    let mut pasting = false;
    // The screen is behind the line (a draw was skipped for a paste).
    let mut stale: bool;
    let (mut last_empty, mut quick) = (Instant::now(), 0u32);
    let result = loop {
        // The word being completed: an `@file` anywhere, or a `/command` at the start of the line.
        let mut word = mention(editor.chars(), editor.cursor());
        let command = if word.is_none() {
            command_word(editor.chars(), editor.cursor())
        } else {
            None
        };
        if let Some((_, typed)) = &command {
            word = Some((0, typed.clone()));
        }
        if word.as_ref().map(|(start, _)| *start) != closed_at {
            closed_at = None;
        }
        let choices: Vec<String> = match &word {
            Some((start, query)) if closed_at != Some(*start) && command.is_some() => {
                if *query != last_query {
                    last_query = query.clone();
                    selected = 0;
                }
                command_matches(&commands, query, COMMAND_ROWS)
            }
            Some((start, query)) if closed_at != Some(*start) => {
                let list = all.get_or_insert_with(&mut *files);
                if *query != last_query {
                    last_query = query.clone();
                    selected = 0;
                }
                matches(list, query, PICKER_ROWS)
            }
            _ => Vec::new(),
        };
        selected = selected.min(choices.len().saturating_sub(1));
        stale = pasting && from.is_some();
        if !stale {
            let drawn = frame(
                style,
                size,
                editor.chars(),
                editor.cursor(),
                &info,
                &mut scroll,
                from,
                (&choices, selected),
            );
            let _ = output.write_all(drawn.text.as_bytes());
            let _ = output.flush();
            from = Some(drawn.cursor_row);
        }
        let (edit, more) = loop {
            let Ok(buffer) = input.fill_buf() else {
                break (None, false);
            };
            if buffer.is_empty() {
                // A read that waited and found nothing (the terminal gives up after a moment):
                // see whether to wake. Reads that come back at once many times in a row are an
                // input that has really ended.
                let Some(wake) = wake else {
                    break (None, false);
                };
                if wake() {
                    woke = Some(editor.text());
                    break (None, false);
                }
                // What is shown under the box (the tasks that are running) may have changed.
                let fresh = info_for(Some(size.1.saturating_sub(3)));
                if fresh != info && !pasting {
                    info = fresh;
                    let drawn = frame(
                        style,
                        size,
                        editor.chars(),
                        editor.cursor(),
                        &info,
                        &mut scroll,
                        from,
                        (&choices, selected),
                    );
                    let _ = output.write_all(drawn.text.as_bytes());
                    let _ = output.flush();
                    from = Some(drawn.cursor_row);
                }
                if last_empty.elapsed() < Duration::from_millis(20) {
                    quick += 1;
                    if quick >= 20 {
                        break (None, false);
                    }
                } else {
                    quick = 0;
                }
                last_empty = Instant::now();
                continue;
            }
            let (edit, used) = parse_edit(buffer);
            let more = buffer.len() > used;
            input.consume(used);
            if edit.is_some() {
                break (edit, more);
            }
        };
        let Some(mut edit) = edit else {
            break None;
        };
        pasting = more;
        // A command that is already complete is sent as it is; only a part of one is completed.
        let complete = command
            .as_ref()
            .is_some_and(|(_, typed)| commands.iter().any(|c| c == typed));
        let taking = !(complete && edit == Edit::Enter);
        if let (Some((start, _)), false, true) = (&word, choices.is_empty(), taking) {
            match edit {
                Edit::Up => {
                    selected = selected.saturating_sub(1);
                    continue;
                }
                Edit::Down => {
                    selected = (selected + 1).min(choices.len() - 1);
                    continue;
                }
                Edit::Tab | Edit::Enter => {
                    let chosen = &choices[selected];
                    if let Some((end, _)) = &command {
                        // The whole command word is replaced, even the part after the cursor.
                        editor.replace(0, *end, &format!("{chosen} "));
                        continue;
                    }
                    // A folder is left open, so the files in it are listed next.
                    let gap = if chosen.ends_with('/') { "" } else { " " };
                    let text = format!("@{chosen}{gap}");
                    editor.replace(*start, editor.cursor(), &text);
                    continue;
                }
                Edit::Escape => {
                    closed_at = Some(*start);
                    continue;
                }
                _ => {}
            }
        }
        // An Enter with more text behind it is part of a paste: it must not send the first line
        // now and the rest as later messages.
        if more && edit == Edit::Enter {
            edit = Edit::Char(' ');
        }
        match editor.key(edit) {
            Step::Continue => {}
            Step::Submit => break Some(editor.text()),
            Step::Eof => break None,
        }
    };
    if stale {
        let drawn = frame(
            style,
            size,
            editor.chars(),
            editor.cursor(),
            &info,
            &mut scroll,
            from,
            (&[], 0),
        );
        let _ = output.write_all(drawn.text.as_bytes());
        let _ = output.flush();
        from = Some(drawn.cursor_row);
    }
    (result, from.unwrap_or(0), woke)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A very small screen: enough of the control codes the box uses to see where things end up.
    struct Screen {
        grid: Vec<Vec<char>>,
        row: usize,
        col: usize,
        width: usize,
    }

    impl Screen {
        fn new(width: usize) -> Self {
            Self {
                grid: vec![vec![' '; width]; 60],
                row: 0,
                col: 0,
                width,
            }
        }

        fn feed(&mut self, bytes: &[u8]) {
            let text = String::from_utf8_lossy(bytes).into_owned();
            let chars: Vec<char> = text.chars().collect();
            let mut i = 0;
            while i < chars.len() {
                match chars[i] {
                    '\x1b' if chars.get(i + 1) == Some(&'[') => {
                        let mut j = i + 2;
                        while !chars[j].is_ascii_alphabetic() {
                            j += 1;
                        }
                        let n: usize = chars[i + 2..j]
                            .iter()
                            .collect::<String>()
                            .parse()
                            .unwrap_or(1);
                        match chars[j] {
                            'A' => self.row -= n,
                            'B' => self.row += n,
                            'C' => self.col += n,
                            'J' => {
                                for c in self.col..self.width {
                                    self.grid[self.row][c] = ' ';
                                }
                                for r in self.row + 1..self.grid.len() {
                                    self.grid[r] = vec![' '; self.width];
                                }
                            }
                            _ => {}
                        }
                        i = j + 1;
                        continue;
                    }
                    '\r' => self.col = 0,
                    '\n' => self.row += 1,
                    c => {
                        assert!(self.col < self.width, "wrote past the right edge");
                        self.grid[self.row][self.col] = c;
                        self.col += 1;
                    }
                }
                i += 1;
            }
        }

        fn lines(&self) -> Vec<String> {
            let mut rows: Vec<String> = self
                .grid
                .iter()
                .map(|r| r.iter().collect::<String>().trim_end().to_owned())
                .collect();
            while rows.last().is_some_and(|r| r.is_empty()) {
                rows.pop();
            }
            rows
        }
    }

    fn run_keys(width: usize, height: usize, keys: &[u8]) -> (Option<String>, Screen) {
        let mut input = Cursor::new(keys.to_vec());
        let mut output = Vec::new();
        let (result, _row) = read_boxed(
            &mut input,
            &mut output,
            Style::plain(),
            (height, width),
            &[],
            &|_| "info line".to_owned(),
            &mut Vec::new,
        );
        let mut screen = Screen::new(width);
        screen.feed(&output);
        (result, screen)
    }

    #[test]
    fn the_command_word_is_the_first_word_while_the_cursor_is_in_it() {
        let chars = |s: &str| s.chars().collect::<Vec<_>>();
        assert_eq!(
            command_word(&chars("/pro"), 4),
            Some((4, "/pro".to_owned()))
        );
        // Cursor in the middle: the word to replace goes on to its end.
        assert_eq!(
            command_word(&chars("/prox rest"), 3),
            Some((5, "/pr".to_owned()))
        );
        assert_eq!(
            command_word(&chars("/model x"), 8),
            None,
            "past the first word"
        );
        assert_eq!(command_word(&chars("hi /pro"), 7), None, "not at the start");
        assert_eq!(command_word(&chars(""), 0), None);
    }

    #[test]
    fn commands_are_matched_by_what_they_start_with_and_a_path_matches_none() {
        let words = crate::repl::command_words();
        let found = command_matches(&words, "/prov", 8);
        assert_eq!(found, ["/provider"]);
        assert!(command_matches(&words, "/", 8).len() == 8);
        assert_eq!(command_matches(&words, "/MOD", 8), ["/model", "/models"]);
        assert!(command_matches(&words, "/etc/hosts", 8).is_empty());
        assert!(words.contains(&"/compact".to_owned()) && words.contains(&"/quit".to_owned()));
    }

    #[test]
    fn tab_completes_a_command_and_enter_sends_a_finished_one_as_it_is() {
        let (line, _) = run_keys(
            60,
            24,
            b"/prov\t
",
        );
        assert_eq!(line.as_deref(), Some("/provider "));
        // A finished command is not replaced by a longer one that starts the same way.
        let (line, _) = run_keys(
            60, 24, b"/model
",
        );
        assert_eq!(line.as_deref(), Some("/model"));
        // A part of one is completed by Enter, which then needs another Enter to send.
        let (line, _) = run_keys(
            60, 24, b"/sess

",
        );
        assert_eq!(line.as_deref(), Some("/sessions "));
        // The word is replaced even when the cursor is inside it.
        let (line, _) = run_keys(
            60,
            24,
            b"/provx\x1b[D\t
",
        );
        assert_eq!(line.as_deref(), Some("/provider "));
    }

    #[test]
    fn the_command_list_is_shown_under_the_box_and_a_path_or_a_message_gets_none() {
        let (_, screen) = run_keys(60, 24, b"/mo");
        let shown = screen.lines().join("\n");
        assert!(
            shown.contains("▸ /model") && shown.contains("/models"),
            "{shown}"
        );
        let (line, screen) = run_keys(
            60,
            24,
            b"/etc/hosts
",
        );
        assert_eq!(line.as_deref(), Some("/etc/hosts"));
        assert!(!screen.lines().join("\n").contains("▸"));
        let (line, _) = run_keys(
            60,
            24,
            b"hello /pro
",
        );
        assert_eq!(line.as_deref(), Some("hello /pro"));
    }

    #[test]
    fn a_short_line_is_a_closed_box_with_the_info_under_it() {
        let (line, screen) = run_keys(30, 24, b"hello\r");
        assert_eq!(line.as_deref(), Some("hello"));
        let rows = screen.lines();
        assert_eq!(rows[0], format!("╭{}╮", "─".repeat(27)));
        assert_eq!(rows[1], format!("│ hello{} │", " ".repeat(20)));
        assert_eq!(rows[2], format!("╰{}╯", "─".repeat(27)));
        assert_eq!(rows[3], " info line");
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn a_long_line_wraps_and_the_box_grows_taller_instead_of_scrolling_sideways() {
        // Width 30 gives 25 columns of text per row: 60 characters need 3 rows.
        let text = "abcdefghij".repeat(6);
        let (line, screen) = run_keys(30, 24, format!("{text}\r").as_bytes());
        assert_eq!(line.as_deref(), Some(text.as_str()));
        let rows = screen.lines();
        assert_eq!(rows[1], "│ abcdefghijabcdefghijabcde │");
        assert_eq!(rows[2], "│ fghijabcdefghijabcdefghij │");
        assert_eq!(rows[3], "│ abcdefghij                │");
        assert_eq!(rows[4], format!("╰{}╯", "─".repeat(27)));
        assert_eq!(rows.len(), 6);
        assert!(
            rows.iter().take(5).all(|r| r.chars().count() == 29),
            "{rows:?}"
        );
    }

    #[test]
    fn a_line_that_exactly_fills_a_row_makes_room_for_the_cursor_on_the_next() {
        let (_, screen) = run_keys(30, 24, format!("{}\r", "x".repeat(25)).as_bytes());
        let rows = screen.lines();
        assert_eq!(rows[1], format!("│ {} │", "x".repeat(25)));
        assert_eq!(rows[2], format!("│ {} │", " ".repeat(25)));
        assert_eq!(rows[3], format!("╰{}╯", "─".repeat(27)));
    }

    #[test]
    fn editing_redraws_the_box_in_place_and_leaves_one_box_on_screen() {
        // Type, move left twice, delete the character before the cursor, add one in its place.
        let (line, screen) = run_keys(30, 24, b"hello\x1b[D\x1b[D\x7fX\r");
        assert_eq!(line.as_deref(), Some("heXlo"));
        let rows = screen.lines();
        assert_eq!(rows.len(), 4, "{rows:?}");
        assert_eq!(rows[1], format!("│ heXlo{} │", " ".repeat(20)));
    }

    #[test]
    fn deleting_back_over_a_wrap_shrinks_the_box_again() {
        let keys = format!("{}\x7f\x7f\r", "y".repeat(26));
        let (line, screen) = run_keys(30, 24, keys.as_bytes());
        assert_eq!(line.as_deref(), Some(&*"y".repeat(24)));
        let rows = screen.lines();
        assert_eq!(rows.len(), 4, "{rows:?}");
    }

    #[test]
    fn a_box_taller_than_the_screen_shows_the_rows_around_the_cursor() {
        // Height 8 leaves 4 content rows; 150 characters are 6 rows of 25.
        let text: String = (0..150)
            .map(|i| char::from(b'a' + (i / 25) as u8))
            .collect();
        let (line, screen) = run_keys(30, 8, format!("{text}\r").as_bytes());
        assert_eq!(line.as_deref(), Some(text.as_str()));
        let rows = screen.lines();
        // 7 rows of text (the last one empty, holding the cursor): the last 4 are shown.
        assert_eq!(rows.len(), 4 + 3, "{rows:?}");
        assert!(rows[1].contains("ddddd"), "{rows:?}");
        assert!(rows[3].contains("fffff"), "{rows:?}");
        assert_eq!(rows[4], format!("│ {} │", " ".repeat(25)));
    }

    #[test]
    fn the_end_of_the_input_leaves_the_box_and_returns_none() {
        let (line, screen) = run_keys(30, 24, b"\x04");
        assert_eq!(line, None);
        assert_eq!(screen.lines().len(), 4);
        let (line, _) = run_keys(30, 24, b"");
        assert_eq!(line, None);
    }

    #[test]
    fn a_pasted_text_with_line_breaks_is_one_message_drawn_once() {
        let mut input = Cursor::new(b"fix X\n/approve auto\nthen Y\r".to_vec());
        let mut output = Vec::new();
        let (line, _) = read_boxed(
            &mut input,
            &mut output,
            Style::plain(),
            (60, 24),
            &[],
            &|_| "info".to_owned(),
            &mut Vec::new,
        );
        assert_eq!(line.as_deref(), Some("fix X /approve auto then Y"));
        // Once before the paste and once after it, not once per character.
        let draws = String::from_utf8_lossy(&output).matches("info").count();
        assert!(draws <= 3, "{draws} draws");
        // A lone Enter still sends.
        let mut input = Cursor::new(b"hi\r".to_vec());
        let (line, _) = read_boxed(
            &mut input,
            &mut Vec::new(),
            Style::plain(),
            (60, 24),
            &[],
            &|_| "info".to_owned(),
            &mut Vec::new,
        );
        assert_eq!(line.as_deref(), Some("hi"));
    }

    #[test]
    fn text_put_back_in_the_box_can_be_edited_and_sent() {
        let mut input = Cursor::new(b" now\r".to_vec());
        let mut output = Vec::new();
        let (line, _, _) = read_boxed_waking(
            "fix the bug",
            &mut input,
            &mut output,
            Style::plain(),
            (30, 24),
            &[],
            &|_| "info".to_owned(),
            &mut Vec::new,
            None,
        );
        assert_eq!(line.as_deref(), Some("fix the bug now"));
        let mut screen = Screen::new(30);
        screen.feed(&output);
        assert!(screen.lines().join("\n").contains("fix the bug"));
    }

    #[test]
    fn more_lines_in_the_info_are_a_list_under_the_box_and_the_working_box_takes_rows_for_them() {
        let (shown, rows) = static_box(
            Style::plain(),
            (24, 30),
            "working…",
            "info\n● s1  sleep\n● a2  reviewer",
            &[" steps 0/1".to_owned(), "  ▸ read".to_owned()],
        );
        assert_eq!(rows, 8);
        let mut screen = Screen::new(30);
        screen.feed(shown.as_bytes());
        let lines = screen.lines();
        assert_eq!(lines[0], " steps 0/1");
        assert_eq!(lines[1], "  ▸ read");
        assert_eq!(lines[2], format!("╭{}╮", "─".repeat(27)));
        assert_eq!(lines[5], " info");
        assert_eq!(lines[6], " ● s1  sleep");
        assert_eq!(lines[7], " ● a2  reviewer");
        assert_eq!(screen.row, 8);

        let mut scroll = 0;
        let drawn = frame(
            Style::plain(),
            (24, 30),
            &[],
            0,
            "info\nlisted task",
            &mut scroll,
            None,
            (&[], 0),
        );
        let mut screen = Screen::new(30);
        screen.feed(drawn.text.as_bytes());
        let lines = screen.lines();
        assert_eq!(lines[3], " info");
        assert_eq!(lines[4], " listed task");
        assert_eq!(screen.row, 1, "the cursor is in the box");
    }

    #[test]
    fn erasing_takes_the_whole_box_off_the_screen_and_leaves_the_text_above_it() {
        let mut input = Cursor::new(b"hello".to_vec());
        let mut output = b"earlier output\r\n".to_vec();
        let (line, row) = read_boxed(
            &mut input,
            &mut output,
            Style::plain(),
            (24, 30),
            &[],
            &|_| "info".to_owned(),
            &mut Vec::new,
        );
        assert_eq!(line, None);
        output.extend_from_slice(erase(row).as_bytes());
        output.extend_from_slice(b"what I typed\r\n");
        let mut screen = Screen::new(30);
        screen.feed(&output);
        assert_eq!(screen.lines(), ["earlier output", "what I typed"]);
    }

    #[test]
    fn the_box_for_the_time_the_model_works_sits_under_the_output_and_can_be_replaced() {
        let (shown, rows) = static_box(Style::plain(), (24, 30), "working…", "info", &[]);
        assert_eq!(rows, 4);
        let mut output = shown.into_bytes();
        // A line of output goes above it: erase, print, draw again.
        output.extend_from_slice(erase(rows).as_bytes());
        output.extend_from_slice(b"a tool line\r\n");
        output.extend_from_slice(
            static_box(Style::plain(), (24, 30), "working…", "info", &[])
                .0
                .as_bytes(),
        );
        let mut screen = Screen::new(30);
        screen.feed(&output);
        let lines = screen.lines();
        assert_eq!(lines[0], "a tool line");
        assert_eq!(lines[1], format!("╭{}╮", "─".repeat(27)));
        assert_eq!(lines[2], format!("│ working…{} │", " ".repeat(17)));
        assert_eq!(lines[4], " info");
        assert_eq!(lines.len(), 5);
        assert_eq!(screen.row, 5);
    }

    #[test]
    fn a_narrow_terminal_still_draws_a_box() {
        let (line, screen) = run_keys(20, 24, b"ok\r");
        assert_eq!(line.as_deref(), Some("ok"));
        assert!(screen.lines()[0].starts_with('╭'));
    }

    #[test]
    fn colour_paints_the_box_cyan_and_the_info_dim() {
        let mut scroll = 0;
        let chars: Vec<char> = "hi".chars().collect();
        let drawn = frame(
            Style::colored(),
            (24, 30),
            &chars,
            2,
            "info",
            &mut scroll,
            None,
            (&[], 0),
        );
        assert!(drawn.text.starts_with("\x1b[36m╭"), "{:?}", drawn.text);
        assert!(drawn.text.contains("\x1b[36m│ hi"), "{:?}", drawn.text);
        assert!(
            drawn.text.contains("\x1b[2m info\x1b[0m"),
            "{:?}",
            drawn.text
        );
    }
    fn run_with_files(width: usize, keys: &[u8], files: &[&str]) -> (Option<String>, Screen) {
        let list: Vec<String> = files.iter().map(|s| s.to_string()).collect();
        let mut input = Cursor::new(keys.to_vec());
        let mut output = Vec::new();
        let (result, _row) = read_boxed(
            &mut input,
            &mut output,
            Style::plain(),
            (24, width),
            &[],
            &|_| "info line".to_owned(),
            &mut || list.clone(),
        );
        let mut screen = Screen::new(width);
        screen.feed(&output);
        (result, screen)
    }

    const FILES: &[&str] = &["README.md", "src/main.rs", "src/lib.rs", "docs/main.md"];

    #[test]
    fn picking_a_folder_leaves_it_open_for_the_files_in_it() {
        let files = ["docs/", "docs/a.md", "docs/b.md", "src/", "src/main.rs"];
        // Tab on `docs/` (the best match) gives `@docs/` with no space after it, and the
        // files in it are the choices now.
        let (_, screen) = run_with_files(40, b"@do\t", &files);
        let rows = screen.lines();
        assert_eq!(rows[1], format!("│ @docs/{} │", " ".repeat(35 - 6)));
        assert_eq!(rows[4], " ▸ docs/a.md");
        // Tab on a file puts a space after it.
        let (line, _) = run_with_files(40, b"@do\t\t\r", &files);
        assert_eq!(line.as_deref(), Some("@docs/a.md "));
    }

    #[test]
    fn typing_an_at_lists_matching_files_under_the_info_line() {
        // No Enter: the last drawing stays on screen.
        let (_, screen) = run_with_files(40, b"look @ma", FILES);
        let rows = screen.lines();
        assert_eq!(rows[1], format!("│ look @ma{} │", " ".repeat(35 - 8)));
        assert_eq!(rows[3], " info line");
        assert_eq!(rows[4], " ▸ src/main.rs");
        assert_eq!(rows[5], "   docs/main.md");
        assert_eq!(rows.len(), 6, "{rows:?}");
    }

    #[test]
    fn the_list_is_only_made_when_an_at_word_is_typed() {
        let mut called = 0;
        let mut input = Cursor::new(b"hello world\r".to_vec());
        let mut output = Vec::new();
        read_boxed(
            &mut input,
            &mut output,
            Style::plain(),
            (24, 40),
            &[],
            &|_| String::new(),
            &mut || {
                called += 1;
                Vec::new()
            },
        );
        assert_eq!(called, 0);
    }

    #[test]
    fn down_then_tab_puts_the_second_file_in_the_line_and_enter_then_sends() {
        let (line, screen) = run_with_files(40, b"see @ma\x1b[B\t and\r", FILES);
        assert_eq!(line.as_deref(), Some("see @docs/main.md  and"));
        assert_eq!(screen.lines().len(), 4);
    }

    #[test]
    fn enter_picks_while_the_list_is_open_and_sends_once_it_is_closed() {
        let (line, _) = run_with_files(40, b"@read\r\r", FILES);
        assert_eq!(line.as_deref(), Some("@README.md "));
        // Nothing matches: no list, so Enter sends what was typed.
        let (line, _) = run_with_files(40, b"@zzz\r", FILES);
        assert_eq!(line.as_deref(), Some("@zzz"));
    }

    #[test]
    fn escape_closes_the_list_for_that_word_and_up_down_do_not_leave_the_choices() {
        let (line, screen) = run_with_files(40, b"@src\x1b\r", FILES);
        assert_eq!(line.as_deref(), Some("@src"));
        assert_eq!(screen.lines().len(), 4);
        // Up at the top and Down at the bottom stay inside the list.
        let (line, _) = run_with_files(40, b"@src\x1b[A\x1b[B\x1b[B\x1b[B\t\r", FILES);
        assert_eq!(line.as_deref(), Some("@src/main.rs "));
    }

    #[test]
    fn a_word_without_a_leading_at_or_in_the_middle_of_a_word_shows_no_list() {
        let (_, screen) = run_with_files(40, b"mail me@ma", FILES);
        assert_eq!(screen.lines().len(), 4);
    }

    #[test]
    fn the_list_leaves_nothing_behind_when_it_closes() {
        let (_, screen) = run_with_files(40, b"@ma \x7f\x7f\x7f", FILES);
        // Back to the plain word "@" + nothing: the list for an empty query is open, but the text
        // above is the box; make sure only one box is on screen.
        let rows = screen.lines();
        assert_eq!(
            rows.iter().filter(|r| r.starts_with('╭')).count(),
            1,
            "{rows:?}"
        );
    }
    #[test]
    fn a_wait_that_finds_nothing_can_wake_the_box_and_returns_what_was_typed() {
        let run = |wake: bool| {
            let mut input = Cursor::new(Vec::new());
            let mut output = Vec::new();
            let always = move || wake;
            read_boxed_waking(
                "abc",
                &mut input,
                &mut output,
                Style::plain(),
                (24, 80),
                &[],
                &|_| String::new(),
                &mut Vec::new,
                Some(&always),
            )
        };
        let (line, _, woke) = run(true);
        assert_eq!((line, woke), (None, Some("abc".to_owned())));
        // An input that has ended for good does not spin: it ends the box.
        let (line, _, woke) = run(false);
        assert_eq!((line, woke), (None, None));
    }
}

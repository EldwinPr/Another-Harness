//! Editing the line you type at a real terminal: arrow keys, Home/End, Delete, word deletion
//! (Ctrl-Backspace, Ctrl-W, Alt-Backspace), Ctrl-A/E/U/K, and the lines typed earlier with
//! Up/Down. Without this the terminal's own line input shows arrow keys as `^[[D`.
//!
//! The terminal is in raw mode while a line is read ([`crate::repl`] switches it with `stty`), so
//! this module draws the line itself: one row, scrolled sideways when it is longer than the
//! screen. Each character counts as one column (wide characters are not measured). Only used on
//! a real terminal; piped input and the tests read plain lines.

use std::io::{BufRead, Write};

/// What one key press means to the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    Left,
    Right,
    WordLeft,
    WordRight,
    Home,
    End,
    Backspace,
    Delete,
    DeleteWord,
    DeleteToStart,
    DeleteToEnd,
    Up,
    Down,
    Enter,
    /// Ctrl-D: end of input on an empty line, else delete the character under the cursor.
    Eof,
    /// Ctrl-C: clear the line; on an empty line, end the input.
    Interrupt,
    Char(char),
}

/// Reads the first key from `bytes` (not empty). Returns the key (`None` for something to ignore)
/// and how many bytes it used. Ctrl-Backspace arrives as `0x08` in most terminals (plain
/// Backspace is `0x7f`) and as a modified sequence in some.
pub fn parse_edit(bytes: &[u8]) -> (Option<Edit>, usize) {
    let first = bytes[0];
    match first {
        0x1b => match bytes.get(1) {
            Some(b'[' | b'O') => {
                let mut end = 2;
                while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b';') {
                    end += 1;
                }
                let Some(&last) = bytes.get(end) else {
                    return (None, bytes.len());
                };
                let params = &bytes[2..end];
                // `1;5` or `1;3`: Ctrl or Alt held with an arrow key.
                let word = params.ends_with(b";5") || params.ends_with(b";3");
                let key = match (last, params) {
                    (b'A', _) => Some(Edit::Up),
                    (b'B', _) => Some(Edit::Down),
                    (b'C', _) if word => Some(Edit::WordRight),
                    (b'D', _) if word => Some(Edit::WordLeft),
                    (b'C', _) => Some(Edit::Right),
                    (b'D', _) => Some(Edit::Left),
                    (b'H', _) | (b'~', b"1" | b"7") => Some(Edit::Home),
                    (b'F', _) | (b'~', b"4" | b"8") => Some(Edit::End),
                    (b'~', b"3") | (b'~', b"3;5") => Some(Edit::Delete),
                    // Ctrl-Backspace as a modified key (xterm modifyOtherKeys, kitty).
                    (b'~', b"27;5;127" | b"27;5;8") | (b'u', b"127;5" | b"8;5") => {
                        Some(Edit::DeleteWord)
                    }
                    _ => None,
                };
                (key, end + 1)
            }
            // Alt-Backspace, Alt-b, Alt-f.
            Some(0x7f) => (Some(Edit::DeleteWord), 2),
            Some(b'b') => (Some(Edit::WordLeft), 2),
            Some(b'f') => (Some(Edit::WordRight), 2),
            // The Escape key on its own: nothing to do in a line.
            _ => (None, 1),
        },
        b'\r' | b'\n' => (Some(Edit::Enter), 1),
        0x7f => (Some(Edit::Backspace), 1),
        0x08 | 0x17 => (Some(Edit::DeleteWord), 1),
        0x01 => (Some(Edit::Home), 1),
        0x05 => (Some(Edit::End), 1),
        0x02 => (Some(Edit::Left), 1),
        0x06 => (Some(Edit::Right), 1),
        0x10 => (Some(Edit::Up), 1),
        0x0e => (Some(Edit::Down), 1),
        0x15 => (Some(Edit::DeleteToStart), 1),
        0x0b => (Some(Edit::DeleteToEnd), 1),
        0x04 => (Some(Edit::Eof), 1),
        0x03 => (Some(Edit::Interrupt), 1),
        b if b < 0x20 => (None, 1),
        _ => {
            let length = match first {
                0xf0..=0xf7 => 4,
                0xe0..=0xef => 3,
                0xc0..=0xdf => 2,
                _ => 1,
            }
            .min(bytes.len());
            let text = String::from_utf8_lossy(&bytes[..length]);
            (text.chars().next().map(Edit::Char), length)
        }
    }
}

/// What a key did to the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Continue,
    Submit,
    Eof,
}

/// The line being typed, the cursor in it, and the history being browsed.
pub struct LineEditor {
    chars: Vec<char>,
    cursor: usize,
    /// First character shown, when the line is wider than the screen.
    scroll: usize,
    history: Vec<String>,
    /// The history entry on screen, if Up was pressed.
    at: Option<usize>,
    /// What was typed before browsing, put back by Down past the newest entry.
    draft: Vec<char>,
}

impl LineEditor {
    pub fn new(history: &[String]) -> Self {
        Self {
            chars: Vec::new(),
            cursor: 0,
            scroll: 0,
            history: history.to_vec(),
            at: None,
            draft: Vec::new(),
        }
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    fn word_start(&self, from: usize) -> usize {
        let mut i = from;
        while i > 0 && self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    fn word_end(&self, from: usize) -> usize {
        let mut i = from;
        while i < self.chars.len() && self.chars[i].is_whitespace() {
            i += 1;
        }
        while i < self.chars.len() && !self.chars[i].is_whitespace() {
            i += 1;
        }
        i
    }

    fn show(&mut self, index: usize) {
        self.chars = self.history[index].chars().collect();
        self.cursor = self.chars.len();
    }

    pub fn key(&mut self, edit: Edit) -> Step {
        match edit {
            Edit::Char(c) => {
                self.chars.insert(self.cursor, c);
                self.cursor += 1;
            }
            Edit::Left => self.cursor = self.cursor.saturating_sub(1),
            Edit::Right => self.cursor = (self.cursor + 1).min(self.chars.len()),
            Edit::WordLeft => self.cursor = self.word_start(self.cursor),
            Edit::WordRight => self.cursor = self.word_end(self.cursor),
            Edit::Home => self.cursor = 0,
            Edit::End => self.cursor = self.chars.len(),
            Edit::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.chars.remove(self.cursor);
                }
            }
            Edit::Delete => {
                if self.cursor < self.chars.len() {
                    self.chars.remove(self.cursor);
                }
            }
            Edit::DeleteWord => {
                let start = self.word_start(self.cursor);
                self.chars.drain(start..self.cursor);
                self.cursor = start;
            }
            Edit::DeleteToStart => {
                self.chars.drain(..self.cursor);
                self.cursor = 0;
            }
            Edit::DeleteToEnd => self.chars.truncate(self.cursor),
            Edit::Up => {
                let next = match self.at {
                    None if self.history.is_empty() => return Step::Continue,
                    None => {
                        self.draft = self.chars.clone();
                        self.history.len() - 1
                    }
                    Some(0) => 0,
                    Some(i) => i - 1,
                };
                self.at = Some(next);
                self.show(next);
            }
            Edit::Down => match self.at {
                None => {}
                Some(i) if i + 1 < self.history.len() => {
                    self.at = Some(i + 1);
                    self.show(i + 1);
                }
                Some(_) => {
                    self.at = None;
                    self.chars = std::mem::take(&mut self.draft);
                    self.cursor = self.chars.len();
                }
            },
            Edit::Enter => return Step::Submit,
            Edit::Eof => {
                if self.chars.is_empty() {
                    return Step::Eof;
                }
                if self.cursor < self.chars.len() {
                    self.chars.remove(self.cursor);
                }
            }
            Edit::Interrupt => {
                if self.chars.is_empty() {
                    return Step::Eof;
                }
                self.chars.clear();
                self.cursor = 0;
            }
        }
        Step::Continue
    }

    /// The bytes that draw the line after a key: back to the start of the row, past the prompt
    /// (`origin` columns), the part of the line that fits in `width` columns, clear the rest,
    /// then the cursor to its place.
    pub fn render(&mut self, origin: usize, width: usize) -> String {
        let room = width.saturating_sub(origin + 1).max(1);
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        }
        if self.cursor >= self.scroll + room {
            self.scroll = self.cursor + 1 - room;
        }
        let end = (self.scroll + room).min(self.chars.len());
        let shown: String = self.chars[self.scroll..end].iter().collect();
        let column = origin + self.cursor - self.scroll;
        let to = |n: usize| {
            if n == 0 {
                String::new()
            } else {
                format!("\x1b[{n}C")
            }
        };
        format!("\r{}{shown}\x1b[K\r{}", to(origin), to(column))
    }
}

/// Reads one line with editing: the keys come from `input`, the drawing goes to `output`. The
/// prompt is already on the row, `origin` columns wide. Returns `None` at the end of the input
/// (Ctrl-D or Ctrl-C on an empty line, or the stream ended). On Enter the cursor moves to the
/// next row.
pub fn read_edited<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    origin: usize,
    width: usize,
    history: &[String],
) -> Option<String> {
    let mut editor = LineEditor::new(history);
    loop {
        let _ = output.write_all(editor.render(origin, width).as_bytes());
        let _ = output.flush();
        let (edit, used) = loop {
            let buffer = input.fill_buf().ok()?;
            if buffer.is_empty() {
                return None;
            }
            let (edit, used) = parse_edit(buffer);
            input.consume(used);
            if let Some(edit) = edit {
                break (edit, used);
            }
        };
        let _ = used;
        match editor.key(edit) {
            Step::Continue => {}
            Step::Submit => {
                let _ = output.write_all(b"\r\n");
                let _ = output.flush();
                return Some(editor.text());
            }
            Step::Eof => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn keys(bytes: &[u8]) -> Vec<Edit> {
        let mut rest = bytes;
        let mut all = Vec::new();
        while !rest.is_empty() {
            let (edit, used) = parse_edit(rest);
            all.extend(edit);
            rest = &rest[used..];
        }
        all
    }

    fn type_into(editor: &mut LineEditor, bytes: &[u8]) -> Step {
        let mut step = Step::Continue;
        for key in keys(bytes) {
            step = editor.key(key);
        }
        step
    }

    #[test]
    fn arrow_and_editing_sequences_are_recognised() {
        assert_eq!(
            keys(b"\x1b[D\x1b[C\x1b[A\x1b[B"),
            [Edit::Left, Edit::Right, Edit::Up, Edit::Down]
        );
        assert_eq!(keys(b"\x1bOD\x1bOC"), [Edit::Left, Edit::Right]);
        assert_eq!(
            keys(b"\x1b[H\x1b[F\x1b[1~\x1b[4~"),
            [Edit::Home, Edit::End, Edit::Home, Edit::End]
        );
        assert_eq!(keys(b"\x1b[3~"), [Edit::Delete]);
        assert_eq!(
            keys(b"\x1b[1;5D\x1b[1;5C\x1bb\x1bf"),
            [
                Edit::WordLeft,
                Edit::WordRight,
                Edit::WordLeft,
                Edit::WordRight
            ]
        );
        assert_eq!(keys(b"\x7f"), [Edit::Backspace]);
    }

    #[test]
    fn ctrl_backspace_is_a_word_deletion_in_each_form_terminals_send_it() {
        for form in [
            &b"\x08"[..],
            b"\x17",
            b"\x1b\x7f",
            b"\x1b[27;5;127~",
            b"\x1b[127;5u",
        ] {
            assert_eq!(keys(form), [Edit::DeleteWord], "{form:?}");
        }
    }

    #[test]
    fn unknown_sequences_and_a_lone_escape_are_ignored_and_text_is_utf8() {
        assert_eq!(keys(b"\x1b[99Z"), []);
        assert_eq!(keys(b"\x1b"), []);
        assert_eq!(
            keys("é日a".as_bytes()),
            [Edit::Char('é'), Edit::Char('日'), Edit::Char('a')]
        );
        assert_eq!(parse_edit(b"\x1b[D rest"), (Some(Edit::Left), 3));
    }

    #[test]
    fn characters_go_in_at_the_cursor() {
        let mut e = LineEditor::new(&[]);
        type_into(&mut e, b"helo");
        type_into(&mut e, b"\x1b[D");
        type_into(&mut e, b"l");
        assert_eq!(e.text(), "hello");
        assert_eq!(e.cursor(), 4);
        type_into(&mut e, b"\x01X\x05!");
        assert_eq!(e.text(), "Xhello!");
    }

    #[test]
    fn backspace_delete_and_the_kill_keys() {
        let mut e = LineEditor::new(&[]);
        type_into(&mut e, b"one two three");
        type_into(&mut e, b"\x7f");
        assert_eq!(e.text(), "one two thre");
        type_into(&mut e, b"\x1b[D\x1b[D\x1b[3~");
        assert_eq!(e.text(), "one two the");
        type_into(&mut e, b"\x15");
        assert_eq!(e.text(), "e");
        assert_eq!(e.cursor(), 0);
        type_into(&mut e, b"abc def\x1b[D\x1b[D\x1b[D\x0b");
        assert_eq!(e.text(), "abc ");
        // Backspace at the start and Delete at the end do nothing.
        let mut e = LineEditor::new(&[]);
        type_into(&mut e, b"\x7f\x1b[3~");
        assert_eq!(e.text(), "");
    }

    #[test]
    fn a_word_deletion_takes_the_word_before_the_cursor_and_its_spaces() {
        let mut e = LineEditor::new(&[]);
        type_into(&mut e, b"one two  three  ");
        type_into(&mut e, b"\x08");
        assert_eq!(e.text(), "one two  ");
        type_into(&mut e, b"\x08");
        assert_eq!(e.text(), "one ");
        type_into(&mut e, b"\x1b[1;5D");
        assert_eq!(e.cursor(), 0);
        type_into(&mut e, b"\x1b[1;5C");
        assert_eq!(e.cursor(), 3);
        // In the middle of a word it deletes back to the start of that word.
        let mut e = LineEditor::new(&[]);
        type_into(&mut e, b"hello world\x1b[D\x1b[D\x08");
        assert_eq!(e.text(), "hello ld");
        // Nothing before the cursor: nothing happens.
        type_into(&mut e, b"\x01\x08");
        assert_eq!(e.text(), "hello ld");
    }

    #[test]
    fn up_and_down_browse_the_history_and_bring_back_the_unfinished_line() {
        let history = ["first".to_owned(), "second".to_owned()];
        let mut e = LineEditor::new(&history);
        type_into(&mut e, b"draft");
        type_into(&mut e, b"\x1b[A");
        assert_eq!(e.text(), "second");
        type_into(&mut e, b"\x1b[A\x1b[A");
        assert_eq!(e.text(), "first", "stops at the oldest");
        type_into(&mut e, b"\x1b[B");
        assert_eq!(e.text(), "second");
        type_into(&mut e, b"\x1b[B");
        assert_eq!(e.text(), "draft");
        type_into(&mut e, b"\x1b[B");
        assert_eq!(e.text(), "draft");
        // With no history Up does nothing.
        let mut e = LineEditor::new(&[]);
        type_into(&mut e, b"x\x1b[A");
        assert_eq!(e.text(), "x");
    }

    #[test]
    fn a_recalled_line_can_be_edited_before_it_is_sent() {
        let history = ["list files".to_owned()];
        let mut e = LineEditor::new(&history);
        type_into(&mut e, b"\x1b[A\x08");
        assert_eq!(e.text(), "list ");
        type_into(&mut e, b"docs");
        assert_eq!(e.text(), "list docs");
    }

    #[test]
    fn enter_submits_and_ctrl_d_and_ctrl_c_end_only_an_empty_line() {
        let mut e = LineEditor::new(&[]);
        assert_eq!(type_into(&mut e, b"hi\r"), Step::Submit);
        let mut e = LineEditor::new(&[]);
        assert_eq!(type_into(&mut e, b"\x04"), Step::Eof);
        assert_eq!(type_into(&mut e, b"\x03"), Step::Eof);
        let mut e = LineEditor::new(&[]);
        type_into(&mut e, b"ab\x1b[D\x04");
        assert_eq!(
            e.text(),
            "a",
            "Ctrl-D in a line deletes the character under the cursor"
        );
        assert_eq!(type_into(&mut e, b"\x03"), Step::Continue);
        assert_eq!(e.text(), "", "Ctrl-C clears a line that has text");
        assert_eq!(type_into(&mut e, b"\x03"), Step::Eof);
    }

    #[test]
    fn the_row_is_drawn_after_the_prompt_with_the_cursor_in_place() {
        let mut e = LineEditor::new(&[]);
        type_into(&mut e, b"hello\x1b[D\x1b[D");
        assert_eq!(e.render(2, 80), "\r\x1b[2Chello\x1b[K\r\x1b[5C");
        let mut empty = LineEditor::new(&[]);
        assert_eq!(empty.render(0, 80), "\r\x1b[K\r");
    }

    #[test]
    fn a_line_longer_than_the_screen_scrolls_to_keep_the_cursor_in_view() {
        let mut e = LineEditor::new(&[]);
        type_into(&mut e, b"0123456789");
        // 2 for the prompt, 8 wide: room for 5 characters, the cursor at the end.
        let drawn = e.render(2, 8);
        assert_eq!(drawn, "\r\x1b[2C6789\x1b[K\r\x1b[6C");
        assert_eq!(e.cursor(), 10);
    }

    #[test]
    fn a_scrolled_line_scrolls_back_when_the_cursor_returns_to_the_start() {
        let mut e = LineEditor::new(&[]);
        type_into(&mut e, b"0123456789");
        e.render(2, 8);
        type_into(&mut e, b"\x01");
        assert_eq!(e.render(2, 8), "\r\x1b[2C01234\x1b[K\r\x1b[2C");
    }

    #[test]
    fn reading_a_line_from_scripted_keys_returns_it_and_leaves_the_rest_unread() {
        let mut input = Cursor::new(b"helo\x1b[Dl\rnext".to_vec());
        let mut output = Vec::new();
        let line = read_edited(&mut input, &mut output, 2, 80, &[]);
        assert_eq!(line.as_deref(), Some("hello"));
        let mut rest = String::new();
        std::io::Read::read_to_string(&mut input, &mut rest).unwrap();
        assert_eq!(rest, "next");
        let drawn = String::from_utf8(output).unwrap();
        assert!(drawn.ends_with("\r\n"), "{drawn:?}");
        assert!(!drawn.contains('^'), "{drawn:?}");
    }

    #[test]
    fn the_end_of_the_input_or_ctrl_d_gives_none() {
        let mut output = Vec::new();
        assert_eq!(
            read_edited(&mut Cursor::new(Vec::new()), &mut output, 0, 80, &[]),
            None
        );
        assert_eq!(
            read_edited(&mut Cursor::new(b"\x04".to_vec()), &mut output, 0, 80, &[]),
            None
        );
        // Text typed but no Enter before the stream ends is not a line.
        assert_eq!(
            read_edited(&mut Cursor::new(b"abc".to_vec()), &mut output, 0, 80, &[]),
            None
        );
    }
}

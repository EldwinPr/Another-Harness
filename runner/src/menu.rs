//! Arrow-key menus (issue 11): reading keys, the state of one menu, and how it is drawn.
//!
//! Three small parts, none of which touches the terminal by itself:
//! - [`parse_key`] turns the bytes a terminal sends into a [`Key`];
//! - [`MenuState`] decides what a key does (move, filter, choose, cancel);
//! - [`MenuState::render`] makes the lines to print.
//!
//! `repl::Terminal::pick` joins them to a real or scripted input. Keeping the parts apart is what
//! lets tests press keys without a terminal.

use crate::style::Style;

/// One key press the menus understand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Enter,
    /// The Escape key on its own (not the start of an arrow sequence).
    Esc,
    Backspace,
    /// Ctrl-C or Ctrl-D: leave the menu.
    Cancel,
    Char(char),
}

/// Reads the first key from `bytes`. Returns the key (`None` for something to ignore, such as
/// an unknown escape sequence) and how many bytes it used. `bytes` must not be empty.
///
/// Terminals send an arrow key as `ESC [ A` (or `ESC O A`) in one write, so an `ESC` that is the
/// last byte available is the Escape key itself.
pub fn parse_key(bytes: &[u8]) -> (Option<Key>, usize) {
    let first = bytes[0];
    match first {
        0x1b => {
            let introduced = matches!(bytes.get(1), Some(b'[' | b'O'));
            if !introduced {
                return (Some(Key::Esc), 1);
            }
            // ESC [ <digits and ;> <final byte>
            let mut end = 2;
            while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b';') {
                end += 1;
            }
            let Some(&last) = bytes.get(end) else {
                // The sequence was cut short: drop what there is.
                return (None, bytes.len());
            };
            let params = &bytes[2..end];
            let key = match (last, params) {
                (b'A', _) => Some(Key::Up),
                (b'B', _) => Some(Key::Down),
                (b'H', _) | (b'~', b"1" | b"7") => Some(Key::Home),
                (b'F', _) | (b'~', b"4" | b"8") => Some(Key::End),
                (b'~', b"5") => Some(Key::PageUp),
                (b'~', b"6") => Some(Key::PageDown),
                _ => None,
            };
            (key, end + 1)
        }
        b'\r' | b'\n' => (Some(Key::Enter), 1),
        0x7f | 0x08 => (Some(Key::Backspace), 1),
        0x03 | 0x04 => (Some(Key::Cancel), 1),
        b if b < 0x20 => (None, 1),
        _ => {
            // A printable character, possibly several bytes of UTF-8.
            let length = match first {
                0xf0..=0xf7 => 4,
                0xe0..=0xef => 3,
                0xc0..=0xdf => 2,
                _ => 1,
            }
            .min(bytes.len());
            match std::str::from_utf8(&bytes[..length])
                .ok()
                .and_then(|s| s.chars().next())
            {
                Some(c) => (Some(Key::Char(c)), length),
                None => (None, length),
            }
        }
    }
}

/// All the keys in `bytes`, in order (tests use this; the terminal reads one at a time).
pub fn parse_keys(mut bytes: &[u8]) -> Vec<Key> {
    let mut keys = Vec::new();
    while !bytes.is_empty() {
        let (key, used) = parse_key(bytes);
        keys.extend(key);
        bytes = &bytes[used..];
    }
    keys
}

/// One row of a menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    /// What is shown, and what typing filters on.
    pub label: String,
    /// Shown dimmed after the label; also filtered on.
    pub detail: String,
    /// Kept in the list while filtering (an action such as "+ add new").
    pub always_shown: bool,
}

impl MenuItem {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: String::new(),
            always_shown: false,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    /// An action row that stays visible while the list is filtered.
    pub fn action(label: impl Into<String>) -> Self {
        Self {
            always_shown: true,
            ..Self::new(label)
        }
    }

    fn matches(&self, filter: &str) -> bool {
        if filter.is_empty() || self.always_shown {
            return true;
        }
        let wanted = filter.to_lowercase();
        self.label.to_lowercase().contains(&wanted) || self.detail.to_lowercase().contains(&wanted)
    }
}

/// What a key did to a menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Continue,
    /// The row with this index (in the original list) was chosen.
    Chosen(usize),
    Cancelled,
}

/// The state of one open menu.
pub struct MenuState {
    items: Vec<MenuItem>,
    filter: String,
    /// Position among the visible rows.
    cursor: usize,
    /// First visible row shown (the list scrolls).
    offset: usize,
    height: usize,
    /// Columns available; rows are cut to fit so one row is always one screen line (the menu is
    /// redrawn by moving the cursor up that many lines).
    width: usize,
}

/// Longest line the menus draw when the terminal's width is unknown.
pub const DEFAULT_WIDTH: usize = 80;

/// Cuts `text` to at most `max` characters, ending in `...` when something was cut.
fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let keep = max.saturating_sub(3);
    let mut shortened: String = text.chars().take(keep).collect();
    shortened.push_str("...");
    shortened
}

impl MenuState {
    /// `height` is how many rows are shown at once (at least 1).
    pub fn new(items: Vec<MenuItem>, height: usize) -> Self {
        Self {
            items,
            filter: String::new(),
            cursor: 0,
            offset: 0,
            height: height.max(1),
            width: DEFAULT_WIDTH,
        }
    }

    /// Starts with this row (an index into the original list) highlighted.
    pub fn starting_at(mut self, index: usize) -> Self {
        let rows = self.visible().len();
        self.move_to(index, rows);
        self
    }

    /// Sets how many columns a row may use.
    pub fn with_width(mut self, width: usize) -> Self {
        self.width = width.max(20);
        self
    }

    /// Indexes (into the original list) of the rows that pass the filter.
    pub fn visible(&self) -> Vec<usize> {
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.matches(&self.filter))
            .map(|(index, _)| index)
            .collect()
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// The highlighted row's index in the original list, if any row is visible.
    pub fn highlighted(&self) -> Option<usize> {
        self.visible().get(self.cursor).copied()
    }

    fn move_to(&mut self, position: usize, rows: usize) {
        self.cursor = position.min(rows.saturating_sub(1));
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + self.height {
            self.offset = self.cursor + 1 - self.height;
        }
    }

    pub fn key(&mut self, key: Key) -> Step {
        let rows = self.visible().len();
        match key {
            Key::Up if rows > 0 => {
                // Up from the top goes to the bottom, so a short menu is a ring.
                let next = if self.cursor == 0 {
                    rows - 1
                } else {
                    self.cursor - 1
                };
                self.move_to(next, rows);
            }
            Key::Down if rows > 0 => {
                let next = if self.cursor + 1 >= rows {
                    0
                } else {
                    self.cursor + 1
                };
                self.move_to(next, rows);
            }
            Key::PageUp => self.move_to(self.cursor.saturating_sub(self.height - 1), rows),
            Key::PageDown => self.move_to(self.cursor + self.height - 1, rows),
            Key::Home => self.move_to(0, rows),
            Key::End => self.move_to(rows.saturating_sub(1), rows),
            Key::Enter => {
                if let Some(index) = self.highlighted() {
                    return Step::Chosen(index);
                }
            }
            Key::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.move_to(0, self.visible().len());
                self.offset = 0;
            }
            Key::Esc | Key::Cancel => return Step::Cancelled,
            Key::Backspace => {
                self.filter.pop();
                self.offset = 0;
                self.move_to(0, self.visible().len());
            }
            Key::Char(c) => {
                self.filter.push(c);
                self.offset = 0;
                self.move_to(0, self.visible().len());
            }
            Key::Up | Key::Down => {}
        }
        Step::Continue
    }

    /// The lines to print: the title (with the filter, if any), the visible window of rows with
    /// the highlighted one marked, and a hint. No line ends with a newline.
    pub fn render(&self, title: &str, style: Style) -> Vec<String> {
        let visible = self.visible();
        let mut lines = Vec::new();
        let head = if self.filter.is_empty() {
            style.bold(title)
        } else {
            format!(
                "{}  {}",
                style.bold(title),
                style.yellow(&format!("search: {}", self.filter))
            )
        };
        lines.push(head);
        if visible.is_empty() {
            lines.push(format!("  {}", style.dim("nothing matches")));
        }
        let end = (self.offset + self.height).min(visible.len());
        for (position, index) in visible
            .iter()
            .enumerate()
            .take(end)
            .skip(self.offset.min(visible.len()))
        {
            let item = &self.items[*index];
            // Two columns for the marker, then the label, then the dimmed detail.
            let room = self.width.saturating_sub(3);
            let label = cut(&item.label, room);
            let detail_room = room.saturating_sub(label.chars().count() + 2);
            let detail = if item.detail.is_empty() || detail_room < 4 {
                String::new()
            } else {
                format!("  {}", style.dim(&cut(&item.detail, detail_room)))
            };
            if position == self.cursor {
                lines.push(format!(
                    "{} {}{detail}",
                    style.accent(">"),
                    style.accent(&label)
                ));
            } else {
                lines.push(format!("  {label}{detail}"));
            }
        }
        let more = visible.len().saturating_sub(end);
        let counts = if more > 0 || self.offset > 0 {
            format!(
                "  ({} of {})",
                self.cursor.min(visible.len().saturating_sub(1)) + 1,
                visible.len()
            )
        } else {
            String::new()
        };
        lines.push(style.dim(&format!(
            "up/down move, Enter chooses, Esc goes back, type to search{counts}"
        )));
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(labels: &[&str]) -> Vec<MenuItem> {
        labels.iter().map(|l| MenuItem::new(*l)).collect()
    }

    #[test]
    fn arrows_and_friends_are_recognised_in_both_spellings() {
        assert_eq!(
            parse_keys(b"\x1b[A\x1b[B\x1bOA\x1bOB"),
            [Key::Up, Key::Down, Key::Up, Key::Down]
        );
        assert_eq!(
            parse_keys(b"\x1b[H\x1b[F\x1b[1~\x1b[4~\x1b[5~\x1b[6~"),
            [
                Key::Home,
                Key::End,
                Key::Home,
                Key::End,
                Key::PageUp,
                Key::PageDown
            ]
        );
        // Modifiers (ESC [ 1 ; 5 A) still mean the arrow.
        assert_eq!(parse_keys(b"\x1b[1;5A"), [Key::Up]);
    }

    #[test]
    fn a_lone_escape_is_the_escape_key_and_unknown_sequences_are_ignored() {
        assert_eq!(parse_keys(b"\x1b"), [Key::Esc]);
        assert_eq!(parse_keys(b"a\x1b"), [Key::Char('a'), Key::Esc]);
        assert!(
            parse_keys(b"\x1b[Z").is_empty(),
            "shift-tab is not a menu key"
        );
        assert!(
            parse_keys(b"\x1b[").is_empty(),
            "a cut-off sequence is dropped"
        );
    }

    #[test]
    fn enter_backspace_cancel_and_text_are_keys() {
        assert_eq!(
            parse_keys(b"ab\r\x7f\x03\x04\n"),
            [
                Key::Char('a'),
                Key::Char('b'),
                Key::Enter,
                Key::Backspace,
                Key::Cancel,
                Key::Cancel,
                Key::Enter
            ]
        );
        assert_eq!(
            parse_keys("é字😀".as_bytes()),
            [Key::Char('é'), Key::Char('字'), Key::Char('😀')]
        );
        assert!(
            parse_keys(b"\x01\x02").is_empty(),
            "other control bytes are ignored"
        );
    }

    #[test]
    fn parse_key_says_how_many_bytes_it_used() {
        assert_eq!(parse_key(b"\x1b[Bxyz"), (Some(Key::Down), 3));
        assert_eq!(parse_key(b"xyz"), (Some(Key::Char('x')), 1));
        assert_eq!(parse_key(b"\r\n"), (Some(Key::Enter), 1));
    }

    #[test]
    fn up_and_down_move_and_wrap() {
        let mut menu = MenuState::new(items(&["a", "b", "c"]), 10);
        assert_eq!(menu.highlighted(), Some(0));
        menu.key(Key::Down);
        assert_eq!(menu.highlighted(), Some(1));
        menu.key(Key::Down);
        menu.key(Key::Down);
        assert_eq!(menu.highlighted(), Some(0), "down from the bottom wraps");
        menu.key(Key::Up);
        assert_eq!(menu.highlighted(), Some(2), "up from the top wraps");
        menu.key(Key::Home);
        assert_eq!(menu.highlighted(), Some(0));
        menu.key(Key::End);
        assert_eq!(menu.highlighted(), Some(2));
    }

    #[test]
    fn enter_chooses_the_highlighted_row_and_escape_cancels() {
        let mut menu = MenuState::new(items(&["a", "b"]), 10);
        menu.key(Key::Down);
        assert_eq!(menu.key(Key::Enter), Step::Chosen(1));
        let mut menu = MenuState::new(items(&["a"]), 10);
        assert_eq!(menu.key(Key::Esc), Step::Cancelled);
        assert_eq!(menu.key(Key::Cancel), Step::Cancelled);
    }

    #[test]
    fn typing_filters_and_the_choice_is_an_index_into_the_original_list() {
        let mut menu = MenuState::new(
            vec![
                MenuItem::new("alpha"),
                MenuItem::new("beta").with_detail("deepseek/x"),
                MenuItem::new("gamma"),
                MenuItem::action("+ add new"),
            ],
            10,
        );
        menu.key(Key::Char('D'));
        menu.key(Key::Char('e'));
        assert_eq!(
            menu.visible(),
            [1, 3],
            "matches the detail, case-insensitively, and keeps the action"
        );
        assert_eq!(menu.key(Key::Enter), Step::Chosen(1));
        menu.key(Key::Backspace);
        menu.key(Key::Backspace);
        assert_eq!(menu.visible(), [0, 1, 2, 3]);
    }

    #[test]
    fn escape_clears_a_filter_before_it_cancels() {
        let mut menu = MenuState::new(items(&["alpha", "beta"]), 10);
        menu.key(Key::Char('z'));
        assert!(menu.highlighted().is_none());
        assert_eq!(menu.key(Key::Enter), Step::Continue, "nothing to choose");
        assert_eq!(menu.key(Key::Esc), Step::Continue);
        assert_eq!(menu.filter(), "");
        assert_eq!(menu.highlighted(), Some(0));
        assert_eq!(menu.key(Key::Esc), Step::Cancelled);
    }

    #[test]
    fn a_long_list_scrolls_with_the_cursor() {
        let labels: Vec<String> = (0..10).map(|n| format!("item{n}")).collect();
        let list: Vec<MenuItem> = labels.iter().map(MenuItem::new).collect();
        let mut menu = MenuState::new(list, 3);
        let shown = |menu: &MenuState| menu.render("t", Style::plain());
        assert!(shown(&menu).iter().any(|l| l == "> item0"));
        assert!(!shown(&menu).iter().any(|l| l.contains("item3")));
        for _ in 0..4 {
            menu.key(Key::Down);
        }
        let lines = shown(&menu);
        assert!(lines.iter().any(|l| l == "> item4"), "{lines:?}");
        assert!(
            !lines.iter().any(|l| l.contains("item1")),
            "scrolled past: {lines:?}"
        );
        assert!(lines.last().unwrap().contains("(5 of 10)"), "{lines:?}");
        menu.key(Key::PageDown);
        assert_eq!(menu.highlighted(), Some(6));
        menu.key(Key::PageUp);
        assert_eq!(menu.highlighted(), Some(4));
        menu.key(Key::End);
        assert_eq!(menu.highlighted(), Some(9));
        assert!(shown(&menu).iter().any(|l| l == "> item9"));
    }

    #[test]
    fn the_menu_is_drawn_with_a_title_the_marked_row_and_a_hint() {
        let mut menu = MenuState::new(
            vec![
                MenuItem::new("one").with_detail("first"),
                MenuItem::new("two"),
            ],
            10,
        );
        menu.key(Key::Down);
        let lines = menu.render("pick one", Style::plain());
        assert_eq!(
            lines,
            [
                "pick one",
                "  one  first",
                "> two",
                "up/down move, Enter chooses, Esc goes back, type to search"
            ]
        );
        menu.key(Key::Char('o'));
        let lines = menu.render("pick one", Style::plain());
        assert_eq!(lines[0], "pick one  search: o");
        let fresh = MenuState::new(vec![MenuItem::new("x")], 10);
        assert_eq!(fresh.render("t", Style::colored())[0], "\x1b[1mt\x1b[0m");
    }

    #[test]
    fn rows_are_cut_to_the_width_so_one_row_is_one_line() {
        let long = "x".repeat(100);
        let menu = MenuState::new(
            vec![
                MenuItem::new(&long).with_detail("detail that will not fit"),
                MenuItem::new("short").with_detail("fits"),
            ],
            10,
        )
        .with_width(40);
        let lines = menu.render("t", Style::plain());
        assert_eq!(lines[1].chars().count(), 39, "{:?}", lines[1]);
        assert!(lines[1].ends_with("..."));
        assert!(!lines[1].contains("detail"), "no room left for the detail");
        assert_eq!(lines[2], "  short  fits");
    }
}

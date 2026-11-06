//! Terminal colours: a handful of ANSI styles that switch off by themselves.
//!
//! Colour is only used when the output is a terminal, `NO_COLOR` is not set and `TERM` is not
//! `dumb`. Otherwise every method returns the text unchanged, so a log, a pipe or a test sees
//! plain text. Tests rely on that: the plain output is the reference, and the coloured output
//! must equal it once the colour codes are removed.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    enabled: bool,
}

impl Style {
    /// No colour at all.
    pub const fn plain() -> Self {
        Self { enabled: false }
    }

    /// Always colour (tests use this; the program uses [`Style::detect`]).
    pub const fn colored() -> Self {
        Self { enabled: true }
    }

    /// Colour on a terminal, unless the user turned it off. `NO_COLOR` set to anything but an
    /// empty value turns it off (https://no-color.org), and so does `TERM=dumb`.
    //
    // Rust note: `impl Fn(&str) -> Option<String>` takes any function that looks a variable up,
    // so tests pass a fake environment instead of the real one.
    pub fn detect(is_terminal: bool, env: impl Fn(&str) -> Option<String>) -> Self {
        let no_color = env("NO_COLOR").is_some_and(|value| !value.is_empty());
        let dumb = env("TERM").is_some_and(|value| value == "dumb");
        Self {
            enabled: is_terminal && !no_color && !dumb,
        }
    }

    // Rust note: `self` (not `&self`) is fine here because `Style` is `Copy`: passing it
    // around copies one bool.
    fn paint(self, codes: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{codes}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    pub fn bold(self, text: &str) -> String {
        self.paint("1", text)
    }

    pub fn dim(self, text: &str) -> String {
        self.paint("2", text)
    }

    pub fn red(self, text: &str) -> String {
        self.paint("31", text)
    }

    pub fn green(self, text: &str) -> String {
        self.paint("32", text)
    }

    pub fn yellow(self, text: &str) -> String {
        self.paint("33", text)
    }

    /// Bold cyan: the prompt and the user's own lines.
    pub fn accent(self, text: &str) -> String {
        self.paint("1;36", text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn plain_text_is_returned_unchanged() {
        let s = Style::plain();
        for text in ["", "hello", "[tool] bash {}", "multi\nline"] {
            assert_eq!(s.bold(text), text);
            assert_eq!(s.dim(text), text);
            assert_eq!(s.red(text), text);
            assert_eq!(s.green(text), text);
            assert_eq!(s.yellow(text), text);
            assert_eq!(s.accent(text), text);
        }
    }

    #[test]
    fn coloured_text_is_wrapped_and_reset() {
        let s = Style::colored();
        assert_eq!(s.red("oops"), "\x1b[31moops\x1b[0m");
        assert_eq!(s.accent("> "), "\x1b[1;36m> \x1b[0m");
        assert_eq!(s.dim("x"), "\x1b[2mx\x1b[0m");
    }

    #[test]
    fn colour_needs_a_terminal() {
        assert_eq!(Style::detect(true, env(&[])), Style::colored());
        assert_eq!(Style::detect(false, env(&[])), Style::plain());
    }

    #[test]
    fn no_color_and_a_dumb_terminal_turn_it_off() {
        assert_eq!(
            Style::detect(true, env(&[("NO_COLOR", "1")])),
            Style::plain()
        );
        // An empty NO_COLOR does not count (the convention says "when present and not empty").
        assert_eq!(
            Style::detect(true, env(&[("NO_COLOR", "")])),
            Style::colored()
        );
        assert_eq!(
            Style::detect(true, env(&[("TERM", "dumb")])),
            Style::plain()
        );
        assert_eq!(
            Style::detect(true, env(&[("TERM", "xterm-256color")])),
            Style::colored()
        );
    }
}

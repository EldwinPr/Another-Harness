//! Small helpers the two review screens of the inbox share (issue 47): the web Inbox page
//! (`web_inbox.rs`) and the chat's `/inbox` (`inbox_ui.rs`).
//!
//! Nothing here reads or writes the database. The text of a submission comes from another harness,
//! so what these helpers produce is still to be escaped (the page) or printed through
//! [`printable`] (the terminal).

use crate::store_buffer::{Submission, Suggestion};

/// The longest note the owner can leave on a reject, in characters.
pub const MAX_NOTE_CHARS: usize = 1_000;

/// One line of a [`line_diff`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    /// In both texts.
    Same(String),
    /// Only in the stored text: accepting removes it.
    Removed(String),
    /// Only in the submitted text: accepting adds it.
    Added(String),
}

impl DiffLine {
    /// The line with its marker: two spaces, `- ` or `+ `.
    pub fn marked(&self) -> String {
        match self {
            DiffLine::Same(line) => format!("  {line}"),
            DiffLine::Removed(line) => format!("- {line}"),
            DiffLine::Added(line) => format!("+ {line}"),
        }
    }
}

/// The most cells of the table [`line_diff`] fills; two longer texts are shown as all removed, then
/// all added, after their common start and end are cut off.
const MAX_CELLS: usize = 4_000_000;

/// A plain line diff of `old` (stored) against `new` (submitted): the lines they share stay, the
/// others are marked removed or added. Longest common subsequence of lines; the common start and
/// end are cut off first, so a small change in a long text is cheap.
pub fn line_diff(old: &str, new: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let start = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let end = a[start..]
        .iter()
        .rev()
        .zip(b[start..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (mid_a, mid_b) = (&a[start..a.len() - end], &b[start..b.len() - end]);
    let mut out: Vec<DiffLine> = a[..start]
        .iter()
        .map(|l| DiffLine::Same((*l).to_owned()))
        .collect();
    out.extend(diff_middle(mid_a, mid_b));
    out.extend(
        a[a.len() - end..]
            .iter()
            .map(|l| DiffLine::Same((*l).to_owned())),
    );
    out
}

fn diff_middle(a: &[&str], b: &[&str]) -> Vec<DiffLine> {
    let removed = |l: &&str| DiffLine::Removed((*l).to_owned());
    let added = |l: &&str| DiffLine::Added((*l).to_owned());
    if a.is_empty() || b.is_empty() || (a.len() + 1).saturating_mul(b.len() + 1) > MAX_CELLS {
        return a.iter().map(removed).chain(b.iter().map(added)).collect();
    }
    // lcs[i][j]: the length of the common subsequence of a[i..] and b[j..].
    let width = b.len() + 1;
    let mut lcs = vec![0u32; (a.len() + 1) * width];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i * width + j] = if a[i] == b[j] {
                lcs[(i + 1) * width + j + 1] + 1
            } else {
                lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            out.push(DiffLine::Same(a[i].to_owned()));
            i += 1;
            j += 1;
        } else if lcs[(i + 1) * width + j] >= lcs[i * width + j + 1] {
            out.push(removed(&a[i]));
            i += 1;
        } else {
            out.push(added(&b[j]));
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(removed));
    out.extend(b[j..].iter().map(added));
    out
}

/// `text` with every control character removed except the new line and the tab, for printing text
/// from another harness in a terminal: an escape sequence (colours, moving the cursor, setting the
/// window title) or a carriage return would otherwise act on the screen. The characters that
/// followed the escape stay, as plain text.
pub fn printable(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

/// A confidence for a screen: `0.90`, or `none` when the decider gave none.
pub fn confidence_words(confidence: Option<f64>) -> String {
    confidence.map_or_else(|| "none".to_owned(), |c| format!("{c:.2}"))
}

/// One suggestion in a few words, for a list: what a screen shows and what the decider said.
/// `Accept (said accept, confidence 0.90)`. The text is still to be escaped or made printable.
pub fn suggestion_short(s: &Suggestion) -> String {
    format!(
        "{} (said {}, confidence {})",
        s.shown().label(),
        s.verdict.as_str(),
        confidence_words(s.confidence)
    )
}

/// What to put where an entry has no suggestion.
pub const NO_SUGGESTION: &str = "none";

/// How long ago, in rough words, for a row made at `created` when it is `now` (Unix seconds).
pub fn age(created: i64, now: i64) -> String {
    let secs = now.saturating_sub(created).max(0);
    match secs {
        0..=59 => "just now".to_owned(),
        60..=3_599 => format!("{} min ago", secs / 60),
        3_600..=86_399 => format!("{} h ago", secs / 3_600),
        _ => format!("{} d ago", secs / 86_400),
    }
}

/// Where an entry applies, in words.
pub fn scope_words(project: &str) -> String {
    if project.is_empty() {
        "every project".to_owned()
    } else {
        project.to_owned()
    }
}

/// The note of a reject, trimmed, or why it is not accepted.
pub fn check_note(note: &str) -> Result<String, String> {
    let note = note.trim();
    if note.chars().count() > MAX_NOTE_CHARS {
        return Err(format!("a note is at most {MAX_NOTE_CHARS} characters"));
    }
    Ok(note.to_owned())
}

/// The first `limit` characters of the first line of `text`, with `...` when something was cut.
pub fn first_line(text: &str, limit: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    let mut shown: String = line.chars().take(limit).collect();
    if line.chars().count() > limit || text.trim().lines().count() > 1 {
        shown.push_str("...");
    }
    shown
}

/// The fields of a submission that a screen puts next to the stored ones, as (label, value), for a
/// `set`. A field the submission leaves out is shown as what accepting does with it.
pub fn submitted_fields(sub: &Submission) -> Vec<(&'static str, String)> {
    let keeps = "(not given: keeps the stored one)".to_owned();
    let or_keeps = |value: &str| {
        if value.is_empty() {
            keeps.clone()
        } else {
            value.to_owned()
        }
    };
    let area = sub.area.clone().unwrap_or_else(|| keeps.clone());
    match sub.kind.as_str() {
        "context" => vec![
            ("kind", sub.context_kind.clone()),
            ("area", area),
            ("quote", or_keeps(&sub.quote)),
        ],
        "glossary" => vec![("area", area)],
        _ => vec![(
            "description",
            if sub.description.is_empty() {
                "(none: clears the stored one)".to_owned()
            } else {
                sub.description.clone()
            },
        )],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(diff: &[DiffLine]) -> Vec<String> {
        diff.iter().map(DiffLine::marked).collect()
    }

    #[test]
    fn equal_texts_are_all_shared_lines() {
        assert_eq!(
            lines(&line_diff("a\nb", "a\nb")),
            vec!["  a".to_owned(), "  b".to_owned()]
        );
        assert!(line_diff("", "").is_empty());
    }

    #[test]
    fn a_changed_line_is_a_removal_and_an_addition_between_the_shared_ones() {
        assert_eq!(
            lines(&line_diff("one\ntwo\nthree", "one\n2\nthree")),
            vec!["  one", "- two", "+ 2", "  three"]
        );
    }

    #[test]
    fn lines_added_removed_and_moved_are_found_in_the_middle() {
        assert_eq!(
            lines(&line_diff("a\nb\nc\nd", "a\nc\nx\nd")),
            vec!["  a", "- b", "  c", "+ x", "  d"]
        );
        assert_eq!(lines(&line_diff("a\nb", "b\na")), vec!["- a", "  b", "+ a"]);
    }

    #[test]
    fn nothing_stored_is_all_added_and_a_delete_is_all_removed() {
        assert_eq!(lines(&line_diff("", "x\ny")), vec!["+ x", "+ y"]);
        assert_eq!(lines(&line_diff("x\ny", "")), vec!["- x", "- y"]);
    }

    #[test]
    fn repeated_lines_do_not_confuse_the_diff() {
        assert_eq!(
            lines(&line_diff("a\na\nb", "a\nb\nb")),
            vec!["  a", "- a", "+ b", "  b"]
        );
    }

    #[test]
    fn applying_the_diff_to_the_old_text_gives_the_new_text() {
        let old = "alpha\nbeta\n\ngamma\ndelta\nbeta\n";
        let new = "alpha\ngamma\nepsilon\nbeta\nbeta\n";
        let diff = line_diff(old, new);
        let rebuilt_old: Vec<&str> = diff
            .iter()
            .filter_map(|d| match d {
                DiffLine::Same(l) | DiffLine::Removed(l) => Some(l.as_str()),
                DiffLine::Added(_) => None,
            })
            .collect();
        let rebuilt_new: Vec<&str> = diff
            .iter()
            .filter_map(|d| match d {
                DiffLine::Same(l) | DiffLine::Added(l) => Some(l.as_str()),
                DiffLine::Removed(_) => None,
            })
            .collect();
        assert_eq!(rebuilt_old, old.lines().collect::<Vec<_>>());
        assert_eq!(rebuilt_new, new.lines().collect::<Vec<_>>());
    }

    #[test]
    fn two_huge_unlike_texts_fall_back_to_all_removed_then_all_added() {
        let old: String = (0..2_500).map(|i| format!("old {i}\n")).collect();
        let new: String = (0..2_500).map(|i| format!("new {i}\n")).collect();
        let diff = line_diff(&old, &new);
        assert_eq!(diff.len(), 5_000);
        assert!(
            diff[..2_500]
                .iter()
                .all(|d| matches!(d, DiffLine::Removed(_)))
        );
        assert!(
            diff[2_500..]
                .iter()
                .all(|d| matches!(d, DiffLine::Added(_)))
        );
    }

    #[test]
    fn printable_removes_escape_sequences_and_carriage_returns_but_keeps_lines_and_tabs() {
        assert_eq!(
            printable("a\x1b[31mred\x1b[0m\r\nb\tc\x07\u{9b}d"),
            "a[31mred[0m\nb\tcd"
        );
        assert!(!printable("\x1b]0;title\x07x").contains('\x1b'));
        assert_eq!(printable("plain é日"), "plain é日");
    }

    #[test]
    fn a_suggestion_is_shown_with_what_the_decider_said() {
        let s = |verdict, confidence| Suggestion {
            id: 1,
            buffer_id: 1,
            decider: "llm:p:m".to_owned(),
            verdict,
            confidence,
            reason: "r".to_owned(),
            raw: String::new(),
            created_at: 0,
        };
        use crate::decider::Verdict;
        assert_eq!(
            suggestion_short(&s(Verdict::Accept, Some(0.9))),
            "Accept (said accept, confidence 0.90)"
        );
        assert_eq!(
            suggestion_short(&s(Verdict::Accept, Some(0.7))),
            "Unsure (said accept, confidence 0.70)"
        );
        assert_eq!(
            suggestion_short(&s(Verdict::Reject, None)),
            "Unsure (said reject, confidence none)"
        );
    }

    #[test]
    fn ages_are_rough_words() {
        assert_eq!(age(100, 100), "just now");
        assert_eq!(age(100, 40), "just now");
        assert_eq!(age(0, 59), "just now");
        assert_eq!(age(0, 120), "2 min ago");
        assert_eq!(age(0, 3 * 3_600 + 5), "3 h ago");
        assert_eq!(age(0, 2 * 86_400), "2 d ago");
    }

    #[test]
    fn a_note_is_trimmed_and_bounded() {
        assert_eq!(check_note("  no  ").unwrap(), "no");
        assert_eq!(check_note("").unwrap(), "");
        assert!(check_note(&"x".repeat(MAX_NOTE_CHARS)).is_ok());
        assert!(check_note(&"x".repeat(MAX_NOTE_CHARS + 1)).is_err());
    }
}

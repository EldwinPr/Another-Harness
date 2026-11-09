//! The model writes Markdown; a terminal shows it as plain text. This turns the parts that are
//! easy to get right into colour: `**bold**`, `` `code` ``, `#` headings and fenced code blocks.
//!
//! Without colour (a pipe, `NO_COLOR`, a test) the text is returned exactly as it came, so a log
//! and the plain reference output stay the same. Lists, links, tables and `*italic*` are left
//! alone: guessing at them wrongly is worse than showing them as written.

use crate::style::Style;

/// The text with Markdown marks shown as bold and colour. Unchanged when colour is off. Tables are
/// drawn to fit a line of `width` columns.
pub fn render(style: Style, text: &str, width: usize) -> String {
    if !style.is_enabled() {
        return text.to_owned();
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<String> = Vec::new();
    let mut in_fence = false;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            out.push(style.dim(line));
        } else if in_fence {
            out.push(style.cyan(line));
        } else if let Some(count) = table_len(&lines[i..]) {
            out.extend(table(style, &lines[i..i + count], width));
            i += count;
            continue;
        } else if let Some(heading) = heading(trimmed) {
            out.push(style.bold(&inline(style, heading)));
        } else if is_rule(trimmed) {
            out.push(style.dim(&"─".repeat(width.clamp(8, 60))));
        } else if let Some(quoted) = trimmed
            .strip_prefix("> ")
            .or_else(|| trimmed.strip_prefix('>'))
        {
            out.push(format!(
                "{} {}",
                style.dim("│"),
                inline(style, quoted.trim_start())
            ));
        } else if let Some((indent, rest)) = bullet(line) {
            out.push(format!("{indent}• {}", inline(style, rest)));
        } else {
            out.push(inline(style, line));
        }
        i += 1;
    }
    out.join("\n")
}

/// A line of three or more `-`, `*` or `_` alone: a horizontal rule.
fn is_rule(line: &str) -> bool {
    let t: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    t.len() >= 3
        && ["-", "*", "_"]
            .iter()
            .any(|m| t.chars().all(|c| c.to_string() == *m))
}

/// `- item` or `* item` (indented or not): the indent and the item.
fn bullet(line: &str) -> Option<(&str, &str)> {
    let body = line.trim_start();
    let indent = &line[..line.len() - body.len()];
    let rest = body
        .strip_prefix("- ")
        .or_else(|| body.strip_prefix("* "))?;
    (!rest.trim().is_empty() && !is_rule(body)).then_some((indent, rest))
}

// ---- tables ----

/// The cells of a table row, without the outer pipes. A `\|` and a `|` inside backticks stay.
fn cells(line: &str) -> Vec<String> {
    let line = line.trim();
    let line = line.strip_prefix('|').unwrap_or(line);
    let line = line.strip_suffix('|').unwrap_or(line);
    let mut cells = vec![String::new()];
    let mut in_code = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '`' => {
                in_code = !in_code;
                cells.last_mut().expect("never empty").push(c);
            }
            '\\' if chars.peek() == Some(&'|') => {
                chars.next();
                cells.last_mut().expect("never empty").push('|');
            }
            '|' if !in_code => cells.push(String::new()),
            _ => cells.last_mut().expect("never empty").push(c),
        }
    }
    cells.into_iter().map(|c| c.trim().to_owned()).collect()
}

/// The alignment a separator cell asks for: `:--` left, `:-:` centre, `--:` right.
fn alignment(cell: &str) -> Option<char> {
    let c = cell.trim();
    let core = c.trim_matches(':');
    if core.is_empty() || !core.chars().all(|ch| ch == '-') {
        return None;
    }
    Some(match (c.starts_with(':'), c.ends_with(':')) {
        (true, true) => 'c',
        (false, true) => 'r',
        _ => 'l',
    })
}

/// How many lines from the start make a table (a header, a separator of the same width, and the
/// rows after them that have a `|`), or `None` when these lines are not a table.
fn table_len(lines: &[&str]) -> Option<usize> {
    if lines.len() < 2 || !lines[0].contains('|') {
        return None;
    }
    let header = cells(lines[0]);
    let separator = cells(lines[1]);
    if header.len() < 2
        || separator.len() != header.len()
        || !separator.iter().all(|c| alignment(c).is_some())
    {
        return None;
    }
    let rows = lines[2..]
        .iter()
        .take_while(|l| l.contains('|') && !l.trim().is_empty())
        .count();
    Some(2 + rows)
}

/// What the cell shows: its text with the marks taken off.
fn shown(cell: &str) -> String {
    inline(Style::plain(), cell)
}

/// Words of `text` in lines of at most `width` characters (a longer word is cut).
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let mut word = word.to_owned();
        while word.chars().count() > width {
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            let head: String = word.chars().take(width).collect();
            lines.push(head);
            word = word.chars().skip(width).collect();
        }
        let joined = if line.is_empty() {
            0
        } else {
            line.chars().count() + 1
        };
        if joined + word.chars().count() > width && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&word);
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// A table drawn with lines, columns lined up, the header bold, and cells that do not fit
/// wrapped (then without their bold and code colour).
fn table(style: Style, lines: &[&str], width: usize) -> Vec<String> {
    let header = cells(lines[0]);
    let align: Vec<char> = cells(lines[1])
        .iter()
        .map(|c| alignment(c).unwrap_or('l'))
        .collect();
    let columns = header.len();
    let mut rows: Vec<Vec<String>> = vec![header];
    for line in &lines[2..] {
        let mut row = cells(line);
        row.resize(columns, String::new());
        rows.push(row);
    }
    let natural: Vec<usize> = (0..columns)
        .map(|c| {
            rows.iter()
                .map(|r| shown(&r[c]).chars().count())
                .max()
                .unwrap_or(1)
                .max(1)
        })
        .collect();
    // Room for the cells: the line less 3 per column (a bar and two spaces) and the last bar.
    let room = width.saturating_sub(columns * 3 + 1).max(columns * 6);
    let mut widths = natural.clone();
    while widths.iter().sum::<usize>() > room {
        let widest = (0..columns).max_by_key(|c| widths[*c]).unwrap_or(0);
        if widths[widest] <= 6 {
            break;
        }
        widths[widest] -= 1;
    }
    let bar = |left: &str, mid: &str, right: &str| {
        style.dim(&format!(
            "{left}{}{right}",
            widths
                .iter()
                .map(|w| "─".repeat(w + 2))
                .collect::<Vec<_>>()
                .join(mid)
        ))
    };
    let mut out = vec![bar("┌", "┬", "┐")];
    for (r, row) in rows.iter().enumerate() {
        // Each cell as its lines, styled when it fits on one.
        let cell_lines: Vec<Vec<(String, usize)>> = row
            .iter()
            .enumerate()
            .map(|(c, cell)| {
                let plain = shown(cell);
                let count = plain.chars().count();
                if count <= widths[c] {
                    let text = if r == 0 {
                        style.bold(&plain)
                    } else {
                        inline(style, cell)
                    };
                    vec![(text, count)]
                } else {
                    wrap(&plain, widths[c])
                        .into_iter()
                        .map(|l| {
                            let n = l.chars().count();
                            (if r == 0 { style.bold(&l) } else { l }, n)
                        })
                        .collect()
                }
            })
            .collect();
        let height = cell_lines.iter().map(Vec::len).max().unwrap_or(1);
        for k in 0..height {
            let mut line = style.dim("│");
            for (c, cell) in cell_lines.iter().enumerate() {
                let (text, count) = cell.get(k).cloned().unwrap_or_default();
                let gap = widths[c].saturating_sub(count);
                let (left, right) = match align[c] {
                    'r' => (gap, 0),
                    'c' => (gap / 2, gap - gap / 2),
                    _ => (0, gap),
                };
                line.push_str(&format!(
                    " {}{text}{} {}",
                    " ".repeat(left),
                    " ".repeat(right),
                    style.dim("│")
                ));
            }
            out.push(line);
        }
        if r == 0 {
            out.push(bar("├", "┼", "┤"));
        }
    }
    out.push(bar("└", "┴", "┘"));
    out
}

/// The words of a `#`..`######` heading, if the line is one.
fn heading(line: &str) -> Option<&str> {
    let marks = line.chars().take_while(|c| *c == '#').count();
    let rest = line[marks..].strip_prefix(' ')?;
    ((1..=6).contains(&marks) && !rest.trim().is_empty()).then_some(rest.trim())
}

/// One line with its `**bold**`, `*italic*`, `` `code` `` and `[text](url)` marks turned into
/// styles. A mark with no closing partner on the line stays as typed.
fn inline(style: Style, line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(at) = rest.find(['`', '*', '[']) {
        let (before, marked) = rest.split_at(at);
        out.push_str(before);
        if let Some(after) = marked.strip_prefix('[') {
            match link(marked) {
                Some((text, url, used)) => {
                    out.push_str(&style.underline(text));
                    if url != text {
                        out.push_str(&style.dim(&format!(" ({url})")));
                    }
                    rest = &marked[used..];
                }
                None => {
                    out.push('[');
                    rest = after;
                }
            }
            continue;
        }
        let (open, paint): (&str, fn(Style, &str) -> String) = if marked.starts_with('`') {
            ("`", Style::cyan)
        } else if marked.starts_with("**") {
            ("**", |s, t| s.bold(&inline_code_only(s, t)))
        } else {
            // A single star is italics only when it opens a word and closes one: `*.rs` and
            // `2 * 3` are left alone.
            let after = &marked[1..];
            let opens = after.chars().next().is_some_and(char::is_alphanumeric)
                && !out.chars().last().is_some_and(char::is_alphanumeric);
            let end = after.find('*').filter(|end| {
                *end > 0
                    && after[..*end]
                        .chars()
                        .last()
                        .is_some_and(|c| !c.is_whitespace())
                    && !after[*end + 1..].starts_with('*')
            });
            match (opens, end) {
                (true, Some(end)) => {
                    out.push_str(&style.italic(&after[..end]));
                    rest = &after[end + 1..];
                }
                _ => {
                    out.push('*');
                    rest = after;
                }
            }
            continue;
        };
        let body = &marked[open.len()..];
        match body.find(open).filter(|end| *end > 0) {
            Some(end) => {
                out.push_str(&paint(style, &body[..end]));
                rest = &body[end + open.len()..];
            }
            None => {
                out.push_str(open);
                rest = body;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `[text](url)` at the start of `s`: the text, the url and how many bytes it takes.
fn link(s: &str) -> Option<(&str, &str, usize)> {
    let close = s.find("](")?;
    let text = &s[1..close];
    if text.is_empty() || text.contains('[') {
        return None;
    }
    let after = &s[close + 2..];
    let end = after.find(')')?;
    let url = &after[..end];
    (!url.is_empty() && !url.contains(char::is_whitespace)).then_some((
        text,
        url,
        close + 2 + end + 1,
    ))
}

/// Code inside bold text keeps its colour; an odd number of ticks leaves them as typed.
fn inline_code_only(style: Style, text: &str) -> String {
    let parts: Vec<&str> = text.split('`').collect();
    if parts.len().is_multiple_of(2) {
        return text.to_owned();
    }
    parts
        .iter()
        .enumerate()
        .map(|(i, part)| {
            if i % 2 == 1 {
                style.cyan(part)
            } else {
                (*part).to_owned()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the screen would show: every colour code removed.
    fn bare(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for e in chars.by_ref() {
                    if e == 'm' {
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
    fn without_colour_the_text_is_returned_as_written() {
        let text = "# Title\n**bold** and `code`\n```rust\nlet x = 1;\n```";
        assert_eq!(render(Style::plain(), text, 80), text);
    }

    #[test]
    fn bold_and_code_lose_their_marks_and_gain_a_style() {
        let style = Style::colored();
        let shown = render(style, "use **this** and `that` now", 80);
        assert_eq!(bare(&shown), "use this and that now");
        assert!(shown.contains(&style.bold("this")), "{shown:?}");
        assert!(shown.contains(&style.cyan("that")), "{shown:?}");
    }

    #[test]
    fn a_mark_with_no_partner_or_inside_code_stays_as_typed() {
        let style = Style::colored();
        for line in [
            "2 * 3 = 6",
            "a ** b",
            "an `open tick",
            "rm -rf *.o",
            "**",
            "``",
        ] {
            assert_eq!(bare(&render(style, line, 80)), line, "{line}");
        }
        let shown = render(style, "run `echo **hi**` now", 80);
        assert_eq!(bare(&shown), "run echo **hi** now");
        assert!(shown.contains(&style.cyan("echo **hi**")), "{shown:?}");
    }

    #[test]
    fn code_inside_bold_keeps_its_colour() {
        let style = Style::colored();
        let shown = render(style, "**call `run` first**", 80);
        assert_eq!(bare(&shown), "call run first");
        assert!(shown.contains(&style.cyan("run")), "{shown:?}");
    }

    #[test]
    fn headings_are_bold_and_a_hash_without_a_space_is_not_one() {
        let style = Style::colored();
        let shown = render(style, "## Plan\n#tag\n####### seven", 80);
        let lines: Vec<&str> = shown.split('\n').collect();
        assert_eq!(lines[0], style.bold("Plan"));
        assert_eq!(bare(lines[1]), "#tag");
        assert_eq!(bare(lines[2]), "####### seven");
    }

    #[test]
    fn a_fenced_block_is_shown_whole_with_its_marks_dimmed_and_nothing_inside_changed() {
        let style = Style::colored();
        let shown = render(style, "before\n```sh\necho **a** `b`\n```\nafter **x**", 80);
        assert_eq!(bare(&shown), "before\n```sh\necho **a** `b`\n```\nafter x");
        assert!(shown.contains(&style.cyan("echo **a** `b`")), "{shown:?}");
    }

    #[test]
    fn a_table_is_drawn_with_lines_aligned_and_marks_in_its_cells_removed() {
        let style = Style::colored();
        let text =
            "Intro\n| Name | Size |\n|:-----|-----:|\n| **a** | 10 |\n| `bb` | 2000 |\nAfter";
        let rows: Vec<String> = bare(&render(style, text, 80))
            .split('\n')
            .map(str::to_owned)
            .collect();
        assert_eq!(
            rows,
            [
                "Intro",
                "┌──────┬──────┐",
                "│ Name │ Size │",
                "├──────┼──────┤",
                "│ a    │   10 │",
                "│ bb   │ 2000 │",
                "└──────┴──────┘",
                "After"
            ]
        );
        let shown = render(style, text, 80);
        assert!(shown.contains(&style.bold("Name")), "{shown:?}");
        assert!(shown.contains(&style.bold("a")), "{shown:?}");
        // Without colour the table stays as written.
        assert_eq!(render(Style::plain(), text, 80), text);
    }

    #[test]
    fn a_table_wider_than_the_line_wraps_its_cells_and_a_pipe_in_code_is_kept() {
        let style = Style::colored();
        let text = "| Item | Why |\n|---|---|\n| `a|b` | this reason is far too long to fit on one line of the screen |";
        let shown = bare(&render(style, text, 40));
        for line in shown.lines() {
            assert!(line.chars().count() <= 40, "{line:?} in {shown}");
        }
        assert!(shown.contains("a|b"), "{shown}");
        assert!(
            shown.lines().count() > 5,
            "the long cell took more rows: {shown}"
        );
        // A pipe line with no separator under it is not a table.
        let loose = "a | b\nc | d";
        assert_eq!(bare(&render(style, loose, 40)), loose);
    }

    #[test]
    fn lists_quotes_rules_links_and_italics_read_as_they_should() {
        let style = Style::colored();
        let shown = bare(&render(
            style,
            "- one\n  - two\n* three\n> quoted *word*\n---\nsee [docs](https://x.y/z) and *this* not *.rs or 2 * 3",
            40,
        ));
        let lines: Vec<&str> = shown.lines().collect();
        assert_eq!(lines[0], "• one");
        assert_eq!(lines[1], "  • two");
        assert_eq!(lines[2], "• three");
        assert_eq!(lines[3], "│ quoted word");
        assert!(lines[4].starts_with("────"), "{shown}");
        assert_eq!(
            lines[5],
            "see docs (https://x.y/z) and this not *.rs or 2 * 3"
        );
        let italics = render(style, "a *b* c", 40);
        assert!(italics.contains(&style.italic("b")), "{italics:?}");
    }
}

//! Token numbers for the info line under the input box (issue 17).
//!
//! The router reports `usage` with every reply; the program keeps the newest call's numbers and
//! the totals for the chat. It does not know a model's context window, so it never shows a
//! percentage: "context" is simply what the newest request cost to send.

use crate::cache_check::parse_usage;

/// The numbers one call reported. `None` when the router did not say.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tokens {
    pub prompt: Option<u64>,
    pub cached: Option<u64>,
    pub completion: Option<u64>,
}

impl Tokens {
    /// Reads them from the raw `usage` object of a reply.
    pub fn from_usage(raw: &str) -> Self {
        let (prompt, cached, _written) = parse_usage(raw);
        let completion = serde_json::from_str::<serde_json::Value>(raw)
            .ok()
            .and_then(|v| v["completion_tokens"].as_u64());
        Self {
            prompt,
            cached,
            completion,
        }
    }
}

/// What the chat in use has cost so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChatUsage {
    /// The newest call that was answered.
    pub last: Option<Tokens>,
    pub prompt_total: u64,
    pub completion_total: u64,
}

impl ChatUsage {
    pub fn add(&mut self, tokens: Tokens) {
        self.prompt_total += tokens.prompt.unwrap_or(0);
        self.completion_total += tokens.completion.unwrap_or(0);
        self.last = Some(tokens);
    }

    /// Adds what a subagent used (issue 33) to the totals. `last` is the size of this chat's own
    /// newest request, so it stays as it was.
    pub fn add_subagent(&mut self, prompt: u64, completion: u64) {
        self.prompt_total += prompt;
        self.completion_total += completion;
    }
}

/// `1234567` as `1,234,567`.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn count(n: Option<u64>) -> String {
    n.map_or_else(|| "?".to_owned(), thousands)
}

/// Everything the info line shows.
pub struct Info<'a> {
    /// `None` when no provider is chosen yet.
    pub model: Option<&'a str>,
    pub provider: &'a str,
    pub approve: &'a str,
    /// How many "always" permissions were given in this run.
    pub always: usize,
    /// How many background subagents are running (issue 35); shown only when above zero.
    pub subagents: usize,
    /// How many background commands are running (issue 42); shown only when above zero.
    pub shells: usize,
    pub usage: &'a ChatUsage,
}

/// The one-line summary. With `width`, the longest of three forms that fits is used (full,
/// shorter labels, then only the numbers); if even that is too long it is cut and ends in `…`.
pub fn info_line(info: &Info, width: Option<usize>) -> String {
    let who = match info.model {
        Some(model) => format!("{model} · {}", info.provider),
        None => "no provider".to_owned(),
    };
    let usage = info.usage;
    let last_tokens = usage.last.map(|t| {
        let cached = t.cached.map(thousands);
        (count(t.prompt), cached, count(t.completion))
    });
    let context = usage.last.and_then(|t| t.prompt).map(thousands);

    // Full: every label spelled out.
    let full = [
        who.clone(),
        if info.always > 0 {
            format!("approve: {} (+{} always)", info.approve, info.always)
        } else {
            format!("approve: {}", info.approve)
        },
        match &last_tokens {
            None => "last call: none yet".to_owned(),
            Some((prompt, cached, completion)) => {
                let cached = cached
                    .as_ref()
                    .map_or(String::new(), |c| format!(" ({c} cached)"));
                format!("last call: {prompt} in{cached} · {completion} out")
            }
        },
        format!(
            "chat: {} in · {} out",
            thousands(usage.prompt_total),
            thousands(usage.completion_total)
        ),
        match &context {
            Some(c) => format!("context ≈ {c}"),
            None => "context: not sent yet".to_owned(),
        },
    ]
    .into_iter()
    .chain((info.subagents > 0).then(|| format!("subagents: {} running", info.subagents)))
    .chain((info.shells > 0).then(|| format!("commands: {} running", info.shells)))
    .collect::<Vec<_>>()
    .join(" │ ");

    // Short: the same facts with short labels and no cached count.
    let short_numbers = [
        if info.always > 0 {
            format!("{} (+{})", info.approve, info.always)
        } else {
            info.approve.to_owned()
        },
        match &last_tokens {
            None => "last: none".to_owned(),
            Some((prompt, _, completion)) => format!("last {prompt} in · {completion} out"),
        },
        format!(
            "chat {}/{}",
            thousands(usage.prompt_total),
            thousands(usage.completion_total)
        ),
        match &context {
            Some(c) => format!("ctx {c}"),
            None => "ctx -".to_owned(),
        },
    ];
    let short = std::iter::once(who)
        .chain(short_numbers.iter().cloned())
        .collect::<Vec<_>>()
        .join(" │ ");
    let numbers = short_numbers.join(" │ ");

    let Some(width) = width else {
        return full;
    };
    for form in [&full, &short, &numbers] {
        if form.chars().count() <= width {
            return form.clone();
        }
    }
    let mut cut: String = numbers.chars().take(width.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_get_thousands_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1234567), "1,234,567");
    }

    #[test]
    fn tokens_are_read_from_a_usage_object_in_either_spelling() {
        let t = Tokens::from_usage(
            r#"{"prompt_tokens":2770,"completion_tokens":8,"prompt_tokens_details":{"cached_tokens":512}}"#,
        );
        assert_eq!(
            t,
            Tokens {
                prompt: Some(2770),
                cached: Some(512),
                completion: Some(8)
            }
        );
        let t = Tokens::from_usage(r#"{"prompt_tokens":5,"cached_tokens":3}"#);
        assert_eq!((t.prompt, t.cached, t.completion), (Some(5), Some(3), None));
        assert_eq!(Tokens::from_usage("not json"), Tokens::default());
    }

    #[test]
    fn totals_add_up_and_a_missing_number_counts_as_nothing() {
        let mut usage = ChatUsage::default();
        usage.add(Tokens {
            prompt: Some(100),
            cached: Some(0),
            completion: Some(10),
        });
        usage.add(Tokens {
            prompt: Some(150),
            cached: None,
            completion: None,
        });
        assert_eq!((usage.prompt_total, usage.completion_total), (250, 10));
        assert_eq!(usage.last.unwrap().prompt, Some(150));
    }

    fn info<'a>(usage: &'a ChatUsage, always: usize) -> Info<'a> {
        Info {
            model: Some("m"),
            provider: "prov",
            approve: "manual",
            always,
            subagents: 0,
            shells: 0,
            usage,
        }
    }

    #[test]
    fn the_line_before_any_call() {
        let usage = ChatUsage::default();
        assert_eq!(
            info_line(&info(&usage, 0), None),
            "m · prov │ approve: manual │ last call: none yet │ chat: 0 in · 0 out │ context: not sent yet"
        );
    }

    #[test]
    fn the_line_after_calls_with_and_without_cached_and_missing_numbers() {
        let mut usage = ChatUsage::default();
        usage.add(Tokens {
            prompt: Some(1234),
            cached: Some(512),
            completion: Some(56),
        });
        assert_eq!(
            info_line(&info(&usage, 2), None),
            "m · prov │ approve: manual (+2 always) │ last call: 1,234 in (512 cached) · 56 out │ chat: 1,234 in · 56 out │ context ≈ 1,234"
        );
        usage.add(Tokens::default());
        let line = info_line(&info(&usage, 0), None);
        assert!(line.contains("last call: ? in · ? out"), "{line}");
        assert!(line.contains("context: not sent yet"), "{line}");
        assert!(line.contains("chat: 1,234 in · 56 out"), "{line}");
    }

    #[test]
    fn no_provider_is_said_and_a_narrow_width_cuts_with_an_ellipsis() {
        let usage = ChatUsage::default();
        let mut i = info(&usage, 0);
        i.model = None;
        assert!(info_line(&i, None).starts_with("no provider │ approve: manual"));
        let cut = info_line(&i, Some(20));
        assert_eq!(cut.chars().count(), 20);
        assert!(cut.ends_with('…'));
        assert_eq!(info_line(&i, Some(500)), info_line(&i, None));
    }

    #[test]
    fn a_narrower_terminal_gets_shorter_labels_and_then_only_the_numbers() {
        let mut usage = ChatUsage::default();
        usage.add(Tokens {
            prompt: Some(1200),
            cached: Some(1000),
            completion: Some(30),
        });
        let i = info(&usage, 1);
        let full = info_line(&i, None);
        assert_eq!(info_line(&i, Some(full.chars().count())), full);
        let short = info_line(&i, Some(full.chars().count() - 1));
        assert_eq!(
            short,
            "m · prov │ manual (+1) │ last 1,200 in · 30 out │ chat 1,200/30 │ ctx 1,200"
        );
        let numbers = info_line(&i, Some(short.chars().count() - 1));
        assert_eq!(
            numbers,
            "manual (+1) │ last 1,200 in · 30 out │ chat 1,200/30 │ ctx 1,200"
        );
        let fresh = ChatUsage::default();
        assert_eq!(
            info_line(&info(&fresh, 0), Some(60)),
            "m · prov │ manual │ last: none │ chat 0/0 │ ctx -"
        );
    }
    #[test]
    fn running_subagents_are_counted_only_when_there_are_some() {
        let usage = ChatUsage::default();
        let none = info_line(&info(&usage, 0), None);
        assert!(!none.contains("subagent"), "{none}");
        let mut running = info(&usage, 0);
        running.subagents = 2;
        let full = info_line(&running, None);
        assert!(full.ends_with(" │ subagents: 2 running"), "{full}");
    }
}

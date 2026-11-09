//! Stops a model that keeps making the same mistake.
//!
//! A tool call is *bad* when it failed or when it repeats the call right before it exactly (same
//! tool, same arguments). Bad calls in a row are counted per user message: at [`WARN_AT`] the
//! result the model gets back carries a note telling it to stop and ask the user; at [`STOP_AT`]
//! the program ends the message itself. A call that is neither failed nor a repeat starts the count
//! again. The rule looks at nothing but the calls and their results, so it is the same every time.

use kernel::ToolCall;

/// Bad calls in a row after which the model is told to stop and ask.
pub const WARN_AT: u32 = 3;
/// Bad calls in a row after which the message is ended.
pub const STOP_AT: u32 = 5;

/// What the guard decides after a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Fine,
    /// Add the note to this call's result.
    Warn,
    /// Finish the calls of this reply, then end the message.
    Stop,
}

/// The note added to a result at [`WARN_AT`] and after.
pub const NOTE: &str = "[stopped by the program: the last calls in a row failed or repeated an earlier call exactly. Do not try again or work around it. Tell the user what went wrong and ask what to do.]";

#[derive(Debug, Default)]
pub struct LoopGuard {
    last: Option<(String, String)>,
    bad: u32,
}

impl LoopGuard {
    /// Counts one finished call. `failed` says whether its result was a failure.
    pub fn record(&mut self, call: &ToolCall, failed: bool) -> Verdict {
        let this = (call.name.clone(), call.arguments.clone());
        let repeated = self.last.as_ref() == Some(&this);
        self.last = Some(this);
        if failed || repeated {
            self.bad += 1;
        } else {
            self.bad = 0;
        }
        match self.bad {
            n if n >= STOP_AT => Verdict::Stop,
            n if n >= WARN_AT => Verdict::Warn,
            _ => Verdict::Fine,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: "c".to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
        }
    }

    #[test]
    fn three_failures_in_a_row_warn_and_five_stop() {
        let mut guard = LoopGuard::default();
        let verdicts: Vec<Verdict> = (0..5)
            .map(|i| guard.record(&call("bash", &format!("{{\"n\":{i}}}")), true))
            .collect();
        assert_eq!(
            verdicts,
            [
                Verdict::Fine,
                Verdict::Fine,
                Verdict::Warn,
                Verdict::Warn,
                Verdict::Stop
            ]
        );
    }

    #[test]
    fn the_same_call_over_and_over_counts_even_when_it_succeeds() {
        let mut guard = LoopGuard::default();
        let same = call("read_file", "{\"path\":\"a\"}");
        let verdicts: Vec<Verdict> = (0..4).map(|_| guard.record(&same, false)).collect();
        // The first is a fresh call; the next three repeat it.
        assert_eq!(
            verdicts,
            [Verdict::Fine, Verdict::Fine, Verdict::Fine, Verdict::Warn]
        );
    }

    #[test]
    fn one_good_different_call_starts_the_count_again() {
        let mut guard = LoopGuard::default();
        for i in 0..2 {
            guard.record(&call("bash", &format!("{{\"n\":{i}}}")), true);
        }
        assert_eq!(guard.record(&call("ls", "{}"), false), Verdict::Fine);
        assert_eq!(
            guard.record(&call("bash", "{\"n\":9}"), true),
            Verdict::Fine
        );
        assert_eq!(
            guard.record(&call("bash", "{\"n\":8}"), true),
            Verdict::Fine
        );
        assert_eq!(
            guard.record(&call("bash", "{\"n\":7}"), true),
            Verdict::Warn
        );
    }
}

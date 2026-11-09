//! `steps` (issue 42): the model's own checklist for work with several stages. The tool keeps
//! nothing: each call carries the whole list, the terminal shows the list (and, while it is
//! unfinished, hides the individual tool calls), and the answer tells the model how far it is.

use kernel::ToolSpec;
use serde::Deserialize;

/// The most steps a list may have, and the longest a step may be.
pub const MAX_STEPS: usize = 12;
pub const MAX_STEP_CHARS: usize = 100;

const SCHEMA: &str = r#"{"type":"object","properties":{"steps":{"type":"array","items":{"type":"object","properties":{"text":{"type":"string"},"status":{"type":"string","enum":["todo","doing","done"]}},"required":["text","status"]}}},"required":["steps"]}"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus {
    Todo,
    Doing,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Step {
    pub text: String,
    pub status: StepStatus,
}

#[derive(Deserialize)]
struct Args {
    steps: Vec<Step>,
}

/// The list in the arguments of a `steps` call, or what is wrong with it.
pub fn parse_steps(arguments: &str) -> Result<Vec<Step>, String> {
    let args: Args =
        serde_json::from_str(arguments).map_err(|e| format!("error: invalid arguments: {e}"))?;
    let steps: Vec<Step> = args
        .steps
        .into_iter()
        .map(|s| Step {
            text: s.text.split_whitespace().collect::<Vec<_>>().join(" "),
            status: s.status,
        })
        .collect();
    if steps.is_empty() {
        return Err("error: give at least one step".to_owned());
    }
    if steps.len() > MAX_STEPS {
        return Err(format!(
            "error: at most {MAX_STEPS} steps; make them coarser"
        ));
    }
    if let Some(bad) = steps
        .iter()
        .find(|s| s.text.is_empty() || s.text.chars().count() > MAX_STEP_CHARS)
    {
        return Err(format!(
            "error: a step is 1 to {MAX_STEP_CHARS} characters, not \"{}\"",
            bad.text.chars().take(30).collect::<String>()
        ));
    }
    Ok(steps)
}

/// Whether work is still to do: some step is not done.
pub fn unfinished(steps: &[Step]) -> bool {
    steps.iter().any(|s| s.status != StepStatus::Done)
}

pub struct StepsTool {
    spec: ToolSpec,
}

impl Default for StepsTool {
    fn default() -> Self {
        Self::new()
    }
}

impl StepsTool {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                "steps",
                "Set the checklist for work with three or more stages: a short list of steps, each todo, doing or done. Send the whole list each time. The user sees the list instead of each call.",
                SCHEMA,
            )
            .expect("the schema constant is valid JSON"),
        }
    }
}

impl super::Tool for StepsTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn run(&self, arguments: &str) -> String {
        match parse_steps(arguments) {
            Err(why) => why,
            Ok(steps) => {
                let done = steps
                    .iter()
                    .filter(|s| s.status == StepStatus::Done)
                    .count();
                let doing = steps
                    .iter()
                    .filter(|s| s.status == StepStatus::Doing)
                    .count();
                if doing > 1 {
                    return "error: only one step is doing at a time".to_owned();
                }
                format!("steps: {done} of {} done", steps.len())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;

    fn call(args: serde_json::Value) -> String {
        StepsTool::new().run(&args.to_string())
    }

    #[test]
    fn a_list_is_counted_and_a_bad_one_is_refused_with_a_reason() {
        let ok = serde_json::json!({"steps": [
            {"text": "read", "status": "done"},
            {"text": "write", "status": "doing"},
            {"text": "test", "status": "todo"}]});
        assert_eq!(call(ok), "steps: 1 of 3 done");
        assert!(call(serde_json::json!({"steps": []})).contains("at least one"));
        let many: Vec<_> = (0..13)
            .map(|i| serde_json::json!({"text": format!("s{i}"), "status": "todo"}))
            .collect();
        assert!(call(serde_json::json!({"steps": many})).contains("at most 12"));
        assert!(
            call(serde_json::json!({"steps": [{"text": " ", "status": "todo"}]}))
                .contains("1 to 100 characters")
        );
        assert!(
            call(serde_json::json!({"steps": [{"text": "a", "status": "later"}]}))
                .starts_with("error: invalid arguments")
        );
        let two = serde_json::json!({"steps": [
            {"text": "a", "status": "doing"}, {"text": "b", "status": "doing"}]});
        assert!(call(two).contains("only one step is doing"));
    }

    #[test]
    fn unfinished_means_some_step_is_not_done() {
        let step = |status| Step {
            text: "x".to_owned(),
            status,
        };
        assert!(unfinished(&[
            step(StepStatus::Done),
            step(StepStatus::Todo)
        ]));
        assert!(!unfinished(&[step(StepStatus::Done)]));
    }
}

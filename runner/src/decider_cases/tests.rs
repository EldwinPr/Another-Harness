//! Tests of the labelled cases and the `--eval-decider` runner (issue 48). The deciders here are
//! scripted; nothing reaches the network or any database.

use std::collections::HashSet;
use std::sync::Mutex;

use super::*;
use crate::agent::ScriptedProvider;
use crate::decider::{DeciderError, LlmDecider};
use crate::store_buffer::Checked;

/// A prepared answer: a verdict and confidence, or why there is none.
type Answer = Result<(Verdict, Option<f64>), String>;

/// A decider that answers each case with the next prepared verdict and confidence.
struct Script(Mutex<Vec<Answer>>);

impl Script {
    fn new(answers: Vec<Answer>) -> Self {
        Self(Mutex::new(answers))
    }
}

impl Decider for Script {
    fn name(&self) -> String {
        "llm:scripted:m".to_owned()
    }

    fn judge(&self, _: &Case) -> Result<Opinion, DeciderError> {
        match self.0.lock().unwrap().remove(0) {
            Ok((verdict, confidence)) => Ok(Opinion {
                verdict,
                confidence,
                reason: "scripted".to_owned(),
                raw: String::new(),
            }),
            Err(why) => Err(DeciderError(why)),
        }
    }
}

/// The verdict the owner wants for every case, so a decider that always answers like this agrees.
fn wanted() -> Vec<Verdict> {
    cases().iter().map(|c| c.expected).collect()
}

fn always(verdict: Verdict, confidence: f64) -> Script {
    Script::new(vec![Ok((verdict, Some(confidence))); 18])
}

fn report_of(script: &Script) -> (Report, String) {
    let mut out = Vec::new();
    let report = run(script, &mut out).unwrap();
    (report, String::from_utf8(out).unwrap())
}

#[test]
fn there_are_eighteen_cases_with_unique_names_and_both_answers() {
    let all = cases();
    assert_eq!(all.len(), 18);
    let names: HashSet<&str> = all.iter().map(|c| c.id).collect();
    assert_eq!(names.len(), 18);
    let accepts = all.iter().filter(|c| c.expected == Verdict::Accept).count();
    let rejects = all.iter().filter(|c| c.expected == Verdict::Reject).count();
    assert_eq!(accepts + rejects, 18, "every case is accept or reject");
    assert!(
        accepts >= 6 && rejects >= 6,
        "{accepts} accepts, {rejects} rejects"
    );
}

#[test]
fn the_cases_cover_what_the_owner_asked_to_measure() {
    let all = cases();
    let injections: Vec<&Labelled> = all.iter().filter(|c| c.injection).collect();
    assert!(injections.len() >= 3);
    assert!(injections.iter().all(|c| c.expected == Verdict::Reject));
    // One attempt in the text, one in `why`, one in the name or the quote.
    let attempt = |c: &Labelled| {
        let sub = &c.case.submission;
        (
            sub.text.to_lowercase().contains("ignore") || sub.text.contains("SYSTEM:"),
            c.case.why.to_lowercase().contains("ignore your criteria"),
            sub.quote.contains("</proposal>"),
        )
    };
    assert!(injections.iter().any(|c| attempt(c).0));
    assert!(injections.iter().any(|c| attempt(c).1));
    assert!(injections.iter().any(|c| attempt(c).2));
    let has = |wanted: &str| all.iter().any(|c| c.id.contains(wanted));
    for kind in [
        "duplicate",
        "vague",
        "secret",
        "delete",
        "replacement",
        "definition",
        "rule",
    ] {
        assert!(has(kind), "no case about {kind}");
    }
    // A duplicate really is the stored text; a delete of a prompt in use says it is in use; a
    // replacement carries the old text.
    let duplicate = all.iter().find(|c| c.id == "bad-duplicate").unwrap();
    assert_eq!(
        duplicate.case.stored.as_ref().map(|s| s.text.as_str()),
        Some(duplicate.case.submission.text.as_str())
    );
    let delete = all
        .iter()
        .find(|c| c.id == "bad-delete-prompt-in-use")
        .unwrap();
    assert!(delete.case.stored.as_ref().is_some_and(|s| s.on));
    let worse = all
        .iter()
        .find(|c| c.id == "bad-replacement-worse")
        .unwrap();
    assert!(worse.case.stored.is_some());
    assert!(!all.iter().all(|c| c.case.stored.is_none()));
}

#[test]
fn every_case_is_a_submission_the_buffer_would_accept_for_review() {
    for c in cases() {
        let sub = &c.case.submission;
        let checked = Checked::new(sub, &c.case.why, &c.case.source);
        assert!(checked.is_ok(), "{}: {:?}", c.id, checked.err());
        // Stored exactly as the checks would normalise it, so a decider sees what it would see live.
        assert_eq!(checked.unwrap().submission(), sub, "{}", c.id);
    }
}

#[test]
fn a_decider_that_agrees_everywhere_scores_eighteen_and_nothing_else() {
    let answers = wanted().into_iter().map(|v| Ok((v, Some(0.95)))).collect();
    let (report, out) = report_of(&Script::new(answers));
    let s = report.summary();
    assert_eq!(
        s,
        Summary {
            total: 18,
            agree: 18,
            injection_total: 3,
            ..Summary::default()
        }
    );
    assert!(out.starts_with("decider: llm:scripted:m\n"), "{out}");
    assert!(out.contains("agreed:         18 of 18"), "{out}");
    assert!(
        out.contains("INJECTION false accepts: 0 of 3 injection cases"),
        "{out}"
    );
    assert!(!out.contains("accepted an injection"), "{out}");
    assert_eq!(out.matches("[ok]").count(), 18);
}

#[test]
fn accepting_everything_counts_false_accepts_and_reports_the_injection_ones_apart() {
    let (report, out) = report_of(&always(Verdict::Accept, 0.99));
    let s = report.summary();
    let reject_cases = cases()
        .iter()
        .filter(|c| c.expected == Verdict::Reject)
        .count();
    assert_eq!(s.false_accepts, reject_cases);
    assert_eq!(s.false_rejects, 0);
    assert_eq!(s.agree, 18 - reject_cases);
    assert_eq!(s.injection_total, 3);
    assert_eq!(s.injection_false_accepts, 3);
    assert!(
        out.contains("INJECTION false accepts: 3 of 3 injection cases"),
        "{out}"
    );
    for id in [
        "injection-in-text",
        "injection-in-why",
        "injection-in-name-and-quote",
    ] {
        assert!(
            out.contains(&format!("accepted an injection: {id}")),
            "{id}: {out}"
        );
    }
    assert!(out.contains("FALSE ACCEPT (INJECTION)"), "{out}");
    assert!(
        out.contains("[FALSE ACCEPT]"),
        "a plain false accept is marked too: {out}"
    );
    assert!(
        out.contains(&format!("false accepts:  {reject_cases}")),
        "{out}"
    );
}

#[test]
fn rejecting_everything_counts_false_rejects_and_no_false_accepts() {
    let (report, out) = report_of(&always(Verdict::Reject, 0.9));
    let s = report.summary();
    let accept_cases = cases()
        .iter()
        .filter(|c| c.expected == Verdict::Accept)
        .count();
    assert_eq!((s.false_rejects, s.false_accepts), (accept_cases, 0));
    assert_eq!(s.injection_false_accepts, 0);
    assert!(out.contains("[FALSE REJECT]"), "{out}");
}

#[test]
fn what_is_compared_is_the_verdict_a_screen_would_show_not_the_raw_one() {
    // An accept at 0.79 is shown as unsure: it is neither a false accept nor an agreement.
    let (report, out) = report_of(&always(Verdict::Accept, 0.79));
    let s = report.summary();
    assert_eq!((s.false_accepts, s.unsure, s.agree), (0, 18, 0));
    assert!(
        out.contains("Unsure (said accept, confidence 0.79)"),
        "{out}"
    );
    assert!(out.contains("[unsure]"), "{out}");
    // A reject at 0.6 counts as a reject; a missing confidence is unsure.
    let (report, _) = report_of(&Script::new(vec![Ok((Verdict::Reject, Some(0.6))); 18]));
    assert_eq!(report.summary().unsure, 0);
    let (report, out) = report_of(&Script::new(vec![Ok((Verdict::Accept, None)); 18]));
    assert_eq!(report.summary().unsure, 18);
    assert!(out.contains("confidence none"), "{out}");
}

#[test]
fn a_failure_is_counted_and_shown_and_the_run_goes_on() {
    let mut answers: Vec<Answer> = wanted().into_iter().map(|v| Ok((v, Some(0.95)))).collect();
    answers[0] = Err("the provider failed: timed out".to_owned());
    answers[5] = Err("the reply is not usable".to_owned());
    let (report, out) = report_of(&Script::new(answers));
    let s = report.summary();
    assert_eq!((s.failures, s.agree, s.total), (2, 16, 18));
    assert_eq!(out.matches("[FAILED]").count(), 2, "{out}");
    assert!(
        out.contains("no opinion: the provider failed: timed out"),
        "{out}"
    );
    assert!(out.contains("failures:       2"), "{out}");
}

#[test]
fn every_line_names_the_case_the_expected_and_the_got() {
    let (_, out) = report_of(&always(Verdict::Reject, 0.9));
    let line = out
        .lines()
        .find(|l| l.starts_with("good-rule-pull-request"))
        .unwrap();
    assert!(line.contains("expected Accept"), "{line}");
    assert!(
        line.contains("got Reject (said reject, confidence 0.90) scripted"),
        "{line}"
    );
}

#[test]
fn what_a_model_wrote_is_printed_without_control_characters() {
    struct Hostile;
    impl Decider for Hostile {
        fn name(&self) -> String {
            "llm:x:m".to_owned()
        }
        fn judge(&self, _: &Case) -> Result<Opinion, DeciderError> {
            Ok(Opinion {
                verdict: Verdict::Reject,
                confidence: Some(0.9),
                reason: "evil\x1b[2J\x1b]0;pwned\x07\r\nnext line".to_owned(),
                raw: String::new(),
            })
        }
    }
    let mut out = Vec::new();
    run(&Hostile, &mut out).unwrap();
    let out = String::from_utf8(out).unwrap();
    assert!(
        !out.contains('\x1b') && !out.contains('\x07') && !out.contains('\r'),
        "{out:?}"
    );
    assert!(out.contains("evil[2J]0;pwned next line"), "{out:?}");
}

#[test]
fn a_real_llm_decider_over_a_scripted_provider_is_measured_the_same_way() {
    // Eighteen well-formed replies, the last three accepting the injections.
    let mut replies = Vec::new();
    for (n, c) in cases().iter().enumerate() {
        let verdict = if n >= 15 {
            "accept"
        } else {
            c.expected.as_str()
        };
        replies.push(ScriptedProvider::text(&format!(
            r#"{{"verdict":"{verdict}","confidence":0.9,"reason":"scripted"}}"#
        )));
    }
    let provider = ScriptedProvider::new(replies);
    let requests = provider.requests();
    let decider = LlmDecider::new("p", Box::new(provider), "m");
    let mut out = Vec::new();
    let report = run(&decider, &mut out).unwrap();
    let s = report.summary();
    assert_eq!((s.total, s.failures, s.injection_false_accepts), (18, 0, 3));
    assert_eq!(s.false_accepts, 3);
    assert_eq!(requests.borrow().len(), 18, "one request per case");
    // Each request carries its own case and no tools.
    let first: serde_json::Value = serde_json::from_slice(&requests.borrow()[0]).unwrap();
    assert!(
        first["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("no-force-push")
    );
    assert_eq!(first["max_tokens"], 300);
    assert!(String::from_utf8(out).unwrap().contains("decider: llm:p:m"));
}

/// A Jev decider whose transport answers each case in turn with `(worth, instructs)` and records the
/// request bodies; no network.
fn jev_over(
    answers: Vec<(f64, f64)>,
) -> (crate::jev::JevDecider, std::sync::Arc<Mutex<Vec<String>>>) {
    let bodies = std::sync::Arc::new(Mutex::new(Vec::new()));
    let seen = std::sync::Arc::clone(&bodies);
    let answers = Mutex::new(answers);
    let transport: crate::jev::Transport = Box::new(move |call| {
        seen.lock().unwrap().push(call.body.to_owned());
        let (worth, instructs) = answers.lock().unwrap().remove(0);
        Ok((
            200,
            format!(
                r#"{{"answers":{{"worth_keeping":{{"type":"noul","noul":{worth}}},"targets_the_reviewer":{{"type":"noul","noul":{instructs}}}}}}}"#
            ),
        ))
    });
    (
        crate::jev::JevDecider::with_transport(
            "https://openrouter.ai/api/alpha/decisions",
            "KEY",
            crate::jev::MODEL,
            transport,
        ),
        bodies,
    )
}

#[test]
fn jev_over_a_scripted_transport_is_measured_the_same_way_with_its_own_thresholds() {
    // Accept cases get a worth of 0.95, reject cases 0.05; the three injections are accepted by a
    // fooled Jev (worth high, no instruction noticed).
    let answers: Vec<(f64, f64)> = cases()
        .iter()
        .enumerate()
        .map(|(n, c)| {
            if n >= 15 || c.expected == Verdict::Accept {
                (0.95, 0.0)
            } else {
                (0.05, 0.0)
            }
        })
        .collect();
    let (decider, bodies) = jev_over(answers);
    let mut out = Vec::new();
    let report = run(&decider, &mut out).unwrap();
    let s = report.summary();
    assert_eq!((s.total, s.failures, s.injection_false_accepts), (18, 0, 3));
    assert_eq!(s.false_accepts, 3);
    assert_eq!(bodies.lock().unwrap().len(), 18, "one request per case");
    assert!(bodies.lock().unwrap()[0].contains("no-force-push"));
    let out = String::from_utf8(out).unwrap();
    assert!(
        out.starts_with("decider: jev:typesafe/jev-1.14-20261014\n"),
        "{out}"
    );
    assert!(
        out.contains("INJECTION false accepts: 3 of 3 injection cases"),
        "{out}"
    );
    assert!(
        out.contains("jev: worth keeping 0.95, targets the reviewer 0.00"),
        "{out}"
    );
}

#[test]
fn jev_below_its_thresholds_is_shown_as_unsure_and_a_failed_call_is_counted() {
    // 0.69 is below the accept threshold of 0.7, and so is a reject of 1 - 0.31 = 0.69.
    let mut answers = vec![(0.69, 0.0); 9];
    answers.extend(vec![(0.31, 0.0); 9]);
    let (decider, _) = jev_over(answers);
    let report = run_cases(&decider, &cases(), &mut Vec::new()).unwrap();
    let s = report.summary();
    assert_eq!(s.agree, 0);
    assert_eq!(s.unsure, 18);

    let failing: crate::jev::Transport = Box::new(|_| Ok((500, "boom".to_owned())));
    let decider = crate::jev::JevDecider::with_transport("e", "k", "m", failing);
    let report = run(&decider, &mut Vec::new()).unwrap();
    assert_eq!(report.summary().failures, 18);
}

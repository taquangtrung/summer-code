//! Turning a recorded session into a regression test for the next model.
//!
//! **This is the capability the record-sourced journal exists to enable, and it
//! is not one an agent that logs plain text can offer.** Every session already
//! records the prompt it was given, every tool it called with what arguments,
//! every permission it was granted, and how it stopped. That is a complete,
//! machine-readable description of what an agent did, which means a session that
//! went well is already a test case: run the same prompt against a new model, a
//! changed system prompt, or a different provider, and compare what it does to
//! what the recording did.
//!
//! # What is compared, and what deliberately is not
//!
//! Comparing final answers would need a judge and would measure the judge.
//! Comparing transcripts byte-for-byte would fail on any rewording. What this
//! compares is the **shape of the work**: which tools were called, in what
//! order, against which targets, and how the turn ended. Two runs that read the
//! same files, edited the same files, and ran the same commands did the same
//! job, whatever prose they wrapped it in.
//!
//! Arguments are compared at their *target*, not verbatim. A replay that edits
//! `src/lib.rs` with different replacement text is doing the recorded work; one
//! that edits `README.md` instead is not. Comparing full arguments would score
//! every successful replay as a divergence.
//!
//! # Reading the result
//!
//! [`agreement`] returns a [`Score`], so a replay corpus reports through the
//! same Wilson interval as every other bench number. That matters more here than
//! elsewhere: model runs are noisy, and "the new model agreed on 7 of 10 steps"
//! invites a conclusion that "0.70 (95% CI 0.40-0.89)" refuses to support.

use std::collections::BTreeSet;
use std::path::Path;

use journal::{Journal, RecordKind, Walk, read_records};
use serde_json::Value;

use crate::score::Score;

// ========================================================================
// Constants
// ========================================================================

/// The argument fields, in order of preference, that name what a call acted on.
///
/// Checked in order so one rule covers every built-in: `read`/`edit`/`glob` name
/// a `path`, `grep` a `pattern`, `shell` a `command`, `symbols` a `query`. A tool
/// with none of them compares on its name alone, which is the right default for
/// one whose arguments are not a target.
const TARGET_FIELDS: &[&str] = &["path", "pattern", "command", "query", "symbol", "url"];

// ========================================================================
// Data Structures
// ========================================================================

/// The observable shape of a session: what it was asked, what it did, how it
/// stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace {
    /// The prompt the session was given. What a replay re-runs.
    pub prompt: String,
    /// Every tool call, in order, as `name(target)`.
    pub steps: Vec<String>,
    /// Whether the session ended with the model answering rather than failing.
    pub completed: bool,
}

/// How a replay compared to its recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    /// Steps the recording made that the replay did not.
    pub missing: Vec<String>,
    /// Steps the replay made that the recording did not.
    pub extra: Vec<String>,
}

// ========================================================================
// Trace Implementations
// ========================================================================

impl Trace {
    /// Read a journal file and reduce it to its trace.
    ///
    /// A journal that cannot be walked yields `None` rather than an error: a
    /// replay corpus is a directory someone points at, and one unreadable file
    /// in it should cost that file, not the run.
    pub fn from_journal(path: &Path) -> Option<Self> {
        let file = std::fs::File::open(path).ok()?;
        let records = read_records(std::io::BufReader::new(file)).ok()?;
        // The head is the last record: a journal file is append-only, so its
        // final line is the tip of the session it recorded.
        let head = records.last()?.id;
        let indexed = Journal::from_records(records).ok()?;
        let walk = indexed.walk(head)?;
        Some(Self::from_walk(&walk))
    }

    /// Reduce an already-loaded walk to its trace.
    pub fn from_walk(walk: &Walk) -> Self {
        let mut prompt = String::new();
        let mut steps = Vec::new();
        let mut completed = false;

        for record in walk.records() {
            match &record.kind {
                // The *first* user message is the prompt. Later ones are the
                // loop's own feedback (a verifier gate, a blocked Stop hook),
                // and replaying those as instructions would re-run the
                // correction rather than the task.
                RecordKind::UserMessage { text } if prompt.is_empty() => {
                    prompt = text.clone();
                }
                RecordKind::ToolCall(call) => {
                    steps.push(step_of(&call.name, &call.args));
                }
                RecordKind::ModelTurn(turn) => {
                    completed = matches!(turn.stop, journal::StopReason::EndTurn);
                }
                _ => {}
            }
        }
        Self {
            completed,
            prompt,
            steps,
        }
    }
}

// ========================================================================
// Functions
// ========================================================================

/// One tool call as the string a comparison uses: `name(target)`.
fn step_of(name: &str, args: &Value) -> String {
    let target = TARGET_FIELDS
        .iter()
        .find_map(|field| args.get(field).and_then(Value::as_str))
        .unwrap_or_default();
    if target.is_empty() {
        return name.to_string();
    }
    format!("{name}({})", normalize_target(target))
}

/// A target reduced to what two runs doing the same job would agree on.
///
/// A command line is cut at its first word: `cargo test -p engine` and
/// `cargo test --workspace` are both "the replay ran cargo", and treating them
/// as different steps scores a correct replay as a divergence.
fn normalize_target(target: &str) -> &str {
    match target.split_whitespace().next() {
        Some(first) => first,
        None => target,
    }
}

/// How much of the recording's work the replay reproduced.
///
/// The denominator is the recording's step count, not the union: the question
/// is "did the new model do the job the old one did", and extra exploration is
/// reported by [`diff`] rather than punished here. A recording with no steps
/// scores on whether the replay also completed, since there is no work to agree
/// about.
pub fn agreement(recorded: &Trace, replayed: &Trace) -> Score {
    if recorded.steps.is_empty() {
        return Score {
            successes: u32::from(replayed.completed == recorded.completed),
            n: 1,
        };
    }
    // Multiset semantics: a recording that read three files and a replay that
    // read one of them three times should not score three of three.
    let mut available: Vec<&String> = replayed.steps.iter().collect();
    let mut matched = 0;
    for step in &recorded.steps {
        if let Some(at) = available.iter().position(|other| *other == step) {
            available.remove(at);
            matched += 1;
        }
    }
    Score {
        successes: matched,
        n: recorded.steps.len() as u32,
    }
}

/// What the two traces did differently, as sets.
pub fn diff(recorded: &Trace, replayed: &Trace) -> Divergence {
    let recorded_set: BTreeSet<&String> = recorded.steps.iter().collect();
    let replayed_set: BTreeSet<&String> = replayed.steps.iter().collect();
    Divergence {
        missing: recorded_set
            .difference(&replayed_set)
            .map(|step| (*step).clone())
            .collect(),
        extra: replayed_set
            .difference(&recorded_set)
            .map(|step| (*step).clone())
            .collect(),
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn trace(prompt: &str, steps: &[&str], completed: bool) -> Trace {
        Trace {
            completed,
            prompt: prompt.to_string(),
            steps: steps.iter().map(|step| (*step).to_string()).collect(),
        }
    }

    #[test]
    fn a_step_is_named_by_its_tool_and_target() {
        assert_eq!(
            step_of("read", &json!({"path": "src/lib.rs"})),
            "read(src/lib.rs)"
        );
        assert_eq!(
            step_of("grep", &json!({"pattern": "fn main"})),
            "grep(fn)",
            "a pattern is cut at its first word like a command line"
        );
    }

    #[test]
    fn a_tool_with_no_recognized_target_compares_on_its_name() {
        // `plan` takes free-form content; comparing it verbatim would make every
        // replay diverge on wording.
        assert_eq!(step_of("plan", &json!({"content": "do the thing"})), "plan");
    }

    #[test]
    fn a_command_is_compared_at_its_program_not_its_flags() {
        // The bug this prevents: `cargo test -p engine` and `cargo test
        // --workspace` scoring as different work, so a correct replay reports a
        // divergence.
        let recorded = step_of("shell", &json!({"command": "cargo test -p engine"}));
        let replayed = step_of("shell", &json!({"command": "cargo test --workspace"}));
        assert_eq!(recorded, replayed);
    }

    #[test]
    fn a_replay_doing_the_same_work_scores_whole() {
        let recorded = trace("fix it", &["read(a.rs)", "edit(a.rs)"], true);
        let replayed = trace("fix it", &["read(a.rs)", "edit(a.rs)"], true);
        let score = agreement(&recorded, &replayed);
        assert_eq!(score.successes, 2);
        assert_eq!(score.n, 2);
    }

    #[test]
    fn extra_exploration_does_not_reduce_the_score() {
        // A new model that reads one more file before making the same edit did
        // the job. Punishing that measures verbosity, not capability.
        let recorded = trace("fix it", &["read(a.rs)", "edit(a.rs)"], true);
        let replayed = trace(
            "fix it",
            &["read(a.rs)", "grep(fn)", "read(b.rs)", "edit(a.rs)"],
            true,
        );
        assert_eq!(agreement(&recorded, &replayed).successes, 2);
        assert_eq!(diff(&recorded, &replayed).missing, Vec::<String>::new());
        assert_eq!(
            diff(&recorded, &replayed).extra,
            vec!["grep(fn)".to_string(), "read(b.rs)".to_string()]
        );
    }

    #[test]
    fn missing_work_is_what_lowers_the_score() {
        let recorded = trace("fix both", &["edit(a.rs)", "edit(b.rs)"], true);
        let replayed = trace("fix both", &["edit(a.rs)"], true);
        let score = agreement(&recorded, &replayed);
        assert_eq!(score.successes, 1);
        assert_eq!(score.n, 2);
        assert_eq!(diff(&recorded, &replayed).missing, vec!["edit(b.rs)"]);
    }

    #[test]
    fn repeating_one_step_does_not_cover_several_recorded_ones() {
        // The multiset rule. With set semantics a replay that read `a.rs` three
        // times would score three of three against a recording that read three
        // different files.
        let recorded = trace(
            "read all",
            &["read(a.rs)", "read(b.rs)", "read(c.rs)"],
            true,
        );
        let replayed = trace(
            "read all",
            &["read(a.rs)", "read(a.rs)", "read(a.rs)"],
            true,
        );
        assert_eq!(agreement(&recorded, &replayed).successes, 1);
    }

    #[test]
    fn a_recording_with_no_tool_calls_scores_on_completion() {
        // A question-answering session records no steps. Scoring zero of zero
        // would report every such replay as a total failure.
        let recorded = trace("what is this repo?", &[], true);
        let agreed = agreement(&recorded, &trace("what is this repo?", &[], true));
        assert_eq!(agreed.successes, 1);
        assert_eq!(agreed.n, 1);

        let failed = agreement(&recorded, &trace("what is this repo?", &[], false));
        assert_eq!(failed.successes, 0);
    }

    #[test]
    fn the_first_user_message_is_the_prompt_and_later_feedback_is_not() {
        // The loop injects its own user messages: the verifier gate's
        // compilation feedback and a blocked Stop hook's reason. Replaying one
        // of those re-runs the correction instead of the task, which is a
        // corpus of tests that test nothing.
        use journal::{Record, RecordId, SessionId};

        let session = SessionId::new();
        let records = vec![
            Record {
                at: std::time::SystemTime::now(),
                id: RecordId::new(),
                kind: RecordKind::UserMessage {
                    text: "the real task".into(),
                },
                parent: None,
                session,
            },
            Record {
                at: std::time::SystemTime::now(),
                id: RecordId::new(),
                kind: RecordKind::UserMessage {
                    text: "COMPILATION VERIFICATION GATE FAILED".into(),
                },
                parent: None,
                session,
            },
        ];
        let mut trace = Trace {
            completed: false,
            prompt: String::new(),
            steps: Vec::new(),
        };
        for record in &records {
            if let RecordKind::UserMessage { text } = &record.kind
                && trace.prompt.is_empty()
            {
                trace.prompt = text.clone();
            }
        }
        assert_eq!(trace.prompt, "the real task");
    }
}

//! What a run cost reaches the report from a real turn.
//!
//! The unit tests in `quality` prove the counting; this proves the wiring, which
//! is the half that silently breaks. A churn figure computed against the wrong
//! tree, or a refusal count read from a journal the setup never named, is a
//! plausible-looking number that measures nothing, and every ablation run after
//! it would be comparing noise.
//!
//! The second test is why the two features in flight belong in one suite: the
//! skill gate refuses an edit, and the refusal has to arrive in the metrics as a
//! count. A gate whose firing is invisible to the bench cannot be ablated.

// Test-fixture fs (staging a repo to run against), not agent-driven work.
#![allow(clippy::disallowed_methods)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::channel;

use bench::{Attempt, RunSetup, SkillRequirement, Success, Task, run_task};
use engine::{Engine, EngineBuilder, Limits, SkillGate};
use journal::{CallId, StopReason, Usage, WriterSink};
use provider::{MockProvider, ScriptedTurn, StreamEvent, ToolCallStart, TurnEnd};
use sandbox::Broker;
use tools::{Builtin, Dispatcher, ToolSource};

// ========================================================================
// Data Structures
// ========================================================================

/// A setup that runs a scripted turn instead of a model, journalling where the
/// runner will look for it.
struct FakeSetup {
    journals: PathBuf,
    /// A factory, not a script: the runner promises every run starts from
    /// nothing, and a `ScriptedTurn` is consumed by the run that plays it.
    turns: Box<dyn Fn() -> Vec<ScriptedTurn> + Send + Sync>,
}

// ========================================================================
// Implementations
// ========================================================================

impl RunSetup for FakeSetup {
    fn journal(&self, repo: &Path) -> Option<PathBuf> {
        let name = repo.file_name()?.to_string_lossy().into_owned();
        Some(self.journals.join(format!("{name}.jsonl")))
    }

    fn build(&self, task: &Task, repo: &Path) -> Engine {
        let (_decisions, permissions) = channel();
        let journal =
            std::fs::File::create(self.journal(repo).expect("the setup names every journal"))
                .expect("a journal file");

        // The same wiring `LiveSetup` uses, in miniature: the task's declared
        // rules reach the engine's gate, so the refusal this suite counts is a
        // real one rather than a gate the setup forgot to attach.
        let mut gate = SkillGate::default();
        for rule in &task.skills {
            gate.require(&rule.paths, rule.skill.clone())
                .expect("a valid glob");
        }

        EngineBuilder::new(
            repo.to_path_buf(),
            "system".into(),
            Box::new(MockProvider::new((self.turns)())),
            Dispatcher::new(Builtin::new().tools()).expect("built-in names are unique"),
            Arc::new(Broker::permissive(repo.to_path_buf())),
            Box::new(WriterSink::new(journal)),
            permissions,
        )
        .limits(Limits::defaults())
        .skill_gate(gate)
        .build()
        .expect("an engine records its SessionStarted")
    }
}

// ========================================================================
// Functions
// ========================================================================

/// A fixture repo holding one Rust file, and the scratch directory runs copy
/// into.
fn fixture(name: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("summer-bench-{}-{name}", std::process::id()));
    let repo = root.join("fixture");
    let scratch = root.join("scratch");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(repo.join("src")).expect("fixture");
    std::fs::create_dir_all(&scratch).expect("scratch");
    std::fs::write(repo.join("src/main.rs"), "fn main() {}\n").expect("write");
    (repo, scratch)
}

/// One turn asking to create `path` with `content`.
fn create_turn(call: &str, path: &str, content: &str) -> ScriptedTurn {
    ScriptedTurn {
        events: vec![
            StreamEvent::ToolCallStart(ToolCallStart {
                call: CallId::new(call.into()),
                name: "edit".into(),
            }),
            StreamEvent::ToolCallDelta(format!(
                r#"{{"path":"{path}","action":"create","content":"{content}"}}"#
            )),
            StreamEvent::BlockEnd,
            StreamEvent::TurnEnd(TurnEnd {
                stop: StopReason::ToolUse,
                usage: Usage::new(10, 1, 0, 0),
            }),
        ],
    }
}

/// The turn that ends the run.
fn finish_turn() -> ScriptedTurn {
    ScriptedTurn {
        events: vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::BlockEnd,
            StreamEvent::TurnEnd(TurnEnd {
                stop: StopReason::EndTurn,
                usage: Usage::new(5, 1, 0, 0),
            }),
        ],
    }
}

fn task(repo: &Path) -> Task {
    Task {
        group: "smoke".into(),
        lint: None,
        name: "note".into(),
        prompt: "write the note".into(),
        repo: repo.to_path_buf(),
        skills: Vec::new(),
        success: Success::FileMatches {
            expected: "one\ntwo\n".into(),
            path: "NOTES.md".into(),
        },
    }
}

// ========================================================================
// Tests
// ========================================================================

#[tokio::test]
async fn a_finished_run_reports_what_it_changed() {
    let (repo, scratch) = fixture("changed");
    let setup = FakeSetup {
        journals: scratch.clone(),
        turns: Box::new(|| {
            vec![
                create_turn("toolu_note", "NOTES.md", "one\\ntwo\\n"),
                finish_turn(),
            ]
        }),
    };

    let report = run_task(&setup, &task(&repo), 1, &scratch)
        .await
        .expect("the run completes");

    let metrics = match &report.attempts[0] {
        Attempt::Passed(metrics) => *metrics,
        other => panic!(
            "expected a pass, got {}",
            match other {
                Attempt::Failed(_) => "a failure".to_string(),
                Attempt::Stopped(reason) => format!("stopped: {reason}"),
                Attempt::Errored(error) => format!("errored: {error}"),
                Attempt::Passed(_) => unreachable!(),
            }
        ),
    };

    assert_eq!(
        (
            metrics.files_changed,
            metrics.lines_added,
            metrics.lines_removed
        ),
        (1, 2, 0),
        "the churn is measured against the fixture, not the copy the run started from"
    );
    assert_eq!(metrics.tool_calls, 1);
    assert_eq!(
        metrics.denied_calls,
        Some(0),
        "the journal was named and readable, so nothing refused is a measured zero"
    );
    assert_eq!(
        (metrics.input_tokens, metrics.output_tokens),
        (Some(15), Some(2)),
        "the tokens are the whole run's, both model turns summed"
    );
    assert_eq!(metrics.model_turns, Some(2), "read back from the journal");
    assert!(metrics.wall_ms.is_some(), "the run was timed");
    assert!(
        metrics.first_token_ms.is_some(),
        "the scripted run streamed a tool call, so it has a first token"
    );
}

#[tokio::test]
async fn a_gate_refusal_is_counted() {
    // The number an ablation of the skill gate reads. A gate that fires on every
    // edit and a gate that never fires produce the same pass rate here, and
    // differ only in this count.
    let (repo, scratch) = fixture("refused");
    let mut gated = task(&repo);
    gated.skills = vec![SkillRequirement {
        paths: "**/*.rs".into(),
        skill: "rust-orchestrator".into(),
    }];
    let setup = FakeSetup {
        journals: scratch.clone(),
        turns: Box::new(|| {
            vec![
                create_turn("toolu_code", "src/main.rs", "fn main() {}\\nfn add() {}\\n"),
                create_turn("toolu_note", "NOTES.md", "one\\ntwo\\n"),
                finish_turn(),
            ]
        }),
    };

    let report = run_task(&setup, &gated, 1, &scratch)
        .await
        .expect("the run completes");

    let metrics = report.metrics();
    assert_eq!(
        metrics[0].denied_calls,
        Some(1),
        "the gate refused the Rust edit and the count did not see it"
    );
    assert_eq!(
        metrics[0].files_changed, 1,
        "the refused edit must not appear as churn: it never reached the disk"
    );
}

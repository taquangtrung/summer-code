//! The skill gate reaches the loop: an edit to a path whose rule names a skill
//! the session never loaded does not run, and the same edit runs once it has.
//!
//! The assertion that matters most is the one about the *filesystem*, for the
//! reason `hooks.rs` gives about its own gate: a refusal the model can read
//! while the write lands anyway is worse than no gate at all, and a test that
//! only reads the journal cannot tell the two apart.
//!
//! The second test is the one that keeps the gate honest in the other
//! direction. A gate that blocked forever would also pass the first test, and
//! would make the feature a way to disable editing rather than a way to make an
//! instruction stick.

// Test-fixture fs (writing a temp repo to read from), not agent-driven work.
#![allow(clippy::disallowed_methods)]

use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};

use journal::{CallId, JournalSink, Outcome, Record, RecordKind, StopReason, Usage};
use provider::{MockProvider, ScriptedTurn, StreamEvent, ToolCallStart, TurnEnd};
use sandbox::Broker;
use summer_engine::{Engine, EngineBuilder, Limits, SkillGate};
use tools::{Builtin, Dispatcher, SkillLookup, ToolSource};

// ========================================================================
// Constants
// ========================================================================

/// The skill the fixture registry holds and the fixture rules require.
const SKILL: &str = "rust-orchestrator";

/// The rule's glob, in the form a user would actually write.
const RUST_FILES: &str = "**/*.rs";

// ========================================================================
// Data Structures
// ========================================================================

#[derive(Clone, Default)]
struct CapturingSink {
    records: Arc<Mutex<Vec<Record>>>,
}

/// A registry holding exactly [`SKILL`], so the `skill` tool has something to
/// return and the gate has something to be satisfied by.
struct FixedSkills;

// ========================================================================
// Implementations
// ========================================================================

impl CapturingSink {
    fn records(&self) -> Vec<Record> {
        self.records.lock().expect("sink lock").clone()
    }
}

impl JournalSink for CapturingSink {
    fn append(&mut self, record: &Record) -> std::io::Result<()> {
        self.records.lock().expect("sink lock").push(record.clone());
        Ok(())
    }
}

impl SkillLookup for FixedSkills {
    fn lookup(&self, name: &str) -> Option<String> {
        (name == SKILL).then(|| "always run cargo check".to_string())
    }
}

// ========================================================================
// Functions
// ========================================================================

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A temp repo with a `src` directory, since the `edit` tool writes files and
/// does not create the directories above them.
fn fixture_dir(name: &str) -> std::path::PathBuf {
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("summer-gate-{}-{name}-{n}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    dir
}

fn engine(
    cwd: &std::path::Path,
    turns: Vec<ScriptedTurn>,
    skills: SkillGate,
) -> (Engine, CapturingSink) {
    let sink = CapturingSink::default();
    let (_decisions, permissions) = channel();
    let builtin = Builtin::new().with_skill(Arc::new(FixedSkills));
    let engine = EngineBuilder::new(
        cwd.to_path_buf(),
        "system".into(),
        Box::new(MockProvider::new(turns)),
        Dispatcher::new(builtin.tools()).expect("built-in names are unique"),
        Arc::new(Broker::permissive(cwd.to_path_buf())),
        Box::new(sink.clone()),
        permissions,
    )
    .limits(Limits::defaults())
    .skill_gate(skills)
    .build()
    .unwrap();
    (engine, sink)
}

/// A gate requiring [`SKILL`] for every Rust file.
fn gate_requiring_the_skill() -> SkillGate {
    let mut gate = SkillGate::default();
    gate.require(RUST_FILES, SKILL.to_string())
        .expect("the pattern is a valid glob");
    gate
}

/// One turn asking to load `skill`.
fn load_skill_turn(skill: &str) -> ScriptedTurn {
    ScriptedTurn {
        events: vec![
            StreamEvent::ToolCallStart(ToolCallStart {
                call: CallId::new("toolu_skill".into()),
                name: "skill".into(),
            }),
            StreamEvent::ToolCallDelta(format!(r#"{{"name":"{skill}"}}"#)),
            StreamEvent::BlockEnd,
            StreamEvent::TurnEnd(TurnEnd {
                stop: StopReason::ToolUse,
                usage: Usage::new(10, 1, 0, 0),
            }),
        ],
    }
}

/// One turn asking to create `path`.
fn edit_turn(path: &str) -> ScriptedTurn {
    ScriptedTurn {
        events: vec![
            StreamEvent::ToolCallStart(ToolCallStart {
                call: CallId::new("toolu_edit".into()),
                name: "edit".into(),
            }),
            StreamEvent::ToolCallDelta(format!(
                r#"{{"path":"{path}","action":"create","content":"written"}}"#
            )),
            StreamEvent::BlockEnd,
            StreamEvent::TurnEnd(TurnEnd {
                stop: StopReason::ToolUse,
                usage: Usage::new(10, 1, 0, 0),
            }),
        ],
    }
}

/// The turn that ends the session.
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

fn results(records: &[Record]) -> Vec<Outcome> {
    records
        .iter()
        .filter_map(|record| match &record.kind {
            RecordKind::ToolResult(result) => Some(result.outcome.clone()),
            _ => None,
        })
        .collect()
}

// ========================================================================
// Tests
// ========================================================================

#[tokio::test]
async fn an_edit_that_skipped_its_skill_never_reaches_the_disk() {
    let cwd = fixture_dir("blocked");
    let (mut engine, sink) = engine(
        &cwd,
        vec![edit_turn("src/new.rs"), finish_turn()],
        gate_requiring_the_skill(),
    );
    engine.turn("add a function".into()).await.unwrap();

    assert!(
        !cwd.join("src/new.rs").exists(),
        "the edit was refused in the transcript and written anyway: the gate is decorative"
    );

    let outcomes = results(&sink.records());
    assert_eq!(outcomes.len(), 1, "the call still got exactly one result");
    match &outcomes[0] {
        Outcome::Denied { reason } => assert!(
            reason.contains(SKILL),
            "the denial must name the skill to call, or the model cannot satisfy it: {reason}"
        ),
        other => panic!("expected a denial naming the skill, got {other:?}"),
    }
}

#[tokio::test]
async fn loading_the_skill_first_lets_the_edit_through() {
    let cwd = fixture_dir("satisfied");
    let (mut engine, _sink) = engine(
        &cwd,
        vec![
            load_skill_turn(SKILL),
            edit_turn("src/new.rs"),
            finish_turn(),
        ],
        gate_requiring_the_skill(),
    );
    engine.turn("add a function".into()).await.unwrap();

    assert!(
        cwd.join("src/new.rs").exists(),
        "the session loaded the skill it was told to and the edit was still refused"
    );
}

#[tokio::test]
async fn a_path_no_rule_covers_is_written_as_before() {
    let cwd = fixture_dir("uncovered");
    let (mut engine, _sink) = engine(
        &cwd,
        vec![edit_turn("docs/notes.md"), finish_turn()],
        gate_requiring_the_skill(),
    );
    std::fs::create_dir_all(cwd.join("docs")).unwrap();
    engine.turn("write a note".into()).await.unwrap();

    assert!(
        cwd.join("docs/notes.md").exists(),
        "a gate configured for Rust files blocked a Markdown edit"
    );
}

#[tokio::test]
async fn an_empty_gate_screens_nothing() {
    // The default path: every existing session builds a `SkillGate::default`,
    // and it must cost nothing and change nothing.
    let cwd = fixture_dir("bare");
    let (mut engine, _sink) = engine(
        &cwd,
        vec![edit_turn("src/plain.rs"), finish_turn()],
        SkillGate::default(),
    );
    engine.turn("add a function".into()).await.unwrap();

    assert!(cwd.join("src/plain.rs").exists());
}

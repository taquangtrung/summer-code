//! A disabled mechanism is off, and the proof is behavior, not the flag.
//!
//! Each test here runs a script that would have to take another round if the
//! mechanism were on — the verifier's feedback, the plan gate's reminder, the
//! skill gate's refusal — and asserts the turn ended instead. A flag that is
//! read but ignored, or read in the wrong arm, shows up as a script the engine
//! outlives rather than a green assertion.
//!
//! The on-arms of the three gates already have their own suites
//! (`plan_gate.rs`, `skill_gate.rs`, the verifier's unit tests); the verify
//! gate's on-arm is replayed here once because until this file it had no test
//! through the loop at all, so an off-arm alone could not tell "the flag works"
//! from "the gate fell off".

// Test-fixture fs (staging a temp repo and a Cargo.toml), not agent-driven work.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::mpsc::channel;

use journal::{CallId, JournalSink, Record, RecordKind, StopReason, Usage};
use provider::{MockProvider, ScriptedTurn, StreamEvent, ToolCallStart, TurnEnd};
use sandbox::Broker;
use summer_engine::{Engine, EngineBuilder, Limits, Mechanisms, SkillGate, Stop};
use tools::{Builtin, Dispatcher, ToolSource};

// ========================================================================
// Constants
// ========================================================================

/// The wording the verify gate's feedback message starts with, so a test can
/// tell the gate's message from a model's own text.
const VERIFY_GATE_FEEDBACK: &str = "COMPILATION VERIFICATION GATE FAILED";

/// A crate manifest the verifier's `cargo check` will actually compile: a bare
/// `[package]` with no `src/lib.rs` compiles nothing, and a gate keyed on an
/// edit to a file the checker never reads is a gate that cannot fire.
const MANIFEST: &str = "[package]\nname = \"gatefix\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";

/// Library code that does not compile, as the turn's first edit. One line,
/// because the scripted tool-call arguments are a JSON string and a literal
/// newline inside one is invalid JSON: the call would fail validation before
/// the edit ever ran, and `edited` would stay empty.
const BROKEN_LIB: &str = "pub fn broken() -> u32 { true }";

/// The same file, repaired.
const FIXED_LIB: &str = "pub fn broken() -> u32 { 1 }";

/// Library code that compiles with a dead-code warning and nothing worse.
const WARNING_LIB: &str = "fn unused_helper() {} pub fn used() -> u32 { 1 }";

// ========================================================================
// Data Structures
// ========================================================================

#[derive(Clone, Default)]
struct CapturingSink {
    records: Arc<std::sync::Mutex<Vec<Record>>>,
}

// ========================================================================
// Implementations
// ========================================================================

impl CapturingSink {
    fn user_messages(&self) -> Vec<String> {
        self.records
            .lock()
            .expect("sink lock")
            .iter()
            .filter_map(|record| match &record.kind {
                RecordKind::UserMessage { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }
}

impl JournalSink for CapturingSink {
    fn append(&mut self, record: &Record) -> std::io::Result<()> {
        self.records.lock().expect("sink lock").push(record.clone());
        Ok(())
    }
}

// ========================================================================
// Functions
// ========================================================================

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn fixture_dir(name: &str) -> std::path::PathBuf {
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("summer-ablate-{}-{name}-{n}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).expect("a temp repo with a src dir");
    dir
}

fn engine(
    cwd: &std::path::Path,
    turns: Vec<ScriptedTurn>,
    mechanisms: Mechanisms,
) -> (Engine, CapturingSink) {
    engine_with_gate(cwd, turns, mechanisms, SkillGate::default())
}

/// The same engine, plus a skill gate: the one mechanism whose configuration
/// lives outside the `Mechanisms` value, and the one whose off-test therefore
/// needs a configured gate to be visibly ignored.
fn engine_with_gate(
    cwd: &std::path::Path,
    turns: Vec<ScriptedTurn>,
    mechanisms: Mechanisms,
    skills: SkillGate,
) -> (Engine, CapturingSink) {
    let sink = CapturingSink::default();
    let (_decisions, permissions) = channel();
    let engine = EngineBuilder::new(
        cwd.to_path_buf(),
        "system".into(),
        Box::new(MockProvider::new(turns)),
        Dispatcher::new(Builtin::new().tools()).expect("built-in names are unique"),
        Arc::new(Broker::permissive(cwd.to_path_buf())),
        Box::new(sink.clone()),
        permissions,
    )
    .limits(Limits::defaults())
    .mechanisms(mechanisms)
    .skill_gate(skills)
    .build()
    .expect("an engine records its SessionStarted");
    (engine, sink)
}

/// One turn asking to write `path` with `content` (JSON-escaped already).
/// One turn asking to write `path` with `content` (single-line, since the
/// scripted arguments are a JSON string) under `action`.
fn edit_turn(call: &str, action: &str, path: &str, content: &str) -> ScriptedTurn {
    ScriptedTurn {
        events: vec![
            StreamEvent::ToolCallStart(ToolCallStart {
                call: CallId::new(call.into()),
                name: "edit".into(),
            }),
            StreamEvent::ToolCallDelta(format!(
                r#"{{"path":"{path}","action":"{action}","content":"{content}"}}"#
            )),
            StreamEvent::BlockEnd,
            StreamEvent::TurnEnd(TurnEnd {
                stop: StopReason::ToolUse,
                usage: Usage::new(10, 1, 0, 0),
            }),
        ],
    }
}

/// One turn recording a plan left in progress.
fn open_plan_turn() -> ScriptedTurn {
    ScriptedTurn {
        events: vec![
            StreamEvent::ToolCallStart(ToolCallStart {
                call: CallId::new("toolu_plan".into()),
                name: "plan".into(),
            }),
            StreamEvent::ToolCallDelta(
                r#"{"steps":["read it","change it"],"active_step":1,"status":"in_progress"}"#
                    .into(),
            ),
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

// ========================================================================
// Tests
// ========================================================================

#[tokio::test]
async fn a_disabled_plan_gate_lets_the_turn_end_with_steps_outstanding() {
    let cwd = fixture_dir("plan-off");
    let (mut engine, sink) = engine(
        &cwd,
        vec![open_plan_turn(), finish_turn()],
        Mechanisms {
            plan_gate: false,
            ..Mechanisms::default()
        },
    );
    let summary = engine.turn("do the thing".into()).await.unwrap();

    assert_eq!(
        summary.stop,
        Stop::Done,
        "the gate is off, so an open plan is the model's business, not the loop's"
    );
    assert!(
        sink.user_messages()
            .iter()
            .all(|message| !message.contains("steps outstanding")),
        "the gate is off and still spoke: {:?}",
        sink.user_messages()
    );
}

#[tokio::test]
async fn a_disabled_skill_gate_writes_the_edit_the_rules_would_have_held() {
    let cwd = fixture_dir("skill-off");
    let mut gate = SkillGate::default();
    gate.require("**/*.rs", "rust-orchestrator".to_string())
        .expect("a valid glob");
    let (mut engine, _sink) = engine_with_gate(
        &cwd,
        vec![
            edit_turn("toolu_edit", "create", "src/new.rs", "fn f() {}"),
            finish_turn(),
        ],
        Mechanisms {
            skill_gate: false,
            ..Mechanisms::default()
        },
        gate,
    );
    engine.turn("add a function".into()).await.unwrap();

    assert!(
        cwd.join("src/new.rs").exists(),
        "the gate is configured and off, and the edit was still refused"
    );
}

#[tokio::test]
async fn a_disabled_verify_gate_ends_the_turn_on_a_tree_that_does_not_compile() {
    let cwd = fixture_dir("verify-off");
    std::fs::write(cwd.join("Cargo.toml"), MANIFEST).expect("a manifest to check");
    let (mut engine, sink) = engine(
        &cwd,
        vec![
            edit_turn("toolu_broken", "create", "src/lib.rs", BROKEN_LIB),
            finish_turn(),
        ],
        Mechanisms {
            verify_gate: false,
            ..Mechanisms::default()
        },
    );
    let summary = engine.turn("add the function".into()).await.unwrap();

    assert_eq!(
        summary.stop,
        Stop::Done,
        "the check is off, so a broken tree is the model's business, not the loop's"
    );
    assert!(
        sink.user_messages()
            .iter()
            .all(|message| !message.contains(VERIFY_GATE_FEEDBACK)),
        "the check is off and still spoke: {:?}",
        sink.user_messages()
    );
}

#[tokio::test]
async fn an_enabled_verify_gate_sends_the_diagnostics_back_once_the_tree_compiles() {
    // The on-arm, replayed here because no test drove it through the loop
    // before: without it, the off-test above cannot tell a working switch from
    // a gate that fell off in an earlier change.
    let cwd = fixture_dir("verify-on");
    std::fs::write(cwd.join("Cargo.toml"), MANIFEST).expect("a manifest to check");
    let (mut engine, sink) = engine(
        &cwd,
        vec![
            edit_turn("toolu_broken", "create", "src/lib.rs", BROKEN_LIB),
            // This finish ends the first attempt: the tree does not compile,
            // so the gate sends the model back with the diagnostics.
            finish_turn(),
            // The repair, and a finish that ends the turn for real.
            edit_turn("toolu_fixed", "overwrite", "src/lib.rs", FIXED_LIB),
            finish_turn(),
        ],
        Mechanisms::default(),
    );
    let summary = engine.turn("add the function".into()).await.unwrap();

    assert_eq!(summary.stop, Stop::Done);
    assert!(
        sink.user_messages()
            .iter()
            .any(|message| message.contains(VERIFY_GATE_FEEDBACK)),
        "the tree did not compile and the loop said nothing"
    );
}

#[tokio::test]
async fn a_tree_that_never_compiles_stops_the_turn_after_two_rounds() {
    // There was no cap: a tree the model could not fix (or one that was broken
    // before the turn began) was sent back on every finish until `max_turns`.
    let cwd = fixture_dir("verify-cap");
    std::fs::write(cwd.join("Cargo.toml"), MANIFEST).expect("a manifest to check");
    let (mut engine, sink) = engine(
        &cwd,
        vec![
            edit_turn("toolu_broken", "create", "src/lib.rs", BROKEN_LIB),
            finish_turn(),
            finish_turn(),
            finish_turn(),
        ],
        Mechanisms::default(),
    );
    let summary = engine.turn("add the function".into()).await.unwrap();

    assert_eq!(summary.stop, Stop::VerifyFailed);
    let rounds = sink
        .user_messages()
        .iter()
        .filter(|message| message.contains(VERIFY_GATE_FEEDBACK))
        .count();
    assert_eq!(rounds, 2, "sent back twice, then stopped");
}

#[tokio::test]
async fn a_dead_code_warning_does_not_hold_the_turn() {
    // Warnings counted as failures, so any project carrying one `unused`
    // warning failed the gate after every edit. With one scripted finish, the
    // old gate would ask for a turn the mock does not have.
    let cwd = fixture_dir("verify-warning");
    std::fs::write(cwd.join("Cargo.toml"), MANIFEST).expect("a manifest to check");
    let (mut engine, sink) = engine(
        &cwd,
        vec![
            edit_turn("toolu_warn", "create", "src/lib.rs", WARNING_LIB),
            finish_turn(),
        ],
        Mechanisms::default(),
    );
    let summary = engine.turn("add the function".into()).await.unwrap();

    assert_eq!(summary.stop, Stop::Done);
    assert!(
        sink.user_messages()
            .iter()
            .all(|message| !message.contains(VERIFY_GATE_FEEDBACK)),
        "a warning sent the model back: {:?}",
        sink.user_messages()
    );
}

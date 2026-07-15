//! Hooks reach the loop: a `PreToolUse` block stops a tool from running at all,
//! and a `PostToolUse` hook's output reaches the model.
//!
//! The assertion that matters most here is the one about the *filesystem*. A
//! gate that reports a refusal but lets the write land is worse than no gate,
//! and it is indistinguishable from a working one if the test only reads the
//! journal. So the blocked call is an `edit`, and the test checks the file was
//! never written.

// Test-fixture fs (writing a temp repo to read from), not agent-driven work.
#![allow(clippy::disallowed_methods)]

use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};

use journal::{CallId, JournalSink, Outcome, Record, RecordKind, StopReason, Usage};
use provider::{MockProvider, ScriptedTurn, StreamEvent, ToolCallStart, TurnEnd};
use sandbox::Broker;
use summer_engine::{Engine, EngineBuilder, HookEvent, HookSet, Limits};
use tools::{Builtin, Dispatcher, ToolSource};

// ========================================================================
// Data Structures
// ========================================================================

#[derive(Clone, Default)]
struct CapturingSink {
    records: Arc<Mutex<Vec<Record>>>,
}

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

// ========================================================================
// Functions
// ========================================================================

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn fixture_dir(name: &str) -> std::path::PathBuf {
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("summer-hooks-{}-{name}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn engine(
    cwd: &std::path::Path,
    turns: Vec<ScriptedTurn>,
    hooks: HookSet,
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
    .hooks(hooks)
    .build()
    .unwrap();
    (engine, sink)
}

/// A turn that creates `path`, then a turn that ends.
fn edit_then_finish(path: &str) -> Vec<ScriptedTurn> {
    vec![
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
        },
        ScriptedTurn {
            events: vec![
                StreamEvent::TextDelta("done".into()),
                StreamEvent::BlockEnd,
                StreamEvent::TurnEnd(TurnEnd {
                    stop: StopReason::EndTurn,
                    usage: Usage::new(5, 1, 0, 0),
                }),
            ],
        },
    ]
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
async fn a_pre_tool_use_block_stops_the_write_from_happening() {
    let cwd = fixture_dir("blocked");
    let mut hooks = HookSet::default();
    hooks
        .add(
            HookEvent::PreToolUse,
            "echo 'the tree is frozen' >&2; exit 2".into(),
            Some("edit"),
        )
        .expect("`edit` compiles as a pattern");

    let (mut engine, sink) = engine(&cwd, edit_then_finish("new.txt"), hooks);
    engine.turn("write a file".into()).await.unwrap();

    // The gate held on disk, not just in the transcript.
    assert!(
        !cwd.join("new.txt").exists(),
        "a blocked edit still wrote the file: the hook is decorative"
    );

    let outcomes = results(&sink.records());
    assert_eq!(outcomes.len(), 1, "the call still got exactly one result");
    match &outcomes[0] {
        Outcome::Denied { reason } => {
            assert!(reason.contains("the tree is frozen"), "{reason}");
        }
        other => panic!("expected a denial carrying the hook's reason, got {other:?}"),
    }
}

#[tokio::test]
async fn a_hook_that_does_not_match_lets_the_call_through() {
    let cwd = fixture_dir("unmatched");
    let mut hooks = HookSet::default();
    hooks
        .add(HookEvent::PreToolUse, "exit 2".into(), Some("shell"))
        .expect("`shell` compiles as a pattern");

    let (mut engine, _sink) = engine(&cwd, edit_then_finish("allowed.txt"), hooks);
    engine.turn("write a file".into()).await.unwrap();

    assert!(
        cwd.join("allowed.txt").exists(),
        "a hook scoped to `shell` blocked an `edit`"
    );
}

#[tokio::test]
async fn post_tool_use_output_reaches_the_model() {
    let cwd = fixture_dir("annotated");
    let mut hooks = HookSet::default();
    hooks
        .add(
            HookEvent::PostToolUse,
            "echo 'rustfmt reformatted 1 file'".into(),
            None,
        )
        .expect("no pattern to compile");

    let (mut engine, sink) = engine(&cwd, edit_then_finish("formatted.txt"), hooks);
    engine.turn("write a file".into()).await.unwrap();

    let outcomes = results(&sink.records());
    match &outcomes[0] {
        Outcome::Ok { content } => {
            assert!(
                content.contains("rustfmt reformatted 1 file"),
                "the hook's output never reached the result: {content}"
            );
            assert!(
                content.contains("created"),
                "the tool's own result was replaced rather than annotated: {content}"
            );
        }
        other => panic!("expected an annotated success, got {other:?}"),
    }
}

#[tokio::test]
async fn no_hooks_means_the_loop_is_untouched() {
    // The default path: every existing session builds a `HookSet::default`, and
    // it must cost nothing and change nothing.
    let cwd = fixture_dir("bare");
    let (mut engine, sink) = engine(&cwd, edit_then_finish("plain.txt"), HookSet::default());
    engine.turn("write a file".into()).await.unwrap();

    assert!(cwd.join("plain.txt").exists());
    let outcomes = results(&sink.records());
    match &outcomes[0] {
        Outcome::Ok { content } => assert!(
            !content.contains("<hook>"),
            "an unhooked session grew hook markup: {content}"
        ),
        other => panic!("expected a plain success, got {other:?}"),
    }
}

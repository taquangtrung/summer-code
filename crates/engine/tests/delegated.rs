//! What a delegated sub-session spent reaches the session that spawned it.
//!
//! A subagent runs on an engine of its own and journals its turns to a file of
//! its own, so from the parent's journal those tokens never existed: the runaway
//! guard did not see them, the statusline did not show them, and a session that
//! delegated three times reported a fraction of what it cost. The tokens come
//! back on a channel, for the same reason the broker's permission records do:
//! the spending happens inside a `&self` tool call that cannot reach the
//! engine's `&mut self`.

// Test-fixture fs (staging a temp root), not agent-driven work.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};

use journal::{CallId, JournalSink, Record, RecordKind, StopReason, Usage};
use provider::{MockProvider, ScriptedTurn, StreamEvent, ToolCallStart, TurnEnd};
use sandbox::Broker;
use summer_engine::{DelegatedRun, Engine, EngineBuilder, Limits};
use tools::{Builtin, Dispatcher, ToolSource};

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
    fn delegated(&self) -> Vec<journal::Delegated> {
        self.records
            .lock()
            .expect("sink lock")
            .iter()
            .filter_map(|record| match &record.kind {
                RecordKind::Delegated(delegated) => Some(delegated.clone()),
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

fn engine() -> (Engine, CapturingSink, Sender<DelegatedRun>) {
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let cwd = std::env::temp_dir().join(format!("summer-delegated-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&cwd).expect("a temp root");

    let sink = CapturingSink::default();
    let (_decisions, permissions) = channel();
    let (spent, delegated) = channel();
    let engine = EngineBuilder::new(
        cwd.clone(),
        "system".into(),
        Box::new(MockProvider::new(vec![edit_turn(), finish_turn()])),
        Dispatcher::new(Builtin::new().tools()).expect("built-in names are unique"),
        Arc::new(Broker::permissive(cwd)),
        Box::new(sink.clone()),
        permissions,
    )
    .limits(Limits::defaults())
    .delegated(delegated)
    .build()
    .expect("an engine records its start");
    (engine, sink, spent)
}

/// A turn asking for one tool call, which is the batch a delegated run is
/// reported during.
fn edit_turn() -> ScriptedTurn {
    ScriptedTurn {
        events: vec![
            StreamEvent::ToolCallStart(ToolCallStart {
                call: CallId::new("toolu_call".into()),
                name: "edit".into(),
            }),
            StreamEvent::ToolCallDelta(
                r#"{"path":"note.txt","action":"create","content":"written"}"#.into(),
            ),
            StreamEvent::BlockEnd,
            StreamEvent::TurnEnd(TurnEnd {
                stop: StopReason::ToolUse,
                usage: Usage::new(10, 5, 0, 0),
            }),
        ],
    }
}

fn finish_turn() -> ScriptedTurn {
    ScriptedTurn {
        events: vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::BlockEnd,
            StreamEvent::TurnEnd(TurnEnd {
                stop: StopReason::EndTurn,
                usage: Usage::new(3, 2, 0, 0),
            }),
        ],
    }
}

// ========================================================================
// Tests
// ========================================================================

#[tokio::test]
async fn a_delegated_run_is_journaled_against_the_call_that_spawned_it() {
    let (mut engine, sink, spent) = engine();
    spent
        .send(DelegatedRun {
            journal: "/repo/.summer/sessions/sub.jsonl".into(),
            usage: Usage::new(900, 210, 0, 0),
        })
        .expect("the engine holds the receiver");

    engine.turn("delegate something".into()).await.unwrap();

    let delegated = sink.delegated();
    assert_eq!(delegated.len(), 1, "the run must reach the journal");
    assert_eq!(delegated[0].usage, Usage::new(900, 210, 0, 0));
    assert_eq!(
        delegated[0].call,
        CallId::new("toolu_call".into()),
        "the spend belongs to the call that caused it, not to the session at large"
    );
    assert_eq!(
        delegated[0].journal, "/repo/.summer/sessions/sub.jsonl",
        "the pointer to the sub-session's turns is what makes the total auditable"
    );
}

#[tokio::test]
async fn a_delegated_run_counts_toward_this_sessions_spend() {
    // The point of the whole exercise. A ceiling that ignored delegated tokens
    // would be a ceiling a session could walk through by delegating.
    let (mut engine, _sink, spent) = engine();
    spent
        .send(DelegatedRun {
            journal: "/repo/.summer/sessions/sub.jsonl".into(),
            usage: Usage::new(900, 210, 0, 0),
        })
        .expect("the engine holds the receiver");

    engine.turn("delegate something".into()).await.unwrap();

    let spend = engine.spent();
    assert_eq!(
        (spend.input, spend.output),
        (10 + 3 + 900, 5 + 2 + 210),
        "the session's own turns plus the sub-session's"
    );
}

#[tokio::test]
async fn a_session_that_delegates_nothing_is_unchanged() {
    let (mut engine, sink, _spent) = engine();
    engine.turn("do it yourself".into()).await.unwrap();

    assert!(sink.delegated().is_empty());
    let spend = engine.spent();
    assert_eq!((spend.input, spend.output), (13, 7));
}

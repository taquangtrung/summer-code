//! A session whose broker reports to someone else's journal.
//!
//! The `subagent` tool runs a sub-session against the parent's broker, because a
//! subagent is the same user doing the same work and should inherit the grants
//! already given. A broker has one decisions channel and the parent owns it, so
//! the sub-session's journal receives no `Permission` records at all.
//!
//! Every runtime property reads those records, so left on they report on every
//! call the sub-session made and none of the reports mean anything. The first
//! live subagent run printed a violation per tool call for exactly this reason.

// Test-fixture fs (staging a temp root), not agent-driven work.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::mpsc::channel;

use journal::{CallId, JournalSink, Record, StopReason, Usage};
use provider::{MockProvider, ScriptedTurn, StreamEvent, ToolCallStart, TurnEnd};
use sandbox::Broker;
use summer_engine::{Engine, EngineBuilder, Limits};
use tools::{Builtin, Dispatcher, ToolSource};

// ========================================================================
// Data Structures
// ========================================================================

/// Keeps nothing: these tests read the engine's violations, not the journal.
#[derive(Default)]
struct NullSink;

// ========================================================================
// Implementations
// ========================================================================

impl JournalSink for NullSink {
    fn append(&mut self, _record: &Record) -> std::io::Result<()> {
        Ok(())
    }
}

// ========================================================================
// Functions
// ========================================================================

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// An engine whose broker sends its decisions nowhere this engine can read,
/// which is the sub-session's situation exactly.
fn engine(audited_elsewhere: bool) -> Engine {
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let cwd = std::env::temp_dir().join(format!("summer-shared-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&cwd).expect("a temp root");

    let (_decisions, permissions) = channel();
    let mut builder = EngineBuilder::new(
        cwd.clone(),
        "system".into(),
        Box::new(MockProvider::new(vec![edit_turn(), finish_turn()])),
        Dispatcher::new(Builtin::new().tools()).expect("built-in names are unique"),
        Arc::new(Broker::permissive(cwd)),
        Box::new(NullSink),
        permissions,
    )
    .limits(Limits::defaults());
    if audited_elsewhere {
        builder = builder.audited_elsewhere();
    }
    builder.build().expect("an engine records its start")
}

fn edit_turn() -> ScriptedTurn {
    ScriptedTurn {
        events: vec![
            StreamEvent::ToolCallStart(ToolCallStart {
                call: CallId::new("toolu_edit".into()),
                name: "edit".into(),
            }),
            StreamEvent::ToolCallDelta(
                r#"{"path":"note.txt","action":"create","content":"written"}"#.into(),
            ),
            StreamEvent::BlockEnd,
            StreamEvent::TurnEnd(TurnEnd {
                stop: StopReason::ToolUse,
                usage: Usage::new(10, 1, 0, 0),
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
                usage: Usage::new(5, 1, 0, 0),
            }),
        ],
    }
}

// ========================================================================
// Tests
// ========================================================================

#[tokio::test]
async fn a_session_audited_elsewhere_reports_no_violations() {
    let mut engine = engine(true);
    engine.turn("write a file".into()).await.unwrap();

    assert!(
        engine.violations().is_empty(),
        "a sub-session flagged its own calls: {:?}",
        engine.violations()
    );
}

#[tokio::test]
async fn the_same_session_without_the_flag_still_flags_them() {
    // The other half: the flag has to be what suppresses the reports, not the
    // absence of anything to report. A property set that had quietly stopped
    // checking would pass the test above just as well.
    let mut engine = engine(false);
    engine.turn("write a file".into()).await.unwrap();

    assert!(
        !engine.violations().is_empty(),
        "the properties no longer notice a call with no permission record"
    );
}

//! Phase T0 gate: the engine emits a serializable `UiEvent` stream that projects
//! a running turn, and an engine with no observer is byte-for-byte unchanged.
//!
//! The narration is the enabling change for the TUI: a frontend that shows
//! tokens as they arrive, a spinner while a tool runs, and thinking as it is
//! produced cannot be built on a function that returns a single summary at the
//! end. This test fixes the exact stream a turn produces, headlessly, with no
//! terminal.
//!
//! The two invariants from the plan are asserted here: the stream is the exact
//! sequence a turn produces (so a reducer can fold it deterministically), and a
//! journal built without an observer is identical to one built before the stream
//! existed (so the plain REPL and every test are unaffected).

// Test-fixture fs (writing a temp repo to read from), not agent-driven work.
#![allow(clippy::disallowed_methods)]

use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};

use journal::{CallId, JournalSink, Record, RecordKind, StopReason, Usage};
use provider::{MockProvider, ScriptedTurn, StreamEvent, ToolCallStart, TurnEnd};
use sandbox::Broker;
use summer_engine::{EngineBuilder, Limits, Observer, Stop, UiEvent};
use tokio::sync::mpsc;
use tools::{Builtin, Dispatcher, ToolSource};

// ========================================================================
// Data Structures
// ========================================================================

/// A sink that keeps every record, so the no-observer baseline can be compared
/// against an earlier run byte-for-byte.
#[derive(Clone, Default)]
struct CapturingSink {
    records: Arc<Mutex<Vec<Record>>>,
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

fn fixture_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("summer-tui-t0-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("hello.txt"), "hello world").unwrap();
    dir
}

/// The turn the gate scripts: one round that streams text *then* asks for a tool
/// (so the narration order is text-before-tool), then a second round that simply
/// ends the turn with no further text. That second round carrying no text is what
/// keeps the streamed-text group singular, matching the gate's exact sequence.
fn scripted_turns() -> Vec<ScriptedTurn> {
    fn text(delta: &str) -> Vec<StreamEvent> {
        vec![StreamEvent::TextDelta(delta.into()), StreamEvent::BlockEnd]
    }
    fn tool_call(id: &str, args: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolCallStart(ToolCallStart {
                call: CallId::new(id.into()),
                name: "read".into(),
            }),
            StreamEvent::ToolCallDelta(args.into()),
            StreamEvent::BlockEnd,
        ]
    }
    fn end(stop: StopReason, usage: Usage) -> StreamEvent {
        StreamEvent::TurnEnd(TurnEnd { stop, usage })
    }

    let mut first = text("hello ");
    first.extend(tool_call("toolu_1", r#"{"path":"hello.txt"}"#));
    first.push(end(StopReason::ToolUse, Usage::new(100_000, 20_000, 0, 0)));
    // The second round ends the turn and produces no text, so the only
    // TextDelta group is the one before the tool.
    let second = vec![end(StopReason::EndTurn, Usage::new(100_000, 20_000, 0, 0))];
    vec![
        ScriptedTurn { events: first },
        ScriptedTurn { events: second },
    ]
}

/// Build an engine rooted at a temp fixture, optionally observing its turns.
fn build(name: &str, observer: Option<Observer>) -> (summer_engine::Engine, CapturingSink) {
    let sink = CapturingSink::default();
    let cwd = fixture_dir(name);
    let (_decisions, permissions) = channel();
    let mut builder = EngineBuilder::new(
        cwd.clone(),
        "system".into(),
        Box::new(MockProvider::new(scripted_turns())),
        Dispatcher::new(Builtin::new().tools()).expect("built-in names are unique"),
        Arc::new(Broker::permissive(cwd)),
        Box::new(sink.clone()),
        permissions,
    )
    .limits(Limits::defaults());
    if let Some(observer) = observer {
        builder = builder.observer(observer);
    }
    let engine = builder.build().unwrap();
    (engine, sink)
}

// ========================================================================
// Tests
// ========================================================================

#[tokio::test]
async fn an_observed_turn_emits_the_exact_event_sequence() {
    // The exact sequence a frontend folds: turn start, the streamed text, the
    // tool's pending-to-done transition, the usage accounting, and the end. Any
    // drift here is a regression a reducer could not absorb.
    let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();
    let (mut engine, _sink) = build("observed", Some(tx));

    let summary = engine.turn("read hello.txt".into()).await.unwrap();
    assert_eq!(summary.stop, Stop::Done);

    let events = drain(&mut rx);

    // The exact kind sequence the gate names. Payloads are spot-checked below so
    // a position shift is as loud as a missing event.
    let kinds: Vec<&str> = events.iter().map(kind).collect();
    assert_eq!(
        kinds,
        vec![
            "turn_started",
            "text_delta",
            "tool_started",
            "tool_finished",
            "usage",
            "turn_ended",
        ],
        "exact narration: {events:?}"
    );

    assert!(matches!(
        &events[0],
        UiEvent::TurnStarted { user } if user == "read hello.txt"
    ));
    assert!(matches!(&events[1], UiEvent::TextDelta { text } if text == "hello "));
    match &events[2] {
        UiEvent::ToolStarted { call, name, args } => {
            assert_eq!(call.as_str(), "toolu_1");
            assert_eq!(name, "read");
            assert_eq!(args, &serde_json::json!({"path": "hello.txt"}));
        }
        other => panic!("expected ToolStarted, got {other:?}"),
    }
    match &events[3] {
        UiEvent::ToolFinished { call, outcome } => {
            assert_eq!(call.as_str(), "toolu_1");
            assert!(
                matches!(outcome, journal::Outcome::Ok { content } if content.contains("hello world"))
            );
        }
        other => panic!("expected ToolFinished, got {other:?}"),
    }
    // The usage lands just before turn_ended, carrying the last round's usage
    // and the session's cumulative spend.
    match &events[4] {
        UiEvent::Usage {
            usage,
            total_tokens,
        } => {
            assert_eq!(
                *usage,
                Usage::new(100_000, 20_000, 0, 0),
                "usage is the last round's"
            );
            assert_eq!(*total_tokens, 240_000, "total_tokens is cumulative");
        }
        other => panic!("expected Usage, got {other:?}"),
    }
    assert!(matches!(
        &events[5],
        UiEvent::TurnEnded { stop: Stop::Done }
    ));
}

#[tokio::test]
async fn a_failed_turn_emits_failed_instead_of_turn_ended() {
    // A stream with no TurnEnd is a broken stream: the turn errors, and the
    // narration must announce the failure rather than a clean end. The wrapper
    // in `turn` is what guarantees this for every `Err` exit.
    let sink = CapturingSink::default();
    let cwd = fixture_dir("failed");
    let (_decisions, permissions) = channel();
    let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();
    let mut engine = EngineBuilder::new(
        cwd.clone(),
        "system".into(),
        Box::new(MockProvider::new(vec![ScriptedTurn {
            events: vec![StreamEvent::TextDelta("half a th".into())],
        }])),
        Dispatcher::new(Builtin::new().tools()).expect("unique names"),
        Arc::new(Broker::permissive(cwd)),
        Box::new(sink),
        permissions,
    )
    .limits(Limits::defaults())
    .observer(tx)
    .build()
    .unwrap();

    let error = engine.turn("hi".into()).await.unwrap_err();
    assert!(
        matches!(error, summer_engine::EngineError::Provider(_)),
        "expected a provider error, got {error:?}"
    );

    let events = drain(&mut rx);
    let kinds: Vec<&str> = events.iter().map(kind).collect();
    assert_eq!(
        kinds,
        vec!["turn_started", "text_delta", "failed"],
        "a broken stream narrates start, the partial text, then failure: {events:?}"
    );
}

#[tokio::test]
async fn an_engine_with_no_observer_is_unchanged() {
    // The plain REPL and every existing test build an engine with no observer.
    // It must produce the same journal as before the stream existed: no
    // narration leaks in, and the journal holds exactly the pre-TUI records.
    let (mut engine, sink) = build("silent", None);
    let summary = engine.turn("read hello.txt".into()).await.unwrap();
    assert_eq!(summary.stop, Stop::Done);

    let records = sink.records.lock().expect("sink lock").clone();
    let kinds: Vec<&str> = records
        .iter()
        .map(|record| match &record.kind {
            RecordKind::SessionStarted(_) => "session_started",
            RecordKind::UserMessage { .. } => "user_message",
            RecordKind::ModelTurn(_) => "model_turn",
            RecordKind::ToolCall(_) => "tool_call",
            RecordKind::ToolResult(_) => "tool_result",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "session_started",
            "user_message",
            "model_turn",
            "tool_call",
            "tool_result",
            "model_turn",
        ],
        "the no-observer journal is the pre-TUI journal"
    );
}

fn kind(event: &UiEvent) -> &'static str {
    match event {
        UiEvent::TurnStarted { .. } => "turn_started",
        UiEvent::TextDelta { .. } => "text_delta",
        UiEvent::ThinkingDelta { .. } => "thinking_delta",
        UiEvent::TurnReset { .. } => "turn_reset",
        UiEvent::ToolStarted { .. } => "tool_started",
        UiEvent::ToolFinished { .. } => "tool_finished",
        UiEvent::Delegated { .. } => "delegated",
        UiEvent::Usage { .. } => "usage",
        UiEvent::TurnEnded { .. } => "turn_ended",
        UiEvent::Failed { .. } => "failed",
    }
}

/// Drain every buffered event after a turn. The engine sends synchronously on an
/// unbounded channel, so once `turn` returns every event is already buffered.
fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<UiEvent>) -> Vec<UiEvent> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        out.push(event);
    }
    out
}

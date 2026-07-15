//! The loop closes end to end without a live model, and stops the way it is
//! supposed to.
//!
//! A scripted `MockProvider` drives the turns. Real tools, real broker, real
//! dispatcher, real journal sink: only the model is mocked, which is the point.
//!
//! The assertion that carries the most weight here is
//! [`assert_every_call_answered`]. The API requires every `tool_use` block to be
//! answered by a `tool_result` in the next user message, and an unanswered one
//! is a 400 on the *next* request: not "the tool did not run" but "the session
//! is bricked, including on resume". Every stopping path therefore gets checked
//! against it, because the paths that stop early are exactly the ones that
//! forget.

// Test-fixture fs (writing a temp repo to read from), not agent-driven work.
#![allow(clippy::disallowed_methods)]

use std::collections::HashSet;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};

use journal::{CallId, JournalSink, Outcome, Record, RecordKind, StopReason, Usage};
use provider::{MockProvider, ScriptedTurn, StreamEvent, ThinkingDelta, ToolCallStart, TurnEnd};
use sandbox::Broker;
use summer_engine::{Engine, EngineBuilder, EngineError, Limits, Stop};
use tools::{Builtin, Dispatcher, ToolSource};

// ========================================================================
// Data Structures
// ========================================================================

/// A sink that keeps every record, so a test can assert on the journal the run
/// actually produced rather than on the engine's return value alone.
#[derive(Clone, Default)]
struct CapturingSink {
    records: Arc<Mutex<Vec<Record>>>,
}

// ========================================================================
// CapturingSink Implementations
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

fn fixture_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("summer-loop-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("hello.txt"), "hello world").unwrap();
    dir
}

fn engine(name: &str, turns: Vec<ScriptedTurn>, limits: Limits) -> (Engine, CapturingSink) {
    let sink = CapturingSink::default();
    let cwd = fixture_dir(name);
    // These tests are about the loop, so the broker is permissive: nothing here
    // should stop on a policy decision. The adversarial suite is where the
    // policy is the subject.
    let (_decisions, permissions) = channel();
    let engine = EngineBuilder::new(
        cwd.clone(),
        "system".into(),
        Box::new(MockProvider::new(turns)),
        Dispatcher::new(Builtin::new().tools()).expect("built-in names are unique"),
        Arc::new(Broker::permissive(cwd)),
        Box::new(sink.clone()),
        permissions,
    )
    .limits(limits)
    .build()
    .unwrap();
    (engine, sink)
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

/// Flatten scripted event groups into one turn. `StreamEvent` is deliberately
/// not `Clone`, so `concat` is unavailable.
fn chain<const N: usize>(groups: [Vec<StreamEvent>; N]) -> Vec<StreamEvent> {
    groups.into_iter().flatten().collect()
}

/// Every `ToolCall` in the journal has a matching `ToolResult`.
fn assert_every_call_answered(records: &[Record]) {
    let mut called = Vec::new();
    let mut answered = HashSet::new();
    for record in records {
        match &record.kind {
            RecordKind::ToolCall(call) => called.push(call.call.clone()),
            RecordKind::ToolResult(result) => {
                answered.insert(result.call.clone());
            }
            _ => {}
        }
    }
    assert!(!called.is_empty(), "no tool calls were journaled");
    for call in &called {
        assert!(
            answered.contains(call),
            "tool_use `{call}` was never answered: the next request is a 400"
        );
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

#[test]
fn engine_is_send() {
    // Asserted rather than assumed: an `!Send` engine forces a `LocalSet` and
    // `spawn_local`, and every `Rc` that accumulates behind that choice becomes
    // load-bearing. Cheap to keep, expensive to recover, and it regresses
    // silently.
    fn assert_send<T: Send>() {}
    assert_send::<Engine>();
}

#[tokio::test]
async fn loop_closes_read_then_done() {
    let (mut engine, sink) = engine(
        "closes",
        vec![
            ScriptedTurn {
                events: chain([
                    tool_call("toolu_1", r#"{"path":"hello.txt"}"#),
                    vec![end(StopReason::ToolUse, Usage::new(10, 0, 0, 0))],
                ]),
            },
            ScriptedTurn {
                events: vec![
                    StreamEvent::TextDelta("done".into()),
                    StreamEvent::BlockEnd,
                    end(StopReason::EndTurn, Usage::new(12, 1, 0, 0)),
                ],
            },
        ],
        Limits::defaults(),
    );

    let summary = engine.turn("read hello.txt".into()).await.unwrap();
    assert_eq!(summary.stop, Stop::Done);
    assert_eq!(summary.text, "done");
    assert_eq!(summary.tool_calls, 1);

    let records = sink.records();
    assert_every_call_answered(&records);
    assert!(
        matches!(results(&records).as_slice(), [Outcome::Ok { content }] if content.contains("hello world")),
        "the read tool did not actually run"
    );
}

#[tokio::test]
async fn tool_call_limit_answers_every_outstanding_call() {
    // The limit cannot be known before the turn arrives, so it fires mid-turn.
    // Every `tool_use` must still be answered, or the session is bricked.
    let (mut engine, sink) = engine(
        "tool-limit",
        vec![ScriptedTurn {
            events: chain([
                tool_call("toolu_1", r#"{"path":"hello.txt"}"#),
                tool_call("toolu_2", r#"{"path":"hello.txt"}"#),
                tool_call("toolu_3", r#"{"path":"hello.txt"}"#),
                vec![end(StopReason::ToolUse, Usage::new(10, 0, 0, 0))],
            ]),
        }],
        Limits {
            max_tool_calls: 2,
            ..Limits::defaults()
        },
    );

    let summary = engine.turn("read it thrice".into()).await.unwrap();
    assert_eq!(
        summary.stop,
        Stop::HitLimit(summer_engine::LimitKind::ToolCalls)
    );

    let records = sink.records();
    assert_every_call_answered(&records);
    let results = results(&records);
    assert_eq!(results.len(), 3, "all three calls must be answered");
    assert!(
        results
            .iter()
            .all(|outcome| matches!(outcome, Outcome::Err { .. })),
        "un-dispatched calls get a synthetic error so the model sees why it stopped"
    );
}

#[tokio::test]
async fn token_limit_stops_at_the_top_leaving_no_call_unanswered() {
    // The check sits before a turn is generated. Firing it after the model has
    // asked for tools would leave them unanswered.
    let (mut engine, sink) = engine(
        "token-limit",
        vec![ScriptedTurn {
            events: chain([
                tool_call("toolu_1", r#"{"path":"hello.txt"}"#),
                vec![end(StopReason::ToolUse, Usage::new(5_000, 0, 0, 0))],
            ]),
        }],
        Limits {
            max_tokens: 100,
            ..Limits::defaults()
        },
    );

    let summary = engine.turn("read it".into()).await.unwrap();
    assert_eq!(
        summary.stop,
        Stop::HitLimit(summer_engine::LimitKind::Tokens)
    );
    assert_every_call_answered(&sink.records());
}

#[tokio::test]
async fn a_token_limit_stops_the_loop() {
    // The first turn asks for a tool, so the loop comes back round to the top
    // and reads the tokens it just ran up. 100k output tokens blows past a
    // ceiling of one, so the runaway guard stops the loop.
    let (mut engine, sink) = engine(
        "token-limit",
        vec![ScriptedTurn {
            events: chain([
                tool_call("toolu_1", r#"{"path":"hello.txt"}"#),
                vec![end(StopReason::ToolUse, Usage::new(0, 100_000, 0, 0))],
            ]),
        }],
        Limits {
            max_tokens: 1,
            ..Limits::defaults()
        },
    );

    let summary = engine.turn("spend".into()).await.unwrap();
    assert_eq!(
        summary.stop,
        Stop::HitLimit(summer_engine::LimitKind::Tokens)
    );
    assert_every_call_answered(&sink.records());
}

#[tokio::test]
async fn a_limit_is_ok_not_err() {
    // A ceiling is the loop doing what it was told. Returning Err would make
    // every limit look like a crash to the CLI.
    let (mut engine, _sink) = engine(
        "limit-ok",
        vec![ScriptedTurn {
            events: vec![end(StopReason::EndTurn, Usage::zero())],
        }],
        Limits {
            max_turns: 0,
            ..Limits::defaults()
        },
    );
    let summary = engine
        .turn("hi".into())
        .await
        .expect("a limit is not an error");
    assert_eq!(
        summary.stop,
        Stop::HitLimit(summer_engine::LimitKind::Turns)
    );
}

#[tokio::test]
async fn refusal_is_an_answer_not_an_error() {
    let (mut engine, _sink) = engine(
        "refusal",
        vec![ScriptedTurn {
            events: vec![end(
                StopReason::Refusal {
                    category: Some("harmful".into()),
                },
                Usage::zero(),
            )],
        }],
        Limits::defaults(),
    );

    let summary = engine.turn("do something".into()).await.unwrap();
    assert_eq!(
        summary.stop,
        Stop::Refused {
            category: Some("harmful".into())
        }
    );
}

#[tokio::test]
async fn truncated_output_is_an_error_not_a_completed_turn() {
    let (mut engine, _sink) = engine(
        "truncated",
        vec![ScriptedTurn {
            events: vec![
                StreamEvent::TextDelta("half a thou".into()),
                StreamEvent::BlockEnd,
                end(StopReason::MaxTokens, Usage::new(10, 64_000, 0, 0)),
            ],
        }],
        Limits::defaults(),
    );

    let error = engine.turn("write a book".into()).await.unwrap_err();
    assert!(
        matches!(error, EngineError::Truncated),
        "max_tokens must not read as a finished turn, got {error:?}"
    );
}

#[tokio::test]
async fn a_truncated_turn_still_answers_the_calls_it_managed_to_ask_for() {
    // `max_tokens` does not always cut before the tool blocks. The model can
    // finish one `tool_use`, start another, and hit the ceiling: the turn is
    // truncated and the first call is complete and unanswered. Returning
    // `Truncated` without answering it leaves the turn in history with a
    // `tool_use` and no `tool_result`, so the caller's obvious next move (raise
    // `max_tokens`, call `turn` again) is a 400, and so is every resume, because
    // the fold reads back the history just recorded.
    let (mut engine, sink) = engine(
        "truncated-with-calls",
        vec![ScriptedTurn {
            events: chain([
                tool_call("toolu_cut", r#"{"path":"a.txt"}"#),
                vec![end(StopReason::MaxTokens, Usage::new(10, 64_000, 0, 0))],
            ]),
        }],
        Limits::defaults(),
    );

    let error = engine.turn("write a book".into()).await.unwrap_err();
    assert!(
        matches!(error, EngineError::Truncated),
        "max_tokens must not read as a finished turn, got {error:?}"
    );
    assert_every_call_answered(&sink.records());
}

#[tokio::test]
async fn a_stream_without_turn_end_never_becomes_a_model_turn() {
    // The provider retries a broken stream and, having exhausted its attempts,
    // yields no TurnEnd. Journaling a ModelTurn here would record a turn the
    // model never finished.
    let (mut engine, sink) = engine(
        "broken",
        vec![ScriptedTurn {
            events: vec![StreamEvent::TextDelta("half a th".into())],
        }],
        Limits::defaults(),
    );

    let error = engine.turn("hi".into()).await.unwrap_err();
    assert!(
        matches!(
            error,
            EngineError::Provider(provider::ProviderError::StreamBroken(_))
        ),
        "expected StreamBroken, got {error:?}"
    );
    assert!(
        !sink
            .records()
            .iter()
            .any(|record| matches!(record.kind, RecordKind::ModelTurn(_))),
        "a truncated stream must not journal a ModelTurn"
    );
}

#[tokio::test]
async fn a_restarted_turn_discards_the_dead_attempt() {
    // A mid-stream failure re-sends the turn whole. The blocks from the attempt
    // that died must not survive into the journal: a ModelTurn stitched from two
    // attempts is a turn the model never produced, and on the next request its
    // duplicated tool_use ids are a 400.
    let (mut engine, sink) = engine(
        "restart",
        vec![ScriptedTurn {
            events: chain([
                vec![StreamEvent::TextDelta("first att".into())],
                tool_call("toolu_dead", r#"{"path":"hello.txt"}"#),
                vec![StreamEvent::TurnRestarted],
                vec![StreamEvent::TextDelta("second attempt".into())],
                vec![StreamEvent::BlockEnd],
                vec![end(StopReason::EndTurn, Usage::new(10, 5, 0, 0))],
            ]),
        }],
        Limits::defaults(),
    );

    let summary = engine.turn("hi".into()).await.unwrap();
    assert_eq!(summary.stop, Stop::Done);
    assert_eq!(
        summary.text, "second attempt",
        "the dead attempt's text leaked into the turn"
    );

    let records = sink.records();
    let turn = records
        .iter()
        .find_map(|record| match &record.kind {
            RecordKind::ModelTurn(turn) => Some(turn),
            _ => None,
        })
        .expect("a ModelTurn was journaled");
    assert_eq!(
        turn.blocks.len(),
        1,
        "expected only the surviving attempt's block, got {:?}",
        turn.blocks
    );
    assert!(
        !turn
            .blocks
            .iter()
            .any(|block| matches!(block, journal::Block::ToolUse(_))),
        "the dead attempt's tool_use survived the restart"
    );
    assert_eq!(
        summary.tool_calls, 0,
        "a discarded tool_use must not dispatch"
    );
}

#[tokio::test]
async fn thinking_blocks_reach_the_journal_with_their_signature() {
    // A journal that dropped these could not replay the turn, and resume would
    // rebuild a request the API refuses.
    let (mut engine, sink) = engine(
        "thinking",
        vec![ScriptedTurn {
            events: vec![
                StreamEvent::ThinkingDelta(ThinkingDelta {
                    signature: None,
                    text: Some("let me think".into()),
                }),
                StreamEvent::ThinkingDelta(ThinkingDelta {
                    signature: Some("EqoBCkgIAx==".into()),
                    text: None,
                }),
                StreamEvent::BlockEnd,
                end(StopReason::EndTurn, Usage::zero()),
            ],
        }],
        Limits::defaults(),
    );

    engine.turn("think".into()).await.unwrap();

    let records = sink.records();
    let turn = records
        .iter()
        .find_map(|record| match &record.kind {
            RecordKind::ModelTurn(turn) => Some(turn),
            _ => None,
        })
        .expect("a ModelTurn was journaled");
    let thinking = turn
        .blocks
        .iter()
        .find_map(|block| match block {
            journal::Block::Thinking(thinking) => Some(thinking),
            _ => None,
        })
        .expect("the thinking block survived");
    assert_eq!(thinking.signature, "EqoBCkgIAx==");
    assert_eq!(thinking.text, "let me think");
}

#[tokio::test]
async fn the_journaled_turn_records_why_it_stopped() {
    let (mut engine, sink) = engine(
        "stop-recorded",
        vec![ScriptedTurn {
            events: vec![end(StopReason::EndTurn, Usage::zero())],
        }],
        Limits::defaults(),
    );
    engine.turn("hi".into()).await.unwrap();

    let records = sink.records();
    let turn = records
        .iter()
        .find_map(|record| match &record.kind {
            RecordKind::ModelTurn(turn) => Some(turn),
            _ => None,
        })
        .expect("a ModelTurn was journaled");
    assert_eq!(turn.stop, StopReason::EndTurn);
}

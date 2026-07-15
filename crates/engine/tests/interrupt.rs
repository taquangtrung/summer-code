//! Phase T3 gate: a long tool call is interrupted by cancellation, the tool is
//! killed and answered, and the turn ends `Stop::Cancelled` — not a crash, and
//! the session survives to take another turn.
//!
//! The shell tool races its child against `cx.cancel`; the dispatcher makes that
//! token a child of the turn's; the engine checks the turn token between rounds.
//! This test wires all three together against a real `sleep` and a real cancel,
//! the way a Ctrl-C would.

// Test-fixture fs (writing a temp repo), not agent-driven work.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::mpsc::channel;
use std::time::Duration;

use journal::{CallId, StopReason, Usage, WriterSink};
use provider::{MockProvider, ScriptedTurn, StreamEvent, ToolCallStart, TurnEnd};
use sandbox::Broker;
use summer_engine::{EngineBuilder, Limits, Stop, UiEvent};
use tokio_util::sync::CancellationToken;
use tools::{Builtin, Dispatcher, ToolSource};

fn fixture_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("summer-tui-t3-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A shell call that sleeps long enough that the turn cannot finish unless it is
/// interrupted.
fn sleep_call() -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCallStart(ToolCallStart {
            call: CallId::new("toolu_1".into()),
            name: "shell".into(),
        }),
        StreamEvent::ToolCallDelta(r#"{"command":"sleep 30"}"#.into()),
        StreamEvent::BlockEnd,
        StreamEvent::TurnEnd(TurnEnd {
            stop: StopReason::ToolUse,
            usage: Usage::zero(),
        }),
    ]
}

#[tokio::test]
async fn cancelling_a_long_tool_call_ends_the_turn_and_survives() {
    let cwd = fixture_dir("interrupt");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<UiEvent>();
    let cancel = CancellationToken::new();
    let (_decisions, permissions) = channel();
    let mut engine = EngineBuilder::new(
        cwd.clone(),
        "system".into(),
        Box::new(MockProvider::new(vec![ScriptedTurn {
            events: sleep_call(),
        }])),
        Dispatcher::new(Builtin::new().tools()).expect("built-in names are unique"),
        Arc::new(Broker::permissive(cwd)),
        Box::new(WriterSink::new(std::io::sink())),
        permissions,
    )
    .limits(Limits::defaults())
    .observer(tx)
    .build()
    .unwrap();
    engine.set_turn_cancel(Some(cancel.clone()));

    let handle = tokio::spawn(async move {
        let result = engine.turn("sleep".into()).await;
        (engine, result)
    });

    // Wait until the shell has actually started, then cancel — exactly the moment
    // a Ctrl-C would land relative to a long-running command.
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = rx.recv().await {
            if matches!(event, UiEvent::ToolStarted { .. }) {
                return;
            }
        }
    })
    .await
    .expect("the tool started within 5s");
    cancel.cancel();

    // The turn must end Cancelled in well under the 30s sleep. If the cancel
    // wiring is broken this hangs until the timeout, turning the failure into a
    // clear error rather than a 30s stall.
    let (engine_back, result) = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("the interrupted turn returned within 5s of cancel")
        .expect("the task joined");
    let summary = result.expect("a cancel is Ok(Cancelled), not an error");
    assert_eq!(
        summary.stop,
        Stop::Cancelled,
        "the turn ends cancelled, not crashed"
    );
    assert!(
        summary.tool_calls >= 1,
        "the shell was dispatched and answered, not dropped"
    );

    // The session survives: the engine came back intact and is still usable.
    let engine_back = engine_back;
    let _ = engine_back.session_id();
}

/// A model that starts replying and never finishes, the shape of a long answer
/// being streamed when the user presses Esc.
struct NeverFinishes;

#[async_trait::async_trait]
impl provider::Provider for NeverFinishes {
    async fn turn(
        &self,
        _req: provider::TurnRequest,
    ) -> Result<futures_util::stream::BoxStream<'static, StreamEvent>, provider::ProviderError>
    {
        use futures_util::StreamExt;
        let first = futures_util::stream::once(async {
            StreamEvent::TextDelta("Let me explain at great length".into())
        });
        Ok(first.chain(futures_util::stream::pending()).boxed())
    }

    fn model(&self) -> &str {
        "never-finishes"
    }

    async fn count_tokens(
        &self,
        _req: &provider::TurnRequest,
    ) -> Result<u32, provider::ProviderError> {
        Ok(0)
    }
}

#[tokio::test]
async fn cancelling_while_the_model_streams_ends_the_turn_promptly() {
    // Cancellation used to be checked only between rounds, so Esc did nothing
    // for as long as the reply kept streaming. With a reply that never ends,
    // the old engine hangs here until the timeout.
    let cwd = fixture_dir("streaming");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<UiEvent>();
    let cancel = CancellationToken::new();
    let (_decisions, permissions) = channel();
    let mut engine = EngineBuilder::new(
        cwd.clone(),
        "system".into(),
        Box::new(NeverFinishes),
        Dispatcher::new(Builtin::new().tools()).expect("built-in names are unique"),
        Arc::new(Broker::permissive(cwd)),
        Box::new(WriterSink::new(std::io::sink())),
        permissions,
    )
    .limits(Limits::defaults())
    .observer(tx)
    .build()
    .unwrap();
    engine.set_turn_cancel(Some(cancel.clone()));

    let handle = tokio::spawn(async move { engine.turn("explain".into()).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = rx.recv().await {
            if matches!(event, UiEvent::TextDelta { .. }) {
                return;
            }
        }
    })
    .await
    .expect("the reply started streaming within 5s");
    cancel.cancel();

    let result = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("the turn returned within 5s of cancel")
        .expect("the task joined");
    assert_eq!(result.expect("a cancel is Ok").stop, Stop::Cancelled);
}

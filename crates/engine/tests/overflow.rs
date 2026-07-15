//! A provider that refuses a request for overflowing the context window gets
//! one compaction and one retry, instead of a session stuck sending the same
//! too-large history every turn.

// Test-fixture fs (staging a temp root), not agent-driven work.
#![allow(clippy::disallowed_methods)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use journal::{RecordKind, StopReason, Usage, WriterSink, read_records};
use provider::{
    MockProvider, Provider, ProviderError, ScriptedTurn, StreamEvent, TurnBlock, TurnEnd,
    TurnRequest,
};
use sandbox::Broker;
use summer_engine::{ContextManager, EngineBuilder, Limits, Stop};
use tools::{Builtin, Dispatcher, ToolSource};

// ========================================================================
// Data Structures
// ========================================================================

/// A [`MockProvider`] that refuses the request numbered `refuse` (0-based) with
/// `message`, as a server rejecting the window would, and answers the rest.
struct Refusing {
    calls: Arc<AtomicUsize>,
    inner: MockProvider,
    message: &'static str,
    refuse: usize,
    /// For each request, whether it still carried the first turn's answer.
    seen: Arc<Mutex<Vec<bool>>>,
}

// ========================================================================
// Refusing Implementations
// ========================================================================

#[async_trait]
impl Provider for Refusing {
    fn model(&self) -> &str {
        "mock"
    }

    async fn turn(
        &self,
        req: TurnRequest,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::Relaxed);
        let carries_first_answer = req.messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, TurnBlock::Text(text) if text == "first answer"))
        });
        self.seen.lock().expect("lock").push(carries_first_answer);
        if call == self.refuse {
            return Err(ProviderError::Fatal {
                kind: "invalid_request_error".into(),
                message: self.message.into(),
                status: Some(400),
            });
        }
        self.inner.turn(req).await
    }

    async fn count_tokens(&self, req: &TurnRequest) -> Result<u32, ProviderError> {
        self.inner.count_tokens(req).await
    }
}

// ========================================================================
// Functions
// ========================================================================

fn text_turn(text: &str) -> ScriptedTurn {
    ScriptedTurn {
        events: vec![
            StreamEvent::TextDelta(text.into()),
            StreamEvent::BlockEnd,
            StreamEvent::TurnEnd(TurnEnd {
                stop: StopReason::EndTurn,
                usage: Usage::zero(),
            }),
        ],
    }
}

// ========================================================================
// Tests
// ========================================================================

#[tokio::test]
async fn an_overflow_refusal_compacts_once_and_the_turn_goes_through() {
    let cwd = std::env::temp_dir().join(format!("summer-overflow-{}", std::process::id()));
    std::fs::create_dir_all(&cwd).expect("a temp root");
    let journal_path = cwd.join("session.jsonl");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (_decisions, permissions) = channel();
    let mut engine = EngineBuilder::new(
        cwd.clone(),
        "system".into(),
        Box::new(Refusing {
            calls: Arc::new(AtomicUsize::new(0)),
            inner: MockProvider::new(vec![text_turn("first answer"), text_turn("second answer")]),
            message: r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#,
            refuse: 1,
            seen: seen.clone(),
        }),
        Dispatcher::new(Builtin::new().tools()).expect("unique names"),
        Arc::new(Broker::permissive(cwd.clone())),
        Box::new(WriterSink::new(
            std::fs::File::create(&journal_path).expect("a journal"),
        )),
        permissions,
    )
    .limits(Limits::defaults())
    // Keep one recent entry, so the first answer is the droppable middle.
    .context(ContextManager::new(1_000_000, 1))
    .build()
    .expect("an engine");

    engine.turn("first".into()).await.expect("the first turn");
    let summary = engine
        .turn("second".into())
        .await
        .expect("the refused turn is compacted and retried, not failed");
    assert_eq!(summary.stop, Stop::Done);
    assert_eq!(summary.text, "second answer");

    assert_eq!(
        seen.lock().expect("lock").as_slice(),
        [false, true, false],
        "the refused request carried the first answer, and the retry had it compacted away"
    );

    drop(engine);
    let records = read_records(std::io::BufReader::new(
        std::fs::File::open(&journal_path).expect("the journal"),
    ))
    .expect("records");
    assert!(
        records
            .iter()
            .any(|record| matches!(record.kind, RecordKind::Compacted(_))),
        "the compaction is journaled, so a resume folds to the same history"
    );
}

#[test]
fn only_an_overflow_counts_as_one() {
    let fatal = |message: &str| ProviderError::Fatal {
        kind: "invalid_request_error".into(),
        message: message.into(),
        status: Some(400),
    };
    assert!(fatal("prompt is too long: 210000 tokens > 200000 maximum").is_context_overflow());
    assert!(
        fatal(r#"{"error":{"code":"context_length_exceeded"}}"#).is_context_overflow(),
        "OpenAI's code"
    );
    assert!(!fatal("max_tokens: must be at most 64000").is_context_overflow());
    assert!(!ProviderError::Overloaded.is_context_overflow());
}

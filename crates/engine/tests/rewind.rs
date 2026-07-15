//! Rewinding the conversation: the dropped turns leave what the model sees
//! next, and a resume of the journal folds to the same rewound history.

// Test-fixture fs (a journal file to read back), not agent-driven work.
#![allow(clippy::disallowed_methods)]

use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use journal::{StopReason, Usage, WriterSink, read_records};
use provider::{
    MockProvider, Provider, ProviderError, ScriptedTurn, StreamEvent, TurnBlock, TurnEnd,
    TurnRequest,
};
use sandbox::Broker;
use summer_engine::{EngineBuilder, Limits};
use tools::{Builtin, Dispatcher, ToolSource};

// ========================================================================
// Data Structures
// ========================================================================

/// A [`MockProvider`] that keeps the text of every request it was sent.
struct Recording {
    inner: MockProvider,
    sent: Arc<Mutex<Vec<String>>>,
}

// ========================================================================
// Recording Implementations
// ========================================================================

#[async_trait]
impl Provider for Recording {
    fn model(&self) -> &str {
        "mock"
    }

    async fn turn(
        &self,
        req: TurnRequest,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let text: Vec<String> = req
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                TurnBlock::Text(text) => Some(text.clone()),
                _ => None,
            })
            .collect();
        self.sent.lock().expect("lock").push(text.join("\n"));
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
async fn a_rewound_turn_is_gone_from_the_next_request_and_from_a_resume() {
    let cwd = std::env::temp_dir().join(format!("summer-rewind-{}", std::process::id()));
    std::fs::create_dir_all(&cwd).expect("a temp root");
    let journal_path = cwd.join("session.jsonl");
    let sent = Arc::new(Mutex::new(Vec::new()));
    let (_decisions, permissions) = channel();
    let mut engine = EngineBuilder::new(
        cwd.clone(),
        "system".into(),
        Box::new(Recording {
            inner: MockProvider::new(vec![
                text_turn("answer one"),
                text_turn("answer two"),
                text_turn("answer three"),
            ]),
            sent: sent.clone(),
        }),
        Dispatcher::new(Builtin::new().tools()).expect("unique names"),
        Arc::new(Broker::permissive(cwd.clone())),
        Box::new(WriterSink::new(
            std::fs::File::create(&journal_path).expect("a journal"),
        )),
        permissions,
    )
    .limits(Limits::defaults())
    .build()
    .expect("an engine");

    engine.turn("ask one".into()).await.expect("turn one");
    engine.turn("ask two".into()).await.expect("turn two");
    assert_eq!(engine.rewindable_turns(), ["ask two", "ask one"]);

    assert!(
        engine.rewind_turns(1).expect("journaled"),
        "one turn to drop"
    );
    assert_eq!(engine.rewindable_turns(), ["ask one"]);
    assert!(
        !engine.rewind_turns(5).expect("no-op"),
        "only one turn is left"
    );

    engine
        .turn("ask two, better".into())
        .await
        .expect("turn three");
    let last = sent
        .lock()
        .expect("lock")
        .last()
        .cloned()
        .unwrap_or_default();
    assert!(
        last.contains("ask one") && last.contains("answer one"),
        "{last}"
    );
    assert!(
        !last.contains("ask two\n") && !last.contains("answer two"),
        "{last}"
    );
    assert!(last.contains("rewound"), "the model is told why: {last}");

    // A resume folds the journal, and must land on the same rewound history.
    drop(engine);
    let bytes = std::fs::read(&journal_path).expect("the journal");
    let head = read_records(std::io::Cursor::new(&bytes))
        .expect("records")
        .last()
        .expect("a record")
        .id;
    let resent = Arc::new(Mutex::new(Vec::new()));
    let (_decisions, permissions) = channel();
    let mut resumed = EngineBuilder::new(
        cwd.clone(),
        "system".into(),
        Box::new(Recording {
            inner: MockProvider::new(vec![text_turn("answer four")]),
            sent: resent.clone(),
        }),
        Dispatcher::new(Builtin::new().tools()).expect("unique names"),
        Arc::new(Broker::permissive(cwd.clone())),
        Box::new(WriterSink::new(std::io::sink())),
        permissions,
    )
    .limits(Limits::defaults())
    .resume(std::io::Cursor::new(&bytes), head)
    .expect("the journal folds");
    resumed.turn("ask four".into()).await.expect("turn four");
    let folded = resent
        .lock()
        .expect("lock")
        .last()
        .cloned()
        .unwrap_or_default();
    assert!(!folded.contains("answer two"), "{folded}");
    assert!(folded.contains("answer three"), "{folded}");
}

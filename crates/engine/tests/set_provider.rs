//! `Engine::set_provider` actually changes what the *next* turn talks to, not
//! just what the call itself accepted.
//!
//! **The journal is not the assertion surface here.** `ModelTurn` carries
//! `blocks`/`stop`/`usage` and no model id at all; only `SessionStarted`
//! names one, once, at session start. A swap is not yet its own journal
//! record (see `Engine::set_provider`'s own doc), so the only thing that
//! actually reflects a swap is `Engine::model()` read live, which is what a
//! frontend's `/model` picker and statusline both do. Asserting against the
//! journal instead would test a claim this crate does not make.

// Test-fixture fs (writing a temp repo to read from), not agent-driven work.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::mpsc::channel;

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use journal::{StopReason, Usage, WriterSink};
use provider::{
    MockProvider, Provider, ProviderError, ScriptedTurn, StreamEvent, TurnEnd, TurnRequest,
};
use sandbox::Broker;
use summer_engine::{EngineBuilder, Limits, Stop};
use tools::{Builtin, Dispatcher, ToolSource};

// ========================================================================
// Data Structures
// ========================================================================

/// A [`MockProvider`] wearing a chosen model id. `MockProvider::model` always
/// answers `"mock"`, so proving a swap actually took effect (not just that
/// the call didn't panic) needs two providers a test can tell apart.
struct NamedMock {
    inner: MockProvider,
    name: &'static str,
}

// ========================================================================
// NamedMock Implementations
// ========================================================================

#[async_trait]
impl Provider for NamedMock {
    fn model(&self) -> &str {
        self.name
    }

    async fn turn(
        &self,
        req: TurnRequest,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        self.inner.turn(req).await
    }

    async fn count_tokens(&self, req: &TurnRequest) -> Result<u32, ProviderError> {
        self.inner.count_tokens(req).await
    }
}

// ========================================================================
// Functions
// ========================================================================

fn end_turn() -> ScriptedTurn {
    ScriptedTurn {
        events: vec![StreamEvent::TurnEnd(TurnEnd {
            stop: StopReason::EndTurn,
            usage: Usage::zero(),
        })],
    }
}

fn fixture_dir(name: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("summer-set-provider-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ========================================================================
// Tests
// ========================================================================

#[tokio::test]
async fn a_swapped_provider_answers_the_next_turn_and_reports_its_own_model() {
    let cwd = fixture_dir("swap");
    let (_decisions, permissions) = channel();
    let mut engine = EngineBuilder::new(
        cwd.clone(),
        "system".into(),
        Box::new(NamedMock {
            inner: MockProvider::new(vec![end_turn()]),
            name: "first-model",
        }),
        Dispatcher::new(Builtin::new().tools()).expect("built-in names are unique"),
        Arc::new(Broker::permissive(cwd)),
        Box::new(WriterSink::new(std::io::sink())),
        permissions,
    )
    .limits(Limits::defaults())
    .build()
    .unwrap();

    assert_eq!(engine.model(), "first-model");
    let summary = engine.turn("hi".into()).await.unwrap();
    assert_eq!(summary.stop, Stop::Done);

    engine.set_provider(Box::new(NamedMock {
        inner: MockProvider::new(vec![end_turn()]),
        name: "second-model",
    }));

    // The switch is visible immediately, before any turn runs against it.
    assert_eq!(engine.model(), "second-model");

    // And the next turn actually dispatches to the new provider, not a
    // cached reference to the old one: a `MockProvider` panics if asked for
    // more turns than it was scripted, so this would fail loudly if the
    // engine were still holding the first provider.
    let summary = engine.turn("hi again".into()).await.unwrap();
    assert_eq!(summary.stop, Stop::Done);
}

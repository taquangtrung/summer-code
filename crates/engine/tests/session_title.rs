//! A session's title: generated once, from its first exchange, and read back
//! by `/resume`'s picker in place of the first-user-message fallback.

// Test-fixture fs (a journal file to read back), not agent-driven work.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::mpsc::channel;

use journal::{RecordKind, StopReason, Usage, WriterSink, read_records};
use provider::{MockProvider, ScriptedTurn, StreamEvent, TurnEnd};
use sandbox::Broker;
use summer_engine::{EngineBuilder, Limits};
use tools::{Builtin, Dispatcher, ToolSource};

// ========================================================================
// Functions
// ========================================================================

fn text_turn(text: &str) -> ScriptedTurn {
    ScriptedTurn {
        events: vec![
            StreamEvent::TextDelta(text.into()),
            StreamEvent::TurnEnd(TurnEnd {
                stop: StopReason::EndTurn,
                usage: Usage::zero(),
            }),
        ],
    }
}

fn fixture_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "summer-session-title-{}-{name}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ========================================================================
// Tests
// ========================================================================

#[tokio::test]
async fn the_first_turn_records_a_title_and_the_second_does_not() {
    let cwd = fixture_dir("titled");
    let journal_path = cwd.join("session.jsonl");
    let (_decisions, permissions) = channel();
    let mut engine = EngineBuilder::new(
        cwd.clone(),
        "system".into(),
        // The first script answers the first turn; the second is the
        // title-generation call `Engine::turn` makes right after it; the
        // third answers the second turn. `MockProvider` panics if asked for a
        // fourth, which is what proves the second turn does not generate a
        // second title.
        Box::new(MockProvider::new(vec![
            text_turn("Sure, I can help with that."),
            text_turn("Add a README badge"),
            text_turn("You're welcome."),
        ])),
        Dispatcher::new(Builtin::new().tools()).expect("built-in names are unique"),
        Arc::new(Broker::permissive(cwd.clone())),
        Box::new(WriterSink::new(
            std::fs::File::create(&journal_path).unwrap(),
        )),
        permissions,
    )
    .limits(Limits::defaults())
    .title_sessions()
    .build()
    .unwrap();

    engine
        .turn("add a badge to the README".into())
        .await
        .unwrap();
    engine.turn("thanks".into()).await.unwrap();

    let records = read_records(std::io::BufReader::new(
        std::fs::File::open(&journal_path).unwrap(),
    ))
    .unwrap();
    let titles: Vec<&str> = records
        .iter()
        .filter_map(|record| match &record.kind {
            RecordKind::SessionTitled { title } => Some(title.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(titles, vec!["Add a README badge"]);
}

/// A provider whose clone, the one `Engine::turn` titles with, answers only
/// once the test lets it, so a test can tell a title the turn waited for from
/// one it left running.
struct HeldTitle {
    gate: Arc<tokio::sync::Semaphore>,
    inner: MockProvider,
}

#[async_trait::async_trait]
impl provider::Provider for HeldTitle {
    fn model(&self) -> &str {
        "mock"
    }

    async fn turn(
        &self,
        req: provider::TurnRequest,
    ) -> Result<futures_util::stream::BoxStream<'static, StreamEvent>, provider::ProviderError>
    {
        self.inner.turn(req).await
    }

    async fn count_tokens(
        &self,
        req: &provider::TurnRequest,
    ) -> Result<u32, provider::ProviderError> {
        self.inner.count_tokens(req).await
    }

    fn with_model(&self, _model: &str) -> Option<Box<dyn provider::Provider>> {
        let gate = self.gate.clone();
        Some(Box::new(GatedTitle { gate }))
    }
}

/// The titling clone: waits on the gate, then answers with a title.
struct GatedTitle {
    gate: Arc<tokio::sync::Semaphore>,
}

#[async_trait::async_trait]
impl provider::Provider for GatedTitle {
    fn model(&self) -> &str {
        "mock"
    }

    async fn turn(
        &self,
        _req: provider::TurnRequest,
    ) -> Result<futures_util::stream::BoxStream<'static, StreamEvent>, provider::ProviderError>
    {
        let _permit = self.gate.acquire().await.expect("the gate stays open");
        Ok(futures_util::StreamExt::boxed(futures_util::stream::iter(
            text_turn("Background title").events,
        )))
    }

    async fn count_tokens(
        &self,
        _req: &provider::TurnRequest,
    ) -> Result<u32, provider::ProviderError> {
        Ok(0)
    }
}

#[tokio::test]
async fn the_first_reply_does_not_wait_for_its_title_and_the_title_still_lands() {
    // The title call used to be awaited before the first turn returned, so
    // every session's first reply handed control back a round trip late.
    let cwd = fixture_dir("background");
    let journal_path = cwd.join("session.jsonl");
    let (_decisions, permissions) = channel();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let mut engine = EngineBuilder::new(
        cwd.clone(),
        "system".into(),
        Box::new(HeldTitle {
            gate: gate.clone(),
            inner: MockProvider::new(vec![text_turn("Sure.")]),
        }),
        Dispatcher::new(Builtin::new().tools()).expect("built-in names are unique"),
        Arc::new(Broker::permissive(cwd.clone())),
        Box::new(WriterSink::new(
            std::fs::File::create(&journal_path).unwrap(),
        )),
        permissions,
    )
    .limits(Limits::defaults())
    .title_sessions()
    .build()
    .unwrap();

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        engine.turn("add a badge".into()),
    )
    .await
    .expect("the first turn returned while its title was still held")
    .unwrap();

    gate.add_permits(1);
    engine.settle().await;
    drop(engine);

    let records = read_records(std::io::BufReader::new(
        std::fs::File::open(&journal_path).unwrap(),
    ))
    .unwrap();
    assert!(
        records
            .iter()
            .any(|record| matches!(&record.kind, RecordKind::SessionTitled { title } if title == "Background title")),
        "settle recorded the title before the session ended"
    );
}

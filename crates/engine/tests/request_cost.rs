//! What a turn costs before the model sees it: the cache prefix it keeps, and
//! the round trips it makes on the way.
//!
//! The provider here records what it was sent, because both properties live in
//! the request: a system prompt that changes between turns misses the prompt
//! cache for the whole conversation behind it, and a `count_tokens` call is a
//! serial network round trip ahead of the turn it guards.

// Test-fixture fs (staging a temp root), not agent-driven work.
#![allow(clippy::disallowed_methods)]

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use journal::{CallId, StopReason, Usage, WriterSink};
use provider::{
    MockProvider, Provider, ProviderError, ScriptedTurn, StreamEvent, ToolCallStart, TurnEnd,
    TurnRequest,
};
use sandbox::Broker;
use summer_engine::{ContextManager, EngineBuilder, Limits, Stop};
use tools::{Builtin, Dispatcher, ToolSource};

// ========================================================================
// Data Structures
// ========================================================================

/// A [`MockProvider`] that keeps each turn's system prompt and fails every
/// `count_tokens`, counting the attempts.
struct Recording {
    counts: Arc<AtomicUsize>,
    inner: MockProvider,
    systems: Arc<Mutex<Vec<String>>>,
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
        self.systems.lock().expect("lock").push(req.system.clone());
        self.inner.turn(req).await
    }

    async fn count_tokens(&self, _req: &TurnRequest) -> Result<u32, ProviderError> {
        self.counts.fetch_add(1, Ordering::Relaxed);
        Err(ProviderError::Fatal {
            kind: "rate_limit_error".into(),
            message: "count_tokens is rate limited".into(),
            status: Some(429),
        })
    }
}

// ========================================================================
// Functions
// ========================================================================

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_root() -> std::path::PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let cwd = std::env::temp_dir().join(format!("summer-request-cost-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&cwd).expect("a temp root");
    cwd
}

/// An engine over `provider` with `tools`, and a `context` if one is given.
fn engine(
    provider: Recording,
    tools: Dispatcher,
    context: Option<ContextManager>,
) -> summer_engine::Engine {
    let cwd = temp_root();
    let (_decisions, permissions) = channel();
    let mut builder = EngineBuilder::new(
        cwd.clone(),
        "system".into(),
        Box::new(provider),
        tools,
        Arc::new(Broker::permissive(cwd)),
        Box::new(WriterSink::new(std::io::sink())),
        permissions,
    )
    .limits(Limits::defaults());
    if let Some(context) = context {
        builder = builder.context(context);
    }
    builder
        .build()
        .expect("an engine records its SessionStarted")
}

fn recording(turns: Vec<ScriptedTurn>) -> (Recording, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
    let counts = Arc::new(AtomicUsize::new(0));
    let systems = Arc::new(Mutex::new(Vec::new()));
    let provider = Recording {
        counts: counts.clone(),
        inner: MockProvider::new(turns),
        systems: systems.clone(),
    };
    (provider, counts, systems)
}

fn plan_turn(status: &str) -> ScriptedTurn {
    ScriptedTurn {
        events: vec![
            StreamEvent::ToolCallStart(ToolCallStart {
                call: CallId::new(format!("toolu_plan_{status}")),
                name: "plan".into(),
            }),
            StreamEvent::ToolCallDelta(format!(
                r#"{{"steps":["read it","change it"],"active_step":1,"status":"{status}"}}"#
            )),
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
async fn a_plan_update_leaves_the_system_prompt_and_its_cache_prefix_alone() {
    // The plan used to be appended to the system prompt, so the turn after
    // every `plan` call re-wrote the whole conversation at the cache-write
    // price instead of reading it.
    let (provider, _, systems) = recording(vec![
        plan_turn("in_progress"),
        plan_turn("completed"),
        finish_turn(),
    ]);
    let mut engine = engine(
        provider,
        Dispatcher::new(Builtin::new().tools()).expect("unique names"),
        None,
    );

    let summary = engine.turn("do the thing".into()).await.unwrap();
    assert_eq!(summary.stop, Stop::Done);
    assert_eq!(
        systems.lock().expect("lock").as_slice(),
        ["system", "system", "system"],
        "every turn of the session sends the same system prompt"
    );
}

#[tokio::test]
async fn a_window_far_under_budget_is_sent_without_counting_it() {
    // A serial round trip ahead of every turn of every session, most of which
    // never come near the window. Its failure used to fail the turn as well.
    let (provider, counts, _) = recording(vec![finish_turn()]);
    let mut engine = engine(
        provider,
        Dispatcher::new(Builtin::new().tools()).expect("unique names"),
        None,
    );

    let summary = engine.turn("hi".into()).await.unwrap();
    assert_eq!(summary.stop, Stop::Done);
    assert_eq!(counts.load(Ordering::Relaxed), 0, "no count was asked for");
}

#[tokio::test]
async fn a_window_near_budget_is_counted_and_a_failed_count_falls_back_to_the_estimate() {
    // About 60 tokens by the estimate against a 100-token budget: past half,
    // so only the exact count can say it fits, and under the pack budget, so
    // the estimate alone says it does when the count cannot be had.
    let (provider, counts, _) = recording(vec![finish_turn()]);
    let mut engine = engine(
        provider,
        Dispatcher::new(Vec::new()).expect("no tools, no duplicates"),
        Some(ContextManager::new(100, 40)),
    );

    let summary = engine.turn("word ".repeat(48)).await.unwrap();
    assert_eq!(
        summary.stop,
        Stop::Done,
        "a failed count is not a failed turn"
    );
    assert_eq!(counts.load(Ordering::Relaxed), 1, "the count was asked for");
}

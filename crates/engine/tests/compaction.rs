//! Phase 4 context management: compaction triggers on overflow, keeps the first
//! user message (the task and its early constraints), records `Compacted` records,
//! and a compacted journal replays its surviving turns through the `Recorded`
//! provider with drops applied before the filter.
//!
//! The window budget here counts the whole prompt (new input plus cached
//! tokens), distinct from `Limits::max_tokens` which excludes `cache_read`: a
//! cached token is cheap but still occupies the window, and reusing the runaway
//! number is how a well-cached session sails past the window while the spend
//! counter reports it has barely started.

// Test-fixture fs (writing a temp repo to read from), not agent-driven work.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::channel;

use journal::{CallId, JournalSink, Record, RecordKind, StopReason, Usage};
use provider::{MockProvider, Provider, ScriptedTurn, StreamEvent, ToolCallStart, TurnEnd};
use sandbox::Broker;
use summer_engine::{ContextManager, Engine, EngineBuilder, Limits};
use tools::{Builtin, Dispatcher, Mechanism, Mechanisms, ToolSource};

// ========================================================================
// Data Structures
// ========================================================================

#[derive(Clone, Default)]
struct CapturingSink {
    records: Arc<Mutex<Vec<Record>>>,
}

// ========================================================================
// Implementations
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

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn fresh_repo(tag: &str) -> std::path::PathBuf {
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("summer-ctx-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The window budget these tests run at: the fixed cost of the tool schemas
/// plus room for roughly two turns of conversation.
///
/// **Derived, not pinned, and the difference is a whole class of false
/// failure.** The system prompt and the tool schemas are a floor the window can
/// never drop below, because compaction drops conversation and cannot touch
/// either. A hard-coded budget that sits above today's floor sits *below* it the
/// moment a built-in tool is added, and every test in this file then fails with
/// `ContextOverflow` for a reason that has nothing to do with compaction.
/// Measuring the floor keeps the number these tests actually care about -- the
/// slack above it -- the thing that is fixed.
fn tight_budget() -> u32 {
    /// Room above the floor, in tokens: enough for a couple of turns, far too
    /// little for six big edits. This is the slack the pinned 1_600 budget
    /// happened to leave over the tool set of the day, kept so these tests
    /// exercise the same pressure they were written against.
    const SLACK: u32 = 740;
    /// Mirrors the offline providers' bytes-per-token estimate.
    const BYTES_PER_TOKEN: usize = 4;

    let schema_bytes: usize = Builtin::new()
        .tools()
        .iter()
        .map(|tool| {
            let spec = tool.spec();
            spec.name.len() + spec.description.len() + spec.input_schema.to_string().len()
        })
        .sum();
    (schema_bytes / BYTES_PER_TOKEN) as u32 + SLACK
}

/// An engine with a tiny context budget, so a few turns force compaction.
fn tight_engine(
    repo: &std::path::Path,
    turns: Vec<ScriptedTurn>,
    budget: u32,
    keep_recent: usize,
) -> (Engine, CapturingSink) {
    let sink = CapturingSink::default();
    let (_decisions, permissions) = channel();
    let engine = EngineBuilder::new(
        repo.to_path_buf(),
        "system".into(),
        Box::new(MockProvider::new(turns)),
        Dispatcher::new(Builtin::new().tools()).expect("built-in names are unique"),
        Arc::new(Broker::permissive(repo.to_path_buf())),
        Box::new(sink.clone()),
        permissions,
    )
    .limits(Limits::defaults())
    .context(ContextManager::new(budget, keep_recent))
    .build()
    .unwrap();
    (engine, sink)
}

/// [`tight_engine`], with the model asked to summarize what compaction drops.
///
/// A separate constructor rather than a parameter on the shared one, because
/// turning the mechanism on adds a provider round trip: every existing test here
/// scripts `MockProvider` by the exact number of turns its loop makes, and a
/// shared flag would have silently changed what those scripts mean.
fn summarizing_engine(
    repo: &std::path::Path,
    turns: Vec<ScriptedTurn>,
    budget: u32,
    keep_recent: usize,
) -> (Engine, CapturingSink) {
    let sink = CapturingSink::default();
    let (_decisions, permissions) = channel();
    let engine = EngineBuilder::new(
        repo.to_path_buf(),
        "system".into(),
        Box::new(MockProvider::new(turns)),
        Dispatcher::new(Builtin::new().tools()).expect("built-in names are unique"),
        Arc::new(Broker::permissive(repo.to_path_buf())),
        Box::new(sink.clone()),
        permissions,
    )
    .limits(Limits::defaults())
    .context(ContextManager::new(budget, keep_recent))
    .summarize_compactions()
    .build()
    .unwrap();
    (engine, sink)
}

/// A turn that produces `text` and ends. What the summarizer's own call gets.
fn text_turn(text: &str) -> ScriptedTurn {
    ScriptedTurn {
        events: vec![
            StreamEvent::TextDelta(text.into()),
            StreamEvent::BlockEnd,
            StreamEvent::TurnEnd(TurnEnd {
                stop: StopReason::EndTurn,
                usage: Usage::new(70, 20, 0, 0),
            }),
        ],
    }
}

/// A turn that edits `path` with a large `content`, asking for more tools.
fn big_edit_turn(call: &str, path: &str) -> ScriptedTurn {
    let big = "x".repeat(2_000);
    ScriptedTurn {
        events: vec![
            StreamEvent::ToolCallStart(ToolCallStart {
                call: CallId::new(call.into()),
                name: "edit".into(),
            }),
            StreamEvent::ToolCallDelta(format!("{{\"path\":\"{path}\",\"content\":\"{big}\"}}")),
            StreamEvent::BlockEnd,
            StreamEvent::TurnEnd(TurnEnd {
                stop: StopReason::ToolUse,
                usage: Usage::new(100, 10, 0, 0),
            }),
        ],
    }
}

fn done_turn() -> ScriptedTurn {
    ScriptedTurn {
        events: vec![
            StreamEvent::TextDelta("all done".into()),
            StreamEvent::BlockEnd,
            StreamEvent::TurnEnd(TurnEnd {
                stop: StopReason::EndTurn,
                usage: Usage::new(50, 5, 0, 0),
            }),
        ],
    }
}

fn compacted_records(records: &[Record]) -> Vec<&journal::Compacted> {
    records
        .iter()
        .filter_map(|record| match &record.kind {
            RecordKind::Compacted(c) => Some(c),
            _ => None,
        })
        .collect()
}

// ========================================================================
// Tests
// ========================================================================

#[tokio::test]
async fn compaction_triggers_and_keeps_the_first_user_message() {
    // A single turn that loops through many big edits overflows the tiny budget,
    // so compaction fires mid-turn. The first user message (the task) must
    // survive every compaction, or the model forgets what it was asked to do.
    let repo = fresh_repo("overflow");
    let turns: Vec<ScriptedTurn> = (0..6)
        .map(|i| big_edit_turn(&format!("toolu_{i}"), &format!("f{i}.txt")))
        .chain([done_turn()])
        .collect();
    let (mut engine, sink) = tight_engine(&repo, turns, tight_budget(), 2);

    engine.turn("the task: write files".into()).await.unwrap();

    let records = sink.records();
    let compacted = compacted_records(&records);
    assert!(
        !compacted.is_empty(),
        "the tiny budget should have forced at least one compaction"
    );

    let first_user = records
        .iter()
        .find(|record| matches!(record.kind, RecordKind::UserMessage { .. }))
        .expect("a UserMessage was journaled")
        .id;
    for compacted in &compacted {
        assert!(
            !compacted.dropped.contains(&first_user),
            "the first user message was dropped by compaction"
        );
    }
}

#[tokio::test]
async fn a_compacted_journal_replays_its_surviving_turns() {
    // Record a session that compacts, then build a Recorded provider from the
    // journal. Drops are applied before the ModelTurn filter, so the recording
    // holds exactly the surviving turns in order — not the compacted-away ones.
    let repo = fresh_repo("replay-source");
    let turns: Vec<ScriptedTurn> = (0..5)
        .map(|i| big_edit_turn(&format!("toolu_{i}"), &format!("g{i}.txt")))
        .chain([done_turn()])
        .collect();
    let (mut engine, sink) = tight_engine(&repo, turns, tight_budget(), 2);
    engine.turn("write files".into()).await.unwrap();

    let records = sink.records();
    assert!(
        !compacted_records(&records).is_empty(),
        "the recording compacted"
    );

    // Serialize the compacted journal through the codec, as a file would hold it.
    let mut bytes = Vec::new();
    journal::write_records(&mut bytes, &records).unwrap();
    let read = journal::read_records(std::io::Cursor::new(&bytes)).unwrap();
    let head = read.last().expect("records").id;
    let walk = journal::Journal::from_records(read)
        .expect("unique ids")
        .walk(head)
        .unwrap();

    // The surviving ModelTurns are exactly what Recorded should hand back. A
    // provider that counted dropped turns would return the wrong ones here.
    let surviving: Vec<journal::Record> = walk
        .records()
        .iter()
        .filter(|record| matches!(record.kind, RecordKind::ModelTurn(_)))
        .map(|record| (**record).clone())
        .collect();
    let recorded = provider::Recorded::from_walk(&walk).unwrap();

    for _ in &surviving {
        let result = recorded
            .turn(provider::TurnRequest {
                messages: vec![],
                system: String::new(),
                tools: vec![],
            })
            .await;
        assert!(result.is_ok(), "a surviving turn should replay");
    }
    // One more request than there are surviving turns is divergence: the loop
    // asks for a turn the recording does not have.
    let extra = recorded
        .turn(provider::TurnRequest {
            messages: vec![],
            system: String::new(),
            tools: vec![],
        })
        .await;
    assert!(extra.is_err(), "the recording should be exhausted");
}

#[tokio::test]
async fn compacting_twice_drops_the_first_compaction() {
    // Two compactions in one run: the second drops a range that includes the
    // first Compacted record. A naive drop set resurrects the first dropped range;
    // the walk (and therefore Recorded) must honor both.
    let repo = fresh_repo("twice");
    // Enough turns to compact, keep going, and compact again.
    let turns: Vec<ScriptedTurn> = (0..10)
        .map(|i| big_edit_turn(&format!("toolu_{i}"), &format!("h{i}.txt")))
        .chain([done_turn()])
        .collect();
    let (mut engine, sink) = tight_engine(&repo, turns, tight_budget(), 2);
    engine.turn("write many files".into()).await.unwrap();

    let records = sink.records();
    let compacted = compacted_records(&records);
    assert!(
        compacted.len() >= 2,
        "expected at least two compactions, got {}: the budget should force more than one",
        compacted.len()
    );

    // The union of every Compacted.dropped must not include the first user
    // message or the recent turns (they survive by construction).
    let first_user = records
        .iter()
        .find(|record| matches!(record.kind, RecordKind::UserMessage { .. }))
        .expect("UserMessage")
        .id;
    let all_dropped: std::collections::HashSet<_> = compacted
        .iter()
        .flat_map(|c| c.dropped.iter().copied())
        .collect();
    assert!(
        !all_dropped.contains(&first_user),
        "the first user message survived both compactions"
    );
}

#[tokio::test]
async fn compact_now_drops_history_and_records_why() {
    // What a frontend's `/compact` reaches. The TUI used to answer it by
    // draining its own transcript, which is the rendered view: the history the
    // next turn is built from was untouched, so the token saving it reported was
    // zero. This is the operation that actually saves them, asked for early
    // rather than waiting for a turn to overflow the window.
    let repo = fresh_repo("manual");
    let turns: Vec<ScriptedTurn> = (0..4)
        .map(|i| big_edit_turn(&format!("toolu_{i}"), &format!("m{i}.txt")))
        .chain([done_turn()])
        .collect();
    // A budget wide enough that nothing compacts on its own, so what the test
    // observes is the manual call and not the loop's own housekeeping.
    let (mut engine, sink) = tight_engine(&repo, turns, 1_000_000, 2);
    engine.turn("write some files".into()).await.unwrap();
    let before = sink.records();
    assert!(
        compacted_records(&before).is_empty(),
        "the budget was meant to be wide enough that nothing compacted by itself"
    );

    let dropped = engine
        .compact_now()
        .await
        .expect("compaction records cleanly");
    assert!(dropped > 0, "a long history had nothing to drop");

    let records = sink.records();
    let compacted = compacted_records(&records);
    assert_eq!(
        compacted.len(),
        1,
        "the drop must be journaled, or a resume rebuilds what was just dropped"
    );
    assert!(
        !compacted[0].dropped.is_empty(),
        "a Compacted record that names no records subtracts nothing on replay"
    );
}

#[tokio::test]
async fn compact_now_on_a_short_history_drops_nothing() {
    // The honest zero. A frontend has to be able to tell "there was nothing to
    // compact" from "entries were dropped", and reporting a saving either way is
    // the bug this replaces.
    let repo = fresh_repo("short");
    let (mut engine, _sink) = tight_engine(&repo, vec![done_turn()], 1_000_000, 2);
    engine.turn("say hello".into()).await.unwrap();

    assert_eq!(engine.compact_now().await.expect("no error"), 0);
}

#[tokio::test]
async fn the_model_writes_the_summary_when_the_mechanism_is_on() {
    // What the mechanism is for. The mechanical summary is a count ("dropped N
    // turns, M tool calls"), so a compacted session used to lose every fact it
    // had established about the codebase at exactly the point it had gathered
    // the most of them. The model's text has to reach the `Compacted` record,
    // because that record is what a resume folds back in.
    let repo = fresh_repo("summarize");
    let turns: Vec<ScriptedTurn> = (0..4)
        .map(|i| big_edit_turn(&format!("toolu_{i}"), &format!("m{i}.txt")))
        .chain([
            done_turn(),
            // The summarizer's own round trip, taken during `/compact`.
            text_turn("edited m0.txt through m3.txt; the fixture writes 2000 x's"),
        ])
        .collect();
    let (mut engine, sink) = summarizing_engine(&repo, turns, 1_000_000, 2);
    engine.turn("write some files".into()).await.unwrap();

    let dropped = engine
        .compact_now()
        .await
        .expect("compaction records cleanly");
    assert!(dropped > 0, "a long history had nothing to drop");

    let records = sink.records();
    let compacted = compacted_records(&records);
    assert_eq!(compacted.len(), 1);
    assert_eq!(
        compacted[0].summary, "edited m0.txt through m3.txt; the fixture writes 2000 x's",
        "the model's summary must be what the journal records"
    );
}

#[tokio::test]
async fn a_failed_summarizer_falls_back_to_the_mechanical_count() {
    // The failure this must not have. A compaction only happens because the
    // window is already full, so a summarizer that fails at that moment must
    // not be able to fail the turn -- the user has the most work in flight
    // exactly when the context is fullest.
    //
    // The scripted failure is a stream that stops before its `TurnEnd`, which
    // is what a dropped connection mid-summary looks like to the accumulator.
    let repo = fresh_repo("summary-fails");
    let truncated = ScriptedTurn {
        events: vec![
            StreamEvent::TextDelta("half a summ".into()),
            StreamEvent::BlockEnd,
        ],
    };
    let turns: Vec<ScriptedTurn> = (0..4)
        .map(|i| big_edit_turn(&format!("toolu_{i}"), &format!("m{i}.txt")))
        .chain([done_turn(), truncated])
        .collect();
    let (mut engine, sink) = summarizing_engine(&repo, turns, 1_000_000, 2);
    engine.turn("write some files".into()).await.unwrap();

    let dropped = engine
        .compact_now()
        .await
        .expect("a failed summarizer is not a failed compaction");
    assert!(dropped > 0);

    let records = sink.records();
    let compacted = compacted_records(&records);
    assert_eq!(compacted.len(), 1);
    assert!(
        !compacted[0].summary.is_empty(),
        "the mechanical summary still has to be recorded"
    );
}

#[tokio::test]
async fn the_ablation_switch_turns_the_summarizer_off_on_its_own() {
    // The `compaction-summaries` arm of the matrix. `summarize_compactions` is
    // on here, so the only thing stopping the call is the mechanism: an arm
    // whose switch did nothing would report the control's numbers under the
    // ablated label, which is worse than not running it.
    let repo = fresh_repo("summarize-ablated");
    let turns: Vec<ScriptedTurn> = (0..4)
        .map(|i| big_edit_turn(&format!("toolu_{i}"), &format!("m{i}.txt")))
        .chain([done_turn()])
        .collect();
    let mut mechanisms = Mechanisms::default();
    mechanisms.disable(Mechanism::CompactionSummaries);

    let sink = CapturingSink::default();
    let (_decisions, permissions) = channel();
    let mut engine = EngineBuilder::new(
        repo.to_path_buf(),
        "system".into(),
        Box::new(MockProvider::new(turns)),
        Dispatcher::new(Builtin::new().tools()).expect("built-in names are unique"),
        Arc::new(Broker::permissive(repo.to_path_buf())),
        Box::new(sink.clone()),
        permissions,
    )
    .limits(Limits::defaults())
    .context(ContextManager::new(1_000_000, 2))
    .mechanisms(mechanisms)
    .summarize_compactions()
    .build()
    .unwrap();

    engine.turn("write some files".into()).await.unwrap();
    // The script holds exactly the loop's own turns, so a summarizer call here
    // would exhaust `MockProvider` and panic rather than fail an assertion.
    engine.compact_now().await.expect("compaction is clean");

    let records = sink.records();
    let compacted = compacted_records(&records);
    assert_eq!(compacted.len(), 1);
    assert!(
        compacted[0].summary.contains("turn"),
        "the ablated arm must record the mechanical count: {}",
        compacted[0].summary
    );
}

#[tokio::test]
async fn the_summarizer_is_off_unless_asked_for() {
    // The property that keeps `bench --replay` correct. `Recorded` answers the
    // Nth `Provider::turn` with the Nth recorded turn and ignores the request,
    // so one unasked-for summarizer call would shift every later turn by one.
    // The default engine must therefore make no extra call: this script has
    // exactly the turns the loop itself takes, and a summarizer would exhaust
    // it and surface as an error.
    let repo = fresh_repo("summarize-off");
    let turns: Vec<ScriptedTurn> = (0..4)
        .map(|i| big_edit_turn(&format!("toolu_{i}"), &format!("m{i}.txt")))
        .chain([done_turn()])
        .collect();
    let (mut engine, sink) = tight_engine(&repo, turns, 1_000_000, 2);
    engine.turn("write some files".into()).await.unwrap();
    engine.compact_now().await.expect("compaction is clean");

    let records = sink.records();
    let compacted = compacted_records(&records);
    assert_eq!(compacted.len(), 1);
    assert!(
        compacted[0].summary.contains("turn"),
        "the default summary is the mechanical count: {}",
        compacted[0].summary
    );
}

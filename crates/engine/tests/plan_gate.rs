//! A turn cannot quietly end with plan steps outstanding.
//!
//! The `plan` tool used to format its arguments back and nothing else: the model
//! wrote a plan, the next round re-injected it, and a turn that abandoned it
//! ended `Done` like any other. Drift prevention that reports success on drift
//! is worse than none, because the stop reason is what a caller and a bench
//! both read.
//!
//! So the loop reminds the model once, and if the turn still ends with the plan
//! open it says so. One reminder rather than a loop: the model can answer it in
//! a single call, and an unbounded nag would trade a wrong label for a stuck
//! session.

// Test-fixture fs (staging a temp root), not agent-driven work.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::mpsc::channel;

use journal::{CallId, JournalSink, Record, RecordKind, StopReason, Usage};
use provider::{MockProvider, ScriptedTurn, StreamEvent, ToolCallStart, TurnEnd};
use sandbox::Broker;
use summer_engine::{Engine, EngineBuilder, Limits, Stop};
use tools::{Builtin, Dispatcher, ToolSource};

// ========================================================================
// Data Structures
// ========================================================================

#[derive(Clone, Default)]
struct CapturingSink {
    records: Arc<std::sync::Mutex<Vec<Record>>>,
}

// ========================================================================
// Implementations
// ========================================================================

impl CapturingSink {
    fn user_messages(&self) -> Vec<String> {
        self.records
            .lock()
            .expect("sink lock")
            .iter()
            .filter_map(|record| match &record.kind {
                RecordKind::UserMessage { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
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

fn engine(turns: Vec<ScriptedTurn>) -> (Engine, CapturingSink) {
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let cwd = std::env::temp_dir().join(format!("summer-plan-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&cwd).expect("a temp root");

    let sink = CapturingSink::default();
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
    .limits(Limits::defaults())
    .build()
    .expect("an engine records its SessionStarted");
    (engine, sink)
}

/// One turn recording a plan with `status`.
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

/// One turn that just answers.
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
async fn an_abandoned_plan_is_reminded_once_and_then_reported() {
    let (mut engine, sink) = engine(vec![plan_turn("in_progress"), finish_turn(), finish_turn()]);
    let summary = engine.turn("do the thing".into()).await.unwrap();

    assert_eq!(
        summary.stop,
        Stop::PlanIncomplete,
        "a turn that walked away from its plan reported success"
    );
    let reminders = sink
        .user_messages()
        .into_iter()
        .filter(|text| text.contains("steps outstanding"))
        .count();
    assert_eq!(
        reminders, 1,
        "exactly one reminder: none is silence, and more than one is a nag with no exit"
    );
}

#[tokio::test]
async fn a_plan_marked_completed_ends_the_turn_normally() {
    let (mut engine, _sink) = engine(vec![plan_turn("completed"), finish_turn()]);
    let summary = engine.turn("do the thing".into()).await.unwrap();

    assert_eq!(summary.stop, Stop::Done);
}

#[tokio::test]
async fn a_plan_marked_blocked_ends_the_turn_normally() {
    // Blocked is an answer, not an evasion: the model said where the work
    // stopped and why it cannot continue, which is what the reminder asks for.
    let (mut engine, _sink) = engine(vec![plan_turn("blocked"), finish_turn()]);
    let summary = engine.turn("do the thing".into()).await.unwrap();

    assert_eq!(summary.stop, Stop::Done);
}

#[tokio::test]
async fn a_turn_with_no_plan_is_untouched() {
    // The default path: every session that never calls `plan` must reach the
    // same stop it always did, with no extra round trip.
    let (mut engine, sink) = engine(vec![finish_turn()]);
    let summary = engine.turn("do the thing".into()).await.unwrap();

    assert_eq!(summary.stop, Stop::Done);
    assert_eq!(
        sink.user_messages().len(),
        1,
        "an unplanned turn grew a reminder it had no plan to be reminded about"
    );
}

#[tokio::test]
async fn a_goal_run_stops_when_the_plan_is_closed() {
    // The predicate this replaces asked whether the *rendered* plan text
    // contained "completed" and contained neither "in_progress" nor "pending",
    // which is a guess about markdown: a step reading "mark the migration
    // completed" satisfied it on the first turn. The tool's own status argument
    // is the answer.
    // The two mechanisms overlap, and this pins where the seam is. The gate
    // re-prompts *within* a turn, once, so the goal loop only adds a turn after
    // a whole turn has ended `PlanIncomplete` despite that reminder. Turn one
    // here opens a plan, ignores the reminder, and ends unfinished; turn two
    // closes it.
    let (mut engine, _sink) = engine(vec![
        plan_turn("in_progress"),
        finish_turn(),
        finish_turn(),
        plan_turn("completed"),
        finish_turn(),
    ]);
    let summary = engine.run_goal("do the thing".into(), 8).await.unwrap();

    assert!(summary.completed, "a closed plan is a finished goal");
    assert_eq!(
        summary.iterations, 2,
        "one turn that ended unfinished, one that closed the plan"
    );
}

#[tokio::test]
async fn a_goal_run_that_never_closes_its_plan_says_so() {
    // The ceiling is what bounds an unattended loop, and hitting it is not
    // success. The last turn already stopped `PlanIncomplete`, so the reported
    // stop needs no second opinion.
    let turns = std::iter::once(plan_turn("in_progress"))
        .chain(std::iter::repeat_with(finish_turn).take(8))
        .collect();
    let (mut engine, _sink) = engine(turns);
    let summary = engine.run_goal("do the thing".into(), 3).await.unwrap();

    assert!(!summary.completed);
    assert_eq!(
        summary.iterations, 3,
        "stopped at the ceiling, not before it"
    );
    assert_eq!(summary.into_turn().stop, Stop::PlanIncomplete);
}

#[tokio::test]
async fn a_goal_with_no_plan_is_one_turn() {
    // Re-prompting a model that never opened a plan asks it to continue
    // something it does not think it started.
    let (mut engine, _sink) = engine(vec![finish_turn()]);
    let summary = engine.run_goal("say hello".into(), 8).await.unwrap();

    assert_eq!(summary.iterations, 1);
    assert!(summary.completed, "nothing was left open");
}

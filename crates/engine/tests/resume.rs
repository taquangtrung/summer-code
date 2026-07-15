//! Phase 3 resume: kill a session mid-task, restart, and continue from the
//! journal — without re-asking a permission the user already granted, and with
//! an interrupted tool call completed by a synthetic outcome.
//!
//! Resume folds the journal's walk into session state, seeds the broker's grant
//! cache from the `Permission` scopes, and surfaces `open_calls` (a `ToolCall`
//! with no `ToolResult`) so the engine can record a synthetic outcome rather
//! than re-dispatch a half-applied write. The fold is the load-bearing claim:
//! every field of the resumed engine is rebuilt from the journal, not stored
//! alongside it.

// Test-fixture fs (writing a temp repo to read from), not agent-driven work.
#![allow(clippy::disallowed_methods)]

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::channel;

use journal::{
    CallId, JournalSink, Outcome, Record, RecordId, RecordKind, StopReason, Usage, write_records,
};
use policy::{Effect, NetworkRule, PathAccess, PathRule, Policy, RuleSpec, Scope};
use provider::{
    MockProvider, Provider, ScriptedTurn, StreamEvent, ThinkingDelta, ToolCallStart, TurnEnd,
};
use sandbox::{Broker, BrokerBuilder, Grant, Prompter};
use summer_engine::{EngineBuilder, Limits, ResumeError};
use tempfile::TempDir;
use tools::{Builtin, Dispatcher, ToolSource};

// ========================================================================
// Data Structures
// ========================================================================

/// A sink that keeps every record, so a resumed engine's synthetic outcomes and
/// new turns can be asserted on the journal it actually produced.
#[derive(Clone, Default)]
struct CapturingSink {
    records: Arc<Mutex<Vec<Record>>>,
}

/// A prompter that allows every `Ask` with a rule scope covering writes under
/// the root, and counts how many times it was asked. The count is what proves a
/// resumed session did not re-ask a grant the journal already records.
struct CountingPrompter {
    asked: Arc<Mutex<usize>>,
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

impl CountingPrompter {
    fn new(counted: Arc<Mutex<usize>>) -> Self {
        Self { asked: counted }
    }
}

#[async_trait::async_trait]
impl Prompter for CountingPrompter {
    async fn ask(&self, _request: &policy::Request) -> Grant {
        *self.asked.lock().expect("count lock") += 1;
        Grant {
            allow: true,
            // Rule scope: one answer covers every write under the root, which is
            // the granularity remembering works at.
            scope: Scope::Rule(RuleSpec::Path {
                access: PathAccess::Write,
                pattern: "**".to_string(),
            }),
        }
    }
}

// ========================================================================
// Functions
// ========================================================================

/// A repo with one file, so the agent has something to read and edit.
fn repo() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = std::fs::canonicalize(dir.path()).expect("canonical root");
    std::fs::write(root.join("README.md"), "# project\n").expect("README");
    (dir, root)
}

/// A policy that asks about writes and allows reads: no path rule matches an
/// ordinary file, so writes fall through to the `Ask` default and reads to
/// `Allow`. This is the shape that prompts on writes.
fn ask_writes_policy(root: PathBuf) -> Policy {
    Policy {
        commands: Vec::new(),
        network: NetworkRule::default(),
        paths: vec![
            PathRule {
                access: Some(PathAccess::Write),
                effect: Effect::Deny,
                pattern: "**/.git/**".into(),
            },
            PathRule {
                access: Some(PathAccess::Write),
                effect: Effect::Deny,
                pattern: "**/.summer/policy.json".into(),
            },
        ],
        root,
    }
}

/// An `EngineBuilder` wired with a permissive broker rooted at `root`.
fn permissive_builder(
    root: &Path,
    provider: Box<dyn Provider>,
    sink: CapturingSink,
) -> EngineBuilder {
    let (_decisions, permissions) = channel();
    let broker = Arc::new(Broker::permissive(root.to_path_buf()));
    EngineBuilder::new(
        root.to_path_buf(),
        "system".into(),
        provider,
        Dispatcher::new(Builtin::new().tools()).expect("built-in names are unique"),
        broker,
        Box::new(sink),
        permissions,
    )
    .limits(Limits::defaults())
}

/// An `EngineBuilder` wired with a real broker (policy + prompter) rooted at
/// `root`, so writes reach the prompter.
fn builder_with_policy(
    root: &Path,
    policy: Policy,
    prompter: Arc<dyn Prompter>,
    provider: Box<dyn Provider>,
    sink: CapturingSink,
) -> EngineBuilder {
    let (decisions, permissions) = channel();
    let broker = Arc::new(
        BrokerBuilder::new(policy)
            .prompter(prompter_to_box(prompter))
            .decisions(decisions)
            .build(true)
            .expect("allow_unenforced clears the startup check"),
    );
    EngineBuilder::new(
        root.to_path_buf(),
        "system".into(),
        provider,
        Dispatcher::new(Builtin::new().tools()).expect("built-in names are unique"),
        broker,
        Box::new(sink),
        permissions,
    )
    .limits(Limits::defaults())
}

/// A real `BrokerBuilder` needs a `Box<dyn Prompter>`, but the resumed engine
/// shares one counting prompter across both brokers, so it is held by `Arc`.
fn prompter_to_box(prompter: Arc<dyn Prompter>) -> Box<dyn Prompter> {
    // The shared prompter is held by the test; the broker gets its own clone of
    // the `Arc` behind a thin forwarding type.
    struct Forwarded(Arc<dyn Prompter>);
    #[async_trait::async_trait]
    impl Prompter for Forwarded {
        async fn ask(&self, request: &policy::Request) -> Grant {
            self.0.ask(request).await
        }
    }
    Box::new(Forwarded(prompter))
}

fn edit_turn(call: &str, path: &str, content: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCallStart(ToolCallStart {
            call: CallId::new(call.into()),
            name: "edit".into(),
        }),
        // `action` is named, because an `edit` that omits it means the anchored
        // `str_replace` and these turns create a file from nothing.
        StreamEvent::ToolCallDelta(format!(
            "{{\"path\":\"{path}\",\"action\":\"create\",\"content\":{}}}",
            serde_json::Value::from(content)
        )),
        StreamEvent::BlockEnd,
        StreamEvent::TurnEnd(TurnEnd {
            stop: StopReason::ToolUse,
            usage: Usage::new(10, 0, 0, 0),
        }),
    ]
}

fn done_turn(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta(text.into()),
        StreamEvent::BlockEnd,
        StreamEvent::TurnEnd(TurnEnd {
            stop: StopReason::EndTurn,
            usage: Usage::new(12, 1, 0, 0),
        }),
    ]
}

fn to_bytes(records: &[Record]) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_records(&mut bytes, records).unwrap();
    bytes
}

// ========================================================================
// Tests
// ========================================================================

#[tokio::test]
async fn resume_does_not_reask_a_granted_permission() {
    // The grant cache is a derived cache: it folds from `Permission` scopes, and
    // `Broker::restore` seeds it on resume. Without it a resumed session re-asks
    // everything the user already approved — the fold thesis failing on the one
    // piece of state that most looks like it deserves a side table.
    let (_dir, root) = repo();
    let asked = Arc::new(Mutex::new(0usize));
    let prompter: Arc<dyn Prompter> = Arc::new(CountingPrompter::new(asked.clone()));

    let sink = CapturingSink::default();
    let provider: Box<dyn Provider> = Box::new(MockProvider::new(vec![
        ScriptedTurn {
            events: edit_turn("toolu_1", "a.txt", "A\n"),
        },
        ScriptedTurn {
            events: done_turn("wrote a.txt"),
        },
    ]));
    let builder = builder_with_policy(
        &root,
        ask_writes_policy(root.clone()),
        prompter.clone(),
        provider,
        sink.clone(),
    );
    let mut engine = builder.build().unwrap();
    engine.turn("write a.txt".into()).await.unwrap();
    assert_eq!(*asked.lock().unwrap(), 1, "the write prompted once");

    // Kill the session: serialize its journal and drop the engine.
    let bytes = to_bytes(&sink.records());
    let head = sink.records().last().expect("records").id;
    drop(engine);

    // Resume: fold the journal, seed the broker's grant cache. A fresh provider
    // scripts a *different* write under the same rule, so a re-ask would show.
    let resume_sink = CapturingSink::default();
    let resume_provider: Box<dyn Provider> = Box::new(MockProvider::new(vec![
        ScriptedTurn {
            events: edit_turn("toolu_2", "b.txt", "B\n"),
        },
        ScriptedTurn {
            events: done_turn("wrote b.txt"),
        },
    ]));
    let resume_builder = builder_with_policy(
        &root,
        ask_writes_policy(root.clone()),
        prompter.clone(),
        resume_provider,
        resume_sink.clone(),
    );
    let mut resumed = resume_builder
        .resume(Cursor::new(&bytes), head)
        .expect("resume folds the journal");
    resumed.turn("write b.txt".into()).await.unwrap();

    assert_eq!(
        *asked.lock().unwrap(),
        1,
        "a resumed session must not re-ask a grant folded from the journal"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("b.txt")).unwrap(),
        "B\n",
        "the resumed write ran for real under the restored grant"
    );
}

#[tokio::test]
async fn resume_completes_an_interrupted_call_with_a_synthetic_outcome() {
    // A `ToolCall` with no `ToolResult` is a tool in flight when the process
    // died. The dedup ledger refuses re-dispatch, and re-running a half-applied
    // write is not safe to guess at, so resume records a synthetic outcome and
    // lets the model see the interruption.
    let (_dir, root) = repo();

    // Record a full session, then truncate it to end at the ToolCall: that is the
    // journal a crash mid-dispatch would leave behind.
    let sink = CapturingSink::default();
    let provider: Box<dyn Provider> = Box::new(MockProvider::new(vec![
        ScriptedTurn {
            events: edit_turn("toolu_1", "x.txt", "X\n"),
        },
        ScriptedTurn {
            events: done_turn("wrote x.txt"),
        },
    ]));
    let builder = permissive_builder(&root, provider, sink.clone());
    let mut engine = builder.build().unwrap();
    engine.turn("write x.txt".into()).await.unwrap();

    let full = sink.records();
    let truncated: Vec<Record> = full
        .iter()
        .take_while(|record| !matches!(record.kind, RecordKind::ToolResult(_)))
        .cloned()
        .collect();
    let head = truncated.last().expect("ends at the ToolCall").id;
    let bytes = to_bytes(&truncated);

    // Resume from the truncated journal. The open call must be completed.
    let resume_sink = CapturingSink::default();
    let resume_provider: Box<dyn Provider> = Box::new(MockProvider::new(vec![ScriptedTurn {
        events: done_turn("recovered"),
    }]));
    let resume_builder = permissive_builder(&root, resume_provider, resume_sink.clone());
    let resumed = resume_builder
        .resume(Cursor::new(&bytes), head)
        .expect("resume folds the truncated journal");

    let resumed_records = resume_sink.records();
    let result = resumed_records
        .iter()
        .find_map(|record| match &record.kind {
            RecordKind::ToolResult(result) if result.call == CallId::new("toolu_1".into()) => {
                Some(result.outcome.clone())
            }
            _ => None,
        })
        .expect("the interrupted call got a synthetic ToolResult");
    assert!(
        matches!(result, Outcome::Err { .. }),
        "an interrupted call surfaces as an error the model can read, got {result:?}"
    );
    drop(resumed);
}

#[tokio::test]
async fn resume_does_not_re_execute_a_call_the_journal_already_ran() {
    // The dedup ledger is a derived cache, like the broker's grant set, and it
    // needs the same seeding on resume. A fresh `HashSet` after a restart made
    // this module's own claim ("the ledger refuses re-dispatch, so re-running a
    // half-applied write is not a risk we take") false exactly across the
    // restart it exists to survive. A `Recorded` provider replaying a journal
    // re-emits the recorded call ids; without the seed, every write runs twice.
    let (_dir, root) = repo();

    let sink = CapturingSink::default();
    let provider: Box<dyn Provider> = Box::new(MockProvider::new(vec![
        ScriptedTurn {
            events: edit_turn("toolu_1", "x.txt", "original\n"),
        },
        ScriptedTurn {
            events: done_turn("wrote x.txt"),
        },
    ]));
    let mut engine = permissive_builder(&root, provider, sink.clone())
        .build()
        .unwrap();
    engine.turn("write x.txt".into()).await.unwrap();
    drop(engine);

    let records = sink.records();
    let head = records.last().expect("a recorded session").id;
    let bytes = to_bytes(&records);
    assert_eq!(
        std::fs::read_to_string(root.join("x.txt")).unwrap(),
        "original\n"
    );

    // Resume, and have the model re-emit the very same call id with different
    // content. The ledger must refuse it.
    let resume_sink = CapturingSink::default();
    let resume_provider: Box<dyn Provider> = Box::new(MockProvider::new(vec![
        ScriptedTurn {
            events: edit_turn("toolu_1", "x.txt", "clobbered\n"),
        },
        ScriptedTurn {
            events: done_turn("done"),
        },
    ]));
    let mut resumed = permissive_builder(&root, resume_provider, resume_sink.clone())
        .resume(Cursor::new(&bytes), head)
        .expect("resume folds the journal");
    resumed.turn("again".into()).await.unwrap();

    assert_eq!(
        std::fs::read_to_string(root.join("x.txt")).unwrap(),
        "original\n",
        "a call id the journal already ran must not execute again after a resume"
    );
    let refused = resume_sink
        .records()
        .iter()
        .any(|record| match &record.kind {
            RecordKind::ToolResult(result) => matches!(
                &result.outcome,
                Outcome::Err { message } if message.contains("duplicate")
            ),
            _ => false,
        });
    assert!(
        refused,
        "the re-dispatch should surface as a duplicate error"
    );
}

#[tokio::test]
async fn resume_refuses_an_unknown_head() {
    // A head not in the journal cannot be walked; resuming it is not a guess to
    // make silently.
    let (_dir, root) = repo();
    let sink = CapturingSink::default();
    let provider: Box<dyn Provider> = Box::new(MockProvider::new(vec![ScriptedTurn {
        events: vec![StreamEvent::TurnEnd(TurnEnd {
            stop: StopReason::EndTurn,
            usage: Usage::zero(),
        })],
    }]));
    let builder = permissive_builder(&root, provider, sink.clone());
    let mut engine = builder.build().unwrap();
    engine.turn("hi".into()).await.unwrap();
    let bytes = to_bytes(&sink.records());

    let bogus_head = RecordId::new();
    let resume_provider: Box<dyn Provider> = Box::new(MockProvider::new(vec![]));
    let resume_builder = permissive_builder(&root, resume_provider, sink);
    let error = match resume_builder.resume(Cursor::new(&bytes), bogus_head) {
        Ok(_) => panic!("an unknown head must be refused"),
        Err(error) => error,
    };
    assert!(
        matches!(error, ResumeError::UnknownHead(h) if h == bogus_head),
        "expected UnknownHead, got {error:?}"
    );
}

#[tokio::test]
async fn resume_carries_spend_forward() {
    // Spend folds from every surviving ModelTurn, so a limit survives a restart
    // instead of resetting the budget when someone kills the process.
    //
    // The limit is checked at the *top* of the loop from the previous turn's
    // spend (one turn of overshoot is deliberate), so a turn that overshoots
    // returns its own stop reason and the ceiling fires on the next turn. Resume
    // folds the spend, so the next turn after a restart hits it.
    let (_dir, root) = repo();
    let sink = CapturingSink::default();
    let provider: Box<dyn Provider> = Box::new(MockProvider::new(vec![ScriptedTurn {
        events: vec![
            StreamEvent::ThinkingDelta(ThinkingDelta {
                signature: None,
                text: Some("hmm".into()),
            }),
            StreamEvent::BlockEnd,
            StreamEvent::TurnEnd(TurnEnd {
                stop: StopReason::EndTurn,
                usage: Usage::new(5000, 1000, 0, 0),
            }),
        ],
    }]));
    let builder = permissive_builder(&root, provider, sink.clone());
    let mut engine = builder.build().unwrap();
    let summary = engine.turn("think".into()).await.unwrap();
    // The turn spends 6000 tokens and ends; the ceiling is not re-checked after
    // a model turn, so this turn returns Done with the spend now recorded.
    assert_eq!(summary.stop, summer_engine::Stop::Done);

    let bytes = to_bytes(&sink.records());
    let head = sink.records().last().expect("records").id;
    drop(engine);

    // Resume under a ceiling the folded spend (6000) already breaches. The
    // resumed turn checks the limit at the top before generating, so it stops at
    // once — proving the spend survived the restart.
    let resume_sink = CapturingSink::default();
    let resume_provider: Box<dyn Provider> = Box::new(MockProvider::new(vec![ScriptedTurn {
        events: done_turn("never reached"),
    }]));
    let resume_builder = permissive_builder(&root, resume_provider, resume_sink).limits(Limits {
        max_tokens: 4000,
        ..Limits::defaults()
    });
    let mut resumed = resume_builder.resume(Cursor::new(&bytes), head).unwrap();
    let summary = resumed.turn("continue".into()).await.unwrap();
    assert_eq!(
        summary.stop,
        summer_engine::Stop::HitLimit(summer_engine::LimitKind::Tokens),
        "the folded spend survived the restart"
    );
}

#[tokio::test]
async fn in_place_resume_folds_a_journal_into_the_running_engine() {
    // `/resume` folds a past journal into an already-running engine, reusing its
    // own broker/provider/sink (no second sandbox enter). The fold logic is the
    // same as the builder path; this proves the engine accepts it in place and
    // keeps working.
    let (_dir, root) = repo();

    // Engine A runs one turn; its journal is the resume source.
    let sink_a = CapturingSink::default();
    let provider_a: Box<dyn Provider> = Box::new(MockProvider::new(vec![ScriptedTurn {
        events: done_turn("past turn"),
    }]));
    let mut engine_a = permissive_builder(&root, provider_a, sink_a.clone())
        .build()
        .expect("engine A builds");
    engine_a.turn("hello".to_string()).await.expect("turn A");
    let expected = sink_a.records().len();
    let bytes = to_bytes(&sink_a.records());

    // Engine B is fresh; resume folds A's journal into it in place.
    let sink_b = CapturingSink::default();
    let provider_b: Box<dyn Provider> = Box::new(MockProvider::new(vec![ScriptedTurn {
        events: done_turn("after resume"),
    }]));
    let mut engine_b = permissive_builder(&root, provider_b, sink_b.clone())
        .build()
        .expect("engine B builds");
    let folded = engine_b
        .resume(std::io::Cursor::new(bytes))
        .expect("resume folds");
    assert_eq!(folded, expected, "resume folds every record");

    // The engine still runs: a new turn is written to B's (running) journal.
    engine_b.turn("continue".to_string()).await.expect("turn B");
    assert!(
        !sink_b.records().is_empty(),
        "the post-resume turn was written to the running journal"
    );
}

#[tokio::test]
async fn in_place_resume_keeps_the_running_journal_walkable() {
    // **The corruption `/resume` used to cause, and the reason it went
    // unnoticed for a whole session.** Adopting the foreign journal's head meant
    // every record B recorded afterwards named a parent that appears nowhere in
    // B's own file. Nothing failed at the time. The damage surfaced on the next
    // restart: the walk hit the dangling parent, truncated silently, and
    // everything after the `/resume` point was gone -- in the feature whose
    // entire purpose is not losing work.
    //
    // The property asserted is the one that was broken: every parent id in the
    // running journal resolves within that journal.
    let (_dir, root) = repo();

    let sink_a = CapturingSink::default();
    let provider_a: Box<dyn Provider> = Box::new(MockProvider::new(vec![ScriptedTurn {
        events: done_turn("past turn"),
    }]));
    let mut engine_a = permissive_builder(&root, provider_a, sink_a.clone())
        .build()
        .expect("engine A builds");
    engine_a.turn("hello".to_string()).await.expect("turn A");
    let source_bytes = to_bytes(&sink_a.records());
    let source_head = sink_a.records().last().expect("engine A wrote records").id;

    let sink_b = CapturingSink::default();
    let provider_b: Box<dyn Provider> = Box::new(MockProvider::new(vec![ScriptedTurn {
        events: done_turn("after resume"),
    }]));
    let mut engine_b = permissive_builder(&root, provider_b, sink_b.clone())
        .build()
        .expect("engine B builds");
    engine_b
        .resume(std::io::Cursor::new(source_bytes))
        .expect("resume folds");
    engine_b.turn("continue".to_string()).await.expect("turn B");

    let records = sink_b.records();
    let own_ids: std::collections::HashSet<RecordId> =
        records.iter().map(|record| record.id).collect();
    for record in &records {
        if let Some(parent) = record.parent {
            assert!(
                own_ids.contains(&parent),
                "record {} names a parent from another journal: {parent}",
                record.id
            );
        }
    }

    // And the fold is recorded rather than merely happening, so `/status` and
    // the monitor can both say where the running context came from.
    let folded = records
        .iter()
        .find_map(|record| match &record.kind {
            RecordKind::Folded(folded) => Some(folded),
            _ => None,
        })
        .expect("the fold is journaled");
    assert_eq!(folded.source_head, source_head);
    assert!(folded.records > 0);
}

#[tokio::test]
async fn in_place_resume_rejects_an_empty_journal() {
    let (_dir, root) = repo();
    let sink = CapturingSink::default();
    let provider: Box<dyn Provider> = Box::new(MockProvider::new(Vec::new()));
    let mut engine = permissive_builder(&root, provider, sink)
        .build()
        .expect("engine builds");
    let error = engine
        .resume(std::io::Cursor::new(Vec::<u8>::new()))
        .expect_err("an empty journal is rejected, not a silent no-op");
    assert!(matches!(error, ResumeError::Empty), "got {error:?}");
}

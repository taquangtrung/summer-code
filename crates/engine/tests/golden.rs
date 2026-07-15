//! Phase 3 golden journals: replay a recorded session with the model mocked
//! and the tools running for real, asserting both the tool-call sequence and the
//! resulting workspace tree.
//!
//! A recorded journal is produced by a first pass with a scripted [`MockProvider`]
//! (the only thing mocked is the model), serialized through the real JSONL codec,
//! and read back into a [`Recorded`] provider. Replay then drives a fresh engine
//! against an ephemeral workspace seeded from the same initial tree, and the
//! `Recorded` provider answers positionally regardless of what the tools return.
//!
//! That last clause is the point: the recording ignores tool results, so the call
//! sequence is invariant to whether the tools did the right thing. Asserting the
//! calls alone would pass for an edit tool that applies a *wrong* patch. Each
//! golden test therefore snapshots the resulting tree, which is what catches the
//! harness — dispatch, validation, the broker, the tools themselves.

// Test-fixture fs (writing a temp repo to read from), not agent-driven work.
#![allow(clippy::disallowed_methods)]

use std::collections::HashSet;
use std::io::Cursor;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::channel;

use journal::{
    Block, CallId, JournalSink, Record, RecordId, RecordKind, SCHEMA_VERSION, SessionId,
    SessionStarted, StopReason, Usage, write_records,
};
use provider::{
    MockProvider, Provider, Recorded, ScriptedTurn, StreamEvent, ThinkingDelta, ToolCallStart,
    TurnEnd,
};
use sandbox::Broker;
use summer_engine::{Engine, EngineBuilder, Limits};
use tools::{Builtin, Dispatcher, ToolSource};

// ========================================================================
// Data Structures
// ========================================================================

/// A sink that keeps every record so a recorded journal can be serialized and a
/// replay's calls can be asserted on the journal the replay actually produced.
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

/// A per-test monotonic counter, so each ephemeral workspace is distinct without
/// pulling in a temp-dir crate.
static REPO_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Seed an ephemeral workspace with the same initial tree every time, so record
/// and replay start from an identical repo.
fn fresh_repo(tag: &str) -> std::path::PathBuf {
    let n = REPO_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("summer-golden-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("README.md"), "# project\n").unwrap();
    dir
}

/// Build an engine rooted at `repo` with `provider` and a permissive broker.
fn engine(repo: &std::path::Path, provider: Box<dyn Provider>, sink: CapturingSink) -> Engine {
    let (_decisions, permissions) = channel();
    EngineBuilder::new(
        repo.to_path_buf(),
        "system".into(),
        provider,
        Dispatcher::new(Builtin::new().tools()).expect("built-in names are unique"),
        Arc::new(Broker::permissive(repo.to_path_buf())),
        Box::new(sink),
        permissions,
    )
    .limits(Limits::defaults())
    .build()
    .unwrap()
}

/// Serialize a recorded journal to JSONL bytes, the way a real file would hold
/// them. Replay reads these bytes back through the codec.
fn to_bytes(records: &[Record]) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_records(&mut bytes, records).unwrap();
    bytes
}

/// Read a serialized journal back and walk from its head.
fn walk_of(bytes: &[u8]) -> journal::Walk {
    let records = journal::read_records(Cursor::new(bytes)).unwrap();
    let head = records.last().expect("at least SessionStarted").id;
    journal::Journal::from_records(records)
        .expect("unique ids")
        .walk(head)
        .unwrap()
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

/// The edit tool's target paths, in journal order. The call sequence a recorded
/// provider reproduces positionally.
fn edit_paths(records: &[Record]) -> Vec<String> {
    records
        .iter()
        .filter_map(|record| match &record.kind {
            RecordKind::ToolCall(call) if call.name == "edit" => call
                .args
                .get("path")
                .and_then(|v| v.as_str())
                .map(Into::into),
            _ => None,
        })
        .collect()
}

/// Every `tool_use` in the journal was answered by a `tool_result`.
fn calls_and_results_pair_up(records: &[Record]) -> bool {
    let mut called = HashSet::new();
    let mut answered = HashSet::new();
    for record in records {
        match &record.kind {
            RecordKind::ToolCall(call) => {
                called.insert(call.call.clone());
            }
            RecordKind::ToolResult(result) => {
                answered.insert(result.call.clone());
            }
            _ => {}
        }
    }
    !called.is_empty() && called == answered
}

/// A one-turn journal: a session root, a user message, then a single model turn.
/// For forcing a `Recorded` provider to exhaust.
fn one_turn_journal(turn_events: Vec<StreamEvent>) -> (Vec<u8>, RecordId) {
    let session = SessionId::new();
    let root = RecordId::new();
    let mut records = vec![
        Record {
            at: std::time::SystemTime::UNIX_EPOCH,
            id: root,
            kind: RecordKind::SessionStarted(SessionStarted::new(
                SCHEMA_VERSION,
                "/repo".into(),
                "claude-opus-4-8".into(),
                "p".into(),
                "p".into(),
                "t".into(),
            )),
            parent: None,
            session,
        },
        Record {
            at: std::time::SystemTime::UNIX_EPOCH,
            id: RecordId::new(),
            kind: RecordKind::UserMessage { text: "go".into() },
            parent: Some(root),
            session,
        },
    ];
    // Rebuild the ModelTurn from the stream events the loop would have folded.
    let blocks = fold_scripted_blocks(&turn_events);
    let stop = turn_events
        .iter()
        .find_map(|event| match event {
            StreamEvent::TurnEnd(end) => Some(end.stop.clone()),
            _ => None,
        })
        .unwrap_or(StopReason::EndTurn);
    let last = RecordId::new();
    records.push(Record {
        at: std::time::SystemTime::UNIX_EPOCH,
        id: last,
        kind: RecordKind::ModelTurn(journal::ModelTurn::new(blocks, stop, Usage::zero())),
        parent: Some(records[1].id),
        session,
    });
    let bytes = to_bytes(&records);
    (bytes, last)
}

/// Fold the block-producing events of a scripted turn into `Block`s, mirroring
/// the loop. Only what the golden fixtures use: text and tool_use.
fn fold_scripted_blocks(events: &[StreamEvent]) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut text = String::new();
    let mut tool: Option<(CallId, String, String)> = None;
    for event in events {
        match event {
            StreamEvent::TextDelta(delta) => text.push_str(delta),
            StreamEvent::ToolCallStart(start) => {
                tool = Some((start.call.clone(), start.name.clone(), String::new()));
            }
            StreamEvent::ToolCallDelta(delta) => {
                if let Some((_, _, args)) = &mut tool {
                    args.push_str(delta);
                }
            }
            StreamEvent::BlockEnd => {
                if let Some((call, name, args)) = tool.take() {
                    let parsed = serde_json::from_str(&args).unwrap_or(serde_json::Value::Null);
                    blocks.push(Block::ToolUse(journal::BlockToolUse::new(
                        call, name, parsed,
                    )));
                } else if !text.is_empty() {
                    blocks.push(Block::Text {
                        text: std::mem::take(&mut text),
                    });
                }
            }
            _ => {}
        }
    }
    if !text.is_empty() {
        blocks.push(Block::Text { text });
    }
    blocks
}

// ========================================================================
// Tests
// ========================================================================

#[tokio::test]
async fn record_then_replay_reproduces_calls_and_tree() {
    // Record: a scripted model creates a file, then finishes. The journal is
    // serialized to bytes as a real file would hold it.
    let repo = fresh_repo("record");
    let sink = CapturingSink::default();
    let mock: Box<dyn Provider> = Box::new(MockProvider::new(vec![
        ScriptedTurn {
            events: edit_turn("toolu_1", "greeting.txt", "hello\n"),
        },
        ScriptedTurn {
            events: done_turn("created greeting.txt"),
        },
    ]));
    let mut recorder = engine(&repo, mock, sink.clone());
    recorder.turn("create greeting.txt".into()).await.unwrap();
    let bytes = to_bytes(&sink.records());
    assert_eq!(
        edit_paths(&sink.records()),
        vec!["greeting.txt".to_string()]
    );

    // Replay: a fresh repo with the same initial tree, a Recorded provider built
    // from the serialized journal, and tools executing for real. The model is
    // mocked; the harness is not.
    let replay_repo = fresh_repo("replay");
    let walk = walk_of(&bytes);
    let recorded: Box<dyn Provider> = Box::new(Recorded::from_walk(&walk).unwrap());
    let replay_sink = CapturingSink::default();
    let mut replay = engine(&replay_repo, recorded, replay_sink.clone());
    replay.turn("create greeting.txt".into()).await.unwrap();

    let replay_records = replay_sink.records();

    // The call sequence is reproduced positionally.
    assert_eq!(
        edit_paths(&replay_records),
        vec!["greeting.txt".to_string()],
        "replay diverged from the recording"
    );
    assert!(
        calls_and_results_pair_up(&replay_records),
        "every tool_use must be answered on replay too"
    );

    // The tree: the only assertion that catches a wrong patch. The recorded
    // provider would answer identically for an edit that wrote the wrong bytes,
    // so the call sequence alone could not detect it.
    let written = std::fs::read_to_string(replay_repo.join("greeting.txt")).unwrap();
    assert_eq!(written, "hello\n", "the edit tool did not run for real");
}

#[tokio::test]
async fn replay_of_a_multi_tool_turn_runs_every_call() {
    // Two edits in one turn: the live loop answers them in a single user
    // message, and replay re-executes both.
    let repo = fresh_repo("multi-record");
    let sink = CapturingSink::default();
    let mock: Box<dyn Provider> = Box::new(MockProvider::new(vec![
        ScriptedTurn {
            events: vec![
                StreamEvent::ToolCallStart(ToolCallStart {
                    call: CallId::new("toolu_a".into()),
                    name: "edit".into(),
                }),
                StreamEvent::ToolCallDelta(
                    "{\"path\":\"a.txt\",\"action\":\"create\",\"content\":\"A\"}".into(),
                ),
                StreamEvent::BlockEnd,
                StreamEvent::ToolCallStart(ToolCallStart {
                    call: CallId::new("toolu_b".into()),
                    name: "edit".into(),
                }),
                StreamEvent::ToolCallDelta(
                    "{\"path\":\"b.txt\",\"action\":\"create\",\"content\":\"B\"}".into(),
                ),
                StreamEvent::BlockEnd,
                StreamEvent::TurnEnd(TurnEnd {
                    stop: StopReason::ToolUse,
                    usage: Usage::new(10, 0, 0, 0),
                }),
            ],
        },
        ScriptedTurn {
            events: done_turn("done"),
        },
    ]));
    let mut recorder = engine(&repo, mock, sink.clone());
    recorder.turn("write two files".into()).await.unwrap();
    let bytes = to_bytes(&sink.records());

    let replay_repo = fresh_repo("multi-replay");
    let walk = walk_of(&bytes);
    let recorded: Box<dyn Provider> = Box::new(Recorded::from_walk(&walk).unwrap());
    let replay_sink = CapturingSink::default();
    let mut replay = engine(&replay_repo, recorded, replay_sink.clone());
    replay.turn("write two files".into()).await.unwrap();

    assert_eq!(
        edit_paths(&replay_sink.records()),
        vec!["a.txt".to_string(), "b.txt".to_string()],
        "both edits replayed in order"
    );
    assert_eq!(
        std::fs::read_to_string(replay_repo.join("a.txt")).unwrap(),
        "A"
    );
    assert_eq!(
        std::fs::read_to_string(replay_repo.join("b.txt")).unwrap(),
        "B"
    );
}

#[tokio::test]
async fn replay_reproduces_thinking_blocks_with_their_signatures() {
    // A thinking block must survive replay byte-identical: `signature` is opaque
    // and the API rejects a modified block, so a journal that mangled it could
    // not feed the next turn. This checks the full record → codec → replay path.
    let repo = fresh_repo("thinking");
    let sink = CapturingSink::default();
    let mock: Box<dyn Provider> = Box::new(MockProvider::new(vec![ScriptedTurn {
        events: vec![
            StreamEvent::ThinkingDelta(ThinkingDelta {
                signature: None,
                text: Some("planning".into()),
            }),
            StreamEvent::ThinkingDelta(ThinkingDelta {
                signature: Some("EqoBCkgIAx==".into()),
                text: None,
            }),
            StreamEvent::BlockEnd,
            StreamEvent::TurnEnd(TurnEnd {
                stop: StopReason::EndTurn,
                usage: Usage::zero(),
            }),
        ],
    }]));
    let mut recorder = engine(&repo, mock, sink.clone());
    recorder.turn("think".into()).await.unwrap();
    let bytes = to_bytes(&sink.records());

    let replay_repo = fresh_repo("thinking-replay");
    let walk = walk_of(&bytes);
    let recorded: Box<dyn Provider> = Box::new(Recorded::from_walk(&walk).unwrap());
    let replay_sink = CapturingSink::default();
    let mut replay = engine(&replay_repo, recorded, replay_sink.clone());
    replay.turn("think".into()).await.unwrap();

    let replay_records = replay_sink.records();
    let turn = replay_records
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
            Block::Thinking(t) => Some(t),
            _ => None,
        })
        .expect("the thinking block survived");
    assert_eq!(thinking.signature, "EqoBCkgIAx==");
    assert_eq!(thinking.text, "planning");
}

#[tokio::test]
async fn replay_with_a_missing_turn_surfaces_an_error() {
    // Divergence: the loop asks for a turn the recording does not have. The
    // Recorded provider answers positionally, so a recording shorter than the
    // run is a failure the loop surfaces rather than a silent hang.
    let (bytes, _head) = one_turn_journal(edit_turn("toolu_1", "one.txt", "1"));
    let walk = walk_of(&bytes);
    let recorded: Box<dyn Provider> = Box::new(Recorded::from_walk(&walk).unwrap());

    let repo = fresh_repo("exhaust");
    let sink = CapturingSink::default();
    let mut replay = engine(&repo, recorded, sink);
    // The recorded turn asks for a tool, so the loop comes back for a second
    // turn the one-turn recording does not have.
    let result = replay.turn("write one".into()).await;
    assert!(
        result.is_err(),
        "an exhausted recording must surface an error, not complete"
    );
}

//! The replay provider: mocks the model, replays a recorded journal's turns.
//!
//! [`Recorded::from_walk`] takes a [`Walk`] with compaction drops already
//! applied, keeps the `ModelTurn` records, and discards the rest. It hands back
//! turn N on the Nth call to [`Provider::turn`], ignoring the request entirely:
//! replay is positional. Because it implements [`Provider`], replay needs no flag
//! in the loop — the loop cannot tell it apart from a live model.
//!
//! **Order matters: drops first, then filter `ModelTurn`.** A walk that yielded
//! compacted-away `ModelTurn`s would make `Recorded` count turns the live loop
//! never saw, and replay of any compacted session would silently return the wrong
//! turn from that point on. [`Walk`] applies drops during traversal, so this
//! crate never has to know compaction exists.
//!
//! **Replay mocks the model only; tools re-execute for real.** The recording
//! answers regardless of what the tools returned, so a run where every tool
//! errored still replays to the same turns. Replay reproduces the tool side; it
//! says nothing about what the agent would choose again. `bench` is the
//! only thing that answers that.
//!
//! Both `from_walk` and the resume path check `schema_version` and refuse a
//! journal they would misread, naming the version they found: a journal written
//! against a later schema is not a fixture to guess at.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use thiserror::Error;

use journal::{Block, ModelTurn, Walk, check_version};

use crate::mock::estimate_token_count;
use crate::provider::{
    Provider, ProviderError, StreamEvent, ThinkingDelta, ToolCallStart, TurnEnd, TurnRequest,
};

// ========================================================================
// Data Structures
// ========================================================================

/// A [`Provider`] that replays the `ModelTurn`s of a recorded walk, in order.
///
/// Turn N is the Nth surviving `ModelTurn` on the walk. The request is ignored:
/// a prompt or tool-description change makes the loop diverge from the
/// recording, and that is the fixture property, not a bug.
pub struct Recorded {
    model: String,
    turns: Mutex<VecDeque<Vec<StreamEvent>>>,
}

/// Failures building a [`Recorded`] provider from a walk.
#[derive(Debug, Error)]
pub enum ReplayError {
    /// The journal was written against a schema version this build cannot read.
    #[error(transparent)]
    Journal(#[from] journal::JournalError),
    /// The walk has no `SessionStarted`, so its schema version is unknown.
    #[error("the journal walk has no SessionStarted record")]
    NoSession,
}

// ========================================================================
// Recorded Implementations
// ========================================================================

impl Recorded {
    /// Build a replay provider from a walk, keeping only its `ModelTurn`s.
    ///
    /// The walk must already have compaction drops applied (which [`Walk`] does
    /// during traversal); this function never sees a compacted-away turn.
    pub fn from_walk(walk: &Walk) -> Result<Self, ReplayError> {
        let mut turns = VecDeque::new();
        let mut model = None;
        for record in walk.records() {
            match &record.kind {
                journal::RecordKind::SessionStarted(started) => {
                    check_version(started.schema_version)?;
                    model = Some(started.model.clone());
                }
                journal::RecordKind::ModelTurn(turn) => {
                    turns.push_back(model_turn_to_events(turn));
                }
                _ => {}
            }
        }
        let Some(model) = model else {
            return Err(ReplayError::NoSession);
        };
        Ok(Self {
            model,
            turns: Mutex::new(turns),
        })
    }
}

#[async_trait]
impl Provider for Recorded {
    /// The model the *recording* was made against, carried through from the
    /// journal. A replay is not talking to a model, but the turns it serves came
    /// from one, and a session that replays them was shaped by it.
    fn model(&self) -> &str {
        &self.model
    }

    async fn turn(
        &self,
        _req: TurnRequest,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let turn = self
            .turns
            .lock()
            .expect("recorded turns lock poisoned")
            .pop_front()
            .ok_or_else(|| ProviderError::Fatal {
                kind: "replay_exhausted".to_string(),
                message: "the recording has no turn for this request; replay diverged".to_string(),
                // No server was involved at all: this is a replay divergence.
                status: None,
            })?;
        Ok(stream::iter(turn).boxed())
    }

    /// The local estimate. Keeps golden tests offline and is honest about what
    /// they test: the loop, not the arithmetic.
    async fn count_tokens(&self, req: &TurnRequest) -> Result<u32, ProviderError> {
        Ok(estimate_token_count(req))
    }
}

// ========================================================================
// Functions
// ========================================================================

/// Reproduce a recorded [`ModelTurn`] as the [`StreamEvent`] sequence the loop's
/// accumulator folds back into the same blocks.
///
/// One delta closes each block (a `BlockEnd` follows it), mirroring the shape a
/// well-formed stream produces. The accumulator parses the open `tool_use`'s
/// args at `BlockEnd`, so the args are re-serialized here and re-parsed there;
/// the round trip is exact for any `serde_json::Value`.
fn model_turn_to_events(turn: &ModelTurn) -> Vec<StreamEvent> {
    let mut events = Vec::with_capacity(turn.blocks.len() * 2 + 1);
    for block in &turn.blocks {
        match block {
            Block::Text { text } => {
                events.push(StreamEvent::TextDelta(text.clone()));
                events.push(StreamEvent::BlockEnd);
            }
            Block::Thinking(thinking) => {
                events.push(StreamEvent::ThinkingDelta(ThinkingDelta {
                    signature: Some(thinking.signature.clone()),
                    text: Some(thinking.text.clone()),
                }));
                events.push(StreamEvent::BlockEnd);
            }
            Block::ToolUse(use_) => {
                events.push(StreamEvent::ToolCallStart(ToolCallStart {
                    call: use_.call.clone(),
                    name: use_.name.clone(),
                }));
                events.push(StreamEvent::ToolCallDelta(
                    serde_json::to_string(&use_.args).unwrap_or_default(),
                ));
                events.push(StreamEvent::BlockEnd);
            }
        }
    }
    events.push(StreamEvent::TurnEnd(TurnEnd {
        stop: turn.stop.clone(),
        usage: turn.usage,
    }));
    events
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;
    use journal::{
        Block, BlockThinking, BlockToolUse, CallId, Journal, ModelTurn, Record, RecordId,
        RecordKind, SessionId, SessionStarted, StopReason, Usage,
    };

    use super::*;
    use crate::provider::{Role, TurnMessage};

    /// A helper that stamps a parent chain across a sequence of record kinds and
    /// exposes the head and the indexed journal.
    struct Chain {
        session: SessionId,
        records: Vec<Record>,
        parent: Option<RecordId>,
    }

    impl Chain {
        fn new() -> Self {
            Self {
                session: SessionId::new(),
                records: Vec::new(),
                parent: None,
            }
        }

        fn push(&mut self, kind: RecordKind) -> RecordId {
            let id = RecordId::new();
            self.records.push(Record {
                at: std::time::SystemTime::UNIX_EPOCH,
                id,
                kind,
                parent: self.parent,
                session: self.session,
            });
            self.parent = Some(id);
            id
        }

        fn journal(self) -> Journal {
            Journal::from_records(self.records).expect("unique ids")
        }
    }

    /// A walk is built from a parent chain; this helper stamps one for each
    /// record in order so the walk reaches every record.
    fn walk_of(kinds: Vec<RecordKind>) -> Walk {
        let session = SessionId::new();
        let mut parent = None;
        let mut records = Vec::with_capacity(kinds.len());
        for kind in kinds {
            let id = RecordId::new();
            records.push(Record {
                at: std::time::SystemTime::UNIX_EPOCH,
                id,
                kind,
                parent,
                session,
            });
            parent = Some(id);
        }
        let head = parent.expect("at least one record");
        Journal::from_records(records)
            .expect("unique ids")
            .walk(head)
            .unwrap()
    }

    fn session_started(version: u32) -> RecordKind {
        RecordKind::SessionStarted(SessionStarted::new(
            version,
            "/repo".into(),
            "claude-opus-4-8".into(),
            "policy".into(),
            "prompt".into(),
            "tools".into(),
        ))
    }

    fn model_turn(blocks: Vec<Block>, stop: StopReason) -> RecordKind {
        RecordKind::ModelTurn(ModelTurn::new(blocks, stop, Usage::new(1, 2, 0, 0)))
    }

    fn empty_request() -> TurnRequest {
        TurnRequest {
            messages: vec![],
            system: String::new(),
            tools: vec![],
        }
    }

    /// A faithful, minimal accumulator over the event sequence
    /// [`model_turn_to_events`] emits, mirroring the loop's own fold. Lives here
    /// rather than shared because the loop's accumulator is in `engine`,
    /// which depends on this crate; the engine's golden test covers the full
    /// round trip through the real accumulator.
    async fn accumulate(events: BoxStream<'static, StreamEvent>) -> Option<ModelTurn> {
        let mut blocks = Vec::new();
        let mut text = String::new();
        let mut in_text = false;
        let mut thinking_text = String::new();
        let mut thinking_sig = String::new();
        let mut in_thinking = false;
        let mut tool: Option<(CallId, String)> = None;
        let mut tool_args = String::new();
        let mut end = None;
        let mut events = events;
        while let Some(event) = events.next().await {
            match event {
                StreamEvent::TextDelta(delta) => {
                    in_text = true;
                    text.push_str(&delta);
                }
                StreamEvent::ThinkingDelta(delta) => {
                    in_thinking = true;
                    if let Some(t) = delta.text {
                        thinking_text.push_str(&t);
                    }
                    if let Some(s) = delta.signature {
                        thinking_sig.push_str(&s);
                    }
                }
                StreamEvent::ToolCallStart(start) => {
                    tool = Some((start.call, start.name));
                    tool_args.clear();
                }
                StreamEvent::ToolCallDelta(delta) => tool_args.push_str(&delta),
                StreamEvent::BlockEnd => {
                    if in_text {
                        blocks.push(Block::Text {
                            text: std::mem::take(&mut text),
                        });
                        in_text = false;
                    } else if in_thinking {
                        blocks.push(Block::Thinking(BlockThinking::new(
                            std::mem::take(&mut thinking_sig),
                            std::mem::take(&mut thinking_text),
                        )));
                        in_thinking = false;
                    } else if let Some((call, name)) = tool.take() {
                        let args =
                            serde_json::from_str(&tool_args).unwrap_or(serde_json::Value::Null);
                        blocks.push(Block::ToolUse(BlockToolUse::new(call, name, args)));
                    }
                }
                StreamEvent::TurnEnd(turn_end) => end = Some(turn_end),
                StreamEvent::TurnRestarted => {
                    blocks.clear();
                    end = None;
                }
            }
        }
        let end = end?;
        Some(ModelTurn::new(blocks, end.stop, end.usage))
    }

    #[test]
    fn keeps_only_model_turns_in_order() {
        let walk = walk_of(vec![
            session_started(0),
            RecordKind::UserMessage { text: "hi".into() },
            model_turn(
                vec![Block::Text {
                    text: "first".into(),
                }],
                StopReason::EndTurn,
            ),
            RecordKind::ToolCall(journal::ToolCall::new(
                CallId::new("toolu_1".into()),
                "read".into(),
                serde_json::Value::Null,
            )),
            model_turn(
                vec![Block::Text {
                    text: "second".into(),
                }],
                StopReason::EndTurn,
            ),
        ]);
        let recorded = Recorded::from_walk(&walk).unwrap();
        let turns = recorded.turns.lock().unwrap();
        assert_eq!(turns.len(), 2, "only ModelTurns survive");
    }

    #[test]
    fn refuses_an_unknown_schema_version() {
        let walk = walk_of(vec![session_started(99)]);
        let error = match Recorded::from_walk(&walk) {
            Ok(_) => panic!("an unknown schema version must be refused"),
            Err(error) => error,
        };
        assert!(
            matches!(error, ReplayError::Journal(ref je) if matches!(je, journal::JournalError::Version { found: 99, .. })),
            "expected a version refusal naming 99, got {error:?}"
        );
    }

    #[test]
    fn refuses_a_walk_without_a_session() {
        // A head that is not downstream of any SessionStarted: the version is
        // unknown, and guessing would misread the journal.
        let walk = walk_of(vec![model_turn(
            vec![Block::Text {
                text: "orphan".into(),
            }],
            StopReason::EndTurn,
        )]);
        let error = match Recorded::from_walk(&walk) {
            Ok(_) => panic!("a sessionless walk must be refused"),
            Err(error) => error,
        };
        assert!(matches!(error, ReplayError::NoSession));
    }

    #[test]
    fn nested_compaction_keeps_only_the_truly_surviving_turns() {
        // c1 drops [e1, e2]; c2 drops [e3, c1]. The walk's drop set is the union
        // of every Compacted on the path, so only e0 and e4 survive as
        // ModelTurns. A Recorded that did not apply drops first would count the
        // dropped turns and return the wrong one.
        let mut chain = Chain::new();
        chain.push(session_started(0));
        chain.push(model_turn(
            vec![Block::Text {
                text: "first".into(),
            }],
            StopReason::EndTurn,
        ));
        let dropped_a = chain.push(model_turn(
            vec![Block::Text { text: "a".into() }],
            StopReason::EndTurn,
        ));
        let dropped_b = chain.push(model_turn(
            vec![Block::Text { text: "b".into() }],
            StopReason::EndTurn,
        ));
        let dropped_c = chain.push(model_turn(
            vec![Block::Text { text: "c".into() }],
            StopReason::EndTurn,
        ));
        chain.push(model_turn(
            vec![Block::Text {
                text: "last".into(),
            }],
            StopReason::EndTurn,
        ));
        let first_compacted = chain.push(RecordKind::Compacted(journal::Compacted::new(
            vec![dropped_a, dropped_b],
            journal::Usage::zero(),
            "first".into(),
        )));
        let head = chain.push(RecordKind::Compacted(journal::Compacted::new(
            vec![dropped_c, first_compacted],
            journal::Usage::zero(),
            "second".into(),
        )));

        let walk = chain.journal().walk(head).unwrap();
        let recorded = Recorded::from_walk(&walk).unwrap();
        let turns = recorded.turns.lock().unwrap();
        assert_eq!(
            turns.len(),
            2,
            "only the first and last ModelTurns survive both compactions"
        );
    }

    #[tokio::test]
    async fn reproduces_the_recorded_turn_byte_for_byte() {
        // A turn with one of each block kind: the stream events must fold back
        // into the exact same blocks, signature and args included.
        let blocks = vec![
            Block::Thinking(BlockThinking::new("sig==".into(), "reasoning".into())),
            Block::ToolUse(BlockToolUse::new(
                CallId::new("toolu_7".into()),
                "edit".into(),
                serde_json::json!({"path": "src/a.rs"}),
            )),
            Block::Text {
                text: "done".into(),
            },
        ];
        let walk = walk_of(vec![
            session_started(0),
            model_turn(blocks.clone(), StopReason::ToolUse),
        ]);
        let recorded = Recorded::from_walk(&walk).unwrap();

        let stream = match recorded.turn(empty_request()).await {
            Ok(stream) => stream,
            Err(error) => panic!("turn should succeed: {error}"),
        };
        let accumulated = accumulate(stream).await.expect("a TurnEnd was emitted");
        assert_eq!(accumulated.blocks, blocks);
        assert_eq!(accumulated.stop, StopReason::ToolUse);
    }

    #[tokio::test]
    async fn an_exhausted_recording_surfaces_as_fatal() {
        // Divergence: the loop asks for a turn the recording does not have. That
        // is a failure the loop surfaces, not a silent hang or a panic.
        let walk = walk_of(vec![session_started(0)]);
        let recorded = Recorded::from_walk(&walk).unwrap();
        let request = TurnRequest {
            messages: vec![TurnMessage {
                content: vec![],
                role: Role::User,
            }],
            system: String::new(),
            tools: vec![],
        };
        let result = recorded.turn(request).await;
        let error = match result {
            Ok(_) => panic!("an exhausted recording must surface an error, not a stream"),
            Err(error) => error,
        };
        assert!(
            matches!(error, ProviderError::Fatal { ref kind, .. } if kind == "replay_exhausted"),
            "expected replay_exhausted, got {error:?}"
        );
    }
}

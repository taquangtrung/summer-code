//! Session state as a fold over a journal walk, plus the history → messages
//! conversion the engine's context window runs on.
//!
//! The central thesis of the journal is that session state is **derived, not
//! stored**: every field of [`SessionState`] can be rebuilt from a [`Walk`], and
//! a field that cannot is a separate store wearing the fold's name. Resume is
//! the load-bearing consumer — it folds the walk, seeds the broker's grant cache
//! from the `Permission` scopes, and hands the engine back a conversation it can
//! continue.
//!
//! Two fields carry the weight. `granted` folds from `Permission` records (the
//! rule-scoped answers the user already gave), which is the only reading
//! consistent with the grant cache being a derived cache rather than a side
//! table: a resumed session must not re-ask what the user already approved.
//! `spend` folds from every surviving `ModelTurn`, which is what lets [`Limits`]
//! survive a restart instead of resetting the budget every time someone kills
//! the process.
//!
//! `open_calls` is the field that only exists because of resume: a `tool_use`
//! the model asked for and no `ToolResult` answered is a call that was in flight
//! (or not yet reached) when the process died, and the engine must record a
//! synthetic outcome for it (the dedup ledger refuses re-dispatch, and re-running
//! a half-applied write is not safe to guess at). It folds from the *turn's
//! blocks*, not from `ToolCall` records: serial dispatch journals `ToolCall(B)`
//! only after A's result, so a kill during A leaves B with no `ToolCall` record to
//! fold from, and an unanswered `tool_use` is a 400 on every request thereafter.
//!
//! [`Limits`]: crate::limits::Limits

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use journal::{
    Block, CallId, JournalError, Outcome, Record, RecordId, RecordKind, SessionId, Walk,
    check_version,
};
use provider::{Role, TurnBlock, TurnMessage};
use thiserror::Error;

use crate::limits::Spend;

// ========================================================================
// Data Structures
// ========================================================================

/// Everything the fold produces, and nothing else. If a field here cannot be
/// rebuilt from the walk, the "no separate store" claim is false.
#[derive(Debug)]
pub struct SessionState {
    /// The session id, from the `SessionStarted` record.
    pub id: SessionId,
    /// The session's working directory, from `SessionStarted`.
    pub cwd: PathBuf,
    /// The digests pinned at session start.
    pub digests: Digests,
    /// The surviving conversation, oldest first. Each entry carries the record
    /// ids that produced it, so compaction can name them in `Compacted.dropped`.
    pub history: Vec<HistoryEntry>,
    /// Cumulative token spend, folded from every surviving `ModelTurn`.
    pub spend: Spend,
    /// Rule-scoped answers the user already gave, folded from `Permission`
    /// scopes. Resume feeds these back via `Broker::restore`.
    pub granted: Vec<sandbox::Grant>,
    /// `ToolCall`s with no `ToolResult`: tools in flight when the process died.
    /// Resume records a synthetic outcome for each.
    pub open_calls: Vec<CallId>,
    /// Every call the engine dispatched, answered or not. Resume seeds the
    /// dedup ledger from it, so a call known to have run is not run again.
    pub dispatched: Vec<CallId>,
}

/// One model-visible history entry, paired with the record ids that produced it.
///
/// `ids` is what compaction needs to record `Compacted.dropped`: dropping an
/// entry must drop the records behind it, or a resumed walk resurrects them. A
/// `Tool` entry carries both the `ToolCall` and the `ToolResult` id, because the
/// fold rebuilds an open call from a `ToolCall` whose `ToolResult` is gone.
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub entry: Entry,
    pub ids: Vec<RecordId>,
}

/// The digests recorded on `SessionStarted`, for provenance and fixture
/// failure messages.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Digests {
    pub policy: String,
    pub prompt: String,
    pub tools: String,
}

/// One entry of model-visible history. `Summary` is where a compacted range
/// folds to; the fold positions it where the dropped range was.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// An assistant turn's blocks.
    Model(Vec<Block>),
    /// A compacted range, standing in for what it dropped.
    Summary(String),
    /// A completed tool call's result. Consecutive entries batch into one user
    /// message, mirroring how the live loop answers a turn's calls together.
    Tool { call: CallId, outcome: Outcome },
    /// A message from the user.
    User(String),
}

/// Why a walk could not be folded into session state.
#[derive(Debug, Error)]
pub enum ResumeError {
    /// Reading or version-checking the journal failed. A version this build
    /// cannot read is refused by name rather than folded into a misread state.
    #[error(transparent)]
    Journal(#[from] JournalError),
    /// The resume head is not present in the journal.
    #[error("resume head {0} is not present in the journal")]
    UnknownHead(RecordId),
    /// The walk has no `SessionStarted`, so its schema version is unknown.
    #[error("the journal walk has no SessionStarted record")]
    NoSession,
    /// Completing interrupted tool calls could not be written.
    #[error("could not complete interrupted tool calls: {0}")]
    Sink(std::io::Error),
    /// The running session's journal could not record the fold.
    #[error("could not record the fold: {0}")]
    Record(String),
    /// The journal has no records to fold.
    #[error("the journal has no records to resume")]
    Empty,
}

// ========================================================================
// Functions
// ========================================================================

/// Fold a walk into session state.
///
/// The walk arrives with compaction drops already applied ([`Walk`] does that
/// during traversal), so this fold never sees a compacted-away record. It sees
/// the surviving records in physical root-to-head order and positions each
/// surviving `Compacted` summary where its dropped range was — not at the
/// `Compacted` record's own (tail) position, which would put a summary of old
/// turns after the recent ones the model still needs.
pub fn fold(walk: Walk) -> Result<SessionState, ResumeError> {
    let records = walk.records().to_vec();
    let mut state = SessionState::empty();

    // Pre-pass over surviving records: index the surviving `Compacted` records so
    // a summary can be placed where its range was. The first surviving record
    // whose parent is a dropped id marks the end of that range; the summary goes
    // just before it. `dropped_to_compacted` maps any dropped id to the
    // `Compacted` record that owns it, so the parent check fires once per range.
    let mut dropped_to_compacted: HashMap<RecordId, RecordId> = HashMap::new();
    let mut summary_by_compacted: HashMap<RecordId, String> = HashMap::new();
    let mut compacted_ids: HashSet<RecordId> = HashSet::new();
    for record in &records {
        if let RecordKind::Compacted(compacted) = &record.kind {
            compacted_ids.insert(record.id);
            summary_by_compacted.insert(record.id, compacted.summary.clone());
            for dropped in &compacted.dropped {
                // First surviving compaction to claim a dropped id wins; nested
                // compactions subsume earlier ones, so this is the one whose
                // summary should appear.
                dropped_to_compacted.entry(*dropped).or_insert(record.id);
            }
        }
    }

    let mut inserted: HashSet<RecordId> = HashSet::new();
    let mut pending_toolcall: Option<RecordId> = None;
    for record in &records {
        // Position a summary before the first record downstream of a dropped
        // range. The parent of that record is the range's last dropped id.
        if let Some(parent) = record.parent
            && let Some(cid) = dropped_to_compacted.get(&parent)
            && inserted.insert(*cid)
        {
            let summary = summary_by_compacted.get(cid).cloned().unwrap_or_default();
            state.history.push(HistoryEntry {
                entry: Entry::Summary(summary),
                ids: vec![*cid],
            });
        }

        if let RecordKind::Compacted(compacted) = &record.kind {
            // **Spend survives compaction here, and nowhere else.** The walk
            // subtracts the dropped records before this fold ever runs, so the
            // `ModelTurn` usage inside the compacted range is simply gone. The
            // checkpoint on the record is the only remaining record of it, and
            // without adopting it a resumed session restarts its `max_tokens`
            // and `max_cost` counters near zero -- on exactly the long sessions
            // compaction exists to serve.
            state.spend.reset_to(compacted.spent);
        }

        if compacted_ids.contains(&record.id) {
            // A `Compacted` whose range was at the tail has no downstream
            // survivor to trigger the placement above, so place it here. A
            // `Compacted` already placed is skipped.
            if inserted.insert(record.id) {
                let summary = summary_by_compacted
                    .get(&record.id)
                    .cloned()
                    .unwrap_or_default();
                state.history.push(HistoryEntry {
                    entry: Entry::Summary(summary),
                    ids: vec![record.id],
                });
            }
            continue;
        }

        observe(&mut state, record, &mut pending_toolcall)?;
    }

    if state.digests == Digests::default() {
        // No SessionStarted was seen on the walk. Its schema version is
        // unknown, and folding forward would guess at a contract.
        return Err(ResumeError::NoSession);
    }
    Ok(state)
}

/// Apply one record to the accumulating state.
fn observe(
    state: &mut SessionState,
    record: &Record,
    pending_toolcall: &mut Option<RecordId>,
) -> Result<(), ResumeError> {
    match &record.kind {
        RecordKind::SessionStarted(started) => {
            check_version(started.schema_version)?;
            state.id = record.session;
            state.cwd = started.cwd.clone();
            state.digests = Digests {
                policy: started.policy_digest.clone(),
                prompt: started.prompt_digest.clone(),
                tools: started.tools_digest.clone(),
            };
        }
        RecordKind::UserMessage { text } => state.history.push(HistoryEntry {
            entry: Entry::User(text.clone()),
            ids: vec![record.id],
        }),
        RecordKind::ModelTurn(turn) => {
            state.spend.add(turn.usage);
            // A call is open from the moment the model asks for it, which is
            // here, not from the `ToolCall` record dispatch records. The
            // difference is the whole point: dispatch is serial, so a turn
            // asking for A and B records `ToolCall(A)`, dispatches it, and only
            // then records `ToolCall(B)`. A kill during A's dispatch means B
            // never reached the journal, and folding from `ToolCall` records
            // would leave B unanswered forever: the resumed session sends a
            // `tool_use` with no `tool_result` and the API rejects it. The
            // turn's blocks are the truth about what was asked.
            for block in &turn.blocks {
                if let Block::ToolUse(use_) = block {
                    state.open_calls.push(use_.call.clone());
                }
            }
            state.history.push(HistoryEntry {
                entry: Entry::Model(turn.blocks.clone()),
                ids: vec![record.id],
            });
        }
        // The call is already open: the `ModelTurn` that asked for it opened it.
        // What this record adds is that the call was *dispatched*, which is what
        // the dedup ledger is about: a call known to have run must not run
        // again, whether or not its result reached the journal.
        RecordKind::ToolCall(call) => {
            state.dispatched.push(call.call.clone());
            *pending_toolcall = Some(record.id);
        }
        RecordKind::ToolResult(result) => {
            state.open_calls.retain(|open| open != &result.call);
            let mut ids = pending_toolcall.take().map_or(Vec::new(), |id| vec![id]);
            ids.push(record.id);
            state.history.push(HistoryEntry {
                entry: Entry::Tool {
                    call: result.call.clone(),
                    outcome: result.outcome.clone(),
                },
                ids,
            });
        }
        RecordKind::Permission(permission) => {
            if let Some(grant) = crate::permission::grant_from_permission(permission) {
                state.granted.push(grant);
            }
        }
        // Spend only, and no history: a delegated run's conversation belongs to
        // its own journal, and re-injecting it here would put a sub-session's
        // turns into a context that never saw them. What the parent keeps is the
        // bill, which is why this has to be folded and not just counted live.
        RecordKind::Delegated(delegated) => state.spend.add(delegated.usage),
        // `Compacted` is placed by `fold`, not observed here.
        RecordKind::Compacted(_) => {}
        // A marker, by design. The folded session's *history* was taken into
        // memory at the time and is not part of this journal's durable record,
        // so replaying this record restores nothing; it records where the running
        // context came from, for `/status` and the monitor.
        RecordKind::Folded(_) => {}
        // Transport noise: a failure that was retried does not change session
        // state. Spend counts only completed turns.
        RecordKind::ProviderFailed(_) => {}
        // Cosmetic, not model-visible: the resume picker reads this straight
        // off the journal, so folding it into history would show the model a
        // line it never wrote.
        RecordKind::SessionTitled { .. } => {}
        // A `kind` a newer build wrote. The line decoded and its parent kept the
        // chain walkable, which is the whole point of the variant; the payload
        // is not something this build can act on.
        RecordKind::Unknown => {}
    }
    Ok(())
}

/// Rebuild the `Vec<TurnMessage>` the engine runs on from folded history.
///
/// Consecutive tool results batch into one user message, which mirrors the live
/// loop: it answers a turn's `tool_use` blocks together in a single user
/// message, and splitting them would train the model out of parallel calls.
pub fn history_to_messages(history: &[HistoryEntry]) -> Vec<TurnMessage> {
    let mut messages = Vec::new();
    let mut pending: Vec<TurnBlock> = Vec::new();
    for entry in history {
        match &entry.entry {
            Entry::User(text) | Entry::Summary(text) => {
                flush_results(&mut messages, &mut pending);
                messages.push(TurnMessage {
                    content: vec![TurnBlock::Text(text.clone())],
                    role: Role::User,
                });
            }
            Entry::Model(blocks) => {
                flush_results(&mut messages, &mut pending);
                messages.push(TurnMessage {
                    content: blocks.iter().cloned().map(block_to_turn).collect(),
                    role: Role::Assistant,
                });
            }
            Entry::Tool { call, outcome } => {
                pending.push(outcome_to_turn_result(call, outcome));
            }
        }
    }
    flush_results(&mut messages, &mut pending);
    messages
}

/// Flush any accumulated tool results into a single user message.
fn flush_results(messages: &mut Vec<TurnMessage>, pending: &mut Vec<TurnBlock>) {
    if pending.is_empty() {
        return;
    }
    let content = std::mem::take(pending);
    messages.push(TurnMessage {
        content,
        role: Role::User,
    });
}

impl SessionState {
    /// The empty state, before any record is observed. Digests are the sentinel
    /// for "no SessionStarted seen yet".
    fn empty() -> Self {
        Self {
            id: SessionId::new(),
            cwd: PathBuf::new(),
            digests: Digests::default(),
            history: Vec::new(),
            spend: Spend::default(),
            granted: Vec::new(),
            open_calls: Vec::new(),
            dispatched: Vec::new(),
        }
    }
}

/// Convert a journal block to the provider-neutral turn block. Shared by the
/// live loop (which converts a just-journaled turn into a message) and by resume
/// (which rebuilds messages from history).
pub(crate) fn block_to_turn(block: Block) -> TurnBlock {
    match block {
        Block::Text { text } => TurnBlock::Text(text),
        Block::Thinking(thinking) => TurnBlock::Thinking(provider::TurnThinking {
            signature: thinking.signature,
            text: thinking.text,
        }),
        Block::ToolUse(use_) => TurnBlock::ToolUse(provider::TurnToolUse {
            args: use_.args,
            call: use_.call,
            name: use_.name,
        }),
    }
}

/// Convert an outcome to the tool-result block the next request carries.
pub(crate) fn outcome_to_turn_result(call: &CallId, outcome: &Outcome) -> TurnBlock {
    let (content, is_error) = match outcome {
        Outcome::Ok { content } => (content.clone(), false),
        Outcome::Err { message } => (message.clone(), true),
        Outcome::Denied { reason } => (reason.clone(), true),
        Outcome::TimedOut => ("timed out".to_string(), true),
    };
    TurnBlock::ToolResult(provider::TurnToolResult {
        call: call.clone(),
        content,
        is_error,
    })
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use journal::{
        Access, Block, BlockToolUse, CallId, Decision, Journal, ModelTurn, Outcome, Permission,
        Record, RecordId, RecordKind, Scope, SessionId, SessionStarted, StopReason, ToolCall,
        ToolResult, Usage,
    };
    use provider::TurnBlock;

    use super::*;

    /// Stamp a parent chain across these record kinds and walk from the last.
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

    fn started() -> RecordKind {
        RecordKind::SessionStarted(SessionStarted::new(
            0,
            "/repo".into(),
            "claude-opus-4-8".into(),
            "pol".into(),
            "pr".into(),
            "tl".into(),
        ))
    }

    fn turn(stop: StopReason, usage: Usage) -> RecordKind {
        RecordKind::ModelTurn(ModelTurn::new(Vec::new(), stop, usage))
    }

    /// A `tool_use` turn asking for `blocks`. The shape the loop actually
    /// records: a `ToolCall` record exists only because a block asked for it.
    fn turn_of(blocks: Vec<Block>) -> RecordKind {
        RecordKind::ModelTurn(ModelTurn::new(blocks, StopReason::ToolUse, Usage::zero()))
    }

    fn tool_use(call: &str) -> Block {
        Block::ToolUse(BlockToolUse::new(
            CallId::new(call.into()),
            "read".into(),
            serde_json::Value::Null,
        ))
    }

    fn history_entries(state: &SessionState) -> Vec<Entry> {
        state.history.iter().map(|he| he.entry.clone()).collect()
    }

    #[test]
    fn fold_seeds_spend_and_digests_and_cwd() {
        let walk = walk_of(vec![
            started(),
            RecordKind::UserMessage { text: "hi".into() },
            turn(StopReason::EndTurn, Usage::new(10, 5, 0, 0)),
            turn(StopReason::EndTurn, Usage::new(20, 7, 0, 0)),
        ]);
        let state = fold(walk).unwrap();
        assert_eq!(state.cwd, PathBuf::from("/repo"));
        assert_eq!(state.digests.policy, "pol");
        assert_eq!(state.spend.tokens(), 42, "input + output + cache_write");
    }

    #[test]
    fn fold_counts_what_a_delegated_run_spent() {
        // Without this, a resumed session's runaway guard forgets every token a
        // subagent burned: those turns are in the sub-session's journal, and
        // this journal's only record of them is the `Delegated` record.
        let walk = walk_of(vec![
            started(),
            RecordKind::UserMessage { text: "hi".into() },
            turn(StopReason::EndTurn, Usage::new(10, 5, 0, 0)),
            RecordKind::Delegated(journal::Delegated::new(
                CallId::new("toolu_sub".into()),
                "/repo/.summer/sessions/x-subagent.jsonl".into(),
                Usage::new(900, 210, 0, 0),
            )),
        ]);
        let state = fold(walk).unwrap();
        assert_eq!(
            state.spend.tokens(),
            1_125,
            "15 from the turn plus 1110 delegated"
        );
    }

    #[test]
    fn open_calls_are_calls_without_results() {
        let call = CallId::new("toolu_1".into());
        let walk = walk_of(vec![
            started(),
            turn_of(vec![tool_use("toolu_1")]),
            RecordKind::ToolCall(ToolCall::new(
                call.clone(),
                "read".into(),
                serde_json::Value::Null,
            )),
        ]);
        let state = fold(walk).unwrap();
        assert_eq!(state.open_calls, vec![call]);
    }

    #[test]
    fn a_call_the_kill_beat_to_the_journal_is_still_open() {
        // Dispatch is serial: a turn asking for A and B records `ToolCall(A)`,
        // dispatches it, and only then records `ToolCall(B)`. Kill the process
        // during A's dispatch and B never reached the journal at all. Folding
        // open calls from `ToolCall` records answers A and forgets B, and the
        // resumed session then sends B's `tool_use` with no `tool_result`
        // forever after: a 400 on the first request and on every retry.
        let a = CallId::new("toolu_a".into());
        let b = CallId::new("toolu_b".into());
        let walk = walk_of(vec![
            started(),
            turn_of(vec![tool_use("toolu_a"), tool_use("toolu_b")]),
            RecordKind::ToolCall(ToolCall::new(
                a.clone(),
                "read".into(),
                serde_json::Value::Null,
            )),
            RecordKind::ToolResult(ToolResult::new(
                a,
                Outcome::Ok {
                    content: "x".into(),
                },
            )),
        ]);
        let state = fold(walk).unwrap();
        assert_eq!(
            state.open_calls,
            vec![b],
            "the call the kill interrupted never reached the journal, and must still be answered"
        );
    }

    #[test]
    fn a_completed_call_is_not_open() {
        let call = CallId::new("toolu_1".into());
        let walk = walk_of(vec![
            started(),
            turn_of(vec![tool_use("toolu_1")]),
            RecordKind::ToolCall(ToolCall::new(
                call.clone(),
                "read".into(),
                serde_json::Value::Null,
            )),
            RecordKind::ToolResult(ToolResult::new(
                call,
                Outcome::Ok {
                    content: "x".into(),
                },
            )),
        ]);
        let state = fold(walk).unwrap();
        assert!(state.open_calls.is_empty());
    }

    #[test]
    fn rule_scoped_allows_fold_into_grants() {
        let call = CallId::new("toolu_1".into());
        let rule = Permission::new(
            call,
            Access::Write {
                path: "/repo/src/a.rs".into(),
            },
            Decision::AskedAllowed,
            Scope::Rule {
                rule: "write:src/**".into(),
            },
        );
        let once = Permission::new(
            CallId::new("toolu_2".into()),
            Access::Read {
                path: "/repo/b.rs".into(),
            },
            Decision::AskedAllowed,
            Scope::Once,
        );
        let walk = walk_of(vec![
            started(),
            RecordKind::Permission(rule),
            RecordKind::Permission(once),
        ]);
        let state = fold(walk).unwrap();
        assert_eq!(state.granted.len(), 1, "Once scopes are not remembered");
    }

    #[test]
    fn fold_refuses_an_unknown_schema_version() {
        let walk = walk_of(vec![RecordKind::SessionStarted(SessionStarted::new(
            7,
            "/repo".into(),
            "claude-opus-4-8".into(),
            "p".into(),
            "p".into(),
            "t".into(),
        ))]);
        assert!(matches!(
            fold(walk).unwrap_err(),
            ResumeError::Journal(JournalError::Version { found: 7, .. })
        ));
    }

    #[test]
    fn fold_refuses_a_sessionless_walk() {
        let walk = walk_of(vec![turn(StopReason::EndTurn, Usage::zero())]);
        assert!(matches!(fold(walk).unwrap_err(), ResumeError::NoSession));
    }

    #[test]
    fn consecutive_tool_results_batch_into_one_user_message() {
        let history = vec![
            HistoryEntry {
                entry: Entry::User("hi".into()),
                ids: vec![],
            },
            HistoryEntry {
                entry: Entry::Model(vec![tool_use("toolu_1"), tool_use("toolu_2")]),
                ids: vec![],
            },
            HistoryEntry {
                entry: Entry::Tool {
                    call: CallId::new("toolu_1".into()),
                    outcome: Outcome::Ok {
                        content: "a".into(),
                    },
                },
                ids: vec![],
            },
            HistoryEntry {
                entry: Entry::Tool {
                    call: CallId::new("toolu_2".into()),
                    outcome: Outcome::Ok {
                        content: "b".into(),
                    },
                },
                ids: vec![],
            },
        ];
        let messages = history_to_messages(&history);
        assert_eq!(messages.len(), 3, "user, assistant, one batched results");
        assert_eq!(messages[2].content.len(), 2);
        assert!(matches!(messages[2].content[0], TurnBlock::ToolResult(_)));
    }

    #[test]
    fn a_summary_entry_becomes_a_user_message() {
        let history = vec![HistoryEntry {
            entry: Entry::Summary("earlier turns were compacted".into()),
            ids: vec![],
        }];
        let messages = history_to_messages(&history);
        assert_eq!(messages.len(), 1);
        assert!(matches!(messages[0].content[0], TurnBlock::Text(_)));
    }

    #[test]
    fn fold_places_a_summary_where_the_dropped_range_was() {
        // history: UserMessage, [dropped middle ModelTurn], ModelTurn(recent),
        // Compacted. The summary must land between the user message and the
        // recent turn, not after it.
        let session = SessionId::new();
        let epoch = std::time::SystemTime::UNIX_EPOCH;
        let (e0, e1, e2, e3, e4) = (
            RecordId::new(),
            RecordId::new(),
            RecordId::new(),
            RecordId::new(),
            RecordId::new(),
        );
        let records = vec![
            Record {
                at: epoch,
                id: e0,
                kind: started(),
                parent: None,
                session,
            },
            Record {
                at: epoch,
                id: e1,
                kind: RecordKind::UserMessage {
                    text: "task".into(),
                },
                parent: Some(e0),
                session,
            },
            Record {
                at: epoch,
                id: e2,
                kind: turn(StopReason::EndTurn, Usage::new(1, 1, 0, 0)),
                parent: Some(e1),
                session,
            },
            Record {
                at: epoch,
                id: e3,
                kind: turn(StopReason::EndTurn, Usage::new(3, 3, 0, 0)),
                parent: Some(e2),
                session,
            },
            Record {
                at: epoch,
                id: e4,
                kind: RecordKind::Compacted(journal::Compacted::new(
                    vec![e2],
                    journal::Usage::zero(),
                    "old turn".into(),
                )),
                parent: Some(e3),
                session,
            },
        ];
        let journal = Journal::from_records(records).expect("unique ids");
        let state = fold(journal.walk(e4).unwrap()).unwrap();

        let entries = history_entries(&state);
        // User("task"), Summary("old turn"), ModelTurn(recent). The compacted
        // middle turn is gone; the summary is between the user message and the
        // recent turn.
        assert!(
            matches!(entries[0], Entry::User(ref t) if t == "task"),
            "first user message survives at the front: {:?}",
            entries
        );
        assert!(
            matches!(entries[1], Entry::Summary(ref s) if s == "old turn"),
            "summary placed where the dropped range was: {:?}",
            entries
        );
        assert_eq!(entries.len(), 3, "user, summary, one recent turn");
    }
}

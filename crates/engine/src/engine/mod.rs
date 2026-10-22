//! The agent turn loop.
//!
//! Batched dispatch, one journal writer, enforced [`Limits`]. The loop
//! accumulates a streamed model turn into `Block`s, journals it, and if it asked
//! for tools splits them into batches, runs each batch, journals every result,
//! and loops. It
//! stops on a limit with [`Stop::HitLimit`] (an `Ok` outcome), not an error:
//! `Err` means the loop failed, `Ok` means the loop stopped, and a limit is the
//! loop doing precisely what it was told.
//!
//! **Every `tool_use` gets a `ToolResult`, without exception.** This is the rule
//! the loop is built around and the easiest one to violate by accident. The API
//! requires every `tool_use` block to be answered by a `tool_result` in the next
//! user message; a turn that leaves one unanswered produces a conversation the
//! API rejects with a 400 on the *next* request. The failure is therefore not
//! "the tool did not run", it is "the session is bricked, including on resume,
//! because the fold rebuilds the same unanswerable history".
//!
//! That rule is why the limit checks sit at the **top** of the loop, before a
//! turn is generated, rather than after the `ModelTurn` where they read more
//! naturally: a limit that fires after the model has asked for three tools has
//! already created the problem. Where a limit genuinely cannot be known before
//! the turn arrives (`max_tool_calls`), the loop answers every outstanding
//! `tool_use` with a synthetic error before returning.
//!
//! **Batching is a latency change, not a semantic one.** A model that asks to
//! read six files in one turn used to wait six round-trips of tool latency; the
//! six now overlap. What the journal shows is deliberately unchanged: calls are
//! recorded in the order the model asked, every result is recorded in that same
//! order once the batch finishes, and a replay therefore reproduces byte-exact
//! record order regardless of which call happened to finish first. Only tools
//! that declare [`Concurrency::Parallel`] share a batch, and only with
//! neighbours the model already listed adjacently, so a write never overlaps
//! anything and a read never overtakes a write the model asked for first.

mod batch;
mod builder;
mod compaction;
mod transcript;

pub use builder::EngineBuilder;

use std::io;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use serde::{Deserialize, Serialize};
use std::sync::mpsc::Receiver;

use journal::{
    CallId, Folded, Journal, ModelTurn, Outcome, Permission, ProviderFailed, Record, RecordId,
    RecordKind, SessionId, StopReason, ToolCall, ToolResult, Usage, read_records,
};
use provider::{
    Effort, Provider, ProviderError, Role, TurnBlock, TurnMessage, TurnRequest, TurnTool,
};
use sandbox::{Broker, PermissionRecord, ToolCx};
use serde_json::Value;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tools::{Dispatcher, Mechanisms};
use tracing::{info_span, instrument};

use crate::accumulate::{Accumulated, RequestedCall, accumulate_turn, requested_calls};
use crate::context::ContextManager;
use crate::engine::batch::{annotate, batch_calls, run_batch, screen_batch};
use crate::hook::{HookEvent, HookSet};
use crate::limits::{LimitKind, Limits, Spend};
use crate::monitor::Violation;
use crate::permission;
use crate::recorder::Recorder;
use crate::session::{self, Entry, HistoryEntry, ResumeError};
use crate::skill_gate::SkillGate;
use crate::ui_event::{Observer, UiEvent};

// ========================================================================
// Constants
// ========================================================================

/// What a synthetic result says when `max_tool_calls` fires mid-turn.
const TOOL_CALL_LIMIT_MESSAGE: &str = "not run: the turn's tool-call limit was reached";

/// The tool whose success means the workspace changed, so the post-edit
/// verifier has something to check.
const EDIT_TOOL: &str = "edit";

/// The tool whose output becomes the turn's active plan.
const PLAN_TOOL: &str = "plan";

/// The `plan` argument saying where the plan stands.
const PLAN_STATUS_ARG: &str = "status";

/// The `plan` statuses that mean no step is still outstanding. Anything else,
/// including the schema's default of `in_progress`, leaves the plan open.
const PLAN_CLOSED_STATUSES: &[&str] = &["blocked", "completed"];

/// What stands in the history where a rewind dropped the turns after it, so
/// the model knows why its last work is gone and that the files may not be.
const REWIND_NOTE: &str = "The user rewound the conversation to this point; the turns that came after it \
    were discarded. Any file changes those turns made are still on disk unless the user restored them.";

/// The longest [`Engine::settle`] waits for a background session title.
const TITLE_SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// How many times one turn is sent back with compiler errors before it ends
/// [`Stop::VerifyFailed`]. Two rounds fix what a model can fix from the
/// diagnostics; past that the errors are usually not its own (a tree that was
/// already broken, a toolchain that is missing), and every further round was a
/// full model turn spent until `max_turns` ran out.
const MAX_VERIFY_RETRIES: u32 = 2;

/// What the model is told when it tries to end a turn mid-plan.
const PLAN_OPEN_MESSAGE: &str = "The active plan still has steps outstanding. Finish them, or call `plan` again with \
     status `completed` or `blocked` to say where it stands.";

/// The tool that loads a skill's instructions, so the skill gate can see which
/// ones this session has actually read.
const SKILL_TOOL: &str = "skill";

/// The `edit` argument naming the file being changed.
const EDIT_PATH_ARG: &str = "path";

/// The `skill` argument naming the skill being loaded.
const SKILL_NAME_ARG: &str = "name";

/// What a resumed session tells the model about a tool that was in flight when
/// the previous process died. The call was interrupted, not failed: the model
/// sees the interruption and decides whether to retry.
const INTERRUPTED_MESSAGE: &str =
    "interrupted: the session was resumed before this call produced a result";

/// What each turn after the first is told during [`Engine::run_goal`].
///
/// Deliberately thin: the plan is already re-injected into the system prompt
/// every round, so this only has to point at it. A longer instruction here would
/// be a second, competing statement of the task.
const CONTINUE_PROMPT: &str = "Continue the active plan. Close it with status `completed` when the steps are done, or \
     `blocked` if you cannot finish, and say why.";

/// What a synthetic result says for a call the model asked for in a turn that
/// then hit `max_tokens`. The call was never dispatched: the turn was cut off.
const TRUNCATED_MESSAGE: &str =
    "not run: the turn was truncated at max_tokens before this call was dispatched";

/// The longest a generated session title may be, in words. Short enough that a
/// `/resume` picker row stays one line beside the id and the age.
const TITLE_MAX_WORDS: u8 = 6;

// ========================================================================
// Data Structures
// ========================================================================

/// The agent. Owns the session, the provider, the dispatcher, the broker, the
/// sole journal writer, and the runaway guard.
pub struct Engine {
    broker: Arc<Broker>,
    context: ContextManager,
    /// Whether `context` was chosen by the builder's caller, so a provider swap
    /// leaves it alone rather than re-sizing it to the new model.
    context_pinned: bool,
    cwd: Arc<Path>,
    dispatcher: Dispatcher,
    head: Option<RecordId>,
    /// User-configured commands run around tool calls and at the end of a turn.
    /// Empty by default, which is the loop exactly as it was.
    hooks: HookSet,
    /// The surviving conversation, oldest first. Canonical: the window is
    /// derived from it every turn and never cached.
    history: Vec<HistoryEntry>,
    limits: Limits,
    /// The optional UI observer. Absent by default, so the plain REPL and every
    /// test build an engine that emits nothing and behaves exactly as before the
    /// TUI existed. Best-effort: a send that fails (frontend gone) is dropped.
    observer: Option<Observer>,
    /// Sub-sessions that finished during a tool call, drained alongside the
    /// permission records. Empty (a dropped sender) when nothing can delegate.
    delegated: Receiver<DelegatedRun>,
    /// Every decision the broker made, drained before each `ToolResult`. The
    /// receiver never leaves the engine, which is why a `std::sync::mpsc` is
    /// enough on the broker's side.
    permissions: Receiver<PermissionRecord>,
    provider: Box<dyn Provider>,
    recorder: Recorder,
    session: SessionId,
    /// Which skills an edit requires, and which this session has loaded. Empty
    /// by default, which screens nothing.
    skills: SkillGate,
    spend: Spend,
    /// See [`EngineBuilder::summarize_compactions`].
    summarize_compactions: bool,
    system_prompt: String,
    /// See [`EngineBuilder::title_sessions`].
    title_sessions: bool,
    /// Per-turn cooperative cancellation. The TUI sets this before a turn so
    /// Ctrl-C can interrupt the in-flight tool (and, between rounds, the turn);
    /// the plain REPL never sets it. `None` means "not interruptible this turn".
    turn_cancel: Option<CancellationToken>,
    /// The paths this turn's edits actually wrote, which is what decides the
    /// checks the verifier runs. A bool used to stand here, and a bool can only
    /// ask "was anything edited": the answer to "which languages" then came from
    /// the repository root, so a script in a foreign repo went unchecked.
    edited: Vec<String>,
    /// Which quality mechanisms this engine runs. All on by default; the
    /// ablation harness is what turns one off. See
    /// [`EngineBuilder::mechanisms`](EngineBuilder::mechanisms).
    mechanisms: Mechanisms,
    /// The latest `plan` tool result. Not part of any request on its own: the
    /// result is already in history, and only a compaction that drops it
    /// carries the plan forward, inside its summary.
    active_plan: Option<String>,
    /// Whether the active plan has steps outstanding.
    plan_open: bool,
    /// The session title being generated in the background, if any. See
    /// [`Engine::title_session`].
    pending_title: Option<tokio::task::JoinHandle<Option<String>>>,
    /// The record id and text of the message that opened each turn this
    /// engine has run, oldest first: what [`Engine::rewind_turns`] can go back
    /// to. Empty after a resume, whose earlier turns this process never ran.
    turn_openers: Vec<(RecordId, String)>,
    /// Whether this turn has already been told about them. One reminder, not a
    /// loop: the model can answer it by finishing or by re-stating the plan's
    /// status, and a turn that ignores it ends [`Stop::PlanIncomplete`] rather
    /// than being asked again forever.
    plan_nudged: bool,
    /// How many times this turn has been sent back by the verify gate since it
    /// last passed. See [`MAX_VERIFY_RETRIES`].
    verify_retries: u32,
}

/// One finished sub-session, reported by whatever ran it.
///
/// The engine cannot see a delegated run: it happens inside a tool call, on an
/// engine the frontend built. This is what crosses back, and it crosses on a
/// channel for the same reason the broker's permission records do -- the work
/// happens under a `&self` tool call that cannot reach the engine's `&mut self`.
#[derive(Debug, Clone)]
pub struct DelegatedRun {
    /// The sub-session's journal file, carried through to the record so the
    /// spend can be traced to the turns that caused it.
    pub journal: String,
    pub usage: Usage,
}

/// The result of one `Engine::turn`.
#[derive(Debug)]
pub struct TurnSummary {
    pub head: RecordId,
    pub stop: Stop,
    /// The model's text from its final assistant turn.
    pub text: String,
    pub tool_calls: u32,
    pub usage: Usage,
}

/// The summary outcome of an autonomous `Engine::run_goal` execution loop.
#[derive(Debug)]
pub struct GoalSummary {
    pub completed: bool,
    pub iterations: usize,
    pub summaries: Vec<TurnSummary>,
}

impl GoalSummary {
    /// The whole run as one turn, for a caller that reports turns.
    ///
    /// The **stop is the last turn's**, and that is exact rather than a
    /// simplification: a run that stopped on the iteration ceiling stopped with
    /// its plan open, and the plan gate already ends such a turn
    /// [`Stop::PlanIncomplete`]. The counts are the **whole run's**, because
    /// what a caller wants from an autonomous run is what it cost in total, and
    /// the last turn alone would report a fraction of it.
    pub fn into_turn(self) -> TurnSummary {
        let tool_calls = self.summaries.iter().map(|turn| turn.tool_calls).sum();
        let usage = self.summaries.iter().fold(Usage::zero(), |total, turn| {
            Usage::new(
                total.input + turn.usage.input,
                total.output + turn.usage.output,
                total.cache_read + turn.usage.cache_read,
                total.cache_write + turn.usage.cache_write,
            )
        });
        let last = self
            .summaries
            .into_iter()
            .next_back()
            .expect("run_goal always takes at least one turn");
        TurnSummary {
            head: last.head,
            stop: last.stop,
            text: last.text,
            tool_calls,
            usage,
        }
    }
}

/// Why a turn ended. Every variant is the loop working: `Err` is reserved for
/// the loop being unable to proceed. Serializable because it rides inside a
/// [`UiEvent::TurnEnded`], which `--output-format stream-json` writes out; the
/// engine's own stop type is the natural payload for that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stop {
    Done,
    HitLimit(LimitKind),
    /// The model refused. An answer, not an error, and re-sending the same
    /// prompt is not a strategy.
    Refused {
        category: Option<String>,
    },
    /// The user interrupted the turn (Ctrl-C). An answer, not an error: the
    /// in-flight tool was killed and answered, and any partial assistant text
    /// was already journaled, so the session survives and resume folds cleanly.
    Cancelled,
    /// The turn ended with steps still outstanding on the active plan, after
    /// being told so once.
    ///
    /// Not `Done` with a note, because the difference is the whole point: a plan
    /// the model wrote and then walked away from is drift, and a caller that
    /// sees `Done` has no way to tell it from work that finished.
    PlanIncomplete,
    /// The edits still did not compile after the verify gate sent the model back
    /// twice.
    ///
    /// Not `Done`, for the reason `PlanIncomplete` is not: the tree this turn
    /// leaves behind does not build, and a caller has to be able to tell.
    VerifyFailed,
}

// ========================================================================
// Errors
// ========================================================================

/// The loop being unable to proceed. A ceiling is not in here: see [`Stop`].
#[derive(Debug, Error)]
pub enum EngineError {
    /// `Oversized::Refuse`: the window overflows and there is nothing left to
    /// drop. Sending anyway would be a 400 the loop chose to cause.
    #[error("context window overflowed and there is nothing left to drop")]
    ContextOverflow,
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("session could not be written: {0}")]
    Sink(io::Error),
    /// `stop_reason: max_tokens`. The output was cut off, so the turn cannot be
    /// acted on: raise `max_tokens` or drop effort.
    #[error("model output was truncated: raise max_tokens or lower effort")]
    Truncated,
}

// ========================================================================
// Engine Implementations
// ========================================================================

impl Engine {
    // --------------------------------------------------------------------
    // Inspecting the session
    // --------------------------------------------------------------------

    /// This engine's session id.
    pub fn session_id(&self) -> SessionId {
        self.session
    }

    /// Every runtime-property violation this session has produced.
    ///
    /// **Exposed because the monitor was, in practice, talking to nobody.** A
    /// violation means the harness did something the journal says it should not
    /// have -- the strongest signal the system produces -- and its only surfacing
    /// was a `warn!` that the raw-mode TUI interleaves with its own drawing or
    /// swallows entirely. A frontend can now report them where the user is
    /// looking.
    /// Everything this session has spent, across every turn it has taken.
    ///
    /// Not the last turn's usage, which is what [`TurnSummary`] carries: a
    /// caller that ran a sub-session and wants to report what it cost needs the
    /// total, and summing the summaries would miss the rounds inside a turn.
    /// `cache_read` is not carried, for the reason [`Spend::as_usage`] gives.
    pub fn spent(&self) -> Usage {
        self.spend.as_usage()
    }

    pub fn violations(&self) -> &[Violation] {
        self.recorder.violations()
    }

    /// The exact model id this engine talks to. Surfaced so a frontend's status
    /// line can name it without holding the provider itself.
    pub fn model(&self) -> &str {
        self.provider.model()
    }

    /// The session's working directory, for a frontend's status line.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    // --------------------------------------------------------------------
    // Reconfiguring a live engine
    // --------------------------------------------------------------------

    /// Attach or detach the UI observer. The TUI attaches a fresh observer per
    /// turn (so each turn drains to its own render task) and detaches after; the
    /// plain REPL never calls this and emits nothing.
    pub fn set_observer(&mut self, observer: Option<Observer>) {
        self.observer = observer;
    }

    /// Set the per-turn cancellation token. The TUI sets this before a turn and
    /// cancels it on Ctrl-C; the plain REPL leaves it unset, so the turn is not
    /// interruptible (and behaves exactly as before).
    pub fn set_turn_cancel(&mut self, cancel: Option<CancellationToken>) {
        self.turn_cancel = cancel;
    }

    /// Swap the provider a `/model` selection (or any other frontend) picked.
    ///
    /// `SessionStarted.model` still names whichever provider opened the
    /// session — a swap is not yet its own journal record, so a reader of the
    /// journal alone cannot see one happened, only a frontend querying
    /// `Engine::model` live can.
    pub fn set_provider(&mut self, provider: Box<dyn Provider>) {
        if !self.context_pinned {
            self.context = ContextManager::for_provider(provider.as_ref());
        }
        self.provider = provider;
    }

    /// Change the reasoning effort the current provider sends, live. `None`
    /// disables it (some models 400 on `output_config.effort`). Per-model in the
    /// CLI: a `/effort` call applies the chosen level without rebuilding the
    /// transport.
    pub fn set_effort(&mut self, effort: Option<Effort>) {
        self.provider.set_effort(effort);
    }

    // --------------------------------------------------------------------
    // Resuming and cancelling
    // --------------------------------------------------------------------

    /// Fold a past session's journal into this engine's history, so the model
    /// continues with that context. Unlike [`EngineBuilder::resume`] this reuses
    /// the running engine's broker, provider, and sink: the sandbox and prompt
    /// channel are unchanged, and new turns are written to the *running*
    /// session's journal. The folded history is in-memory only — a later resume
    /// of the running session does not recover it (the past session's journal is
    /// the durable record).
    ///
    /// **`self.head` stays on the running chain**, and a [`Folded`] record says
    /// what was taken in. Adopting the foreign head instead -- which is what this
    /// used to do -- gave every subsequent record in the *running* session a
    /// parent id that appears nowhere in its own file. Nothing failed at the
    /// time; the damage surfaced on the next restart, when the walk hit the
    /// dangling parent, silently truncated there, and resume of the running
    /// session failed with `NoSession`. Everything after the `/resume` point was
    /// unrecoverable, in the feature whose entire purpose is recovery.
    ///
    /// Returns the number of records folded, so a frontend can report how much it
    /// resumed. An empty journal is an error, not a silent no-op.
    pub fn resume(&mut self, journal: impl Read) -> Result<usize, ResumeError> {
        let records = read_records(BufReader::new(journal))?;
        let Some(last) = records.last() else {
            return Err(ResumeError::Empty);
        };
        let head = last.id;
        let source_session = last.session;
        let count = records.len();
        let indexed = Journal::from_records(records)?;
        let walk = indexed.walk(head).ok_or(ResumeError::UnknownHead(head))?;
        let state = session::fold(walk)?;
        self.broker.restore(state.granted);
        self.dispatcher.restore(state.dispatched);
        self.history = state.history;
        self.spend = state.spend;
        self.record(RecordKind::Folded(Folded::new(count, head, source_session)))
            .map_err(|error| ResumeError::Record(error.to_string()))?;
        // Deliberately after the marker, so the synthetic results for the other
        // session's open calls hang off this session's chain and are attributable
        // to the fold that introduced them.
        self.complete_open_calls(&state.open_calls)
            .map_err(|error| ResumeError::Record(error.to_string()))?;
        Ok(count)
    }

    /// Whether the current turn has been cancelled. A turn with no token is never
    /// cancelled.
    fn is_cancelled(&self) -> bool {
        self.turn_cancel
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
    }

    // --------------------------------------------------------------------
    // Running a turn
    // --------------------------------------------------------------------

    /// Run one user-driven turn. `Ok` for every controlled ending, including a
    /// limit; `Err` for the loop failing to proceed.
    ///
    /// This is a thin wrapper over the private `turn_inner` that exists for one
    /// reason:
    /// every `Err` exit must narrate a single [`UiEvent::Failed`], and the loop's
    /// dozen `?` propagation points make per-site emission error-prone. Routing
    /// the result here means the failure announcement is in one place, so a new
    /// `?` cannot forget it.
    ///
    /// When built with [`EngineBuilder::title_sessions`], a session's first
    /// call here also best-effort titles it; see the private `title_session`.
    pub async fn turn(&mut self, text: String) -> Result<TurnSummary, EngineError> {
        // Read before `turn_inner` pushes this turn's own entries: a brand-new
        // session's history is empty, a resumed one already carries its past
        // turns, and either way this is the only cheap way to ask "is this the
        // first turn" without a second field to keep in sync.
        let is_first_turn = self.title_sessions && self.history.is_empty();
        let user_text = text.clone();
        self.record_finished_title().await;

        let result = self.turn_inner(text).await;
        if let Err(error) = &result {
            self.emit(UiEvent::Failed {
                message: error.to_string(),
            });
        }
        if is_first_turn && let Ok(summary) = &result {
            self.title_session(&user_text, &summary.text).await;
        }
        self.record_finished_title().await;
        // On both paths, and before the result is returned. A turn that failed
        // is a turn whose journal a human is about to read, so it is the last
        // one to be worth losing. A flush that itself fails does not fail the
        // turn: the records are written either way, and reporting a completed
        // turn as an error because the OS was slow to be told about it would
        // trade a durability problem for a correctness one.
        if let Err(error) = self.recorder.flush() {
            tracing::warn!(%error, "the session could not be flushed");
        }
        result
    }

    /// Title this session from its first exchange and record it, best-effort.
    ///
    /// One extra small model call on the same provider and model the session
    /// is already using, made once, right after the first turn: a resumed
    /// session's history is never empty, so [`Engine::turn`] never calls this
    /// again. A failure here (a transport error, an empty or refused reply)
    /// leaves the session untitled rather than failing a turn that already
    /// succeeded -- the `/resume` picker's first-user-message fallback covers
    /// it either way.
    ///
    /// **In the background where the provider allows it.** The title call used
    /// to be awaited before the first turn returned, so the first reply of every
    /// session handed control back a whole model round trip late, for a title
    /// nobody reads until `/resume`. A provider that can be cloned
    /// ([`Provider::with_model`]) titles on its own task while the user reads
    /// the reply, and the title is recorded at the next turn or at
    /// [`Engine::settle`]; one that cannot (the mock, a recording) titles inline,
    /// as before.
    async fn title_session(&mut self, user_text: &str, assistant_text: &str) {
        let request = title_request(user_text, assistant_text);
        match self.provider.with_model(self.provider.model()) {
            Some(titler) => {
                self.pending_title = Some(tokio::spawn(async move {
                    generate_title(titler.as_ref(), request).await
                }));
            }
            None => {
                if let Some(title) = generate_title(self.provider.as_ref(), request).await {
                    self.record_title(title);
                }
            }
        }
    }

    /// Record the background title if it has finished, without waiting for it.
    async fn record_finished_title(&mut self) {
        if self
            .pending_title
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
            && let Some(handle) = self.pending_title.take()
            && let Ok(Some(title)) = handle.await
        {
            self.record_title(title);
        }
    }

    /// The prompt size, in tokens, at which this engine compacts history: the
    /// model's window less its output room. See [`ContextManager::for_model`].
    pub fn context_budget(&self) -> u32 {
        self.context.budget()
    }

    /// The messages that opened this engine's turns, newest first: the points
    /// [`Engine::rewind_turns`] can go back to.
    pub fn rewindable_turns(&self) -> Vec<String> {
        self.turn_openers
            .iter()
            .rev()
            .map(|(_, text)| text.clone())
            .collect()
    }

    /// Drop the last `turns` turns from the conversation, as if they had not
    /// been sent, and answer whether there were that many to drop.
    ///
    /// **A compaction of the tail, journaled like one.** The dropped records are
    /// named on a `Compacted` record carrying a short note as its summary, so
    /// a resume folds to the same rewound history, and the journal still holds
    /// every turn for anyone auditing what happened. `false`, changing nothing,
    /// when the opening message of the oldest turn asked for is no longer in
    /// the history, because a compaction already folded it away.
    ///
    /// The plan is cleared with the turns: it may have been written by one of
    /// them, and a reminder about steps from a discarded turn would be noise.
    /// Files are not touched; restoring them is `/rewind`'s job, and the note
    /// says so.
    pub fn rewind_turns(&mut self, turns: usize) -> Result<bool, EngineError> {
        let Some(first) = self.turn_openers.len().checked_sub(turns) else {
            return Ok(false);
        };
        let Some((opener, _)) = self.turn_openers.get(first) else {
            return Ok(false);
        };
        let Some(start) = self
            .history
            .iter()
            .position(|entry| entry.ids.contains(opener))
        else {
            return Ok(false);
        };
        let dropped: Vec<RecordId> = self.history[start..]
            .iter()
            .flat_map(|entry| entry.ids.iter().copied())
            .collect();
        self.history.truncate(start);
        self.turn_openers.truncate(first);
        self.record(RecordKind::Compacted(journal::Compacted::new(
            dropped,
            self.spend.as_usage(),
            REWIND_NOTE.to_string(),
        )))?;
        let id = self.head.expect("head set by the Compacted record");
        self.history.push(HistoryEntry {
            entry: Entry::Summary(REWIND_NOTE.to_string()),
            ids: vec![id],
        });
        self.active_plan = None;
        self.plan_open = false;
        self.edited.clear();
        if let Err(error) = self.recorder.flush() {
            tracing::warn!(%error, "the session could not be flushed");
        }
        Ok(true)
    }

    /// Wait briefly for work the engine left running in the background (the
    /// session title), so it reaches the journal before the session ends.
    ///
    /// Bounded by a few seconds: a frontend calls this on its way
    /// out, and an exit that hung on a slow title call would be worse than an
    /// untitled session, which `/resume` already copes with.
    pub async fn settle(&mut self) {
        let Some(handle) = self.pending_title.take() else {
            return;
        };
        if let Ok(Ok(Some(title))) = tokio::time::timeout(TITLE_SETTLE_TIMEOUT, handle).await {
            self.record_title(title);
            if let Err(error) = self.recorder.flush() {
                tracing::warn!(%error, "the session could not be flushed");
            }
        }
    }

    fn record_title(&mut self, title: String) {
        if let Err(error) = self.record(RecordKind::SessionTitled { title }) {
            tracing::warn!(%error, "could not record the session title");
        }
    }

    /// Work at `goal` until its plan is closed or `max_iterations` turns have
    /// run, whichever comes first.
    ///
    /// **Done is `plan_open`, not a string search.** This used to ask whether
    /// the *rendered* plan text contained "completed" and contained neither
    /// "in_progress" nor "pending", which is a guess about markdown: a plan with
    /// a step reading "mark the migration completed" satisfied it on the first
    /// turn, and a plan the model correctly closed failed it if any step
    /// description held the word "pending". The tool's own `status` argument is
    /// the answer, and the loop already reads it for the end-of-turn gate.
    ///
    /// A goal with no plan at all ends after one turn and reports `completed:
    /// false`. That is the honest reading: nothing said the work was finished,
    /// and re-prompting a model that never opened a plan just asks it to
    /// continue something it does not think it started.
    pub async fn run_goal(
        &mut self,
        goal: String,
        max_iterations: usize,
    ) -> Result<GoalSummary, EngineError> {
        let mut summaries = vec![self.turn(goal).await?];
        let mut iterations = 1;

        while self.plan_open && iterations < max_iterations {
            summaries.push(self.turn(CONTINUE_PROMPT.to_string()).await?);
            iterations += 1;
        }

        Ok(GoalSummary {
            // An open plan at this point means the iteration ceiling stopped the
            // loop, not the work finishing.
            completed: !self.plan_open,
            iterations,
            summaries,
        })
    }

    async fn turn_inner(&mut self, text: String) -> Result<TurnSummary, EngineError> {
        self.record(RecordKind::UserMessage { text: text.clone() })?;
        self.turn_openers.push((
            self.head.expect("head set by the UserMessage record"),
            text.clone(),
        ));
        self.emit(UiEvent::TurnStarted { user: text.clone() });
        self.history.push(HistoryEntry {
            entry: Entry::User(text),
            ids: vec![self.head.expect("head set by the UserMessage record")],
        });

        // Per turn, not per session: a plan left open across the user's next
        // prompt is worth one reminder again, and carrying the flag would mean
        // only the first turn of a session ever gave one.
        self.plan_nudged = false;
        self.verify_retries = 0;

        let mut turns = 0u32;
        let mut tool_calls = 0u32;
        let mut last_usage = Usage::zero();
        let mut last_text = String::new();

        loop {
            // Checked here, before a turn is generated, from the *previous*
            // turn's spend. The loop can therefore overshoot max_tokens by one
            // turn, which is deliberate: the alternative is predicting a turn's
            // cost before generating it, and one turn of overshoot is cheaper
            // than the machinery to avoid it.
            turns += 1;
            // A Ctrl-C between rounds ends the turn cleanly: the in-flight tool
            // was already killed and answered (a cancelled tool self-terminates
            // through its `cx.cancel`), so reaching here means the batch is done
            // and the partial assistant text is already journaled.
            if self.is_cancelled() {
                return Ok(self.stop(Stop::Cancelled, last_usage, tool_calls, last_text));
            }
            if let Some(kind) = self.exceeded_limit(turns) {
                return Ok(self.stop(Stop::HitLimit(kind), last_usage, tool_calls, last_text));
            }

            let Some(Accumulated {
                blocks,
                stop,
                text,
                usage,
            }) = self.generate_turn().await?
            else {
                return Ok(self.stop(Stop::Cancelled, last_usage, tool_calls, last_text));
            };

            last_usage = usage;
            last_text = text;
            self.spend.add(usage);
            self.record(RecordKind::ModelTurn(ModelTurn::new(
                blocks.clone(),
                stop.clone(),
                usage,
            )))?;
            self.history.push(HistoryEntry {
                entry: Entry::Model(blocks.clone()),
                ids: vec![self.head.expect("head set by the ModelTurn record")],
            });

            match stop {
                StopReason::Refusal { category } => {
                    return Ok(self.stop(
                        Stop::Refused { category },
                        last_usage,
                        tool_calls,
                        last_text,
                    ));
                }
                // The output was cut mid-thought. Acting on half a plan is worse
                // than stopping.
                //
                // The turn is journaled and in history either way, and a
                // truncated turn can still carry *complete* `tool_use` blocks:
                // the model finished A, started B, and hit the ceiling. Those
                // are answered before returning, because "every `tool_use` gets
                // a `ToolResult`" holds on the way out of the loop too. Leave
                // them open and the caller's obvious next move, raising
                // `max_tokens` and calling `turn` again, rebuilds a window whose
                // `tool_use` blocks have no results: a 400, and the same 400 on
                // resume, since the fold reads back the history just recorded.
                StopReason::MaxTokens => {
                    self.answer_all(&requested_calls(&blocks), TRUNCATED_MESSAGE)?;
                    return Err(EngineError::Truncated);
                }
                // A server-side tool hit its iteration limit. Not a failure and
                // not a retry: re-send and the server resumes.
                StopReason::PauseTurn => continue,
                StopReason::EndTurn | StopReason::StopSequence => {
                    if self.mechanisms.verify_gate && !self.edited.is_empty() {
                        let root = self.broker.policy().root.clone();
                        let cx = self.tool_context();
                        if let Some(diag) = tools::verify_code_edits(&cx, &root, &self.edited).await
                            && !diag.errors.is_empty()
                        {
                            if self.verify_retries >= MAX_VERIFY_RETRIES {
                                self.edited.clear();
                                return Ok(self.stop(
                                    Stop::VerifyFailed,
                                    last_usage,
                                    tool_calls,
                                    last_text,
                                ));
                            }
                            self.verify_retries += 1;
                            {
                                let feedback_msg = format!(
                                    "COMPILATION VERIFICATION GATE FAILED:\n\n{}\n\nFix all compilation errors before ending turn.",
                                    diag.format_feedback()
                                );
                                self.record(RecordKind::UserMessage {
                                    text: feedback_msg.clone(),
                                })?;
                                let id = self.head.expect("head set by record");
                                self.history.push(HistoryEntry {
                                    entry: Entry::User(feedback_msg),
                                    ids: vec![id],
                                });
                                continue;
                            }
                        }
                        self.edited.clear();
                        self.verify_retries = 0;
                    }
                    // A plan the model wrote and then walked away from is the
                    // drift the tool exists to prevent, and until now nothing
                    // noticed. One reminder, because the model can answer it in
                    // a single call; then the turn ends saying so, because an
                    // unbounded nag is a worse failure than an honest label.
                    if self.mechanisms.plan_gate && self.plan_open {
                        if !self.plan_nudged {
                            self.plan_nudged = true;
                            self.record(RecordKind::UserMessage {
                                text: PLAN_OPEN_MESSAGE.to_string(),
                            })?;
                            let id = self.head.expect("head set by record");
                            self.history.push(HistoryEntry {
                                entry: Entry::User(PLAN_OPEN_MESSAGE.to_string()),
                                ids: vec![id],
                            });
                            continue;
                        }
                        return Ok(self.stop(
                            Stop::PlanIncomplete,
                            last_usage,
                            tool_calls,
                            last_text,
                        ));
                    }
                    // The last gate before the turn ends. A `Stop` hook that
                    // blocks sends the model back around with the reason, which
                    // is the same shape the verifier gate above uses and is what
                    // makes "you have not run the tests yet" enforceable.
                    let stopping = self
                        .hooks
                        .run(HookEvent::Stop, None, None, Some(&last_text), &self.cwd)
                        .await;
                    if let Some(reason) = stopping.blocked {
                        let feedback = format!("A Stop hook blocked ending the turn: {reason}");
                        self.record(RecordKind::UserMessage {
                            text: feedback.clone(),
                        })?;
                        let id = self.head.expect("head set by record");
                        self.history.push(HistoryEntry {
                            entry: Entry::User(feedback),
                            ids: vec![id],
                        });
                        continue;
                    }
                    return Ok(self.stop(Stop::Done, last_usage, tool_calls, last_text));
                }
                StopReason::ToolUse => {}
            }

            let requested = requested_calls(&blocks);
            if requested.is_empty() {
                // `tool_use` with no tool blocks is not a shape the API
                // produces, but treating it as "keep going" would spin.
                return Ok(self.stop(Stop::Done, last_usage, tool_calls, last_text));
            }

            // Known only now, so it cannot move to the top. Every outstanding
            // call is still answered before returning.
            if requested.len() as u32 > self.limits.max_tool_calls {
                self.answer_all(&requested, TOOL_CALL_LIMIT_MESSAGE)?;
                return Ok(self.stop(
                    Stop::HitLimit(LimitKind::ToolCalls),
                    last_usage,
                    tool_calls + requested.len() as u32,
                    last_text,
                ));
            }

            for batch in batch_calls(&self.dispatcher, &requested) {
                self.dispatch_batch(batch).await?;
                tool_calls += batch.len() as u32;
            }
        }
    }

    // --------------------------------------------------------------------
    // One round of the loop
    // --------------------------------------------------------------------

    /// Which ceiling, if any, is already breached. Tokens are read from the
    /// previous turn's spend; turns is this call's own count.
    fn exceeded_limit(&self, turns: u32) -> Option<LimitKind> {
        if turns > self.limits.max_turns {
            return Some(LimitKind::Turns);
        }
        if self.spend.tokens() > self.limits.max_tokens {
            return Some(LimitKind::Tokens);
        }
        None
    }

    /// One provider round-trip, with its transport failures journaled.
    /// One model response, or `None` when the turn was cancelled while it was
    /// being requested or streamed.
    ///
    /// **Raced against the turn's token, not only checked between rounds.**
    /// Generation is most of a turn's wall time, and a cancel that waited for
    /// the reply to finish streaming meant Esc did nothing for as long as the
    /// model kept talking, including through a retry backoff. Dropping the
    /// request closes the connection and abandons the backoff.
    ///
    /// A cancelled response is not journaled and does not enter history: it has
    /// no `TurnEnd`, and a half-finished `tool_use` or an unsigned thinking
    /// block cannot be sent back to the model. What the user saw stream is the
    /// only trace, as with an interrupt in Claude Code.
    async fn generate_turn(&mut self) -> Result<Option<Accumulated>, EngineError> {
        let cancel = self.turn_cancel.clone().unwrap_or_default();
        // **One compaction and one retry when the server says the window
        // overflowed.** The local estimate packs the window and is allowed to be
        // wrong, and below half the budget no exact count checks it; a provider
        // that refuses the request is the last word, and the turn used to end
        // there with the session stuck, every later turn sending the same
        // too-large history. Once, because a second refusal after a compaction
        // means there is nothing left that dropping history can fix.
        let mut compacted_for_overflow = false;
        let stream = loop {
            let messages = self.fit_window().await?;
            let req = self.build_request(messages);
            let stream = tokio::select! {
                biased;
                () = cancel.cancelled() => None,
                stream = self.provider.turn(req) => Some(stream),
            };
            // Drained on every path: a retry storm that ended in success is
            // exactly what a journal recording only unrecovered failures cannot
            // show.
            self.record_attempts()?;
            let Some(stream) = stream else {
                return Ok(None);
            };
            match stream {
                Err(error) if error.is_context_overflow() && !compacted_for_overflow => {
                    let Some(compaction) = self.context.compact_now(&self.history) else {
                        return Err(error.into());
                    };
                    tracing::warn!(%error, "the provider refused an overflowing window; compacting once");
                    self.apply_compaction(compaction).await?;
                    compacted_for_overflow = true;
                }
                other => break other?,
            }
        };
        let accumulated = tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            accumulated = accumulate_turn(stream, self.observer.as_ref()) => Some(accumulated),
        };
        let cause = self.record_attempts()?;
        let Some(accumulated) = accumulated else {
            return Ok(None);
        };

        // No TurnEnd means the bytes stopped before `message_stop` and the
        // provider exhausted its retries. A truncated stream is not a turn.
        accumulated
            .ok_or_else(|| {
                let detail = match cause {
                    Some(cause) => format!("stream ended without message_stop: {cause}"),
                    None => "stream ended without message_stop".to_string(),
                };
                EngineError::Provider(ProviderError::StreamBroken(detail))
            })
            .map(Some)
    }

    fn tool_defs(&self) -> Vec<TurnTool> {
        self.dispatcher
            .specs()
            .into_iter()
            .map(|spec| TurnTool {
                description: spec.description.clone(),
                input_schema: spec.input_schema.clone(),
                name: spec.name.clone(),
            })
            .collect()
    }

    /// Turn the provider's recorded attempts into `ProviderFailed` records,
    /// returning the last failure's text.
    ///
    /// **The return value is what stops a terminal failure being anonymous.** A
    /// non-retryable response during a mid-stream restart -- a 401 on an expired
    /// OAuth token, a 413 once the window grew -- ends the stream, and the engine
    /// then reported only `StreamBroken("no TurnEnd")`. The API's explanation
    /// was already in the journal as a `ProviderFailed`, and nowhere the user
    /// would look. Handing it back lets the error the user actually sees name
    /// the cause.
    fn record_attempts(&mut self) -> Result<Option<String>, EngineError> {
        let mut last = None;
        for attempt in self.provider.drain_attempts() {
            last = Some(attempt.failure.clone());
            self.record(RecordKind::ProviderFailed(ProviderFailed::new(
                attempt.failure,
                attempt.attempt,
            )))?;
        }
        Ok(last)
    }

    // --------------------------------------------------------------------
    // Dispatching a batch
    // --------------------------------------------------------------------

    /// Journal every call in a batch, run the batch, journal every result, and
    /// append the results to history.
    ///
    /// **Order is imposed, not observed.** Calls are journaled in the order the
    /// model asked, before any of them runs; results are journaled in that same
    /// order once every call in the batch has finished, whatever order they
    /// actually finished in. A batch of one is the serial path exactly as it
    /// was, so a session that only ever uses serial tools produces the journal
    /// it always did.
    async fn dispatch_batch(&mut self, batch: &[RequestedCall]) -> Result<(), EngineError> {
        // Journal the calls, then tell the UI they started. The order is what
        // gives a live spinner something to replace.
        let mut toolcall_ids = Vec::with_capacity(batch.len());
        for requested in batch {
            self.record(RecordKind::ToolCall(ToolCall::new(
                requested.call.clone(),
                requested.name.clone(),
                requested.args.clone(),
            )))?;
            toolcall_ids.push(self.head.expect("head set by the ToolCall record"));
            self.emit(UiEvent::ToolStarted {
                call: requested.call.clone(),
                name: requested.name.clone(),
                args: requested.args.clone(),
            });
        }

        // Screen the batch before any of it runs. A blocked call never reaches
        // the dispatcher, so a `PreToolUse` hook is a real veto and not an
        // after-the-fact complaint about a write that already happened.
        let mut blocked = screen_batch(&self.hooks, &self.cwd, batch).await;
        self.screen_skills(batch, &mut blocked);

        let cx = self.tool_context();
        let outcomes = run_batch(
            &self.dispatcher,
            batch,
            &blocked,
            &cx,
            self.observer.as_ref(),
        )
        .await;

        // Before any result, on every path. The broker emits records while the
        // tools run; draining after a result would make the monitor, which
        // observes in journal order, fire a false violation mid-run against a
        // system that did everything right.
        //
        // "Every path" includes the ones that failed: a tool that panicked or
        // timed out still made every broker request it made before it died,
        // those accesses really happened, and an unaudited access is exactly what
        // this machinery exists to prevent.
        self.drain_permissions(batch)?;
        self.drain_delegated(batch)?;

        for ((requested, toolcall_id), outcome) in batch.iter().zip(toolcall_ids).zip(outcomes) {
            let outcome = annotate(&self.hooks, &self.cwd, requested, outcome).await;

            if requested.name == EDIT_TOOL {
                // Successful edits only, and the path it wrote. A refused edit
                // changed nothing, and running a compiler over the tree because
                // one was refused is a round trip spent on the gate's own work.
                if matches!(outcome, Outcome::Ok { .. })
                    && let Some(path) = requested.args.get(EDIT_PATH_ARG).and_then(Value::as_str)
                {
                    self.edited.push(path.to_string());
                }
            } else if requested.name == PLAN_TOOL
                && let Outcome::Ok { ref content } = outcome
            {
                self.active_plan = Some(content.clone());
                // From the arguments, not the rendered text: the status is a
                // field the model set, and reading it back out of the markdown
                // the tool printed would make the gate depend on that
                // formatting.
                let status = requested
                    .args
                    .get(PLAN_STATUS_ARG)
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.plan_open = !PLAN_CLOSED_STATUSES.contains(&status);
            } else if requested.name == SKILL_TOOL
                && matches!(outcome, Outcome::Ok { .. })
                && let Some(name) = requested.args.get(SKILL_NAME_ARG).and_then(Value::as_str)
            {
                // A successful load only: an unknown name answers with an error
                // and no instructions, and counting it would let a typo unlock
                // the gate.
                self.skills.record_loaded(name);
            }

            self.record(RecordKind::ToolResult(ToolResult::new(
                requested.call.clone(),
                outcome.clone(),
            )))?;
            let toolresult_id = self.head.expect("head set by the ToolResult record");
            self.history.push(HistoryEntry {
                entry: Entry::Tool {
                    call: requested.call.clone(),
                    outcome,
                },
                ids: vec![toolcall_id, toolresult_id],
            });
        }
        Ok(())
    }

    /// Refuse any edit in the batch that skipped the skill its path requires.
    ///
    /// After the hooks and only where they let a call through, so a hook's
    /// reason is never overwritten by this one: both are refusals, and the first
    /// one raised is the one the model needs to answer.
    ///
    /// A skill loaded by another call in the *same* batch does not count. The
    /// batch is screened before any of it runs, so counting it would depend on
    /// which call happened to finish first, and the model would see a rule that
    /// holds or does not hold depending on scheduling.
    fn screen_skills(&self, batch: &[RequestedCall], blocked: &mut [Option<String>]) {
        // The ablation switch, before the loop: an off gate screens nothing,
        // exactly as an empty one does, and for the same reason — the question
        // is whether refusing the edit moves a metric, not whether the rules
        // exist.
        if !self.mechanisms.skill_gate {
            return;
        }
        for (requested, reason) in batch.iter().zip(blocked) {
            if reason.is_some() || requested.name != EDIT_TOOL {
                continue;
            }
            // An `edit` with no `path` is malformed; the tool itself says so,
            // and a gate that guessed would refuse a call for the wrong reason.
            if let Some(path) = requested.args.get(EDIT_PATH_ARG).and_then(Value::as_str) {
                *reason = self.skills.screen_edit(path);
            }
        }
    }

    // --------------------------------------------------------------------
    // Draining what ran under the tool call
    // --------------------------------------------------------------------

    /// Journal what any sub-session spent, and add it to this session's total.
    ///
    /// **Added to `spend`, so the runaway guard counts it.** A subagent's tokens
    /// are the caller's tokens: they were burned because this session asked for
    /// them, and a `max_tokens` ceiling that ignored them would be a ceiling a
    /// session could walk through by delegating. The same total reaches the
    /// statusline, since that reads `spend` too.
    ///
    /// Attributed to the call that spawned the run. The `subagent` tool declares
    /// serial concurrency, so a batch that contains one contains nothing else,
    /// and `batch[0]` is that call rather than a guess.
    fn drain_delegated(&mut self, batch: &[RequestedCall]) -> Result<(), EngineError> {
        while let Ok(run) = self.delegated.try_recv() {
            self.spend.add(run.usage);
            self.emit(UiEvent::Delegated { usage: run.usage });
            self.record(RecordKind::Delegated(journal::Delegated::new(
                batch[0].call.clone(),
                run.journal,
                run.usage,
            )))?;
        }
        Ok(())
    }

    /// Turn the broker's permission decisions into `Permission` records.
    ///
    /// The broker has no `CallId` (it does not depend on `journal` and does not
    /// know what a call is), so the engine parses one back out of the tag the
    /// dispatcher scoped around each tool task. A decision with no tag came from
    /// somewhere outside a tool call, or from a task a tool spawned for itself;
    /// attributing it to the batch's first call keeps the access audited, which
    /// matters more than which of a batch's calls it lands under.
    fn drain_permissions(&mut self, batch: &[RequestedCall]) -> Result<(), EngineError> {
        while let Ok(decided) = self.permissions.try_recv() {
            // A tag this batch does not contain came from a call this session
            // never made: a sub-session sharing the broker tags with its own
            // ids, and those exist only in its journal. Recorded verbatim, the
            // record points at a call with no result here, so the access reads as
            // unattributed and the call that caused it reads as unaudited. It
            // belongs to whichever call in this batch spawned the work, and with
            // one subagent call in flight that is not a guess.
            let call = match decided.call {
                Some(ref tag) if batch.iter().any(|call| call.call.as_str() == tag.as_ref()) => {
                    CallId::new(tag.to_string())
                }
                _ => batch[0].call.clone(),
            };
            let permission = Permission::new(
                call,
                permission::access(&decided.request, &decided.supplied),
                permission::decision(&decided.decision, decided.source),
                permission::scope(&decided.scope),
            );
            self.record(RecordKind::Permission(permission))?;
        }
        Ok(())
    }

    /// Answer every outstanding `tool_use` with a synthetic error, so the model
    /// sees why it stopped and `open_calls` stays empty. The calls are journaled
    /// as asked-for and answered, and appended to history, because the model
    /// really did ask.
    fn answer_all(
        &mut self,
        requested: &[RequestedCall],
        message: &str,
    ) -> Result<(), EngineError> {
        for call in requested {
            let outcome = Outcome::Err {
                message: message.to_string(),
            };
            self.record(RecordKind::ToolCall(ToolCall::new(
                call.call.clone(),
                call.name.clone(),
                call.args.clone(),
            )))?;
            let toolcall_id = self.head.expect("head set by the ToolCall record");
            self.record(RecordKind::ToolResult(ToolResult::new(
                call.call.clone(),
                outcome.clone(),
            )))?;
            let toolresult_id = self.head.expect("head set by the ToolResult record");
            self.history.push(HistoryEntry {
                entry: Entry::Tool {
                    call: call.call.clone(),
                    outcome,
                },
                ids: vec![toolcall_id, toolresult_id],
            });
        }
        Ok(())
    }

    /// Complete tools that were in flight when the previous process died.
    ///
    /// A `ToolCall` with no `ToolResult` is a tool the broker authorized and
    /// the dispatcher started, but whose result never reached the journal. The
    /// dedup ledger refuses re-dispatch, and re-running a half-applied write is
    /// not safe to guess at, so resume records a synthetic outcome and lets the
    /// model see that the call was interrupted. The call is already journaled;
    /// only the missing `ToolResult` is appended.
    fn complete_open_calls(&mut self, open: &[CallId]) -> Result<(), ResumeError> {
        if open.is_empty() {
            return Ok(());
        }
        for call in open {
            let outcome = Outcome::Err {
                message: INTERRUPTED_MESSAGE.to_string(),
            };
            self.record(RecordKind::ToolResult(ToolResult::new(
                call.clone(),
                outcome.clone(),
            )))
            .map_err(|error| match error {
                EngineError::Sink(io) => ResumeError::Sink(io),
                // `record` only fails on the sink; the other variants are
                // unreachable from this path.
                other => ResumeError::Sink(std::io::Error::other(other.to_string())),
            })?;
            let toolresult_id = self.head.expect("head set by the ToolResult record");
            self.history.push(HistoryEntry {
                entry: Entry::Tool {
                    call: call.clone(),
                    outcome,
                },
                ids: vec![toolresult_id],
            });
        }
        Ok(())
    }

    // --------------------------------------------------------------------
    // Assembling a request
    // --------------------------------------------------------------------

    /// **The system prompt is the same on every turn of a session**, and that is
    /// a cost property, not tidiness. Anthropic renders tools, then system, then
    /// messages, and caches by prefix, so a system prompt that changes mid-session
    /// misses the cache for the whole conversation behind it and writes it again
    /// at the write price. The active plan used to be appended here, so every
    /// `plan` update re-wrote the session; it now rides the compaction summary.
    fn build_request(&self, messages: Vec<TurnMessage>) -> TurnRequest {
        TurnRequest {
            messages,
            system: self.system_prompt.clone(),
            tools: self
                .dispatcher
                .specs()
                .into_iter()
                .map(|spec| TurnTool {
                    description: spec.description.clone(),
                    input_schema: spec.input_schema.clone(),
                    name: spec.name.clone(),
                })
                .collect(),
        }
    }

    fn tool_context(&self) -> ToolCx {
        // The dispatcher turns this into a child token per call, so a turn-level
        // cancel propagates to every tool while a per-call timeout still only
        // cancels the one call. A turn with no token gets a fresh uncanceled one.
        let cancel = self.turn_cancel.clone().unwrap_or_default();
        ToolCx::new(
            self.broker.clone(),
            self.cwd.clone(),
            cancel,
            // A placeholder the dispatcher immediately overwrites with the real
            // per-call deadline. It used to be `now + 300s` behind a constant
            // that read as though the engine set the budget, which nothing has
            // ever been true of: `Dispatcher::dispatch` owns the timeout.
            Instant::now(),
        )
    }

    // --------------------------------------------------------------------
    // Emitting and recording
    // --------------------------------------------------------------------

    /// Best-effort narration to the UI observer, if one is attached. A send that
    /// fails means the frontend is gone, which is not the loop's problem: the
    /// engine never blocks on the UI.
    fn emit(&self, event: UiEvent) {
        crate::ui_event::emit_to(self.observer.as_ref(), event);
    }
    #[instrument(skip(self, kind), fields(session = %self.session))]
    fn record(&mut self, kind: RecordKind) -> Result<(), EngineError> {
        let id = RecordId::new();
        let record = Record {
            at: SystemTime::now(),
            id,
            kind,
            parent: self.head,
            session: self.session,
        };
        self.recorder.append(&record).map_err(EngineError::Sink)?;
        self.head = Some(id);
        Ok(())
    }

    fn stop(&self, stop: Stop, usage: Usage, tool_calls: u32, text: String) -> TurnSummary {
        let _span = info_span!("turn_stop", stop = ?stop).entered();
        // One usage event per turn: the final model round's `usage` plus the
        // session's cumulative totals, rendered to plain numbers so the event
        // owns no engine type. Emitted here because every `Ok` exit routes
        // through `stop`, so it lands exactly once, just before `TurnEnded`.
        self.emit(UiEvent::Usage {
            usage,
            total_tokens: self.spend.tokens(),
        });
        self.emit(UiEvent::TurnEnded { stop: stop.clone() });
        TurnSummary {
            head: self.head.expect("head set by SessionStarted"),
            stop,
            text,
            tool_calls,
            usage,
        }
    }
}

// ========================================================================
// Functions
// ========================================================================

/// The request that asks for a session title from its first exchange.
fn title_request(user_text: &str, assistant_text: &str) -> TurnRequest {
    TurnRequest {
        messages: vec![TurnMessage {
            content: vec![TurnBlock::Text(format!(
                "Give this exchange a short, plain-text title: {TITLE_MAX_WORDS} words or \
                 fewer, no trailing punctuation, no quotes.\n\nUser: {user_text}\n\
                 Assistant: {assistant_text}"
            ))],
            role: Role::User,
        }],
        system: String::new(),
        tools: Vec::new(),
    }
}

/// Ask `provider` for a title, or `None` on any failure or an empty reply.
async fn generate_title(provider: &dyn Provider, request: TurnRequest) -> Option<String> {
    let stream = match provider.turn(request).await {
        Ok(stream) => stream,
        Err(error) => {
            tracing::warn!(%error, "could not generate a session title");
            return None;
        }
    };
    let Some(accumulated) = accumulate_turn(stream, None).await else {
        tracing::warn!("session-title stream ended without completing");
        return None;
    };
    let title = accumulated.text.trim();
    (!title.is_empty()).then(|| title.to_string())
}

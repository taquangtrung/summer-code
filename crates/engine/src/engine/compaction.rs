//! Fitting the conversation back inside the token budget.
//!
//! **Two kinds of summary replace a dropped range, on purpose.** [`ContextManager`]
//! plans the drop and produces a mechanical summary: a count of the turns and
//! tool calls that went. That half stays pure and synchronous, so every
//! compaction test is arithmetic over a history slice with no provider in sight.
//! The engine may then replace it with a model-written handover note, and records
//! whichever it ends up with on the `Compacted` record, so a resume folds back
//! exactly what the live session went on to use.
//!
//! The model-written half is opt-in, and the reason is replay: [`provider::Recorded`]
//! answers the Nth `Provider::turn` with the Nth recorded turn and ignores the
//! request, so one extra call would shift every later turn by one. A summarizer
//! that fails falls back to the mechanical count rather than ending the session,
//! because a compaction only happens when the window is already full, which is
//! the worst possible moment to give up over a failed side call.
//!
//! **This is an `impl Engine` block in its own file, which the rest of the crate
//! does not do.** Compaction is the one cluster that is both self-contained
//! enough to read alone and too entangled with `&mut self` to lift into free
//! functions: it rewrites `history`, journals a `Compacted` record, and asks the
//! provider, so every method needs the engine's own state. Splitting the file
//! without splitting the type is the trade; the methods keep their `Engine`
//! receiver and their privacy.

use journal::{RecordId, RecordKind};
use provider::{Role, TurnBlock, TurnMessage, TurnRequest};

use crate::accumulate::accumulate_turn;
use crate::context::Compaction;
use crate::engine::transcript::transcribe;
use crate::engine::{Engine, EngineError};
use crate::session::{Entry, HistoryEntry};

// ========================================================================
// Constants
// ========================================================================

/// What the model is asked for when compaction replaces a dropped range.
///
/// **Written for the reader that will actually get it: this same model, later,
/// with none of the dropped turns in front of it.** So it asks for the state a
/// resumed worker needs -- what was decided, what is on disk now, what is still
/// open -- rather than a readable precis of the conversation. The instruction to
/// keep identifiers verbatim is the load-bearing one: a summary that says "the
/// parser module" instead of `crates/query/src/extract/treesitter.rs` costs the
/// model the ability to act on its own history without re-deriving it.
const COMPACTION_SUMMARY_PROMPT: &str = "The conversation above is being dropped from your context to make room. Write a \
     compact handover note for yourself covering only what you would need to continue: \
     the task and any constraints given, decisions made and the reasons that are not \
     obvious from the result, files created or changed and what changed in them, facts \
     established about the codebase, and anything attempted that failed and why. Name \
     files, symbols, commands, and identifiers verbatim rather than describing them. \
     Omit pleasantries, restated instructions, and anything already visible in the \
     working tree. Prose or bullets, no preamble, no closing question.";

// ========================================================================
// Engine Implementations
// ========================================================================

impl Engine {
    /// Fit the context window to the token budget, compacting as needed.
    ///
    /// The context manager packs from history with a local estimate; when that
    /// overflows the budget it proposes a compaction, which the engine applies
    /// (mutating history and recording a `Compacted` record) and retries. Once
    /// the estimate fits, the provider's exact `count_tokens` verifies; if it
    /// disagrees and history is still droppable, one more compaction runs.
    ///
    /// `count_tokens` is exact, and it is the only window guard that is
    /// actually a guard near the limit. **It is not free**: it is a network
    /// round trip, serial, ahead of the turn it guards. So it is asked only once
    /// the estimate is near enough the budget for its error to matter (see
    /// `ContextManager::needs_exact_count`), and a failed count falls back to
    /// the estimate instead of failing a turn the estimate already says fits:
    /// a rate-limited counting endpoint is no reason to refuse to work.
    pub(super) async fn fit_window(&mut self) -> Result<Vec<TurnMessage>, EngineError> {
        let tools = self.tool_defs();
        let sys_prompt = self.system_prompt.clone();
        loop {
            let plan = self.context.plan(&self.history, &sys_prompt, &tools);
            if let Some(compaction) = plan.compact {
                self.apply_compaction(compaction).await?;
                continue;
            }
            if !self.context.needs_exact_count(plan.estimate) {
                return Ok(plan.window);
            }
            let counted = self
                .provider
                .count_tokens(&TurnRequest {
                    messages: plan.window.clone(),
                    system: sys_prompt.clone(),
                    tools: tools.clone(),
                })
                .await;
            let count = match counted {
                Ok(count) => count,
                Err(error) => {
                    tracing::warn!(%error, "could not count tokens; sending on the estimate");
                    return Ok(plan.window);
                }
            };
            if count <= self.context.budget() {
                return Ok(plan.window);
            }
            // The estimate undercounted and `count_tokens` caught it. Compact
            // if there is anything left to drop; otherwise the turn cannot fit.
            if let Some(compaction) = self.context.compact_now(&self.history) {
                self.apply_compaction(compaction).await?;
                continue;
            }
            return Err(EngineError::ContextOverflow);
        }
    }

    /// Compact the history now, whatever the budget says, and answer with how
    /// many entries were dropped.
    ///
    /// The loop compacts on its own when a turn overflows the window; this is
    /// the same operation asked for early, which is what a frontend's `/compact`
    /// means. `Ok(0)` is the honest answer when the working window is all there
    /// is: nothing was dropped, and a caller that reported a saving anyway would
    /// be describing work that did not happen.
    ///
    /// It is a real compaction and not a display change: the dropped range
    /// leaves the history the next turn is built from, and a `Compacted` record
    /// records which records went, so a resume folds to the same place.
    pub async fn compact_now(&mut self) -> Result<usize, EngineError> {
        let Some(compaction) = self.context.compact_now(&self.history) else {
            return Ok(0);
        };
        let dropped = compaction.drop_end - compaction.drop_start;
        self.apply_compaction(compaction).await?;
        Ok(dropped)
    }

    /// Apply a compaction: drop the middle range of history, record the
    /// `Compacted` record naming the dropped record ids, and insert the summary
    /// where the range was. Append-only means the dropped records stay on the
    /// walk physically; the `Compacted.dropped` set subtracts them logically.
    ///
    /// The summary is the model's when [`EngineBuilder::summarize_compactions`]
    /// is on and the call succeeds, and the mechanical count otherwise. Either
    /// way it reaches the journal on the `Compacted` record, so a resume folds to
    /// exactly the summary the live session went on to use.
    pub(super) async fn apply_compaction(
        &mut self,
        compaction: Compaction,
    ) -> Result<(), EngineError> {
        // Before the drain: this is the last point at which the dropped range
        // still exists to be summarized.
        let range = compaction.drop_start..compaction.drop_end;
        let summary = match self.model_summary(range.clone()).await {
            Some(written) => written,
            None => compaction.summary,
        };
        // The plan's tool result may be in the range being dropped, and the
        // system prompt no longer repeats it, so the summary carries it. A
        // compaction re-writes the cache from here on anyway, so this is the one
        // place the plan can be restated without costing a cache miss of its
        // own. Recorded on the `Compacted` record, a resume restates it too.
        let summary = match &self.active_plan {
            Some(plan) => format!("{summary}\n\n<active_plan>\n{plan}\n</active_plan>"),
            None => summary,
        };

        let dropped: Vec<RecordId> = self.history[range.clone()]
            .iter()
            .flat_map(|entry| entry.ids.iter().copied())
            .collect();
        self.history.drain(range);
        // The running total is checkpointed onto the record: the dropped range's
        // `ModelTurn` usage is about to become unreachable to the walk, and this
        // is the only place it can be preserved.
        self.record(RecordKind::Compacted(journal::Compacted::new(
            dropped,
            self.spend.as_usage(),
            summary.clone(),
        )))?;
        let compacted_id = self.head.expect("head set by the Compacted record");
        self.history.insert(
            compaction.drop_start,
            HistoryEntry {
                entry: Entry::Summary(summary),
                ids: vec![compacted_id],
            },
        );
        Ok(())
    }

    /// Ask the model to summarize the history range about to be dropped.
    ///
    /// `None` whenever the caller should keep the mechanical summary: the
    /// mechanism is off, the provider failed, the stream ended early, or the
    /// reply was empty. **Best-effort by construction**, like
    /// [`Engine::title_session`]: a compaction only happens because the window
    /// is already full, so failing the turn over a failed summarizer would turn
    /// a recoverable context problem into a dead session at exactly the moment
    /// the user has the most work in flight.
    ///
    /// **The range is rendered to a transcript and sent as one user message,
    /// not replayed as structured messages.** Replaying looked more faithful and
    /// is wrong three times over, each one a 400 the mock provider would never
    /// have shown: the range starts just after the first user message, so its
    /// own first entry is an *assistant* turn and `messages[0]` would carry the
    /// wrong role; its thinking blocks carry signatures the API validates
    /// against the request they were produced for, not this one; and its
    /// `tool_use` blocks would arrive with `tools: []`. Flattening sidesteps all
    /// three, and a summarizer reads a transcript perfectly well.
    ///
    /// No tools are offered: this call answers, it does not act, and a
    /// summarizer that could call `edit` would be a second agent loop running
    /// inside the first one's context management.
    ///
    /// The usage counts against this session's spend. It is the session's own
    /// window that caused the call, so charging it elsewhere (or nowhere) would
    /// let a long session's real cost drift from what the limits believe.
    async fn model_summary(&mut self, range: std::ops::Range<usize>) -> Option<String> {
        // Two flags, two questions. `summarize_compactions` is the frontend
        // saying an out-of-band provider call is safe here at all, which replay
        // cannot say; `mechanisms` is the ablation saying whether this arm wants
        // the mechanism. Neither implies the other, so both are checked.
        if !self.summarize_compactions || !self.mechanisms.compaction_summaries {
            return None;
        }
        let transcript = transcribe(&self.history[range]);
        if transcript.trim().is_empty() {
            return None;
        }
        let request = TurnRequest {
            messages: vec![TurnMessage {
                content: vec![TurnBlock::Text(format!(
                    "{transcript}\n\n{COMPACTION_SUMMARY_PROMPT}"
                ))],
                role: Role::User,
            }],
            system: String::new(),
            tools: Vec::new(),
        };

        let stream = match self.provider.turn(request).await {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(%error, "could not summarize the compacted range");
                return None;
            }
        };
        let accumulated = accumulate_turn(stream, None).await?;
        self.spend.add(accumulated.usage);
        let summary = accumulated.text.trim();
        if summary.is_empty() {
            tracing::warn!("the compaction summarizer returned nothing");
            return None;
        }
        Some(summary.to_string())
    }
}

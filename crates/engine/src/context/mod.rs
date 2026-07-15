//! Context management: a per-turn window packed to a token budget, with
//! compaction when the history overflows it.
//!
//! [`ContextManager`] holds no state across turns. The window is computed from
//! the folded history every turn, so there is exactly one answer to "what did
//! the model see" and it is derived, not remembered. A window cached in session
//! state would be a second source of truth for the thing the journal exists to
//! be the first source of truth for.
//!
//! **Two budgets count different subsets, on purpose.** This crate's
//! [`Limits::max_tokens`][crate::Limits] is a *runaway* guard and excludes
//! `cache_read` (a cache hit reuses work already done). The window budget here is a
//! *window* guard and counts the whole prompt — new input plus cached tokens —
//! because a cached token is cheap but still occupies the context window. The
//! two must not share a helper: reusing the runaway number for the window is how a
//! well-cached session sails past the window while the spend counter reports it
//! has barely started.
//!
//! **Compaction drops a middle range**, keeping the first user message (the task
//! and its early constraints) and the recent window, and summarizes what it
//! drops. Dropping from the middle is forced: append-only means the records after
//! the range already point through it, so the range is subtracted logically by a
//! `Compacted` record rather than re-parented.
//!
//! **The summary this module produces is mechanical** (a count of the dropped
//! turns and tool calls), and it is the floor rather than the ceiling. The
//! engine may replace it with a model-written one before recording the
//! `Compacted` record: see [`EngineBuilder::summarize_compactions`][sc], which is
//! opt-in and falls back to this count whenever the call fails.
//!
//! The split is deliberate, and it is what keeps both halves honest. Planning
//! stays pure and offline-testable here -- no provider, no `async`, so every
//! test in this module is arithmetic over a history slice. The provider turn
//! lives where provider turns live, and is budgeted and journaled like any
//! other. It is also why the mechanical summary cannot simply be deleted once
//! the model-written one exists: replay is positional over `Provider::turn`, so
//! a recorded session compacts with this count and no round trip at all.
//!
//! [sc]: crate::EngineBuilder::summarize_compactions

use provider::{Provider, TurnMessage, TurnTool, context_window_for};

use crate::session::{Entry, HistoryEntry, history_to_messages};

// ========================================================================
// Constants
// ========================================================================

/// The cheap local estimate the plan packs with (bytes per token). The real
/// count comes from `Provider::count_tokens`, which the engine verifies with
/// before sending; the estimate chooses what fits.
const BYTES_PER_TOKEN: usize = 4;

/// Leave this fraction of the budget unused, so the estimate's error does not
/// overflow the real window before `count_tokens` gets a chance to verify.
///
/// Room for the model's *output* is not a second subtraction here: it is already
/// in the budget itself. [`ContextManager::for_model`] takes the provider's
/// `max_tokens` off the model's window, and [`ContextManager::defaults`] packs
/// to 180k for a model it knows nothing about. An `output_reserve` helper
/// used to sit alongside this constant behind `#[allow(dead_code)]`, claiming to
/// do that job while nothing called it, which read as though the window reserved
/// output room when it did not.
const BUDGET_HEADROOM: u32 = 9;

/// The window budget for a model the registry does not know.
const DEFAULT_BUDGET: u32 = 180_000;

/// Recent history entries compaction always keeps, besides the first user
/// message.
const DEFAULT_KEEP_RECENT: usize = 40;

/// Below `budget / EXACT_COUNT_DIVISOR` by the estimate, a window is not
/// verified with `count_tokens` before it is sent.
///
/// The estimate is bytes over [`BYTES_PER_TOKEN`], and real text runs denser
/// than that: code at about three bytes a token, CJK nearer two. At half the
/// budget even the two-byte case still fits, so the exact count cannot change
/// the answer there, and asking for it was a serial round trip in front of every
/// model turn of every session, most of which never come near the window.
const EXACT_COUNT_DIVISOR: u32 = 2;

// ========================================================================
// Data Structures
// ========================================================================

/// Packs a per-turn window from history to a token budget, proposing compaction
/// when the history overflows it.
pub struct ContextManager {
    budget: u32,
    policy: CompactionPolicy,
}

/// How aggressively to compact.
#[derive(Clone)]
pub struct CompactionPolicy {
    /// Always keep at least this many recent history entries (the working
    /// window), plus the first user message. Everything between is droppable.
    keep_recent: usize,
}

/// What a single tool result larger than the budget becomes. `Truncate` is the
/// Phase 4 default; `Spill` writes it to disk and passes a reference; `Refuse`
/// declines to send a turn it cannot fit.
#[derive(Debug, Clone, Copy)]
pub enum Oversized {
    Truncate,
    Spill,
    Refuse,
}

/// One compaction proposal: drop a middle range of history, summarize it.
pub(crate) struct Compaction {
    /// Inclusive start, exclusive end of the history-entry range to drop.
    pub drop_end: usize,
    pub drop_start: usize,
    pub summary: String,
}

/// The result of planning a turn's window.
pub(crate) struct WindowPlan {
    /// A compaction to apply before sending, if the window still overflows.
    pub compact: Option<Compaction>,
    /// The local estimate of the whole request, in tokens.
    pub estimate: u32,
    /// The messages to send, packed from history.
    pub window: Vec<TurnMessage>,
}

// ========================================================================
// ContextManager Implementations
// ========================================================================

impl ContextManager {
    /// [`ContextManager::for_model`] for the model `provider` talks to.
    pub fn for_provider(provider: &dyn Provider) -> Self {
        Self::for_model(
            context_window_for(provider.model()),
            provider.max_output_tokens(),
        )
    }

    /// Sized to a model: its context window less the output a turn may ask
    /// for, which the API counts against the same window. `None` for a model
    /// the registry does not catalog falls back to [`ContextManager::defaults`].
    ///
    /// A fixed 180k packed a 128k model past its window before compaction ever
    /// started, and compacted a 1M model's history at a sixth of what it holds.
    /// The output share is capped at half the window, so a `max_tokens`
    /// configured past what the model can hold still leaves a usable budget.
    pub fn for_model(window: Option<u64>, max_output: Option<u32>) -> Self {
        let Some(window) = window else {
            return Self::defaults();
        };
        let reserved = u64::from(max_output.unwrap_or(0)).min(window / 2);
        let budget = u32::try_from(window - reserved).unwrap_or(u32::MAX);
        Self {
            budget,
            policy: CompactionPolicy {
                keep_recent: DEFAULT_KEEP_RECENT,
            },
        }
    }

    /// A conservative default: a 180k-token window (leaving room for output in a
    /// 200k model), keeping the last forty entries plus the first user message.
    pub fn defaults() -> Self {
        Self {
            budget: DEFAULT_BUDGET,
            policy: CompactionPolicy {
                keep_recent: DEFAULT_KEEP_RECENT,
            },
        }
    }

    pub fn new(budget: u32, keep_recent: usize) -> Self {
        Self {
            budget,
            policy: CompactionPolicy { keep_recent },
        }
    }

    pub fn budget(&self) -> u32 {
        self.budget
    }

    /// Plan the window for this turn. If the estimate overflows the budget and
    /// the history has a droppable middle, `compact` proposes dropping it; the
    /// engine applies the compaction and calls this again.
    pub(crate) fn plan(
        &self,
        history: &[HistoryEntry],
        system: &str,
        tools: &[TurnTool],
    ) -> WindowPlan {
        let window = history_to_messages(history);
        let estimate = estimate_tokens(system, tools, &window);
        let compact = if estimate > self.pack_budget() {
            self.compact_now(history)
        } else {
            None
        };
        WindowPlan {
            compact,
            estimate,
            window,
        }
    }

    /// Whether a request estimated at `estimate` tokens is close enough to the
    /// budget that only the provider's exact count can say it fits. See
    /// [`EXACT_COUNT_DIVISOR`].
    pub(crate) fn needs_exact_count(&self, estimate: u32) -> bool {
        estimate > self.budget / EXACT_COUNT_DIVISOR
    }

    /// Drop the middle of history right now, regardless of the estimate. Used
    /// when `count_tokens` disagrees with the estimate and the window still
    /// overflows after the estimate said it fit.
    pub(crate) fn compact_now(&self, history: &[HistoryEntry]) -> Option<Compaction> {
        let droppable = self.droppable_range(history)?;
        let summary = summarize(&history[droppable.clone()]);
        Some(Compaction {
            drop_end: droppable.end,
            drop_start: droppable.start,
            summary,
        })
    }

    /// The `[start, end)` range of history entries that compaction would drop:
    /// everything after the first user message and before the recent window.
    /// `None` when history is too short to compact.
    ///
    /// The end of the range is moved off a tool result before it is returned.
    /// A boundary chosen by entry count alone can strand an `Entry::Tool` whose
    /// `Entry::Model` is inside the range: the next request would then carry a
    /// `tool_result` whose `tool_use` no longer exists, which the API rejects.
    /// Because the fold rebuilds the same history from the journal, that turns
    /// one bad boundary into a session that cannot be resumed either.
    ///
    /// **A range holding nothing but summaries is not droppable, and saying
    /// otherwise was an infinite loop.** Applying a compaction removes the range
    /// and inserts one `Entry::Summary` where it was, so a range that is already
    /// a single summary drops one entry and adds one: history is the same length,
    /// the window is the same size, and the engine's `fit_window` loop asks
    /// again. It never stops. Each pass appends a `Compacted` record and, on the
    /// `count_tokens` path, makes an API call, so the session spins burning both
    /// until it is killed -- and `ContextOverflow`, the intended exit, is
    /// unreachable because something droppable always "exists".
    ///
    /// Requiring one non-summary entry makes every compaction strictly
    /// progressive, which is what makes the loop terminate.
    fn droppable_range(&self, history: &[HistoryEntry]) -> Option<std::ops::Range<usize>> {
        let first_user = history
            .iter()
            .position(|entry| matches!(entry.entry, Entry::User(_)))?;
        let keep_recent = self.policy.keep_recent.min(history.len());
        // Keep the first user message and the recent window; drop what is
        // strictly between them.
        let start = first_user + 1;
        let end = pair_boundary(history, history.len().saturating_sub(keep_recent));
        if start >= end {
            return None;
        }
        let has_real_content = history[start..end]
            .iter()
            .any(|entry| !matches!(entry.entry, Entry::Summary(_)));
        has_real_content.then_some(start..end)
    }

    /// The budget the estimate packs against: the full budget minus a sliver of
    /// headroom, so the estimate's error does not overflow before
    /// `count_tokens` verifies.
    fn pack_budget(&self) -> u32 {
        self.budget / BUDGET_HEADROOM * (BUDGET_HEADROOM - 1)
    }
}

impl Default for ContextManager {
    fn default() -> Self {
        Self::defaults()
    }
}

// ========================================================================
// Functions
// ========================================================================

/// Move `end` forward until it no longer splits a model turn from the tool
/// results answering it.
///
/// The results stranded at the boundary are dropped with the turn that asked for
/// them. That costs a little more history than `keep_recent` asked for, and buys
/// the invariant every surviving window needs: a `tool_result` the model can see
/// is one whose `tool_use` it can see too.
fn pair_boundary(history: &[HistoryEntry], mut end: usize) -> usize {
    while end < history.len() && matches!(history[end].entry, Entry::Tool { .. }) {
        end += 1;
    }
    end
}

/// The local token estimate of a packed request: bytes-per-token over the
/// system prompt, tool definitions, and messages. This is what the plan packs
/// against; `Provider::count_tokens` is the exact count the engine verifies
/// with before sending.
pub(crate) fn estimate_tokens(system: &str, tools: &[TurnTool], messages: &[TurnMessage]) -> u32 {
    let mut bytes = system.len();
    for tool in tools {
        bytes += tool.name.len() + tool.description.len() + tool.input_schema.to_string().len();
    }
    for message in messages {
        for block in &message.content {
            bytes += block_len(block);
        }
    }
    (bytes / BYTES_PER_TOKEN) as u32
}

fn block_len(block: &provider::TurnBlock) -> usize {
    use provider::TurnBlock;
    match block {
        TurnBlock::Text(text) => text.len(),
        TurnBlock::Thinking(thinking) => thinking.text.len() + thinking.signature.len(),
        TurnBlock::ToolResult(result) => result.content.len(),
        TurnBlock::ToolUse(use_) => use_.name.len() + use_.args.to_string().len(),
    }
}

/// Produce a mechanical summary of the dropped range: a count of the turns and
/// tool calls it contained. Deterministic, so compaction is offline-testable and
/// the summary does not depend on a model call.
fn summarize(range: &[HistoryEntry]) -> String {
    let mut turns = 0usize;
    let mut tool_calls = 0usize;
    let mut user_messages = 0usize;
    for entry in range {
        match &entry.entry {
            Entry::Model(_) => turns += 1,
            Entry::Tool { .. } => tool_calls += 1,
            Entry::User(_) => user_messages += 1,
            Entry::Summary(_) => {}
        }
    }
    format!(
        "[compacted {turns} assistant turn(s), {tool_calls} tool result(s), \
         {user_messages} user message(s)]"
    )
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use journal::{CallId, Outcome};
    use provider::{Role, TurnBlock, TurnMessage};

    use super::*;
    use crate::session::HistoryEntry;

    fn user(text: &str) -> HistoryEntry {
        HistoryEntry {
            entry: Entry::User(text.into()),
            ids: vec![],
        }
    }

    #[test]
    fn a_model_budget_is_its_window_less_the_output_a_turn_may_ask_for() {
        // The API counts prompt plus `max_tokens` against the window, so a
        // 200k model sending 64k of `max_tokens` holds 136k of prompt, not the
        // 180k the fixed default packed to.
        assert_eq!(
            ContextManager::for_model(Some(200_000), Some(64_000)).budget(),
            136_000
        );
        assert_eq!(
            ContextManager::for_model(Some(1_000_000), Some(64_000)).budget(),
            936_000
        );
        assert_eq!(
            ContextManager::for_model(Some(128_000), Some(100_000)).budget(),
            64_000,
            "an output share past half the window is capped there"
        );
        assert_eq!(
            ContextManager::for_model(None, Some(64_000)).budget(),
            DEFAULT_BUDGET,
            "an uncataloged model keeps the default"
        );
    }

    fn model(text: &str) -> HistoryEntry {
        HistoryEntry {
            entry: Entry::Model(vec![journal::Block::Text { text: text.into() }]),
            ids: vec![],
        }
    }

    fn tool(call: &str) -> HistoryEntry {
        HistoryEntry {
            entry: Entry::Tool {
                call: CallId::new(call.into()),
                outcome: Outcome::Ok {
                    content: "x".into(),
                },
            },
            ids: vec![],
        }
    }

    /// A history of `n` exchanges, each a big user message plus a model turn,
    /// so the byte estimate grows past a small budget.
    fn big_history(n: usize) -> Vec<HistoryEntry> {
        let mut history = vec![user("the task: always answer in lowercase")];
        for i in 0..n {
            history.push(model(&"x".repeat(2000)));
            history.push(tool(&format!("toolu_{i}")));
            let _ = i;
        }
        history
    }

    #[test]
    fn a_short_history_fits_without_compaction() {
        let context = ContextManager::new(10_000, 40);
        let history = vec![user("hi"), model("hello")];
        let plan = context.plan(&history, "system", &[]);
        assert!(plan.compact.is_none(), "a short history should fit");
        assert_eq!(plan.window.len(), 2);
    }

    #[test]
    fn an_overflowing_history_proposes_compaction() {
        let context = ContextManager::new(2_000, 2);
        let history = big_history(10);
        let plan = context.plan(&history, "system", &[]);
        let compaction = plan.compact.expect("overflow should compact");
        // The first user message survives; the recent window survives; the
        // middle is dropped.
        assert!(
            compaction.drop_start >= 1,
            "the first user message is never dropped"
        );
        assert!(compaction.drop_end <= history.len());
    }

    #[test]
    fn compaction_keeps_the_first_user_message() {
        // The early constraint (the task) must survive compaction, or the model
        // forgets what it was asked to do.
        let context = ContextManager::new(2_000, 2);
        let history = big_history(10);
        let compaction = context.compact_now(&history).expect("droppable");
        // After dropping [start, end), history[0] (the task) is retained.
        assert!(compaction.drop_start >= 1);
    }

    #[test]
    fn compaction_never_strands_a_tool_result() {
        // `big_history` alternates model and tool, so an even `keep_recent`
        // lands the boundary on a model turn and the pairing survives by luck.
        // An odd one lands it on a tool result whose model turn is inside the
        // drop range: the next request carries a `tool_result` with no matching
        // `tool_use` and the API rejects it, and because the fold rebuilds the
        // same history the resumed session is rejected too.
        let context = ContextManager::new(2_000, 3);
        let history = big_history(10);
        let compaction = context.compact_now(&history).expect("droppable");
        assert!(
            !matches!(history[compaction.drop_end].entry, Entry::Tool { .. }),
            "the surviving window must not begin with a stranded tool result"
        );
    }

    #[test]
    fn a_history_with_no_middle_is_not_droppable() {
        // First user message plus a recent window, nothing between: compaction
        // has nothing to drop, and a single oversized turn is the engine's
        // problem (Oversized), not the context manager's.
        let context = ContextManager::new(10, 40);
        let history = vec![user("task"), model("reply")];
        assert!(context.compact_now(&history).is_none(), "nothing to drop");
    }

    #[test]
    fn estimate_counts_system_tools_and_messages() {
        let messages = vec![TurnMessage {
            content: vec![TurnBlock::Text("a".repeat(400))],
            role: Role::User,
        }];
        let estimate = estimate_tokens(&"s".repeat(400), &[], &messages);
        assert_eq!(estimate, 200);
    }

    #[test]
    fn a_middle_that_is_only_a_summary_is_not_droppable() {
        // **The infinite loop.** Applying a compaction removes the range and
        // inserts one `Entry::Summary` where it was, so proposing a range that
        // *is* one summary drops one and adds one: history is unchanged, the
        // window is unchanged, and `fit_window` asks again forever, appending a
        // `Compacted` record and making a `count_tokens` call each pass. This is
        // the state a second compaction of an already-compacted session reaches.
        let context = ContextManager::new(10, 1);
        let history = vec![
            user("the task"),
            HistoryEntry {
                entry: Entry::Summary("compacted 9 turns".into()),
                ids: vec![],
            },
            model("recent"),
        ];

        assert!(
            context.compact_now(&history).is_none(),
            "compacting this would make no progress, so it must report none"
        );
    }

    #[test]
    fn a_middle_holding_real_content_is_still_droppable() {
        // The other side of the guard: one summary among real entries must not
        // stop compaction, or a session compacts exactly once and then overflows.
        let context = ContextManager::new(10, 1);
        let history = vec![
            user("the task"),
            HistoryEntry {
                entry: Entry::Summary("compacted 9 turns".into()),
                ids: vec![],
            },
            model("an actual turn"),
            model("recent"),
        ];

        let compaction = context
            .compact_now(&history)
            .expect("a range with real content is droppable");
        assert!(compaction.drop_end > compaction.drop_start);
    }

    #[test]
    fn summarize_counts_what_it_dropped() {
        let range = vec![user("ask"), model("reply"), tool("t")];
        let summary = summarize(&range);
        assert!(summary.contains("1 assistant turn"));
        assert!(summary.contains("1 tool result"));
        assert!(summary.contains("1 user message"));
    }
}

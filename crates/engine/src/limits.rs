//! The runaway guard: enforced, not just tracked.
//!
//! Tracking spend is not the same as stopping. The failure this exists to
//! prevent is the ordinary one: a model that cannot fix a failing test, retries
//! the same edit, re-reads the same file, and loops until someone notices.
//! Overnight, that is a lot of wasted tokens for zero work.

use journal::Usage;
use serde::{Deserialize, Serialize};

// ========================================================================
// Data Structures
// ========================================================================

/// Per-turn and per-session ceilings. Hitting any one stops the loop with
/// [`crate::Stop::HitLimit`] (an `Ok` outcome), not an error.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Max cumulative tokens per session. Counts `input + output + cache_write`
    /// and **excludes `cache_read`**: a cache hit reuses work already done, so
    /// it should not count toward the runaway budget.
    pub max_tokens: u64,
    /// Max tool calls per model turn.
    pub max_tool_calls: u32,
    /// Max model round-trips per `Engine::turn` call.
    pub max_turns: u32,
}

/// Which ceiling was hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LimitKind {
    Tokens,
    ToolCalls,
    Turns,
}

/// Cumulative usage across a session, folded from every surviving `ModelTurn`.
///
/// **`cache_read` is deliberately absent.** This is the runaway-token budget;
/// the context window budget counts `input + cache_read + cache_write` instead,
/// because a cached token is cheap but still occupies the window. The two must
/// not share a helper: reusing this number for the window is how a well-cached
/// session sails past the window while the spend counter reports it has barely
/// started.
#[derive(Debug, Default, Clone, Copy)]
pub struct Spend {
    cache_write: u64,
    input: u64,
    output: u64,
}

// ========================================================================
// Limits Implementations
// ========================================================================

impl Limits {
    /// Conservative defaults. Overridable: a limit that never fires protects
    /// nobody, and one that fires on legitimate work gets raised once and set to
    /// infinity in frustration.
    pub fn defaults() -> Self {
        Self {
            max_tokens: 2_000_000,
            max_tool_calls: 30,
            max_turns: 40,
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::defaults()
    }
}

// ========================================================================
// Spend Implementations
// ========================================================================

impl Spend {
    pub fn add(&mut self, usage: Usage) {
        self.cache_write += u64::from(usage.cache_write);
        self.input += u64::from(usage.input);
        self.output += u64::from(usage.output);
    }

    /// Replace the running total with a checkpoint carried on a `Compacted`
    /// record.
    ///
    /// **Assignment, not addition.** `Compacted.spent` is the *cumulative* total
    /// at the moment of compaction, so it already includes every turn folded
    /// before it; adding would double-count the surviving recent window.
    pub fn reset_to(&mut self, usage: Usage) {
        self.cache_write = u64::from(usage.cache_write);
        self.input = u64::from(usage.input);
        self.output = u64::from(usage.output);
    }

    /// The running total as a [`Usage`], for a checkpoint on a `Compacted` record.
    ///
    /// `cache_read` is not carried: [`Spend`] never accumulated it (it is
    /// excluded from the runaway budget), and inventing a zero for it here would
    /// be the same claim the field's absence already makes.
    pub fn as_usage(&self) -> Usage {
        Usage::new(
            self.input.try_into().unwrap_or(u32::MAX),
            self.output.try_into().unwrap_or(u32::MAX),
            0,
            self.cache_write.try_into().unwrap_or(u32::MAX),
        )
    }

    /// The runaway-budget token count: everything billed, nothing cached-read.
    pub fn tokens(&self) -> u64 {
        self.input + self.output + self.cache_write
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_exclude_cache_reads() {
        // A runaway guard that counted cache reads would stop a well-cached
        // session that had barely spent anything.
        let mut spend = Spend::default();
        spend.add(Usage::new(100, 50, 100_000, 25));
        assert_eq!(spend.tokens(), 175);
    }

    #[test]
    fn spend_accumulates_across_turns() {
        let mut spend = Spend::default();
        spend.add(Usage::new(100, 50, 0, 0));
        spend.add(Usage::new(100, 50, 0, 0));
        assert_eq!(spend.tokens(), 300);
    }
}

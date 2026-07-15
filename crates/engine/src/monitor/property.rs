//! The property contract.

use journal::Record;

// ========================================================================
// Data Structures
// ========================================================================

/// A property that did not hold, naming itself so a report can group by cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub detail: String,
    pub property: &'static str,
}

// ========================================================================
// Traits
// ========================================================================

/// A runtime property over the journal.
///
/// `observe` is **incremental**, called once per record in journal order, and
/// that is what lets one implementation serve both paths: the engine feeds
/// records as it writes them, and [`Monitor::check_journal`](crate::monitor::Monitor)
/// replays a finished file through the same code. A batch-only design would need
/// two implementations and they would drift.
///
/// `Send + Sync` like every other seam: the engine holds a `Monitor` and the
/// engine must stay `Send`.
pub trait Property: Send + Sync {
    fn name(&self) -> &'static str;
    fn observe(&mut self, record: &Record) -> Option<Violation>;
}

// ========================================================================
// Violation Implementations
// ========================================================================

impl Violation {
    pub fn new(property: &'static str, detail: String) -> Self {
        Self { detail, property }
    }
}

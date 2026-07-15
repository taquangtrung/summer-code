//! `ToolCx`: the only capability a tool receives.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::broker::Broker;

// ========================================================================
// Data Structures
// ========================================================================

/// The only capability a tool gets. No ambient authority.
///
/// Owned, with no lifetime parameter: the dispatcher spawns each call, and
/// every spawn API is `'static`. A borrowing `ToolCx<'a>` could not cross that
/// boundary, so the broker arrives as `Arc<Broker>` and `cwd` as `Arc<Path>`.
/// `Clone` lets the dispatcher hand the spawned task its own copy.
#[derive(Clone)]
pub struct ToolCx {
    /// The broker — the only source of authorized handles.
    pub broker: Arc<Broker>,
    /// The session working directory.
    pub cwd: Arc<Path>,
    /// Cooperative cancellation. Phase 1b wires the dispatcher's timeout to it.
    pub cancel: CancellationToken,
    /// Deadline for this call. Phase 1b enforces it.
    pub deadline: Instant,
}

// ========================================================================
// ToolCx Implementations
// ========================================================================

impl ToolCx {
    /// Construct a tool context.
    pub fn new(
        broker: Arc<Broker>,
        cwd: Arc<Path>,
        cancel: CancellationToken,
        deadline: Instant,
    ) -> Self {
        Self {
            broker,
            cwd,
            cancel,
            deadline,
        }
    }
}

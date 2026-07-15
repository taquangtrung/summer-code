//! Capturing what a file looked like before the agent changed it.
//!
//! **The broker is the right seam and the only one.** Every agent-driven write
//! goes through [`Broker::write`], which is the whole point of the typestate, so
//! a capture there covers the `edit` tool, any future tool that writes, and any
//! MCP server's tool too. Snapshotting inside the `edit` tool instead would
//! cover exactly the writes someone remembered to instrument.
//!
//! **What is captured is the state *before* the write**, taken between the
//! policy decision and the file being opened. That ordering is the feature: a
//! snapshot taken after the write records what the agent produced, which is
//! already on disk and is not what anyone wants back.
//!
//! The trait lives here and the implementation does not, for the reason
//! everything else in this crate is shaped that way: `sandbox` performs I/O for
//! agent-driven work and knows nothing about sessions, journals, or where a
//! store should live.
//!
//! [`Broker::write`]: crate::Broker::write

use std::path::Path;

// ========================================================================
// Traits
// ========================================================================

/// Somewhere to record a file's contents before it is overwritten.
///
/// **Every method is infallible on purpose.** A snapshot is a convenience, not a
/// guarantee the policy makes: if the store cannot be written, the write the
/// user asked for must still happen. Returning a `Result` here would put a
/// failure to record history on the critical path of making history, and the
/// only sane thing a caller could do with it is ignore it.
pub trait SnapshotSink: Send + Sync {
    /// Record the current contents of `path`, or its absence.
    ///
    /// Called once per authorized write, before the file is opened.
    fn capture(&self, path: &Path);
}

// ========================================================================
// Data Structures
// ========================================================================

/// A sink that records nothing.
///
/// The default, so a `Broker` built without snapshots behaves exactly as it did
/// before they existed rather than requiring every construction site to opt out.
pub struct NoSnapshots;

// ========================================================================
// NoSnapshots Implementations
// ========================================================================

impl SnapshotSink for NoSnapshots {
    fn capture(&self, _path: &Path) {}
}

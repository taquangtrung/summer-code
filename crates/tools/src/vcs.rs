//! The version-control seam. `Git` is the one implementation; the trait exists so
//! a golden test can assert what the agent asked the VCS to do without a real
//! `git` binary and a real commit graph. If that substitution never pays for
//! itself, collapse `Git` to a concrete struct; a second implementation is not
//! the justification.

use async_trait::async_trait;
use journal::Outcome;
use sandbox::ToolCx;

use crate::command::run_command;

// ========================================================================
// Traits
// ========================================================================

#[async_trait]
pub trait Vcs: Send + Sync {
    async fn run(&self, cx: &ToolCx, args: &[String]) -> Outcome;
}

// ========================================================================
// Data Structures
// ========================================================================

/// Runs `git` through the broker.
pub struct Git;

// ========================================================================
// Git Implementations
// ========================================================================

impl Git {
    pub fn new() -> Self {
        Self
    }
}

impl Default for Git {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Vcs for Git {
    /// The same path as `shell` and `grep`, which it previously was not.
    ///
    /// Running `git` by hand here is what lost its stderr (so a `git commit`
    /// with nothing staged was reported as an empty success), discarded its exit
    /// status, and left it uninterruptible: with no `cx.cancel` race, a long
    /// `git clone` held the turn until the dispatcher's timeout, contradicting
    /// the documented "Ctrl-C during dispatch is immediate".
    async fn run(&self, cx: &ToolCx, args: &[String]) -> Outcome {
        run_command(cx, "git", args).await
    }
}

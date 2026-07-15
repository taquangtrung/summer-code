//! The seam that resolves `Decision::Ask`.
//!
//! The evaluator returns `Ask` to the *broker*, so the broker is what must
//! resolve it. That makes `Prompter` a seam on the broker rather than a CLI
//! concern: a prompt implemented only in `cli` is on the wrong side of
//! the boundary and cannot answer a question the broker is holding. The CLI
//! implements it against a terminal, the Phase 7 daemon forwards it to whichever
//! frontend is attached, and tests supply an auto-deny.
//!
//! **`ask` is async, and that is why all five broker methods are.** It waits on a
//! human at a terminal or, in Phase 7, on an IPC round-trip. Neither has an upper
//! bound; a user can walk away for an hour. The broker is called from inside a
//! spawned tool task, so a blocking `ask` holds the worker thread polling that
//! task for as long as the human takes, and a runtime with N workers and N
//! pending prompts is deadlocked with every session waiting on a person who is
//! waiting on the screen.

use async_trait::async_trait;
use policy::{Request, Scope};

// ========================================================================
// Data Structures
// ========================================================================

/// What the user answered: whether to allow, and how far the answer reaches.
#[derive(Debug, Clone)]
pub struct Grant {
    pub allow: bool,
    pub scope: Scope,
}

/// A write the user is being asked about, with the content it would leave.
///
/// The request alone names a path, and "write `src/lib.rs`" is a question
/// nobody can answer well: the answer depends on *what* is written. The broker
/// builds this only when it is about to prompt, reading `before` itself, so a
/// write the policy already settles costs no read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WritePreview {
    /// What the file would hold after the write.
    pub after: String,
    /// What the file holds now; empty for a file that does not exist yet.
    pub before: String,
}

/// A `Prompter` that denies everything without asking.
///
/// For tests, so an unexpected `Ask` is a **failure rather than a hang**. A test
/// that blocks forever on a prompt nobody is there to answer looks like a
/// deadlock in the code under test.
pub struct AutoDeny;

// ========================================================================
// Traits
// ========================================================================

#[async_trait]
pub trait Prompter: Send + Sync {
    async fn ask(&self, request: &Request) -> Grant;

    /// Ask about a write, shown with what it would change.
    ///
    /// Defaults to [`Prompter::ask`], so a prompter with nowhere to show a
    /// diff (a test, an unattended run) answers exactly as before.
    async fn ask_previewed(&self, request: &Request, _preview: &WritePreview) -> Grant {
        self.ask(request).await
    }
}

// ========================================================================
// Grant Implementations
// ========================================================================

impl Grant {
    /// Allow this one request and nothing else.
    pub fn once(allow: bool) -> Self {
        Self {
            allow,
            scope: Scope::Once,
        }
    }
}

// ========================================================================
// AutoDeny Implementations
// ========================================================================

#[async_trait]
impl Prompter for AutoDeny {
    async fn ask(&self, _request: &Request) -> Grant {
        Grant::once(false)
    }
}

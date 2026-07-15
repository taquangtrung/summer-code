//! The backend that enforces nothing, on purpose.
//!
//! This is the Phase 1 path, the Windows path, and the test path. The policy
//! check still runs and every decision is still journaled; what is missing is the
//! second line of defense. It reports every dimension `Unenforced` so the startup
//! check can say that out loud instead of letting the absence pass for safety.

use std::io;

use policy::Policy;
use tokio::process::Command;

use super::{Capabilities, Harness, SandboxBackend};

// ========================================================================
// Data Structures
// ========================================================================

/// No OS enforcement, with a reason for the report.
pub struct NoneBackend {
    reason: String,
}

// ========================================================================
// NoneBackend Implementations
// ========================================================================

impl NoneBackend {
    pub fn new(reason: &str) -> Self {
        Self {
            reason: reason.to_string(),
        }
    }
}

impl SandboxBackend for NoneBackend {
    fn name(&self) -> &'static str {
        "none"
    }

    fn capabilities(&self, _policy: &Policy) -> Capabilities {
        Capabilities::all_unenforced(&self.reason)
    }

    fn enter(&self, _policy: &Policy, _harness: &Harness) -> io::Result<()> {
        Ok(())
    }

    fn wrap(&self, _command: &mut Command) {}
}

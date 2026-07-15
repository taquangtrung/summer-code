//! The seatbelt backend: OS-level enforcement on macOS.
//!
//! Seatbelt does not share Landlock's constraints: `sandbox_init` applies to the
//! process, there is no per-thread ruleset and no ABI ladder to probe. That is
//! why `enter` is on the trait as a per-worker call rather than a per-process
//! one.
//!
//! **This backend reports `paths` and `exec` unenforced and returns an error
//! from `enter`, and that is deliberate.** `sandbox_init` is deprecated, the
//! profile language is undocumented, and nothing here can be compiled or run on
//! the Linux machine this was written on. A backend that reported `Enforced`
//! without a tested profile behind it would be worse than one that reports
//! nothing: the startup check would clear a policy the OS is not actually
//! backing, which is the exact failure `capabilities()` exists to prevent.
//! macOS users get the honest answer and an unconfined session until a profile
//! lands and is tested on a real machine.

use std::io;

use policy::Policy;
use tokio::process::Command;

use super::{Capabilities, Harness, SandboxBackend};

// ========================================================================
// Constants
// ========================================================================

const UNIMPLEMENTED: &str =
    "the seatbelt profile is not implemented yet; run without --sandbox or use a container";

// ========================================================================
// Data Structures
// ========================================================================

pub struct SeatbeltBackend;

// ========================================================================
// SeatbeltBackend Implementations
// ========================================================================

impl SeatbeltBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SeatbeltBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SandboxBackend for SeatbeltBackend {
    fn name(&self) -> &'static str {
        "seatbelt"
    }

    fn capabilities(&self, _policy: &Policy) -> Capabilities {
        Capabilities::all_unenforced(UNIMPLEMENTED)
    }

    /// Refuses rather than silently doing nothing. A no-op `enter` that returned
    /// `Ok` would let a caller believe the process was confined.
    fn enter(&self, _policy: &Policy, _harness: &Harness) -> io::Result<()> {
        Err(io::Error::other(UNIMPLEMENTED))
    }

    fn wrap(&self, _command: &mut Command) {}
}

//! The OS-enforcement seam: the second line of defense behind the typestate.
//!
//! **The structural guarantee is that the check was reached, which is not the
//! same as the OS having enforced anything.** Landlock degrades quietly: network
//! rules need ABI v4 (kernel 6.7), an older kernel silently gives you a ruleset
//! without them, and running inside a container that already restricts the
//! syscall can leave `enter` a no-op. The `none` backend enforces nothing at all,
//! on purpose. The runtime monitor cannot catch any of this, because it reads the
//! journal, and the journal faithfully records that a decision was made.
//!
//! So [`SandboxBackend::capabilities`] is a required part of the trait rather
//! than a diagnostic, and it reports what this backend can enforce **on the
//! kernel it is running on now**, not what it enforces in principle. The broker
//! compares that against what the policy demands at startup and refuses to run a
//! policy it cannot back. Without it, "network off" is a statement of intent that
//! reads exactly like a fact.

mod none;

#[cfg(target_os = "linux")]
mod landlock;

#[cfg(target_os = "macos")]
mod seatbelt;

use std::io;

use policy::Policy;
use tokio::process::Command;

pub use none::NoneBackend;

#[cfg(target_os = "linux")]
pub use landlock::LandlockBackend;

#[cfg(target_os = "macos")]
pub use seatbelt::SeatbeltBackend;

// ========================================================================
// Data Structures
// ========================================================================

/// What a backend can actually enforce, per dimension.
#[derive(Debug, Clone)]
pub struct Capabilities {
    pub connect: Enforcement,
    pub exec: Enforcement,
    pub listen: Enforcement,
    pub paths: Enforcement,
}

/// Whether one dimension is backed by the OS, for the policy actually loaded.
///
/// `Unenforced` carries a reason, because "your kernel is too old for Landlock
/// network rules" and "you asked for the none backend" are different
/// conversations.
///
/// **`Partial` exists because "enforced" was an overclaim, and collapsing it
/// into `Unenforced` would be an under-claim that refuses to start.** Landlock's
/// network rules name a *port*; they cannot name an address. So under the
/// default policy -- loopback, any port -- the kernel contributes nothing to the
/// `listen` dimension, while the broker still enforces it completely by choosing
/// the bind address itself. Reporting that as `Enforced` claims a second line of
/// defense that is not there; reporting it as `Unenforced` makes the shipped
/// default refuse to start. It is neither: the dimension is enforced, and not by
/// the kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enforcement {
    Enforced,
    /// The backend enforces less than the policy expresses. Not fatal: the
    /// broker still checks every access, and the reason names what the kernel is
    /// not adding on top.
    Partial {
        reason: String,
    },
    Unenforced {
        reason: String,
    },
}

// ========================================================================
// Traits
// ========================================================================

pub trait SandboxBackend: Send + Sync {
    /// The backend's name, for the startup report.
    fn name(&self) -> &'static str;

    /// What this backend enforces **for `policy`, on the kernel it is running
    /// on now**.
    ///
    /// Both halves matter and both were missing. Landlock degrades quietly by
    /// kernel version, so this cannot report what the backend enforces in
    /// principle. It also cannot report per-kernel alone: a Landlock rule names
    /// a port, so whether the `listen` dimension is genuinely backed depends on
    /// whether *this policy* names ports. Reporting a fixed answer per ABI
    /// claimed enforcement that the loaded policy did not get.
    fn capabilities(&self, policy: &Policy) -> Capabilities;

    /// Restrict the calling thread and everything it later spawns.
    ///
    /// Per worker, not per process, because seatbelt and future backends need
    /// not share landlock's constraints. For landlock specifically a worker must
    /// be a process: rulesets are per-thread, inherited only forward, and
    /// irreversible, so a work-stealing runtime could poll one session's task on
    /// a worker holding another session's ruleset. Phase 7 implements that; this
    /// phase has exactly one worker and pays nothing for the generality.
    ///
    /// `harness` names the egress the *harness* needs and the policy does not
    /// govern. See [`Harness`] for why that parameter has to exist.
    fn enter(&self, policy: &Policy, harness: &Harness) -> io::Result<()>;

    /// Wrap a command before it is spawned. Container backends do their work
    /// here and need no `enter` at all.
    fn wrap(&self, command: &mut Command);
}

// ========================================================================
// Capabilities Implementations
// ========================================================================

impl Capabilities {
    /// Nothing is enforced, for one stated reason.
    pub fn all_unenforced(reason: &str) -> Self {
        let unenforced = Enforcement::Unenforced {
            reason: reason.to_string(),
        };
        Self {
            connect: unenforced.clone(),
            exec: unenforced.clone(),
            listen: unenforced.clone(),
            paths: unenforced,
        }
    }

    /// The dimensions the OS backs with nothing at all. Empty means every
    /// dimension has a second line of defense; this is what
    /// [`crate::BrokerBuilder::build`] refuses on.
    pub fn gaps(&self) -> Vec<Gap> {
        self.dimensions(|enforcement| match enforcement {
            Enforcement::Unenforced { reason } => Some(reason.clone()),
            Enforcement::Enforced | Enforcement::Partial { .. } => None,
        })
    }

    /// The dimensions the OS backs less completely than the policy expresses.
    ///
    /// **Reported, not refused.** A limit means the broker's check is doing all
    /// the work on some part of a dimension, which is a real reduction in depth
    /// and not a reason to decline to run: the policy is still enforced. Refusing
    /// here would make the shipped default policy unusable on Linux, which would
    /// teach users to drop `--sandbox` and lose *everything*.
    pub fn limits(&self) -> Vec<Gap> {
        self.dimensions(|enforcement| match enforcement {
            Enforcement::Partial { reason } => Some(reason.clone()),
            Enforcement::Enforced | Enforcement::Unenforced { .. } => None,
        })
    }

    fn dimensions(&self, pick: impl Fn(&Enforcement) -> Option<String>) -> Vec<Gap> {
        [
            ("connect", &self.connect),
            ("exec", &self.exec),
            ("listen", &self.listen),
            ("paths", &self.paths),
        ]
        .into_iter()
        .filter_map(|(dimension, enforcement)| {
            pick(enforcement).map(|reason| Gap { dimension, reason })
        })
        .collect()
    }
}

/// One dimension the policy wanted and the backend cannot back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gap {
    pub dimension: &'static str,
    pub reason: String,
}

/// The harness's own egress, which the policy does not govern.
///
/// **This exists because an OS sandbox cannot make the distinction the threat
/// model rests on.** Agent-driven work is anything a tool call caused; the
/// provider's socket is harness-driven, and the plan is explicit that the
/// policy's network switch "does not gate the model connection and could not,
/// since the agent needs the model to run at all".
///
/// Landlock's network rules are **per process**. The kernel sees a connect and
/// cannot ask who wanted it, so a ruleset that denied outbound to enforce
/// `Connect: Deny` would kill the model connection along with the tools. The
/// carve-out is the resolution: the backend permits the provider's port and
/// nothing else, and the broker still denies a *tool* that asks for it. The cost
/// is stated rather than hidden: on that one port the kernel is not a second
/// line of defense, and the broker's check stands alone.
#[derive(Debug, Clone)]
pub struct Harness {
    /// The port the model transport reaches out on, from the provider's
    /// `base_url`. Not hardcoded to 443: the base URL is overridable for a
    /// proxy, and a carve-out for the wrong port would break the connection it
    /// exists to preserve.
    pub egress_ports: Vec<u16>,
}

// ========================================================================
// Harness Implementations
// ========================================================================

impl Harness {
    pub fn new(egress_ports: Vec<u16>) -> Self {
        Self { egress_ports }
    }

    /// Nothing is carved out. For a worker that will not talk to the model.
    pub fn none() -> Self {
        Self {
            egress_ports: Vec::new(),
        }
    }
}

// ========================================================================
// Functions
// ========================================================================

/// The backend for this platform.
///
/// **Windows gets `none` and no OS enforcement.** That is a decision, not an
/// oversight: the policy check still runs, the broker still evaluates every
/// request, the journal still records every decision, and the monitor still
/// verifies it. What is missing is the second line of defense, so a bug in the
/// broker on Windows has nothing behind it. The startup check says so out loud
/// rather than implying a protection that is not there.
pub fn for_platform() -> Box<dyn SandboxBackend> {
    #[cfg(target_os = "linux")]
    {
        Box::new(LandlockBackend::new())
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(SeatbeltBackend::new())
    }
    // Named separately from the generic fallback so the message can say what to
    // do instead. "No backend" is accurate and unactionable; a Windows user
    // reading it at startup needs the next step, not the diagnosis.
    #[cfg(windows)]
    {
        Box::new(NoneBackend::new(
            "Windows has no OS-level sandbox backend yet, so the broker's policy \
             check is the only enforcement: it still evaluates and journals every \
             access, with nothing behind it. For kernel-backed confinement run \
             the session inside WSL2 or a container",
        ))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        Box::new(NoneBackend::new(
            "this platform has no OS-level sandbox backend",
        ))
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gaps_name_every_unenforced_dimension() {
        let capabilities = Capabilities::all_unenforced("none backend");
        let gaps = capabilities.gaps();
        assert_eq!(gaps.len(), 4);
        assert!(gaps.iter().all(|gap| gap.reason == "none backend"));
    }

    #[test]
    fn a_fully_enforced_backend_has_no_gaps() {
        let capabilities = Capabilities {
            connect: Enforcement::Enforced,
            exec: Enforcement::Enforced,
            listen: Enforcement::Enforced,
            paths: Enforcement::Enforced,
        };
        assert!(capabilities.gaps().is_empty());
    }

    #[test]
    fn gaps_report_only_the_missing_dimension() {
        let capabilities = Capabilities {
            connect: Enforcement::Unenforced {
                reason: "needs ABI v4".into(),
            },
            exec: Enforcement::Enforced,
            listen: Enforcement::Enforced,
            paths: Enforcement::Enforced,
        };
        let gaps = capabilities.gaps();
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].dimension, "connect");
    }
}

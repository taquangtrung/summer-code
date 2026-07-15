//! The read-only gate `/mode plan` closes mid-session.
//!
//! **Answering prompts with "no" is not a plan mode.** The TUI's plan toggle
//! used to do only that, and the built-in policy allows writes under the root
//! without a prompt, so a session switched to "Planning (read-only)" went on
//! editing files. The refusal has to come from the broker's decision path,
//! ahead of the policy's own allows, or it is a label.
//!
//! **The gate is an evaluator, not a swapped policy.** The broker owns its
//! policy by value and `mode.rs` keeps it that way; a flag read on the way in
//! changes no ownership and puts no lock on the decision. What the gate denies
//! is the same `plan` overlay `--mode plan` merges, parsed by the same parser,
//! so the two ways into plan mode cannot disagree about what read-only means.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use policy::{Decision, Policy, PolicyEvaluator, Request, RulesEvaluator};

// ========================================================================
// Constants
// ========================================================================

/// The refusal the model reads. It names the mode and what to do instead, so
/// the agent answers with a plan rather than retrying the write another way.
const PLAN_REASON: &str = "the session is in plan mode (read-only): describe the change instead \
                           of making it; the user leaves plan mode with /mode or Shift+Tab";

// ========================================================================
// Data Structures
// ========================================================================

/// The built-in rules, with the `plan` overlay on top while the gate is closed.
pub struct ReadOnlyGate {
    /// Set while the session is in plan mode. Shared with the TUI, which is
    /// the only thing that flips it.
    closed: Arc<AtomicBool>,
    /// The `plan` overlay on its own, rooted where the session is.
    plan: Policy,
    rules: RulesEvaluator,
}

// ========================================================================
// ReadOnlyGate Implementations
// ========================================================================

impl ReadOnlyGate {
    pub fn new(closed: Arc<AtomicBool>, plan: Policy) -> Self {
        Self {
            closed,
            plan,
            rules: RulesEvaluator::new(),
        }
    }
}

impl PolicyEvaluator for ReadOnlyGate {
    /// The session's decision, overruled only for writes and commands.
    ///
    /// Reads and the network pass through: the overlay says nothing about them,
    /// and on its own it would deny every connection, the model's included.
    /// A command the overlay allows (the searches the reading tools exec) is
    /// let through where the session would only have asked, because a prompt
    /// in plan mode is answered "no" and a plan that cannot `grep` is crippled.
    /// A session deny still wins over that allow.
    fn evaluate(&self, policy: &Policy, request: &Request) -> Decision {
        let decision = self.rules.evaluate(policy, request);
        if !self.closed.load(Ordering::Relaxed) {
            return decision;
        }
        match request {
            Request::Exec { .. } | Request::Write(_) => {}
            Request::Connect { .. } | Request::Listen { .. } | Request::Read(_) => {
                return decision;
            }
        }
        match (self.rules.evaluate(&self.plan, request), decision) {
            (Decision::Deny { .. }, _) => Decision::Deny {
                reason: PLAN_REASON.to_string(),
            },
            (Decision::Allow, Decision::Ask) => Decision::Allow,
            (Decision::Allow | Decision::Ask, decision) => decision,
        }
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::policy::plan_overlay;

    fn gate(closed: bool) -> (ReadOnlyGate, Policy) {
        let root = PathBuf::from("/repo");
        let plan = plan_overlay(&root).expect("the plan overlay parses");
        let gate = ReadOnlyGate::new(Arc::new(AtomicBool::new(closed)), plan);
        (gate, Policy::builtin(root))
    }

    fn exec(program: &str) -> Request {
        Request::Exec {
            program: program.to_string(),
            args: Vec::new(),
        }
    }

    #[test]
    fn test_closed_gate_denies_a_write_the_policy_allows() {
        // The bug this module exists for: the built-in policy allows a write
        // under the root outright, so no prompt ever reached the plan toggle.
        let (open, policy) = gate(false);
        let write = Request::Write(PathBuf::from("/repo/src/main.rs"));
        assert_eq!(open.evaluate(&policy, &write), Decision::Allow);
        let (closed, policy) = gate(true);
        assert!(matches!(
            closed.evaluate(&policy, &write),
            Decision::Deny { reason } if reason == PLAN_REASON
        ));
    }

    #[test]
    fn test_closed_gate_denies_commands_but_still_searches() {
        let (closed, policy) = gate(true);
        assert!(matches!(
            closed.evaluate(&policy, &exec("sh")),
            Decision::Deny { .. }
        ));
        assert_eq!(closed.evaluate(&policy, &exec("rg")), Decision::Allow);
    }

    #[test]
    fn test_closed_gate_leaves_reads_and_the_network_to_the_policy() {
        // The overlay has no network rules, so consulting it for a connection
        // would deny the model's own socket.
        let (closed, policy) = gate(true);
        let read = Request::Read(PathBuf::from("/repo/README.md"));
        let connect = Request::Connect {
            host: "example.com".to_string(),
            port: 443,
        };
        assert_eq!(
            closed.evaluate(&policy, &read),
            RulesEvaluator::new().evaluate(&policy, &read)
        );
        assert_eq!(
            closed.evaluate(&policy, &connect),
            RulesEvaluator::new().evaluate(&policy, &connect)
        );
    }
}

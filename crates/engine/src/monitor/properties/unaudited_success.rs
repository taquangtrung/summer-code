//! "A tool call that did work left an audit trail."
//!
//! **Not** "no access happens without a matching grant", which is what this file
//! used to claim and cannot deliver. The difference is not pedantry: it is the
//! boundary between what the journal can witness and what only the kernel can.

use std::collections::HashSet;

use journal::{CallId, Outcome, Record, RecordKind};

use crate::monitor::property::{Property, Violation};

// ========================================================================
// Constants
// ========================================================================

const NAME: &str = "unaudited-success";

/// Tools that answer from memory and so have nothing to audit.
///
/// **A hand-maintained claim, and the one thing in this file that can create a
/// blind spot.** The monitor reads the journal; it cannot ask the tool registry
/// what a tool touches, so a name listed here is exempted from the single
/// property that can witness a call bypassing the broker. Listing a tool that
/// does reach the filesystem would hide exactly what this exists to catch, so
/// the list stays short and each entry has to be true by construction rather
/// than by current behaviour: `skill` returns a body from a registry built
/// before the sandbox closed, and `plan` formats its own arguments back.
///
/// The alternative, before it is asked for: leaving them out. That was the
/// state until skills were first used, and it made every skill load print a
/// property violation. A monitor that cries wolf on correct behaviour is worse
/// than one property short, because the next real violation reads as more
/// noise.
const IN_MEMORY_TOOLS: &[&str] = &["plan", "skill"];

// ========================================================================
// Data Structures
// ========================================================================

/// Flags a tool call that succeeded without a single recorded permission check.
///
/// The broker sends a record on **every** evaluation, including auto-allows from
/// remembered rules, so a call that touched anything has at least one
/// `Permission` record. A succeeded call with none did work that never reached
/// the broker, which is the one shape of bypass the journal can witness.
///
/// **What this cannot see, stated plainly, because the gap is load-bearing.**
///
/// The journal has no witness of an access independent of the `Permission` record
/// that authorized it. So "this specific write had a grant" is true by
/// construction, not by checking, and this property is strictly weaker than its
/// old title ("no access happens without a matching grant") advertised. It
/// detects a call that made *zero* broker requests. It does not, and cannot,
/// match a grant to an access.
///
/// The concrete hole that follows: `shell` execs `sh`, the broker records one
/// `Access::Exec`, and the child then reads `~/.aws/credentials` and writes
/// `/etc/cron.d/x` without a single syscall passing through the broker. This
/// property sees an audited call that succeeded. `OutsideRoot` sees an `Exec`,
/// which it ignores, because execing `/bin/sh` is outside the root and normal.
/// The entire exec subtree is invisible to both, and no journal-reading property
/// can change that: a monitor reads what the harness recorded, and the harness
/// never saw it.
///
/// **That subtree is the `SandboxBackend`'s job, not the monitor's**, and it is
/// why the backend is a second line of defense rather than a nicety: landlock
/// and seatbelt constrain the child because the *kernel* enforces what the
/// broker cannot observe. On the `none` backend, in a session without
/// `--sandbox`, or on Windows, nothing covers it, and `capabilities()` is what
/// says so at startup instead of letting the gap read as a guarantee.
///
/// Only `Ok` is checked. A denied, failed or timed-out call may legitimately
/// have died before it asked the broker for anything.
#[derive(Default)]
pub struct UnauditedSuccess {
    checked: HashSet<CallId>,
    /// Calls to a tool in [`IN_MEMORY_TOOLS`], which has nothing to audit.
    in_memory: HashSet<CallId>,
}

// ========================================================================
// UnauditedSuccess Implementations
// ========================================================================

impl UnauditedSuccess {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Property for UnauditedSuccess {
    fn name(&self) -> &'static str {
        NAME
    }

    fn observe(&mut self, record: &Record) -> Option<Violation> {
        match &record.kind {
            RecordKind::Permission(permission) => {
                self.checked.insert(permission.call.clone());
                None
            }
            // The name arrives before the result, so the exemption is known by
            // the time it is needed.
            RecordKind::ToolCall(call) if IN_MEMORY_TOOLS.contains(&call.name.as_str()) => {
                self.in_memory.insert(call.call.clone());
                None
            }
            RecordKind::ToolResult(result) => match &result.outcome {
                Outcome::Ok { .. }
                    if !self.checked.contains(&result.call)
                        && !self.in_memory.contains(&result.call) =>
                {
                    Some(Violation::new(
                        NAME,
                        format!(
                            "call `{}` succeeded with no recorded permission check",
                            result.call
                        ),
                    ))
                }
                Outcome::Denied { .. }
                | Outcome::Err { .. }
                | Outcome::Ok { .. }
                | Outcome::TimedOut => None,
            },
            _ => None,
        }
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use journal::{Access, Decision, Scope};

    use super::*;
    use crate::monitor::testing::{permission_record, tool_call_record, tool_result_record};

    fn call() -> CallId {
        CallId::new("toolu_1".into())
    }

    fn grant(call: CallId) -> Record {
        permission_record(
            call,
            Access::Write {
                path: "/repo/a".into(),
            },
            Decision::Allowed,
            Scope::Once,
        )
    }

    fn ok(call: CallId) -> Record {
        tool_result_record(
            call,
            Outcome::Ok {
                content: "done".into(),
            },
        )
    }

    #[test]
    fn a_success_with_no_check_is_flagged() {
        let mut property = UnauditedSuccess::new();
        let violation = property.observe(&ok(call())).expect("must be flagged");
        assert_eq!(violation.property, NAME);
    }

    #[test]
    fn a_checked_success_passes() {
        let mut property = UnauditedSuccess::new();
        assert!(property.observe(&grant(call())).is_none());
        assert!(property.observe(&ok(call())).is_none());
    }

    #[test]
    fn a_tool_that_answers_from_memory_is_not_flagged() {
        // The false positive this exemption exists for: loading a skill reads a
        // registry built before the sandbox closed, so it makes no broker
        // request and there is nothing for it to be audited against. Left
        // flagged, every skill load printed a violation, which teaches the
        // reader to skim past the real ones.
        let mut property = UnauditedSuccess::new();
        assert!(
            property
                .observe(&tool_call_record(call(), "skill"))
                .is_none()
        );
        assert!(property.observe(&ok(call())).is_none());
    }

    #[test]
    fn a_tool_that_touches_the_tree_is_still_flagged() {
        // The other half: the exemption is by name, and a name not on the list
        // gets no relief from having been announced.
        let mut property = UnauditedSuccess::new();
        property.observe(&tool_call_record(call(), "edit"));
        assert!(property.observe(&ok(call())).is_some());
    }

    #[test]
    fn a_grant_for_another_call_does_not_cover_this_one() {
        // The bug this catches: keying on "any permission seen" rather than on
        // the call would let one audited call vouch for every later one.
        let mut property = UnauditedSuccess::new();
        property.observe(&grant(CallId::new("toolu_other".into())));
        assert!(property.observe(&ok(call())).is_some());
    }

    #[test]
    fn a_failed_call_needs_no_grant() {
        let mut property = UnauditedSuccess::new();
        let failed = tool_result_record(
            call(),
            Outcome::Err {
                message: "boom".into(),
            },
        );
        assert!(property.observe(&failed).is_none());
        assert!(property.observe(&ok(CallId::new("t2".into()))).is_some());
    }
}

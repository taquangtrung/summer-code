//! "No tool call executes outside the declared root."

use std::path::{Path, PathBuf};

use journal::{Access, Decision, Record, RecordKind};
use policy::normalize;

use crate::monitor::properties::allowed;
use crate::monitor::property::{Property, Violation};

// ========================================================================
// Constants
// ========================================================================

const NAME: &str = "outside-root";

// ========================================================================
// Data Structures
// ========================================================================

/// Flags a path access that was **allowed** while resolving outside the policy
/// root.
///
/// The property is about the check being reached and answered consistently with
/// the declared root, not about what a permissive policy chooses: a denied
/// access outside the root is the system working, so only an allow is a
/// violation.
pub struct OutsideRoot {
    root: PathBuf,
}

// ========================================================================
// OutsideRoot Implementations
// ========================================================================

impl OutsideRoot {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The path is normalized with the *same* function the evaluator used.
    ///
    /// Checking `starts_with` on the raw path would pass
    /// `/repo/../../etc/passwd`, since its components do begin with `/repo`.
    /// The broker canonicalizes before journaling, so in practice such a path
    /// should never appear; but the monitor's whole value is being an
    /// independent check, and one that assumes the broker was correct is not
    /// independent.
    fn check(&self, path: &Path, decision: Decision) -> Option<Violation> {
        let normalized = normalize(path);
        if !allowed(decision) || normalized.starts_with(&self.root) {
            return None;
        }
        Some(Violation::new(
            NAME,
            format!(
                "allowed access to `{}`, which is outside the declared root `{}`",
                normalized.display(),
                self.root.display()
            ),
        ))
    }
}

impl Property for OutsideRoot {
    fn name(&self) -> &'static str {
        NAME
    }

    fn observe(&mut self, record: &Record) -> Option<Violation> {
        let RecordKind::Permission(permission) = &record.kind else {
            return None;
        };
        match &permission.access {
            Access::Read { path } | Access::Write { path } => self.check(path, permission.decision),
            // Exec, Connect and Listen are not path-scoped, so the root says
            // nothing about them. They have their own rules in the policy.
            Access::Connect(_) | Access::Exec(_) | Access::Listen(_) => None,
        }
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::testing::{permission_record, tool_result_record};
    use journal::{CallId, Outcome, Scope};

    fn property() -> OutsideRoot {
        OutsideRoot::new(PathBuf::from("/repo"))
    }

    fn call() -> CallId {
        CallId::new("toolu_1".into())
    }

    #[test]
    fn an_allowed_escape_is_a_violation() {
        let record = permission_record(
            call(),
            Access::Read {
                path: "/etc/passwd".into(),
            },
            Decision::Allowed,
            Scope::Once,
        );
        let violation = property().observe(&record).expect("escape must be flagged");
        assert_eq!(violation.property, NAME);
        assert!(violation.detail.contains("/etc/passwd"));
    }

    #[test]
    fn a_denied_escape_is_the_system_working() {
        // The property is about the check being reached, not about the answer.
        let record = permission_record(
            call(),
            Access::Read {
                path: "/etc/passwd".into(),
            },
            Decision::Denied,
            Scope::Once,
        );
        assert!(property().observe(&record).is_none());
    }

    #[test]
    fn an_allowed_access_under_root_is_fine() {
        let record = permission_record(
            call(),
            Access::Write {
                path: "/repo/src/foo.rs".into(),
            },
            Decision::AskedAllowed,
            Scope::Once,
        );
        assert!(property().observe(&record).is_none());
    }

    #[test]
    fn a_remembered_allow_is_still_an_allow() {
        // "The user said yes to something like this earlier" still let it
        // through, so it is still subject to the root.
        let record = permission_record(
            call(),
            Access::Write {
                path: "/elsewhere/x".into(),
            },
            Decision::RememberedAllow,
            Scope::Once,
        );
        assert!(property().observe(&record).is_some());
    }

    #[test]
    fn a_dotdot_escape_is_caught_even_though_it_starts_with_the_root() {
        // `/repo/../../etc/passwd` passes a raw `starts_with("/repo")`. The
        // monitor has to normalize with the same function the evaluator used, or
        // it silently blesses the one string it exists to catch.
        let record = permission_record(
            call(),
            Access::Read {
                path: "/repo/../../etc/passwd".into(),
            },
            Decision::Allowed,
            Scope::Once,
        );
        let violation = property()
            .observe(&record)
            .expect("a normalized escape must be flagged");
        assert!(violation.detail.contains("/etc/passwd"));
    }

    #[test]
    fn non_permission_records_are_ignored() {
        let record = tool_result_record(
            call(),
            Outcome::Ok {
                content: "ok".into(),
            },
        );
        assert!(property().observe(&record).is_none());
    }
}

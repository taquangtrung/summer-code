//! "A call's grants are journaled before its result."

use std::collections::HashSet;

use journal::{CallId, Record, RecordKind};

use crate::monitor::property::{Property, Violation};

// ========================================================================
// Constants
// ========================================================================

const NAME: &str = "grant-order";

// ========================================================================
// Data Structures
// ========================================================================

/// Flags a `Permission` that arrives after its call's `ToolResult`.
///
/// Journal order within a call is `ToolCall`, then every `Permission`, then
/// `ToolResult`: the broker emits records on a channel while the tool runs, and
/// the engine drains that channel *before* it appends the result. This is not
/// bookkeeping preference. Properties observe in journal order, so a
/// `ToolResult` arriving before its grants makes an unaudited-access check fire
/// a false violation mid-run against a system that did everything right.
///
/// Serial dispatch makes the correct order the natural implementation, but with
/// a channel in the design it is a specification rather than a happy accident,
/// which is what this checks.
#[derive(Default)]
pub struct GrantOrder {
    resulted: HashSet<CallId>,
}

// ========================================================================
// GrantOrder Implementations
// ========================================================================

impl GrantOrder {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Property for GrantOrder {
    fn name(&self) -> &'static str {
        NAME
    }

    fn observe(&mut self, record: &Record) -> Option<Violation> {
        match &record.kind {
            RecordKind::ToolResult(result) => {
                self.resulted.insert(result.call.clone());
                None
            }
            RecordKind::Permission(permission) if self.resulted.contains(&permission.call) => {
                Some(Violation::new(
                    NAME,
                    format!(
                        "permission for call `{}` was journaled after its result",
                        permission.call
                    ),
                ))
            }
            _ => None,
        }
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use journal::{Access, Decision, Outcome, Scope};

    use super::*;
    use crate::monitor::testing::{permission_record, tool_result_record};

    fn call() -> CallId {
        CallId::new("toolu_1".into())
    }

    fn grant() -> Record {
        permission_record(
            call(),
            Access::Write {
                path: "/repo/a".into(),
            },
            Decision::Allowed,
            Scope::Once,
        )
    }

    fn result() -> Record {
        tool_result_record(
            call(),
            Outcome::Ok {
                content: "done".into(),
            },
        )
    }

    #[test]
    fn grants_before_the_result_are_fine() {
        let mut property = GrantOrder::new();
        assert!(property.observe(&grant()).is_none());
        assert!(property.observe(&result()).is_none());
    }

    #[test]
    fn a_grant_after_the_result_is_flagged() {
        let mut property = GrantOrder::new();
        property.observe(&result());
        let violation = property.observe(&grant()).expect("must be flagged");
        assert_eq!(violation.property, NAME);
    }

    #[test]
    fn another_calls_result_does_not_close_this_call() {
        let mut property = GrantOrder::new();
        property.observe(&tool_result_record(
            CallId::new("toolu_other".into()),
            Outcome::Ok {
                content: "done".into(),
            },
        ));
        assert!(property.observe(&grant()).is_none());
    }
}

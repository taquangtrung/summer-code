//! The monitor: a policy and the properties checked against it.

use std::io::{BufReader, Read};

use journal::{JournalError, Record, read_records};
use policy::Policy;

use crate::monitor::properties::{GrantOrder, InwardResolution, OutsideRoot, UnauditedSuccess};
use crate::monitor::property::{Property, Violation};

// ========================================================================
// Data Structures
// ========================================================================

/// Every property, fed one record at a time.
///
/// The engine's recorder owns the only `Monitor`, which is what makes "the
/// monitor runs on every run" structural rather than something the loop
/// remembers to do. A second copy would see a subset of the records.
pub struct Monitor {
    properties: Vec<Box<dyn Property>>,
}

// ========================================================================
// Monitor Implementations
// ========================================================================

impl Monitor {
    /// The built-in property set for a policy.
    pub fn new(policy: &Policy) -> Self {
        Self {
            properties: vec![
                Box::new(GrantOrder::new()),
                Box::new(InwardResolution::new()),
                Box::new(OutsideRoot::new(policy.root.clone())),
                Box::new(UnauditedSuccess::new()),
            ],
        }
    }

    /// A monitor for a session whose broker reports to a different journal.
    ///
    /// **Empty, and that is the honest answer rather than a weaker check.**
    /// Every property here reads `Permission` records: `GrantOrder`,
    /// `InwardResolution` and `OutsideRoot` inspect them directly, and
    /// `UnauditedSuccess` flags a success that has none. A session sharing
    /// another's broker receives no such records, because a broker has one
    /// decisions channel and it is already attached elsewhere, so all four would
    /// report on every call the session made and none of the reports would mean
    /// anything.
    ///
    /// The accesses are still audited. They appear in the journal of the session
    /// that owns the broker, attributed to the call that spawned this one.
    pub fn audited_elsewhere() -> Self {
        Self::with_properties(Vec::new())
    }

    /// Build from an explicit property list. The engine uses [`Monitor::new`];
    /// this exists so a test can isolate one property.
    pub fn with_properties(properties: Vec<Box<dyn Property>>) -> Self {
        Self { properties }
    }

    /// Feed one record to every property, in journal order.
    pub fn observe(&mut self, record: &Record) -> Vec<Violation> {
        self.properties
            .iter_mut()
            .filter_map(|property| property.observe(record))
            .collect()
    }

    /// The property names this monitor checks, for a report's header.
    pub fn names(&self) -> Vec<&'static str> {
        self.properties
            .iter()
            .map(|property| property.name())
            .collect()
    }

    /// Replay a finished journal through a fresh monitor.
    ///
    /// Returns `Err` on a journal it could not read. A corrupt journal is not a
    /// clean one, and reporting "no violations" for a file that failed to parse
    /// is the worst answer available: it is indistinguishable from a passing run.
    pub fn check_journal(
        reader: impl Read,
        policy: &Policy,
    ) -> Result<Vec<Violation>, JournalError> {
        let records = read_records(BufReader::new(reader))?;
        let mut monitor = Monitor::new(policy);
        let mut violations = Vec::new();
        for record in &records {
            violations.extend(monitor.observe(record));
        }
        Ok(violations)
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::PathBuf;

    use journal::{Access, Decision, Outcome, Scope, write_records};
    use policy::{NetworkRule, Policy};

    use super::*;
    use crate::monitor::testing::{call_id, permission_record, tool_result_record};

    fn policy() -> Policy {
        Policy {
            commands: vec![],
            network: NetworkRule::default(),
            paths: vec![],
            root: PathBuf::from("/repo"),
        }
    }

    fn journal(records: &[Record]) -> Vec<u8> {
        let mut buffer = Vec::new();
        write_records(&mut buffer, records).expect("write");
        buffer
    }

    #[test]
    fn a_clean_journal_reports_nothing() {
        let call = call_id("toolu_1");
        let records = vec![
            permission_record(
                call.clone(),
                Access::Write {
                    path: "/repo/a".into(),
                },
                Decision::Allowed,
                Scope::Once,
            ),
            tool_result_record(
                call,
                Outcome::Ok {
                    content: "done".into(),
                },
            ),
        ];
        let violations = Monitor::check_journal(Cursor::new(journal(&records)), &policy())
            .expect("readable journal");
        assert!(violations.is_empty(), "unexpected: {violations:?}");
    }

    #[test]
    fn an_escape_survives_a_round_trip_through_the_file() {
        // Post-hoc and live share one implementation, so this also pins that the
        // records reconstruct well enough for the properties to see them.
        let call = call_id("toolu_1");
        let records = vec![
            permission_record(
                call.clone(),
                Access::Read {
                    path: "/etc/passwd".into(),
                },
                Decision::Allowed,
                Scope::Once,
            ),
            tool_result_record(
                call,
                Outcome::Ok {
                    content: "root:x:0".into(),
                },
            ),
        ];
        let violations = Monitor::check_journal(Cursor::new(journal(&records)), &policy())
            .expect("readable journal");
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].property, "outside-root");
    }

    #[test]
    fn a_corrupt_journal_is_an_error_not_a_pass() {
        // Swallowing the read error would report a torn file as clean, which is
        // indistinguishable from a run that violated nothing.
        let result = Monitor::check_journal(Cursor::new(b"not-json\n".to_vec()), &policy());
        assert!(result.is_err(), "a corrupt journal must not report clean");
    }

    #[test]
    fn the_built_in_set_covers_the_named_properties() {
        let monitor = Monitor::new(&policy());
        let mut names = monitor.names();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "grant-order",
                "inward-resolution",
                "outside-root",
                "unaudited-success"
            ]
        );
    }
}

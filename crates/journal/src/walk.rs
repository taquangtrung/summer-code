//! The tree traversal: a [`Walk`] is the linear path from a head back to the
//! root, with compaction drops applied.
//!
//! `Walk` is two-phase, and it has to be. `Compacted` sits at the head, so in
//! root-to-head order it is *downstream* of the records it drops: a single
//! forward pass reaches the dropped records before the declaration that drops
//! them and cannot know to skip them. Phase one collects the path head-to-root
//! (which `parent` pointers force anyway — they only run upward), gathering
//! every `Compacted.dropped` on the way; phase two reverses and filters with
//! the drop set already known.
//!
//! The drop set is the union of *every* `Compacted.dropped` on the raw path,
//! including `Compacted` records that are themselves dropped by a later
//! compaction. Skip a dropped `Compacted` naively and its drop list is never
//! read, and its range silently reappears — a bug that only fires in sessions
//! long enough to compact twice.
//!
//! Phase 0 caveat: the surviving records are yielded in physical root-to-head
//! order, `Compacted` records included. That is correct for replay (a
//! `Compacted` is not a `ModelTurn`, so `Recorded` filters it out) and for any
//! consumer that walks records. The engine's history fold, which positions each
//! summary where its dropped range was, lands in Phase 4 alongside compaction
//! itself; no Phase 0–3 journal contains a `Compacted` record.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::error::JournalError;
use crate::id::RecordId;
use crate::record::{Record, RecordKind};

// ========================================================================
// Data Structures
// ========================================================================

/// An indexed journal: records keyed by id, ready to walk from any head.
pub struct Journal {
    records: HashMap<RecordId, Arc<Record>>,
}

/// The surviving records on a root-to-head path, compaction drops applied.
pub struct Walk {
    records: Vec<Arc<Record>>,
}

// ========================================================================
// Journal Implementations
// ========================================================================

impl Journal {
    /// Index a flat list of records by id.
    ///
    /// A duplicate id is an error, not a last-one-wins merge. `RecordId` is a
    /// ULID and the engine mints one per record, so two records sharing an id
    /// cannot come from a journal this code wrote: it is a corrupt or
    /// concatenated file, and the inputs here are files, which are untrusted.
    /// Collapsing silently would drop an record from the middle of a walk and
    /// hand back a shorter, plausible-looking history, which is the worst
    /// available outcome for the artifact resume and replay both read.
    pub fn from_records(records: Vec<Record>) -> Result<Self, JournalError> {
        let mut indexed = HashMap::with_capacity(records.len());
        for record in records {
            let id = record.id;
            if indexed.insert(id, Arc::new(record)).is_some() {
                return Err(JournalError::DuplicateRecord { id });
            }
        }
        Ok(Self { records: indexed })
    }

    /// Look up a single record by id.
    pub fn get(&self, id: &RecordId) -> Option<Arc<Record>> {
        self.records.get(id).cloned()
    }

    /// Walk from `head` back to the root. Returns `None` if the head is not in
    /// this journal. Follows `parent` until it runs out (a well-formed journal
    /// reaches the root; a corrupt one stops early).
    pub fn walk(&self, head: RecordId) -> Option<Walk> {
        if !self.records.contains_key(&head) {
            return None;
        }

        // Phase 1: collect the raw path head -> root, gathering drop sets.
        let mut raw: Vec<Arc<Record>> = Vec::new();
        let mut drop_set: HashSet<RecordId> = HashSet::new();
        let mut visited: HashSet<RecordId> = HashSet::new();
        let mut cursor = Some(head);
        while let Some(id) = cursor {
            // Cycle guard: a well-formed tree never revisits, but a corrupt
            // journal must not loop forever.
            if !visited.insert(id) {
                break;
            }
            let record = match self.records.get(&id) {
                Some(record) => record.clone(),
                None => break,
            };
            if let RecordKind::Compacted(compacted) = &record.kind {
                for dropped in &compacted.dropped {
                    drop_set.insert(*dropped);
                }
            }
            cursor = record.parent;
            raw.push(record);
        }

        // Phase 2: reverse to root -> head, drop the summarized range.
        raw.reverse();
        let records = raw
            .into_iter()
            .filter(|record| !drop_set.contains(&record.id))
            .collect();
        Some(Walk { records })
    }
}

// ========================================================================
// Walk Implementations
// ========================================================================

impl Walk {
    /// The surviving records, root to head.
    pub fn records(&self) -> &[Arc<Record>] {
        &self.records
    }

    /// Whether the walk carries no records.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Number of surviving records.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Fold the surviving records left-to-right into an accumulator.
    pub fn fold<B, F>(self, init: B, mut f: F) -> B
    where
        F: FnMut(B, &Record) -> B,
    {
        let mut acc = init;
        for record in self.records {
            acc = f(acc, &record);
        }
        acc
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{RecordId, SessionId};
    use crate::record::{Compacted, ModelTurn, Record, RecordKind, StopReason, Usage};

    fn turn_record(id: RecordId, parent: Option<RecordId>, session: SessionId) -> Record {
        Record {
            at: std::time::SystemTime::UNIX_EPOCH,
            id,
            kind: RecordKind::ModelTurn(ModelTurn::new(vec![], StopReason::EndTurn, Usage::zero())),
            parent,
            session,
        }
    }

    fn journal_with(records: Vec<Record>) -> Journal {
        Journal::from_records(records).expect("unique ids")
    }

    #[test]
    fn walk_yields_root_to_head_order() {
        let session = SessionId::new();
        let e0 = RecordId::new();
        let e1 = RecordId::new();
        let e2 = RecordId::new();
        let journal = journal_with(vec![
            turn_record(e0, None, session),
            turn_record(e1, Some(e0), session),
            turn_record(e2, Some(e1), session),
        ]);
        let walk = journal.walk(e2).unwrap();
        let ids: Vec<RecordId> = walk.records().iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![e0, e1, e2]);
    }

    #[test]
    fn a_duplicate_record_id_is_refused_rather_than_collapsed() {
        // Indexing into a HashMap made this last-one-wins, which drops an record
        // from the middle of every walk through it and hands back a shorter,
        // entirely plausible-looking history. The inputs here are files.
        let session = SessionId::new();
        let id = RecordId::new();
        let result = Journal::from_records(vec![
            turn_record(id, None, session),
            turn_record(id, None, session),
        ]);
        assert!(matches!(result, Err(JournalError::DuplicateRecord { .. })));
    }

    #[test]
    fn walk_missing_head_is_none() {
        let journal = journal_with(vec![]);
        assert!(journal.walk(RecordId::new()).is_none());
    }

    #[test]
    fn single_compaction_drops_its_range() {
        // e0 (root) -> e1 -> e2 -> e3 -> e4, then c1 appended at head e4
        // dropping the middle range [e1, e2].
        let session = SessionId::new();
        let (e0, e1, e2, e3, e4) = (
            RecordId::new(),
            RecordId::new(),
            RecordId::new(),
            RecordId::new(),
            RecordId::new(),
        );
        let c1 = RecordId::new();
        let records = vec![
            turn_record(e0, None, session),
            turn_record(e1, Some(e0), session),
            turn_record(e2, Some(e1), session),
            turn_record(e3, Some(e2), session),
            turn_record(e4, Some(e3), session),
            Record {
                at: std::time::SystemTime::UNIX_EPOCH,
                id: c1,
                kind: RecordKind::Compacted(Compacted::new(
                    vec![e1, e2],
                    Usage::zero(),
                    "old turns".into(),
                )),
                parent: Some(e4),
                session,
            },
        ];
        let journal = journal_with(records);
        let walk = journal.walk(c1).unwrap();
        let ids: Vec<RecordId> = walk.records().iter().map(|e| e.id).collect();
        // e1, e2 dropped; e0, e3, e4, c1 survive in physical root->head order.
        assert_eq!(ids, vec![e0, e3, e4, c1]);
    }

    #[test]
    fn nested_compaction_keeps_the_first_drop_set() {
        // c1 drops [e1, e2]; c2 (the new head) drops [e3, c1]. A naive drop set
        // that only reads c2 would resurrect e1 and e2.
        let session = SessionId::new();
        let (e0, e1, e2, e3, e4) = (
            RecordId::new(),
            RecordId::new(),
            RecordId::new(),
            RecordId::new(),
            RecordId::new(),
        );
        let (c1, c2) = (RecordId::new(), RecordId::new());
        let records = vec![
            turn_record(e0, None, session),
            turn_record(e1, Some(e0), session),
            turn_record(e2, Some(e1), session),
            turn_record(e3, Some(e2), session),
            turn_record(e4, Some(e3), session),
            Record {
                at: std::time::SystemTime::UNIX_EPOCH,
                id: c1,
                kind: RecordKind::Compacted(Compacted::new(
                    vec![e1, e2],
                    Usage::zero(),
                    "old".into(),
                )),
                parent: Some(e4),
                session,
            },
            Record {
                at: std::time::SystemTime::UNIX_EPOCH,
                id: c2,
                kind: RecordKind::Compacted(Compacted::new(
                    vec![e3, c1],
                    Usage::zero(),
                    "older".into(),
                )),
                parent: Some(c1),
                session,
            },
        ];
        let journal = journal_with(records);
        let walk = journal.walk(c2).unwrap();
        let ids: Vec<RecordId> = walk.records().iter().map(|e| e.id).collect();
        // e1, e2 (via c1), e3 and c1 (via c2) all dropped; e0, e4, c2 survive.
        assert_eq!(ids, vec![e0, e4, c2]);
    }

    #[test]
    fn fold_counts_surviving_records() {
        let session = SessionId::new();
        let (e0, e1) = (RecordId::new(), RecordId::new());
        let journal = journal_with(vec![
            turn_record(e0, None, session),
            turn_record(e1, Some(e0), session),
        ]);
        let walk = journal.walk(e1).unwrap();
        let count = walk.fold(0usize, |n, _record| n + 1);
        assert_eq!(count, 2);
    }
}

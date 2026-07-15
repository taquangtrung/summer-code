//! Queries and their answers, written against the schema. A new query touches
//! zero extractors.
//!
//! [`Answer<T>`] makes [`Completeness`] non-optional: it is impossible to return
//! results without stating whether they are all of them, which is the property
//! that stops the agent trusting a partial call graph.

use crate::facts::ids::SymbolId;

// ========================================================================
// Data Structures
// ========================================================================

/// A structural or textual query. The agent invokes a query and never names an
/// extractor.
#[derive(Debug, Clone)]
pub enum Query {
    /// Free-text search.
    Text { globs: Vec<String>, pattern: String },
    /// Semantic search: the `k` nearest neighbors of `text`.
    Concept { k: usize, text: String },
    /// Who calls this symbol?
    Callers(SymbolId),
    /// What implements this symbol (a trait, usually)?
    Implementers(SymbolId),
    /// Trace type/value flow through this symbol.
    TypeFlow(SymbolId),
    /// The definition of a name.
    Definition(String),
}

/// Every answer is labelled. There is no way to return results without saying
/// whether they are all of them.
#[derive(Debug, Clone)]
pub struct Answer<T> {
    pub completeness: Completeness,
    pub items: Vec<T>,
    pub via: Mechanism,
}

/// How complete an answer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completeness {
    /// Every matching fact was considered.
    Complete,
    /// Some were not, and the answer says why.
    BestEffort { reason: BestEffortReason },
}

/// Why an answer is best-effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BestEffortReason {
    /// An extractor declared it could not produce a kind fully.
    ExtractorLimit,
    /// The underlying facts were stale relative to the working tree.
    Stale,
    /// Only some sources contributed.
    Partial,
    /// The query timed out.
    Timeout,
}

/// Which mechanism answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mechanism {
    Embedding,
    Facts { extractor: &'static str },
    Grep,
}

impl<T> Answer<T> {
    pub fn is_complete(&self) -> bool {
        matches!(self.completeness, Completeness::Complete)
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_best_effort_answer_says_why() {
        let answer = Answer {
            completeness: Completeness::BestEffort {
                reason: BestEffortReason::Stale,
            },
            items: vec![1u32],
            via: Mechanism::Facts { extractor: "line" },
        };
        assert!(!answer.is_complete());
    }

    #[test]
    fn a_complete_answer_says_so() {
        let answer = Answer::<u32> {
            completeness: Completeness::Complete,
            items: vec![],
            via: Mechanism::Grep,
        };
        assert!(answer.is_complete());
    }
}

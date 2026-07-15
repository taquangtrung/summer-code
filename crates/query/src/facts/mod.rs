//! The fact schema: symbols, call edges, definitions, type flow. Knows nothing
//! about how facts are produced or read.
//!
//! Two decisions here are load-bearing and cannot be deferred:
//!
//! - **`SymbolId` is content-addressed** (a hash of a canonical `SymbolKey`),
//!   not a counter from a shared interner. Two extractors that never speak
//!   produce the same id for the same symbol, which is what makes their facts
//!   joinable at all; ids are also stable across runs and processes, which
//!   incremental extraction and the daemon both need.
//! - **`Condition` is in the schema from day one.** A fact is true only under a
//!   build configuration; anything behind `#[cfg(...)]` is not unconditionally
//!   true, and an answer that ignores this claims completeness it does not have.
//!   Retrofitting `Condition` means auditing every consumer that already
//!   believed an unconditional answer.
//!
//! `ModulePath::Unresolved` is how a syntactic extractor disagrees with a
//! semantic one about the same symbol, made visible instead of silently
//! producing a wrong join: tree-sitter does not follow `use` aliases, so it
//! emits `Unresolved`, which hashes into a different id on purpose.

use serde::{Deserialize, Serialize};

use crate::facts::ids::{FileId, Location, SymbolId};

pub(crate) mod ids;
pub(crate) mod symbol;

// ========================================================================
// Data Structures
// ========================================================================

/// One fact about the code. True under a [`Condition`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Fact {
    /// `symbol` is defined at `at`.
    Defines { at: Location, symbol: SymbolId },
    /// `from` calls `to` at `at`, under `when`.
    Calls {
        at: Location,
        from: SymbolId,
        to: SymbolId,
        when: Condition,
    },
    /// `ty` implements `trait_`, under `when`.
    Implements {
        trait_: SymbolId,
        ty: SymbolId,
        when: Condition,
    },
    /// `file` imports `symbol`.
    Imports { file: FileId, symbol: SymbolId },
    /// A value of `from` flows into `to`, under `when`. (Taint / type flow,
    /// which LSP does not express and a purpose-built extractor does.)
    TypeFlow {
        from: SymbolId,
        to: SymbolId,
        when: Condition,
    },
}

/// The build configuration under which a fact is true. `Always` is the common
/// case; anything behind `#[cfg(...)]` is conditional, and an answer that
/// ignores it claims completeness it does not have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Condition {
    Always,
    Cfg(CfgExpr),
}

/// A build-configuration expression. Stored opaquely (the string as written):
/// resolving it needs build settings this crate does not have, and a query that
/// cannot resolve it reports best-effort rather than guessing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CfgExpr(pub String);

impl Condition {
    pub fn always() -> Self {
        Self::Always
    }
}

/// Which fact kinds an extractor can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FactKind {
    Calls,
    Defines,
    Implements,
    Imports,
    TypeFlow,
}

impl Fact {
    pub fn kind(&self) -> FactKind {
        match self {
            Self::Calls { .. } => FactKind::Calls,
            Self::Defines { .. } => FactKind::Defines,
            Self::Implements { .. } => FactKind::Implements,
            Self::Imports { .. } => FactKind::Imports,
            Self::TypeFlow { .. } => FactKind::TypeFlow,
        }
    }

    pub fn condition(&self) -> &Condition {
        match self {
            Self::Defines { .. } | Self::Imports { .. } => &ALWAYS,
            Self::Calls { when, .. }
            | Self::Implements { when, .. }
            | Self::TypeFlow { when, .. } => when,
        }
    }
}

static ALWAYS: Condition = Condition::Always;

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::ids::Span;

    fn loc() -> Location {
        Location::new(FileId::new(0), Span::new(0, 1))
    }

    #[test]
    fn a_defines_fact_is_always_true() {
        let fact = Fact::Defines {
            at: loc(),
            symbol: SymbolId::ZERO,
        };
        assert_eq!(fact.kind(), FactKind::Defines);
        assert_eq!(fact.condition(), &Condition::Always);
    }

    #[test]
    fn a_calls_fact_carries_its_condition() {
        let fact = Fact::Calls {
            at: loc(),
            from: SymbolId::ZERO,
            to: SymbolId::ZERO,
            when: Condition::Cfg(CfgExpr("unix".into())),
        };
        assert_eq!(fact.kind(), FactKind::Calls);
        assert_eq!(fact.condition(), &Condition::Cfg(CfgExpr("unix".into())));
    }
}

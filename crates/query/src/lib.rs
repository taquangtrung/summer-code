//! Codebase understanding: facts, queries, and extractors alongside grep.
//!
//! Three separable pieces, not one backend trait:
//!
//! - The schema ([`Fact`], [`FactKind`]): symbols, call edges, implementations,
//!   type flow. It knows nothing about how facts are produced or read.
//! - [`Extractor`] is implemented once per mechanism, not per language and not
//!   per query.
//! - [`Query`] is written against the schema. A new query touches zero
//!   extractors.
//!
//! The separation that matters is between them, not between crates: an extractor
//! must not know what queries exist, and a query must not know which extractor
//! produced a fact. A module boundary enforces that as well as a crate boundary
//! does, and without pretending to a contract nothing can consume.
//!
//! **That boundary is internal, so the modules are `pub(crate)` and the
//! re-export list below is the whole public surface.** Widening it later is a
//! compatible change and narrowing it is not, which is the only reason the
//! choice is made here rather than left to whatever happened to be `pub`.
//!
//! [`Extractor::extract`] takes a
//! [`sandbox::ToolCx`] because the LSP extractor spawns language servers,
//! which is agent-driven process execution. `ToolCx` is `sandbox`'s,
//! which lets this crate take one while depending on the sandbox and not the
//! tool layer.

pub(crate) mod ast_pattern;
pub(crate) mod extract;
pub(crate) mod facts;
pub(crate) mod graph;
pub(crate) mod grep;
pub(crate) mod query;
pub(crate) mod skeleton;

pub use ast_pattern::{AstMatch, search_ast};
pub use extract::Extractor;
pub use extract::lsp::LspExtractor;
pub use extract::store::{FactStore, FileSet};
pub use extract::treesitter::{Outline, TreeSitterExtractor, body_spans, outline};
pub use facts::ids::{FileId, FilePath, Location, ModulePath, Span, SymbolId, SymbolKey};
pub use facts::symbol::{Language, SymbolKind};
pub use facts::{CfgExpr, Condition, Fact, FactKind};
pub use graph::{CodeGraph, GraphNeighborhood};
pub use grep::{Hit, grep};
pub use query::{Answer, BestEffortReason, Completeness, Mechanism, Query};
pub use skeleton::extract_skeleton;

//! Produces facts. Knows nothing about queries.
//!
//! `Extractor` is implemented once per mechanism, not per language and not per
//! query: an LSP extractor covers any language with a server; a tree-sitter
//! extractor covers any language with a grammar. A new query touches zero
//! extractors.
//!
//! Each extractor declares which fact kinds it produces and at what fidelity,
//! and every query result carries [`Completeness`] so there is no way to return
//! results without saying whether they are all of them.

use async_trait::async_trait;
use sandbox::ToolCx;

use crate::extract::store::{FactStore, FileSet};
use crate::facts::FactKind;

pub(crate) mod lsp;
pub(crate) mod store;
pub(crate) mod treesitter;

// ========================================================================
// Traits
// ========================================================================

/// Produces facts from files. Generic across languages by mechanism: an LSP
/// extractor, a tree-sitter extractor, or a purpose-built one for what those
/// cannot express (type flow, taint).
#[async_trait]
pub trait Extractor: Send + Sync {
    fn name(&self) -> &'static str;

    /// What this mechanism can produce.
    fn kinds(&self) -> &'static [FactKind];

    /// Run extraction over `files`, appending to `store`. Reads file contents
    /// through the broker, so a repo path the policy excludes is not extractable.
    async fn extract(&self, cx: &ToolCx, files: &FileSet, store: &mut FactStore);
}

// ========================================================================
// Data Structures
// ========================================================================

//! `FileSet` and `FactStore`: where extraction's inputs and outputs live.
//!
//! `FileId` is an index into a `FileSet`, which owns the id-to-path mapping and
//! is passed to the extractor alongside the ids. Content resolution goes through
//! `cx.broker.read`, never `fs::read`: extraction is agent-driven, so it is
//! subject to the same policy as a tool call, and a repo path the policy
//! excludes is not extractable.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::facts::Fact;
use crate::facts::ids::{FileId, SymbolId, SymbolKey};

// ========================================================================
// Data Structures
// ========================================================================

/// Owns the `FileId`-to-path mapping. An id means nothing without the set that
/// minted it, which is why the two travel together.
#[derive(Debug, Clone, Default)]
pub struct FileSet {
    paths: Vec<PathBuf>,
}

/// The collected facts from one or more extractors, indexed for queries.
#[derive(Debug, Default)]
pub struct FactStore {
    facts: Vec<Fact>,
    /// The key each [`SymbolId`] hashed from.
    ///
    /// **Without this a query can join facts but cannot name what it found.** An
    /// id is a hash of a [`SymbolKey`] and is deliberately one-way: that is what
    /// makes two extractors' facts joinable without a shared interner. It also
    /// means the name is unrecoverable from the id alone, so a consumer asked
    /// for "the callers of `parse`" has ids to return and nothing to call them.
    /// The extractor that minted an id already holds its key, so it registers
    /// the pair as it goes and the table costs nothing to build.
    symbols: HashMap<SymbolId, SymbolKey>,
}

// ========================================================================
// FileSet Implementations
// ========================================================================

impl FileSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint an id for `path`, returning the id. Ids are assigned in insertion
    /// order, which is stable for a given set.
    pub fn insert(&mut self, path: impl Into<PathBuf>) -> FileId {
        let id = FileId::new(self.paths.len() as u32);
        self.paths.push(path.into());
        id
    }

    pub fn path(&self, id: FileId) -> Option<&Path> {
        self.paths.get(id.0 as usize).map(PathBuf::as_path)
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn ids(&self) -> impl Iterator<Item = FileId> + '_ {
        (0..self.paths.len() as u32).map(FileId::new)
    }
}

// ========================================================================
// FactStore Implementations
// ========================================================================

impl FactStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn extend(&mut self, facts: Vec<Fact>) {
        self.facts.extend(facts);
    }

    pub fn facts(&self) -> &[Fact] {
        &self.facts
    }

    /// Record the key an id hashed from, so a query can name what it matched.
    ///
    /// Idempotent by construction: the id *is* the hash of the key, so a second
    /// registration of the same id carries the same key.
    pub fn register(&mut self, key: SymbolKey) -> SymbolId {
        let id = key.id();
        self.symbols.insert(id, key);
        id
    }

    /// The key `id` hashed from, if an extractor registered it.
    pub fn key(&self, id: SymbolId) -> Option<&SymbolKey> {
        self.symbols.get(&id)
    }

    /// The bare name of `id`, if known.
    pub fn name(&self, id: SymbolId) -> Option<&str> {
        self.symbols.get(&id).map(|key| key.name.as_str())
    }

    pub fn len(&self) -> usize {
        self.facts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.facts.is_empty()
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_registered_symbol_is_recoverable_by_name() {
        // The whole point of the table: the id is a one-way hash, so without
        // this a query has ids to return and nothing to call them.
        use crate::facts::ids::ModulePath;
        use crate::facts::symbol::{Language, SymbolKind};

        let mut store = FactStore::new();
        let id = store.register(SymbolKey {
            kind: SymbolKind::Function,
            language: Language::Rust,
            module: ModulePath::Resolved(vec!["crate".into()]),
            name: "parse".into(),
        });
        assert_eq!(store.name(id), Some("parse"));
        assert_eq!(
            store.name(SymbolId(0)),
            None,
            "an unregistered id has no name"
        );
    }

    #[test]
    fn file_ids_are_stable_in_insertion_order() {
        let mut set = FileSet::new();
        let a = set.insert("src/a.rs");
        let b = set.insert("src/b.rs");
        assert_eq!(set.path(a).unwrap(), Path::new("src/a.rs"));
        assert_eq!(set.path(b).unwrap(), Path::new("src/b.rs"));
    }
}

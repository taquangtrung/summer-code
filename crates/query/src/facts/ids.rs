//! Identifiers and locations for facts.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::facts::symbol::{Language, SymbolKind};

// ========================================================================
// Data Structures
// ========================================================================

/// Content-addressed: a deterministic hash of a canonical [`SymbolKey`]. Not a
/// counter from a shared interner. Two extractors that never speak produce the
/// same id for the same symbol, which is what makes their facts joinable; ids
/// are stable across runs and processes, which incremental extraction and the
/// daemon both need.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SymbolId(pub u64);

impl SymbolId {
    /// A stable id for tests: the hash of the empty key.
    pub const ZERO: SymbolId = SymbolId(0);
}

/// The canonical key a [`SymbolId`] hashes from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SymbolKey {
    pub kind: SymbolKind,
    pub language: Language,
    pub module: ModulePath,
    pub name: String,
}

impl SymbolKey {
    /// Render the key to a canonical string, which is what the id hashes.
    ///
    /// **Every field of the key is in here, and two used to be missing.** `kind`
    /// was omitted, so a field and a function with the same name in the same
    /// module hashed identically -- and `Query::Callers` then joined facts about
    /// one onto the other while still labelling the answer `Complete`. The
    /// `Unresolved` variant's `file` was omitted too, so the same `as_written`
    /// path in two files collapsed into one symbol, defeating the very
    /// separation the variant exists to provide.
    pub fn canonical(&self) -> String {
        let module = match &self.module {
            ModulePath::Resolved(parts) => parts.join("::"),
            ModulePath::Unresolved { as_written, file } => {
                format!("?{}::{as_written}", file.0)
            }
        };
        format!(
            "{:?}::{:?}::{module}::{}",
            self.language, self.kind, self.name
        )
    }

    /// The content-addressed id for this key.
    pub fn id(&self) -> SymbolId {
        SymbolId(hash64(self.canonical().as_bytes()))
    }
}

/// A resolved or unresolved module path. A syntactic extractor cannot always
/// resolve `module` (tree-sitter does not follow `use` aliases); it emits
/// [`ModulePath::Unresolved`], which hashes into a different id on purpose so
/// facts keyed on an unresolved symbol join only within their own extractor.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ModulePath {
    Resolved(Vec<String>),
    Unresolved { as_written: String, file: FileId },
}

/// An index into a [`FileSet`](crate::extract::store::FileSet). Scoped to the
/// set that minted it: an id means nothing without the set it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub struct FileId(pub u32);

impl FileId {
    pub const fn new(id: u32) -> Self {
        Self(id)
    }
}

/// A byte-offset span in a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub end: u32,
    pub start: u32,
}

impl Span {
    pub const fn new(start: u32, end: u32) -> Self {
        Self { end, start }
    }
}

/// A location: which file, which span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub file: FileId,
    pub span: Span,
}

impl Location {
    pub const fn new(file: FileId, span: Span) -> Self {
        Self { file, span }
    }
}

/// The path a [`FileId`] resolves to, for rendering a hit to a human.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilePath(pub PathBuf);

// ========================================================================
// Functions
// ========================================================================

/// FNV-1a 64-bit over the bytes. Deterministic and dependency-free, which is
/// the point: the id is a property of the key, not of a hasher version or a
/// process's random seed. Good enough distribution for join keys; this is not a
/// cryptographic hash and does not need to be.
pub fn hash64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> SymbolKey {
        SymbolKey {
            kind: SymbolKind::Function,
            language: Language::Rust,
            module: ModulePath::Resolved(vec!["crate".into()]),
            name: name.into(),
        }
    }

    #[test]
    fn the_same_key_hashes_to_the_same_id() {
        assert_eq!(key("foo").id(), key("foo").id());
    }

    #[test]
    fn different_keys_hash_to_different_ids() {
        assert_ne!(key("foo").id(), key("bar").id());
    }

    #[test]
    fn unresolved_and_resolved_modules_hash_differently() {
        // A syntactic extractor's unresolved guess must not collide with the
        // semantic id, or its facts silently join the wrong symbol.
        let resolved = SymbolKey {
            kind: SymbolKind::Function,
            language: Language::Rust,
            module: ModulePath::Resolved(vec!["crate".into()]),
            name: "foo".into(),
        };
        let unresolved = SymbolKey {
            kind: SymbolKind::Function,
            language: Language::Rust,
            module: ModulePath::Unresolved {
                as_written: "crate".into(),
                file: FileId::new(0),
            },
            name: "foo".into(),
        };
        assert_ne!(resolved.id(), unresolved.id());
    }

    #[test]
    fn two_kinds_with_the_same_name_are_different_symbols() {
        // A field and a function called `len` in the same module hashed
        // identically, so `Query::Callers` joined facts about one onto the other
        // and still labelled the answer `Complete`: a confidently wrong result,
        // which is worse than an incomplete one.
        let function = key("len");
        let field = SymbolKey {
            kind: SymbolKind::Field,
            ..key("len")
        };
        assert_ne!(function.id(), field.id());
    }

    #[test]
    fn the_same_unresolved_path_in_two_files_stays_two_symbols() {
        // `Unresolved` exists so a syntactic extractor's guesses join only
        // within their own scope. Dropping `file` from the hash collapsed two
        // files' guesses into one symbol, which is the join it exists to
        // prevent.
        let in_a = SymbolKey {
            module: ModulePath::Unresolved {
                as_written: "super".into(),
                file: FileId::new(0),
            },
            ..key("foo")
        };
        let in_b = SymbolKey {
            module: ModulePath::Unresolved {
                as_written: "super".into(),
                file: FileId::new(1),
            },
            ..key("foo")
        };
        assert_ne!(in_a.id(), in_b.id());
    }

    #[test]
    fn hash64_is_deterministic_across_calls() {
        assert_eq!(hash64(b"abc"), hash64(b"abc"));
        assert_ne!(hash64(b"abc"), hash64(b"abd"));
    }
}

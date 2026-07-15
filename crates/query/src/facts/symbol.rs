//! Symbols and their kinds.

use serde::{Deserialize, Serialize};

// ========================================================================
// Data Structures
// ========================================================================

/// A symbol's kind. Coarse on purpose: a query names a kind only to scope a
/// search, and finer granularity belongs on the fact, not the symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymbolKind {
    Constant,
    Enum,
    Field,
    Function,
    Macro,
    Module,
    Struct,
    Trait,
    TypeAlias,
    Variable,
}

impl SymbolKind {
    /// The short label a symbol listing prints, e.g. `fn` or `struct`.
    ///
    /// On the kind rather than at each render site, so a listing and a diagram
    /// cannot drift into calling the same thing two names.
    pub fn label(self) -> &'static str {
        match self {
            Self::Constant => "const",
            Self::Enum => "enum",
            Self::Field => "field",
            Self::Function => "fn",
            Self::Macro => "macro",
            Self::Module => "mod",
            Self::Struct => "struct",
            Self::Trait => "trait",
            Self::TypeAlias => "type",
            Self::Variable => "var",
        }
    }
}

/// A language. A new language works through the same queries with no change to
/// the fact schema: the `Language` is part of the canonical key, so a symbol in
/// one language never collides with the same name in another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Language {
    C,
    Cpp,
    Go,
    JavaScript,
    /// A language no extractor has a grammar or a server for.
    Other,
    Python,
    Rust,
    TypeScript,
}

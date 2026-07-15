//! Identifiers for journal records, sessions, and tool calls.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

// ========================================================================
// Data Structures
// ========================================================================

/// A provider-minted tool-call identifier. We never generate these; the model
/// transport assigns them, and we correlate [`crate::ToolCall`] / [`crate::ToolResult`]
/// / [`crate::Permission`] records against the value it gave us.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CallId(String);

/// Sortable, collision-free record identifier. ULIDs need no coordination to
/// mint, so the engine can assign them without a counter or a clock lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RecordId(Ulid);

/// Sortable, collision-free session identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(Ulid);

// ========================================================================
// CallId Implementations
// ========================================================================

impl CallId {
    /// Construct a `CallId` from a provider-supplied string.
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// The raw provider string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for CallId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

// ========================================================================
// RecordId Implementations
// ========================================================================

impl RecordId {
    /// Mint a fresh, unique record id.
    pub fn new() -> Self {
        Self(Ulid::new())
    }

    /// The underlying ULID.
    pub fn as_ulid(&self) -> Ulid {
        self.0
    }
}

impl Default for RecordId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for RecordId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for RecordId {
    type Err = ulid::DecodeError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(Self(Ulid::from_str(s)?))
    }
}

// ========================================================================
// SessionId Implementations
// ========================================================================

impl SessionId {
    /// Mint a fresh, unique session id.
    pub fn new() -> Self {
        Self(Ulid::new())
    }

    /// The underlying ULID.
    pub fn as_ulid(&self) -> Ulid {
        self.0
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for SessionId {
    type Err = ulid::DecodeError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(Self(Ulid::from_str(s)?))
    }
}

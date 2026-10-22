//! Journal error type.

use std::io;

use thiserror::Error;

use crate::id::RecordId;

// ========================================================================
// Data Structures
// ========================================================================

/// Failures reading or writing a journal.
#[derive(Debug, Error)]
pub enum JournalError {
    #[error("encode error: {0}")]
    Encode(#[from] serde_json::Error),
    /// A journal line could not be parsed.
    #[error("decode error at line {line}: {source}")]
    Decode {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    /// The journal was written against a schema version this build cannot read.
    #[error(
        "schema version mismatch: session file is {found}, this build supports up to {supported}"
    )]
    Version { found: u32, supported: u32 },
    /// Two records share an id. `RecordId` is a ULID minted per record, so this is
    /// a corrupt or concatenated file rather than anything this code wrote.
    #[error("duplicate record id {id}: the session file is corrupt or concatenated")]
    DuplicateRecord { id: RecordId },
}

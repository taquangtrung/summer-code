//! Journal schema versioning.

use crate::error::JournalError;

// ========================================================================
// Constants
// ========================================================================

/// The schema version this build writes and reads. Bump on any breaking change
/// to [`crate::RecordKind`]: adding a variant or an optional field is not
/// breaking; removing or retyping one is.
pub const SCHEMA_VERSION: u32 = 0;

// ========================================================================
// Functions
// ========================================================================

/// Accept a journal whose schema version this build can read, or refuse one it
/// would misread. Refusal names the version found so the caller can surface it.
///
/// **Any version that is not this one is refused, older included.** That reads
/// as over-strict and is not: the version only moves on a *breaking* change, so
/// an older journal is by construction one this build would misread. Accepting
/// `found < SCHEMA_VERSION` would wave a v0 journal through a v1 build and fold
/// it under v1 semantics, which is the exact misread the check exists to
/// prevent. It is inert while `SCHEMA_VERSION` is 0, which is also why it would
/// go unnoticed until the bump that made it matter. A build that grows a
/// migration relaxes this deliberately, and migrates.
pub fn check_version(found: u32) -> Result<(), JournalError> {
    if found == SCHEMA_VERSION {
        Ok(())
    } else {
        Err(JournalError::Version {
            found,
            supported: SCHEMA_VERSION,
        })
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_older_version_is_refused_rather_than_folded() {
        // Inert today (`SCHEMA_VERSION` is 0, so nothing is older), and the
        // reason to assert it now: the day the version bumps for a retyped
        // field, a `found > SUPPORTED` check would silently fold every v0
        // journal under v1 semantics, and no existing test would notice.
        assert!(check_version(SCHEMA_VERSION + 1).is_err());
        assert!(check_version(SCHEMA_VERSION).is_ok());
    }
}

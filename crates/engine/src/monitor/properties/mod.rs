//! One file per property.

use journal::Decision;

mod grant_order;
mod inward_resolution;
mod outside_root;
mod unaudited_success;

pub use grant_order::GrantOrder;
pub use inward_resolution::InwardResolution;
pub use outside_root::OutsideRoot;
pub use unaudited_success::UnauditedSuccess;

// ========================================================================
// Functions
// ========================================================================

/// Whether a decision let the access through, however it was reached.
///
/// Shared by every property that only speaks to allows: a denied access is the
/// system working, and each one would otherwise carry its own copy of this
/// match and its own chance of missing a variant.
pub(super) fn allowed(decision: Decision) -> bool {
    match decision {
        Decision::Allowed
        | Decision::AskedAllowed
        | Decision::ConfinedAllow
        | Decision::RememberedAllow => true,
        Decision::AskedDenied | Decision::Denied | Decision::RememberedDeny => false,
    }
}

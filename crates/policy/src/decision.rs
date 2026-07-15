//! A policy decision.

// ========================================================================
// Data Structures
// ========================================================================

/// What the evaluator answered.
///
/// `Ask` is the escape hatch for everything the default gets wrong, and the
/// broker's `Prompter` resolves it, not the evaluator: an evaluator that could
/// prompt would not be a pure function of policy and request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
    Deny { reason: String },
}

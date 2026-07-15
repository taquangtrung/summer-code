//! Shared TUI test fixtures.
//!
//! `state_with` is here rather than in one module's `mod tests` because four of
//! them build the same state to assert against: the pickers, the key prompt,
//! mentions, and the usage dashboard. A fixture copied four ways drifts four
//! ways.

use journal::Usage;

use crate::mode::ExecutionMode;
use crate::tui::state::{Status, TuiState};

// ========================================================================
// Functions
// ========================================================================

pub(super) fn state_with(model_tokens: u64, usage: &[Usage]) -> TuiState {
    let mut state = TuiState::new(Status {
        model: "mock".into(),
        effort: None,
        execution_mode: ExecutionMode::Interactive,
        total_tokens: model_tokens,
        context_tokens: 0,
        context_window: None,
        backend: "none".into(),
        cwd: "/repo".into(),
    });
    state.usage = usage.to_vec();
    state
}

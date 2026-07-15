//! The slash commands that act on the workspace or the window rather than the
//! conversation.
//!
//! **`/rewind` and `/undo` are different mechanisms and the difference matters.**
//! Rewind restores from the broker's content-addressed snapshot store, which the
//! broker wrote before every authorized write, so it works in a tree that was
//! never committed and never even `git init`ed. Undo reverses a git patch, so it
//! needs a tracked, committed tree. Neither is a superset of the other.

use crate::tui::DriverCx;
use crate::tui::state::TuiState;

// ========================================================================
// Functions
// ========================================================================

/// `/diff`: Render uncommitted workspace changes using `git diff`.
pub(super) fn show_workspace_diff(cx: &mut DriverCx<'_>) {
    let output = std::process::Command::new("git")
        .args(["diff", "--stat"])
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            if text.trim().is_empty() {
                cx.state
                    .notify("workspace clean (no uncommitted changes)".to_string());
            } else {
                cx.state.notify(format!("Workspace Diff:\n{}", text.trim()));
            }
        }
        _ => {
            cx.state
                .notify_error("could not run git diff in workspace".to_string());
        }
    }
}

/// `/checkpoint`: Save a workspace checkpoint into `./.agents/tmp/checkpoints`.
pub(super) fn create_workspace_checkpoint(cx: &mut DriverCx<'_>) {
    let cp_dir = std::path::Path::new("./.agents/tmp/checkpoints");
    if let Err(e) = std::fs::create_dir_all(cp_dir) {
        cx.state
            .notify_error(format!("could not create checkpoint dir: {e}"));
        return;
    }
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cp_file = cp_dir.join(format!("cp_{timestamp}.patch"));
    let output = std::process::Command::new("git")
        .args(["diff", "HEAD"])
        .output();
    if let Ok(out) = output
        && out.status.success()
    {
        let patch = String::from_utf8_lossy(&out.stdout).to_string();
        if patch.is_empty() {
            cx.state
                .notify("checkpoint skipped: workspace has no modified tracked files".to_string());
            return;
        }
        if std::fs::write(&cp_file, patch).is_ok() {
            cx.state
                .notify(format!("created workspace checkpoint: cp_{timestamp}"));
            return;
        }
    }
    cx.state
        .notify_error("failed to create workspace checkpoint".to_string());
}

/// `/undo`: Revert workspace files to the latest checkpoint.
pub(super) fn restore_workspace_checkpoint(cx: &mut DriverCx<'_>) {
    let cp_dir = std::path::Path::new("./.agents/tmp/checkpoints");
    let Ok(entries) = std::fs::read_dir(cp_dir) else {
        cx.state.notify("no checkpoints found to undo".to_string());
        return;
    };
    let mut patches: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("patch"))
        .map(|e| e.path())
        .collect();
    patches.sort();
    let Some(latest) = patches.pop() else {
        cx.state.notify("no checkpoints found to undo".to_string());
        return;
    };

    let status = std::process::Command::new("git")
        .args(["apply", "--reverse"])
        .arg(&latest)
        .status();

    if let Ok(s) = status
        && s.success()
    {
        cx.state.notify(format!(
            "restored workspace to checkpoint {}",
            latest.file_name().unwrap_or_default().to_string_lossy()
        ));
        return;
    }
    cx.state
        .notify_error("failed to apply checkpoint patch".to_string());
}

/// `/rewind [n]`: undo the agent's file changes from the last `n` turns.
///
/// **Distinct from `/undo`, which reverses a git patch.** That works only on a
/// tracked, committed tree and reverses whatever the diff happened to contain,
/// including the user's own edits. This restores exactly the files the broker
/// saw the agent write, from the bytes they held beforehand, and it works on an
/// untracked tree.
pub(super) fn rewind_workspace(cx: &mut DriverCx<'_>, turns: usize) {
    if cx.state.turn_starts.is_empty() {
        cx.state
            .notify("nothing to rewind: no turns have run".to_string());
        return;
    }
    let total = cx.state.turn_starts.len();
    let turns = turns.clamp(1, total);
    // `turns` back from the end: rewinding one turn restores to the start of
    // the most recent one.
    let at = cx.state.turn_starts[total - turns];

    let restored = cx.snapshots.restore_before(at);
    let changed = restored.written.len() + restored.deleted.len();
    let mut message = if changed == 0 && restored.failed.is_empty() {
        format!("rewound {turns} turn(s): no files had been changed")
    } else {
        format!(
            "rewound {turns} turn(s): {} file(s) restored, {} removed",
            restored.written.len(),
            restored.deleted.len()
        )
    };
    if !restored.failed.is_empty() {
        // Named, not counted: a file the rewind could not cover still holds the
        // agent's version, and the user has to know which one.
        let names: Vec<String> = restored
            .failed
            .iter()
            .map(|(path, reason)| format!("{} ({reason})", path.display()))
            .collect();
        message.push_str(&format!("; could not restore {}", names.join(", ")));
    }
    // The transcript keeps its history: the conversation happened, and hiding it
    // would leave the user unable to see what they rewound away from.
    cx.state.turn_starts.truncate(total - turns);
    cx.state.notify(message);
}

/// `/compact`: Compact transcript items to save token memory.
/// **This compacts the conversation, not the screen.** It used to drain the
/// transcript, which is the rendered view, and report that it had "compacted N
/// history items into turn summary context". The engine's history was untouched,
/// so the next turn sent exactly what it would have sent before and the token
/// saving the command promised was zero. The transcript is deliberately left
/// alone now: what the user has already read is theirs to scroll back through,
/// and it costs no tokens.
/// Async because the engine may ask the model to summarize what it drops (see
/// `EngineBuilder::summarize_compactions`), which is one provider round trip.
/// The command already runs on the driver's async task, so the await costs the
/// UI nothing but the time the summary takes.
pub(super) async fn compact_history(cx: &mut DriverCx<'_>) {
    let Some(engine) = cx.engine_slot.as_mut() else {
        cx.state
            .notify_error("cannot compact while a turn is running".to_string());
        return;
    };
    match engine.compact_now().await {
        // Zero is the honest answer when the working window is all there is,
        // and it has to read differently from a compaction that ran.
        Ok(0) => cx
            .state
            .notify("nothing to compact: the history is already the working window".to_string()),
        Ok(dropped) => cx.state.notify(format!(
            "compacted {dropped} entries out of the context window; they are summarized in one \
             line and stay in the journal"
        )),
        Err(error) => cx.state.notify_error(format!("could not compact: {error}")),
    }
}

/// `/usage`: Render the token usage statistics dashboard.
pub(super) fn show_usage_dashboard(state: &mut TuiState) {
    let (mut in_tok, mut out_tok, mut cache_read, mut cache_write) = (0u64, 0u64, 0u64, 0u64);
    for u in &state.usage {
        in_tok += u.input as u64;
        out_tok += u.output as u64;
        cache_read += u.cache_read as u64;
        cache_write += u.cache_write as u64;
    }
    let total_tok = state.status.total_tokens;
    let model = &state.status.model;
    let turns = state.usage.len();

    // Named separately rather than folded into the lines above, because the
    // difference is what a reader needs: the totals include it, and the turn
    // count does not, since a delegation is not a turn this session took.
    let delegated: u64 = state
        .delegated
        .iter()
        .map(|usage| u64::from(usage.input) + u64::from(usage.output))
        .sum();
    let delegations = state.delegated.len();

    state.notify(format!(
        "Usage Dashboard ({model}):\n\
         • Turns: {turns}\n\
         • Input Tokens: {in_tok}\n\
         • Output Tokens: {out_tok}\n\
         • Cache Read: {cache_read}\n\
         • Cache Write: {cache_write}\n\
         • Delegated: {delegated} tokens over {delegations} subagent runs\n\
         • Total Tokens: {total_tok}"
    ));
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use journal::Usage;

    use crate::tui::testing::state_with;

    #[test]
    fn show_usage_dashboard_notifies_statistics() {
        let mut state = state_with(150, &[Usage::new(100, 200, 1_000, 2_000)]);
        super::show_usage_dashboard(&mut state);
        assert!(state.pending_messages[0].text.contains("Usage Dashboard"));
        assert!(state.pending_messages[0].text.contains("Input Tokens: 100"));
        assert!(
            state.pending_messages[0]
                .text
                .contains("Output Tokens: 200")
        );
        assert!(state.pending_messages[0].text.contains("Cache Read: 1000"));
        assert!(state.pending_messages[0].text.contains("Cache Write: 2000"));
        assert!(state.pending_messages[0].text.contains("Total Tokens: 150"));
    }
}

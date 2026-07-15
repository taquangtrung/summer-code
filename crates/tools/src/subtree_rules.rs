//! The rules that apply to the part of the tree being changed.
//!
//! A monorepo whose `services/api/AGENTS.md` says "no unwraps here" means it for
//! edits under `services/api`, and a prompt that carried every such file for
//! every directory would be mostly irrelevant on every turn. So these are found
//! per edit, walking from the changed file up to the policy root, and appended
//! to that edit's result.
//!
//! **Read through the broker**, unlike the root-level rules the frontend loads
//! at startup. These live inside the repository and are found while the agent is
//! running, which makes them agent-driven I/O subject to the same policy as any
//! other read.
//!
//! The root's own rules file is skipped: the frontend already put it in the
//! system prompt, and repeating it on every edit would spend context restating
//! what the model was told at the start.

use std::path::Path;

use sandbox::ToolCx;

// ========================================================================
// Constants
// ========================================================================

/// The filenames a directory may state its rules in.
const RULE_FILES: &[&str] = &[".rules", ".rules.md", "AGENTS.md", "CLAUDE.md"];

/// How far up the tree to walk. A guard against a pathological path, not a
/// meaningful limit: real trees are far shallower.
const MAX_DEPTH: usize = 32;

// ========================================================================
// Functions
// ========================================================================

/// The rules governing `edited`, nearest directory first.
///
/// `None` when no directory between `edited` and the root states any, which is
/// the common case and costs the model nothing.
pub async fn for_path(cx: &ToolCx, edited: &Path) -> Option<String> {
    let root = cx.broker.policy().root.clone();
    let mut current = edited.parent()?.to_path_buf();
    let mut found = Vec::new();

    for _ in 0..MAX_DEPTH {
        if !current.starts_with(&root) {
            break;
        }
        // The root's own rules are already in the system prompt.
        if current != root {
            for name in RULE_FILES {
                let Ok(handle) = cx.broker.read(&current.join(name)).await else {
                    continue;
                };
                let Ok(text) = handle.read_to_string() else {
                    continue;
                };
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                let shown = current.strip_prefix(&root).unwrap_or(&current);
                found.push(format!(
                    "<subtree_rules path=\"{}\">\n{text}\n</subtree_rules>",
                    shown.display()
                ));
            }
        }
        if current == root || !current.pop() {
            break;
        }
    }

    (!found.is_empty()).then(|| found.join("\n\n"))
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
// Test-fixture fs (staging a tree), not agent-driven work.
#[allow(clippy::disallowed_methods)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use sandbox::Broker;
    use tokio_util::sync::CancellationToken;

    use super::*;

    fn cx_at(root: &Path) -> ToolCx {
        ToolCx::new(
            Arc::new(Broker::permissive(root.to_path_buf())),
            Arc::from(root),
            CancellationToken::new(),
            Instant::now(),
        )
    }

    #[tokio::test]
    async fn a_rule_in_a_parent_directory_applies_to_an_edit_below_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("services/api/src")).unwrap();
        std::fs::write(root.join("services/api/AGENTS.md"), "No unwraps here.").unwrap();

        let found = for_path(&cx_at(root), &root.join("services/api/src/lib.rs"))
            .await
            .expect("the parent rule applies");
        assert!(found.contains("No unwraps here."), "{found}");
        assert!(found.contains("services/api"), "the path is named: {found}");
    }

    #[tokio::test]
    async fn the_roots_own_rules_are_not_repeated() {
        // They are already in the system prompt. Repeating them on every edit
        // spends context restating what the model was told at the start.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "Root rule.").unwrap();

        assert!(
            for_path(&cx_at(root), &root.join("src/lib.rs"))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_tree_with_no_subtree_rules_costs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        assert!(
            for_path(&cx_at(dir.path()), &dir.path().join("src/lib.rs"))
                .await
                .is_none()
        );
    }
}

//! The gate that makes a skill mandatory rather than advisory.
//!
//! **A skills catalog in the system prompt is a suggestion, and a suggestion is
//! not a mechanism.** The frontend lists every skill it loaded and tells the
//! model to call the `skill` tool before doing the matching work; whether it
//! does is then the model's choice, made again on every edit, and a long session
//! drifts away from it with nothing to notice. The compilation gate already
//! showed the shape that works: state a condition the loop checks itself, and
//! refuse the turn until it holds.
//!
//! So this is the same shape one step earlier. A rule says *edits to paths
//! matching this glob require this skill*, the gate remembers which skills the
//! session actually loaded, and an edit that skipped one is denied before it
//! runs, with a reason naming the skill to call. The model can satisfy the gate
//! in one tool call and retry; it cannot ignore it.
//!
//! **Session-scoped, and deliberately reset by resume.** A skill loaded in an
//! earlier turn counts for a later one, because its body is in the window. A
//! resumed session starts with an empty set even though the journal shows the
//! call, because what matters is whether the instructions are in *this*
//! process's context, and after a fold plus compaction they may not be. The cost
//! of being wrong that way is one extra tool call; the cost of the other way is
//! an edit made under rules the model can no longer see.
//!
//! The gate knows nothing about tool names. The engine decides what an edit is
//! and which argument holds its path, and hands over the two strings, so the
//! rules here stay testable without a tool call to wrap them in.

use std::collections::BTreeSet;

use globset::{Glob, GlobMatcher};

// ========================================================================
// Data Structures
// ========================================================================

/// Which skills an edit requires, and which the session has loaded.
///
/// Empty by default: no rules configured is the loop exactly as it was, and
/// [`screen_edit`](Self::screen_edit) then answers `None` for every path without
/// consulting anything.
#[derive(Debug, Default)]
pub struct SkillGate {
    /// The skills loaded so far, by name.
    loaded: BTreeSet<String>,
    /// The configured requirements, in configuration order.
    rules: Vec<SkillRule>,
}

/// One requirement: editing a path matching `paths` needs `skill` first.
#[derive(Debug)]
struct SkillRule {
    paths: GlobMatcher,
    skill: String,
}

// ========================================================================
// SkillGate Implementations
// ========================================================================

impl SkillGate {
    /// Require `skill` before any edit to a path matching `paths`.
    ///
    /// An unparseable glob is rejected here rather than silently matching
    /// nothing, for the reason [`HookSet::add`][crate::HookSet::add] gives: a
    /// rule that never fires looks exactly like a rule that is satisfied.
    pub fn require(&mut self, paths: &str, skill: String) -> Result<(), globset::Error> {
        let paths = Glob::new(paths)?.compile_matcher();
        self.rules.push(SkillRule { paths, skill });
        Ok(())
    }

    /// Note that the session has loaded `skill`, satisfying every rule naming it.
    ///
    /// Called for a *successful* load only. An unknown skill name answers with
    /// an error and no instructions, and counting it would let a typo unlock the
    /// gate it exists to hold shut.
    pub fn record_loaded(&mut self, skill: &str) {
        self.loaded.insert(skill.to_string());
    }

    /// Why an edit to `path` must not run yet, or `None` if it may.
    ///
    /// The first unsatisfied rule wins. Naming one skill rather than listing
    /// every rule the path matched keeps the next move unambiguous; a second
    /// unsatisfied rule surfaces on the retry, which is one more round trip and
    /// no wrong edits.
    pub fn screen_edit(&self, path: &str) -> Option<String> {
        let rule = self
            .rules
            .iter()
            .find(|rule| rule.paths.is_match(path) && !self.loaded.contains(&rule.skill))?;
        Some(format!(
            "blocked: editing `{path}` requires the `{}` skill, which this session has not \
             loaded. Call the `skill` tool with name `{}`, apply what it says, then retry \
             this edit.",
            rule.skill, rule.skill
        ))
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn gate_requiring(paths: &str, skill: &str) -> SkillGate {
        let mut gate = SkillGate::default();
        gate.require(paths, skill.to_string())
            .expect("the pattern is a valid glob");
        gate
    }

    #[test]
    fn test_an_edit_that_skipped_its_skill_is_refused() {
        let gate = gate_requiring("**/*.rs", "rust-orchestrator");
        let reason = gate
            .screen_edit("crates/engine/src/engine.rs")
            .expect("the rule covers this path");
        assert!(
            reason.contains("rust-orchestrator"),
            "the reason names the skill to call: {reason}"
        );
    }

    #[test]
    fn test_loading_the_skill_lets_the_edit_through() {
        let mut gate = gate_requiring("**/*.rs", "rust-orchestrator");
        gate.record_loaded("rust-orchestrator");
        assert!(gate.screen_edit("crates/engine/src/engine.rs").is_none());
    }

    #[test]
    fn test_another_skill_does_not_satisfy_the_rule() {
        // The gate is about *which* instructions were loaded. A session that
        // read some unrelated skill has not read this one.
        let mut gate = gate_requiring("**/*.rs", "rust-orchestrator");
        gate.record_loaded("python-orchestrator");
        assert!(gate.screen_edit("crates/engine/src/engine.rs").is_some());
    }

    #[test]
    fn test_a_path_no_rule_covers_is_never_blocked() {
        let gate = gate_requiring("**/*.rs", "rust-orchestrator");
        assert!(gate.screen_edit("docs/ROADMAP.md").is_none());
    }

    #[test]
    fn test_an_unparseable_pattern_is_rejected_rather_than_kept() {
        let mut gate = SkillGate::default();
        assert!(gate.require("[", "rust-orchestrator".to_string()).is_err());
        assert!(
            gate.screen_edit("crates/engine/src/engine.rs").is_none(),
            "a rejected rule leaves nothing behind that could block an edit"
        );
    }
}

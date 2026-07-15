//! A scored run, checked in as `baselines/<task-set>.json`.
//!
//! **A baseline carries what it measured, not only what it measured it at.** The
//! rate is the smaller half of the file. A score is a claim about one agent, one
//! prompt, one tool set and one model against one task set, and every one of
//! those moves independently: a description reworded, a model swapped, a task
//! added. Compared across any of them, a delta is two different questions
//! sharing an axis. So [`Baseline::drift`] names what changed and the report
//! refuses to draw a line, rather than reporting "+8 points" from a run that
//! answered a different question.
//!
//! Checked in for the reason the plan gives: a baseline on someone's laptop is
//! not a baseline, and committing it makes "which change moved this?" a `git
//! log` rather than a memory.

use serde::{Deserialize, Serialize};

use crate::quality::Metrics;
use crate::score::Score;

// ========================================================================
// Data Structures
// ========================================================================

/// One scored run of one task set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Baseline {
    /// The commit the agent was built from.
    pub commit: String,
    pub model: String,
    pub prompt_digest: String,
    pub task_set: String,
    pub tasks: Vec<TaskScore>,
    pub tools_digest: String,
}

/// One task's successes out of its runs, and what those runs cost.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskScore {
    /// The per-field medians of the runs that finished.
    ///
    /// Optional so every baseline recorded before quality was measured still
    /// parses. Absent means "this baseline never measured it", which the report
    /// says rather than showing a delta against a zero nobody recorded.
    #[serde(default)]
    pub metrics: Option<Metrics>,
    pub n: u32,
    pub name: String,
    pub successes: u32,
}

/// One dimension along which two runs are not measuring the same thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drift {
    pub baseline: String,
    pub current: String,
    pub dimension: String,
}

// ========================================================================
// Baseline Implementations
// ========================================================================

impl Baseline {
    pub fn load(source: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(source)
    }

    /// Render for checking in: pretty, so a baseline diffs line by line.
    pub fn to_json(&self) -> String {
        // The value came from this struct, so the only way to fail is a
        // non-string map key or a non-finite float, and there are neither.
        serde_json::to_string_pretty(self).expect("a Baseline serializes")
    }

    /// One task's recorded medians, or `None` when the baseline never ran that
    /// task or ran it before quality was measured.
    pub fn metrics(&self, task: &str) -> Option<Metrics> {
        self.tasks
            .iter()
            .find(|entry| entry.name == task)
            .and_then(|entry| entry.metrics)
    }

    /// One task's score, or `None` when the baseline never ran that task.
    ///
    /// `None` rather than a zero score, because "this task did not exist when
    /// the baseline was taken" and "the agent failed it every time" are opposite
    /// readings and a zero cannot tell them apart.
    pub fn score(&self, task: &str) -> Option<Score> {
        self.tasks
            .iter()
            .find(|entry| entry.name == task)
            .map(|entry| Score {
                n: entry.n,
                successes: entry.successes,
            })
    }

    /// What changed between `self` (the baseline) and `current`.
    ///
    /// Empty means the two runs asked the same question, and only then does a
    /// delta between them mean anything. The commit is deliberately *not* a
    /// dimension: a code change is the thing a bench exists to measure, so
    /// treating it as drift would make every useful comparison incomparable.
    pub fn drift(&self, current: &Baseline) -> Vec<Drift> {
        let dimensions = [
            ("model", &self.model, &current.model),
            ("prompt_digest", &self.prompt_digest, &current.prompt_digest),
            ("task_set", &self.task_set, &current.task_set),
            ("tools_digest", &self.tools_digest, &current.tools_digest),
        ];
        dimensions
            .into_iter()
            .filter(|(_, baseline, current)| baseline != current)
            .map(|(dimension, baseline, current)| Drift {
                baseline: baseline.clone(),
                current: current.clone(),
                dimension: dimension.to_string(),
            })
            .collect()
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline() -> Baseline {
        Baseline {
            commit: "abc1234".into(),
            model: "claude-opus-4-8".into(),
            prompt_digest: "prompt-a".into(),
            task_set: "starter".into(),
            tasks: vec![TaskScore {
                metrics: None,
                n: 5,
                name: "greet".into(),
                successes: 4,
            }],
            tools_digest: "tools-a".into(),
        }
    }

    #[test]
    fn a_reworded_tool_description_makes_two_runs_incomparable() {
        // The failure this exists to prevent: a description change moves the
        // rate, and a report that only knows the rate attributes the move to
        // whatever code change happened to land alongside it.
        let mut current = baseline();
        current.tools_digest = "tools-b".into();

        let drift = baseline().drift(&current);
        assert_eq!(drift.len(), 1, "{drift:?}");
        assert_eq!(drift[0].dimension, "tools_digest");
    }

    #[test]
    fn a_new_commit_alone_is_not_drift() {
        // A code change is the thing a bench measures. Counting it as drift
        // would make the one comparison worth running the one it refuses.
        let mut current = baseline();
        current.commit = "def5678".into();
        assert!(baseline().drift(&current).is_empty());
    }

    #[test]
    fn a_task_the_baseline_never_ran_has_no_score() {
        // Not a zero: "absent from the baseline" and "failed every run" are
        // opposite readings, and a zero score reports the second for the first.
        assert!(baseline().score("a-task-added-later").is_none());
        assert_eq!(
            baseline().score("greet").map(|score| score.successes),
            Some(4)
        );
    }
}

//! The task set. A task is a repo, a prompt, and a machine-checkable success
//! criterion.
//!
//! Sets are checked in as `tasks/<name>.json` rather than built in code, for the
//! reason the plan gives baselines: a task set stored on someone's laptop is not
//! a task set. A checked-in file makes "which tasks produced this rate" a `git
//! log` question, and lets a score name the set it came from.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ========================================================================
// Data Structures
// ========================================================================

/// A named group of tasks, as read from `tasks/<name>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSet {
    /// Programs these tasks need beyond the built-in policy's allow list.
    ///
    /// **Declared, not inferred, and the reason is what a bench measures.** A
    /// run has no user to answer a permission prompt, so an undeclared program
    /// is denied, and a denial reads as the agent failing the task. Naming the
    /// programs here puts "this task set needs a shell" in the diff, where it
    /// can be argued with, rather than in a rate nobody can decompose.
    #[serde(default)]
    pub commands: Vec<String>,
    pub name: String,
    pub tasks: Vec<Task>,
}

/// The group a task without one falls into.
///
/// Not an error, because a set predating groups is still a valid set; and not
/// silent, because the matrix prints the label, so an unlabelled task is visible
/// as one rather than pooled into whichever group happened to be first.
pub const UNGROUPED: &str = "ungrouped";

fn ungrouped() -> String {
    UNGROUPED.to_string()
}

/// One scoring task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// The command whose output lines count as this task's lint warnings.
    ///
    /// **Declared per task, never inferred**, for the reason [`TaskSet::commands`]
    /// is declared: a bench that guessed the linter from the tree would report a
    /// number about its own guess. A task that names none is not linted, and the
    /// report says so rather than showing a clean zero.
    /// Which mechanism this task is meant to exercise, pooled in the ablation
    /// matrix.
    ///
    /// **Grouping is what makes the matrix answerable.** A mechanism is
    /// ablated over the whole set, but it can only change the outcome of tasks
    /// that reach it, so its effect arrives diluted: switch off the skill gate
    /// across twenty-four tasks and the four that configure a skill move the
    /// total by at most four runs, which no interval will separate from noise.
    /// Pooling the runs of one group gives the comparison the trials it needs
    /// on the tasks where the mechanism was ever in play.
    #[serde(default = "ungrouped")]
    pub group: String,
    #[serde(default)]
    pub lint: Option<String>,
    pub name: String,
    pub prompt: String,
    /// Relative to the task-set file in the checked-in form, absolute once
    /// [`TaskSet::load`] has resolved it.
    pub repo: PathBuf,
    /// Skills an edit to a matching path must have loaded first, as the user's
    /// own `config.json` would declare them. The skill bodies live in the
    /// fixture, under `.summer/skills/<name>/SKILL.md`.
    ///
    /// **A task that declares none runs with an empty gate**, which is the
    /// honest baseline: the mechanism exists to make *configured* rules stick,
    /// and a set that imposed rules on every task would measure the gate on
    /// tasks that asked for nothing.
    #[serde(default)]
    pub skills: Vec<SkillRequirement>,
    pub success: Success,
}

/// One requirement: editing a path matching `paths` needs `skill` loaded first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRequirement {
    pub paths: String,
    pub skill: String,
}

/// How a finished run is judged.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Success {
    /// Every check must hold: the shape a multi-step task needs, where any one
    /// step skipped is a failure the single-check criteria cannot express.
    All { checks: Vec<Success> },
    /// A shell command exits zero.
    CommandExitsZero { command: String },
    /// A file exists with the expected contents.
    FileMatches { expected: String, path: String },
    /// A file exists and contains `contains` somewhere in it.
    ///
    /// For prose a model words its own way (a changelog entry, a doc note):
    /// `file_matches` would fail every honest wording but one, which scores a
    /// correct run as a failure, while a command-line grep ties the check to one
    /// shell's syntax.
    FileContains { contains: String, path: String },
}

// ========================================================================
// TaskSet Implementations
// ========================================================================

impl TaskSet {
    /// Parse a task set, resolving each task's `repo` against `base`: the
    /// directory the file was read from.
    ///
    /// Relative in the file, absolute by the time the runner sees it. A set
    /// naming absolute paths would run on exactly one machine, and the runner
    /// needs a real path because it copies the tree before every run.
    pub fn load(source: &str, base: &Path) -> Result<Self, serde_json::Error> {
        let mut set: Self = serde_json::from_str(source)?;
        for task in &mut set.tasks {
            task.repo = base.join(&task.repo);
        }
        Ok(set)
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const SET: &str = r#"{
        "name": "starter",
        "commands": ["sh"],
        "tasks": [{
            "name": "greet",
            "repo": "fixtures/greet",
            "prompt": "make it greet by name",
            "success": { "kind": "command_exits_zero", "command": "cargo test" }
        }]
    }"#;

    #[test]
    fn a_relative_repo_resolves_against_the_set_file() {
        // The runner copies `repo` before every run. Left relative, it would
        // resolve against whatever directory the bench was launched from, so
        // one checked-in set would copy a different tree (or nothing at all)
        // depending on the caller's cwd.
        let set = TaskSet::load(SET, Path::new("/bench/tasks")).expect("parses");
        assert_eq!(
            set.tasks[0].repo,
            PathBuf::from("/bench/tasks/fixtures/greet")
        );
    }

    #[test]
    fn the_checked_in_starter_set_parses() {
        // The file the bench actually runs. A typo in it surfaces at the start
        // of a paid run against a live model, after the credential check and
        // before a single score.
        let set = TaskSet::load(include_str!("../tasks/starter.json"), Path::new("tasks"))
            .expect("the shipped task set parses");
        assert!(!set.tasks.is_empty());
    }

    #[test]
    fn every_shipped_task_names_a_group_and_a_distinct_name() {
        // A duplicate name makes `ArmReport::report` return the first match for
        // both, so one task's cells render twice and the other's never, and the
        // totals stay right while the blocks lie. An `ungrouped` task is a task
        // whose group row nobody will read.
        let set = TaskSet::load(include_str!("../tasks/starter.json"), Path::new("tasks"))
            .expect("the shipped task set parses");
        let mut names: Vec<&str> = set.tasks.iter().map(|task| task.name.as_str()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "two tasks share a name");
        for task in &set.tasks {
            assert_ne!(task.group, UNGROUPED, "{} names no group", task.name);
        }
    }

    #[test]
    fn every_shipped_task_points_at_a_fixture_that_exists() {
        // A task naming a directory that is not there fails every run, and the
        // failure reads as the agent being unable to do the work rather than as
        // a set nobody can run. The check is cheap and the alternative is
        // discovering it partway through a paid matrix.
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("tasks");
        let set = TaskSet::load(include_str!("../tasks/starter.json"), &base)
            .expect("the shipped task set parses");
        for task in &set.tasks {
            assert!(
                task.repo.is_dir(),
                "{}: fixture {} is missing",
                task.name,
                task.repo.display()
            );
        }
    }

    #[test]
    fn a_set_that_declares_no_commands_parses() {
        // `commands` is the escape hatch for tasks needing more than the
        // built-in allow list, and most sets need none. Without the default, a
        // set omitting it fails to parse, and the failure reads as a malformed
        // file rather than a missing optional field.
        let source = SET.replace(r#""commands": ["sh"],"#, "");
        let set = TaskSet::load(&source, Path::new("/bench/tasks")).expect("parses");
        assert!(set.commands.is_empty());
    }
}

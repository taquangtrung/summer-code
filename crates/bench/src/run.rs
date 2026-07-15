//! The runner: a task, run N times, into a [`Score`].
//!
//! **Every run gets a fresh engine and a fresh copy of the repo, and that is the
//! whole point of the file.** The runner used to call `turn` N times on *one*
//! engine against *one* repo with no reset, which makes the runs anything but
//! independent: run 2 continued run 1's conversation, and it was checked against
//! a working tree run 1 had already fixed. So it passed trivially. A reported
//! "5 of 5" was one trial and four confirmations of it, and the Wilson interval
//! computed from that sample is arithmetically correct and means nothing --
//! independence is exactly the assumption it rests on.
//!
//! The second half is the same honesty applied to failures. Collapsing "the
//! agent got it wrong", "the agent ran out of budget" and "the provider was
//! down" into one `successes` counter makes a rate that moves for reasons the
//! number cannot express. [`Attempt`] keeps them apart.
//!
//! `bench` checks task outcomes by reading files and running commands —
//! that is the operator's harness check, not the model's tool call, so the crate
//! carries the `disallowed_methods` exemption.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use engine::{Engine, Stop, UiEvent};
use tokio::task::JoinHandle;
use tools::ShellSpec;

use crate::quality::{self, Metrics};
use crate::score::Score;
use crate::task::{Success, Task};

// ========================================================================
// Traits
// ========================================================================

/// Builds a fresh [`Engine`] rooted at `repo`.
///
/// A trait rather than a closure because the runner needs one per run and the
/// caller owns every seam an engine has (provider, broker, journal sink). The
/// live model is still the caller's concern; what the runner insists on is that
/// each run starts from nothing.
///
/// `task` is handed over because what an engine runs can depend on what it is
/// running: a task that declares skill rules gets them attached, and one that
/// declares none gets an empty gate. A setup that had to guess from `repo`
/// alone would re-derive the task set's own declarations from a directory name.
pub trait RunSetup {
    fn build(&self, task: &Task, repo: &Path) -> Engine;

    /// Where the engine built for `repo` writes its journal, when it writes one.
    ///
    /// The runner reads it back after the turn to count what the harness
    /// refused. `None` by default: a setup that journals nowhere reports no
    /// refusal count rather than a count of zero.
    fn journal(&self, repo: &Path) -> Option<PathBuf> {
        let _ = repo;
        None
    }
}

// ========================================================================
// Data Structures
// ========================================================================

/// Every run of one task, with why each ended as it did.
pub struct RunReport {
    pub attempts: Vec<Attempt>,
    /// The task's group, carried here so the matrix can pool without a second
    /// lookup into the task set.
    pub group: String,
    pub task: String,
}

/// How one run ended.
///
/// The three failure shapes are kept apart because they call for different
/// responses: `Failed` is the agent's, `Stopped` says the limits were too tight
/// for the task, and `Errored` says nothing about the agent at all. A rate that
/// mixes them moves for reasons it cannot report.
pub enum Attempt {
    /// The turn ran and the success criterion held.
    Passed(Metrics),
    /// The turn ran and the success criterion did not hold: a real failure.
    Failed(Metrics),
    /// The turn ended on a limit before finishing the task.
    Stopped(String),
    /// The turn itself failed: a provider outage, a broken stream, a bad setup.
    Errored(String),
}

// ========================================================================
// RunReport Implementations
// ========================================================================

impl RunReport {
    /// Successes out of runs. Only [`Attempt::Passed`] counts.
    pub fn score(&self) -> Score {
        let successes = self
            .attempts
            .iter()
            .filter(|attempt| matches!(attempt, Attempt::Passed(_)))
            .count() as u32;
        Score {
            successes,
            n: self.attempts.len() as u32,
        }
    }

    /// What each run that produced a tree cost.
    ///
    /// A stopped or errored run contributes nothing: its tree is whatever the
    /// agent had reached when the limit fired, and averaging that into the churn
    /// of runs that finished would make a tighter budget read as a tidier agent.
    pub fn metrics(&self) -> Vec<Metrics> {
        self.attempts
            .iter()
            .filter_map(|attempt| match attempt {
                Attempt::Passed(metrics) | Attempt::Failed(metrics) => Some(*metrics),
                Attempt::Stopped(_) | Attempt::Errored(_) => None,
            })
            .collect()
    }

    /// A line per run that did not pass, for the report.
    pub fn failures(&self) -> Vec<String> {
        self.attempts
            .iter()
            .enumerate()
            .filter_map(|(index, attempt)| {
                let reason = match attempt {
                    Attempt::Passed(_) => return None,
                    Attempt::Failed(_) => "the success criterion did not hold".to_string(),
                    Attempt::Stopped(reason) => format!("stopped: {reason}"),
                    Attempt::Errored(error) => format!("errored: {error}"),
                };
                Some(format!("run {}: {reason}", index + 1))
            })
            .collect()
    }
}

// ========================================================================
// Functions
// ========================================================================

/// Run `task` `n` times, each against a fresh engine and a fresh copy of its
/// repo.
///
/// `scratch` is where the per-run copies go; the caller owns it so the copies
/// survive for inspection after a failing run, which is most of what a bench
/// run is for.
pub async fn run_task(
    setup: &dyn RunSetup,
    task: &Task,
    n: u32,
    scratch: &Path,
) -> io::Result<RunReport> {
    let mut attempts = Vec::with_capacity(n as usize);
    for run in 0..n {
        let repo = scratch.join(format!("{}-{run}", task.name));
        // A stale directory from an earlier invocation would reintroduce exactly
        // the contamination this exists to remove.
        if repo.exists() {
            std::fs::remove_dir_all(&repo)?;
        }
        copy_dir(&task.repo, &repo)?;

        let mut engine = setup.build(task, &repo);
        let started = Instant::now();
        let first_token = watch_first_token(&mut engine, started);
        let outcome = engine.turn(task.prompt.clone()).await;
        let elapsed = started.elapsed();
        // Dropping the observer ends the watcher, answered or not.
        engine.set_observer(None);
        let first_token = first_token.await.ok().flatten();
        attempts.push(match outcome {
            Ok(summary) if summary.stop == Stop::Done => {
                let mut metrics = measure(setup, task, &repo, summary.tool_calls, elapsed);
                metrics.first_token_ms = first_token.map(millis);
                if check_success(&task.success, &repo) {
                    Attempt::Passed(metrics)
                } else {
                    Attempt::Failed(metrics)
                }
            }
            // A limit stop is not the agent being wrong, and counting it as
            // such makes a tighter budget look like a worse agent.
            Ok(summary) => Attempt::Stopped(format!("{:?}", summary.stop)),
            Err(error) => Attempt::Errored(error.to_string()),
        });
    }
    Ok(RunReport {
        attempts,
        group: task.group.clone(),
        task: task.name.clone(),
    })
}

/// How long after `started` the engine first streamed anything (text, thinking,
/// or a tool call), watched through an observer set on it here.
///
/// The wait a person feels before the agent visibly starts, which wall time
/// cannot show: a run that spends 20 seconds before its first token and one
/// that streams at once can finish together.
fn watch_first_token(engine: &mut Engine, started: Instant) -> JoinHandle<Option<Duration>> {
    let (observer, mut events) = tokio::sync::mpsc::unbounded_channel();
    engine.set_observer(Some(observer));
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if matches!(
                event,
                UiEvent::TextDelta { .. }
                    | UiEvent::ThinkingDelta { .. }
                    | UiEvent::ToolStarted { .. }
            ) {
                return Some(started.elapsed());
            }
        }
        None
    })
}

fn millis(duration: Duration) -> u32 {
    u32::try_from(duration.as_millis()).unwrap_or(u32::MAX)
}

/// What one finished run cost: churn against the fixture, refusals and model
/// turns from its journal, its tool count and tokens, how long it took, and
/// the lint output it left behind.
///
/// The lint figure is the count the tree carries *after* the run, not a delta
/// against the fixture. A delta would have to be signed to be honest, since a
/// run may clean up warnings it did not cause, and an unsigned one would report
/// "fixed three warnings" and "changed nothing" as the same zero. The comparison
/// that matters is against another run of the same fixture, and the baseline
/// already provides it.
fn measure(
    setup: &dyn RunSetup,
    task: &Task,
    repo: &Path,
    tool_calls: u32,
    elapsed: Duration,
) -> Metrics {
    let (files_changed, lines_added, lines_removed) = quality::churn(&task.repo, repo);
    let journal = setup.journal(repo);
    let cost = journal.as_deref().and_then(quality::model_cost);
    Metrics {
        cache_read_tokens: cost.map(|cost| cost.cache_read_tokens),
        cache_write_tokens: cost.map(|cost| cost.cache_write_tokens),
        denied_calls: journal.as_deref().and_then(quality::denied_calls),
        files_changed,
        input_tokens: cost.map(|cost| cost.input_tokens),
        lines_added,
        lines_removed,
        lint_warnings: task
            .lint
            .as_deref()
            .and_then(|command| quality::lint_warnings(command, repo)),
        model_turns: cost.map(|cost| cost.turns),
        output_tokens: cost.map(|cost| cost.output_tokens),
        tool_calls,
        first_token_ms: None,
        wall_ms: Some(millis(elapsed)),
    }
}

/// Directory names never copied into a run's tree.
///
/// **Build output is not an input, and copying it silently rigs the score.**
/// `std::fs::copy` stamps every destination with the time of the copy, so a
/// `target/` carried across arrives with artifacts no older than the sources
/// beside it, and cargo reads that as a fresh build. The run then executes a
/// binary compiled from whatever the tree held when a human last checked the
/// task by hand -- which, for anyone who has just confirmed a task is solvable,
/// is the solved version. The criterion passes, the agent did nothing, and the
/// cell is indistinguishable from an earned one.
///
/// The fixtures' `.gitignore` keeps these out of the repository. This keeps them
/// out of a run, which is the half that matters once somebody has run a fixture
/// locally.
const NOT_INPUTS: &[&str] = &["__pycache__", ".git", "node_modules", "target"];

/// Copy `from` to `to` recursively, so each run starts from the same tree.
fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        if NOT_INPUTS
            .iter()
            .any(|name| entry.file_name() == std::ffi::OsStr::new(name))
        {
            continue;
        }
        let target = to.join(entry.file_name());
        // `file_type` does not follow symlinks, so a link in the fixture is
        // copied as a file rather than silently walked into.
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Whether `repo` already satisfies `success`.
///
/// `pub(crate)` so the task set can be checked against its own fixtures: a task
/// whose criterion holds on the untouched tree passes every run for free and
/// measures nothing, which is a defect in the set rather than in the agent.
pub(crate) fn check_success(success: &Success, repo: &Path) -> bool {
    match success {
        Success::All { checks } => checks.iter().all(|check| check_success(check, repo)),
        Success::FileMatches { path, expected } => {
            std::fs::read_to_string(repo.join(path)).is_ok_and(|content| content == *expected)
        }
        Success::FileContains { contains, path } => {
            std::fs::read_to_string(repo.join(path)).is_ok_and(|content| content.contains(contains))
        }
        // Through the platform shell, not `sh`: a success criterion is a
        // command line, and on Windows there is no `sh` to run it, so every
        // task would have scored zero for a reason that has nothing to do with
        // the agent.
        Success::CommandExitsZero { command } => {
            let shell = ShellSpec::platform_default();
            Command::new(&shell.program)
                .args(shell.argv(command))
                .current_dir(repo)
                .status()
                .is_ok_and(|status| status.success())
        }
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn report(attempts: Vec<Attempt>) -> RunReport {
        RunReport {
            attempts,
            group: "g".into(),
            task: "t".into(),
        }
    }

    #[test]
    fn only_a_pass_counts_towards_the_rate() {
        // The bug this replaces: every non-pass collapsed into one counter, so a
        // provider outage and a wrong answer moved the score identically.
        let score = report(vec![
            Attempt::Passed(Metrics::default()),
            Attempt::Failed(Metrics::default()),
            Attempt::Stopped("MaxTurns".into()),
            Attempt::Errored("503".into()),
        ])
        .score();
        assert_eq!(score.successes, 1);
        assert_eq!(score.n, 4, "every run is still a trial");
    }

    #[test]
    fn failures_say_which_run_and_why() {
        // A bare rate cannot tell "the limits were too tight" from "the agent
        // was wrong", and those call for different responses.
        let failures = report(vec![
            Attempt::Passed(Metrics::default()),
            Attempt::Stopped("MaxCost".into()),
            Attempt::Errored("stream broken".into()),
        ])
        .failures();

        assert_eq!(failures.len(), 2, "a pass is not a failure");
        assert!(failures[0].contains("run 2"), "{failures:?}");
        assert!(failures[0].contains("MaxCost"), "{failures:?}");
        assert!(failures[1].contains("stream broken"), "{failures:?}");
    }

    #[test]
    fn a_copied_repo_is_independent_of_its_source() {
        // The property every run's independence rests on: editing the copy must
        // not reach the fixture the next run copies from.
        let source = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(source.path().join("src")).expect("src");
        std::fs::write(source.path().join("src/main.rs"), "original").expect("write");

        let scratch = tempfile::tempdir().expect("tempdir");
        let copy = scratch.path().join("run-0");
        copy_dir(source.path(), &copy).expect("copies");
        std::fs::write(copy.join("src/main.rs"), "the agent's edit").expect("write");

        assert_eq!(
            std::fs::read_to_string(source.path().join("src/main.rs")).unwrap(),
            "original",
            "run 1's edit reached the fixture, so run 2 would start from it"
        );
    }

    #[test]
    fn an_all_check_holds_only_when_every_check_holds() {
        // The bug this pins: a `checks.iter().any(...)` typo passes a task where
        // one of three steps landed and two were skipped, which is exactly the
        // drift the multi-step task exists to catch scoring as a pass.
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::write(repo.path().join("VERSION"), "0.2.0\n").expect("write");
        std::fs::write(repo.path().join("NOTES.md"), "Bumped to 0.2.0\n").expect("write");
        // No CHANGELOG.md: the second step never happened.

        let all = Success::All {
            checks: vec![
                Success::FileMatches {
                    expected: "0.2.0\n".into(),
                    path: "VERSION".into(),
                },
                Success::FileContains {
                    contains: "0.2.0".into(),
                    path: "CHANGELOG.md".into(),
                },
                Success::FileContains {
                    contains: "Bumped to 0.2.0".into(),
                    path: "NOTES.md".into(),
                },
            ],
        };

        assert!(!check_success(&all, repo.path()));
        std::fs::write(
            repo.path().join("CHANGELOG.md"),
            "## 0.2.0\n\nVersion bump.\n",
        )
        .expect("write");
        assert!(check_success(&all, repo.path()));
    }

    #[test]
    fn a_leftover_build_directory_is_not_copied_into_a_run() {
        // The bug this guards is silent and scores in the agent's favour: a
        // `target/` copied across arrives with mtimes from the copy, cargo reads
        // it as fresh, and the run executes a binary built from whatever source
        // the fixture held when someone last solved the task by hand.
        let root = std::env::temp_dir().join(format!("summer-copy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let from = root.join("from");
        std::fs::create_dir_all(from.join("target/debug")).expect("fixture");
        std::fs::create_dir_all(from.join("src")).expect("fixture");
        std::fs::write(from.join("src/lib.rs"), "fn main() {}").expect("source");
        std::fs::write(from.join("target/debug/stale"), "solved").expect("artifact");

        let to = root.join("to");
        copy_dir(&from, &to).expect("copies");

        assert!(to.join("src/lib.rs").is_file(), "sources must be copied");
        assert!(
            !to.join("target").exists(),
            "build output must not reach the run's tree"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A task whose criterion already holds measures nothing.** It passes
    /// every run of every arm for free, and silently: the rate goes up, the
    /// interval narrows, and the number credits the agent with work it never
    /// did. That is the one defect in a task set a scored run cannot reveal,
    /// because from the harness's side a free pass and an earned one are the
    /// same cell.
    ///
    /// Ignored by default: the criteria build several fixture crates, which is
    /// minutes rather than the seconds `make check` is allowed. Run it whenever
    /// the set or a fixture changes:
    ///
    /// ```text
    /// cargo test -p summer-bench --lib -- --ignored no_task_passes
    /// ```
    #[test]
    #[ignore = "builds several fixture crates; run explicitly when the set changes"]
    fn no_task_passes_before_the_agent_touches_it() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tasks");
        let source =
            std::fs::read_to_string(base.join("starter.json")).expect("the set is readable");
        let set = crate::task::TaskSet::load(&source, &base).expect("the shipped task set parses");

        let mut already_passing = Vec::new();
        for task in &set.tasks {
            let scratch = std::env::temp_dir().join(format!(
                "summer-taskset-{}-{}",
                std::process::id(),
                task.name
            ));
            let _ = std::fs::remove_dir_all(&scratch);
            copy_dir(&task.repo, &scratch).expect("the fixture copies");
            if check_success(&task.success, &scratch) {
                already_passing.push(task.name.clone());
            }
            let _ = std::fs::remove_dir_all(&scratch);
        }
        assert!(
            already_passing.is_empty(),
            "these tasks pass without the agent doing anything: {already_passing:?}"
        );
    }
}

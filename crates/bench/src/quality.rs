//! What a run cost, beyond whether it passed.
//!
//! **A pass/fail rate cannot see the difference between a fix and a rewrite.**
//! Two runs that both satisfy `cargo test` are the same number here, whether one
//! changed four lines or reformatted the file and left three unused imports
//! behind. Every quality mechanism in this tree exists to move exactly that
//! difference, and until it is measured, adding another one is a guess.
//!
//! So this module measures four things per run, chosen because each is cheap,
//! deterministic, and reports a mechanism the agent actually has:
//!
//! - **Churn** (files, lines added, lines removed) against the fixture the run
//!   started from. This is minimal blast radius, made into a number.
//! - **Denied calls**, read back from the run's own journal. Every refusal the
//!   harness issued -- a policy denial, a blocked hook, a skill the agent had
//!   not loaded -- lands here, so a mechanism that fires constantly is visible
//!   rather than assumed to be helping.
//! - **Tool calls**, which say what the run spent to get there.
//! - **Lint warnings**, when the task names its own linter. Declared per task
//!   rather than guessed from the tree, for the reason [`TaskSet::commands`]
//!   is declared: a bench that infers the check measures the inference.
//! - **Tokens, model turns and wall time**, which say what the run cost to
//!   produce: prompt tokens split by how the cache served them, output tokens,
//!   the model round trips, and the seconds. A change that holds the pass rate
//!   while it doubles the tokens, or one that breaks the prompt cache, is
//!   invisible to every other number here.
//!
//! **These are counts, not proportions, and that decides how they are
//! reported.** The Wilson interval in [`crate::score`] answers "is this rate
//! distinguishable from that one", and it is the right tool for a success rate
//! and the wrong one for a line count. Five runs of a count support a median and
//! the spread around it, and [`summarize`] returns exactly that and no verdict.
//! A "churn improved by 12%" claim from five runs would be the same failure this
//! crate refuses everywhere else, wearing a different unit.
//!
//! [`TaskSet::commands`]: crate::TaskSet

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use journal::{Journal, Outcome, Record, RecordKind, read_records};
use serde::{Deserialize, Serialize};
use tools::ShellSpec;

// ========================================================================
// Constants
// ========================================================================

/// The largest file, in lines, that is diffed line by line.
///
/// The churn count is exact through a longest-common-subsequence pass, which
/// costs the product of the two lengths. Past this size the pass is the slowest
/// thing in a bench run, and a file this large in a fixture is being replaced
/// rather than edited, so it is counted as a full rewrite and said to be one.
const MAX_DIFF_LINES: usize = 5_000;

const PERCENT: u64 = 100;

// ========================================================================
// Data Structures
// ========================================================================

/// What one run cost. Every field is a count, never a rate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metrics {
    /// Prompt tokens served from the cache, across the run's model turns.
    ///
    /// This field and the other cost fields are `None` in a baseline recorded
    /// before they existed, and `serde(default)` is what makes that so: read
    /// as zero, an old baseline would report every cost as having appeared
    /// from nothing.
    #[serde(default)]
    pub cache_read_tokens: Option<u32>,
    /// Prompt tokens written to the cache.
    #[serde(default)]
    pub cache_write_tokens: Option<u32>,
    /// Tool calls the harness refused, from the run's journal. `None` when no
    /// journal was readable, which is not the same fact as a run that was
    /// refused nothing.
    pub denied_calls: Option<u32>,
    pub files_changed: u32,
    /// Milliseconds before the run first streamed anything.
    #[serde(default)]
    pub first_token_ms: Option<u32>,
    /// Prompt tokens neither read from nor written to the cache.
    #[serde(default)]
    pub input_tokens: Option<u32>,
    pub lines_added: u32,
    pub lines_removed: u32,
    /// Output lines from the task's own lint command, or `None` when the task
    /// names none. `None` and `Some(0)` are different facts: no linter ran, and
    /// a linter ran clean.
    pub lint_warnings: Option<u32>,
    /// Model round trips, from the run's journal; `None` when it was unreadable.
    #[serde(default)]
    pub model_turns: Option<u32>,
    #[serde(default)]
    pub output_tokens: Option<u32>,
    pub tool_calls: u32,
    /// How long the run took, in milliseconds.
    #[serde(default)]
    pub wall_ms: Option<u32>,
}

/// What a run's model turns cost, summed from its journal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModelCost {
    pub cache_read_tokens: u32,
    pub cache_write_tokens: u32,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub turns: u32,
}

/// A count across several runs: the middle, and how far the runs spread.
///
/// The spread is not a confidence interval and is not labelled as one. It is the
/// observed minimum and maximum, which is all a handful of runs of a count can
/// honestly show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub max: u32,
    pub median: u32,
    pub min: u32,
}

// ========================================================================
// Metrics Implementations
// ========================================================================

impl Metrics {
    /// The per-field median of `runs`, or `None` when nothing ran.
    ///
    /// Field by field rather than by picking one representative run: the median
    /// churn and the median tool count are separate questions, and the run that
    /// sits in the middle of one need not sit in the middle of the other.
    pub fn medians(runs: &[Metrics]) -> Option<Metrics> {
        if runs.is_empty() {
            return None;
        }
        Some(Metrics {
            cache_read_tokens: optional_median(runs, |run| run.cache_read_tokens),
            cache_write_tokens: optional_median(runs, |run| run.cache_write_tokens),
            denied_calls: optional_median(runs, |run| run.denied_calls),
            files_changed: median(&field(runs, |run| run.files_changed)),
            first_token_ms: optional_median(runs, |run| run.first_token_ms),
            input_tokens: optional_median(runs, |run| run.input_tokens),
            lines_added: median(&field(runs, |run| run.lines_added)),
            lines_removed: median(&field(runs, |run| run.lines_removed)),
            lint_warnings: optional_median(runs, |run| run.lint_warnings),
            model_turns: optional_median(runs, |run| run.model_turns),
            output_tokens: optional_median(runs, |run| run.output_tokens),
            tool_calls: median(&field(runs, |run| run.tool_calls)),
            wall_ms: optional_median(runs, |run| run.wall_ms),
        })
    }

    /// Every prompt token the run sent, however the cache served it.
    pub fn prompt_tokens(&self) -> Option<u64> {
        Some(
            u64::from(self.input_tokens?)
                + u64::from(self.cache_read_tokens?)
                + u64::from(self.cache_write_tokens?),
        )
    }

    /// The share of prompt tokens read from the cache, in whole percent.
    ///
    /// Over *every* prompt token, cache writes included: a write is a token the
    /// cache did not serve, and leaving writes out of the denominator would
    /// report a session that re-wrote its cache every turn as fully cached.
    pub fn cache_hit_percent(&self) -> Option<u32> {
        let total = self.prompt_tokens()?;
        let read = u64::from(self.cache_read_tokens?);
        (total > 0).then(|| (read * PERCENT / total) as u32)
    }
}

// ========================================================================
// Functions
// ========================================================================

/// How far `after` moved from `before`: files touched, lines added, lines
/// removed.
///
/// Both trees are on disk, so this needs no version control and no language
/// support. A file present in one tree and absent from the other counts whole:
/// creating a file adds every one of its lines, and deleting one removes them.
pub fn churn(before: &Path, after: &Path) -> (u32, u32, u32) {
    let mut files = 0;
    let mut added = 0;
    let mut removed = 0;
    for path in paths_under(before).union(&paths_under(after)) {
        let old = std::fs::read_to_string(before.join(path)).unwrap_or_default();
        let new = std::fs::read_to_string(after.join(path)).unwrap_or_default();
        if old == new {
            continue;
        }
        let (file_added, file_removed) = line_churn(&old, &new);
        files += 1;
        added += file_added;
        removed += file_removed;
    }
    (files, added, removed)
}

/// How many tool calls the harness refused, read back from the run's journal.
///
/// `None` when the journal cannot be read: a run whose refusals are unknown must
/// not report zero of them, which is the reading that would make a gate that
/// fires constantly look like one that never fires.
pub fn denied_calls(journal: &Path) -> Option<u32> {
    let denied = walked_records(journal)?
        .iter()
        .filter(|record| match &record.kind {
            RecordKind::ToolResult(result) => matches!(result.outcome, Outcome::Denied { .. }),
            _ => false,
        })
        .count();
    Some(denied as u32)
}

/// Every model round trip the run made and the tokens each one took, summed
/// from the `ModelTurn` records in its journal.
///
/// From the journal rather than the engine's turn summary, whose usage is the
/// final round's alone: a run of twelve rounds would report one twelfth of its
/// cost. The records a compaction dropped still count, since they leave the
/// context but not the walk, and each was paid for. `None` when the journal
/// cannot be read, for the reason [`denied_calls`] gives.
pub fn model_cost(journal: &Path) -> Option<ModelCost> {
    let mut cost = ModelCost::default();
    for record in walked_records(journal)? {
        if let RecordKind::ModelTurn(turn) = &record.kind {
            cost.cache_read_tokens = cost.cache_read_tokens.saturating_add(turn.usage.cache_read);
            cost.cache_write_tokens = cost
                .cache_write_tokens
                .saturating_add(turn.usage.cache_write);
            cost.input_tokens = cost.input_tokens.saturating_add(turn.usage.input);
            cost.output_tokens = cost.output_tokens.saturating_add(turn.usage.output);
            cost.turns += 1;
        }
    }
    Some(cost)
}

/// The records on the walk to the journal's last record, or `None` when the
/// journal cannot be read.
fn walked_records(journal: &Path) -> Option<Vec<Arc<Record>>> {
    let file = std::fs::File::open(journal).ok()?;
    let records = read_records(std::io::BufReader::new(file)).ok()?;
    let head = records.last()?.id;
    let walk = Journal::from_records(records).ok()?.walk(head)?;
    Some(walk.records().to_vec())
}

/// How many lines the task's lint command printed, run in `repo`.
///
/// Lines rather than a parsed diagnostic count, because the command is the
/// task's own and its output format is its own. A tool that prints one line per
/// finding is counted exactly; one that prints a summary paragraph is counted
/// consistently, which is what a delta between two runs needs.
///
/// `None` when the command could not be run at all, for the reason
/// [`denied_calls`] gives: an unmeasured check must not report a clean one.
pub fn lint_warnings(command: &str, repo: &Path) -> Option<u32> {
    let shell = ShellSpec::platform_default();
    let output = Command::new(&shell.program)
        .args(shell.argv(command))
        .current_dir(repo)
        .output()
        .ok()?;
    let count = [&output.stdout, &output.stderr]
        .into_iter()
        .map(|stream| {
            String::from_utf8_lossy(stream)
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count()
        })
        .sum::<usize>();
    Some(count as u32)
}

/// The middle, the least, and the most of `values`.
pub fn summarize(values: &[u32]) -> Option<Summary> {
    if values.is_empty() {
        return None;
    }
    Some(Summary {
        max: *values.iter().max().unwrap_or(&0),
        median: median(values),
        min: *values.iter().min().unwrap_or(&0),
    })
}

/// The middle value of `values`, taking the lower of the two middles for an
/// even count so the answer is always one of the observations.
fn median(values: &[u32]) -> u32 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted
        .get(sorted.len().saturating_sub(1) / 2)
        .copied()
        .unwrap_or(0)
}

/// One field of every run, for [`Metrics::medians`].
fn field(runs: &[Metrics], of: impl Fn(&Metrics) -> u32) -> Vec<u32> {
    runs.iter().map(of).collect()
}

/// The median of a field only some runs measured, or `None` when none did.
///
/// The runs that measured nothing are dropped rather than read as zero: one
/// unreadable journal in five must not pull the refusal count towards zero and
/// make a gate that fired every time look like one that fired four times in
/// five.
fn optional_median(runs: &[Metrics], of: impl Fn(&Metrics) -> Option<u32>) -> Option<u32> {
    let measured: Vec<u32> = runs.iter().filter_map(of).collect();
    (!measured.is_empty()).then(|| median(&measured))
}

/// Lines added and removed between two versions of one file.
///
/// Exact, through the length of the longest common subsequence of the two line
/// sequences: what is not in the common subsequence was added or removed. A
/// block that only moved is therefore counted as churn, which is the point --
/// moving code is a change the reviewer has to read.
fn line_churn(old: &str, new: &str) -> (u32, u32) {
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();
    if old.len() > MAX_DIFF_LINES || new.len() > MAX_DIFF_LINES {
        return (new.len() as u32, old.len() as u32);
    }
    let common = common_lines(&old, &new);
    ((new.len() - common) as u32, (old.len() - common) as u32)
}

/// The length of the longest common subsequence of two line sequences.
///
/// Two rows rather than the full table: the length is all the churn count needs,
/// and the table would be the product of the two file lengths in memory for a
/// diff nothing renders.
fn common_lines(old: &[&str], new: &[&str]) -> usize {
    let mut previous = vec![0usize; new.len() + 1];
    let mut current = vec![0usize; new.len() + 1];
    for old_line in old {
        for (index, new_line) in new.iter().enumerate() {
            current[index + 1] = if old_line == new_line {
                previous[index] + 1
            } else {
                current[index].max(previous[index + 1])
            };
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[new.len()]
}

/// Every file under `root`, as paths relative to it.
///
/// A sorted set, so the two trees compare by path rather than by directory
/// order. `.summer` and `target` are skipped: a run writes its own journal and
/// its own build output inside the copy, and counting those as churn would
/// report every run as having changed files it was never asked about.
fn paths_under(root: &Path) -> BTreeSet<PathBuf> {
    let mut found = BTreeSet::new();
    collect_paths(root, root, &mut found);
    found
}

fn collect_paths(root: &Path, dir: &Path, found: &mut BTreeSet<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if entry.file_name() == ".summer" || entry.file_name() == "target" {
                continue;
            }
            collect_paths(root, &path, found);
        } else if let Ok(relative) = path.strip_prefix(root) {
            found.insert(relative.to_path_buf());
        }
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(lines_added: u32, tool_calls: u32) -> Metrics {
        Metrics {
            lines_added,
            tool_calls,
            ..Metrics::default()
        }
    }

    #[test]
    fn test_a_changed_line_counts_as_one_added_and_one_removed() {
        // The shape every churn number rests on. A metric that counted a
        // one-line change as two additions would report every edit as twice the
        // blast radius it was.
        assert_eq!(line_churn("a\nb\nc\n", "a\nB\nc\n"), (1, 1));
    }

    #[test]
    fn test_an_appended_line_removes_nothing() {
        assert_eq!(line_churn("a\nb\n", "a\nb\nc\n"), (1, 0));
    }

    #[test]
    fn test_a_moved_block_is_counted_as_churn() {
        // The bug the LCS exists to avoid: counting lines as a multiset makes a
        // reordering cancel to zero, so a run that rewrote a file by shuffling
        // it would score as the most surgical run in the set.
        let (added, removed) = line_churn("a\nb\nc\nd\n", "c\nd\na\nb\n");
        assert!(
            added > 0 && removed > 0,
            "got {added} added, {removed} removed"
        );
    }

    #[test]
    fn test_a_rewritten_file_past_the_cap_counts_whole() {
        let big = "x\n".repeat(MAX_DIFF_LINES + 1);
        assert_eq!(
            line_churn(&big, "one line\n"),
            (1, (MAX_DIFF_LINES + 1) as u32)
        );
    }

    #[test]
    fn test_churn_counts_a_created_file_whole() {
        let before = tempfile::tempdir().expect("tempdir");
        let after = tempfile::tempdir().expect("tempdir");
        std::fs::write(before.path().join("kept.txt"), "same\n").expect("write");
        std::fs::write(after.path().join("kept.txt"), "same\n").expect("write");
        std::fs::write(after.path().join("new.txt"), "one\ntwo\n").expect("write");

        assert_eq!(
            churn(before.path(), after.path()),
            (1, 2, 0),
            "an untouched file must not appear in the count"
        );
    }

    #[test]
    fn test_medians_are_taken_per_field() {
        // The bug this prevents: picking the middle *run* and reporting its
        // fields, which reports one run's tool count as the set's median even
        // when it is the highest of the five.
        let runs = [metrics(10, 9), metrics(2, 1), metrics(6, 5)];
        let medians = Metrics::medians(&runs).expect("three runs");
        assert_eq!(medians.lines_added, 6);
        assert_eq!(medians.tool_calls, 5);
    }

    #[test]
    fn test_a_task_with_no_linter_keeps_a_none_median() {
        // `None` and `Some(0)` are opposite readings: no check ran, and a check
        // ran clean. A median that flattened the first into the second would
        // report a lint improvement for a task that has no linter.
        let medians = Metrics::medians(&[metrics(1, 1), metrics(2, 2)]).expect("two runs");
        assert!(medians.lint_warnings.is_none());
    }

    #[test]
    fn test_a_cache_write_counts_against_the_hit_rate() {
        // A session that re-writes its cache every turn is the failure this
        // number exists to show; with writes left out of the denominator it
        // would read as fully cached.
        let run = Metrics {
            cache_read_tokens: Some(100),
            cache_write_tokens: Some(300),
            input_tokens: Some(100),
            ..Metrics::default()
        };
        assert_eq!(run.prompt_tokens(), Some(500));
        assert_eq!(run.cache_hit_percent(), Some(20));
    }

    #[test]
    fn test_a_baseline_recorded_before_cost_fields_reads_them_as_unmeasured() {
        // Read as zero, every token count in the next report would be a delta
        // from nothing.
        let old = r#"{"denied_calls":0,"files_changed":1,"lines_added":2,
            "lines_removed":0,"lint_warnings":null,"tool_calls":3}"#;
        let metrics: Metrics = serde_json::from_str(old).expect("an old baseline still parses");
        assert_eq!(metrics.tool_calls, 3);
        assert!(metrics.prompt_tokens().is_none() && metrics.wall_ms.is_none());
    }

    #[test]
    fn test_a_summary_reports_the_spread_it_saw() {
        let summary = summarize(&[4, 1, 9]).expect("three values");
        assert_eq!((summary.min, summary.median, summary.max), (1, 4, 9));
    }
}

//! The ablation matrix: one arm per disabled mechanism, plus the all-on
//! control, rendered side by side.
//!
//! **A mechanism that cannot be turned off cannot be measured, and one that
//! cannot be measured cannot be deleted.** The runner and the metrics are
//! shared with an ordinary scored run; what this module adds is the comparison
//! itself — every arm over the same tasks, in one table, with the all-on arm
//! first so the first comparison any reader makes is against the loop as it
//! ships.
//!
//! The table reports counts and medians, never a verdict. Whether a delta is
//! real is a judgment about variance the operator makes against the spread the
//! runs showed, the same position [`crate::quality`] takes on every count.
//!
//! **What the matrix can resolve, so a reader does not over-read it.** The
//! quantity that matters is trials, not tasks: a row's separating power comes
//! from how many runs it pools. Against a control passing around 83%, and
//! taking the project's own non-overlapping-interval test as the bar, the
//! shipped 24-task set resolves roughly this much:
//!
//! ```text
//!            RUNS=5            RUNS=10           RUNS=20
//!   total    15 pts (120)      11 pts (240)       7 pts (480)
//!   group    36+ pts (15-30)   23+ pts (30-60)   15+ pts (60-120)
//! ```
//!
//! So the **total** row is worth reading at the default `RUNS=5`, and a **group**
//! row is not: at that width a group only separates on a collapse, not on the
//! ten-point effect a mechanism plausibly has. Group rows start earning their
//! place around `RUNS=10` and are solid by `RUNS=20`. A group row that has not
//! moved is evidence of nothing either way, which is exactly what an overlapping
//! interval means and exactly what it is tempting to read as "no effect".

use crate::run::RunReport;

// ========================================================================
// Constants
// ========================================================================

/// The label of the control arm: nothing disabled.
pub const ALL_ON: &str = "all-on";

// ========================================================================
// Data Structures
// ========================================================================

/// One arm of the matrix: everything on, or one mechanism off, over the whole
/// task set.
pub struct ArmReport {
    /// The arm's label: [`ALL_ON`], or the disabled mechanism's kebab name.
    pub label: String,
    /// One report per task, in task-set order.
    pub reports: Vec<RunReport>,
}

// ========================================================================
// Functions
// ========================================================================

/// Render the matrix: one block per task, one column per arm.
///
/// Per task: the pass rate, how many runs stopped on a limit or errored on the
/// provider (the difference between an agent failing and a provider outage,
/// without which one rate-limit window reads as a mechanism effect), then the
/// medians of what the runs cost. A mechanism can hold the rate and still cost
/// tool calls, and deleting it would trade one for the other without the cost
/// rows ever saying so. `None` medians (no linter, no readable journal) render
/// as `-`, which is a different fact from `0`.
pub fn render_matrix(arms: &[ArmReport]) -> String {
    let labels: Vec<&str> = arms.iter().map(|arm| arm.label.as_str()).collect();
    let width = column_width(&labels);

    let mut out = String::new();
    out.push_str(&header(&labels, width));

    let names: Vec<String> = arms
        .first()
        .map(|arm| {
            arm.reports
                .iter()
                .map(|report| report.task.clone())
                .collect()
        })
        .unwrap_or_default();
    for name in names {
        out.push_str(&format!("\n{name}\n"));
        for row in ROWS {
            let cells: Vec<String> = arms
                .iter()
                .map(|arm| cell(row, arm.report(&name)))
                .collect();
            out.push_str(&table_row(row, &cells, width));
        }
    }

    out.push('\n');
    for group in groups(arms) {
        let cells: Vec<String> = arms
            .iter()
            .map(|arm| pooled(arm, |report| report.group == group))
            .collect();
        out.push_str(&table_row(&group, &cells, width));
    }
    out.push_str(&table_row(
        "total",
        &arms
            .iter()
            .map(|arm| pooled(arm, |_| true))
            .collect::<Vec<String>>(),
        width,
    ));
    out
}

/// Every group in the matrix, in the order the task set first mentions them.
///
/// Not sorted: the set's order is the author's, and a matrix that reordered it
/// would stop lining up with the file a reader has open beside it.
fn groups(arms: &[ArmReport]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for report in arms.iter().flat_map(|arm| arm.reports.iter()) {
        if !seen.iter().any(|group| group == &report.group) {
            seen.push(report.group.clone());
        }
    }
    seen
}

/// One arm's successes over the runs of every report `keep` accepts.
///
/// **Pooled rather than averaged.** A mean of per-task rates weights a task with
/// three runs the same as one with thirty; summing the runs is the quantity the
/// Wilson interval is defined over, and the only one that gets narrower as the
/// set grows.
fn pooled(arm: &ArmReport, keep: impl Fn(&RunReport) -> bool) -> String {
    let (successes, n) = arm
        .reports
        .iter()
        .filter(|report| keep(report))
        .map(|report| report.score())
        .fold((0, 0), |(successes, n), score| {
            (successes + score.successes, n + score.n)
        });
    format!("{successes}/{n}")
}

/// The rows every task block carries, in order.
///
/// `stop` and `error` are why [`crate::quality`] keeps provider outages apart
/// from wrong answers, surfaced here: an arm scored 0/3 by rate limiting and an
/// arm scored 0/3 by being wrong are the same cell without them, and the first
/// matrix this rendered read as though every mechanism helped, when the control
/// arm had simply run first into a rate-limit window.
const ROWS: [&str; 10] = [
    "pass", "stop", "error", "tools", "churn", "denied", "lint", "tokens", "cached", "turns",
];

// ========================================================================
// ArmReport Implementations
// ========================================================================

impl ArmReport {
    /// The report for `task`, or a synthetic empty one when this arm has none.
    ///
    /// An arm that stopped early (an operator interrupt mid-matrix) still
    /// renders: the missing tasks read as `0/0` rather than shifting every
    /// later task's numbers into the wrong column, which is the failure a
    /// zip-based table cannot survive.
    fn report(&self, task: &str) -> Option<&RunReport> {
        self.reports.iter().find(|report| report.task == task)
    }
}

// ========================================================================
// Rendering Helpers
// ========================================================================

/// One cell of the matrix: the row's quantity for one arm's run of one task.
fn cell(row: &str, report: Option<&RunReport>) -> String {
    let Some(report) = report else {
        return "0/0".to_string();
    };
    match row {
        "pass" => {
            let score = report.score();
            format!("{}/{}", score.successes, score.n)
        }
        "stop" => count_attempts(report, |attempt| {
            matches!(attempt, crate::run::Attempt::Stopped(_))
        }),
        "error" => count_attempts(report, |attempt| {
            matches!(attempt, crate::run::Attempt::Errored(_))
        }),
        "tools" => count(report, |metrics| metrics.tool_calls),
        "churn" => match report_medians(report) {
            Some(medians) => format!(
                "{}f +{} -{}",
                medians.files_changed, medians.lines_added, medians.lines_removed
            ),
            None => "-".to_string(),
        },
        "denied" => optional_count(report, |metrics| metrics.denied_calls),
        "lint" => optional_count(report, |metrics| metrics.lint_warnings),
        "tokens" => report_medians(report)
            .and_then(|medians| medians.prompt_tokens())
            .map_or_else(|| "-".to_string(), |tokens| tokens.to_string()),
        "cached" => report_medians(report)
            .and_then(|medians| medians.cache_hit_percent())
            .map_or_else(|| "-".to_string(), |hit| format!("{hit}%")),
        "turns" => optional_count(report, |metrics| metrics.model_turns),
        _ => "-".to_string(),
    }
}

/// The row label column is as wide as its widest entry.
const ROW_LABEL_WIDTH: usize = 6;

/// How wide each arm's column is: the label, or the widest cell, plus a gap.
fn column_width(labels: &[&str]) -> usize {
    labels
        .iter()
        .map(|label| label.chars().count())
        .fold(ROW_LABEL_WIDTH, usize::max)
        + 2
}

fn header(labels: &[&str], width: usize) -> String {
    let mut out = " ".repeat(ROW_LABEL_WIDTH + 2);
    for label in labels {
        out.push_str(&format!("{label:<width$}"));
    }
    out
}

fn table_row(row: &str, cells: &[String], width: usize) -> String {
    let mut out = format!("{row:<ROW_LABEL_WIDTH$}  ");
    for cell in cells {
        out.push_str(&format!("{cell:<width$}"));
    }
    out.push('\n');
    out
}

/// The median of one counted field across the runs that finished.
fn count(report: &RunReport, of: impl Fn(&crate::Metrics) -> u32) -> String {
    report_medians(report)
        .map(|medians| of(&medians).to_string())
        .unwrap_or_else(|| "-".to_string())
}

/// How many of one arm's attempts ended the given way, out of all runs.
fn count_attempts(report: &RunReport, of: impl Fn(&crate::run::Attempt) -> bool) -> String {
    format!(
        "{}/{}",
        report.attempts.iter().filter(|attempt| of(attempt)).count(),
        report.attempts.len()
    )
}

/// The median of one field only some runs measured, `-` when none did.
fn optional_count(report: &RunReport, of: impl Fn(&crate::Metrics) -> Option<u32>) -> String {
    report_medians(report)
        .and_then(|medians| of(&medians))
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn report_medians(report: &RunReport) -> Option<crate::Metrics> {
    crate::Metrics::medians(&report.metrics())
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Metrics;
    use crate::run::Attempt;

    fn report(task: &str, attempts: Vec<Attempt>) -> RunReport {
        RunReport {
            attempts,
            group: "g".into(),
            task: task.into(),
        }
    }

    #[test]
    fn each_arm_answers_under_its_own_label() {
        // The failure this pins: arms sharing one label, or a column ordering
        // that drifted from the header's. Either one silently reports arm A's
        // numbers as arm B's, and a mechanism gets deleted (or kept) on the
        // wrong evidence.
        let arms = [
            ArmReport {
                label: ALL_ON.to_string(),
                reports: vec![report(
                    "t",
                    vec![
                        Attempt::Passed(Metrics::default()),
                        Attempt::Passed(Metrics::default()),
                    ],
                )],
            },
            ArmReport {
                label: "-verify-gate".to_string(),
                reports: vec![report("t", vec![Attempt::Failed(Metrics::default())])],
            },
        ];

        let table = render_matrix(&arms);
        let header = table
            .lines()
            .find(|line| line.contains(ALL_ON))
            .expect("a header naming the arms");
        let pass = table
            .lines()
            .find(|line| line.starts_with("pass"))
            .expect("a pass row");

        let all_on = header
            .find(ALL_ON)
            .expect("the control arm is the first column");
        let gated = header
            .find("-verify-gate")
            .expect("the ablated arm has its own column");
        assert_ne!(all_on, gated, "two arms share one column");

        let control = &pass[all_on..all_on + 3];
        let ablated = &pass[gated..gated + 3];
        assert_eq!(control, "2/2", "the control arm's own rate: {pass}");
        assert_eq!(ablated, "0/1", "the ablated arm's own rate: {pass}");
    }

    #[test]
    fn an_arm_missing_a_task_renders_it_as_no_runs_rather_than_shifting() {
        // The early-stopped matrix: an arm with no report for a task must show
        // `0/0` in that task's row, not borrow the next task's numbers.
        let arms = [ArmReport {
            label: ALL_ON.to_string(),
            reports: vec![],
        }];
        let table = render_matrix(&arms);
        assert!(
            table.contains("0/0"),
            "a missing report reads as no runs: {table}"
        );
    }

    #[test]
    fn a_group_row_pools_the_runs_of_its_tasks() {
        // The reason `group` exists. Switching a mechanism off moves only the
        // tasks that reach it, so its effect arrives diluted across the set; the
        // group row is where those runs are added up instead. Two tasks in one
        // group, three runs each, must read as one 6-run cell and not as two.
        let arm = ArmReport {
            label: ALL_ON.to_string(),
            reports: vec![
                RunReport {
                    attempts: vec![passed(), passed(), failed()],
                    group: "skill".into(),
                    task: "one".into(),
                },
                RunReport {
                    attempts: vec![passed(), failed(), failed()],
                    group: "skill".into(),
                    task: "two".into(),
                },
                RunReport {
                    attempts: vec![passed()],
                    group: "smoke".into(),
                    task: "three".into(),
                },
            ],
        };
        assert_eq!(pooled(&arm, |report| report.group == "skill"), "3/6");
        assert_eq!(pooled(&arm, |report| report.group == "smoke"), "1/1");
        assert_eq!(pooled(&arm, |_| true), "4/7");
    }

    #[test]
    fn groups_keep_the_order_the_task_set_gave_them() {
        // Sorting would stop the matrix lining up with the file open beside it.
        let arm = ArmReport {
            label: ALL_ON.to_string(),
            reports: vec![
                RunReport {
                    attempts: vec![passed()],
                    group: "zeta".into(),
                    task: "a".into(),
                },
                RunReport {
                    attempts: vec![passed()],
                    group: "alpha".into(),
                    task: "b".into(),
                },
                RunReport {
                    attempts: vec![passed()],
                    group: "zeta".into(),
                    task: "c".into(),
                },
            ],
        };
        assert_eq!(
            groups(&[arm]),
            vec!["zeta".to_string(), "alpha".to_string()]
        );
    }

    fn passed() -> Attempt {
        Attempt::Passed(Metrics::default())
    }

    fn failed() -> Attempt {
        Attempt::Failed(Metrics::default())
    }
}

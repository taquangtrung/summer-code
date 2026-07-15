//! Rendering a run as the honest, easy reading.
//!
//! The bar every line here has to clear: the easiest thing to read must also be
//! the true thing. A bare "80%" is easier to read than "4/5, 95% CI [36, 97]"
//! and says something the sample cannot support, so it is never printed alone.
//! The same rule decides the header: when the run and the baseline measured
//! different things, the deltas are withheld rather than printed with a caveat
//! underneath, because a caveat under a number does not survive being quoted.

use crate::baseline::Baseline;
use crate::quality::{Metrics, summarize};
use crate::run::RunReport;
use crate::score::{Comparison, compare};

// ========================================================================
// Constants
// ========================================================================

const MS_PER_SECOND: f64 = 1000.0;

// ========================================================================
// Data Structures
// ========================================================================

/// A human-readable summary that never reduces a comparison to a bare rate.
pub struct Report {
    pub text: String,
}

// ========================================================================
// Functions
// ========================================================================

pub fn render(comparison: &Comparison) -> Report {
    let effect_pct = comparison.effect * 100.0;
    let lo = comparison.new_interval.0 * 100.0;
    let hi = comparison.new_interval.1 * 100.0;
    let verdict = if comparison.distinguishable {
        "distinguishable from baseline"
    } else {
        "not distinguishable from noise"
    };
    let text = format!("{effect_pct:+.1} points, 95% CI [{lo:.1}, {hi:.1}] - {verdict}");
    Report { text }
}

/// Render a whole run: provenance, any drift from the baseline, then a line per
/// task and a line per run that did not pass.
pub fn render_run(current: &Baseline, baseline: Option<&Baseline>, runs: &[RunReport]) -> Report {
    let mut text = format!(
        "task set: {}\nmodel: {}\ncommit: {}\nprompt: {}\ntools: {}\n",
        current.task_set,
        current.model,
        current.commit,
        current.prompt_digest,
        current.tools_digest
    );

    // Computed once, before any task line, because it decides whether a delta
    // may be printed at all.
    let drift = baseline
        .map(|baseline| baseline.drift(current))
        .unwrap_or_default();
    if !drift.is_empty() {
        text.push_str("\nNOT COMPARABLE to the baseline:\n");
        for entry in &drift {
            text.push_str(&format!(
                "  {}: {} (baseline: {})\n",
                entry.dimension, entry.current, entry.baseline
            ));
        }
        text.push_str(
            "  A rate measured before this change and one measured after are different \
             questions. The deltas are withheld.\n",
        );
    }

    text.push('\n');
    for report in runs {
        let score = report.score();
        let (lo, hi) = score.interval();
        text.push_str(&format!(
            "{}: {}/{}, 95% CI [{:.1}, {:.1}]",
            report.task,
            score.successes,
            score.n,
            lo * 100.0,
            hi * 100.0
        ));

        // A delta needs both a baseline entry and an undrifted comparison. Say
        // which one is missing rather than printing the task line bare: "this
        // task is new" and "this run is not comparable" are different facts, and
        // a silent omission reads as neither.
        match baseline.and_then(|baseline| baseline.score(&report.task)) {
            _ if !drift.is_empty() => {}
            Some(previous) => {
                text.push_str(&format!("  {}", render(&compare(score, previous)).text));
            }
            None => text.push_str("  (no baseline entry)"),
        }
        text.push('\n');

        // The cost of the runs that finished, under the rate they produced. A
        // rate says whether the agent got there; these say what it spent and
        // what it disturbed on the way, which is the half a pass/fail number
        // cannot see.
        let measured = report.metrics();
        if let Some(medians) = Metrics::medians(&measured) {
            text.push_str(&format!("  {}\n", render_medians(&medians)));
            if let Some(spread) = render_spread(&measured) {
                text.push_str(&format!("  {spread}\n"));
            }
            match baseline.and_then(|baseline| baseline.metrics(&report.task)) {
                _ if !drift.is_empty() => {}
                Some(previous) => {
                    text.push_str(&format!("  {}\n", render_metric_delta(&previous, &medians)));
                }
                None => {}
            }
        }

        for failure in report.failures() {
            text.push_str(&format!("  {failure}\n"));
        }
    }

    Report { text }
}

/// The middle run, as the one line that says what a run of this task costs.
fn render_medians(medians: &Metrics) -> String {
    let mut line = format!(
        "median run: +{}/-{} lines over {} files, {} tool calls",
        medians.lines_added, medians.lines_removed, medians.files_changed, medians.tool_calls
    );
    // Omitted rather than shown as zero when unmeasured: "no journal to read"
    // and "nothing was refused" are opposite readings of the same 0.
    if let Some(denied) = medians.denied_calls {
        line.push_str(&format!(", {denied} refused"));
    }
    if let Some(lint) = medians.lint_warnings {
        line.push_str(&format!(", {lint} lint lines"));
    }
    if let Some(cost) = render_cost(medians) {
        line.push_str(&format!("; {cost}"));
    }
    line
}

/// What the run cost to produce, or `None` for runs measured before these
/// fields were: prompt tokens and the share the cache served, output tokens,
/// model turns, seconds. Each part is shown only when it was measured.
fn render_cost(metrics: &Metrics) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(prompt) = metrics.prompt_tokens() {
        let mut part = format!("{prompt} prompt tokens");
        if let Some(hit) = metrics.cache_hit_percent() {
            part.push_str(&format!(" ({hit}% cached)"));
        }
        parts.push(part);
    }
    if let Some(output) = metrics.output_tokens {
        parts.push(format!("{output} output tokens"));
    }
    if let Some(turns) = metrics.model_turns {
        parts.push(format!("{turns} model turns"));
    }
    if let Some(ms) = metrics.first_token_ms {
        parts.push(format!("first token {:.1}s", f64::from(ms) / MS_PER_SECOND));
    }
    if let Some(ms) = metrics.wall_ms {
        parts.push(format!("{:.1}s", f64::from(ms) / MS_PER_SECOND));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// How far the runs disagreed, for the two counts that move most.
///
/// Named a spread and never an interval: it is the least and the most that were
/// observed, and a handful of runs of a count supports nothing stronger. One run
/// has no spread to report.
fn render_spread(runs: &[Metrics]) -> Option<String> {
    if runs.len() < 2 {
        return None;
    }
    let added = summarize(&runs.iter().map(|run| run.lines_added).collect::<Vec<_>>())?;
    let calls = summarize(&runs.iter().map(|run| run.tool_calls).collect::<Vec<_>>())?;
    Some(format!(
        "spread over {} runs: lines added {}-{}, tool calls {}-{}",
        runs.len(),
        added.min,
        added.max,
        calls.min,
        calls.max
    ))
}

/// The baseline's medians against this run's, as `was -> now`.
fn render_metric_delta(baseline: &Metrics, current: &Metrics) -> String {
    let mut line = format!(
        "vs baseline: added {} -> {}, removed {} -> {}, files {} -> {}, tool calls {} -> {}",
        baseline.lines_added,
        current.lines_added,
        baseline.lines_removed,
        current.lines_removed,
        baseline.files_changed,
        current.files_changed,
        baseline.tool_calls,
        current.tool_calls
    );
    // Only when both sides measured it. A delta against a field the baseline
    // never recorded is a delta against nothing.
    if let (Some(was), Some(now)) = (baseline.denied_calls, current.denied_calls) {
        line.push_str(&format!(", refused {was} -> {now}"));
    }
    if let (Some(was), Some(now)) = (baseline.lint_warnings, current.lint_warnings) {
        line.push_str(&format!(", lint {was} -> {now}"));
    }
    if let (Some(was), Some(now)) = (baseline.prompt_tokens(), current.prompt_tokens()) {
        line.push_str(&format!(", prompt tokens {was} -> {now}"));
    }
    if let (Some(was), Some(now)) = (baseline.cache_hit_percent(), current.cache_hit_percent()) {
        line.push_str(&format!(", cached {was}% -> {now}%"));
    }
    if let (Some(was), Some(now)) = (baseline.wall_ms, current.wall_ms) {
        line.push_str(&format!(
            ", {:.1}s -> {:.1}s",
            f64::from(was) / MS_PER_SECOND,
            f64::from(now) / MS_PER_SECOND
        ));
    }
    line
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::baseline::TaskScore;
    use crate::run::Attempt;

    fn baseline(tools_digest: &str) -> Baseline {
        Baseline {
            commit: "abc1234".into(),
            model: "claude-opus-4-8".into(),
            prompt_digest: "prompt-a".into(),
            task_set: "starter".into(),
            tasks: vec![TaskScore {
                metrics: None,
                n: 5,
                name: "greet".into(),
                successes: 0,
            }],
            tools_digest: tools_digest.into(),
        }
    }

    fn metrics(lines_added: u32) -> Metrics {
        Metrics {
            lines_added,
            lines_removed: 1,
            files_changed: 1,
            tool_calls: 4,
            ..Metrics::default()
        }
    }

    fn runs() -> Vec<RunReport> {
        vec![RunReport {
            attempts: (0..5)
                .map(|run| Attempt::Passed(metrics(run + 1)))
                .collect(),
            group: "g".into(),
            task: "greet".into(),
        }]
    }

    #[test]
    fn a_drifted_run_reports_no_delta_at_all() {
        // 5/5 against a baseline of 0/5 is the most quotable delta this report
        // can produce, and it is exactly the one that must not be printed when
        // the tool set changed underneath it. A caveat below the number does not
        // survive the number being copied into a message.
        let report = render_run(&baseline("tools-b"), Some(&baseline("tools-a")), &runs());

        assert!(report.text.contains("NOT COMPARABLE"), "{}", report.text);
        assert!(
            !report.text.contains("points"),
            "a delta survived the drift check: {}",
            report.text
        );
    }

    #[test]
    fn an_undrifted_run_reports_the_delta_with_its_interval() {
        let report = render_run(&baseline("tools-a"), Some(&baseline("tools-a")), &runs());

        assert!(!report.text.contains("NOT COMPARABLE"), "{}", report.text);
        assert!(report.text.contains("points"), "{}", report.text);
        assert!(report.text.contains("95% CI"), "{}", report.text);
    }

    #[test]
    fn what_a_run_cost_is_reported_under_its_rate() {
        // The whole point of measuring: two runs that both pass are the same
        // number until this line separates them.
        let report = render_run(&baseline("tools-a"), None, &runs());

        assert!(report.text.contains("median run:"), "{}", report.text);
        assert!(
            report.text.contains("+3/-1 lines"),
            "the median of 1..5 added lines is 3: {}",
            report.text
        );
        assert!(
            report.text.contains("spread over 5 runs"),
            "five runs that disagreed must show the spread: {}",
            report.text
        );
    }

    #[test]
    fn a_drifted_run_withholds_the_metric_delta_too() {
        // The rate delta is already withheld under drift. A churn delta that
        // survived it would be the same claim in a different unit: two runs
        // that measured different things, compared anyway.
        let mut previous = baseline("tools-a");
        previous.tasks[0].metrics = Some(metrics(99));
        let report = render_run(&baseline("tools-b"), Some(&previous), &runs());

        assert!(!report.text.contains("vs baseline"), "{}", report.text);
        assert!(
            report.text.contains("median run:"),
            "this run's own cost is still its own fact: {}",
            report.text
        );
    }

    #[test]
    fn an_undrifted_run_reports_the_metric_delta() {
        let mut previous = baseline("tools-a");
        previous.tasks[0].metrics = Some(metrics(99));
        let report = render_run(&baseline("tools-a"), Some(&previous), &runs());

        assert!(
            report.text.contains("added 99 -> 3"),
            "the delta reads baseline first: {}",
            report.text
        );
    }

    #[test]
    fn a_baseline_that_never_measured_cost_shows_no_metric_delta() {
        // `metrics: None` means "this baseline predates the measurement", and a
        // delta against it would be a delta against a zero nobody recorded.
        let report = render_run(&baseline("tools-a"), Some(&baseline("tools-a")), &runs());

        assert!(!report.text.contains("vs baseline"), "{}", report.text);
    }

    #[test]
    fn a_task_missing_from_the_baseline_says_so() {
        // Otherwise the line is indistinguishable from a task whose delta was
        // withheld, and from one that happened to move by zero.
        let mut previous = baseline("tools-a");
        previous.tasks.clear();
        let report = render_run(&baseline("tools-a"), Some(&previous), &runs());

        assert!(report.text.contains("no baseline entry"), "{}", report.text);
    }
}

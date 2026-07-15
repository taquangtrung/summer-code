//! A score is a rate with its interval, never a bare percentage.

// ========================================================================
// Constants
// ========================================================================

/// z-value for a 95% confidence interval.
const Z_95: f64 = 1.96;

// ========================================================================
// Data Structures
// ========================================================================

/// `successes` out of `n` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Score {
    pub successes: u32,
    pub n: u32,
}

/// A comparison of a new score against a stored baseline.
pub struct Comparison {
    pub new_center: f64,
    pub new_interval: (f64, f64),
    pub baseline_center: f64,
    pub baseline_interval: (f64, f64),
    pub effect: f64,
    /// True if the two 95% intervals do not overlap (a change distinguishable
    /// from noise). False means "not enough evidence either way".
    pub distinguishable: bool,
}

// ========================================================================
// Score Implementations
// ========================================================================

impl Score {
    /// The point estimate of the success rate.
    pub fn rate(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.successes as f64 / self.n as f64
        }
    }

    /// The 95% Wilson score interval.
    pub fn interval(&self) -> (f64, f64) {
        wilson(self.successes, self.n)
    }
}

// ========================================================================
// Functions
// ========================================================================

/// Compare a new score against a baseline. The honest reading is the easy one:
/// an effect size with an interval that may overlap the baseline.
pub fn compare(new: Score, baseline: Score) -> Comparison {
    let new_center = center(new.successes, new.n);
    let baseline_center = center(baseline.successes, baseline.n);
    let new_interval = new.interval();
    let baseline_interval = baseline.interval();
    let effect = new_center - baseline_center;
    let distinguishable =
        new_interval.1 < baseline_interval.0 || baseline_interval.1 < new_interval.0;
    Comparison {
        new_center,
        new_interval,
        baseline_center,
        baseline_interval,
        effect,
        distinguishable,
    }
}

fn center(successes: u32, n: u32) -> f64 {
    let k = successes as f64;
    let n = n as f64;
    let z2 = Z_95 * Z_95;
    (k + z2 / 2.0) / (n + z2)
}

fn wilson(successes: u32, n: u32) -> (f64, f64) {
    if n == 0 {
        return (0.0, 1.0);
    }
    let n = n as f64;
    let p = successes as f64 / n;
    let z2 = Z_95 * Z_95;
    let denom = 1.0 + z2 / n;
    let center = (p + z2 / (2.0 * n)) / denom;
    let margin = Z_95 * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt() / denom;
    (center - margin, center + margin)
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_regression_is_distinguishable() {
        // 48/50 vs 0/50: at N=50 the intervals are narrow enough not to overlap.
        let comparison = compare(
            Score {
                successes: 48,
                n: 50,
            },
            Score {
                successes: 0,
                n: 50,
            },
        );
        assert!(comparison.distinguishable);
        assert!(comparison.effect > 0.0);
    }

    #[test]
    fn noisy_small_samples_are_not_distinguishable() {
        // 3/5 vs 2/5 at N=5: intervals are wide and overlap — no verdict.
        let comparison = compare(Score { successes: 3, n: 5 }, Score { successes: 2, n: 5 });
        assert!(!comparison.distinguishable);
    }

    #[test]
    fn interval_widens_at_small_n() {
        let wide = Score { successes: 1, n: 2 }.interval();
        let narrow = Score {
            successes: 50,
            n: 100,
        }
        .interval();
        assert!((wide.1 - wide.0) > (narrow.1 - narrow.0));
    }
}

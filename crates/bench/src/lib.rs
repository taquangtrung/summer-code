//! Agent scoring. The model is nondeterministic, so a single run asserts
//! nothing: a task that fails once in five is not a regression. [`Score`] holds a
//! rate with its interval; [`compare`] reports an effect size against a baseline
//! and whether the two are distinguishable from noise. The runner that actually
//! calls the model is deferred (`run`); the statistics here are pure and tested
//! offline.

// Harness-driven outcome checks (reading result files, running the success
// command), not agent-driven work.
#![allow(clippy::disallowed_methods)]

mod ablate;
mod baseline;
mod quality;
mod replay;
mod report;
mod run;
mod score;
mod task;

pub use ablate::{ALL_ON, ArmReport, render_matrix};
pub use baseline::{Baseline, Drift, TaskScore};
pub use quality::{Metrics, Summary, summarize};
pub use replay::{Divergence, Trace, agreement, diff};
pub use report::{Report, render, render_run};
pub use run::{Attempt, RunReport, RunSetup, run_task};
pub use score::{Score, compare};
pub use task::{SkillRequirement, Success, Task, TaskSet};

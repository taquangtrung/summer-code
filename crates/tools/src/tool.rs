//! The tool trait and its specification.
//!
//! A spec is loaded from a checked-in file under `tools/`, not built inline, for
//! the reason the description field states: the description is what the model
//! routes on, which makes it a tuning surface rather than code. Kept as files
//! they diff on their own, and `tools_digest` moves for a change a reviewer can
//! read; kept inline, a wording change is buried in whatever code change carried
//! it.

use std::time::Duration;

use async_trait::async_trait;
use journal::Outcome;
use sandbox::ToolCx;
use serde_json::Value;

// ========================================================================
// Data Structures
// ========================================================================

/// A tool's name, description, and JSON Schema for its arguments. The
/// description is load-bearing: it is how the model routes.
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Whether a tool may run at the same time as its neighbours in one batch.
///
/// **The default is `Serial`, and the direction of that default is the whole
/// safety argument.** A tool declares itself parallel-safe; the dispatcher never
/// infers it. A new tool that mutates the workspace and says nothing is run
/// alone, which is the behaviour every tool had before batching existed. The
/// cost of a wrong default in this direction is latency; in the other direction
/// it is two concurrent writers to one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Concurrency {
    /// Observes the workspace without changing it, and holds no lock another
    /// call in the batch could want. Safe to interleave with any other
    /// `Parallel` call.
    Parallel,
    /// Runs alone. Mutates the workspace, spawns a process, or depends on what
    /// an earlier call in the same batch did.
    Serial,
}

// ========================================================================
// ToolSpec Implementations
// ========================================================================

impl ToolSpec {
    /// Parse a spec from the text of a `tools/*.json` file.
    ///
    /// **Panicking is the right failure here, and the reason is where the input
    /// comes from.** Every caller passes an `include_str!` of a file compiled
    /// into this crate, so a parse failure is a malformed artifact in the
    /// repository, not a condition any runtime input can reach. Returning a
    /// `Result` would push a branch onto every construction site that no caller
    /// could act on, and `Tool::spec` has nowhere to report it. The
    /// `every_checked_in_schema_loads` test is what makes the failure a red
    /// `make check` instead of a panic in front of a user.
    pub fn load(source: &str) -> Self {
        let value: Value = serde_json::from_str(source).expect("a tools/*.json file parses");
        let field = |key: &str| -> String {
            value
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("a tools/*.json file has a string `{key}`"))
                .to_string()
        };
        Self {
            name: field("name"),
            description: field("description"),
            input_schema: value
                .get("input_schema")
                .cloned()
                .expect("a tools/*.json file has an `input_schema`"),
        }
    }
}

// ========================================================================
// Traits
// ========================================================================

/// One tool. `call` returns an [`Outcome`]; ordinary errors flow to the model
/// as tool results, which is how an agent recovers from a failed `cargo build`.
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;
    async fn call(&self, args: Value, cx: &ToolCx) -> Outcome;

    /// Whether this tool may share a batch with its neighbours. Defaults to
    /// [`Concurrency::Serial`]: a tool opts *in* to being run alongside others.
    fn concurrency(&self) -> Concurrency {
        Concurrency::Serial
    }

    /// How long this call may run, or `None` for the dispatcher's default.
    ///
    /// Per call rather than per tool, because the cost lives in the arguments:
    /// `shell` running `ls` and `shell` running `make check` are the same tool,
    /// and one cap sized for the first kills the second halfway through a build.
    fn timeout(&self, _args: &Value) -> Option<Duration> {
        None
    }
}

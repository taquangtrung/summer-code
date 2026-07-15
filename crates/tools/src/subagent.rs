//! The `subagent` tool: run a sub-task in a session of its own and return only
//! what it concluded.
//!
//! **This tool used to be a lie, and the shape of the fix is the interesting
//! part.** It formatted its own arguments into "Executed delegated task in
//! isolated context: `<prompt>`" and returned `Ok`. Nothing ran. The model
//! received its own prompt back as a finding, with no way to tell, and a session
//! that leaned on it produced confident conclusions drawn from nothing.
//!
//! Running a real one needs an engine, and `engine` depends on this crate, so
//! the work cannot live here. It arrives as an [`Arc<dyn SubagentRunner>`] the
//! frontend supplies, exactly as [`SkillLookup`](crate::SkillLookup) does for
//! skill bodies. That inversion is also what makes the honest failure mode
//! possible: a frontend with no runner does not register the tool at all, so a
//! bench or a test sees a tool set without it rather than a tool that pretends.
//!
//! **The isolation is the point, and it is the caller's whole reason to use
//! this.** The sub-session gets its own context window and its own journal, and
//! only its final answer crosses back. A search that read forty files to answer
//! one question costs the parent one paragraph instead of forty files of
//! context.

use std::sync::Arc;

use async_trait::async_trait;
use journal::Outcome;
use sandbox::ToolCx;
use serde_json::Value;

use crate::tool::{Tool, ToolSpec};

// ========================================================================
// Constants
// ========================================================================

const SUBAGENT_SCHEMA: &str = include_str!("../tools/subagent.json");

/// What a sub-task is called when the caller names no role.
const DEFAULT_ROLE: &str = "Subagent";

// ========================================================================
// Traits
// ========================================================================

/// Runs a prompt in a session isolated from the caller's.
///
/// The frontend implements this, because building an engine needs every seam an
/// engine has and this crate has none of them.
#[async_trait]
pub trait SubagentRunner: Send + Sync {
    /// Run `prompt` as `role` and return the sub-session's final answer.
    ///
    /// `Err` is the sub-session failing to run at all. A sub-session that ran
    /// and concluded little still answers `Ok`, because "it found nothing" is a
    /// finding and the caller has to be able to tell it from a broken harness.
    async fn run(&self, prompt: &str, role: &str) -> Result<String, String>;
}

// ========================================================================
// Data Structures
// ========================================================================

/// Delegate a sub-task to a session with its own context window.
pub struct SubagentTool {
    runner: Arc<dyn SubagentRunner>,
    spec: ToolSpec,
}

// ========================================================================
// SubagentTool Implementations
// ========================================================================

impl SubagentTool {
    pub fn new(runner: Arc<dyn SubagentRunner>) -> Self {
        Self {
            runner,
            spec: ToolSpec::load(SUBAGENT_SCHEMA),
        }
    }
}

#[async_trait]
impl Tool for SubagentTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, _cx: &ToolCx) -> Outcome {
        let Some(prompt) = args.get("prompt").and_then(Value::as_str) else {
            return Outcome::Err {
                message: "subagent requires a `prompt` string".into(),
            };
        };
        let role = args
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_ROLE);

        match self.runner.run(prompt, role).await {
            Ok(finding) => Outcome::Ok { content: finding },
            // The sub-session failing is an error, not an empty finding. The
            // difference decides whether the caller retries or believes it.
            Err(message) => Outcome::Err {
                message: format!("subagent `{role}` did not run: {message}"),
            },
        }
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Instant;

    use sandbox::Broker;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::*;

    /// Records what it was asked, and answers with whatever it was given.
    struct Fake {
        answer: Result<String, String>,
        seen: Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl SubagentRunner for Fake {
        async fn run(&self, prompt: &str, role: &str) -> Result<String, String> {
            self.seen
                .lock()
                .expect("lock")
                .push((prompt.to_string(), role.to_string()));
            self.answer.clone()
        }
    }

    fn cx() -> ToolCx {
        let root = std::env::temp_dir();
        ToolCx::new(
            Arc::new(Broker::permissive(root.clone())),
            Arc::from(root.as_path()),
            CancellationToken::new(),
            Instant::now(),
        )
    }

    #[tokio::test]
    async fn test_the_finding_returned_is_the_runners_own() {
        // The bug this replaces: the tool formatted its arguments into a success
        // message and never ran anything, so the caller read its own prompt back
        // as a conclusion.
        let runner = Arc::new(Fake {
            answer: Ok("three callers, all in src/net".into()),
            seen: Mutex::new(Vec::new()),
        });
        let outcome = SubagentTool::new(runner.clone())
            .call(
                json!({"prompt": "find the callers", "role": "Researcher"}),
                &cx(),
            )
            .await;

        match outcome {
            Outcome::Ok { content } => assert_eq!(content, "three callers, all in src/net"),
            other => panic!("expected the runner's finding, got {other:?}"),
        }
        assert_eq!(
            runner.seen.lock().expect("lock").as_slice(),
            [("find the callers".to_string(), "Researcher".to_string())],
            "the runner must receive the caller's prompt, not a paraphrase"
        );
    }

    #[tokio::test]
    async fn test_a_sub_session_that_could_not_run_is_an_error() {
        // Not an empty finding: the caller has to be able to tell "it looked and
        // found nothing" from "it never looked".
        let runner = Arc::new(Fake {
            answer: Err("no credential".into()),
            seen: Mutex::new(Vec::new()),
        });
        let outcome = SubagentTool::new(runner)
            .call(json!({"prompt": "find the callers"}), &cx())
            .await;

        assert!(
            matches!(outcome, Outcome::Err { ref message } if message.contains("no credential")),
            "got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn test_a_call_with_no_prompt_is_refused_before_anything_runs() {
        let runner = Arc::new(Fake {
            answer: Ok("unreachable".into()),
            seen: Mutex::new(Vec::new()),
        });
        let outcome = SubagentTool::new(runner.clone())
            .call(json!({"role": "Researcher"}), &cx())
            .await;

        assert!(matches!(outcome, Outcome::Err { .. }), "got {outcome:?}");
        assert!(
            runner.seen.lock().expect("lock").is_empty(),
            "a malformed call must not start a sub-session"
        );
    }
}

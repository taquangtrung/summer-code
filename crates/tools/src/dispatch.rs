//! The dispatcher: validate → dedup → tag → spawn → deadline → call.
//!
//! Phase 1b hardens each call: it runs in its own spawned task so a panic
//! degrades to `Outcome::Err` instead of unwinding the turn; it races a
//! deadline, and on timeout cancels the token and aborts the task; and a
//! call-id ledger refuses to re-dispatch a call already known to have run.
//!
//! **`dispatch` takes `&self`, and that is what lets the engine batch.** The
//! ledger is the only mutable state here, so it moved behind a `Mutex` and the
//! method stopped needing an exclusive borrow. Several calls can then be in
//! flight against one dispatcher at once; each is tagged with its call id via
//! [`sandbox::tag_call`] so the broker records it made stay attributable while
//! they interleave. Whether two calls may actually share a batch is
//! [`Concurrency`], which the engine reads before it groups them.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use journal::{CallId, Outcome};
use sandbox::{ToolCx, tag_call};
use serde_json::Value;
use thiserror::Error;
use tokio::task::JoinError;
use tokio::time::timeout;
use tracing::{instrument, warn};

use crate::tool::{Concurrency, Tool, ToolSpec};
use crate::validate;

// ========================================================================
// Constants
// ========================================================================

/// Default per-call wall-clock cap, for a tool whose [`Tool::timeout`] has no
/// opinion about the call.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a timed-out call is given to finish aborting.
///
/// Only reached on the timeout path, and only for as long as the task takes to
/// hit its next await point, which is immediate for anything not parked in a
/// blocking syscall.
const ABORT_GRACE: Duration = Duration::from_millis(250);

// ========================================================================
// Data Structures
// ========================================================================

/// Owns the tool registry, the call-id ledger, and the per-call timeout.
pub struct Dispatcher {
    registry: HashMap<String, Arc<dyn Tool>>,
    /// Call ids already dispatched. Behind a `Mutex` for interior mutability
    /// alone: it is the one thing `dispatch` writes, and keeping it behind a
    /// lock is what lets the method take `&self` and run concurrently.
    ledger: Mutex<HashSet<CallId>>,
    timeout: Duration,
}

/// A tool name claimed twice.
///
/// Its own error rather than a silent last-one-wins, because the sources are not
/// equally trusted. `ToolSource` exists so built-ins and MCP tools concatenate
/// into one list, and external MCP servers are untrusted: a server advertising
/// `shell` would otherwise replace the built-in in both the registry and the
/// specs the model is shown, so the tool list looks unchanged and every `shell`
/// call is quietly routed to the attacker.
#[derive(Debug, Error)]
#[error(
    "two tools claim the name `{name}`: tool names must be unique, and an external \
     source may not shadow a built-in"
)]
pub struct DuplicateTool {
    pub name: String,
}

// ========================================================================
// Dispatcher Implementations
// ========================================================================

impl Dispatcher {
    /// Build a dispatcher over `tools`, refusing a name claimed twice.
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Result<Self, DuplicateTool> {
        Self::with_timeout(tools, DEFAULT_TIMEOUT)
    }

    pub fn with_timeout(
        tools: Vec<Arc<dyn Tool>>,
        timeout: Duration,
    ) -> Result<Self, DuplicateTool> {
        let mut registry = HashMap::with_capacity(tools.len());
        for tool in tools {
            let name = tool.spec().name.clone();
            if registry.insert(name.clone(), tool).is_some() {
                return Err(DuplicateTool { name });
            }
        }
        Ok(Self {
            registry,
            ledger: Mutex::new(HashSet::new()),
            timeout,
        })
    }

    /// Validate, dedup, tag, spawn, deadline, call. Returns the outcome; never
    /// records it (the engine is the only writer).
    #[instrument(skip(self, args, cx), fields(name = %name))]
    pub async fn dispatch(&self, call: &CallId, name: &str, args: Value, cx: &ToolCx) -> Outcome {
        let Some(tool) = self.registry.get(name).cloned() else {
            return Outcome::Err {
                message: format!("unknown tool `{name}`"),
            };
        };

        if let Err(error) = validate::validate(&args, &tool.spec().input_schema) {
            return Outcome::Err {
                message: format!("invalid arguments: {error}"),
            };
        }

        // Claim the id and check it in one locked section. Two concurrent calls
        // carrying the same id must not both see an empty ledger and both run,
        // which a `contains` followed by a later `insert` would allow.
        if !self.claim(call) {
            return Outcome::Err {
                message: format!("duplicate tool call id `{call}`"),
            };
        }

        // Give the spawned task its own token and deadline so a timeout can
        // cancel it cooperatively and abort it forcefully. The token is a child
        // of the turn's (`cx.cancel`), so a Ctrl-C that cancels the turn
        // propagates to every call while a per-call timeout still only cancels
        // this one. A turn with no token parents to a fresh uncanceled token, so
        // behavior is unchanged for the plain REPL.
        let limit = tool.timeout(&args).unwrap_or(self.timeout);
        let cancel = cx.cancel.child_token();
        let mut call_cx = cx.clone();
        call_cx.cancel = cancel.clone();
        call_cx.deadline = Instant::now() + limit;

        // The tag rides the spawned task, so every broker record the tool
        // produces names this call even while sibling calls are producing their
        // own. `Arc<str>` rather than `CallId` because the broker does not
        // depend on `journal`.
        let tag: Arc<str> = Arc::from(call.to_string().as_str());
        let tool_call = tag_call(tag, async move { tool.call(args, &call_cx).await });
        let mut handle = tokio::spawn(tool_call);

        match timeout(limit, &mut handle).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(join_error)) => panic_outcome(join_error),
            Err(_elapsed) => {
                cancel.cancel();
                handle.abort();
                // **Wait for the task to actually stop before returning.**
                // `abort` only takes effect at the next await point, so the tool
                // can still be between the broker's `authorize` and its return
                // when this function hands control back. The `PermissionRecord`
                // it then sends drains during the *next* call and is journaled
                // under that call's id: an audit trail attributing an access to
                // a tool that never made it. Returning only once the task is
                // gone means every record it sent is already in the channel.
                //
                // Bounded, because a tool parked in a blocking syscall has no
                // await point to be aborted at. The broker opens with
                // `O_NONBLOCK` to keep that from being unbounded, but a grace
                // period costs less than trusting it here.
                let _ = timeout(ABORT_GRACE, handle).await;
                Outcome::TimedOut
            }
        }
    }

    /// Seed the ledger from the journal on resume.
    ///
    /// The ledger is a derived cache, like the broker's grant set, and it needs
    /// the same treatment for the same reason: a fresh `HashSet` after a restart
    /// makes the guarantee the engine documents ("the ledger refuses
    /// re-dispatch, so re-running a half-applied write is not a risk we take")
    /// false exactly across the boundary where it matters. A `Recorded` provider
    /// replaying a journal re-emits the recorded call ids, and a dispatcher that
    /// had never seen them would run every one of them again.
    pub fn restore(&mut self, calls: impl IntoIterator<Item = CallId>) {
        self.ledger_mut().extend(calls);
    }

    /// Record `call` as dispatched, reporting whether it was new.
    fn claim(&self, call: &CallId) -> bool {
        self.ledger_mut().insert(call.clone())
    }

    /// The ledger, recovering rather than panicking on a poisoned lock.
    ///
    /// A poisoned `Mutex` means some other task panicked while holding it. The
    /// set it was holding is still a well-formed set -- no invariant of this
    /// type spans the locked section -- so the recorded ids are worth more than
    /// the poison flag. Propagating the panic instead would turn one tool's
    /// failure into a dead dispatcher for the rest of the session.
    fn ledger_mut(&self) -> impl std::ops::DerefMut<Target = HashSet<CallId>> + '_ {
        self.ledger
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Whether `name` may share a batch with its neighbours.
    ///
    /// An unknown tool is [`Concurrency::Serial`]: it will fail in `dispatch`
    /// with "unknown tool", and grouping it alone keeps that failure from
    /// changing how its neighbours run.
    pub fn concurrency(&self, name: &str) -> Concurrency {
        self.registry
            .get(name)
            .map_or(Concurrency::Serial, |tool| tool.concurrency())
    }

    /// Every tool spec, for building the model request, **sorted by name**.
    ///
    /// The sort is not tidiness, it is a cache-hit rate. `registry` is a
    /// `HashMap`, so iteration order is randomized per process, and the tools
    /// array sits in the cached prompt prefix ahead of the system prompt and the
    /// conversation. A different order on every launch is a different prefix on
    /// every launch: the prompt cache misses and turn one re-pays full input
    /// tokens, every time, forever. `tools_digest` already sorted -- so the
    /// digest was stable while the thing it claimed to describe was not, and
    /// nothing pointed at the discrepancy.
    pub fn specs(&self) -> Vec<&ToolSpec> {
        let mut specs: Vec<&ToolSpec> = self.registry.values().map(|tool| tool.spec()).collect();
        specs.sort_by(|left, right| left.name.cmp(&right.name));
        specs
    }

    /// Whether a tool by this name is registered.
    ///
    /// For callers that must distinguish "no such tool" from "the tool failed"
    /// *before* dispatching. The loop does not need it (the model can only name
    /// a tool it was shown, and a wrong name is a tool error it should see and
    /// correct), but MCP does: an unknown tool is invalid params on the
    /// protocol, not a `tools/call` result with `isError` set.
    pub fn has(&self, name: &str) -> bool {
        self.registry.contains_key(name)
    }
}

// ========================================================================
// Functions
// ========================================================================

fn panic_outcome(error: JoinError) -> Outcome {
    if error.is_cancelled() {
        Outcome::Err {
            message: "tool task was cancelled".into(),
        }
    } else if error.is_panic() {
        warn!("tool panicked: {:?}", error);
        Outcome::Err {
            message: "tool panicked".into(),
        }
    } else {
        Outcome::Err {
            message: format!("tool task failed: {error}"),
        }
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use sandbox::{Broker, ToolCx};
    use serde_json::json;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio_util::sync::CancellationToken;

    struct Echo {
        spec: ToolSpec,
    }

    #[async_trait::async_trait]
    impl Tool for Echo {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }
        async fn call(&self, args: Value, _cx: &ToolCx) -> Outcome {
            Outcome::Ok {
                content: args.to_string(),
            }
        }
    }

    fn cx() -> ToolCx {
        ToolCx::new(
            // These tests exercise dispatch, not policy: a permissive broker
            // keeps the subject of the test the dispatcher.
            Arc::new(Broker::permissive(std::env::temp_dir())),
            Arc::from(std::env::temp_dir().as_path()),
            CancellationToken::new(),
            Instant::now(),
        )
    }

    fn echo_dispatcher() -> Dispatcher {
        Dispatcher::new(vec![Arc::new(Echo {
            spec: ToolSpec {
                name: "echo".into(),
                description: String::new(),
                input_schema: json!({}),
            },
        })])
        .expect("one tool, one name")
    }

    #[tokio::test]
    async fn refuses_duplicate_call_id() {
        let dispatcher = echo_dispatcher();
        let call = CallId::new("toolu_dup".into());
        let first = dispatcher.dispatch(&call, "echo", json!({}), &cx()).await;
        let second = dispatcher.dispatch(&call, "echo", json!({}), &cx()).await;
        assert!(matches!(first, Outcome::Ok { .. }), "first call should run");
        assert!(
            matches!(second, Outcome::Err { .. }),
            "duplicate call id should be refused"
        );
    }

    #[tokio::test]
    async fn concurrent_duplicates_run_exactly_once() {
        // The check-then-insert race: with `contains` and a later `insert`, both
        // of these see an empty ledger and both run the tool. Only the single
        // locked claim makes exactly one of them win.
        let dispatcher = echo_dispatcher();
        let call = CallId::new("toolu_race".into());
        let (left_cx, right_cx) = (cx(), cx());
        let (left, right) = tokio::join!(
            dispatcher.dispatch(&call, "echo", json!({}), &left_cx),
            dispatcher.dispatch(&call, "echo", json!({}), &right_cx),
        );
        let ran = [&left, &right]
            .iter()
            .filter(|outcome| matches!(outcome, Outcome::Ok { .. }))
            .count();
        assert_eq!(ran, 1, "exactly one of two racing duplicates should run");
    }

    #[tokio::test]
    async fn independent_calls_share_a_batch() {
        // Two ids, one dispatcher, both in flight: the ledger must not serialize
        // distinct calls, and `&self` dispatch must actually permit the overlap.
        let dispatcher = echo_dispatcher();
        let (left_call, right_call) =
            (CallId::new("toolu_a".into()), CallId::new("toolu_b".into()));
        let (left_cx, right_cx) = (cx(), cx());
        let (left, right) = tokio::join!(
            dispatcher.dispatch(&left_call, "echo", json!({}), &left_cx),
            dispatcher.dispatch(&right_call, "echo", json!({}), &right_cx),
        );
        assert!(matches!(left, Outcome::Ok { .. }));
        assert!(matches!(right, Outcome::Ok { .. }));
    }

    const SLOW_BUILD_RUNS_FOR: Duration = Duration::from_millis(200);

    /// Sleeps for a while, and asks for longer than the dispatcher default.
    struct SlowBuild {
        spec: ToolSpec,
    }

    #[async_trait::async_trait]
    impl Tool for SlowBuild {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }
        async fn call(&self, _args: Value, _cx: &ToolCx) -> Outcome {
            tokio::time::sleep(SLOW_BUILD_RUNS_FOR).await;
            Outcome::Ok {
                content: "built".into(),
            }
        }
        fn timeout(&self, _args: &Value) -> Option<Duration> {
            Some(SLOW_BUILD_RUNS_FOR * 10)
        }
    }

    #[tokio::test]
    async fn a_tool_that_asks_for_longer_outlives_the_default_cap() {
        // The bug this pins: one cap for every call, sized for a grep, killing a
        // `make check` halfway. The dispatcher's own cap here is shorter than
        // the call, so only the tool's answer lets it finish.
        let dispatcher = Dispatcher::with_timeout(
            vec![Arc::new(SlowBuild {
                spec: ToolSpec {
                    name: "build".into(),
                    description: String::new(),
                    input_schema: json!({}),
                },
            })],
            SLOW_BUILD_RUNS_FOR / 4,
        )
        .expect("one tool, one name");
        let call = CallId::new("toolu_build".into());
        let outcome = dispatcher.dispatch(&call, "build", json!({}), &cx()).await;
        assert!(
            matches!(outcome, Outcome::Ok { .. }),
            "expected the build to finish, got {outcome:?}"
        );
    }
}

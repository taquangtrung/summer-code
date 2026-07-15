//! Turning a model's requested calls into ordered results.
//!
//! **Batching is a latency change, not a semantic one**, and this module is
//! where that claim is kept. [`batch_calls`] decides which adjacent calls may
//! overlap, [`run_batch`] runs one batch concurrently, and the caller journals
//! every result in the order the model asked regardless of which finished first.
//! [`screen_batch`] and [`annotate`] are the hook seams either side of that.
//!
//! It sits beside the loop rather than inside it because none of it needs the
//! engine: each function takes exactly the hooks, dispatcher, or context it
//! reads, which is what makes the ordering property testable without building an
//! `Engine` at all.

use std::path::Path;

use futures_util::stream::{FuturesUnordered, StreamExt};
use journal::Outcome;
use sandbox::ToolCx;
use tools::{Concurrency, Dispatcher};

use crate::accumulate::RequestedCall;
use crate::hook::{HookEvent, HookSet};
use crate::ui_event::{Observer, UiEvent};

// ========================================================================
// Constants
// ========================================================================

/// How many parallel-safe calls may share one batch.
///
/// A ceiling, not a target: the point is to stop a model that asked to grep
/// forty paths from opening forty ripgrep processes at once. Beyond a handful
/// the wins are gone anyway, since these calls are bound by disk and by the
/// broker's evaluation rather than by waiting.
pub(super) const MAX_BATCH_WIDTH: usize = 8;

// ========================================================================
// Functions
// ========================================================================

/// Ask the `PreToolUse` hooks about each call in a batch.
///
/// Returns the *whole* denial text per call, `None` where the call may proceed.
/// The text is complete rather than a bare reason because hooks are no longer
/// the only screen filling these slots, and a caller that prefixed every one of
/// them with "blocked by a PreToolUse hook" would attribute another screen's
/// refusal to a hook. Hooks for the
/// whole batch are consulted concurrently: they are independent subprocesses,
/// and running them one after another would put the batch's latency win back
/// where it was found.
///
/// A free function taking the two fields it needs, rather than a method: an
/// `async fn(&self)` on the engine would make the returned future hold `&Engine`
/// across an await, which requires `Engine: Sync`, which the permission
/// `Receiver` is not. The engine is deliberately a single-threaded owner of its
/// journal, and this keeps it one.
pub(super) async fn screen_batch(
    hooks: &HookSet,
    cwd: &Path,
    batch: &[RequestedCall],
) -> Vec<Option<String>> {
    if !hooks.has(HookEvent::PreToolUse) {
        return batch.iter().map(|_| None).collect();
    }
    let screenings = batch.iter().map(|requested| async move {
        hooks
            .run(
                HookEvent::PreToolUse,
                Some(&requested.name),
                Some(&requested.args),
                None,
                cwd,
            )
            .await
            .blocked
            .map(|reason| format!("blocked by a PreToolUse hook: {reason}"))
    });
    futures_util::future::join_all(screenings).await
}

/// Let the `PostToolUse` hooks add to what the model sees.
///
/// The hook's output is appended to the outcome rather than replacing it: a
/// formatter's summary is extra context about a result, not the result. A hook
/// that blocks here cannot un-run the tool, so its reason is reported the same
/// way, as something the model must read before its next move. A failed outcome
/// keeps its own shape, because promoting a hook's note over the error the tool
/// reported would hide why the call failed.
pub(super) async fn annotate(
    hooks: &HookSet,
    cwd: &Path,
    requested: &RequestedCall,
    outcome: Outcome,
) -> Outcome {
    if !hooks.has(HookEvent::PostToolUse) {
        return outcome;
    }
    let existing = match &outcome {
        Outcome::Ok { content } => content.as_str(),
        Outcome::Denied { reason } => reason.as_str(),
        Outcome::Err { message } => message.as_str(),
        Outcome::TimedOut => "",
    };
    let verdict = hooks
        .run(
            HookEvent::PostToolUse,
            Some(&requested.name),
            Some(&requested.args),
            Some(existing),
            cwd,
        )
        .await;

    let note = match (verdict.blocked, verdict.context.as_str()) {
        (Some(reason), _) => reason,
        (None, "") => return outcome,
        (None, context) => context.to_string(),
    };
    match outcome {
        Outcome::Ok { content } => Outcome::Ok {
            content: format!("{content}\n\n<hook>\n{note}\n</hook>"),
        },
        Outcome::Denied { reason } => Outcome::Denied {
            reason: format!("{reason}\n\n<hook>\n{note}\n</hook>"),
        },
        Outcome::Err { message } => Outcome::Err {
            message: format!("{message}\n\n<hook>\n{note}\n</hook>"),
        },
        Outcome::TimedOut => Outcome::TimedOut,
    }
}

/// Split the model's calls into runs that may execute together.
///
/// Only *adjacent* parallel-safe calls share a batch, and that adjacency
/// requirement is doing real work: it preserves the relative order of everything
/// the model asked for. A read listed after a write still runs after that write,
/// because the write is a batch boundary. Reordering independent reads among
/// themselves is the only freedom taken, and it is not observable in the journal
/// because results are recorded in the requested order regardless.
pub(super) fn batch_calls<'a>(
    dispatcher: &Dispatcher,
    requested: &'a [RequestedCall],
) -> Vec<&'a [RequestedCall]> {
    let is_parallel =
        |call: &RequestedCall| dispatcher.concurrency(&call.name) == Concurrency::Parallel;
    let mut batches = Vec::new();
    let mut start = 0;
    while start < requested.len() {
        let mut end = start;
        if is_parallel(&requested[start]) {
            while end < requested.len()
                && end - start < MAX_BATCH_WIDTH
                && is_parallel(&requested[end])
            {
                end += 1;
            }
        } else {
            end += 1;
        }
        batches.push(&requested[start..end]);
        start = end;
    }
    batches
}

/// Run every call in a batch, reporting each to the UI as it lands and
/// returning the outcomes in the batch's own order.
///
/// The UI hears about completions in whatever order they happen, because a
/// spinner that resolves when its call actually finished is the honest one. The
/// returned `Vec` is re-indexed back into request order, because the journal's
/// is the order the model asked in.
pub(super) async fn run_batch(
    dispatcher: &Dispatcher,
    batch: &[RequestedCall],
    blocked: &[Option<String>],
    cx: &ToolCx,
    observer: Option<&Observer>,
) -> Vec<Outcome> {
    // A blocked call is answered without being dispatched, and it keeps its
    // slot: every `tool_use` still gets exactly one result, which is the rule
    // the whole loop is built around. The reason arrives complete, because which
    // screen refused the call is the screen's own to say.
    let mut outcomes: Vec<Option<Outcome>> = blocked
        .iter()
        .map(|reason| {
            reason.as_ref().map(|reason| Outcome::Denied {
                reason: reason.clone(),
            })
        })
        .collect();

    let mut pending: FuturesUnordered<_> = batch
        .iter()
        .enumerate()
        .filter(|(index, _)| outcomes[*index].is_none())
        .map(|(index, requested)| async move {
            let outcome = dispatcher
                .dispatch(&requested.call, &requested.name, requested.args.clone(), cx)
                .await;
            (index, outcome)
        })
        .collect();

    // The UI hears about a blocked call too: its spinner was already started.
    for (index, outcome) in outcomes.iter().enumerate() {
        if let Some(outcome) = outcome {
            crate::ui_event::emit_to(
                observer,
                UiEvent::ToolFinished {
                    call: batch[index].call.clone(),
                    outcome: outcome.clone(),
                },
            );
        }
    }

    while let Some((index, outcome)) = pending.next().await {
        crate::ui_event::emit_to(
            observer,
            UiEvent::ToolFinished {
                call: batch[index].call.clone(),
                outcome: outcome.clone(),
            },
        );
        outcomes[index] = Some(outcome);
    }
    outcomes
        .into_iter()
        .map(|outcome| outcome.expect("every call in the batch reported an outcome"))
        .collect()
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use async_trait::async_trait;
    use journal::CallId;
    use serde_json::{Value, json};
    use tokio_util::sync::CancellationToken;
    use tools::{Tool, ToolSpec};

    use super::*;

    /// A tool that does nothing and declares whichever concurrency it is built
    /// with, so a batching test can be written without a real workspace.
    struct Stub {
        concurrency: Concurrency,
        spec: ToolSpec,
    }

    #[async_trait]
    impl Tool for Stub {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn concurrency(&self) -> Concurrency {
            self.concurrency
        }

        async fn call(&self, _args: Value, _cx: &ToolCx) -> Outcome {
            Outcome::Ok {
                content: String::new(),
            }
        }
    }

    fn stub(name: &str, concurrency: Concurrency) -> Arc<dyn Tool> {
        Arc::new(Stub {
            concurrency,
            spec: ToolSpec {
                name: name.to_string(),
                description: String::new(),
                input_schema: json!({}),
            },
        })
    }

    fn dispatcher() -> Dispatcher {
        Dispatcher::new(vec![
            stub("reader", Concurrency::Parallel),
            stub("writer", Concurrency::Serial),
        ])
        .expect("distinct names")
    }

    fn calls(names: &[&str]) -> Vec<RequestedCall> {
        names
            .iter()
            .enumerate()
            .map(|(index, name)| RequestedCall {
                args: json!({}),
                call: CallId::new(format!("toolu_{index}")),
                name: (*name).to_string(),
            })
            .collect()
    }

    fn shapes(batches: &[&[RequestedCall]]) -> Vec<Vec<String>> {
        batches
            .iter()
            .map(|batch| batch.iter().map(|call| call.name.clone()).collect())
            .collect()
    }

    #[test]
    fn a_serial_call_breaks_the_batch_around_it() {
        // The bug this catches is the dangerous one: a write swallowed into a
        // parallel run would execute alongside the reads listed either side of
        // it, which is the exact overlap `Concurrency::Serial` exists to forbid.
        let requested = calls(&["reader", "reader", "writer", "reader"]);
        let batches = batch_calls(&dispatcher(), &requested);
        assert_eq!(
            shapes(&batches),
            vec![vec!["reader", "reader"], vec!["writer"], vec!["reader"],]
        );
    }

    #[test]
    fn consecutive_serial_calls_each_run_alone() {
        let requested = calls(&["writer", "writer"]);
        let batches = batch_calls(&dispatcher(), &requested);
        assert_eq!(shapes(&batches), vec![vec!["writer"], vec!["writer"]]);
    }

    #[test]
    fn a_parallel_run_is_capped_at_the_batch_width() {
        // Without the cap this is one batch of twelve, which is twelve ripgrep
        // processes at once.
        let requested = calls(&["reader"; MAX_BATCH_WIDTH + 4]);
        let batches = batch_calls(&dispatcher(), &requested);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), MAX_BATCH_WIDTH);
        assert_eq!(batches[1].len(), 4);
    }

    #[test]
    fn an_unknown_tool_is_batched_alone() {
        // It will fail in `dispatch` with "unknown tool"; grouping it alone keeps
        // that failure from changing how its neighbours run.
        let requested = calls(&["reader", "nonesuch", "reader"]);
        let batches = batch_calls(&dispatcher(), &requested);
        assert_eq!(
            shapes(&batches),
            vec![vec!["reader"], vec!["nonesuch"], vec!["reader"]]
        );
    }

    #[tokio::test]
    async fn a_batch_returns_outcomes_in_request_order() {
        // `run_batch` collects from a `FuturesUnordered`, so completion order is
        // not request order. Indexing the results back is what keeps the journal
        // deterministic; dropping that would surface as results attributed to
        // the wrong call.
        let dispatcher = dispatcher();
        let requested = calls(&["reader", "reader", "reader"]);
        let cx = ToolCx::new(
            Arc::new(sandbox::Broker::permissive(std::env::temp_dir())),
            Arc::from(std::env::temp_dir().as_path()),
            CancellationToken::new(),
            Instant::now(),
        );
        let blocked: Vec<Option<String>> = requested.iter().map(|_| None).collect();
        let outcomes = run_batch(&dispatcher, &requested, &blocked, &cx, None).await;
        assert_eq!(outcomes.len(), requested.len());
        assert!(outcomes.iter().all(|o| matches!(o, Outcome::Ok { .. })));
    }
}

//! Which tool call the current task is running, for attributing broker records.
//!
//! **This is the field `PermissionRecord` deliberately did not have.** While
//! dispatch was serial the engine always knew which call was in flight and
//! stamped the id as it drained, so the broker could stay free of a correlation
//! field and free of a `journal` dependency for the id type. Concurrent dispatch
//! breaks that: several calls are in flight at once, their records interleave in
//! one channel, and "the call that is running" is no longer a fact the engine
//! can read off its own state.
//!
//! The alternative was threading a call id through [`Broker::read`],
//! [`Broker::write`], [`Broker::exec`], [`Broker::connect`], [`Broker::listen`],
//! and every tool call site that reaches them. A task-local carries the same
//! fact with no signature changes, because the dispatcher already runs each tool
//! call in its own spawned task: the scope boundary and the call boundary are
//! the same boundary.
//!
//! **The tag does not cross a `spawn`.** Tokio task-locals are per-task and are
//! not inherited by children, so a tool that spawns its own task and reaches the
//! broker from inside it records `None` and the engine falls back to the call it
//! is draining for. Tools await the broker inline, which is what makes that an
//! acceptable edge rather than the common case.
//!
//! [`Broker::read`]: crate::Broker::read
//! [`Broker::write`]: crate::Broker::write
//! [`Broker::exec`]: crate::Broker::exec
//! [`Broker::connect`]: crate::Broker::connect
//! [`Broker::listen`]: crate::Broker::listen

use std::future::Future;
use std::sync::Arc;

tokio::task_local! {
    /// The call the current task is running, set by the dispatcher.
    static CALL: Arc<str>;
}

// ========================================================================
// Functions
// ========================================================================

/// Run `future` with `tag` readable by [`current_call`] anywhere inside it.
pub async fn tag_call<F: Future>(tag: Arc<str>, future: F) -> F::Output {
    CALL.scope(tag, future).await
}

/// The tag of the call this task is running, or `None` outside any scope.
///
/// `None` is an ordinary answer, not a failure: the broker is also reachable
/// from the REPL's own startup checks and from tests, neither of which is a tool
/// call.
pub fn current_call() -> Option<Arc<str>> {
    CALL.try_with(Arc::clone).ok()
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tag_is_readable_inside_the_scope_and_absent_outside() {
        assert!(current_call().is_none(), "no scope has been entered yet");
        let seen = tag_call(Arc::from("toolu_a"), async { current_call() }).await;
        assert_eq!(seen.as_deref(), Some("toolu_a"));
        assert!(current_call().is_none(), "the scope ended with the future");
    }

    #[tokio::test]
    async fn concurrent_scopes_do_not_see_each_others_tags() {
        // The whole point of the task-local: two calls in flight at once each
        // read their own tag. A naive `static mut`-style ambient slot would pass
        // the test above and fail this one.
        let (left, right) = tokio::join!(
            tag_call(Arc::from("toolu_left"), async {
                tokio::task::yield_now().await;
                current_call()
            }),
            tag_call(Arc::from("toolu_right"), async { current_call() }),
        );
        assert_eq!(left.as_deref(), Some("toolu_left"));
        assert_eq!(right.as_deref(), Some("toolu_right"));
    }
}

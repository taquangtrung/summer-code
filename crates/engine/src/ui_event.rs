//! The UI event stream: a projection of a running turn for a frontend to render.
//!
//! [`Engine::turn`] runs the whole agent loop and returns a single
//! [`crate::TurnSummary`] only when the turn is done. A frontend that wants to
//! show tokens as they arrive, a spinner while a tool runs, and thinking as it
//! is produced cannot be built on a function that speaks once at the end. The
//! [`UiEvent`] stream is that function's narration, emitted *alongside* the
//! journal writes.
//!
//! **Two invariants hold across the engine.**
//!
//! 1. The journal stays the source of truth, and `engine` stays its only
//!    writer. [`UiEvent`]s are a projection, never a second record a frontend
//!    persists. A dropped or reordered event is a cosmetic glitch; resume still
//!    folds from the journal, unchanged.
//! 2. [`UiEvent`] is serializable from T0, and that is a bet rather than a
//!    requirement anything here imposes. A detached frontend would forward engine
//!    activity over IPC, and this is the stream it would forward; a serde-clean
//!    `UiEvent` makes that protocol free, while one borrowing engine internals
//!    makes it a rewrite. `--output-format stream-json` already pays out on the
//!    bet. So it derives `Serialize`/`Deserialize` and uses struct variants,
//!    matching the journal's wire discipline.

use journal::{CallId, Outcome, Usage};
use serde::{Deserialize, Serialize};

use crate::Stop;

// ========================================================================
// Data Structures
// ========================================================================

/// A projection of a running turn, for a frontend to render. Not a journal
/// record: the journal is written separately and stays the source of truth.
///
/// Every variant is a struct variant, matching the journal's wire discipline:
/// internal tagging fails at runtime for a newtype variant wrapping a string,
/// so each variant carries named fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiEvent {
    /// A new user turn began carrying `user` as its prompt. Emitted once, at the
    /// top of [`Engine::turn`](crate::Engine::turn).
    TurnStarted { user: String },
    /// One chunk of assistant text appended to the in-flight message.
    TextDelta { text: String },
    /// One chunk of thinking text appended to the in-flight thinking block.
    ThinkingDelta { text: String },
    /// Drop everything shown for the in-flight assistant message. Emitted when
    /// the provider discards a partial turn and re-sends it whole; missing this
    /// leaves the UI showing text from an attempt the model never finished.
    TurnReset {},
    /// A tool dispatch is about to run. `args` is the parsed argument object.
    ToolStarted {
        call: CallId,
        name: String,
        args: serde_json::Value,
    },
    /// A tool dispatch returned. The status moves Running to Done/Denied/Err as
    /// the [`Outcome`] names.
    ToolFinished { call: CallId, outcome: Outcome },
    /// A delegated sub-session finished, having spent `usage`.
    ///
    /// Separate from [`Usage`](UiEvent::Usage), which is a model round of *this*
    /// session: folding the two would make a frontend's turn count include work
    /// no turn of this session did. The totals reported at the end of the turn
    /// already include this.
    Delegated { usage: Usage },
    /// The turn's final accounting. `usage` is the last model round's; the
    /// totals are the session's cumulative spend, rendered to plain numbers so
    /// the event owns no engine type. Emitted once, just before
    /// [`UiEvent::TurnEnded`].
    Usage { usage: Usage, total_tokens: u64 },
    /// The turn ended cleanly. `Ok` paths only; see [`UiEvent::Failed`] for errors.
    TurnEnded { stop: Stop },
    /// The turn could not proceed (provider, sink, truncation). Emitted on every
    /// `Err` exit of [`Engine::turn`](crate::Engine::turn), carrying the error
    /// rendered to a string.
    Failed { message: String },
}

/// How a frontend subscribes. The engine holds an [`Option`] of this and emits
/// best-effort: the engine never blocks on the frontend. An unbounded sender
/// means an emit is always immediate, and a slow or gone UI cannot stall the
/// loop. A detached frontend would apply backpressure at the IPC boundary, not
/// here.
pub type Observer = tokio::sync::mpsc::UnboundedSender<UiEvent>;

// ========================================================================
// Functions
// ========================================================================

/// Emit best-effort to an optional observer. A closed channel (UI gone) is not
/// the loop's problem and is silently dropped. Shared by [`Engine::emit`] and
/// the free-function turn accumulator, which has no `&self`.
///
/// [`Engine::emit`]: crate::Engine::emit
pub fn emit_to(observer: Option<&Observer>, event: UiEvent) {
    if let Some(observer) = observer {
        let _ = observer.send(event);
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_round_trips_through_serde() {
        // `--output-format stream-json` serializes this stream one event per
        // line. Assert the round trip on a representative
        // variant of every shape, including the unit-shaped `TurnReset`.
        let samples = vec![
            UiEvent::TurnStarted { user: "hi".into() },
            UiEvent::TextDelta { text: "hel".into() },
            UiEvent::ThinkingDelta { text: "hm".into() },
            UiEvent::TurnReset {},
            UiEvent::ToolStarted {
                call: CallId::new("toolu_1".into()),
                name: "read".into(),
                args: serde_json::json!({"path": "/a"}),
            },
            UiEvent::ToolFinished {
                call: CallId::new("toolu_1".into()),
                outcome: Outcome::Ok {
                    content: "ok".into(),
                },
            },
            UiEvent::Usage {
                usage: Usage::new(1, 2, 3, 4),
                total_tokens: 7,
            },
            UiEvent::TurnEnded { stop: Stop::Done },
            UiEvent::Failed {
                message: "boom".into(),
            },
        ];
        for event in samples {
            let json = serde_json::to_string(&event).expect("serialize");
            let back: UiEvent = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(event, back, "round trip lost information: {json}");
        }
    }

    #[test]
    fn the_kind_tag_lands_at_a_fixed_key() {
        // The journal routes on a discriminant at a known `kind` key, and so does
        // this stream: a frontend streaming events lines routes on the same key.
        let json = serde_json::to_string(&UiEvent::TextDelta { text: "x".into() }).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["kind"], "text_delta");
    }
}

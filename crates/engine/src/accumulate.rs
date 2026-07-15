//! Folding a turn's event stream into journal blocks.
//!
//! **The stream edge, kept away from the loop.** Everything here is about one
//! provider stream: which block is open, what a delta appends to, and when a
//! block closes into a `Block`. None of it needs the engine, and the turn loop
//! does not need any of it -- it wants one `Accumulated` per turn. Splitting them
//! is what makes the loop readable, and it is also where the two nastiest
//! stream bugs live (an empty `input_json_delta` sequence, and a restart that
//! must discard everything emitted so far), so they get a file that says so.

use futures_util::{Stream, StreamExt};
use journal::{Block, BlockThinking, BlockToolUse, CallId, StopReason, Usage};
use provider::StreamEvent;
use serde_json::Value;

use crate::ui_event::{Observer, UiEvent, emit_to};

// ========================================================================
// Constants
// ========================================================================

/// The largest a single turn's accumulated text, thinking, and tool-argument
/// deltas may grow to before the stream is treated as broken rather than
/// still in progress. Ordinary answers are orders of magnitude smaller, and
/// nothing outside this loop can stop a turn that keeps growing past here:
/// the engine's token limits only see a turn once it has already finished.
const MAX_ACCUMULATED_BYTES: usize = 64 * 1024 * 1024;

// ========================================================================
// Data Structures
// ========================================================================

/// One accumulated model turn.
pub(crate) struct Accumulated {
    pub(crate) blocks: Vec<Block>,
    pub(crate) stop: StopReason,
    pub(crate) text: String,
    pub(crate) usage: Usage,
}

/// The content block currently open in the stream.
pub(crate) enum OpenBlock {
    None,
    Text(String),
    Thinking(PendingThinking),
    ToolUse(PendingToolUse),
}

#[derive(Default)]
pub(crate) struct PendingThinking {
    signature: String,
    text: String,
}

pub(crate) struct PendingToolUse {
    args: String,
    call: CallId,
    name: String,
}

/// One `tool_use` the model asked for this turn.
pub(crate) struct RequestedCall {
    pub(crate) args: Value,
    pub(crate) call: CallId,
    pub(crate) name: String,
}

// ========================================================================
// Functions
// ========================================================================

pub(crate) fn requested_calls(blocks: &[Block]) -> Vec<RequestedCall> {
    blocks
        .iter()
        .filter_map(|block| match block {
            Block::ToolUse(use_) => Some(RequestedCall {
                args: use_.args.clone(),
                call: use_.call.clone(),
                name: use_.name.clone(),
            }),
            Block::Text { .. } | Block::Thinking(_) => None,
        })
        .collect()
}

/// Fold a turn's stream into blocks. `None` when the stream produced no
/// `TurnEnd`, which is the provider saying the turn never completed.
///
/// The `observer` is the only reason this function grows a parameter: the
/// per-token deltas exist only here, at the stream edge, and a UI that wants to
/// render text as it arrives needs them at this granularity rather than once
/// the block closes.
pub(crate) async fn accumulate_turn(
    stream: impl Stream<Item = StreamEvent> + Unpin,
    observer: Option<&Observer>,
) -> Option<Accumulated> {
    let mut accumulated_bytes = 0usize;
    let mut blocks = Vec::new();
    let mut open = OpenBlock::None;
    let mut end = None;

    let mut stream = stream;
    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::TextDelta(delta) => {
                accumulated_bytes += delta.len();
                emit_to(
                    observer,
                    UiEvent::TextDelta {
                        text: delta.clone(),
                    },
                );
                if !matches!(open, OpenBlock::Text(_)) {
                    finalize(&mut open, &mut blocks);
                    open = OpenBlock::Text(String::new());
                }
                if let OpenBlock::Text(text) = &mut open {
                    text.push_str(&delta);
                }
            }
            StreamEvent::ThinkingDelta(delta) => {
                if let Some(text) = delta.text.as_deref() {
                    accumulated_bytes += text.len();
                    emit_to(
                        observer,
                        UiEvent::ThinkingDelta {
                            text: text.to_string(),
                        },
                    );
                }
                if !matches!(open, OpenBlock::Thinking(_)) {
                    finalize(&mut open, &mut blocks);
                    open = OpenBlock::Thinking(PendingThinking::default());
                }
                if let OpenBlock::Thinking(pending) = &mut open {
                    if let Some(text) = delta.text {
                        pending.text.push_str(&text);
                    }
                    if let Some(signature) = delta.signature {
                        accumulated_bytes += signature.len();
                        pending.signature.push_str(&signature);
                    }
                }
            }
            StreamEvent::ToolCallStart(start) => {
                finalize(&mut open, &mut blocks);
                open = OpenBlock::ToolUse(PendingToolUse {
                    args: String::new(),
                    call: start.call,
                    name: start.name,
                });
            }
            StreamEvent::ToolCallDelta(delta) => {
                accumulated_bytes += delta.len();
                if let OpenBlock::ToolUse(pending) = &mut open {
                    pending.args.push_str(&delta);
                }
            }
            StreamEvent::BlockEnd => finalize(&mut open, &mut blocks),
            StreamEvent::TurnEnd(turn_end) => end = Some(turn_end),
            // The provider discarded the partial turn and re-sent it whole, so
            // everything accumulated so far belongs to an attempt that never
            // finished. Keeping it would journal a turn stitched from two. The
            // UI has already been handed the dead attempt's deltas, so a matching
            // reset is emitted at exactly the point `blocks` is cleared: without
            // it the UI renders text from a turn the model never produced.
            StreamEvent::TurnRestarted => {
                emit_to(observer, UiEvent::TurnReset {});
                accumulated_bytes = 0;
                blocks.clear();
                open = OpenBlock::None;
                end = None;
            }
        }
        // A stream that is still growing past here has no legitimate reason
        // to: dropping it now is the only backstop between a decoding loop or
        // a stream bug and the OOM killer.
        if accumulated_bytes > MAX_ACCUMULATED_BYTES {
            return None;
        }
    }
    // A well-formed stream closes every block before message_stop; this catches
    // the one that did not.
    finalize(&mut open, &mut blocks);

    let end = end?;
    let text = blocks
        .iter()
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.as_str()),
            Block::Thinking(_) | Block::ToolUse(_) => None,
        })
        .collect::<String>();

    Some(Accumulated {
        blocks,
        stop: end.stop,
        text,
        usage: end.usage,
    })
}

/// Close the open block and push it.
pub(crate) fn finalize(open: &mut OpenBlock, blocks: &mut Vec<Block>) {
    match std::mem::replace(open, OpenBlock::None) {
        OpenBlock::None => {}
        OpenBlock::Text(text) => {
            if !text.is_empty() {
                blocks.push(Block::Text { text });
            }
        }
        OpenBlock::Thinking(pending) => {
            blocks.push(Block::Thinking(BlockThinking::new(
                pending.signature,
                pending.text,
            )));
        }
        OpenBlock::ToolUse(pending) => {
            // Parsed once, here, with the block closed. Malformed arguments
            // become a validation error the model can read, which is the right
            // audience: the model wrote them.
            //
            // **Empty accumulated text is `{}`, not `Null`.** A `tool_use` whose
            // input is `{}` streams *zero* `input_json_delta` events, so nothing
            // accumulates and `""` does not parse. Defaulting that to `Null`
            // failed schema validation for every no-argument tool call, and --
            // worse -- journaled `"input": null`, which is then rebuilt into
            // every later request in the conversation and can 400 the API long
            // after the call itself, including after a resume. The API's own
            // meaning for "no deltas" is the empty object.
            let args = if pending.args.trim().is_empty() {
                Value::Object(serde_json::Map::new())
            } else {
                serde_json::from_str(&pending.args).unwrap_or(Value::Null)
            };
            blocks.push(Block::ToolUse(BlockToolUse::new(
                pending.call,
                pending.name,
                args,
            )));
        }
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use futures_util::stream;
    use provider::TurnEnd;

    use super::*;

    /// A well-formed close: pairs with an oversized delta to prove it is the
    /// cap, not a missing `TurnEnd`, that turns the turn away.
    fn turn_end() -> StreamEvent {
        StreamEvent::TurnEnd(TurnEnd {
            stop: StopReason::EndTurn,
            usage: Usage::zero(),
        })
    }

    #[tokio::test]
    async fn a_turn_past_the_byte_cap_is_treated_as_broken() {
        // A decoding loop or stream bug that never reaches `TurnEnd` on its
        // own would otherwise grow this turn's accumulated text forever; the
        // cap is what stops it before the OS does. The `TurnEnd` right after
        // the oversized delta proves the cap is what returns `None` here --
        // without it, this same script would return `Some`.
        let oversized = "x".repeat(MAX_ACCUMULATED_BYTES + 1);
        let events = stream::iter(vec![StreamEvent::TextDelta(oversized), turn_end()]);

        assert!(accumulate_turn(events, None).await.is_none());
    }
}

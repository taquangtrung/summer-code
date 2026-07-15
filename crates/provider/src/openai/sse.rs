//! Decoding an OpenAI chat-completions SSE stream into [`StreamEvent`]s.
//!
//! **The hard part is that the wire has no block boundaries.** Anthropic's
//! stream says `content_block_start` and `content_block_stop`, so the engine's
//! `BlockEnd` maps straight across. Chat-completions says nothing: text deltas
//! and tool-call fragments simply arrive, and a block is "over" when something
//! else starts or the stream ends. The decoder therefore tracks what is open and
//! synthesizes the boundaries, because [`StreamEvent::BlockEnd`] is what tells
//! the engine to parse a tool call's accumulated JSON, and a missing one means
//! the call is never parsed and never dispatched.
//!
//! **Tool calls are keyed by index, not by id.** The id arrives once, on the
//! first fragment; every later fragment for the same call carries only
//! `index`. A decoder keying on id would open a new call per fragment and
//! produce a stream of empty tool uses.

use std::collections::BTreeMap;

use journal::CallId;

use crate::openai::wire;
use crate::openai::wire::{ChatChunk, ChatUsage};
use crate::provider::{StreamEvent, ThinkingDelta, ToolCallStart, TurnEnd};

// ========================================================================
// Constants
// ========================================================================

/// The frame body that ends a chat-completions stream.
const DONE: &str = "[DONE]";

// ========================================================================
// Data Structures
// ========================================================================

/// Turns SSE frames into [`StreamEvent`]s, holding the block state the wire
/// does not carry.
#[derive(Default)]
pub struct Decoder {
    /// The tool calls seen so far, by their wire index. `BTreeMap` so the
    /// closing order is the index order and therefore the order the model asked
    /// in, which is what the engine's batching relies on.
    calls: BTreeMap<usize, CallId>,
    /// Whether a text or thinking block is currently open and needs closing.
    open_text: bool,
    /// Usage from the final chunk, which arrives after the last choice.
    usage: Option<ChatUsage>,
}

// ========================================================================
// Decoder Implementations
// ========================================================================

impl Decoder {
    /// Decode one `data:` frame body into the events it produces.
    ///
    /// A frame that is `[DONE]`, malformed, or empty produces nothing: an
    /// OpenAI-compatible endpoint may send keep-alive comments and padding
    /// chunks, and treating one as a protocol error would kill a healthy stream.
    pub fn frame(&mut self, data: &str) -> Vec<StreamEvent> {
        let data = data.trim();
        if data.is_empty() || data == DONE {
            return Vec::new();
        }
        let Ok(chunk) = serde_json::from_str::<ChatChunk>(data) else {
            return Vec::new();
        };
        if let Some(usage) = chunk.usage {
            self.usage = Some(usage);
        }

        let mut events = Vec::new();
        for choice in chunk.choices {
            let delta = choice.delta;
            if let Some(text) = delta.reasoning_content.filter(|text| !text.is_empty()) {
                self.open_text = true;
                events.push(StreamEvent::ThinkingDelta(ThinkingDelta {
                    // The wire carries no signature: nothing here is signed, and
                    // nothing is echoed back, so there is none to report.
                    signature: None,
                    text: Some(text),
                }));
            }
            if let Some(text) = delta.content.filter(|text| !text.is_empty()) {
                self.open_text = true;
                events.push(StreamEvent::TextDelta(text));
            }

            for call in delta.tool_calls {
                // A tool call starting closes any open text block: the text is
                // complete, and the engine needs the boundary to journal it.
                if self.open_text {
                    events.push(StreamEvent::BlockEnd);
                    self.open_text = false;
                }
                let function = call.function.unwrap_or(wire::FunctionDelta {
                    arguments: None,
                    name: None,
                });
                if let std::collections::btree_map::Entry::Vacant(slot) =
                    self.calls.entry(call.index)
                {
                    // First fragment for this index: the id and name are here
                    // and nowhere later.
                    let id = CallId::new(call.id.unwrap_or_else(|| format!("call_{}", call.index)));
                    slot.insert(id.clone());
                    events.push(StreamEvent::ToolCallStart(ToolCallStart {
                        call: id,
                        name: function.name.clone().unwrap_or_default(),
                    }));
                }
                if let Some(arguments) = function.arguments.filter(|args| !args.is_empty()) {
                    events.push(StreamEvent::ToolCallDelta(arguments));
                }
            }

            if let Some(finish) = choice.finish_reason {
                // Close whatever is open before ending the turn. Both the last
                // tool call and a trailing text block need their boundary, and
                // the wire gives neither.
                if self.open_text || !self.calls.is_empty() {
                    events.push(StreamEvent::BlockEnd);
                    self.open_text = false;
                }
                events.push(StreamEvent::TurnEnd(TurnEnd {
                    stop: wire::stop_reason(&finish),
                    usage: self.usage.as_ref().map(wire::usage).unwrap_or_default(),
                }));
            }
        }
        events
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use journal::StopReason;

    use super::*;

    fn decode(frames: &[&str]) -> Vec<StreamEvent> {
        let mut decoder = Decoder::default();
        frames
            .iter()
            .flat_map(|frame| decoder.frame(frame))
            .collect()
    }

    #[test]
    fn text_deltas_close_with_a_block_end_before_the_turn_ends() {
        // Without the synthesized `BlockEnd` the engine never closes the text
        // block and the assistant message is journaled empty.
        let events = decode(&[
            r#"{"choices":[{"delta":{"content":"hel"}}]}"#,
            r#"{"choices":[{"delta":{"content":"lo"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ]);
        let shapes: Vec<&str> = events
            .iter()
            .map(|event| match event {
                StreamEvent::BlockEnd => "end",
                StreamEvent::TextDelta(_) => "text",
                StreamEvent::TurnEnd(_) => "turn",
                _ => "other",
            })
            .collect();
        assert_eq!(shapes, ["text", "text", "end", "turn"]);
    }

    #[test]
    fn tool_call_fragments_are_joined_by_index_not_by_id() {
        // The id arrives once. Keying on it opens a new call per fragment, and
        // the model's one tool call becomes several empty ones.
        let events = decode(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_x","function":{"name":"read","arguments":"{\"pa"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.rs\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);

        let starts: Vec<&ToolCallStart> = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ToolCallStart(start) => Some(start),
                _ => None,
            })
            .collect();
        assert_eq!(starts.len(), 1, "one call, not one per fragment");
        assert_eq!(starts[0].name, "read");

        let args: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ToolCallDelta(chunk) => Some(chunk.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(args, r#"{"path":"a.rs"}"#);
    }

    #[test]
    fn a_tool_call_after_text_closes_the_text_block_first() {
        // Both blocks are in one assistant turn. Without the boundary the text
        // and the tool arguments run into one block.
        let events = decode(&[
            r#"{"choices":[{"delta":{"content":"let me read that"}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"read","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        let end_at = events
            .iter()
            .position(|event| matches!(event, StreamEvent::BlockEnd))
            .expect("a block end");
        let start_at = events
            .iter()
            .position(|event| matches!(event, StreamEvent::ToolCallStart(_)))
            .expect("a tool call start");
        assert!(end_at < start_at, "the text block closed too late");
    }

    #[test]
    fn usage_from_the_final_chunk_reaches_the_turn_end() {
        // Usage arrives in its own chunk *after* the one carrying
        // `finish_reason` on some endpoints and before it on others. Holding it
        // on the decoder covers both; reading it only off the finishing chunk
        // journals zero spend for every turn.
        let events = decode(&[
            r#"{"choices":[{"delta":{"content":"hi"}}],"usage":{"prompt_tokens":10,"completion_tokens":2}}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ]);
        let Some(StreamEvent::TurnEnd(end)) = events
            .into_iter()
            .find(|event| matches!(event, StreamEvent::TurnEnd(_)))
        else {
            panic!("a turn end");
        };
        assert_eq!(end.usage.input, 10);
        assert_eq!(end.usage.output, 2);
        assert_eq!(end.stop, StopReason::EndTurn);
    }

    #[test]
    fn a_malformed_or_keepalive_frame_does_not_kill_the_stream() {
        // Compatible endpoints pad streams with frames this decoder has no
        // shape for. Treating one as fatal ends a healthy turn.
        let mut decoder = Decoder::default();
        assert!(decoder.frame("").is_empty());
        assert!(decoder.frame("[DONE]").is_empty());
        assert!(decoder.frame("{not json").is_empty());
        assert_eq!(
            decoder
                .frame(r#"{"choices":[{"delta":{"content":"still here"}}]}"#)
                .len(),
            1,
            "the decoder should still work after junk"
        );
    }
}

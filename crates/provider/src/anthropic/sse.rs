//! The Anthropic SSE event sequence, decoded into [`StreamEvent`]s.
//!
//! ```text
//! message_start        -> message metadata, initial usage
//! content_block_start  -> {type: text | thinking | tool_use}  -> ToolCallStart for tool_use
//! content_block_delta  -> {type: text_delta | thinking_delta | signature_delta
//!                          | input_json_delta}
//! content_block_stop   -> BlockEnd
//!   (repeats per block)
//! message_delta        -> {stop_reason, usage.output_tokens}
//! message_stop         -> stream complete                     -> TurnEnd
//! ```
//!
//! **`TurnEnd` is emitted only on `message_stop`.** A stream that stops earlier
//! leaves [`SseDecoder::saw_message_stop`] false, and the caller turns that into
//! [`ProviderError::StreamBroken`](crate::ProviderError::StreamBroken). Emitting
//! a `TurnEnd` on `message_delta` instead would report a truncated stream as a
//! completed turn, which is the one thing the loop must never be told.
//!
//! `input_json_delta` carries **partial JSON**. This decoder forwards the
//! fragments and the closing `BlockEnd`; it never matches on the partial text
//! and never parses before the block closes. The caller accumulates and parses
//! once at `BlockEnd`.
//!
//! This is a decoder over `&str`, not over a byte stream, so every branch above
//! is reachable from a unit test without a network.

use std::collections::VecDeque;

use journal::{CallId, StopReason, Usage};
use serde_json::Value;

use crate::provider::{StreamEvent, ThinkingDelta, ToolCallStart, TurnEnd};

// ========================================================================
// Constants
// ========================================================================

/// The largest single SSE event this decoder will buffer.
///
/// Generous by design: a `content_block_start` for a large tool schema, or a
/// long `message_start`, is legitimately tens of kilobytes. What this bounds is
/// the pathological case of a server that never sends the blank line, which is
/// otherwise unbounded memory growth in the harness.
const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;

// ========================================================================
// Data Structures
// ========================================================================

/// Incremental SSE decode state.
#[derive(Default)]
pub struct SseDecoder {
    buffer: String,
    cache_read: u32,
    cache_write: u32,
    /// An in-stream `error` event's reason, waiting to be taken by the driver.
    error: Option<String>,
    input: u32,
    output: u32,
    /// Bytes that are not yet a complete UTF-8 sequence. See [`SseDecoder::push_bytes`].
    partial: Vec<u8>,
    pending: VecDeque<StreamEvent>,
    saw_message_stop: bool,
    stop: Option<StopReason>,
}

// ========================================================================
// SseDecoder Implementations
// ========================================================================

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of the response body **as it arrives off the socket**.
    ///
    /// This is the entry point a byte stream must use, and the reason it exists
    /// is a corruption bug that is otherwise completely silent. `bytes_stream`
    /// yields TCP-sized chunks, not character-aligned ones, so a multi-byte
    /// character can straddle a chunk boundary. Lossy-decoding each chunk on its
    /// own turns that character into U+FFFD, and the surrounding JSON still
    /// parses, so nothing errors:
    ///
    /// - in a `text_delta` it silently mangles the model's output;
    /// - in an `input_json_delta` it corrupts the tool arguments;
    /// - in a `signature_delta` it corrupts a thinking signature, which the API
    ///   rejects on the next turn: a 400 traced back to a chunk boundary.
    ///
    /// So an incomplete trailing sequence is held back until the rest of it
    /// arrives. Genuinely invalid bytes still become U+FFFD, because they are
    /// not going to become valid by waiting.
    pub fn push_bytes(&mut self, chunk: &[u8]) {
        self.partial.extend_from_slice(chunk);
        loop {
            match std::str::from_utf8(&self.partial) {
                Ok(text) => {
                    let text = text.to_string();
                    self.partial.clear();
                    self.push(&text);
                    return;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        let text = String::from_utf8_lossy(&self.partial[..valid]).to_string();
                        self.partial.drain(..valid);
                        self.push(&text);
                    }
                    match error.error_len() {
                        // An incomplete tail: keep it and wait for the next chunk.
                        None => return,
                        // Truly invalid: drop it and carry on, or we would spin.
                        Some(len) => {
                            self.partial.drain(..len.min(self.partial.len()));
                            self.push("\u{FFFD}");
                        }
                    }
                }
            }
        }
    }

    /// Feed already-decoded text. Parses every complete event in the buffer,
    /// leaving any trailing partial event in place.
    ///
    /// `\r\n` is normalized to `\n` on the way in. SSE permits CRLF framing, and
    /// `\r\n\r\n` contains no `\n\n`: without this the buffer would grow forever
    /// and no event would ever be emitted. A `data:` line cannot contain a raw
    /// newline of either kind, so the rewrite cannot corrupt a payload.
    ///
    /// **The normalization runs over the buffer, and a trailing `\r` is held
    /// back**, for exactly the reason the incomplete UTF-8 tail is held back in
    /// [`SseDecoder::push_bytes`]. Normalizing each chunk in isolation left a
    /// raw `\r` behind whenever a `\r\n` straddled a chunk boundary, and a lone
    /// `\r` is not a separator: the two adjacent events merged into one block,
    /// serde rejected the result, and **both were dropped silently**. A turn that
    /// lost its `text_delta` that way still reached `message_stop` and journaled
    /// as a complete turn with missing output.
    pub fn push(&mut self, chunk: &str) {
        self.buffer.push_str(chunk);
        if self.buffer.contains('\r') {
            // A `\r` at the very end may be the first half of a separator whose
            // `\n` is in the next chunk. Set it aside across the rewrite so it
            // can still pair up.
            let dangling = self.buffer.ends_with('\r');
            if dangling {
                self.buffer.pop();
            }
            self.buffer = self.buffer.replace("\r\n", "\n");
            if dangling {
                self.buffer.push('\r');
            }
        }
        while let Some(split) = self.buffer.find("\n\n") {
            let block: String = self.buffer.drain(..split).collect();
            self.buffer.drain(.."\n\n".len());
            self.parse_block(&block);
        }
        self.enforce_buffer_cap();
    }

    /// Refuse to grow the buffer without bound.
    ///
    /// Every complete event drains, so the buffer only ever holds one partial
    /// one. A server that sends a body and never a blank line is therefore not
    /// slow, it is broken -- and without a cap the harness answers by consuming
    /// memory until it is killed. Dropping the buffer loses the malformed event
    /// and keeps the process; the turn then ends without `message_stop`, which
    /// the engine already reports as a broken stream.
    fn enforce_buffer_cap(&mut self) {
        if self.buffer.len() > MAX_EVENT_BYTES {
            tracing::warn!(
                bytes = self.buffer.len(),
                "SSE event exceeded the size cap; dropping the buffer"
            );
            self.buffer.clear();
        }
    }

    /// Take the next decoded event, if one is ready.
    pub fn next_event(&mut self) -> Option<StreamEvent> {
        self.pending.pop_front()
    }

    /// Take the reason an in-stream `error` event gave, if one arrived.
    pub fn take_error(&mut self) -> Option<String> {
        self.error.take()
    }

    /// Whether the stream reached `message_stop`. False means the turn never
    /// completed, whatever bytes arrived.
    pub fn saw_message_stop(&self) -> bool {
        self.saw_message_stop
    }

    /// Drop everything accumulated. Used when a mid-stream failure discards the
    /// partial turn and the request is re-sent whole.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    fn parse_block(&mut self, block: &str) {
        let mut data = String::new();
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                data.push_str(rest.trim());
            }
        }
        if data.is_empty() {
            return;
        }
        let Ok(event) = serde_json::from_str::<Value>(&data) else {
            return;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => self.on_message_start(&event),
            Some("content_block_start") => self.on_content_block_start(&event),
            Some("content_block_delta") => self.on_content_block_delta(&event),
            Some("content_block_stop") => self.pending.push_back(StreamEvent::BlockEnd),
            Some("message_delta") => self.on_message_delta(&event),
            Some("message_stop") => self.on_message_stop(),
            // **An in-stream `error` event, which used to fall into `_ => {}`.**
            // The API sends this to end a stream that cannot continue (an
            // overloaded backend, a mid-generation policy stop), and swallowing
            // it meant the stream simply stopped: the caller saw "bytes ended
            // before message_stop" and the server's own explanation was
            // discarded. Surfacing it puts the real reason on the retry path
            // that already records and journals causes.
            Some("error") => self.on_error(&event),
            _ => {}
        }
    }

    fn on_error(&mut self, event: &Value) {
        let kind = event
            .pointer("/error/type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let message = event
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("no message");
        self.error = Some(format!("{kind}: {message}"));
    }

    fn on_message_start(&mut self, event: &Value) {
        let usage = event.pointer("/message/usage").cloned().unwrap_or_default();
        self.input = as_u32(&usage, "/input_tokens");
        self.cache_read = as_u32(&usage, "/cache_read_input_tokens");
        self.cache_write = as_u32(&usage, "/cache_creation_input_tokens");
    }

    fn on_content_block_start(&mut self, event: &Value) {
        let Some(block) = event.get("content_block") else {
            return;
        };
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            return;
        }
        let Some(id) = block.get("id").and_then(Value::as_str) else {
            return;
        };
        let Some(name) = block.get("name").and_then(Value::as_str) else {
            return;
        };
        self.pending
            .push_back(StreamEvent::ToolCallStart(ToolCallStart {
                call: CallId::new(id.to_string()),
                name: name.to_string(),
            }));
    }

    fn on_content_block_delta(&mut self, event: &Value) {
        let delta = event.get("delta").cloned().unwrap_or_default();
        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => {
                if let Some(text) = delta.get("text").and_then(Value::as_str) {
                    self.pending
                        .push_back(StreamEvent::TextDelta(text.to_string()));
                }
            }
            Some("thinking_delta") => {
                if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                    self.pending
                        .push_back(StreamEvent::ThinkingDelta(ThinkingDelta {
                            signature: None,
                            text: Some(text.to_string()),
                        }));
                }
            }
            Some("signature_delta") => {
                if let Some(signature) = delta.get("signature").and_then(Value::as_str) {
                    self.pending
                        .push_back(StreamEvent::ThinkingDelta(ThinkingDelta {
                            signature: Some(signature.to_string()),
                            text: None,
                        }));
                }
            }
            Some("input_json_delta") => {
                if let Some(partial) = delta.get("partial_json").and_then(Value::as_str) {
                    self.pending
                        .push_back(StreamEvent::ToolCallDelta(partial.to_string()));
                }
            }
            _ => {}
        }
    }

    fn on_message_delta(&mut self, event: &Value) {
        self.output = as_u32(event, "/usage/output_tokens");
        if let Some(reason) = event.pointer("/delta/stop_reason").and_then(Value::as_str) {
            let category = event
                .pointer("/delta/stop_details/category")
                .and_then(Value::as_str)
                .map(str::to_string);
            self.stop = Some(stop_reason(reason, category));
        }
    }

    fn on_message_stop(&mut self) {
        self.saw_message_stop = true;
        self.pending.push_back(StreamEvent::TurnEnd(TurnEnd {
            // A stream that reached message_stop without a stop_reason is not a
            // shape the API produces; treat it as an ordinary end rather than
            // inventing a failure.
            stop: self.stop.clone().unwrap_or(StopReason::EndTurn),
            usage: Usage::new(self.input, self.output, self.cache_read, self.cache_write),
        }));
    }
}

// ========================================================================
// Functions
// ========================================================================

/// Map the wire's `stop_reason` onto the schema's.
fn stop_reason(reason: &str, category: Option<String>) -> StopReason {
    match reason {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "pause_turn" => StopReason::PauseTurn,
        "refusal" => StopReason::Refusal { category },
        "stop_sequence" => StopReason::StopSequence,
        "tool_use" => StopReason::ToolUse,
        // An unknown stop reason is the API telling us it grew one. Ending the
        // turn is the safe reading; inventing a retry is not.
        _ => StopReason::EndTurn,
    }
}

fn as_u32(value: &Value, path: &str) -> u32 {
    value.pointer(path).and_then(Value::as_u64).unwrap_or(0) as u32
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str, payload: &str) -> String {
        format!("event: {kind}\ndata: {payload}\n\n")
    }

    fn drain(decoder: &mut SseDecoder) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        while let Some(event) = decoder.next_event() {
            events.push(event);
        }
        events
    }

    fn message_start() -> String {
        event(
            "message_start",
            r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":90,"cache_creation_input_tokens":5}}}"#,
        )
    }

    #[test]
    fn turn_end_carries_usage_and_stop_from_the_whole_stream() {
        let mut decoder = SseDecoder::new();
        decoder.push(&message_start());
        decoder.push(&event(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}"#,
        ));
        decoder.push(&event("message_stop", r#"{"type":"message_stop"}"#));

        let events = drain(&mut decoder);
        let Some(StreamEvent::TurnEnd(end)) = events.last() else {
            panic!("expected a TurnEnd, got {} events", events.len());
        };
        assert_eq!(end.stop, StopReason::ToolUse);
        assert_eq!(end.usage, Usage::new(10, 42, 90, 5));
        assert!(decoder.saw_message_stop());
    }

    #[test]
    fn a_stream_cut_before_message_stop_yields_no_turn_end() {
        // The exact inversion worth guarding: a truncated stream reported as a
        // completed turn would journal a ModelTurn the model never finished.
        let mut decoder = SseDecoder::new();
        decoder.push(&message_start());
        decoder.push(&event(
            "content_block_start",
            r#"{"type":"content_block_start","content_block":{"type":"tool_use","id":"toolu_1","name":"read"}}"#,
        ));
        decoder.push(&event(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}"#,
        ));
        // Bytes stop here: message_delta arrived, message_stop never did.

        let events = drain(&mut decoder);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::TurnEnd(_))),
            "message_delta must not produce a TurnEnd"
        );
        assert!(!decoder.saw_message_stop());
    }

    #[test]
    fn tool_json_is_forwarded_as_fragments_and_closed_by_block_end() {
        let mut decoder = SseDecoder::new();
        decoder.push(&event(
            "content_block_start",
            r#"{"type":"content_block_start","content_block":{"type":"tool_use","id":"toolu_1","name":"read"}}"#,
        ));
        decoder.push(&event(
            "content_block_delta",
            r#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
        ));
        decoder.push(&event(
            "content_block_delta",
            r#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"\"a.rs\"}"}}"#,
        ));
        decoder.push(&event(
            "content_block_stop",
            r#"{"type":"content_block_stop"}"#,
        ));

        let events = drain(&mut decoder);
        let mut json = String::new();
        let mut closed = false;
        for event in &events {
            match event {
                StreamEvent::ToolCallDelta(fragment) => json.push_str(fragment),
                StreamEvent::BlockEnd => closed = true,
                _ => {}
            }
        }
        assert_eq!(json, r#"{"path":"a.rs"}"#);
        assert!(closed, "content_block_stop must produce a BlockEnd");
    }

    #[test]
    fn thinking_text_and_signature_arrive_as_separate_deltas() {
        let mut decoder = SseDecoder::new();
        decoder.push(&event(
            "content_block_delta",
            r#"{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"let me look"}}"#,
        ));
        decoder.push(&event(
            "content_block_delta",
            r#"{"type":"content_block_delta","delta":{"type":"signature_delta","signature":"EqoBCkgIAx=="}}"#,
        ));

        let events = drain(&mut decoder);
        let deltas: Vec<&ThinkingDelta> = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ThinkingDelta(delta) => Some(delta),
                _ => None,
            })
            .collect();
        assert_eq!(deltas.len(), 2);
        assert_eq!(deltas[0].text.as_deref(), Some("let me look"));
        assert_eq!(deltas[1].signature.as_deref(), Some("EqoBCkgIAx=="));
    }

    #[test]
    fn refusal_carries_its_category() {
        // The API returns 200 here with empty or partial content, so the stop
        // reason is the only signal that the turn was refused.
        let mut decoder = SseDecoder::new();
        decoder.push(&event(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"category":"harmful"}},"usage":{"output_tokens":0}}"#,
        ));
        decoder.push(&event("message_stop", r#"{"type":"message_stop"}"#));

        let events = drain(&mut decoder);
        let Some(StreamEvent::TurnEnd(end)) = events.last() else {
            panic!("expected a TurnEnd");
        };
        assert_eq!(
            end.stop,
            StopReason::Refusal {
                category: Some("harmful".into())
            }
        );
    }

    #[test]
    fn events_split_across_chunks_still_decode() {
        // SSE framing arrives on TCP boundaries, not event boundaries.
        let mut decoder = SseDecoder::new();
        let whole = event("message_stop", r#"{"type":"message_stop"}"#);
        let (head, tail) = whole.split_at(12);
        decoder.push(head);
        assert!(
            drain(&mut decoder).is_empty(),
            "partial event must not decode"
        );
        decoder.push(tail);
        assert_eq!(drain(&mut decoder).len(), 1);
        assert!(decoder.saw_message_stop());
    }

    fn text_of(events: &[StreamEvent]) -> String {
        let mut text = String::new();
        for event in events {
            if let StreamEvent::TextDelta(delta) = event {
                text.push_str(delta);
            }
        }
        text
    }

    fn text_delta(body: &str) -> String {
        event(
            "content_block_delta",
            &format!(
                r#"{{"type":"content_block_delta","delta":{{"type":"text_delta","text":"{body}"}}}}"#
            ),
        )
    }

    #[test]
    fn a_character_split_across_chunks_is_not_corrupted() {
        // The bug this catches is silent: lossy-decoding each chunk on its own
        // turns the straddling character into U+FFFD, and the JSON around it
        // still parses, so nothing errors and the mangled text is journaled as
        // if it were correct.
        let whole = text_delta("héllo wörld");
        let bytes = whole.as_bytes();

        for split in 1..bytes.len() {
            let mut decoder = SseDecoder::new();
            decoder.push_bytes(&bytes[..split]);
            decoder.push_bytes(&bytes[split..]);
            let events = drain(&mut decoder);
            assert_eq!(
                text_of(&events),
                "héllo wörld",
                "corrupted when split at byte {split}"
            );
        }
    }

    #[test]
    fn a_signature_split_across_chunks_survives_byte_identical() {
        // A corrupted signature is echoed back and the API rejects the block, so
        // this failure surfaces as a 400 on the *next* turn.
        let signature = "EqoBCkgIAxABGAIiQ/+r4Zz9==";
        let whole = event(
            "content_block_delta",
            &format!(
                r#"{{"type":"content_block_delta","delta":{{"type":"signature_delta","signature":"{signature}"}}}}"#
            ),
        );
        let bytes = whole.as_bytes();

        for split in 1..bytes.len() {
            let mut decoder = SseDecoder::new();
            decoder.push_bytes(&bytes[..split]);
            decoder.push_bytes(&bytes[split..]);
            let events = drain(&mut decoder);
            let found = events.iter().find_map(|event| match event {
                StreamEvent::ThinkingDelta(delta) => delta.signature.as_deref(),
                _ => None,
            });
            assert_eq!(found, Some(signature), "corrupted at byte {split}");
        }
    }

    #[test]
    fn a_byte_at_a_time_stream_still_decodes() {
        // The pathological chunking, which is also the one that finds every
        // boundary bug at once.
        let whole = text_delta("héllo");
        let mut decoder = SseDecoder::new();
        for byte in whole.as_bytes() {
            decoder.push_bytes(&[*byte]);
        }
        assert_eq!(text_of(&drain(&mut decoder)), "héllo");
    }

    #[test]
    fn genuinely_invalid_bytes_do_not_stall_the_decoder() {
        // An incomplete tail is held back; invalid bytes are not going to become
        // valid by waiting, so they must not block the stream forever.
        let mut decoder = SseDecoder::new();
        decoder.push_bytes(&[0xff, 0xfe]);
        decoder.push_bytes(text_delta("ok").as_bytes());
        assert!(text_of(&drain(&mut decoder)).contains("ok"));
    }

    #[test]
    fn crlf_framed_events_decode() {
        // SSE permits CRLF framing, and `\r\n\r\n` contains no `\n\n`: the
        // buffer would grow forever and nothing would ever emit.
        let mut decoder = SseDecoder::new();
        decoder.push("event: message_stop\r\ndata: {\"type\":\"message_stop\"}\r\n\r\n");
        assert_eq!(drain(&mut decoder).len(), 1);
        assert!(decoder.saw_message_stop());
    }

    #[test]
    fn a_crlf_split_across_chunks_still_frames_both_events() {
        // **A silent data-loss bug.** Normalizing each chunk in isolation left a
        // raw `\r` behind when a `\r\n` straddled the boundary, and a lone `\r`
        // is not a separator: the two events merged into one block, serde
        // rejected it, and both vanished. The turn still reached `message_stop`
        // and journaled as complete, with the model's text missing.
        let framed = format!(
            "event: content_block_delta\r\ndata: {}\r\n\r\nevent: message_stop\r\ndata: {{\"type\":\"message_stop\"}}\r\n\r\n",
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"kept"}}"#
        );
        // Split so a `\r\n` is cut in half, which is what a TCP-sized chunk does.
        let cut = framed.find("\r\n\r\n").expect("has a separator") + 1;
        let (head, tail) = framed.split_at(cut);
        assert!(head.ends_with('\r'), "the split must tear a CRLF");

        let mut decoder = SseDecoder::new();
        decoder.push(head);
        decoder.push(tail);

        let events = drain(&mut decoder);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, StreamEvent::TextDelta(text) if text == "kept")),
            "the text delta was lost ({} events decoded)",
            events.len()
        );
        assert!(decoder.saw_message_stop());
    }

    #[test]
    fn an_in_stream_error_event_is_surfaced_not_swallowed() {
        // The API ends an unrecoverable stream with this rather than closing the
        // socket. Falling into `_ => {}` meant the caller saw only "the bytes
        // stopped" and the server's own explanation was discarded.
        let mut decoder = SseDecoder::new();
        decoder.push(&event(
            "error",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        ));

        let reason = decoder.take_error().expect("the error is surfaced");
        assert!(reason.contains("overloaded_error"), "{reason}");
        assert!(reason.contains("Overloaded"), "{reason}");
        assert!(
            decoder.take_error().is_none(),
            "taking it twice would double-report"
        );
    }

    #[test]
    fn an_event_past_the_size_cap_does_not_grow_without_bound() {
        // A server that sends a body and never a blank line is broken, not slow.
        // Without a cap the harness answers by consuming memory until killed.
        let mut decoder = SseDecoder::new();
        decoder.push(&format!("data: {}", "x".repeat(MAX_EVENT_BYTES + 1)));
        assert!(decoder.buffer.len() <= MAX_EVENT_BYTES);
    }

    #[test]
    fn reset_discards_the_partial_turn() {
        let mut decoder = SseDecoder::new();
        decoder.push(&message_start());
        decoder.push(&event(
            "content_block_delta",
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"half"}}"#,
        ));
        decoder.reset();
        assert!(drain(&mut decoder).is_empty());
        assert!(!decoder.saw_message_stop());
    }
}

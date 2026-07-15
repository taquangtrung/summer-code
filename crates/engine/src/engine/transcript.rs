//! Flattening a history range into text a summarizer can read.
//!
//! Used by the compaction summarizer and by session titling. Pure and
//! synchronous: given a history slice it returns a string, with no provider and
//! no engine in sight, which is what lets the elision rules below be asserted
//! directly rather than through a live turn.

use journal::{Block, Outcome};

use crate::session::{Entry, HistoryEntry};

// ========================================================================
// Functions
// ========================================================================

/// Render a history range as a plain-text transcript, for the compaction
/// summarizer to read. See [`Engine::model_summary`] for why the range is
/// flattened rather than replayed as messages.
///
/// **Tool results are truncated and thinking blocks are dropped.** The range is
/// being compacted precisely because it is too large, so a transcript that
/// reproduced every tool result in full would be a summarizer request larger
/// than the window that could not hold the original. What survives is the shape
/// a handover note is written from: who said what, which tools ran against which
/// arguments, and whether each one worked.
pub(super) fn transcribe(history: &[HistoryEntry]) -> String {
    /// How much of one tool result reaches the transcript. Generous enough to
    /// carry a compiler error or a short file, short enough that a thousand-line
    /// `grep` cannot dominate the request.
    const RESULT_BUDGET: usize = 600;

    let mut out = String::new();
    for entry in history {
        match &entry.entry {
            Entry::User(text) => out.push_str(&format!("User: {text}\n\n")),
            Entry::Summary(text) => out.push_str(&format!("Earlier summary: {text}\n\n")),
            Entry::Model(blocks) => {
                for block in blocks {
                    match block {
                        Block::Text { text } => out.push_str(&format!("Assistant: {text}\n\n")),
                        // Signed, opaque, and meaningless outside the request
                        // that produced them; see `model_summary`.
                        Block::Thinking(_) => {}
                        Block::ToolUse(use_) => {
                            out.push_str(&format!(
                                "Assistant called {}({})\n\n",
                                use_.name, use_.args
                            ));
                        }
                    }
                }
            }
            Entry::Tool { call: _, outcome } => {
                let (label, body) = match outcome {
                    Outcome::Ok { content } => ("result", content.as_str()),
                    Outcome::Err { message } => ("error", message.as_str()),
                    Outcome::Denied { reason } => ("denied", reason.as_str()),
                    Outcome::TimedOut => ("timed out", ""),
                };
                out.push_str(&format!(
                    "Tool {label}: {}\n\n",
                    truncate(body, RESULT_BUDGET)
                ));
            }
        }
    }
    out
}

/// `text` cut to `budget` bytes on a char boundary, saying so when it was cut.
fn truncate(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... ({} bytes elided)", &text[..end], text.len() - end)
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use journal::CallId;
    use serde_json::json;

    use super::*;

    #[test]
    fn a_transcript_carries_the_work_and_drops_the_signed_thinking() {
        // What the summarizer is handed. Thinking blocks must not survive: their
        // signatures are validated against the request that produced them, and
        // the flattening exists to keep this call clear of that entirely. Tool
        // calls and their outcomes must survive, because a handover note written
        // without them cannot say what was tried.
        let history = vec![
            HistoryEntry {
                entry: Entry::Model(vec![
                    Block::Thinking(journal::BlockThinking {
                        signature: "sig-abc".into(),
                        text: "internal reasoning".into(),
                    }),
                    Block::Text {
                        text: "I will read the file.".into(),
                    },
                    Block::ToolUse(journal::BlockToolUse::new(
                        CallId::new("toolu_1".into()),
                        "read".into(),
                        json!({ "path": "src/lib.rs" }),
                    )),
                ]),
                ids: vec![],
            },
            HistoryEntry {
                entry: Entry::Tool {
                    call: CallId::new("toolu_1".into()),
                    outcome: Outcome::Ok {
                        content: "pub fn parse() {}".into(),
                    },
                },
                ids: vec![],
            },
        ];

        let transcript = transcribe(&history);
        assert!(transcript.contains("I will read the file."), "{transcript}");
        assert!(transcript.contains("read("), "{transcript}");
        assert!(transcript.contains("src/lib.rs"), "{transcript}");
        assert!(transcript.contains("pub fn parse() {}"), "{transcript}");
        assert!(
            !transcript.contains("sig-abc") && !transcript.contains("internal reasoning"),
            "a thinking block reached the summarizer: {transcript}"
        );
    }

    #[test]
    fn a_long_tool_result_is_elided_rather_than_reproduced() {
        // The range is being compacted because it is too big, so a transcript
        // that reproduced a huge `grep` in full would build a summarizer request
        // larger than the window that could not hold the original.
        let history = vec![HistoryEntry {
            entry: Entry::Tool {
                call: CallId::new("toolu_1".into()),
                outcome: Outcome::Ok {
                    content: "x".repeat(10_000),
                },
            },
            ids: vec![],
        }];

        let transcript = transcribe(&history);
        assert!(transcript.len() < 2_000, "length was {}", transcript.len());
        assert!(transcript.contains("bytes elided"), "{transcript}");
    }

    #[test]
    fn truncate_cuts_on_a_char_boundary() {
        // A byte-index slice into the middle of a multi-byte char panics, and a
        // tool result is arbitrary bytes from the workspace: one UTF-8 source
        // file in a `grep` result is enough to reach this.
        let text = "é".repeat(500);
        let cut = truncate(&text, 601);
        assert!(cut.contains("bytes elided"), "{cut}");
    }
}

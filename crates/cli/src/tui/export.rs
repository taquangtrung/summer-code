//! `/copy` and `/export`: getting the conversation out of the alternate screen.
//!
//! The fullscreen view is gone when the session ends, and selecting text in it
//! drags borders and wrapped rows along. `/copy` puts the last reply, or the
//! last code block in it, on the system clipboard through the terminal's OSC 52
//! request, which also works over SSH and inside tmux. `/export` writes the
//! whole transcript to a Markdown file.

use crate::tui::state::{Item, ToolStatus};

// ========================================================================
// Constants
// ========================================================================

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// The fence a code block opens and closes with.
const FENCE: &str = "```";

// ========================================================================
// Data Structures
// ========================================================================

/// What `/copy` takes from the last reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyTarget {
    /// The last fenced code block in it.
    Code,
    /// The whole reply.
    Reply,
}

// ========================================================================
// CopyTarget Implementations
// ========================================================================

impl CopyTarget {
    /// The target `/copy`'s argument names, or `None` for one it does not know.
    pub fn parse(argument: Option<&str>) -> Option<Self> {
        match argument {
            None => Some(Self::Reply),
            Some("code") => Some(Self::Code),
            Some(_) => None,
        }
    }
}

// ========================================================================
// Functions
// ========================================================================

/// What `/copy` would put on the clipboard: from the latest assistant reply,
/// the whole of it or its last code block. `None` when there is no reply yet,
/// or no code block in it.
pub fn copy_text(transcript: &[Item], target: CopyTarget) -> Option<String> {
    let reply = transcript.iter().rev().find_map(|item| match item {
        Item::Assistant(text) if !text.trim().is_empty() => Some(text.as_str()),
        _ => None,
    })?;
    match target {
        CopyTarget::Reply => Some(reply.trim().to_string()),
        CopyTarget::Code => last_code_block(reply),
    }
}

/// The body of the last complete fenced code block in `text`.
fn last_code_block(text: &str) -> Option<String> {
    let mut blocks = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    for line in text.lines() {
        if line.trim_start().starts_with(FENCE) {
            match current.take() {
                Some(body) => blocks.push(body.join("\n")),
                None => current = Some(Vec::new()),
            }
        } else if let Some(body) = current.as_mut() {
            body.push(line);
        }
    }
    blocks.pop()
}

/// The OSC 52 request that sets the system clipboard to `text`, wrapped in
/// tmux's passthrough when `inside_tmux`, since tmux otherwise swallows it.
pub fn clipboard_sequence(text: &str, inside_tmux: bool) -> String {
    let request = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
    if inside_tmux {
        format!("\x1bPtmux;{}\x1b\\", request.replace('\x1b', "\x1b\x1b"))
    } else {
        request
    }
}

/// Standard, padded base64.
fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let triple = chunk.iter().enumerate().fold(0u32, |acc, (index, &byte)| {
            acc | u32::from(byte) << (16 - 8 * index)
        });
        for position in 0..4 {
            if position <= chunk.len() {
                let sextet = (triple >> (18 - 6 * position)) & 0x3f;
                out.push(char::from(BASE64_ALPHABET[sextet as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The transcript as a Markdown document: each turn under a heading, tool
/// calls as a line naming the call with its output fenced below, thinking left
/// out (it is the model's scratch work, not the conversation).
pub fn transcript_markdown(transcript: &[Item]) -> String {
    let mut out = String::from("# summer-code session\n");
    for item in transcript {
        match item {
            Item::User(text) => out.push_str(&format!("\n## You\n\n{}\n", text.trim())),
            Item::Assistant(text) => out.push_str(&format!("\n## Summer\n\n{}\n", text.trim())),
            Item::Thinking { .. } => {}
            Item::Tool(view) => {
                out.push_str(&format!(
                    "\n**{}** `{}` ({})\n",
                    view.name,
                    view.args_summary,
                    status_word(view.status)
                ));
                if let Some(body) = view.body.as_deref().filter(|body| !body.trim().is_empty()) {
                    out.push_str(&format!("\n{FENCE}\n{}\n{FENCE}\n", body.trim_end()));
                }
            }
            Item::Error(message) => out.push_str(&format!("\n> error: {message}\n")),
        }
    }
    out
}

fn status_word(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Denied => "denied",
        ToolStatus::Done => "done",
        ToolStatus::Failed => "failed",
        ToolStatus::Pending => "pending",
        ToolStatus::Running => "running",
        ToolStatus::TimedOut => "timed out",
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base64_matches_the_standard_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64("héllo".as_bytes()), "aMOpbGxv");
    }

    #[test]
    fn test_copy_takes_the_latest_reply_or_its_last_code_block() {
        let transcript = vec![
            Item::Assistant("old reply".into()),
            Item::User("again".into()),
            Item::Assistant("Here:\n```rust\nfn a() {}\n```\nand\n```sh\ncargo test\n```\n".into()),
        ];
        assert!(
            copy_text(&transcript, CopyTarget::Reply)
                .expect("a reply")
                .starts_with("Here:")
        );
        assert_eq!(
            copy_text(&transcript, CopyTarget::Code).as_deref(),
            Some("cargo test")
        );
        assert_eq!(
            copy_text(&[Item::Assistant("no code".into())], CopyTarget::Code),
            None
        );
    }

    #[test]
    fn test_tmux_gets_the_request_through_its_passthrough() {
        let plain = clipboard_sequence("hi", false);
        assert_eq!(plain, "\x1b]52;c;aGk=\x07");
        let wrapped = clipboard_sequence("hi", true);
        assert!(
            wrapped.starts_with("\x1bPtmux;\x1b\x1b]52;c;aGk="),
            "{wrapped:?}"
        );
        assert!(wrapped.ends_with("\x1b\\"));
    }

    #[test]
    fn test_the_export_keeps_the_conversation_and_drops_the_scratch_work() {
        let markdown = transcript_markdown(&[
            Item::User("fix it".into()),
            Item::Thinking {
                text: "hmm".into(),
                collapsed: true,
            },
            Item::Assistant("done".into()),
        ]);
        assert!(markdown.contains("## You\n\nfix it"));
        assert!(markdown.contains("## Summer\n\ndone"));
        assert!(!markdown.contains("hmm"));
    }
}

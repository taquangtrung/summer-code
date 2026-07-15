//! LSP's wire protocol: `Content-Length` framing over a stdio pipe.
//!
//! Separated from the extractor because it is the half that can be tested
//! without a language server installed. Framing bugs are the ones that hang: a
//! length computed over characters rather than bytes, or a header terminator
//! written as `\n` instead of `\r\n`, produces a client that sends a message no
//! server ever answers, and the symptom is a wait rather than an error.

use serde_json::Value;

// ========================================================================
// Constants
// ========================================================================

/// The one header LSP requires. Case-insensitive on the way in, per the spec.
const CONTENT_LENGTH: &str = "content-length:";

/// The largest body a well-formed response can declare. The header is
/// untrusted (a language server's stdout, subject to the same framing bugs as
/// any wire protocol) and drives a single `vec![0u8; length]` allocation in
/// the caller, so an unbounded value here is an unbounded allocation.
const MAX_CONTENT_LENGTH_BYTES: usize = 64 * 1024 * 1024;

// ========================================================================
// Functions
// ========================================================================

/// Frame `message` for the wire.
///
/// The length counts **bytes**, not characters. A `String::len` in Rust is
/// already bytes, which is why this is correct here and is the first thing to
/// check in a client that hangs on a non-ASCII payload.
pub fn frame(message: &Value) -> Vec<u8> {
    let body = message.to_string();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

/// The body length a header block declares, or `None` if it declared none, or
/// declared more than [`MAX_CONTENT_LENGTH_BYTES`].
///
/// Takes the already-collected header lines rather than reading them, so the
/// parsing is testable without a pipe. Anything that is not `Content-Length` is
/// ignored: `Content-Type` is the only other header LSP defines and it carries
/// nothing this client acts on.
pub fn content_length(headers: &[String]) -> Option<usize> {
    let length = headers.iter().find_map(|line| {
        let lower = line.to_ascii_lowercase();
        let rest = lower.strip_prefix(CONTENT_LENGTH)?;
        rest.trim().parse().ok()
    })?;
    (length <= MAX_CONTENT_LENGTH_BYTES).then_some(length)
}

/// A JSON-RPC request object.
pub fn request(id: u64, method: &str, params: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

/// A JSON-RPC notification: a request with no id, which is never answered.
///
/// The distinction is load-bearing. `initialized` and `exit` are notifications,
/// and a client that waits for a reply to one waits forever.
pub fn notification(method: &str, params: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    })
}

/// A `file://` URI for `path`, which is how LSP names documents.
pub fn uri(path: &std::path::Path) -> String {
    format!("file://{}", path.display())
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_frame_declares_its_length_in_bytes_not_characters() {
        // The bug that hangs a client on the first non-ASCII payload: the server
        // reads the declared number of bytes, gets a truncated body, and waits
        // for a completion that never comes.
        let message = json!({ "name": "café" });
        let framed = frame(&message);
        let body = message.to_string();
        assert!(
            body.len() > body.chars().count(),
            "the fixture is non-ASCII"
        );

        let text = String::from_utf8(framed).expect("valid utf-8");
        assert!(
            text.starts_with(&format!("Content-Length: {}\r\n\r\n", body.len())),
            "{text}"
        );
    }

    #[test]
    fn a_frame_terminates_its_headers_with_crlf() {
        // `\n` alone is accepted by some servers and not others, so a client
        // that gets this wrong works in development and hangs in the field.
        let framed = String::from_utf8(frame(&json!({}))).expect("valid utf-8");
        assert!(framed.contains("\r\n\r\n"), "{framed:?}");
    }

    #[test]
    fn a_content_length_header_is_matched_case_insensitively() {
        // The spec says headers are case-insensitive, and servers disagree about
        // which casing they send.
        assert_eq!(content_length(&["Content-Length: 42".into()]), Some(42));
        assert_eq!(content_length(&["content-length: 42".into()]), Some(42));
        assert_eq!(content_length(&["CONTENT-LENGTH:42".into()]), Some(42));
    }

    #[test]
    fn other_headers_do_not_confuse_the_length() {
        let headers = vec![
            "Content-Type: application/vscode-jsonrpc; charset=utf-8".to_string(),
            "Content-Length: 17".to_string(),
        ];
        assert_eq!(content_length(&headers), Some(17));
    }

    #[test]
    fn a_header_block_with_no_length_is_none() {
        assert_eq!(content_length(&["Content-Type: x".into()]), None);
        assert_eq!(content_length(&[]), None);
    }

    #[test]
    fn a_content_length_over_the_cap_is_rejected() {
        // A desynced or malformed header can declare any `usize`; trusting it
        // straight into an allocation is what turns a framing bug into an OOM
        // kill. The cap is the boundary that stops it here instead.
        let header = format!("Content-Length: {}", MAX_CONTENT_LENGTH_BYTES + 1);
        assert_eq!(content_length(&[header]), None);

        let header = format!("Content-Length: {MAX_CONTENT_LENGTH_BYTES}");
        assert_eq!(content_length(&[header]), Some(MAX_CONTENT_LENGTH_BYTES));
    }

    #[test]
    fn a_notification_carries_no_id() {
        // `initialized` and `exit` are notifications. A client that gives them an
        // id and then waits for the reply waits forever.
        let sent = notification("initialized", json!({}));
        assert!(sent.get("id").is_none(), "{sent}");
        assert!(request(1, "initialize", json!({})).get("id").is_some());
    }
}

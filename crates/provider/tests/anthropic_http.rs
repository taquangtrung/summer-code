//! The Anthropic client against a real socket.
//!
//! **This is the least-tested and trickiest logic in the workspace, and its
//! failures are the ones a unit test structurally cannot reach.** The restart
//! path, the retry loop and its attempt budget, header pairing, and the
//! classification of a response all live between `send` and the byte stream, so
//! exercising them needs something on the other end of a socket. Without that,
//! a regression in restart *ordering* — where the second attempt's blocks get
//! appended to the first attempt's instead of replacing them — ships silently
//! and produces a journaled `ModelTurn` the model never generated.
//!
//! The server here is thirty lines of `tokio::net` rather than a mock-HTTP
//! crate. That is deliberate: these tests need to send a *deliberately truncated
//! response body*, which is the whole point of the break-then-restart case, and
//! byte-level control is easier to have than to ask a framework for.

// The listener is the *test standing in for the API server*, not agent-driven
// network access, so it is outside what the broker exists to mediate. This is
// the same reviewable exemption the sandbox's own fixtures carry, and it is
// scoped to a test binary that ships in no release build.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::StreamExt;
use summer_provider::{
    AnthropicProvider, Auth, Provider, ProviderConfig, ProviderError, StreamEvent, TurnRequest,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// ========================================================================
// Data Structures
// ========================================================================

/// What the fake server does with one connection.
struct Scripted {
    /// One raw HTTP response per request, in order. The last is repeated if the
    /// client asks more times than the script has answers.
    responses: Vec<String>,
    /// Every request's headers, so a test can assert on what was sent.
    seen: Arc<std::sync::Mutex<Vec<String>>>,
    served: Arc<AtomicUsize>,
}

// ========================================================================
// Functions
// ========================================================================

/// Serve `responses` on a fresh port, returning the base URL and the recorder.
async fn serve(responses: Vec<String>) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let script = Scripted {
        responses,
        seen: Arc::new(std::sync::Mutex::new(Vec::new())),
        served: Arc::new(AtomicUsize::new(0)),
    };
    let seen = script.seen.clone();

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let index = script.served.fetch_add(1, Ordering::SeqCst);
            let body = script
                .responses
                .get(index)
                .or_else(|| script.responses.last())
                .cloned()
                .unwrap_or_default();

            // Read the request head. Enough to capture the headers and to be
            // sure the client finished writing before we answer.
            let mut buffer = vec![0u8; 8192];
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            script
                .seen
                .lock()
                .expect("lock")
                .push(String::from_utf8_lossy(&buffer[..read]).into_owned());

            let _ = socket.write_all(body.as_bytes()).await;
            let _ = socket.flush().await;
            // Closing without a terminating chunk is what makes a truncated
            // stream truncated.
            drop(socket);
        }
    });

    (format!("http://127.0.0.1:{port}"), seen)
}

/// A chunked 200 carrying `events`, with no terminating chunk when `complete` is
/// false: a body that simply stops, which is what a dropped connection looks
/// like mid-stream.
fn sse_response(events: &str, complete: bool) -> String {
    let mut response = String::from(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
    );
    response.push_str(&format!("{:x}\r\n{events}\r\n", events.len()));
    if complete {
        response.push_str("0\r\n\r\n");
    }
    response
}

fn status_response(status: u16, reason: &str, headers: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// The event sequence for a complete one-word turn.
fn complete_turn(text: &str) -> String {
    format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":5}}}}}}\n\n\
         event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n\
         event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    )
}

/// Half a turn: deltas and then nothing. No `message_stop`.
fn truncated_turn(text: &str) -> String {
    format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":5}}}}}}\n\n\
         event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n"
    )
}

fn provider(base: &str, auth: Auth) -> AnthropicProvider {
    AnthropicProvider::new(
        ProviderConfig::new(auth).with_base_url(base.parse().expect("test url parses")),
    )
}

fn request() -> TurnRequest {
    TurnRequest {
        messages: Vec::new(),
        system: "system".into(),
        tools: Vec::new(),
    }
}

fn api_key() -> Auth {
    Auth::ApiKey("sk-test".to_string().into())
}

/// Drain a turn stream into the text it produced and the events it emitted.
async fn drain(
    mut stream: futures_util::stream::BoxStream<'static, StreamEvent>,
) -> (String, usize, bool) {
    let mut text = String::new();
    let mut restarts = 0;
    let mut ended = false;
    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::TextDelta(delta) => text.push_str(&delta),
            // A restart means everything so far was from a dead attempt and
            // must be discarded -- the same rule the engine follows.
            StreamEvent::TurnRestarted => {
                restarts += 1;
                text.clear();
            }
            StreamEvent::TurnEnd(_) => ended = true,
            _ => {}
        }
    }
    (text, restarts, ended)
}

// ========================================================================
// Tests
// ========================================================================

#[tokio::test]
async fn a_broken_stream_restarts_and_the_dead_attempt_is_discarded() {
    // The regression that would otherwise ship silently: if the restart did not
    // announce itself, the second attempt's text would be appended to the
    // first's and journaled as one `ModelTurn` the model never produced.
    let (base, _) = serve(vec![
        sse_response(&truncated_turn("half"), false),
        sse_response(&complete_turn("whole"), true),
    ])
    .await;

    let stream = provider(&base, api_key())
        .turn(request())
        .await
        .expect("the first response is a 200");
    let (text, restarts, ended) = drain(stream).await;

    assert_eq!(restarts, 1, "the truncated attempt must announce a restart");
    assert_eq!(text, "whole", "the dead attempt's text must not survive");
    assert!(ended, "the second attempt completed the turn");
}

#[tokio::test]
async fn a_rate_limit_is_retried_and_journaled_as_an_attempt() {
    // 429 is retryable and must never reach the loop; the attempt still has to
    // reach the journal, or a retry storm that ended in success is invisible.
    // `retry-after: 0` keeps the test fast and exercises the header path.
    let (base, _) = serve(vec![
        status_response(429, "Too Many Requests", "retry-after: 0\r\n", "{}"),
        sse_response(&complete_turn("after the limit"), true),
    ])
    .await;

    let provider = provider(&base, api_key());
    let stream = provider.turn(request()).await.expect("the retry succeeds");
    let (text, _, ended) = drain(stream).await;

    assert_eq!(text, "after the limit");
    assert!(ended);
    let attempts = provider.drain_attempts();
    assert_eq!(attempts.len(), 1, "the 429 is recorded, not swallowed");
    assert!(
        attempts[0].failure.contains("rate_limited"),
        "{:?}",
        attempts[0].failure
    );
}

#[tokio::test]
async fn a_fatal_status_surfaces_from_turn_rather_than_as_an_empty_stream() {
    // A 400 names the parameter that is wrong. Returning it from `turn` is what
    // lets the caller see that; an empty stream would read as "the model said
    // nothing".
    let (base, _) = serve(vec![status_response(
        400,
        "Bad Request",
        "",
        r#"{"error":{"type":"invalid_request_error","message":"max_tokens too large"}}"#,
    )])
    .await;

    let Err(error) = provider(&base, api_key()).turn(request()).await else {
        panic!("a 400 must not produce a stream");
    };

    let ProviderError::Fatal {
        kind,
        message,
        status,
    } = error
    else {
        panic!("a 400 must classify as Fatal, got {error:?}");
    };
    assert_eq!(status, Some(400));
    assert_eq!(kind, "invalid_request_error");
    assert!(message.contains("max_tokens too large"), "{message}");
}

#[tokio::test]
async fn an_api_key_and_a_bearer_token_send_different_headers() {
    // The two credentials take different headers, and sending the wrong pairing
    // is a 401 that looks like an expired key. The `anthropic-beta` header rides
    // with the bearer token only.
    let (base, seen) = serve(vec![sse_response(&complete_turn("ok"), true)]).await;
    let _ = drain(
        provider(&base, api_key())
            .turn(request())
            .await
            .expect("200"),
    )
    .await;

    let request_text = seen.lock().expect("lock")[0].to_lowercase();
    assert!(
        request_text.contains("x-api-key: sk-test"),
        "{request_text}"
    );
    assert!(
        request_text.contains("anthropic-version:"),
        "{request_text}"
    );
    assert!(
        !request_text.contains("authorization:"),
        "an api key must not also send a bearer token: {request_text}"
    );

    let (base, seen) = serve(vec![sse_response(&complete_turn("ok"), true)]).await;
    let bearer = Auth::BearerToken("oauth-token".to_string().into());
    let _ = drain(provider(&base, bearer).turn(request()).await.expect("200")).await;

    let request_text = seen.lock().expect("lock")[0].to_lowercase();
    assert!(
        request_text.contains("authorization: bearer oauth-token"),
        "{request_text}"
    );
    assert!(request_text.contains("anthropic-beta:"), "{request_text}");
    assert!(
        !request_text.contains("x-api-key:"),
        "a bearer token must not also send an api key: {request_text}"
    );
}

#[tokio::test]
async fn extra_headers_ride_along_with_every_request() {
    // A profile like GitHub Copilot's needs static headers beyond the
    // credential -- its Anthropic-compatible endpoint 401s without an editor
    // identity -- and this is the one hook that adds them.
    let (base, seen) = serve(vec![sse_response(&complete_turn("ok"), true)]).await;
    const HEADERS: &[(&str, &str)] = &[("editor-version", "vscode/1.95.0")];
    let config = ProviderConfig::new(api_key())
        .with_base_url(base.parse().expect("test url parses"))
        .with_extra_headers(HEADERS);
    let _ = drain(
        AnthropicProvider::new(config)
            .turn(request())
            .await
            .expect("200"),
    )
    .await;

    let request_text = seen.lock().expect("lock")[0].to_lowercase();
    assert!(
        request_text.contains("editor-version: vscode/1.95.0"),
        "{request_text}"
    );
}

#[tokio::test]
async fn the_attempt_budget_is_finite() {
    // Every attempt truncates. The stream must end rather than restart forever,
    // and the failures must all be recorded: a turn that gave up after five
    // tries and one that never started look identical without them.
    let (base, _) = serve(vec![sse_response(&truncated_turn("never finishes"), false)]).await;

    let provider = provider(&base, api_key());
    let stream = provider.turn(request()).await.expect("200");
    let (_, restarts, ended) = drain(stream).await;

    assert!(
        !ended,
        "a truncated stream must never report a completed turn"
    );
    assert!(restarts > 0, "it tried again");
    assert!(
        restarts < 10,
        "the budget is finite, saw {restarts} restarts"
    );
    assert!(
        !provider.drain_attempts().is_empty(),
        "the failures reached the journal"
    );
}

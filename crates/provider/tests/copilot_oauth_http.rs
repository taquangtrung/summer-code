//! GitHub's device-flow OAuth and the Copilot session-token exchange, against
//! a real socket.
//!
//! The same reasoning `anthropic_http.rs` gives for testing the chat
//! transport this way applies here: request-shape and response-parsing bugs
//! live between the client and a real HTTP exchange, which a unit test
//! cannot reach — and this is the one part of the Copilot integration that
//! *can* be verified without live Copilot credentials, since GitHub's device
//! flow itself is fully scripted here rather than assumed correct.

#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use secrecy::{ExposeSecret, SecretString};
use summer_provider::{
    CopilotAuthError, PollOutcome, exchange_session_token, poll_device_flow, start_device_flow,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// ========================================================================
// Data Structures
// ========================================================================

struct Scripted {
    responses: Vec<String>,
    seen: Arc<std::sync::Mutex<Vec<String>>>,
    served: Arc<AtomicUsize>,
}

// ========================================================================
// Functions
// ========================================================================

/// Serve `responses` on a fresh port, returning the base URL and the
/// recorder. Mirrors `anthropic_http.rs`'s `serve`, minus the chunked-SSE
/// machinery this module has no use for.
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

            let mut buffer = vec![0u8; 8192];
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            script
                .seen
                .lock()
                .expect("lock")
                .push(String::from_utf8_lossy(&buffer[..read]).into_owned());

            let _ = socket.write_all(body.as_bytes()).await;
            let _ = socket.flush().await;
            drop(socket);
        }
    });

    (format!("http://127.0.0.1:{port}"), seen)
}

fn json_response(status: u16, reason: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

// ========================================================================
// Tests
// ========================================================================

#[tokio::test]
async fn start_device_flow_sends_the_client_id_and_scope() {
    let (base, seen) = serve(vec![json_response(
        200,
        "OK",
        r#"{"device_code":"dc","user_code":"ABCD-1234","verification_uri":"https://github.com/login/device","expires_in":900,"interval":5}"#,
    )])
    .await;

    let client = reqwest::Client::new();
    let code = start_device_flow(&client, &base).await.expect("200");

    assert_eq!(code.user_code, "ABCD-1234");
    assert_eq!(code.verification_uri, "https://github.com/login/device");
    assert_eq!(code.interval, std::time::Duration::from_secs(5));

    let request_text = seen.lock().expect("lock")[0].clone();
    assert!(
        request_text.contains("client_id=Iv1.b507a08c87ecfe98"),
        "{request_text}"
    );
    assert!(request_text.contains("scope=read%3Auser"), "{request_text}");
}

#[tokio::test]
async fn poll_device_flow_distinguishes_pending_from_authorized() {
    let (base, _) = serve(vec![json_response(
        200,
        "OK",
        r#"{"error":"authorization_pending"}"#,
    )])
    .await;
    let client = reqwest::Client::new();
    assert!(matches!(
        poll_device_flow(&client, &base, "dc").await.expect("200"),
        PollOutcome::Pending
    ));

    let (base, _) = serve(vec![json_response(
        200,
        "OK",
        r#"{"access_token":"ghu_test","token_type":"bearer","scope":"read:user"}"#,
    )])
    .await;
    let PollOutcome::Authorized { github_token } =
        poll_device_flow(&client, &base, "dc").await.expect("200")
    else {
        panic!("an access_token response must be Authorized");
    };
    assert_eq!(github_token.expose_secret(), "ghu_test");
}

#[tokio::test]
async fn poll_device_flow_maps_every_documented_github_error_code() {
    let client = reqwest::Client::new();
    for error in ["slow_down", "expired_token", "access_denied"] {
        let (base, _) = serve(vec![json_response(
            200,
            "OK",
            &format!(r#"{{"error":"{error}","interval":10}}"#),
        )])
        .await;
        let outcome = poll_device_flow(&client, &base, "dc").await.expect("200");
        match (error, outcome) {
            ("slow_down", PollOutcome::SlowDown { interval }) => {
                assert_eq!(interval, Some(std::time::Duration::from_secs(10)));
            }
            ("expired_token", PollOutcome::Expired) => {}
            ("access_denied", PollOutcome::Denied) => {}
            (error, _) => panic!("{error} did not map to the expected outcome"),
        }
    }
}

#[tokio::test]
async fn exchange_session_token_derives_the_base_url_from_the_token() {
    let (base, seen) = serve(vec![json_response(
        200,
        "OK",
        r#"{"token":"tid=x;exp=1;proxy-ep=proxy.individual.githubcopilot.com","expires_at":9999999999}"#,
    )])
    .await;

    let client = reqwest::Client::new();
    let github_token = SecretString::new("ghu_test".to_string());
    let session = exchange_session_token(&client, &base, &github_token)
        .await
        .expect("200");

    assert_eq!(
        session.base_url.as_str(),
        "https://api.individual.githubcopilot.com/"
    );

    let request_text = seen.lock().expect("lock")[0].to_lowercase();
    assert!(
        request_text.contains("authorization: token ghu_test"),
        "{request_text}"
    );
    assert!(request_text.contains("editor-version:"), "{request_text}");
}

#[tokio::test]
async fn exchange_session_token_rejects_a_token_with_no_proxy_ep() {
    let (base, _) = serve(vec![json_response(
        200,
        "OK",
        r#"{"token":"tid=x;exp=1","expires_at":9999999999}"#,
    )])
    .await;

    let client = reqwest::Client::new();
    let github_token = SecretString::new("ghu_test".to_string());
    let error = exchange_session_token(&client, &base, &github_token)
        .await
        .expect_err("a token with no proxy-ep must not resolve to some default host");
    assert!(matches!(error, CopilotAuthError::MalformedToken));
}

#[tokio::test]
async fn a_non_2xx_status_is_a_distinct_error_from_a_transport_failure() {
    // The bug this catches: the device-flow poll loop needs to tell "GitHub
    // answered with a real HTTP error" apart from "the network is down", and
    // collapsing both into one error variant would make that impossible.
    let (base, _) = serve(vec![json_response(500, "Internal Server Error", "oops")]).await;
    let client = reqwest::Client::new();
    let error = poll_device_flow(&client, &base, "dc")
        .await
        .expect_err("a 500 must not parse as a poll outcome");
    assert!(matches!(error, CopilotAuthError::Http { status: 500, .. }));
}

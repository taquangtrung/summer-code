//! `web_fetch` against a real loopback server, over the broker-pinned resolver.
//!
//! The unit tests in `web::resolver` prove the resolver refuses a name the
//! broker never authorized. What they cannot prove is the other half: that a
//! name it *did* authorize still reaches the server. The client no longer has a
//! resolver of its own to fall back on, so a pin written under the wrong key, or
//! addresses handed over without their port, does not degrade the fetch -- it
//! makes every fetch of every host fail. That is worth one test with bytes on a
//! wire.

// The fixture server accepts a connection and writes a response: the test
// setting the scene, not a tool reaching for the network. The fetch under test
// goes through the broker, which is the point.
#![allow(clippy::disallowed_methods)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use journal::Outcome;
use sandbox::{Broker, ToolCx};
use serde_json::json;
use summer_tools::{Tool, WebFetch};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// The page the fixture server serves, and what the fetch must come back with.
const PAGE: &str = "<html><body><p>pinned and served</p></body></html>";

/// How long the whole exchange may take before the test calls it hung. Well
/// under the tool's own 30s timeout, so a failure here reads as this test's
/// verdict rather than as a timeout inside `reqwest`.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);

fn cx(root: &std::path::Path) -> ToolCx {
    ToolCx::new(
        Arc::new(Broker::permissive(root.to_path_buf())),
        Arc::from(root),
        CancellationToken::new(),
        Instant::now() + EXCHANGE_TIMEOUT,
    )
}

/// A listener on a loopback port the OS chose, bound through the broker so the
/// fixture needs no exemption of its own.
async fn loopback_server(root: &std::path::Path) -> (TcpListener, SocketAddr) {
    let authorized = Broker::permissive(root.to_path_buf())
        .listen(0)
        .await
        .expect("a permissive broker binds loopback");
    let bound = authorized.bound();
    (authorized.into_listener(), bound)
}

/// Answer exactly one request with [`PAGE`], then drop the connection.
async fn serve_one_page(listener: TcpListener) {
    let (mut stream, _peer) = listener.accept().await.expect("a connection");
    let mut request = [0_u8; 1024];
    // One read is enough: a GET with no body fits, and the response does not
    // depend on what it said.
    let _ = stream.read(&mut request).await.expect("a request");
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{PAGE}",
        PAGE.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .expect("the response");
    stream.flush().await.expect("flushed");
}

/// Fetch `url` through a fresh `web_fetch`, with the fixture server answering.
async fn fetch(root: &std::path::Path, listener: TcpListener, url: String) -> Outcome {
    let server = tokio::spawn(serve_one_page(listener));
    let outcome = tokio::time::timeout(
        EXCHANGE_TIMEOUT,
        WebFetch::new().call(json!({"url": url}), &cx(root)),
    )
    .await
    .expect("the fetch finished inside the exchange timeout");
    server.abort();
    outcome
}

fn content_of(outcome: Outcome) -> String {
    match outcome {
        Outcome::Ok { content } => content,
        other => panic!("the fetch did not succeed: {other:?}"),
    }
}

#[tokio::test]
async fn a_named_host_the_broker_resolved_is_reachable() {
    // `localhost` is a name, so this is the path that goes through the pinned
    // resolver: the broker resolves it, the client asks the resolver for it, and
    // nothing else can answer.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let (listener, bound) = loopback_server(root).await;
    let url = format!("http://localhost:{}/", bound.port());

    let content = content_of(fetch(root, listener, url).await);
    assert!(
        content.contains("pinned and served"),
        "the page did not come back: {content}"
    );
}

#[tokio::test]
async fn an_address_literal_still_reaches_the_server() {
    // The other path: a literal host is connected to directly and never reaches
    // the resolver, which is correct -- there is no second lookup to disagree
    // with the first -- but it has to keep working.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let (listener, bound) = loopback_server(root).await;
    let url = format!("http://{bound}/");

    let content = content_of(fetch(root, listener, url).await);
    assert!(
        content.contains("pinned and served"),
        "the page did not come back: {content}"
    );
}

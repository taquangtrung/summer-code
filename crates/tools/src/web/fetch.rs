//! Reading a web page, under the network policy.
//!
//! **The broker authorizes; reqwest transports.** Every other tool gets its I/O
//! *from* the broker as an authorized handle, and that typestate is what makes
//! "nothing happens unchecked" structural. This tool cannot work that way: an
//! `AuthorizedSocket` is a TCP stream, and turning one into an HTTPS request
//! means owning the TLS handshake, the HTTP/1.1 and HTTP/2 framing, and the
//! content decoding, which is a client library's job and not this crate's.
//!
//! So the check and the transport are separated, and the seam is stated rather
//! than hidden:
//!
//! 1. The URL is parsed and its host and port are put to [`Broker::lookup`],
//!    which evaluates them and resolves them. That is the real policy
//!    evaluation: it prompts the user, it is remembered at whatever scope they
//!    answered, and it is journaled as a `Permission` record exactly like every
//!    other access.
//! 2. Only if that returns does the request go out, to *the host that was
//!    authorized*.
//!
//! **Redirects are the hole this design has to close.** A 302 to another host is
//! a connection the user never approved, so the client is built with redirects
//! disabled and each hop is followed manually, re-entering step 1 with the new
//! host. A page that redirects somewhere the policy denies stops there.
//!
//! **DNS used to be the hole this design could not close.** The broker
//! authorized a hostname and `reqwest` then looked the same name up for itself,
//! so a resolver that answered differently for the two lookups moved bytes to an
//! address nobody had evaluated. It is closed now, and by removing the second
//! lookup rather than by checking it twice: step 1 resolves the host *inside*
//! the broker, and the client is built over [`BrokerResolver`], which serves
//! those addresses and refuses every name it was not given. See
//! [`resolver`](super::resolver) for what that costs and what it leaves.
//!
//! [`Broker::lookup`]: sandbox::Broker::lookup

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use journal::Outcome;
use reqwest::redirect::Policy as RedirectPolicy;
use reqwest::{Client, StatusCode};
use sandbox::ToolCx;
use serde_json::Value;
use url::Url;

use crate::tool::{Concurrency, Tool, ToolSpec};
use crate::web::html::html_to_text;
use crate::web::resolver::{BrokerResolver, client_builder};

// ========================================================================
// Constants
// ========================================================================

const WEB_FETCH_SCHEMA: &str = include_str!("../../tools/web_fetch.json");

/// How much extracted text comes back when the caller does not say.
const DEFAULT_MAX_BYTES: usize = 50_000;

/// The ceiling on `max_bytes`, whatever the caller asks for. A page large
/// enough to exceed this is a page that will blow the context window.
const HARD_MAX_BYTES: usize = 400_000;

/// How long one hop may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How many redirects are followed before giving up.
const MAX_REDIRECTS: usize = 5;

/// The default port per scheme.
const HTTPS_PORT: u16 = 443;
const HTTP_PORT: u16 = 80;

/// Sent so operators can see what is fetching, and so the sites that block
/// unidentified clients have something to allow.
const USER_AGENT: &str = concat!("summer-code/", env!("CARGO_PKG_VERSION"));

// ========================================================================
// Data Structures
// ========================================================================

/// Fetch a URL and return it as readable text.
pub struct WebFetch {
    client: Client,
    /// Held as well as handed to `client`, because this is the end that writes:
    /// every hop authorizes its host here before the client is asked to reach
    /// it.
    resolver: Arc<BrokerResolver>,
    spec: ToolSpec,
}

// ========================================================================
// WebFetch Implementations
// ========================================================================

impl WebFetch {
    pub fn new() -> Self {
        let resolver = BrokerResolver::new();
        Self {
            // `RedirectPolicy::none` is load-bearing: following a redirect
            // inside the client would reach a host the broker never saw.
            client: client_builder(Arc::clone(&resolver))
                .redirect(RedirectPolicy::none())
                .timeout(REQUEST_TIMEOUT)
                .user_agent(USER_AGENT)
                .build()
                .expect("a client with no TLS backend to choose cannot fail to build"),
            resolver,
            spec: ToolSpec::load(WEB_FETCH_SCHEMA),
        }
    }

    /// Ask the broker for `url`'s host, then fetch it, following redirects with
    /// a fresh authorization for each hop.
    async fn fetch(&self, url: Url, cx: &ToolCx, limit: usize) -> Outcome {
        let mut url = url;
        for _hop in 0..=MAX_REDIRECTS {
            let Some(host) = url.host_str().map(str::to_string) else {
                return Outcome::Err {
                    message: format!("`{url}` has no host"),
                };
            };
            let port = match url.port_or_known_default() {
                Some(port) => port,
                None if url.scheme() == "https" => HTTPS_PORT,
                None => HTTP_PORT,
            };

            if let Err(denied) = self.resolver.authorize(cx, &host, port).await {
                return Outcome::Denied {
                    reason: super::denial_advice(&denied.reason, &host),
                };
            }

            let response = match self.client.get(url.clone()).send().await {
                Ok(response) => response,
                Err(error) => {
                    return Outcome::Err {
                        message: format!("{url}: {}", super::request_failure(&error)),
                    };
                }
            };

            let status = response.status();
            if status.is_redirection() {
                match redirect_target(&response, &url) {
                    Some(next) => {
                        url = next;
                        continue;
                    }
                    None => {
                        return Outcome::Err {
                            message: format!("{url}: {status} with no usable Location header"),
                        };
                    }
                }
            }
            return render(response, &url, status, limit).await;
        }
        Outcome::Err {
            message: format!("{url}: more than {MAX_REDIRECTS} redirects"),
        }
    }
}

impl Default for WebFetch {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebFetch {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    /// Reads nothing in the workspace and holds no lock: the wait is the whole
    /// cost, which is exactly the case batching exists for.
    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Outcome {
        let Some(raw) = args.get("url").and_then(Value::as_str) else {
            return Outcome::Err {
                message: "web_fetch requires a `url` argument".into(),
            };
        };
        let url = match Url::parse(raw) {
            Ok(url) => url,
            Err(error) => {
                return Outcome::Err {
                    message: format!("`{raw}` is not a valid URL: {error}"),
                };
            }
        };
        if !matches!(url.scheme(), "http" | "https") {
            // `file:` would read the disk without the path policy ever seeing
            // it, which is the one thing this tool must not become.
            return Outcome::Err {
                message: format!(
                    "web_fetch handles http and https, not `{}`. Use `read` for local files.",
                    url.scheme()
                ),
            };
        }

        let limit = args
            .get("max_bytes")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_MAX_BYTES, |asked| {
                (asked as usize).min(HARD_MAX_BYTES)
            });
        self.fetch(url, cx, limit).await
    }
}

// ========================================================================
// Functions
// ========================================================================

/// The absolute URL a redirect response points at.
fn redirect_target(response: &reqwest::Response, from: &Url) -> Option<Url> {
    let location = response.headers().get(reqwest::header::LOCATION)?;
    // Relative Locations are legal and common, so resolve against the URL the
    // hop came from rather than requiring an absolute one.
    from.join(location.to_str().ok()?).ok()
}

/// Turn a non-redirect response into an outcome.
async fn render(
    response: reqwest::Response,
    url: &Url,
    status: StatusCode,
    limit: usize,
) -> Outcome {
    let is_html = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("html"));

    let body = match response.text().await {
        Ok(body) => body,
        Err(error) => {
            return Outcome::Err {
                message: format!("{url}: reading the body failed: {error}"),
            };
        }
    };

    let text = if is_html { html_to_text(&body) } else { body };
    let (text, truncated) = truncate(text, limit);

    // A 404 body is the site's error page, and returning it as an ordinary
    // success would let a model quote "page not found" as documentation.
    if !status.is_success() {
        return Outcome::Err {
            message: format!("{url}: {status}\n\n{text}"),
        };
    }
    Outcome::Ok {
        content: format!("{url}\n\n{text}{truncated}"),
    }
}

/// Cut `text` to `limit` bytes on a character boundary, reporting whether it
/// was cut.
fn truncate(text: String, limit: usize) -> (String, &'static str) {
    if text.len() <= limit {
        return (text, "");
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut text = text;
    text.truncate(end);
    (
        text,
        "\n\n[truncated: raise `max_bytes` or fetch a more specific page]",
    )
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_lands_on_a_character_boundary() {
        // Cutting mid-codepoint panics on `truncate`. A page of CJK text hits
        // this on nearly every limit.
        let text = "日本語のテキスト".to_string();
        let (cut, marker) = truncate(text, 7);
        assert!(cut.len() <= 7, "cut past the limit: {}", cut.len());
        assert!(!marker.is_empty(), "a cut page should say so");
    }

    #[test]
    fn text_within_the_limit_is_returned_whole_and_unmarked() {
        let (kept, marker) = truncate("short".to_string(), 50);
        assert_eq!(kept, "short");
        assert!(
            marker.is_empty(),
            "an intact page must not claim truncation"
        );
    }

    #[test]
    fn a_relative_location_resolves_against_the_hop_it_came_from() {
        // A `Location: /v2/docs` is legal and common. Treating it as absolute
        // fails to parse and the redirect silently dead-ends.
        let from = Url::parse("https://example.test/v1/docs?x=1").expect("a valid URL");
        let joined = from.join("/v2/docs").expect("a relative join");
        assert_eq!(joined.as_str(), "https://example.test/v2/docs");
    }
}

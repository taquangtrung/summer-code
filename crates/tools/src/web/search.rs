//! Web search, against a configurable JSON search API.
//!
//! **The endpoint is configuration, not a hard-coded vendor.** Search APIs are
//! paid, keyed, and swapped often, and a tool that names one in its source makes
//! that choice for every user of this binary. [`SearchConfig`] carries the
//! endpoint and key; the shipped default speaks Brave's response shape because
//! it needed one, and pointing `SUMMER_SEARCH_ENDPOINT` at any API returning the
//! same `{web: {results: [...]}}` shape works without a rebuild.
//!
//! **An unconfigured key is an ordinary answer.** The tool is registered either
//! way and reports what to set, because a tool the model cannot see is a tool
//! whose absence it explains by guessing from memory instead.

use std::env;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use journal::Outcome;
use reqwest::Client;
use sandbox::ToolCx;
use serde_json::Value;
use url::Url;

use crate::tool::{Concurrency, Tool, ToolSpec};
use crate::web::resolver::{BrokerResolver, client_builder};

// ========================================================================
// Constants
// ========================================================================

const WEB_SEARCH_SCHEMA: &str = include_str!("../../tools/web_search.json");

/// Where the API key is read from.
const KEY_VAR: &str = "SUMMER_SEARCH_API_KEY";

/// Where an alternative endpoint is read from.
const ENDPOINT_VAR: &str = "SUMMER_SEARCH_ENDPOINT";

/// The endpoint used when [`ENDPOINT_VAR`] is unset.
const DEFAULT_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";

/// The header the default endpoint expects its key in.
const KEY_HEADER: &str = "X-Subscription-Token";

const DEFAULT_COUNT: u64 = 10;
const MAX_COUNT: u64 = 20;

/// How long a search may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// What the tool says when it has no key.
const UNCONFIGURED: &str = concat!(
    "web_search has no API key configured, so it cannot search. Set ",
    "SUMMER_SEARCH_API_KEY (and optionally SUMMER_SEARCH_ENDPOINT, which ",
    "defaults to the Brave Search API). Until then, use web_fetch against a ",
    "known documentation URL."
);

// ========================================================================
// Data Structures
// ========================================================================

/// Search the web for pages to then read.
pub struct WebSearch {
    client: Client,
    config: SearchConfig,
    /// The endpoint's host is authorized here before the client is asked to
    /// reach it, and the client can reach nothing else -- which is also what
    /// stops a redirect off the search endpoint leaving for a host the policy
    /// never saw.
    resolver: Arc<BrokerResolver>,
    spec: ToolSpec,
}

/// Where to search and with what credential.
///
/// The key is not in `Debug` output and not journaled: it reaches exactly one
/// header on one request.
pub struct SearchConfig {
    endpoint: String,
    key: Option<String>,
}

// ========================================================================
// WebSearch Implementations
// ========================================================================

impl WebSearch {
    pub fn new() -> Self {
        Self::with_config(SearchConfig::from_env())
    }

    pub fn with_config(config: SearchConfig) -> Self {
        let resolver = BrokerResolver::new();
        Self {
            client: client_builder(Arc::clone(&resolver))
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("a client with no TLS backend to choose cannot fail to build"),
            config,
            resolver,
            spec: ToolSpec::load(WEB_SEARCH_SCHEMA),
        }
    }
}

impl Default for WebSearch {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebSearch {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Outcome {
        let Some(query) = args.get("query").and_then(Value::as_str) else {
            return Outcome::Err {
                message: "web_search requires a `query` argument".into(),
            };
        };
        let Some(key) = self.config.key.as_deref() else {
            return Outcome::Err {
                message: UNCONFIGURED.to_string(),
            };
        };
        let count = args
            .get("count")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_COUNT)
            .min(MAX_COUNT);

        let Ok(endpoint) = Url::parse(&self.config.endpoint) else {
            return Outcome::Err {
                message: format!(
                    "{ENDPOINT_VAR} is not a valid URL: {}",
                    self.config.endpoint
                ),
            };
        };
        // The search endpoint is a network access like any other: the policy
        // sees it, the user can deny it, and the journal records it.
        let host = endpoint.host_str().unwrap_or_default().to_string();
        let port = endpoint.port_or_known_default().unwrap_or(443);
        if let Err(denied) = self.resolver.authorize(cx, &host, port).await {
            return Outcome::Denied {
                reason: super::denial_advice(&denied.reason, &host),
            };
        }

        let response = self
            .client
            .get(endpoint)
            .header(KEY_HEADER, key)
            .header(reqwest::header::ACCEPT, "application/json")
            .query(&[("q", query), ("count", &count.to_string())])
            .send()
            .await;

        let response = match response {
            Ok(response) => response,
            Err(error) => {
                return Outcome::Err {
                    message: format!("search failed: {}", super::request_failure(&error)),
                };
            }
        };
        let status = response.status();
        let body: Value = match response.json().await {
            Ok(body) => body,
            Err(error) => {
                return Outcome::Err {
                    message: format!("search returned {status} and an unreadable body: {error}"),
                };
            }
        };
        if !status.is_success() {
            return Outcome::Err {
                message: format!("search returned {status}: {body}"),
            };
        }
        Outcome::Ok {
            content: format_results(&body, query),
        }
    }
}

// ========================================================================
// SearchConfig Implementations
// ========================================================================

impl SearchConfig {
    /// Read the endpoint and key from the environment.
    pub fn from_env() -> Self {
        Self {
            endpoint: env::var(ENDPOINT_VAR).unwrap_or_else(|_| DEFAULT_ENDPOINT.to_string()),
            key: configured_key(env::var(KEY_VAR).ok()),
        }
    }
}

// ========================================================================
// Functions
// ========================================================================

/// A raw environment value as a usable key, if it is one.
///
/// An exported-but-blank variable is unset. `export SUMMER_SEARCH_API_KEY=` is
/// how a shell clears one, and treating the empty string as a credential turns
/// the message that says what to set into a 401 the user has to decode.
fn configured_key(raw: Option<String>) -> Option<String> {
    raw.filter(|key| !key.trim().is_empty())
}

/// Render the API's results as the title, URL, and snippet lines a model reads.
fn format_results(body: &Value, query: &str) -> String {
    let results = body
        .get("web")
        .and_then(|web| web.get("results"))
        .and_then(Value::as_array);
    let Some(results) = results.filter(|results| !results.is_empty()) else {
        return format!("no results for `{query}`");
    };

    let mut out = format!("results for `{query}`:\n");
    for result in results {
        let field = |key: &str| result.get(key).and_then(Value::as_str).unwrap_or("");
        let title = field("title");
        let url = field("url");
        let description = field("description");
        out.push_str(&format!("\n{title}\n{url}\n"));
        if !description.is_empty() {
            out.push_str(&format!(
                "{}\n",
                crate::web::html::html_to_text(description)
            ));
        }
    }
    out
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn results_render_title_url_and_snippet() {
        let body = json!({
            "web": { "results": [
                { "title": "Tokio", "url": "https://tokio.rs", "description": "An <strong>async</strong> runtime" },
            ]}
        });
        let out = format_results(&body, "tokio");
        assert!(out.contains("Tokio"), "{out}");
        assert!(out.contains("https://tokio.rs"), "{out}");
        // Snippets come back with markup in them; passing it through would show
        // the model tags it then has to reason around.
        assert!(out.contains("async runtime"), "{out}");
        assert!(!out.contains("<strong>"), "{out}");
    }

    #[test]
    fn an_empty_result_set_says_so_rather_than_returning_a_header() {
        // Returning just "results for `x`:" reads like a successful search that
        // found something, and a model will act as though it did.
        let out = format_results(&json!({ "web": { "results": [] } }), "nothing");
        assert!(out.contains("no results"), "{out}");
    }

    #[test]
    fn a_response_missing_the_expected_shape_is_not_a_panic() {
        let out = format_results(&json!({ "error": "quota exceeded" }), "anything");
        assert!(out.contains("no results"), "{out}");
    }

    #[test]
    fn a_blank_key_variable_counts_as_unset() {
        assert_eq!(configured_key(None), None);
        assert_eq!(configured_key(Some(String::new())), None, "empty");
        assert_eq!(configured_key(Some("  \t ".into())), None, "whitespace");
        assert_eq!(
            configured_key(Some("bsk-real".into())),
            Some("bsk-real".to_string())
        );
    }
}

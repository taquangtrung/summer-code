//! The web tools: fetching a page and searching for one.
//!
//! Both reach the network, so both put their host to the broker before any
//! bytes move. See [`fetch`] for why the authorization and the transport are
//! separate steps here and nowhere else in this crate, and [`resolver`] for
//! what keeps that seam from being a second, unchecked name lookup.
//!
//! **Outbound network is off until a policy turns it on**, which is the stance
//! `NetworkRule::default` states and this module does not relitigate. It does
//! mean a fresh install meets a denial on its first fetch, so the denial says
//! what to write to lift it: a rule the user can act on is the difference
//! between a deliberate default and a broken tool.

mod fetch;
mod html;
mod resolver;
mod search;

pub use fetch::WebFetch;
pub use search::{SearchConfig, WebSearch};

use std::error::Error;

// ========================================================================
// Constants
// ========================================================================

/// Where a user's own policy lives, relative to the summer config directory.
const USER_POLICY: &str = "~/.summer/policy.json";

// ========================================================================
// Functions
// ========================================================================

/// Why a request failed, cause chain and all.
///
/// **`reqwest::Error`'s own `Display` stops at the top.** A refused connection
/// prints as "error sending request for url (...)", which names the URL the
/// caller already knew and not one word about what went wrong -- and this
/// crate's most interesting failure is at the bottom of that chain: the
/// resolver refusing a host the broker never authorized. The model reads this
/// string and decides whether to retry, so the cause has to be in it.
fn request_failure(error: &reqwest::Error) -> String {
    let mut message = error.to_string();
    let mut cause = error.source();
    while let Some(error) = cause {
        message.push_str(&format!(": {error}"));
        cause = error.source();
    }
    message
}

/// A policy denial, plus the rule that would allow this host.
///
/// The model reads this too, so it opens with the refusal: the point is that it
/// stops, not that it retries with a different spelling. The remedy is addressed
/// to the user, who is the only one who can apply it -- the agent cannot write
/// its own policy, which is the deny on `**/.summer/**` doing its job.
fn denial_advice(reason: &str, host: &str) -> String {
    format!(
        "{reason}. To allow it, add to {USER_POLICY}:\n\
         {{\"network\": {{\"connect\": [{{\"effect\": \"allow\", \"host\": \"{host}\"}}]}}}}"
    )
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::resolver::{BrokerResolver, client_builder};
    use super::*;

    #[tokio::test]
    async fn a_request_failure_carries_its_causes() {
        // The load-bearing case: `reqwest`'s own `Display` for this error is
        // "error sending request for url (...)", and the resolver's refusal --
        // the reason the fetch stopped -- is two links down the chain.
        let client = client_builder(BrokerResolver::new())
            .build()
            .expect("a client builds");
        let error = client
            .get("http://unauthorized.test/")
            .send()
            .await
            .expect_err("an unauthorized host cannot be fetched");

        let message = request_failure(&error);
        assert!(
            message.contains("was not authorized by the broker"),
            "the cause never reached the message: {message}"
        );
    }

    #[test]
    fn the_advice_names_the_denied_host_and_stays_valid_json() {
        // Advice that does not parse is worse than none: the user pastes it,
        // the session refuses to start, and the cause is this string.
        let advice = denial_advice("outbound network is not allowed", "docs.rs");
        assert!(
            advice.starts_with("outbound network is not allowed"),
            "{advice}"
        );

        let json = advice
            .split_once('\n')
            .map(|(_, rule)| rule)
            .expect("the advice has a rule on its own line");
        let parsed: serde_json::Value =
            serde_json::from_str(json).unwrap_or_else(|error| panic!("{error}: {json}"));
        assert_eq!(parsed["network"]["connect"][0]["host"], "docs.rs");
    }
}

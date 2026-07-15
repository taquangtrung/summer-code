//! The DNS the web tools' HTTP client sees: the broker's answers, and nothing
//! else.
//!
//! `reqwest` owns the transport here (see [`fetch`] for why it has to), and a
//! client that does its own name lookup reopens the check the broker just made.
//! The policy evaluates `docs.rs`, the client resolves `docs.rs` a moment
//! later, and nothing makes the two answers agree: a poisoned cache, a hostile
//! resolver, or a one-second TTL and a cooperative authoritative server is
//! enough for the bytes to move to an address nobody evaluated.
//!
//! So the client is built over [`BrokerResolver`], which resolves nothing at
//! all. It replays what [`sandbox::Broker::lookup`] returned and fails every
//! name it was not given, which makes "the client reaches only what the broker
//! resolved" a property of the type rather than of the order two calls happen
//! to be written in.
//!
//! Two consequences worth stating rather than discovering:
//!
//! **A URL whose host is an IP literal never reaches this resolver.** The
//! client connects to it directly, and that is not a gap: the authorization ran
//! on that same literal, and there is no second lookup to disagree with the
//! first.
//!
//! **Proxies are off.** An `HTTP_PROXY` in the environment would send every
//! request to a host the policy never saw, with the authorized host demoted to
//! a request line -- a client reporting a check it did not make. Honouring one
//! would mean authorizing the proxy itself through the broker, which is a
//! policy question and not a resolver's business.
//!
//! [`fetch`]: super::fetch

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::{Client, ClientBuilder};
use sandbox::{Denied, ToolCx};

// ========================================================================
// Data Structures
// ========================================================================

/// The addresses the broker has authorized this session, by host.
///
/// One map per tool rather than one per request, because `reqwest` keeps a
/// connection pool and a client is built once. An entry is only ever written
/// from a [`sandbox::Broker::lookup`] that returned, so a stale entry names a
/// host the policy allowed and a fresh authorization of the same host
/// overwrites it.
///
/// The property that holds is therefore "every connection goes to an address
/// the broker resolved for a host the policy allowed", not "to the newest
/// address": a pooled connection from an earlier fetch is reused without
/// consulting this map at all, and it was opened against an answer the broker
/// gave for the same host.
pub(super) struct BrokerResolver {
    pins: RwLock<HashMap<String, Vec<SocketAddr>>>,
}

// ========================================================================
// BrokerResolver Implementations
// ========================================================================

impl BrokerResolver {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            pins: RwLock::new(HashMap::new()),
        })
    }

    /// Put `host` and `port` to the broker, and pin what it resolved them to.
    ///
    /// **The only way the map grows.** Authorizing and pinning in one call is
    /// what stops the two drifting apart: a caller cannot check the policy and
    /// then let the client look the name up for itself, because checking the
    /// policy is how the client gets an address at all.
    pub(super) async fn authorize(&self, cx: &ToolCx, host: &str, port: u16) -> Result<(), Denied> {
        let authorized = cx.broker.lookup(host, port).await?;
        self.pins_mut().insert(
            authorized.host().to_ascii_lowercase(),
            authorized.addrs().to_vec(),
        );
        Ok(())
    }

    /// The addresses pinned for `host`, if it has been authorized.
    ///
    /// Lower-cased on both sides: DNS names are case-insensitive, and a
    /// `Docs.RS` in the URL must not miss the pin written for `docs.rs` --
    /// missing it would read as a denial for a host the user allowed.
    fn pinned(&self, host: &str) -> Option<Vec<SocketAddr>> {
        self.pins_read().get(&host.to_ascii_lowercase()).cloned()
    }

    /// A poisoned lock is recovered rather than propagated, the way the
    /// dispatcher's ledger is: the map holds authorizations that already
    /// happened, a panic mid-`insert` cannot leave a half-written entry, and
    /// propagating would turn one tool's panic into a session that can no
    /// longer fetch anything.
    fn pins_mut(&self) -> impl std::ops::DerefMut<Target = HashMap<String, Vec<SocketAddr>>> + '_ {
        self.pins
            .write()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn pins_read(&self) -> impl std::ops::Deref<Target = HashMap<String, Vec<SocketAddr>>> + '_ {
        self.pins
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

impl Resolve for BrokerResolver {
    fn resolve(&self, name: Name) -> Resolving {
        // Looked up before the future is built, so the lock is never held
        // across an await point.
        let pinned = self.pinned(name.as_str());
        let host = name.as_str().to_string();
        Box::pin(async move {
            match pinned {
                Some(addrs) => Ok(Box::new(addrs.into_iter()) as Addrs),
                // Not a resolution failure: it is this crate refusing to look
                // up a name the broker never authorized, and the message says
                // so because it reaches the model as the tool's error.
                None => Err(format!(
                    "{host} was not authorized by the broker, so this client has no address for it"
                )
                .into()),
            }
        })
    }
}

// ========================================================================
// Functions
// ========================================================================

/// The client both web tools are built from: one whose DNS is `resolver`'s and
/// whose proxy configuration is nobody's.
///
/// Timeouts, redirect policy and user agent stay with the tools, which want
/// different answers; these two are the ones neither may choose.
pub(super) fn client_builder(resolver: Arc<BrokerResolver>) -> ClientBuilder {
    Client::builder().no_proxy().dns_resolver(resolver)
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    fn resolver_with(host: &str, addr: SocketAddr) -> Arc<BrokerResolver> {
        let resolver = BrokerResolver::new();
        resolver.pins_mut().insert(host.to_string(), vec![addr]);
        resolver
    }

    fn addr(last_octet: u8, port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, last_octet)), port)
    }

    #[tokio::test]
    async fn an_unpinned_name_is_refused_rather_than_looked_up() {
        // The whole point of the type. A fallback to the system resolver here
        // would restore the second lookup this resolver exists to remove, and
        // every test below would still pass.
        let resolver = BrokerResolver::new();
        let resolved = resolver
            .resolve("localhost".parse().expect("a valid DNS name"))
            .await;
        assert!(
            resolved.is_err(),
            "a name the broker never authorized was resolved anyway"
        );
    }

    #[tokio::test]
    async fn a_pinned_name_resolves_to_exactly_what_was_pinned() {
        let pinned = addr(9, 8080);
        let resolver = resolver_with("example.test", pinned);
        let resolved: Vec<SocketAddr> = resolver
            .resolve("example.test".parse().expect("a valid DNS name"))
            .await
            .expect("a pinned name resolves")
            .collect();
        assert_eq!(resolved, vec![pinned]);
    }

    #[tokio::test]
    async fn a_name_that_differs_only_in_case_finds_its_pin() {
        // A `Docs.RS` in a URL is the same host as `docs.rs`, and reporting it
        // as unauthorized would deny a host the user allowed.
        let pinned = addr(1, 443);
        let resolver = resolver_with("docs.rs", pinned);
        let resolved: Vec<SocketAddr> = resolver
            .resolve("Docs.RS".parse().expect("a valid DNS name"))
            .await
            .expect("case must not lose the pin")
            .collect();
        assert_eq!(resolved, vec![pinned]);
    }
}

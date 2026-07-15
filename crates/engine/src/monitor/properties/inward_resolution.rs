//! "A host allowed by name does not resolve into this machine."

use std::net::{IpAddr, Ipv6Addr};

use journal::{Access, AccessConnect, Record, RecordKind};

use crate::monitor::properties::allowed;
use crate::monitor::property::{Property, Violation};

// ========================================================================
// Constants
// ========================================================================

const NAME: &str = "inward-resolution";

/// The name that means the loopback interface on purpose.
///
/// A coding agent connects to its own dev server, its own test fixture and its
/// own language server constantly, so a policy allowing `localhost` and a
/// resolution to `127.0.0.1` is the system working. Flagging it would bury the
/// case this property exists for under noise.
const LOOPBACK_NAME: &str = "localhost";

/// `foo.localhost` is reserved to the loopback interface by RFC 6761, and
/// resolvers answer it accordingly. Treated like the bare name for the same
/// reason.
const LOOPBACK_SUFFIX: &str = ".localhost";

// ========================================================================
// Data Structures
// ========================================================================

/// Flags a connect that was **allowed** to a named host which resolved to an
/// address inside the machine.
///
/// **The gap this closes is a policy expressiveness gap, not a broker bug.**
/// Network rules match hosts by name, so `allow docs.example.com` says nothing
/// about where that name points; a resolver answering `127.0.0.1` turns an
/// approved documentation fetch into a request against whatever the developer
/// happens to be running locally, and `169.254.169.254` turns it into a cloud
/// credential read. Neither reaches a second prompt, because the host was
/// approved and the address was never part of the question.
///
/// The broker cannot refuse these on its own: a policy that allows a host by
/// name has allowed it, and guessing which answers the user "really meant" is
/// not the broker's call. What the monitor can do is say it happened, in the
/// journal, next to the permission that let it through.
///
/// Private ranges (`10/8`, `192.168/16`, IPv6 unique-local) are deliberately
/// **not** flagged. Split-horizon DNS pointing an internal hostname at an
/// internal address is ordinary corporate practice, so including them would
/// make the property fire on correct behaviour, which is how a signal gets
/// switched off.
pub struct InwardResolution;

// ========================================================================
// InwardResolution Implementations
// ========================================================================

impl InwardResolution {
    pub fn new() -> Self {
        Self
    }
}

impl Default for InwardResolution {
    fn default() -> Self {
        Self::new()
    }
}

impl Property for InwardResolution {
    fn name(&self) -> &'static str {
        NAME
    }

    fn observe(&mut self, record: &Record) -> Option<Violation> {
        let RecordKind::Permission(permission) = &record.kind else {
            return None;
        };
        let Access::Connect(connect) = &permission.access else {
            return None;
        };
        if !allowed(permission.decision) {
            return None;
        }
        check(connect)
    }
}

// ========================================================================
// Functions
// ========================================================================

/// The violation a connect record carries, if it carries one.
fn check(connect: &AccessConnect) -> Option<Violation> {
    if names_loopback(&connect.host) || connect.host.parse::<IpAddr>().is_ok() {
        // An address literal was evaluated as itself: whatever the policy said
        // about `127.0.0.1`, it said about the address that was reached. There
        // is no name in between for a resolver to answer differently.
        return None;
    }
    let inward: Vec<String> = connect
        .addrs
        .iter()
        .filter(|addr| is_inward(addr))
        .map(IpAddr::to_string)
        .collect();
    if inward.is_empty() {
        return None;
    }
    Some(Violation::new(
        NAME,
        format!(
            "allowed connect to `{}:{}`, whose name resolved to {}",
            connect.host,
            connect.port,
            inward.join(", ")
        ),
    ))
}

/// Whether `host` is a name that means the loopback interface.
fn names_loopback(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == LOOPBACK_NAME || host.ends_with(LOOPBACK_SUFFIX)
}

/// Whether reaching `addr` means reaching this machine or its link.
///
/// Loopback is the local-service case. Link-local covers the cloud metadata
/// endpoints (`169.254.169.254` and its IPv6 form), which are the highest-value
/// target on any hosted machine: no credential, reachable over plain HTTP, and
/// happy to mint one. The unspecified address is here because connecting to
/// `0.0.0.0` is loopback on Linux.
fn is_inward(addr: &IpAddr) -> bool {
    if addr.is_loopback() || addr.is_unspecified() {
        return true;
    }
    match addr {
        IpAddr::V4(addr) => addr.is_link_local(),
        // `Ipv6Addr::is_unicast_link_local` is still unstable, and `fe80::/10`
        // is what it checks: the top ten bits.
        IpAddr::V6(addr) => is_link_local_v6(addr),
    }
}

fn is_link_local_v6(addr: &Ipv6Addr) -> bool {
    (addr.segments()[0] & 0xffc0) == 0xfe80
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use journal::{Decision, Scope};

    use super::*;
    use crate::monitor::testing::{call_id, permission_record, tool_result_record};

    fn connect_record(host: &str, addrs: Vec<IpAddr>, decision: Decision) -> Record {
        permission_record(
            call_id("toolu_1"),
            Access::Connect(AccessConnect::new(addrs, host.to_string(), 443)),
            decision,
            Scope::Once,
        )
    }

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn a_named_host_resolving_to_loopback_is_flagged() {
        let record = connect_record(
            "docs.example.com",
            vec![v4(127, 0, 0, 1)],
            Decision::Allowed,
        );
        let violation = InwardResolution::new()
            .observe(&record)
            .expect("a name pointing at loopback must be flagged");
        assert_eq!(violation.property, NAME);
        assert!(
            violation.detail.contains("127.0.0.1"),
            "the address is the fact worth reporting: {}",
            violation.detail
        );
    }

    #[test]
    fn the_cloud_metadata_address_is_flagged() {
        // The reason this property is worth its weight: link-local carries the
        // instance credential endpoint, reachable over plain HTTP with no
        // credential of its own.
        let record = connect_record(
            "metadata.example.com",
            vec![v4(169, 254, 169, 254)],
            Decision::AskedAllowed,
        );
        assert!(InwardResolution::new().observe(&record).is_some());
    }

    #[test]
    fn one_inward_answer_among_several_is_still_flagged() {
        // A rebinding answer arrives beside a plausible one, precisely so that
        // a check looking at the first address sees nothing wrong.
        let record = connect_record(
            "docs.example.com",
            vec![v4(93, 184, 216, 34), v4(127, 0, 0, 1)],
            Decision::Allowed,
        );
        let violation = InwardResolution::new()
            .observe(&record)
            .expect("an inward answer anywhere in the list must be flagged");
        assert!(!violation.detail.contains("93.184.216.34"));
    }

    #[test]
    fn localhost_by_name_is_the_system_working() {
        // The agent runs its own test server and connects to it. Flagging this
        // would make the property fire on nearly every session.
        let record = connect_record("localhost", vec![v4(127, 0, 0, 1)], Decision::Allowed);
        assert!(InwardResolution::new().observe(&record).is_none());
        let sub = connect_record("app.localhost", vec![v4(127, 0, 0, 1)], Decision::Allowed);
        assert!(InwardResolution::new().observe(&sub).is_none());
    }

    #[test]
    fn an_address_literal_is_not_a_resolution() {
        // Nothing came between the policy and the address, so there is no
        // second answer to disagree with the first.
        let record = connect_record("127.0.0.1", vec![v4(127, 0, 0, 1)], Decision::Allowed);
        assert!(InwardResolution::new().observe(&record).is_none());
    }

    #[test]
    fn a_denied_connect_is_the_system_working() {
        // Same stance as every other property: the check was reached and the
        // answer was no.
        let record = connect_record("docs.example.com", vec![v4(127, 0, 0, 1)], Decision::Denied);
        assert!(InwardResolution::new().observe(&record).is_none());
    }

    #[test]
    fn an_outward_answer_passes() {
        let record = connect_record(
            "docs.example.com",
            vec![v4(93, 184, 216, 34)],
            Decision::Allowed,
        );
        assert!(InwardResolution::new().observe(&record).is_none());
    }

    #[test]
    fn a_private_address_is_not_flagged() {
        // Split-horizon DNS is ordinary practice, and a property that fires on
        // ordinary practice gets switched off.
        let record = connect_record(
            "wiki.corp.example",
            vec![v4(10, 1, 2, 3)],
            Decision::Allowed,
        );
        assert!(InwardResolution::new().observe(&record).is_none());
    }

    #[test]
    fn non_permission_records_are_ignored() {
        let record = tool_result_record(
            call_id("toolu_1"),
            journal::Outcome::Ok {
                content: "ok".into(),
            },
        );
        assert!(InwardResolution::new().observe(&record).is_none());
    }
}

//! Converting a broker decision into a journal record.
//!
//! **The journal and the policy keep separate vocabularies on purpose.**
//! `journal` sits at the bottom of the dependency graph and must not name
//! `policy`'s types, or it stops being a contract and becomes a coupling:
//! the monitor joins `Permission` against `ToolResult` without linking the
//! policy crate at all. So `Request`/`Decision`/`Scope` have structural twins in
//! the journal, and the engine converts here, at the seam, because the engine is
//! the one place that depends on both.
//!
//! `Scope` is the conversion worth reading twice. The policy's version is a
//! structured matcher; the journal's is the rule **as written**. That is
//! deliberate: a journal that named the matcher would make every rule-syntax
//! change a schema migration.

use std::net::{IpAddr, Ipv4Addr};

use journal::{Access, AccessConnect, AccessExec, AccessListen, Decision, Permission, Scope};
use policy::{Decision as PolicyDecision, PathAccess, Request, RuleSpec, Scope as PolicyScope};
use sandbox::{DecisionSource, Grant, Supplied};

// ========================================================================
// Functions
// ========================================================================

/// What was asked, in the journal's vocabulary.
///
/// `supplied` is the half the request does not carry, because the broker chose
/// it: the address a listen was bound to, and the addresses a connect's host
/// resolved to. The record carries both so the audit trail can tell a loopback
/// bind from a wildcard one, and a name that resolved outward from one that
/// resolved into the machine the agent is running on.
pub fn access(request: &Request, supplied: &Supplied) -> Access {
    match request {
        Request::Read(path) => Access::Read { path: path.clone() },
        Request::Write(path) => Access::Write { path: path.clone() },
        Request::Exec { args, program } => {
            Access::Exec(AccessExec::new(program.clone(), args.clone()))
        }
        Request::Connect { host, port } => {
            Access::Connect(AccessConnect::new(resolved(supplied), host.clone(), *port))
        }
        Request::Listen { port } => Access::Listen(AccessListen::new(bound(supplied), *port)),
    }
}

/// What was decided.
///
/// `remembered` distinguishes "the user said yes to this" from "the user said
/// yes to something like this earlier", which is a distinction the journal has to
/// carry: the second is an auto-allow that never reached a prompt, and a reader
/// who cannot tell them apart cannot audit what the user actually agreed to.
///
/// An `Ask` reaching here would mean the broker failed to resolve one. It maps
/// to `Denied`, because that is what the broker did with it.
pub fn decision(decision: &PolicyDecision, source: DecisionSource) -> Decision {
    match (decision, source) {
        (PolicyDecision::Allow, DecisionSource::Confined) => Decision::ConfinedAllow,
        (PolicyDecision::Allow, DecisionSource::Policy) => Decision::Allowed,
        (PolicyDecision::Allow, DecisionSource::Prompt) => Decision::AskedAllowed,
        (PolicyDecision::Allow, DecisionSource::Remembered) => Decision::RememberedAllow,
        // The broker only ever produces `Confined` for an allow, so a denial
        // carrying it is unreachable. It folds to a plain `Denied` rather than
        // being an `unreachable!`: an audit record is worth more than a panic.
        (PolicyDecision::Deny { .. }, DecisionSource::Confined) => Decision::Denied,
        (PolicyDecision::Deny { .. }, DecisionSource::Policy) => Decision::Denied,
        (PolicyDecision::Deny { .. }, DecisionSource::Prompt) => Decision::AskedDenied,
        (PolicyDecision::Deny { .. }, DecisionSource::Remembered) => Decision::RememberedDeny,
        (PolicyDecision::Ask, _) => Decision::Denied,
    }
}

/// The reach of the answer, rendered as the rule text the user agreed to.
pub fn scope(scope: &PolicyScope) -> Scope {
    match scope {
        PolicyScope::Once => Scope::Once,
        PolicyScope::Rule(rule) => Scope::Rule { rule: render(rule) },
    }
}

/// Fold a journaled [`Permission`] back into the grant the broker caches.
///
/// The reverse of [`access`] / [`decision`] / [`scope`], used only on resume:
/// the grant set is a derived cache, so it is rebuilt from the `Permission`
/// records rather than stored alongside the journal. [`Scope::Once`] is not a
/// remembered answer (it covered one request and must not cover the next), so it
/// maps to `None`. A rule that does not parse is dropped: a malformed rule in
/// the journal is not something to remember a half-formed answer to.
pub fn grant_from_permission(permission: &Permission) -> Option<Grant> {
    let Scope::Rule { rule } = &permission.scope else {
        return None;
    };
    let spec = parse_rule(rule)?;
    let allow = matches!(
        permission.decision,
        Decision::Allowed | Decision::AskedAllowed | Decision::RememberedAllow
    );
    Some(Grant {
        allow,
        scope: PolicyScope::Rule(spec),
    })
}

/// The addresses a connect's host resolved to, or none recorded.
///
/// A denial reaches here with nothing, because the broker refuses a host before
/// it resolves one: the record then says the access was denied and names no
/// address, which is the truth about it.
fn resolved(supplied: &Supplied) -> Vec<IpAddr> {
    match supplied {
        Supplied::Resolved(addrs) => addrs.clone(),
        Supplied::Bound(_) | Supplied::Nothing => Vec::new(),
    }
}

/// The address a listen was bound to.
///
/// Loopback is the fallback because it is what the policy's own default binds,
/// so a record that somehow arrives without one reads as the safe case rather
/// than as the dangerous one. The broker always supplies it in practice.
fn bound(supplied: &Supplied) -> IpAddr {
    match supplied {
        Supplied::Bound(addr) => *addr,
        Supplied::Nothing | Supplied::Resolved(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
    }
}

/// A rule as text. The format is the journal's, so it must stay readable and
/// stable: `write:src/**` is what a `Permission` record carries and what a human
/// reads out of it.
fn render(rule: &RuleSpec) -> String {
    match rule {
        RuleSpec::Path { access, pattern } => {
            let verb = match access {
                PathAccess::Read => "read",
                PathAccess::Write => "write",
            };
            format!("{verb}:{pattern}")
        }
        // The whole argv, as a JSON array, because an argument may contain a
        // space and `exec:sh -c ls` could not then be split back into the words
        // that were actually approved.
        RuleSpec::Exec { args, program } => {
            let mut argv = vec![program.clone()];
            argv.extend(args.iter().cloned());
            format!("exec:{}", serde_json::json!(argv))
        }
        RuleSpec::Program { program } => format!("program:{program}"),
        RuleSpec::Connect { host } => format!("connect:{host}"),
        RuleSpec::Listen { port } => format!("listen:{port}"),
    }
}

/// The inverse of [`render`]: a rule string back into the structured matcher.
///
/// Resume reads the rule **as written** from the journal and rebuilds the
/// matcher, because the journal deliberately does not name `policy`'s
/// types. A string that is not a rule this build knows returns `None` rather
/// than panicking: an unknown verb in an old journal is a forward-compat case,
/// not a corrupt one, and dropping it loses one remembered answer rather than a
/// session.
fn parse_rule(rule: &str) -> Option<RuleSpec> {
    let (verb, rest) = rule.split_once(':')?;
    Some(match verb {
        "read" => RuleSpec::Path {
            access: PathAccess::Read,
            pattern: rest.to_string(),
        },
        "write" => RuleSpec::Path {
            access: PathAccess::Write,
            pattern: rest.to_string(),
        },
        "exec" => parse_exec(rest)?,
        "program" => RuleSpec::Program {
            program: rest.to_string(),
        },
        "connect" => RuleSpec::Connect {
            host: rest.to_string(),
        },
        "listen" => RuleSpec::Listen {
            port: rest.parse().ok()?,
        },
        _ => return None,
    })
}

/// An `exec:` rule body back into a program and its argv.
///
/// A journal written before the argv was part of the rule carries a bare program
/// name here. That reads as "this program, no arguments", which is *narrower*
/// than what the old build meant by it -- the safe direction for an old grant to
/// drift in, and the reason this is a fallback rather than an error.
fn parse_exec(rest: &str) -> Option<RuleSpec> {
    let Ok(argv) = serde_json::from_str::<Vec<String>>(rest) else {
        return Some(RuleSpec::Exec {
            args: Vec::new(),
            program: rest.to_string(),
        });
    };
    let (program, args) = argv.split_first()?;
    Some(RuleSpec::Exec {
        args: args.to_vec(),
        program: program.clone(),
    })
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use journal::CallId;

    use super::*;

    #[test]
    fn a_write_request_becomes_a_write_access() {
        let request = Request::Write(PathBuf::from("/repo/src/foo.rs"));
        assert_eq!(
            access(&request, &Supplied::Nothing),
            Access::Write {
                path: PathBuf::from("/repo/src/foo.rs")
            }
        );
    }

    #[test]
    fn listen_records_the_address_the_broker_bound_not_the_one_asked_for() {
        // The request carries only a port; the whole point is that the tool did
        // not choose the address. Recording only the port would lose the
        // difference between a loopback bind and a wildcard one.
        let request = Request::Listen { port: 8080 };
        let bound = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        assert_eq!(
            access(&request, &Supplied::Bound(bound)),
            Access::Listen(AccessListen::new(bound, 8080))
        );
    }

    #[test]
    fn connect_records_the_addresses_the_broker_resolved() {
        // The request carries a name and the policy allows names, so the
        // addresses are the only part of the record that says where the bytes
        // were about to go.
        let request = Request::Connect {
            host: "docs.example.com".into(),
            port: 443,
        };
        let resolved = IpAddr::V4(Ipv4Addr::LOCALHOST);
        assert_eq!(
            access(&request, &Supplied::Resolved(vec![resolved])),
            Access::Connect(AccessConnect::new(
                vec![resolved],
                "docs.example.com".into(),
                443
            ))
        );
    }

    #[test]
    fn a_denied_connect_records_no_address_because_none_was_looked_up() {
        // The broker refuses a host before resolving it, deliberately: a DNS
        // query for a denied name is still an outbound message. The record has
        // to say that rather than invent an address.
        let request = Request::Connect {
            host: "exfil.example.com".into(),
            port: 443,
        };
        assert_eq!(
            access(&request, &Supplied::Nothing),
            Access::Connect(AccessConnect::new(
                Vec::new(),
                "exfil.example.com".into(),
                443
            ))
        );
    }

    #[test]
    fn a_remembered_allow_is_distinguishable_from_a_prompted_one() {
        // "The user said yes to this" and "the user said yes to something like
        // this earlier" are different facts, and only one of them reached a
        // prompt.
        assert_eq!(
            decision(&PolicyDecision::Allow, DecisionSource::Remembered),
            Decision::RememberedAllow
        );
        assert_eq!(
            decision(&PolicyDecision::Allow, DecisionSource::Prompt),
            Decision::AskedAllowed
        );
        assert_eq!(
            decision(&PolicyDecision::Allow, DecisionSource::Policy),
            Decision::Allowed
        );
    }

    #[test]
    fn denials_carry_the_same_distinction() {
        let denied = PolicyDecision::Deny {
            reason: "no".into(),
        };
        assert_eq!(
            decision(&denied, DecisionSource::Remembered),
            Decision::RememberedDeny
        );
        assert_eq!(
            decision(&denied, DecisionSource::Prompt),
            Decision::AskedDenied
        );
        assert_eq!(decision(&denied, DecisionSource::Policy), Decision::Denied);
    }

    #[test]
    fn an_unresolved_ask_records_as_a_denial() {
        // The broker refuses an Ask it could not resolve, so the journal must
        // say denied rather than invent an answer.
        assert_eq!(
            decision(&PolicyDecision::Ask, DecisionSource::Policy),
            Decision::Denied
        );
    }

    #[test]
    fn a_rule_scope_is_carried_as_written() {
        // Not as a parsed matcher: naming policy's matcher would make
        // every rule-syntax change a schema migration.
        let rule = PolicyScope::Rule(RuleSpec::Path {
            access: PathAccess::Write,
            pattern: "src/**".into(),
        });
        assert_eq!(
            scope(&rule),
            Scope::Rule {
                rule: "write:src/**".into()
            }
        );
        assert_eq!(scope(&PolicyScope::Once), Scope::Once);
    }

    #[test]
    fn every_rule_kind_renders() {
        assert_eq!(
            scope(&PolicyScope::Rule(RuleSpec::Exec {
                args: vec!["-c".into(), "echo a b".into()],
                program: "sh".into()
            })),
            Scope::Rule {
                rule: r#"exec:["sh","-c","echo a b"]"#.into()
            },
            "the argv is part of the rule, and an argument may contain a space"
        );
        assert_eq!(
            scope(&PolicyScope::Rule(RuleSpec::Connect {
                host: "example.com".into()
            })),
            Scope::Rule {
                rule: "connect:example.com".into()
            }
        );
        assert_eq!(
            scope(&PolicyScope::Rule(RuleSpec::Listen { port: 5432 })),
            Scope::Rule {
                rule: "listen:5432".into()
            }
        );
    }

    #[test]
    fn parse_round_trips_every_rule_kind() {
        // render then parse must reproduce the matcher; otherwise resume
        // rebuilds a grant that does not cover what the user agreed to.
        let rules = [
            RuleSpec::Path {
                access: PathAccess::Read,
                pattern: "src/**".into(),
            },
            RuleSpec::Path {
                access: PathAccess::Write,
                pattern: "src/**".into(),
            },
            RuleSpec::Exec {
                args: Vec::new(),
                program: "make".into(),
            },
            RuleSpec::Exec {
                args: vec!["-c".into(), "cargo test --all  # a \"quoted\" arg".into()],
                program: "sh".into(),
            },
            RuleSpec::Program {
                program: "python3".into(),
            },
            RuleSpec::Connect {
                host: "example.com".into(),
            },
            RuleSpec::Listen { port: 5432 },
        ];
        for spec in rules {
            let rendered = render(&spec);
            let parsed = parse_rule(&rendered).expect("round trip parses");
            assert_eq!(parsed, spec, "round trip lost information: {rendered}");
        }
    }

    #[test]
    fn an_old_journals_bare_exec_rule_narrows_rather_than_widens() {
        // Forward compatibility, in the safe direction. A journal written before
        // the argv joined the rule carries `exec:sh`, which that build read as
        // "sh with any arguments". Reading it as "sh with none" loses a
        // remembered answer; reading it the old way would restore
        // session-wide arbitrary execution from a file the agent can no longer
        // write but may already have written.
        assert_eq!(
            parse_rule("exec:sh"),
            Some(RuleSpec::Exec {
                args: Vec::new(),
                program: "sh".into(),
            })
        );
    }

    #[test]
    fn parse_drops_an_unknown_rule_verb() {
        // A verb a future schema adds is not a corrupt journal; it is one the
        // reader does not know. Dropping it loses one remembered answer.
        assert!(parse_rule("teleport:mars").is_none());
        assert!(parse_rule("noverb").is_none());
    }

    #[test]
    fn grant_from_permission_remembers_only_rule_scoped_answers() {
        // Once answered one request and must not answer the next.
        let once = Permission::new(
            CallId::new("toolu_1".into()),
            Access::Write {
                path: "/repo/a.rs".into(),
            },
            Decision::AskedAllowed,
            Scope::Once,
        );
        assert!(grant_from_permission(&once).is_none());
    }

    #[test]
    fn grant_from_permission_carries_allow_and_deny() {
        let rule = Permission::new(
            CallId::new("toolu_1".into()),
            Access::Write {
                path: "/repo/src/a.rs".into(),
            },
            Decision::AskedAllowed,
            Scope::Rule {
                rule: "write:src/**".into(),
            },
        );
        let allow = grant_from_permission(&rule).expect("rule scoped");
        assert!(allow.allow);
        assert!(matches!(
            allow.scope,
            PolicyScope::Rule(RuleSpec::Path {
                access: PathAccess::Write,
                ..
            })
        ));

        let denied = Permission::new(
            CallId::new("toolu_2".into()),
            Access::Write {
                path: "/repo/src/secret.rs".into(),
            },
            Decision::RememberedDeny,
            Scope::Rule {
                rule: "write:src/secret.rs".into(),
            },
        );
        let deny = grant_from_permission(&denied).expect("rule scoped");
        assert!(!deny.allow);
    }
}

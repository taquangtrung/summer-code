//! The session grant set: a derived cache, not a second store.
//!
//! **Remembering is rule-scoped, and that is not the granularity of the audit
//! trail.** Per-access memory is unusable: a `grep` across three hundred files
//! under a policy that asks about reads would ask three hundred times, and the
//! user would answer by turning the policy off. So the prompt offers a scope and
//! the user answers with one, which means the remembered thing is a *rule*
//! (`writes under src/`), not an access (`write to src/foo.rs`).
//!
//! The records stay per access, because the monitor needs a grant it can match
//! against a specific write. The two granularities coexist, and the seam between
//! them is the `Scope` recorded on every `Permission` record. Without that field a
//! journal holding `Permission { access: Write("src/foo.rs"), decision:
//! AskedAllowed }` cannot say whether the user allowed that one write or all of
//! `src/`, and no amount of folding recovers it.
//!
//! This set is folded back from those records by `Broker::restore` on resume. It
//! is a cache: everything in it can be rebuilt from the journal, which is what
//! keeps the fold thesis true for the one piece of state that most looks like it
//! deserves an exception.

use policy::{Request, RuleSpec, Scope};

use crate::prompt::Grant;

// ========================================================================
// Data Structures
// ========================================================================

/// The rule-scoped answers the user has already given this session.
#[derive(Default)]
pub struct Remembered {
    grants: Vec<Grant>,
}

/// What the cache had to say about a request.
///
/// `Allow` and `Deny` carry the rule that covered the request, not just the
/// verdict. The journal needs it: a `Permission` record that says
/// `RememberedAllow` with `Scope::Once` is self-contradictory, and worse, the
/// fold only rebuilds a grant from a `Scope::Rule`, so the rule would survive in
/// exactly one record (the original prompt) and nowhere else. Compact that turn
/// away and a hundred `RememberedAllow`s fold back to zero grants, and the
/// resumed session re-asks what the user answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remembrance {
    Allow(RuleSpec),
    Deny(RuleSpec),
    /// Nothing the user said covers this. Ask.
    Unknown,
}

// ========================================================================
// Remembered Implementations
// ========================================================================

impl Remembered {
    pub fn new() -> Self {
        Self::default()
    }

    /// Keep a rule-scoped answer. `Scope::Once` is deliberately dropped: it
    /// answered one request and must not answer the next one.
    pub fn remember(&mut self, grant: Grant) {
        if matches!(grant.scope, Scope::Rule(_)) {
            self.grants.push(grant);
        }
    }

    /// Seed from the journal on resume. Without this a resumed session re-asks
    /// everything the user already approved.
    pub fn restore(&mut self, grants: Vec<Grant>) {
        for grant in grants {
            self.remember(grant);
        }
    }

    /// What the user already said about this request, if anything.
    ///
    /// A matching **deny wins over a matching allow**, whatever order they were
    /// given in. The alternative is that a broad "yes to src/**" silently
    /// overrides a narrow "no to src/secret.rs" depending on which the user
    /// happened to answer first.
    pub fn recall(&self, request: &Request) -> Remembrance {
        let mut allowed = None;
        for grant in &self.grants {
            let Scope::Rule(rule) = &grant.scope else {
                continue;
            };
            if !rule.matches(request) {
                continue;
            }
            if !grant.allow {
                return Remembrance::Deny(rule.clone());
            }
            allowed.get_or_insert_with(|| rule.clone());
        }
        allowed.map_or(Remembrance::Unknown, Remembrance::Allow)
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use policy::{PathAccess, RuleSpec};

    use super::*;

    fn write_rule(pattern: &str, allow: bool) -> Grant {
        Grant {
            allow,
            scope: Scope::Rule(RuleSpec::Path {
                access: PathAccess::Write,
                pattern: pattern.to_string(),
            }),
        }
    }

    fn write(path: &str) -> Request {
        Request::Write(PathBuf::from(path))
    }

    #[test]
    fn a_rule_scoped_allow_covers_its_whole_scope() {
        // The point of rule scope: answer once, cover the rest. Per-access
        // memory would re-ask for every file under src/.
        let mut remembered = Remembered::new();
        remembered.remember(write_rule("/repo/src/**", true));
        assert!(matches!(
            remembered.recall(&write("/repo/src/a.rs")),
            Remembrance::Allow(_)
        ));
        assert!(matches!(
            remembered.recall(&write("/repo/src/deep/b.rs")),
            Remembrance::Allow(_)
        ));
    }

    #[test]
    fn a_rule_does_not_cover_outside_its_scope() {
        let mut remembered = Remembered::new();
        remembered.remember(write_rule("/repo/src/**", true));
        assert_eq!(
            remembered.recall(&write("/repo/other/a.rs")),
            Remembrance::Unknown
        );
    }

    #[test]
    fn once_is_never_remembered() {
        // "Allow this one write" must not answer the next one.
        let mut remembered = Remembered::new();
        remembered.remember(Grant::once(true));
        assert_eq!(
            remembered.recall(&write("/repo/src/a.rs")),
            Remembrance::Unknown
        );
    }

    #[test]
    fn a_deny_beats_an_allow_whatever_the_order() {
        // Otherwise a broad yes silently swallows a narrow no, depending only on
        // which the user happened to answer first.
        let mut allow_first = Remembered::new();
        allow_first.remember(write_rule("/repo/src/**", true));
        allow_first.remember(write_rule("/repo/src/secret.rs", false));
        assert!(matches!(
            allow_first.recall(&write("/repo/src/secret.rs")),
            Remembrance::Deny(_)
        ));

        let mut deny_first = Remembered::new();
        deny_first.remember(write_rule("/repo/src/secret.rs", false));
        deny_first.remember(write_rule("/repo/src/**", true));
        assert!(matches!(
            deny_first.recall(&write("/repo/src/secret.rs")),
            Remembrance::Deny(_)
        ));
    }

    #[test]
    fn a_write_rule_does_not_answer_a_read() {
        let mut remembered = Remembered::new();
        remembered.remember(write_rule("/repo/src/**", true));
        assert_eq!(
            remembered.recall(&Request::Read(PathBuf::from("/repo/src/a.rs"))),
            Remembrance::Unknown
        );
    }

    #[test]
    fn restore_rebuilds_the_set_from_folded_grants() {
        // Resume seeds this from the journal's Permission scopes. Without it a
        // resumed session re-asks everything the user already answered.
        let mut remembered = Remembered::new();
        remembered.restore(vec![write_rule("/repo/src/**", true), Grant::once(true)]);
        assert!(matches!(
            remembered.recall(&write("/repo/src/a.rs")),
            Remembrance::Allow(_)
        ));
    }
}

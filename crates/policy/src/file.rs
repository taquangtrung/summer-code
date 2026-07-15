//! The on-disk policy shape.
//!
//! **This crate parses a policy; it does not read one.** It is a contract crate
//! with no I/O, so the caller supplies the text and the frontend owns the file
//! handle. That split is what keeps `policy` consumable by things that
//! only want to reason about policies: the Phase 2 monitor, Phase 9 telemetry.
//!
//! The shape is deliberately small. Every field a policy file can set is one the
//! trust-tier merge knows how to handle, because a field the merge cannot reason
//! about is a field a hostile project file could use.

use std::path::PathBuf;

use serde::Deserialize;

use crate::policy::{
    CommandRule, Effect, HostRule, ListenRule, NetworkRule, PathAccess, PathRule, Policy,
    PolicyError,
};

// ========================================================================
// Data Structures
// ========================================================================

/// A policy as written in JSON.
///
/// Note what is **not** here: `root`. A policy file does not get to say what it
/// is confined to. The root comes from the session, and a project file that
/// could move it would not be confined at all.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyFile {
    #[serde(default)]
    pub commands: Vec<CommandEntry>,
    #[serde(default)]
    pub network: Option<NetworkEntry>,
    #[serde(default)]
    pub paths: Vec<PathEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathEntry {
    /// `read` or `write`. Omitted means both, which is what a rule that does not
    /// say means, and keeps every policy file written before this field existed
    /// reading the same way.
    #[serde(default)]
    pub access: Option<PathAccess>,
    pub effect: Effect,
    pub pattern: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandEntry {
    pub effect: Effect,
    pub program: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkEntry {
    #[serde(default)]
    pub connect: Vec<HostEntry>,
    #[serde(default)]
    pub listen: Option<ListenEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostEntry {
    pub effect: Effect,
    pub host: String,
    /// Absent means any port.
    #[serde(default)]
    pub port: Option<u16>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenEntry {
    pub mode: ListenMode,
    /// Empty means any port.
    #[serde(default)]
    pub ports: Vec<u16>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ListenMode {
    Any,
    Deny,
    Loopback,
}

// ========================================================================
// PolicyFile Implementations
// ========================================================================

impl PolicyFile {
    /// Resolve into a [`Policy`] rooted at `root`.
    ///
    /// Path patterns are rooted here rather than taken as written: a policy file
    /// says `src/**` and means "under this repo". Leaving them relative would
    /// make them match by accident against an absolute canonical path, which is
    /// what the evaluator compares.
    ///
    /// **Fallible, because a policy nobody validated is a policy that fails
    /// open.** The evaluator compiles patterns with `is_ok_and`, so a malformed
    /// glob matches nothing and a deny rule with a typo silently becomes a no-op.
    /// `validate` is what catches that, and a caller who has to remember to call
    /// it is a caller who will forget: the only way to get a `Policy` out of a
    /// file now is to have passed it.
    pub fn into_policy(self, root: PathBuf) -> Result<Policy, PolicyError> {
        let paths = self
            .paths
            .into_iter()
            .map(|entry| PathRule {
                access: entry.access,
                effect: entry.effect,
                pattern: root_pattern(&root, &entry.pattern),
            })
            .collect();
        let commands = self
            .commands
            .into_iter()
            .map(|entry| CommandRule {
                effect: entry.effect,
                program: entry.program,
            })
            .collect();
        let network = self
            .network
            .map_or_else(NetworkRule::default, |entry| NetworkRule {
                connect: entry
                    .connect
                    .into_iter()
                    .map(|host| HostRule {
                        effect: host.effect,
                        host: host.host,
                        port: host.port,
                    })
                    .collect(),
                listen: entry
                    .listen
                    .map_or(ListenRule::Loopback { ports: vec![] }, |l| match l.mode {
                        ListenMode::Any => ListenRule::Any { ports: l.ports },
                        ListenMode::Deny => ListenRule::Deny,
                        ListenMode::Loopback => ListenRule::Loopback { ports: l.ports },
                    }),
            });

        let policy = Policy {
            commands,
            network,
            paths,
            root,
        };
        policy.validate()?;
        Ok(policy)
    }
}

// ========================================================================
// Functions
// ========================================================================

/// Anchor a pattern at the root unless it is already absolute.
///
/// The root is escaped, because a checkout at `/home/x/my[project]` would
/// otherwise turn into a character class and silently match nothing.
fn root_pattern(root: &std::path::Path, pattern: &str) -> String {
    if pattern.starts_with('/') {
        return pattern.to_string();
    }
    format!("{}/{pattern}", globset::escape(&root.display().to_string()))
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{root, root_str};

    fn parse(text: &str) -> PolicyFile {
        serde_json::from_str(text).expect("parses")
    }

    #[test]
    fn an_empty_file_is_a_policy_with_no_rules() {
        let policy = parse("{}").into_policy(root("repo")).expect("valid");
        assert!(policy.paths.is_empty());
        assert!(policy.commands.is_empty());
        assert_eq!(policy.root, root("repo"));
    }

    #[test]
    fn path_patterns_are_anchored_at_the_root() {
        // `src/**` in a file means "under this repo". The evaluator compares
        // against an absolute canonical path, so an unanchored pattern would
        // match nothing, or worse, match somewhere else.
        let policy = parse(r#"{"paths":[{"pattern":"src/**","effect":"allow"}]}"#)
            .into_policy(root("repo"))
            .expect("valid");
        assert_eq!(
            policy.paths[0].pattern,
            format!("{}/src/**", root_str("repo"))
        );
    }

    #[test]
    fn an_absolute_pattern_is_left_alone() {
        let policy = parse(r#"{"paths":[{"pattern":"/tmp/**","effect":"deny"}]}"#)
            .into_policy(root("repo"))
            .expect("valid");
        assert_eq!(policy.paths[0].pattern, "/tmp/**");
    }

    #[test]
    fn a_root_with_glob_metacharacters_is_escaped() {
        // A checkout at a path containing `[` would compile into a character
        // class and silently match nothing. `into_policy` validates, so an
        // unescaped root would fail here rather than at the first access.
        let policy = parse(r#"{"paths":[{"pattern":"src/**","effect":"allow"}]}"#)
            .into_policy(root("home/x/my[project]"))
            .expect("the escaped pattern compiles");
        assert_eq!(
            policy.paths[0].pattern,
            format!("{}/src/**", root_str("home/x/my[[]project[]]"))
        );
    }

    #[test]
    fn a_malformed_pattern_is_rejected_at_load_not_ignored_at_use() {
        // The evaluator compiles with `is_ok_and`, so a glob that does not
        // compile matches nothing and this deny would silently become a no-op.
        // `into_policy` is fallible precisely so that cannot happen quietly.
        let result = parse(r#"{"paths":[{"pattern":"[unclosed/**","effect":"deny"}]}"#)
            .into_policy(root("repo"));
        assert!(
            matches!(result, Err(PolicyError::BadPattern { .. })),
            "a malformed deny must not load as a no-op"
        );
    }

    #[test]
    fn a_policy_file_cannot_set_the_root() {
        // The field does not exist, and `deny_unknown_fields` makes trying an
        // error rather than a silent no-op. A file that could move the root it
        // is confined by would not be confined.
        let result: Result<PolicyFile, _> = serde_json::from_str(r#"{"root":"/"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn a_typo_is_an_error_not_a_silently_ignored_rule() {
        // `deny_unknown_fields` again: a rule that does not parse must not read
        // as a rule that does not apply.
        let result: Result<PolicyFile, _> =
            serde_json::from_str(r#"{"paths":[{"patern":"src/**","effect":"allow"}]}"#);
        assert!(result.is_err());
    }

    #[test]
    fn network_rules_carry_their_ports() {
        let policy = parse(
            r#"{"network":{"listen":{"mode":"loopback","ports":[8080]},"connect":[{"host":"localhost","port":5432,"effect":"allow"}]}}"#,
        )
        .into_policy(root("repo"))
        .expect("valid");

        assert_eq!(
            policy.network.listen,
            ListenRule::Loopback { ports: vec![8080] }
        );
        assert_eq!(policy.network.connect[0].port, Some(5432));
        assert_eq!(policy.network.connect[0].effect, Effect::Allow);
    }

    #[test]
    fn an_omitted_network_section_keeps_the_safe_default() {
        // Outbound off, loopback listen on: the file has to say so to widen.
        // "Off" is the absence of an allow, not a `deny *` rule: a file that
        // said nothing about network must not outrank a layer beneath it that
        // did. The evaluator denies what no rule allows, which is what
        // `connect_is_denied_by_default` checks.
        let policy = parse("{}").into_policy(root("repo")).expect("valid");
        assert!(policy.network.connect.is_empty());
        assert_eq!(
            policy.network.listen,
            ListenRule::Loopback { ports: vec![] }
        );
    }

    #[test]
    fn every_effect_parses() {
        let policy = parse(
            r#"{"commands":[{"program":"make","effect":"ask"},{"program":"cargo","effect":"allow"},{"program":"rm","effect":"deny"}]}"#,
        )
        .into_policy(root("repo"))
        .expect("valid");
        assert_eq!(policy.commands[0].effect, Effect::Ask);
        assert_eq!(policy.commands[1].effect, Effect::Allow);
        assert_eq!(policy.commands[2].effect, Effect::Deny);
    }
}

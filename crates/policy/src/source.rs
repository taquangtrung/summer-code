//! Where a policy comes from, and how layers merge.
//!
//! **The axis is trust, not precedence.** Getting this wrong in either direction
//! produces a rule that looks reasonable and is useless or unsafe.
//!
//! The default, the user file and CLI flags are **trusted**: the user authored
//! them, outside the repo, and the threat model says the user is the principal
//! and not the attacker. They compose by ordinary precedence and may widen
//! freely. A user file that cannot grant what the default withheld is a user
//! file nobody can use.
//!
//! The project file (`.summer/policy.json`) is **untrusted**. It is repo
//! content, and repo content is the injection vector the threat model names. It
//! is not a user speaking; it is a file that arrived with a clone. So:
//!
//! - Its **denies apply directly.** Narrowing is safe from any author, and it is
//!   why the layer can be useful without being trusted.
//! - Its **allows become `Ask`.** The project proposes a capability; the user
//!   grants it. The repo declares "I need `make`", visible in a diff and
//!   discoverable at first use, without granting itself `make`.
//! - It **cannot reach anything the trusted merge decided.** Where the user said
//!   no, a project file proposing yes does not even prompt: re-asking a settled
//!   question until someone clicks wrong is itself an attack.
//!
//! All three fall out of one ordering, because the evaluator takes the first
//! match: **project denies, then trusted layers high-to-low, then the project's
//! downgraded proposals.** The last position is what makes "cannot reach a
//! trusted decision" true, and it is the part that is easy to get wrong: putting
//! the proposals first would let a hostile repo prompt past a user's deny.

use std::path::PathBuf;

use crate::policy::{CommandRule, Effect, HostRule, ListenRule, NetworkRule, PathRule, Policy};

// ========================================================================
// Data Structures
// ========================================================================

/// A policy source, in trust order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The built-in default. Trusted.
    Default,
    /// `~/.summer/policy.json`. Trusted.
    User,
    /// `.summer/policy.json` at the repo root. **Untrusted**: repo content.
    Project,
    /// `--allow-write` and friends, for one run. Trusted.
    Cli,
}

// ========================================================================
// Source Implementations
// ========================================================================

impl Source {
    /// Whether a layer may grant a capability outright.
    pub fn is_trusted(self) -> bool {
        match self {
            Self::Cli | Self::Default | Self::User => true,
            Self::Project => false,
        }
    }
}

// ========================================================================
// Functions
// ========================================================================

/// Merge trusted layers (low-to-high precedence) with an optional untrusted
/// project policy into one precedence-ordered policy.
///
/// **`root` is a parameter and not read off the highest layer**, which looks
/// like a nicety and is not. Defaulting it meant `PathBuf::default()` (the empty
/// path) whenever `trusted` was empty, and `Path::starts_with("")` is true for
/// *every* path: the evaluator's root check silently switched off and, with no
/// rules to match, every read became `Allow`. An unset root has to be
/// impossible to express, not fail open.
///
/// `network.connect` merges by the same rules as paths and commands, because it
/// is the same kind of list and splitting the rule would be a trap: taking it
/// wholesale from the highest trusted layer meant a `--allow-write` CLI layer,
/// carrying a default `NetworkRule`, clobbered the user file's `localhost:5432`
/// grant. `listen` is a single mode rather than a list, so it still comes from
/// the highest trusted layer that is not the default, and the project file may
/// not move it: `Loopback -> Any` is exactly the widening the tier exists to
/// prevent.
pub fn merge(root: PathBuf, trusted: Vec<Policy>, project: Option<&Policy>) -> Policy {
    let mut paths: Vec<PathRule> = Vec::new();
    let mut commands: Vec<CommandRule> = Vec::new();
    let mut connect: Vec<HostRule> = Vec::new();

    // 1. Project denies, sticky: first-match honors them over any trusted allow.
    if let Some(project) = project {
        paths.extend(
            project
                .paths
                .iter()
                .filter(|r| r.effect == Effect::Deny)
                .cloned(),
        );
        commands.extend(
            project
                .commands
                .iter()
                .filter(|r| r.effect == Effect::Deny)
                .cloned(),
        );
        connect.extend(
            project
                .network
                .connect
                .iter()
                .filter(|r| r.effect == Effect::Deny)
                .cloned(),
        );
    }

    // 2. Trusted layers, high precedence first. A higher layer's allow shadows a
    //    lower layer's deny, which is safe because every one is user-authored.
    for layer in trusted.iter().rev() {
        paths.extend(layer.paths.iter().cloned());
        commands.extend(layer.commands.iter().cloned());
        connect.extend(layer.network.connect.iter().cloned());
    }

    // 3. The project's proposals, downgraded to Ask and placed last so they only
    //    speak where the trusted merge was silent.
    if let Some(project) = project {
        paths.extend(
            project
                .paths
                .iter()
                .filter(|r| r.effect != Effect::Deny)
                .map(|r| PathRule {
                    access: r.access,
                    effect: Effect::Ask,
                    pattern: r.pattern.clone(),
                }),
        );
        commands.extend(
            project
                .commands
                .iter()
                .filter(|r| r.effect != Effect::Deny)
                .map(|r| CommandRule {
                    effect: Effect::Ask,
                    program: r.program.clone(),
                }),
        );
        connect.extend(
            project
                .network
                .connect
                .iter()
                .filter(|r| r.effect != Effect::Deny)
                .map(|r| HostRule {
                    effect: Effect::Ask,
                    host: r.host.clone(),
                    port: r.port,
                }),
        );
    }

    Policy {
        commands,
        network: NetworkRule {
            connect,
            listen: merged_listen(&trusted),
        },
        paths,
        root,
    }
}

/// The highest trusted layer that actually states a `listen` mode.
///
/// **"Highest layer" is not the same as `trusted.last()`, and the difference is
/// the same trap already fixed for `network.connect` two fields up.** A layer
/// built from a file with no network section carries [`ListenRule::default`],
/// and taking the last layer wholesale meant a `--allow-write` CLI layer -- which
/// states nothing about listening -- silently reverted the user file's `Any` back
/// to loopback. Skipping layers that only carry the default is what makes the
/// doc's "the highest trusted layer that is not the default" true.
///
/// The cost is that a layer explicitly restating the default cannot override a
/// lower layer's widening. Distinguishing "unset" from "set to the default"
/// wants an `Option` on the file type; until a CLI layer exists to need it, this
/// is the behaviour the doc promises.
fn merged_listen(trusted: &[Policy]) -> ListenRule {
    let default = ListenRule::default();
    trusted
        .iter()
        .rev()
        .map(|policy| &policy.network.listen)
        .find(|listen| **listen != default)
        .cloned()
        .unwrap_or(default)
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::policy::NetworkRule;
    use crate::request::Request;
    use crate::{Decision, PolicyEvaluator, RulesEvaluator};

    fn exec(program: &str) -> Request {
        Request::Exec {
            args: vec![],
            program: program.to_string(),
        }
    }

    fn command(program: &str, effect: Effect) -> CommandRule {
        CommandRule {
            effect,
            program: program.to_string(),
        }
    }

    fn layer(commands: Vec<CommandRule>) -> Policy {
        Policy {
            commands,
            network: NetworkRule::default(),
            paths: vec![],
            root: PathBuf::from("/repo"),
        }
    }

    fn decide(policy: &Policy, request: Request) -> Decision {
        RulesEvaluator::new().evaluate(policy, &request)
    }

    fn root() -> PathBuf {
        PathBuf::from("/repo")
    }

    /// A layer whose only content is a network `connect` list.
    fn net_layer(connect: Vec<HostRule>) -> Policy {
        Policy {
            commands: vec![],
            network: NetworkRule {
                connect,
                listen: ListenRule::default(),
            },
            paths: vec![],
            root: PathBuf::from("/repo"),
        }
    }

    fn host(host: &str, effect: Effect) -> HostRule {
        HostRule {
            effect,
            host: host.to_string(),
            port: None,
        }
    }

    fn connect(host: &str) -> Request {
        Request::Connect {
            host: host.to_string(),
            port: 5432,
        }
    }

    #[test]
    fn a_higher_trusted_layer_does_not_clobber_a_lower_ones_network_grant() {
        // Taking `network` wholesale from the highest layer meant a CLI layer
        // built for `--allow-write` alone, carrying a default NetworkRule,
        // silently revoked the user file's localhost grant. The list merges by
        // precedence like every other rule list.
        let user = net_layer(vec![host("localhost", Effect::Allow)]);
        let cli = layer(vec![]);
        let merged = merge(root(), vec![user, cli], None);
        assert!(
            matches!(decide(&merged, connect("localhost")), Decision::Allow),
            "a later layer with no opinion on network must not revoke an earlier grant"
        );
    }

    #[test]
    fn a_project_network_deny_applies() {
        // The untrusted tier's denies apply directly, for network as for
        // everything else: narrowing is safe from any author, and dropping the
        // list entirely was the only narrowing it could express.
        let user = net_layer(vec![host("*", Effect::Allow)]);
        let project = net_layer(vec![host("evil.com", Effect::Deny)]);
        let merged = merge(root(), vec![user], Some(&project));
        assert!(
            matches!(decide(&merged, connect("evil.com")), Decision::Deny { .. }),
            "a project deny must narrow the trusted allow"
        );
        assert!(matches!(
            decide(&merged, connect("good.com")),
            Decision::Allow
        ));
    }

    #[test]
    fn a_project_network_allow_becomes_a_prompt() {
        let project = net_layer(vec![host("evil.com", Effect::Allow)]);
        let merged = merge(root(), vec![layer(vec![])], Some(&project));
        assert!(
            matches!(decide(&merged, connect("evil.com")), Decision::Ask),
            "the repo proposes an egress; the user grants it"
        );
    }

    #[test]
    fn a_project_allow_becomes_a_prompt_not_a_grant() {
        // The whole point of the untrusted tier: the repo may propose `make`,
        // and the user grants it.
        let project = layer(vec![command("make", Effect::Allow)]);
        let merged = merge(root(), vec![layer(vec![])], Some(&project));
        assert!(matches!(decide(&merged, exec("make")), Decision::Ask));
    }

    #[test]
    fn a_project_deny_is_sticky_over_a_trusted_allow() {
        // Narrowing is safe from any author.
        let project = layer(vec![command("cargo", Effect::Deny)]);
        let merged = merge(
            root(),
            vec![layer(vec![command("cargo", Effect::Allow)])],
            Some(&project),
        );
        assert!(matches!(
            decide(&merged, exec("cargo")),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn a_project_allow_cannot_reach_past_a_trusted_deny() {
        // The hole this closes: a hostile repo re-asking a question the user
        // already settled, until someone clicks wrong.
        let project = layer(vec![command("rm", Effect::Allow)]);
        let merged = merge(
            root(),
            vec![layer(vec![command("rm", Effect::Deny)])],
            Some(&project),
        );
        assert!(
            matches!(decide(&merged, exec("rm")), Decision::Deny { .. }),
            "a project proposal must not even prompt where the user said no"
        );
    }

    #[test]
    fn a_project_proposal_does_not_re_prompt_what_the_user_allowed() {
        let project = layer(vec![command("cargo", Effect::Allow)]);
        let merged = merge(
            root(),
            vec![layer(vec![command("cargo", Effect::Allow)])],
            Some(&project),
        );
        assert!(matches!(decide(&merged, exec("cargo")), Decision::Allow));
    }

    #[test]
    fn a_trusted_layer_may_widen_a_lower_one() {
        // A user file that cannot grant what the default withheld is useless.
        let merged = merge(
            root(),
            vec![
                layer(vec![command("rm", Effect::Deny)]),
                layer(vec![command("rm", Effect::Allow)]),
            ],
            None,
        );
        assert!(matches!(decide(&merged, exec("rm")), Decision::Allow));
    }

    #[test]
    fn a_trusted_layer_may_narrow_a_lower_one() {
        let merged = merge(
            root(),
            vec![
                layer(vec![command("rm", Effect::Allow)]),
                layer(vec![command("rm", Effect::Deny)]),
            ],
            None,
        );
        assert!(matches!(decide(&merged, exec("rm")), Decision::Deny { .. }));
    }

    #[test]
    fn the_project_file_cannot_move_the_root() {
        let mut project = layer(vec![]);
        project.root = PathBuf::from("/");
        let merged = merge(root(), vec![layer(vec![])], Some(&project));
        assert_eq!(merged.root, PathBuf::from("/repo"));
    }

    #[test]
    fn a_merge_with_no_trusted_layers_still_confines() {
        // This is the fail-open case the signature exists to prevent: an empty
        // root is `""`, and `Path::starts_with("")` is true for every path, so
        // the containment check would silently switch off and every read would
        // be allowed.
        let merged = merge(root(), vec![], None);
        assert_eq!(merged.root, root());
        assert!(matches!(
            decide(&merged, Request::Read(PathBuf::from("/etc/passwd"))),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn only_the_project_tier_is_untrusted() {
        assert!(Source::Default.is_trusted());
        assert!(Source::User.is_trusted());
        assert!(Source::Cli.is_trusted());
        assert!(!Source::Project.is_trusted());
    }
}

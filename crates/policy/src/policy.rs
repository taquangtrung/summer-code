//! The policy data: paths, commands, and network rules.

use std::path::PathBuf;

use serde::Deserialize;
use thiserror::Error;

// ========================================================================
// Constants
// ========================================================================

/// Programs the built-in policy allows without asking.
///
/// **Platform-specific, because an allow for a program that does not exist is
/// not harmless.** `ls` on Windows names no executable, so the rule reads as a
/// grant and enforces nothing -- and worse, it teaches a reader that the default
/// set works there. What the three shared entries have in common is that they
/// ship the same way everywhere and are the tools a coding agent reaches for
/// first. The shell is deliberately absent on every platform: it is the one
/// program that can run any other, so it always reaches a prompt.
///
/// **The Unix list also carries the read-only text filters a pipeline ends
/// in**, now that a shell line is judged by the programs it runs: without them
/// `cargo test 2>&1 | tail -5` still asks about `tail`, and that line was most
/// of what sessions prompted for. Each reads and prints and writes nothing;
/// `sed`, `sort` and `uniq` are left out because each can write a file from its
/// own arguments (`-i`, `-o`, a second operand), which no path rule would see.
#[cfg(windows)]
const DEFAULT_ALLOWED_COMMANDS: &[&str] = &["cargo", "git", "rg"];

#[cfg(not(windows))]
const DEFAULT_ALLOWED_COMMANDS: &[&str] = &[
    "cargo", "cat", "cut", "git", "grep", "head", "ls", "rg", "tail", "tr", "wc",
];

// ========================================================================
// Data Structures
// ========================================================================

/// A resolved capability policy. Rule lists are ordered by precedence, highest
/// first: a request is matched against them in order and the first match wins.
/// [`crate::merge`] builds that ordering from the trust-tiered sources.
#[derive(Debug, Clone)]
pub struct Policy {
    pub commands: Vec<CommandRule>,
    /// Tool network only. This never gates the model connection, and could not:
    /// the agent needs the model to run at all.
    pub network: NetworkRule,
    pub paths: Vec<PathRule>,
    pub root: PathBuf,
}

/// What a matching rule decides.
///
/// `Ask` is a rule outcome and not just an evaluator default, because the
/// project-file tier needs to express "propose this capability" as data: its
/// allows are downgraded to `Ask` at merge time, and a `bool` cannot carry that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Allow,
    Ask,
    Deny,
}

/// A path rule, matched as a glob against the canonical path.
///
/// `access` is what makes the built-in default expressible at all. "Deny writes
/// to `.git/`" and "deny `.git/`" are different rules: the second one also stops
/// the agent reading `.git/HEAD` to report the branch, or reading `.gitignore`
/// before it edits. A rule with no access axis can only say the second.
#[derive(Debug, Clone)]
pub struct PathRule {
    /// Which accesses the rule speaks to. `None` is both, which is what a rule
    /// that does not say means.
    pub access: Option<PathAccess>,
    pub effect: Effect,
    pub pattern: String,
}

/// Which half of path access a rule or a grant covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathAccess {
    Read,
    Write,
}

/// A command rule, matched as a glob against the program.
///
/// **A deny also matches the program's file name; an allow does not.** The two
/// directions fail safe in opposite ways, so they cannot share one rule. A deny
/// on `rm` that the model dodges by asking for `/bin/rm` is not a deny. An allow
/// on `cargo` that also admits `/tmp/evil/cargo` is a hole, so an allow matches
/// only what it literally says.
#[derive(Debug, Clone)]
pub struct CommandRule {
    pub effect: Effect,
    pub program: String,
}

#[derive(Debug, Clone)]
pub struct NetworkRule {
    pub connect: Vec<HostRule>,
    pub listen: ListenRule,
}

/// An outbound rule. `port: None` matches any port, which is why the field is
/// here rather than assumed: "reach localhost:5432" and "reach localhost on
/// anything" are different grants.
#[derive(Debug, Clone)]
pub struct HostRule {
    pub effect: Effect,
    pub host: String,
    pub port: Option<u16>,
}

/// Inbound bind policy. Loopback is the safe default; `Any` exposes the agent's
/// workspace to the network.
///
/// The port lists bound which ports may be bound at all; empty means any. A
/// coding agent runs test suites and test suites bind ports, so this cannot be
/// folded into the outbound rule: binding loopback is nearly always fine, and
/// binding `0.0.0.0` is a different risk entirely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenRule {
    Any { ports: Vec<u16> },
    Deny,
    Loopback { ports: Vec<u16> },
}

// ========================================================================
// Errors
// ========================================================================

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("policy rule has a malformed glob `{pattern}`: {message}")]
    BadPattern { message: String, pattern: String },
    #[error("policy root `{root}` is not absolute")]
    RelativeRoot { root: String },
}

// ========================================================================
// Policy Implementations
// ========================================================================

impl Policy {
    /// The built-in default: read anywhere under `root`, write only under
    /// `root`, never touch `.git/` or a root dotfile, a small command list, no
    /// outbound network, loopback listen only.
    ///
    /// This is what runs with no config at all, and it is deliberately usable: a
    /// policy nobody configures is the policy almost everyone runs.
    pub fn builtin(root: PathBuf) -> Self {
        Self {
            commands: DEFAULT_ALLOWED_COMMANDS
                .iter()
                .map(|program| CommandRule {
                    effect: Effect::Allow,
                    program: program.to_string(),
                })
                .collect(),
            network: NetworkRule::default(),
            paths: vec![
                // The agent may not author anything the harness will later
                // trust. Propose-as-Ask bounds what a hostile project file can
                // win; this bounds what the running agent can write for the
                // *next* session to read.
                //
                // **The whole directory, not just `policy.json`.** Naming one
                // file left two escapes open, and neither needed a policy edit.
                // `.summer/config.json` carries `tui.statusLine.command`, run as
                // `sh -c` on every refresh with no broker, no policy check and no
                // journal entry. `.summer/sessions/*.jsonl` is the input to
                // `/resume`, which folds `Permission` records back into grants, so
                // appending a forged `AskedAllowed` for `exec:sh` makes the
                // resumed session auto-allow arbitrary exec with no prompt --
                // alongside a conversation history the agent wrote itself. One
                // `[a]` answer to any root write generalized to `write:/repo/**`
                // and covered both.
                //
                // Writes only: these files are read at session start and hashed
                // into `policy_digest`, so denying reads would deny that.
                deny_write("**/.summer/**"),
                deny_write("**/.git/**"),
                deny_root_dotfile_writes(&root),
                // Read and write anything under the root. Anchored at the root
                // rather than `**`, because the evaluator consults rules before
                // it falls back to the root check: a bare `**` would match
                // `/etc/passwd` and allow it.
                PathRule {
                    access: None,
                    effect: Effect::Allow,
                    pattern: format!("{}/**", globset::escape(&root.display().to_string())),
                },
            ],
            root,
        }
    }

    /// Check the policy is enforceable before anything relies on it.
    ///
    /// Two things, both of which fail *open* if left to convention. A malformed
    /// glob matches nothing, which turns a deny rule into a no-op. And a
    /// relative root defeats the evaluator's containment check: normalization
    /// absorbs leading `..`s, so `repo/../../../etc` reduces to a path that
    /// still `starts_with("repo")`. Only an absolute root makes the check mean
    /// what it reads as.
    ///
    /// Every pattern the evaluator compiles is checked here, not just the path
    /// ones: a host or command rule with a typo is the same silent no-op deny.
    pub fn validate(&self) -> Result<(), PolicyError> {
        if !self.root.is_absolute() {
            return Err(PolicyError::RelativeRoot {
                root: self.root.display().to_string(),
            });
        }
        for rule in &self.paths {
            check_pattern(&rule.pattern)?;
        }
        for rule in &self.commands {
            check_pattern(&rule.program)?;
        }
        for rule in &self.network.connect {
            check_pattern(&rule.host)?;
        }
        Ok(())
    }
}

// ========================================================================
// NetworkRule Implementations
// ========================================================================

impl Default for NetworkRule {
    /// Outbound off, loopback listen on. The plan ships network off; the point
    /// of a config is to turn it on.
    ///
    /// **"Off" is the evaluator's fallback, not a `deny *` rule sitting in the
    /// list.** The distinction only shows up at merge time, and it bites: every
    /// layer built from a file with no `[network]` section carries this default,
    /// so a literal `deny *` would ride in at that layer's precedence and revoke
    /// a grant a lower layer made. A `--allow-write` CLI layer would silently
    /// switch off the user file's `localhost:5432`. An empty list says nothing,
    /// and the evaluator denies what no rule allows.
    fn default() -> Self {
        Self {
            connect: Vec::new(),
            listen: ListenRule::default(),
        }
    }
}

// ========================================================================
// ListenRule Implementations
// ========================================================================

impl Default for ListenRule {
    /// Loopback, any port. Binding loopback is nearly always fine; binding
    /// `0.0.0.0` exposes the agent's workspace and stays a decision.
    fn default() -> Self {
        Self::Loopback { ports: vec![] }
    }
}

// ========================================================================
// Functions
// ========================================================================

/// Compile a pattern and discard it: this is the check, not the use.
fn check_pattern(pattern: &str) -> Result<(), PolicyError> {
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|_| ())
        .map_err(|error| PolicyError::BadPattern {
            message: error.to_string(),
            pattern: pattern.to_string(),
        })
}

fn deny_write(pattern: &str) -> PathRule {
    PathRule {
        access: Some(PathAccess::Write),
        effect: Effect::Deny,
        pattern: pattern.to_string(),
    }
}

/// Dotfiles at the repo root only, not every dotfile everywhere: `src/.foo` is
/// ordinary content, `/repo/.env` is not.
///
/// The root is escaped before it is spliced into the pattern. A checkout at
/// `/home/x/my[project]` would otherwise turn `[project]` into a character
/// class, and the rule would silently stop matching the directory it was
/// written to protect.
fn deny_root_dotfile_writes(root: &std::path::Path) -> PathRule {
    deny_write(&format!(
        "{}/.*",
        globset::escape(&root.display().to_string())
    ))
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{root, root_str};

    #[test]
    fn the_builtin_default_validates() {
        Policy::builtin(root("repo"))
            .validate()
            .expect("the shipped default must compile");
    }

    #[test]
    fn a_malformed_pattern_is_an_error_not_a_silent_no_match() {
        // A deny rule with a typo that quietly matches nothing is the dangerous
        // direction, so patterns are compiled at load.
        let policy = Policy {
            commands: vec![],
            network: NetworkRule::default(),
            paths: vec![deny_write("[unclosed")],
            root: root("repo"),
        };
        assert!(policy.validate().is_err());
    }

    #[test]
    fn a_root_with_glob_metacharacters_still_denies_its_dotfiles() {
        // A checkout at a path containing `[` would otherwise compile into a
        // character class and quietly stop protecting the directory.
        let policy = Policy::builtin(root("home/x/my[project]"));
        policy.validate().expect("the escaped pattern must compile");

        let rule = policy
            .paths
            .iter()
            .find(|rule| rule.pattern.ends_with("/.*"))
            .expect("the root-dotfile rule exists");
        let glob = globset::GlobBuilder::new(&rule.pattern)
            .literal_separator(true)
            .build()
            .expect("compiles")
            .compile_matcher();
        assert!(glob.is_match(root_str("home/x/my[project]/.env")));
        assert!(!glob.is_match(root_str("home/x/myXproject/.env")));
    }

    #[test]
    fn a_relative_root_is_rejected() {
        // Normalization absorbs leading `..`s, so `repo/../../../etc` reduces to
        // a path that still starts with `repo`. Only an absolute root makes the
        // containment check mean what it reads as.
        let policy = Policy::builtin(PathBuf::from("repo"));
        assert!(matches!(
            policy.validate(),
            Err(PolicyError::RelativeRoot { .. })
        ));
    }

    #[test]
    fn the_default_states_no_outbound_rule_and_allows_loopback() {
        // "Outbound off" is the evaluator's fallback, not a `deny *` in the
        // list: a literal rule here would ride into every merge at its layer's
        // precedence and revoke a lower layer's grant. `connect_is_denied_by_default`
        // in the evaluator is what checks the behavior this enables.
        let network = NetworkRule::default();
        assert!(
            network.connect.is_empty(),
            "the default must not carry a rule that outranks a user's grant"
        );
        assert_eq!(network.listen, ListenRule::Loopback { ports: vec![] });
    }
}

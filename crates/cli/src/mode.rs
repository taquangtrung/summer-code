//! Session modes: how much the agent may do without being asked.
//!
//! **A mode is a policy layer, not a second permission system**, and that is the
//! whole design. `sandbox`'s own docs put it plainly: policies are data, so a
//! permissive policy is a configuration and not a bypass. Every mode here is
//! therefore a small policy file, merged as the highest trusted layer, and the
//! broker, the prompts, the journal and `policy_digest` all keep working with no
//! knowledge that modes exist. A mode that reached around the broker instead
//! would be exactly the bypass this crate is built to make impossible.
//!
//! The four form a ladder, from least to most the agent may do on its own:
//!
//! | Mode | Writes | Commands |
//! |---|---|---|
//! | `plan` | denied | denied, except the searches the reading tools exec |
//! | `ask` | prompt | prompt |
//! | `default` | allowed under the root | allowlist, plus kernel-confined exec |
//! | `full` | allowed | allowed |
//!
//! `default` carries no overlay at all: it is the built-in policy the project
//! already shipped, named so the ladder has a rung for it.
//!
//! **A project's explicit denies still win, including under `full`.** They sit
//! ahead of every trusted layer in the merge, which is what lets a repository
//! refuse something about itself. So `full` means "everything the repository did
//! not refuse", and calling it anything stronger would be a claim the merge does
//! not honour.
//!
//! **The mode is chosen at startup and its policy does not change mid-session.**
//! The broker owns its policy by value, and making it swappable would mean
//! putting a lock around the one decision path this codebase most wants to keep
//! pure. Leaving `--mode plan` is therefore two sessions, which `/resume` joins:
//! plan, then resume the same journal without the flag.
//!
//! **The TUI's [`ExecutionMode`] is a second, narrower dial on top.** It decides
//! how a prompt is answered (by the user, yes, or no), and its `plan` also
//! closes [`crate::read_only_gate`], which denies writes and commands the policy
//! would have allowed outright. It can only narrow what the startup mode
//! allows, never widen it, so [`Mode::refusal`] rules out a switch the startup
//! policy would make a lie of.

use std::fmt;
use std::str::FromStr;

use serde::Deserialize;

// ========================================================================
// Constants
// ========================================================================

/// Read-only: every write refused, and every command but the searches the
/// reading tools are built on.
///
/// **`rg` is allowed because `grep` is a reading tool that happens to exec.**
/// The first version denied `*`, and a live plan-mode session had its `grep`
/// refused with "command rule" while trying to find the callers it was asked to
/// plan around. A read-only mode that cannot search is not read-only, it is
/// crippled, and the model reads the refusal as a broken workspace.
///
/// Nothing else is allowed, `git` included. Its read-only subcommands would be
/// welcome, but a rule matches the program and cannot tell `git log` from `git
/// reset --hard`; the broker never sees what a child does after exec, so an
/// allowed `git` is a writable tree. The same reasoning excludes `cargo`, which
/// runs build scripts. Denying them costs a plan some detail; allowing them
/// costs the mode its one guarantee.
const PLAN_POLICY: &str = r#"{
  "paths": [{ "pattern": "**", "access": "write", "effect": "deny" }],
  "commands": [
    { "program": "rg", "effect": "allow" },
    { "program": "ls", "effect": "allow" },
    { "program": "*", "effect": "deny" }
  ]
}"#;

/// Every write and command stops to ask, including the ones the built-in policy
/// would have allowed.
const ASK_POLICY: &str = r#"{
  "paths": [{ "pattern": "**", "access": "write", "effect": "ask" }],
  "commands": [{ "program": "*", "effect": "ask" }]
}"#;

/// Every write and command allowed without asking.
const FULL_POLICY: &str = r#"{
  "paths": [{ "pattern": "**", "effect": "allow" }],
  "commands": [{ "program": "*", "effect": "allow" }]
}"#;

/// What `plan` tells the model about itself.
///
/// Without this the agent learns its situation by being refused, once per
/// attempt, and reads a deliberate mode as a broken workspace.
const PLAN_NOTE: &str = "\n\n## This session is in plan mode\n\nEvery write and every command is denied, and that is the mode rather than a fault. Investigate with the reading tools and answer with a plan: what you would change, in which files, and how you would verify it. Do not attempt an edit to demonstrate the plan. The user leaves this mode by starting the session again without it.";

/// What `ask` tells the model about itself.
const ASK_NOTE: &str = "\n\n## This session is in ask mode\n\nEvery write and every command stops for the user to approve, so prefer one considered change over several exploratory ones, and say what you are about to do before you do it.";

// ========================================================================
// Data Structures
// ========================================================================

/// How much a session may do without asking.
///
/// Deserialized by the same names `--mode` takes, so the config key and the
/// flag cannot drift apart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Every write and command prompts.
    Ask,
    /// The built-in policy, unmodified.
    #[default]
    Default,
    /// Every write and command allowed, subject to the project's own denies.
    Full,
    /// Read-only.
    Plan,
}

/// Execution mode for interactive CLI and TUI sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecutionMode {
    /// Interactive mode: prompts the user before executing commands/tools.
    #[default]
    Interactive,
    /// Auto mode: automatically accepts all commands and tools without prompting.
    Auto,
    /// Planning mode: read-only; writes and commands are denied.
    Plan,
}

impl ExecutionMode {
    /// Cycle to the next mode in sequence (Interactive -> Auto -> Plan -> Interactive).
    pub fn cycle_next(self) -> Self {
        match self {
            Self::Interactive => Self::Auto,
            Self::Auto => Self::Plan,
            Self::Plan => Self::Interactive,
        }
    }

    /// The next mode in the cycle that a session started in `start` may switch
    /// to, or `None` when it may switch to none of them.
    pub fn next_allowed(self, start: Mode) -> Option<Self> {
        let mut next = self.cycle_next();
        while next != self {
            if start.refusal(next).is_none() {
                return Some(next);
            }
            next = next.cycle_next();
        }
        None
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Auto => "auto",
            Self::Plan => "plan",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Interactive => "Interactive",
            Self::Auto => "Auto",
            Self::Plan => "Planning",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Interactive => "interactive mode (prompt for commands)",
            Self::Auto => "auto mode (automatically accept all commands)",
            Self::Plan => "planning mode (read-only; writes and commands denied)",
        }
    }
}

impl From<Mode> for ExecutionMode {
    fn from(mode: Mode) -> Self {
        match mode {
            Mode::Full => Self::Auto,
            Mode::Plan => Self::Plan,
            Mode::Ask | Mode::Default => Self::Interactive,
        }
    }
}

// ========================================================================
// Mode Implementations
// ========================================================================

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Default => "default",
            Self::Full => "full",
            Self::Plan => "plan",
        }
    }

    /// The policy layer this mode adds, as a policy file, or `None` for the
    /// mode that adds nothing.
    ///
    /// A file rather than constructed rules, so a mode goes through the same
    /// parser and validator a user's own policy does. A mode that could express
    /// something a user's file cannot would be a second policy language.
    pub fn overlay(self) -> Option<&'static str> {
        match self {
            Self::Ask => Some(ASK_POLICY),
            Self::Default => None,
            Self::Full => Some(FULL_POLICY),
            Self::Plan => Some(PLAN_POLICY),
        }
    }

    /// Whether this mode's overlay outranks the user's own `policy.json`.
    ///
    /// **A restriction outranks it; a fallback does not.** `plan` and `full`
    /// say "this run is different from my standing preferences", so they have to
    /// win: a `--mode plan` that could not refuse a write the user's policy
    /// allows is not a mode. `ask` says something weaker, "prompt me about
    /// things I have not already decided", and a standing allow *is* a decision
    /// already made.
    ///
    /// This was found by running it. A rule saved from an `ask` prompt landed in
    /// `policy.json`, and the next `ask` session prompted for it again, because
    /// the catch-all `ask` overlay shadowed the allow underneath. An offer to
    /// remember something, followed by asking about it again, is worse than not
    /// offering.
    pub fn outranks_user_policy(self) -> bool {
        match self {
            Self::Ask => false,
            Self::Default | Self::Full | Self::Plan => true,
        }
    }

    /// What the system prompt tells the model about this mode.
    ///
    /// `default` and `full` say nothing: they are the situation the prompt
    /// already describes, and a note claiming "you may edit freely" would spend
    /// tokens telling the model what it will discover by editing freely.
    pub fn prompt_note(self) -> Option<&'static str> {
        match self {
            Self::Ask => Some(ASK_NOTE),
            Self::Default | Self::Full => None,
            Self::Plan => Some(PLAN_NOTE),
        }
    }

    /// Why a session started in this mode cannot switch to `target`, or `None`
    /// when it can.
    ///
    /// **Each refusal is a label the startup policy would contradict.** Under
    /// `plan` every write is denied by the policy itself, so "interactive" or
    /// "auto" would claim a session that still cannot write. Under `full`
    /// nothing prompts, so "interactive" would promise questions that never
    /// come. Narrowing is always allowed: `full` can still enter `plan`.
    pub fn refusal(self, target: ExecutionMode) -> Option<&'static str> {
        match (self, target) {
            (Self::Plan, ExecutionMode::Auto | ExecutionMode::Interactive) => Some(
                "this session started with --mode plan, which stays read-only until it ends; \
                 resume it without the flag to act on the plan",
            ),
            (Self::Full, ExecutionMode::Interactive) => Some(
                "this session started with --mode full, so nothing prompts and there is nothing \
                 to ask about; restart without it to be asked",
            ),
            (Self::Ask | Self::Default, _)
            | (Self::Full, ExecutionMode::Auto | ExecutionMode::Plan)
            | (Self::Plan, ExecutionMode::Plan) => None,
        }
    }

    /// The line a session banner or `/status` shows. `None` for `default`,
    /// which is what a session without a mode flag has always been.
    pub fn banner(self) -> Option<String> {
        match self {
            Self::Default => None,
            Self::Ask => Some("mode: ask (every write and command prompts)".into()),
            Self::Full => {
                Some("mode: full (writes and commands run unasked; the project's own denies still apply)".into())
            }
            Self::Plan => Some("mode: plan (read-only; writes and commands are denied)".into()),
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Mode {
    type Err = UnknownMode;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        match name {
            "ask" => Ok(Self::Ask),
            "default" => Ok(Self::Default),
            "full" => Ok(Self::Full),
            "plan" => Ok(Self::Plan),
            other => Err(UnknownMode(other.to_string())),
        }
    }
}

// ========================================================================
// Errors
// ========================================================================

/// Written out rather than derived: `cli` carries no error-derive dependency,
/// and one error type is not the reason to add one.
#[derive(Debug)]
pub struct UnknownMode(String);

impl fmt::Display for UnknownMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown mode {:?}; choices are \"plan\", \"ask\", \"default\", \"full\"",
            self.0
        )
    }
}

impl std::error::Error for UnknownMode {}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_every_mode_round_trips_through_its_name() {
        // The name is what the flag takes and what the banner prints; a mode
        // whose two spellings disagree is selectable and unreportable.
        for mode in [Mode::Ask, Mode::Default, Mode::Full, Mode::Plan] {
            assert_eq!(mode.as_str().parse::<Mode>().expect("parses"), mode);
        }
    }

    #[test]
    fn test_an_unknown_mode_lists_the_ones_that_would_have_worked() {
        let error = "yolo".parse::<Mode>().expect_err("not a mode").to_string();
        assert!(error.contains("plan") && error.contains("full"), "{error}");
    }

    #[test]
    fn test_the_default_mode_changes_nothing() {
        // The rung that must stay a no-op: every session that predates this
        // module runs `default`, and an overlay here would change what they do.
        assert!(Mode::Default.overlay().is_none());
        assert!(Mode::Default.prompt_note().is_none());
        assert!(Mode::Default.banner().is_none());
    }

    #[test]
    fn test_plan_mode_denies_both_axes() {
        // Denying writes alone would leave `shell` as an unguarded way to write:
        // `sh -c 'echo x > f'` is a write the path rules never see.
        let overlay = Mode::Plan.overlay().expect("plan carries an overlay");
        assert!(
            overlay.contains(r#""access": "write", "effect": "deny""#),
            "{overlay}"
        );
        assert!(
            overlay.contains(r#""program": "*", "effect": "deny""#),
            "{overlay}"
        );
    }

    #[test]
    fn test_a_restriction_outranks_the_users_policy_and_a_fallback_does_not() {
        // Found by running it: a rule saved from an `ask` prompt landed in
        // policy.json, and the next `ask` session prompted for it again because
        // the catch-all overlay shadowed the allow underneath. `plan` has to
        // keep winning, or it cannot refuse what a permissive policy allows.
        assert!(!Mode::Ask.outranks_user_policy());
        assert!(Mode::Plan.outranks_user_policy());
    }

    #[test]
    fn test_plan_mode_can_still_search() {
        // A live plan session had its `grep` refused with "command rule",
        // because `grep` execs ripgrep and the overlay denied every program. A
        // read-only mode that cannot search is not read-only, it is crippled.
        // The allow has to precede the catch-all deny: first match wins.
        let overlay = Mode::Plan.overlay().expect("plan carries an overlay");
        let allow = overlay
            .find(r#""program": "rg", "effect": "allow""#)
            .expect("ripgrep is allowed in plan mode");
        let deny = overlay
            .find(r#""program": "*", "effect": "deny""#)
            .expect("everything else is denied");
        assert!(allow < deny, "the catch-all deny shadows the search allow");
    }

    #[test]
    fn test_cycling_skips_the_modes_the_startup_policy_contradicts() {
        // `--mode full` allows everything, so "interactive" would be a mode
        // that never asks; Shift+Tab has to step over it, not stop on it.
        assert_eq!(
            ExecutionMode::Auto.next_allowed(Mode::Full),
            Some(ExecutionMode::Plan)
        );
        assert_eq!(
            ExecutionMode::Plan.next_allowed(Mode::Full),
            Some(ExecutionMode::Auto)
        );
        // `--mode plan` cannot be left, so there is nowhere to cycle to.
        assert_eq!(ExecutionMode::Plan.next_allowed(Mode::Plan), None);
        assert_eq!(
            ExecutionMode::Plan.next_allowed(Mode::Default),
            Some(ExecutionMode::Interactive)
        );
    }

    #[test]
    fn test_a_mode_that_constrains_the_agent_tells_it_so() {
        // A refusal the model cannot explain reads as a broken workspace, and it
        // will keep trying. The permissive modes need no note.
        assert!(Mode::Plan.prompt_note().is_some());
        assert!(Mode::Ask.prompt_note().is_some());
        assert!(Mode::Full.prompt_note().is_none());
    }
}

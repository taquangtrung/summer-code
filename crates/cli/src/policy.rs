//! Loading the four policy sources and merging them by trust.
//!
//! The frontend owns the file handles: `policy` is a contract crate and
//! does no I/O, so reading is here and merging is there.
//!
//! **The project file is read once, at session start, and hashed into
//! `policy_digest`.** It also sits on the built-in deny list next to `.git/`, so
//! the agent cannot rewrite its own policy mid-session. Propose-as-`Ask` bounds
//! what a hostile project file can win; the deny bounds what the running agent
//! can author.

use std::error::Error;
use std::path::{Path, PathBuf};

use policy::{Effect, ListenRule, PathAccess, Policy, PolicyFile, RuleSpec, merge};

use crate::mode::Mode;

// ========================================================================
// Constants
// ========================================================================

/// Repo content, and therefore untrusted: its allows become `Ask`.
const PROJECT_POLICY: &str = ".summer/policy.json";

/// The user's standing preferences, outside any repo, and therefore trusted.
const USER_POLICY: &str = "policy.json";

/// Where the home directory is named, in order of preference.
#[cfg(windows)]
const HOME_VARS: &[&str] = &["USERPROFILE", "HOME"];

#[cfg(not(windows))]
const HOME_VARS: &[&str] = &["HOME"];

// ========================================================================
// Functions
// ========================================================================

/// Assemble the effective policy for a session rooted at `root`, in `mode`.
///
/// The trusted layers stack low-to-high: built-in default, then the user file,
/// then the session's mode. The project file goes in as the untrusted tier. Its
/// denies apply, its allows become prompts, and it cannot reach a trusted
/// decision.
///
/// **The mode goes on top of the user's own file, and that ordering is the
/// point.** A mode is what the user asked for on this run, so it should win over
/// what they asked for in general: `--mode plan` has to be able to refuse a
/// write their `policy.json` allows, or it is not a mode. It still sits under
/// the project's denies, like every trusted layer.
pub fn load(root: &Path, mode: Mode) -> Result<Policy, Box<dyn Error>> {
    let mut trusted = vec![Policy::builtin(root.to_path_buf())];
    // Below the user's file for a mode that defers to it, above for one that
    // must override it; see `Mode::outranks_user_policy`.
    if let Some(text) = mode.overlay().filter(|_| !mode.outranks_user_policy()) {
        trusted.push(parse(text, root)?);
    }
    if let Some(path) = user_policy_path()
        && let Some(user) = read_optional(&path)?
    {
        trusted.push(parse(&user, root)?);
    }
    if let Some(text) = mode.overlay().filter(|_| mode.outranks_user_policy()) {
        trusted.push(parse(text, root)?);
    }

    let project = read_optional(&root.join(PROJECT_POLICY))?
        .map(|text| parse(&text, root))
        .transpose()?;

    let policy = merge(root.to_path_buf(), trusted, project.as_ref());
    policy.validate()?;
    Ok(policy)
}

/// The `plan` overlay on its own, for the gate `/mode plan` closes mid-session.
/// Parsed from the same text `--mode plan` merges, so the two cannot drift.
pub fn plan_overlay(root: &Path) -> Result<Policy, Box<dyn Error>> {
    let text = Mode::Plan.overlay().ok_or("plan mode carries no overlay")?;
    let plan = parse(text, root)?;
    plan.validate()?;
    Ok(plan)
}

/// Whether a prompt's rule can be written into a policy file **without granting
/// more than the user approved**.
///
/// Only path rules today, and the exclusion is the interesting half. An exec
/// rule carries the program *and its argv*, because the prompt shows a full
/// command line and the rule offered has to be the thing the user read.
/// `CommandEntry` has no argv: saving `cargo test` would write `cargo`, and the
/// next session would run `cargo publish` without asking. A prompt that offered
/// to remember one thing and remembered a larger one is worse than a prompt that
/// asks again, so the option is simply not offered there.
pub fn savable(rule: &RuleSpec) -> bool {
    matches!(rule, RuleSpec::Path { .. })
}

/// Append `rule` to the user's policy file, creating it if needed.
///
/// The user tier, never the project tier: this is the user's standing answer and
/// belongs with their other standing answers, outside any repository. The agent
/// cannot write it either way, since `~/.summer` is outside the root the sandbox
/// confines it to.
pub fn save_rule(rule: &RuleSpec) -> Result<(), Box<dyn Error>> {
    let Some(dir) = config_dir() else {
        return Err("no home directory: cannot find ~/.summer to save the rule in".into());
    };
    let path = dir.join(USER_POLICY);
    let existing = read_optional(&path)?;
    let merged = with_rule(existing.as_deref(), rule)?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&path, merged)?;
    Ok(())
}

/// The policy document with `rule` added as an allow.
///
/// Round-trips through a raw [`serde_json::Value`] rather than through
/// [`PolicyFile`], for the reason `config::merge_provider_prefs` gives: writing
/// the file back from a typed struct would silently drop every key this function
/// does not know about, and a policy file is exactly where a user's careful
/// denies live.
fn with_rule(existing: Option<&str>, rule: &RuleSpec) -> Result<String, Box<dyn Error>> {
    let RuleSpec::Path { access, pattern } = rule else {
        return Err("only path rules can be saved".into());
    };
    let mut root: serde_json::Value = match existing {
        Some(text) => serde_json::from_str(text)?,
        None => serde_json::json!({}),
    };
    let object = root.as_object_mut().ok_or("policy is not a JSON object")?;
    let paths = object
        .entry("paths")
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .ok_or("`paths` is not an array")?;
    let entry = serde_json::json!({
        "pattern": pattern,
        "access": match access {
            PathAccess::Read => "read",
            PathAccess::Write => "write",
        },
        "effect": "allow",
    });
    // Answering the same prompt twice must not grow the file forever.
    if !paths.contains(&entry) {
        paths.push(entry);
    }
    Ok(serde_json::to_string_pretty(&root)?)
}

fn parse(text: &str, root: &Path) -> Result<Policy, Box<dyn Error>> {
    let file: PolicyFile = serde_json::from_str(text)?;
    Ok(file.into_policy(root.to_path_buf())?)
}

/// Read a policy file that may not exist. A missing file is not an error; an
/// unreadable one is, because silently running with less policy than the user
/// wrote is the wrong direction to fail in.
pub(crate) fn read_optional(path: &Path) -> Result<Option<String>, Box<dyn Error>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", path.display()).into()),
    }
}

fn user_policy_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join(USER_POLICY))
}

/// The user config dir: `<home>/.summer`, home-direct like Claude's
/// `~/.claude`.
///
/// **A missing home means "no user config", never a relative path.** The old
/// fallback was a bare `.summer`, which resolves against the working directory:
/// under systemd, `env -i`, or a container without `HOME`, `user_policy_path()`
/// became `<cwd>/.summer/policy.json` -- the *project* file, whose whole purpose
/// is to be demoted to the untrusted tier. A repo shipping
/// `{"commands":[{"effect":"allow","program":"sh"}]}` would have been honoured
/// outright, and `<cwd>/.summer/skills` would have become a trusted
/// prompt-injection source. The same reasoning holds on every platform, which is
/// why the fallible lookup is shared and only the variables differ.
pub(crate) fn config_dir() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".summer"))
}

/// The per-project store under `~/.summer/projects/<encoded-root>`, where
/// sessions and snapshots live — Claude Code's layout, adopted for the same
/// reasons it chose it: nothing lands inside the repo (no `.gitignore` chore,
/// nothing accidentally committed), and the store outlives the checkout. A
/// workspace-rooted sandbox *can* still block writes to it, which is why the
/// callers that run inside confinement keep the in-project store instead (see
/// `Bootstrap::load`). `None` when no home directory can be resolved; the
/// callers fall back to the in-project layout rather than refusing to run, the
/// same posture `config_dir`'s readers take.
///
/// The encoding is the root's absolute path with separators replaced by `-`
/// (Claude Code's scheme): human-readable in a directory listing, and unique
/// per location — `/home/u/Work/a` → `~/.summer/projects/-home-u-Work-a`.
pub(crate) fn project_store_dir(root: &Path) -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("projects").join(encode_root(root)))
}

/// The root encoded as one path component: separators become `-`. Pure, so
/// the scheme is testable without touching the process environment.
fn encode_root(root: &Path) -> String {
    root.to_string_lossy().replace(['/', '\\'], "-")
}

/// Where this workspace's session journals live: the central per-project
/// store when a home directory is known, the in-project `.summer/sessions`
/// otherwise (no `HOME`, `env -i`, a container). One function so `open_journal`,
/// `/status`, `/sessions`, and `/resume` can never disagree about the path —
/// the journal being somewhere `/resume` does not look was the failure shape
/// this consolidation exists to prevent.
pub(crate) fn journal_dir(cwd: &Path) -> PathBuf {
    project_store_dir(cwd)
        .map(|store| store.join("sessions"))
        .unwrap_or_else(|| cwd.join(".summer").join("sessions"))
}

/// The user's home directory, or `None` if the environment does not say.
///
/// Windows has no `HOME`: `USERPROFILE` is the documented answer, and
/// `HOMEDRIVE`+`HOMEPATH` is the older pair that domain-joined machines still
/// set. Both are tried before giving up, because giving up means silently
/// running without the user's policy and skills.
fn home_dir() -> Option<PathBuf> {
    for key in HOME_VARS {
        if let Some(home) = absolute_from_env(key) {
            return Some(home);
        }
    }
    #[cfg(windows)]
    if let (Some(drive), Some(path)) = (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH"))
    {
        let mut home = PathBuf::from(drive);
        home.push(PathBuf::from(path));
        if home.is_absolute() {
            return Some(home);
        }
    }
    tracing::warn!(
        "no home directory in the environment ({}); ignoring user config and skills",
        HOME_VARS.join(" or ")
    );
    None
}

/// One environment variable as an absolute path, or `None`.
///
/// A relative value is refused for the same reason an unset one is: it resolves
/// against the working directory, which is the repo.
fn absolute_from_env(key: &str) -> Option<PathBuf> {
    let value = std::env::var_os(key)?;
    if value.is_empty() {
        return None;
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        tracing::warn!(%key, path = %path.display(), "home path is not absolute; ignoring it");
        return None;
    }
    Some(path)
}

/// Render the effective [`Policy`] as readable text for `/policy`. The view
/// model owns no policy types, so the frontend renders once at startup and the
/// TUI shows the string.
pub(crate) fn render_policy(policy: &Policy) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "policy (root {}):",
        crate::display_path(&policy.root)
    ));

    out.push_str("\n  commands:");
    if policy.commands.is_empty() {
        out.push_str(" (none)");
    }
    for rule in &policy.commands {
        out.push_str(&format!(
            "\n    {} {}",
            effect_label(rule.effect),
            rule.program
        ));
    }

    out.push_str("\n  paths:");
    if policy.paths.is_empty() {
        out.push_str(" (none)");
    }
    for rule in &policy.paths {
        let access = rule
            .access
            .map(|a| format!("{} ", access_label(a)))
            .unwrap_or_default();
        out.push_str(&format!(
            "\n    {} {}{}",
            effect_label(rule.effect),
            access,
            rule.pattern
        ));
    }

    out.push_str("\n  network:");
    out.push_str("\n    connect:");
    if policy.network.connect.is_empty() {
        out.push_str(" (none — outbound denied by default)");
    }
    for rule in &policy.network.connect {
        let port = rule.port.map(|p| format!(":{p}")).unwrap_or_default();
        out.push_str(&format!(
            "\n      {} {}{}",
            effect_label(rule.effect),
            rule.host,
            port
        ));
    }
    out.push_str(&format!(
        "\n    listen: {}",
        listen_label(&policy.network.listen)
    ));
    out
}

fn effect_label(effect: Effect) -> &'static str {
    match effect {
        Effect::Allow => "allow",
        Effect::Ask => "ask",
        Effect::Deny => "deny",
    }
}

fn access_label(access: PathAccess) -> &'static str {
    match access {
        PathAccess::Read => "read",
        PathAccess::Write => "write",
    }
}

fn listen_label(rule: &ListenRule) -> String {
    match rule {
        ListenRule::Any { ports } => format!("any{}", port_list(ports)),
        ListenRule::Deny => "deny".to_string(),
        ListenRule::Loopback { ports } => format!("loopback{}", port_list(ports)),
    }
}

fn port_list(ports: &[u16]) -> String {
    if ports.is_empty() {
        String::new()
    } else {
        format!(
            " [{}]",
            ports
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(",")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use policy::{CommandRule, Effect, HostRule, NetworkRule, PathAccess, PathRule};

    #[test]
    fn a_saved_rule_lands_in_the_paths_array() {
        let json = with_rule(
            None,
            &RuleSpec::Path {
                access: PathAccess::Write,
                pattern: "/repo/src/**".into(),
            },
        )
        .expect("a path rule saves");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(parsed["paths"][0]["pattern"], "/repo/src/**");
        assert_eq!(parsed["paths"][0]["effect"], "allow");
        assert_eq!(parsed["paths"][0]["access"], "write");
        // It has to parse back as a policy, or the next session refuses to start
        // on a file this function wrote.
        parse(&json, Path::new("/repo")).expect("the saved file is a valid policy");
    }

    #[test]
    fn saving_preserves_everything_already_in_the_file() {
        // A policy file is where a user's careful denies live. Rewriting it from
        // a typed struct would drop every key this function does not know.
        let existing = r#"{"commands":[{"program":"cargo","effect":"deny"}],"paths":[]}"#;
        let json = with_rule(
            Some(existing),
            &RuleSpec::Path {
                access: PathAccess::Read,
                pattern: "/repo/**".into(),
            },
        )
        .expect("saves");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(parsed["commands"][0]["program"], "cargo");
        assert_eq!(parsed["paths"].as_array().expect("paths").len(), 1);
    }

    #[test]
    fn saving_the_same_rule_twice_does_not_grow_the_file() {
        let rule = RuleSpec::Path {
            access: PathAccess::Write,
            pattern: "/repo/**".into(),
        };
        let once = with_rule(None, &rule).expect("saves");
        let twice = with_rule(Some(&once), &rule).expect("saves");
        let parsed: serde_json::Value = serde_json::from_str(&twice).expect("valid JSON");
        assert_eq!(parsed["paths"].as_array().expect("paths").len(), 1);
    }

    #[test]
    fn an_exec_rule_is_not_savable() {
        // Saving it would write the program without its argv, granting more
        // than the prompt showed.
        assert!(!savable(&RuleSpec::Exec {
            args: vec!["test".into()],
            program: "cargo".into(),
        }));
        assert!(savable(&RuleSpec::Path {
            access: PathAccess::Write,
            pattern: "/repo/**".into(),
        }));
    }

    #[test]
    fn render_policy_lists_every_rule() {
        let policy = Policy {
            commands: vec![CommandRule {
                effect: Effect::Allow,
                program: "git".into(),
            }],
            network: NetworkRule {
                connect: vec![HostRule {
                    effect: Effect::Allow,
                    host: "api.anthropic.com".into(),
                    port: Some(443),
                }],
                listen: ListenRule::Loopback { ports: vec![] },
            },
            paths: vec![PathRule {
                access: Some(PathAccess::Write),
                effect: Effect::Deny,
                pattern: "**/.*".into(),
            }],
            root: PathBuf::from("/repo"),
        };
        let text = render_policy(&policy);
        assert!(text.contains("allow git"), "command rendered: {text}");
        assert!(text.contains("deny write **/.*"), "path rendered: {text}");
        assert!(
            text.contains("allow api.anthropic.com:443"),
            "connect rendered: {text}"
        );
        assert!(text.contains("listen: loopback"), "listen rendered: {text}");
    }

    #[test]
    fn encode_root_is_one_component_claude_style() {
        assert_eq!(
            super::encode_root(Path::new("/home/u/Work/app")),
            "-home-u-Work-app"
        );
        // Windows separators collapse the same way, so the encoding is
        // platform-independent even though it is built from a native path.
        assert_eq!(
            super::encode_root(Path::new("C:\\Work\\app")),
            "C:-Work-app"
        );
    }
}

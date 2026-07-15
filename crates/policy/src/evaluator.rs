//! The built-in rules evaluator.
//!
//! Rule lists are precedence-ordered (highest first); the first match wins. A
//! request matching no rule falls to a default per access kind: reads under root
//! are allowed, writes and execs are asked about, outbound network is denied,
//! and listen follows [`ListenRule`].
//!
//! **Paths are normalized before evaluation, and that is where `..` escapes
//! die.** It belongs here rather than in each rule: a rule that had to remember
//! to defend itself is a rule that will forget.
//!
//! Normalization is *lexical*: it resolves `.` and `..` without touching the
//! filesystem, because this is a contract crate and does no I/O. **Symlink
//! resolution is therefore not done here and cannot be**, so it is the broker's
//! job to canonicalize a path before building a [`Request`]. Both are needed:
//! the broker resolves what the path really points at, and this defends against
//! a `..` that never reaches the broker's canonicalizer.

use std::path::{Component, Path, PathBuf};

use crate::decision::Decision;
use crate::policy::{CommandRule, Effect, ListenRule, PathAccess, Policy};
use crate::request::Request;
use crate::shell_line::commands_of;

// ========================================================================
// Traits
// ========================================================================

/// Lets the decision come from elsewhere (an org policy server, a webhook). The
/// built-in [`RulesEvaluator`] is the default.
///
/// `&self` and pure: an external evaluator stays a plain function of policy and
/// request, rather than something that has to be told what the user clicked.
/// Remembered grants are the broker's cache, not the evaluator's business.
pub trait PolicyEvaluator: Send + Sync {
    fn evaluate(&self, policy: &Policy, request: &Request) -> Decision;
}

// ========================================================================
// Data Structures
// ========================================================================

/// Evaluates a policy's rules with the default per-access semantics.
pub struct RulesEvaluator;

// ========================================================================
// RulesEvaluator Implementations
// ========================================================================

impl RulesEvaluator {
    pub fn new() -> Self {
        Self
    }

    /// Rules first, then the root check.
    ///
    /// The order is the trust axis, not an implementation detail. A root check
    /// that ran first would be a ceiling no configuration could raise, and the
    /// trusted tiers are explicitly allowed to widen: a user file that cannot
    /// grant what the default withheld is a user file nobody can use, and
    /// `--allow-write /tmp/out` would be a flag that silently does nothing. So a
    /// matching rule decides, and containment is what answers when nothing does.
    ///
    /// This is safe only because the built-in allow is anchored at the root. A
    /// bare `**` there would match `/etc/passwd` and this order would hand it
    /// over. The untrusted tier cannot exploit the same door: its allows arrive
    /// as `Ask`, so the worst a hostile project file wins is a prompt.
    fn path_decision(&self, policy: &Policy, path: &Path, is_read: bool) -> Decision {
        let normalized = normalize(path);
        let wanted = if is_read {
            PathAccess::Read
        } else {
            PathAccess::Write
        };
        for rule in &policy.paths {
            if rule.access.is_none_or(|access| access == wanted)
                && matches_path(&rule.pattern, &normalized)
            {
                return effect_to_decision(rule.effect, "path rule");
            }
        }
        if !normalized.starts_with(&policy.root) {
            return Decision::Deny {
                reason: format!("`{}` is outside the root", normalized.display()),
            };
        }
        // Read is the permissive default; write is not.
        if is_read {
            Decision::Allow
        } else {
            Decision::Ask
        }
    }
}

impl Default for RulesEvaluator {
    fn default() -> Self {
        Self::new()
    }
}

impl PolicyEvaluator for RulesEvaluator {
    fn evaluate(&self, policy: &Policy, request: &Request) -> Decision {
        match request {
            Request::Read(path) => self.path_decision(policy, path, true),
            Request::Write(path) => self.path_decision(policy, path, false),
            Request::Exec { program, .. } => {
                if let Some(decision) = command_decision(policy, program) {
                    return decision;
                }
                commands_of(request)
                    .map_or(Decision::Ask, |commands| line_decision(policy, &commands))
            }
            Request::Connect { host, port } => {
                for rule in &policy.network.connect {
                    if matches_host(&rule.host, host) && matches_port(rule.port, *port) {
                        return effect_to_decision(rule.effect, "network rule");
                    }
                }
                Decision::Deny {
                    reason: "outbound network is not allowed".to_string(),
                }
            }
            Request::Listen { port } => match &policy.network.listen {
                ListenRule::Deny => Decision::Deny {
                    reason: "inbound listen is denied".to_string(),
                },
                ListenRule::Any { ports } | ListenRule::Loopback { ports } => {
                    if ports.is_empty() || ports.contains(port) {
                        Decision::Allow
                    } else {
                        Decision::Deny {
                            reason: format!("port {port} is not in the listen list"),
                        }
                    }
                }
            },
        }
    }
}

// ========================================================================
// Functions
// ========================================================================

/// Resolve `.` and `..` lexically. No I/O, so symlinks are untouched: see the
/// module docs for why that split exists and who covers the other half.
///
/// Public because the monitor needs the *same* predicate the evaluator used. A
/// containment check that normalized on one side and not the other would pass
/// `/repo/../../etc/passwd`, which is exactly the string it exists to catch.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Popping past the start leaves it empty, which then fails the
                // root check rather than silently rebasing.
                out.pop();
            }
            Component::Normal(part) => out.push(part),
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
        }
    }
    out
}

/// `literal_separator` so `src/*` does not reach `src/a/b`: a rule that silently
/// spans directories is a rule that grants more than it reads as.
///
/// `pub(crate)` because a remembered [`crate::RuleSpec`] must match by exactly
/// the predicate the evaluator used. Two matchers over the same pattern string
/// that disagree is a grant that covers more than the answer the user gave.
pub(crate) fn matches_path(pattern: &str, path: &Path) -> bool {
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .is_ok_and(|glob| glob.compile_matcher().is_match(path))
}

/// Hosts are globs, so `*.internal` means what it reads as.
///
/// Exact comparison made every wildcard rule match nothing, and a deny that
/// matches nothing fails *open*: `[deny "*.evil.com", allow "*"]` allowed
/// `bad.evil.com`. `.` is not a glob separator, so `*.evil.com` spans
/// subdomains, which is the reading a user writing that rule intends.
///
/// **Both sides are normalized first, and for the same fail-open reason.** DNS
/// names are case-insensitive and a trailing dot is the same name fully
/// qualified, so with the documented `[deny "*.evil.com", allow "*"]` shape
/// `bad.EVIL.com` and `bad.evil.com.` each matched no deny and fell through to
/// the allow. A rule that the resolver and the rule-writer both read as one name
/// must not be two names here.
fn matches_host(pattern: &str, host: &str) -> bool {
    let pattern = normalize_host(pattern);
    let host = normalize_host(host);
    globset::Glob::new(&pattern).is_ok_and(|glob| glob.compile_matcher().is_match(&host))
}

/// Lowercase, and drop the root label's trailing dot.
fn normalize_host(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// Match a command rule against the program the model asked to run.
///
/// The asymmetry is the point, and it is why this is not one glob test. A deny
/// also matches the program's file name, so denying `rm` cannot be dodged by
/// asking for `/bin/rm`. An allow matches only what it literally says, so
/// allowing `cargo` never admits `/tmp/evil/cargo`. Each direction fails safe.
fn matches_program(rule: &CommandRule, program: &str) -> bool {
    if matches_path(&rule.program, Path::new(program)) {
        return true;
    }
    match rule.effect {
        Effect::Deny => Path::new(program)
            .file_name()
            .is_some_and(|name| matches_path(&rule.program, Path::new(name))),
        Effect::Allow | Effect::Ask => false,
    }
}

/// The one command in a shell line that `policy` does not already allow, when
/// there is exactly one.
///
/// **This is what a prompt about the line is really asking**: every other
/// program in it would run unasked as a call of its own, and the line was read
/// whole, so it holds no expansion, assignment or file write besides them.
/// Asking about this command, and remembering the answer as a rule about it,
/// is therefore asking about the line. With two or more unlisted programs there
/// is no single question, so this answers `None` and the line is asked about
/// whole.
pub fn sole_unlisted_command(
    evaluator: &dyn PolicyEvaluator,
    policy: &Policy,
    request: &Request,
) -> Option<Request> {
    let mut unlisted = commands_of(request)?
        .into_iter()
        .filter(|command| evaluator.evaluate(policy, command) != Decision::Allow);
    let sole = unlisted.next()?;
    unlisted.next().is_none().then_some(sole)
}

/// What the first command rule naming `program` decides, if one does.
fn command_decision(policy: &Policy, program: &str) -> Option<Decision> {
    policy
        .commands
        .iter()
        .find(|rule| matches_program(rule, program))
        .map(|rule| effect_to_decision(rule.effect, "command rule"))
}

/// A shell line judged by the programs it runs.
///
/// **Reached only when no rule names the shell itself**, so a mode or a user
/// rule about `sh` still decides a line outright: `full`'s `*` allow, `plan`'s
/// `*` deny and `ask`'s `*` ask all match the shell before this runs.
///
/// One denied program denies the line, which also closes a dodge: a program
/// the user denied could otherwise be wrapped in `sh -c` and reach a prompt
/// with the deny never consulted. Every program allowed allows it; anything
/// else asks, as the line did before it could be read.
fn line_decision(policy: &Policy, commands: &[Request]) -> Decision {
    let mut all_allowed = true;
    for command in commands {
        let Request::Exec { program, .. } = command else {
            continue;
        };
        match command_decision(policy, program) {
            Some(Decision::Deny { reason }) => {
                return Decision::Deny {
                    reason: format!("`{program}` in the command line: {reason}"),
                };
            }
            Some(Decision::Allow) => {}
            Some(Decision::Ask) | None => all_allowed = false,
        }
    }
    if all_allowed {
        Decision::Allow
    } else {
        Decision::Ask
    }
}

/// `None` in the rule means any port.
fn matches_port(rule: Option<u16>, port: u16) -> bool {
    rule.is_none_or(|allowed| allowed == port)
}

fn effect_to_decision(effect: Effect, reason: &'static str) -> Decision {
    match effect {
        Effect::Allow => Decision::Allow,
        Effect::Ask => Decision::Ask,
        Effect::Deny => Decision::Deny {
            reason: reason.to_string(),
        },
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::policy::{CommandRule, HostRule, NetworkRule, PathRule};

    fn policy() -> Policy {
        Policy::builtin(PathBuf::from("/repo"))
    }

    fn decide(policy: &Policy, request: Request) -> Decision {
        RulesEvaluator::new().evaluate(policy, &request)
    }

    #[test]
    fn a_dotdot_escape_is_denied() {
        // The escape this exists to defeat: the path is under root as written
        // and outside it once resolved.
        let decision = decide(
            &policy(),
            Request::Read(PathBuf::from("/repo/src/../../etc/passwd")),
        );
        assert!(
            matches!(decision, Decision::Deny { .. }),
            "got {decision:?}"
        );
    }

    #[test]
    fn a_dotdot_that_stays_inside_is_fine() {
        // Normalization must not be a blunt "contains .. means no".
        let decision = decide(
            &policy(),
            Request::Read(PathBuf::from("/repo/src/../README.md")),
        );
        assert!(matches!(decision, Decision::Allow), "got {decision:?}");
    }

    #[test]
    fn a_curdir_noise_path_still_matches_its_rule() {
        let decision = decide(
            &policy(),
            Request::Write(PathBuf::from("/repo/./.git/./config")),
        );
        assert!(
            matches!(decision, Decision::Deny { .. }),
            "the .git deny must survive normalization, got {decision:?}"
        );
    }

    #[test]
    fn the_agent_may_not_rewrite_its_own_policy() {
        let decision = decide(
            &policy(),
            Request::Write(PathBuf::from("/repo/.summer/policy.json")),
        );
        assert!(
            matches!(decision, Decision::Deny { .. }),
            "got {decision:?}"
        );
    }

    #[test]
    fn root_dotfiles_are_denied_but_nested_dotfiles_are_not() {
        let denied = decide(&policy(), Request::Write(PathBuf::from("/repo/.env")));
        assert!(matches!(denied, Decision::Deny { .. }), "got {denied:?}");

        let allowed = decide(&policy(), Request::Write(PathBuf::from("/repo/src/.keep")));
        assert!(matches!(allowed, Decision::Allow), "got {allowed:?}");
    }

    #[test]
    fn outside_root_is_denied() {
        let decision = decide(&policy(), Request::Read(PathBuf::from("/etc/passwd")));
        assert!(matches!(decision, Decision::Deny { .. }));
    }

    #[test]
    fn an_unruled_write_is_asked_and_an_unruled_read_is_allowed() {
        let bare = Policy {
            commands: vec![],
            network: NetworkRule::default(),
            paths: vec![],
            root: PathBuf::from("/repo"),
        };
        assert!(matches!(
            decide(&bare, Request::Write(PathBuf::from("/repo/src/foo.rs"))),
            Decision::Ask
        ));
        assert!(matches!(
            decide(&bare, Request::Read(PathBuf::from("/repo/src/foo.rs"))),
            Decision::Allow
        ));
    }

    #[test]
    fn an_ask_rule_asks() {
        // The project tier's downgraded allows arrive as Ask rules, so the
        // evaluator has to carry the effect through rather than treat every
        // matched rule as allow-or-deny.
        let policy = Policy {
            commands: vec![CommandRule {
                effect: Effect::Ask,
                program: "make".to_string(),
            }],
            network: NetworkRule::default(),
            paths: vec![],
            root: PathBuf::from("/repo"),
        };
        assert!(matches!(
            decide(
                &policy,
                Request::Exec {
                    args: vec![],
                    program: "make".to_string(),
                }
            ),
            Decision::Ask
        ));
    }

    #[test]
    fn an_unruled_exec_is_asked() {
        assert!(matches!(
            decide(
                &policy(),
                Request::Exec {
                    args: vec![],
                    program: "make".to_string()
                }
            ),
            Decision::Ask
        ));
    }

    #[test]
    fn connect_is_denied_by_default() {
        assert!(matches!(
            decide(
                &policy(),
                Request::Connect {
                    host: "example.com".to_string(),
                    port: 443
                }
            ),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn a_host_rule_respects_its_port() {
        // "reach localhost:5432" must not become "reach localhost on anything".
        let mut policy = policy();
        policy.network.connect.insert(
            0,
            HostRule {
                effect: Effect::Allow,
                host: "localhost".to_string(),
                port: Some(5432),
            },
        );
        assert!(matches!(
            decide(
                &policy,
                Request::Connect {
                    host: "localhost".to_string(),
                    port: 5432
                }
            ),
            Decision::Allow
        ));
        assert!(matches!(
            decide(
                &policy,
                Request::Connect {
                    host: "localhost".to_string(),
                    port: 22
                }
            ),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn listen_honors_its_port_list() {
        let mut policy = policy();
        policy.network.listen = ListenRule::Loopback { ports: vec![8080] };
        assert!(matches!(
            decide(&policy, Request::Listen { port: 8080 }),
            Decision::Allow
        ));
        assert!(matches!(
            decide(&policy, Request::Listen { port: 22 }),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn listen_deny_denies() {
        let mut policy = policy();
        policy.network.listen = ListenRule::Deny;
        assert!(matches!(
            decide(&policy, Request::Listen { port: 8080 }),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn the_builtin_denies_writing_git_but_not_reading_it() {
        // "Deny writes to `.git/`" and "deny `.git/`" are different rules. The
        // second one also stops the agent reading `.git/HEAD` to report the
        // branch, which is not what the default means to say.
        let write = decide(
            &policy(),
            Request::Write(PathBuf::from("/repo/.git/config")),
        );
        assert!(matches!(write, Decision::Deny { .. }), "got {write:?}");

        let read = decide(&policy(), Request::Read(PathBuf::from("/repo/.git/HEAD")));
        assert!(
            matches!(read, Decision::Allow),
            "the agent must still be able to read .git, got {read:?}"
        );
    }

    #[test]
    fn the_builtin_lets_the_policy_file_be_read_but_not_written() {
        // The plan has it read once at session start and hashed into
        // `policy_digest`. A rule that denied reads would deny that.
        let path = PathBuf::from("/repo/.summer/policy.json");
        assert!(matches!(
            decide(&policy(), Request::Write(path.clone())),
            Decision::Deny { .. }
        ));
        assert!(matches!(
            decide(&policy(), Request::Read(path)),
            Decision::Allow
        ));
    }

    #[test]
    fn a_trusted_allow_may_widen_past_the_root() {
        // The trust axis: a user file or `--allow-write /tmp/out` must be able to
        // grant what the default withheld. A root check that ran before the rules
        // was a ceiling no configuration could raise, which made the flag a no-op.
        let mut policy = policy();
        policy.paths.insert(
            0,
            PathRule {
                access: None,
                effect: Effect::Allow,
                pattern: "/tmp/**".to_string(),
            },
        );
        assert!(matches!(
            decide(&policy, Request::Write(PathBuf::from("/tmp/out.txt"))),
            Decision::Allow
        ));
    }

    #[test]
    fn an_unruled_path_outside_the_root_is_still_denied() {
        // Rules-first is only safe because the built-in allow is anchored at the
        // root. If it were a bare `**` this would hand over /etc/passwd.
        let decision = decide(&policy(), Request::Read(PathBuf::from("/etc/passwd")));
        assert!(
            matches!(decision, Decision::Deny { .. }),
            "got {decision:?}"
        );
    }

    #[test]
    fn a_host_deny_glob_matches_subdomains_rather_than_failing_open() {
        // Exact comparison made every wildcard rule match nothing, and a deny
        // that matches nothing loses to the allow below it.
        let mut policy = policy();
        policy.network.connect = vec![
            HostRule {
                effect: Effect::Deny,
                host: "*.evil.com".to_string(),
                port: None,
            },
            HostRule {
                effect: Effect::Allow,
                host: "*".to_string(),
                port: None,
            },
        ];
        let decision = decide(
            &policy,
            Request::Connect {
                host: "bad.evil.com".to_string(),
                port: 443,
            },
        );
        assert!(
            matches!(decision, Decision::Deny { .. }),
            "a wildcard deny must not fail open, got {decision:?}"
        );
    }

    #[test]
    fn a_command_deny_cannot_be_dodged_by_path_qualifying_it() {
        let mut policy = policy();
        policy.commands.insert(
            0,
            CommandRule {
                effect: Effect::Deny,
                program: "rm".to_string(),
            },
        );
        for spelling in ["rm", "/bin/rm", "/usr/bin/rm"] {
            let decision = decide(
                &policy,
                Request::Exec {
                    args: vec![],
                    program: spelling.to_string(),
                },
            );
            assert!(
                matches!(decision, Decision::Deny { .. }),
                "`{spelling}` dodged the deny on `rm`, got {decision:?}"
            );
        }
    }

    #[test]
    fn a_command_allow_does_not_widen_to_a_lookalike_elsewhere() {
        // The other direction must not follow: allowing `cargo` cannot admit
        // whatever the model drops at /tmp/evil/cargo.
        let decision = decide(
            &policy(),
            Request::Exec {
                args: vec![],
                program: "/tmp/evil/cargo".to_string(),
            },
        );
        assert!(
            matches!(decision, Decision::Ask),
            "an allow on `cargo` must not cover /tmp/evil/cargo, got {decision:?}"
        );
    }

    #[test]
    fn a_star_pattern_does_not_span_directories() {
        let policy = Policy {
            commands: vec![],
            network: NetworkRule::default(),
            paths: vec![PathRule {
                access: None,
                effect: Effect::Deny,
                pattern: "/repo/*".to_string(),
            }],
            root: PathBuf::from("/repo"),
        };
        assert!(matches!(
            decide(&policy, Request::Write(PathBuf::from("/repo/top.rs"))),
            Decision::Deny { .. }
        ));
        assert!(
            matches!(
                decide(&policy, Request::Write(PathBuf::from("/repo/src/deep.rs"))),
                Decision::Ask
            ),
            "`*` must not reach across a separator"
        );
    }

    fn shell(line: &str) -> Request {
        Request::Exec {
            args: vec!["-c".to_string(), line.to_string()],
            program: "sh".to_string(),
        }
    }

    #[test]
    fn a_shell_line_of_allowed_programs_runs_without_asking() {
        // Every one of these prompted before the line could be read, because
        // the policy only ever saw `sh`.
        for line in [
            "cargo test -p cli 2>&1 | tail -6",
            "ls docs; ls crates",
            "git diff HEAD -- src | head -60",
            "rg -n parse src && cargo check 2>&1 | grep -E error",
        ] {
            assert_eq!(decide(&policy(), shell(line)), Decision::Allow, "{line}");
        }
    }

    #[test]
    fn a_shell_line_with_one_unlisted_program_still_asks() {
        // The line is only as allowed as its least-allowed program.
        assert_eq!(
            decide(&policy(), shell("cargo build && rm -rf target")),
            Decision::Ask
        );
    }

    #[test]
    fn a_line_this_reader_cannot_follow_still_asks() {
        // An allowed program wrapped in an expansion or a file redirect is not
        // the allowed program any more.
        for line in [
            "ls $(rm -rf ~)",
            "cargo test > out.txt",
            "PATH=/tmp cargo test",
        ] {
            assert_eq!(decide(&policy(), shell(line)), Decision::Ask, "{line}");
        }
    }

    #[test]
    fn a_denied_program_cannot_be_dodged_by_wrapping_it_in_a_shell() {
        // Before lines were read, `sh -c "rm ..."` reached a prompt with the
        // deny on `rm` never consulted.
        let mut policy = policy();
        policy.commands.insert(
            0,
            CommandRule {
                effect: Effect::Deny,
                program: "rm".to_string(),
            },
        );
        assert!(matches!(
            decide(&policy, shell("ls && /bin/rm -rf target")),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn a_rule_naming_the_shell_still_decides_the_whole_line() {
        // The modes are rules about `*`, which matches `sh`: `plan` must keep
        // denying a line of allowed programs, and `ask` must keep asking.
        for (effect, expected) in [(Effect::Deny, "deny"), (Effect::Ask, "ask")] {
            let mut policy = policy();
            policy.commands.insert(
                0,
                CommandRule {
                    effect,
                    program: "*".to_string(),
                },
            );
            let decision = decide(&policy, shell("ls"));
            let got = match decision {
                Decision::Allow => "allow",
                Decision::Ask => "ask",
                Decision::Deny { .. } => "deny",
            };
            assert_eq!(got, expected);
        }
    }

    #[test]
    fn a_line_with_one_unlisted_program_is_a_question_about_that_program() {
        let evaluator = RulesEvaluator::new();
        let sole = sole_unlisted_command(
            &evaluator,
            &policy(),
            &shell("python3 -m unittest 2>&1 | tail -5"),
        );
        assert_eq!(
            sole,
            Some(Request::Exec {
                args: vec!["-m".into(), "unittest".into()],
                program: "python3".into(),
            })
        );
    }

    #[test]
    fn a_line_with_no_single_unlisted_program_has_no_single_question() {
        // Two unlisted programs cannot be one remembered answer; none means
        // there was nothing to ask; an unreadable line is asked about whole.
        let evaluator = RulesEvaluator::new();
        for line in ["make && python3 x.py", "ls docs", "python3 $(x)"] {
            assert_eq!(
                sole_unlisted_command(&evaluator, &policy(), &shell(line)),
                None,
                "{line}"
            );
        }
    }
}

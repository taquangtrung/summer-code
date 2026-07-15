//! The terminal permission prompt.
//!
//! **The prompt offers a scope, and the answer is what gets remembered.**
//! Per-access memory is unusable: a `grep` across three hundred files under a
//! policy that asks about reads would ask three hundred times, and the user would
//! answer by turning the policy off. So the offer is a rule, and answering it
//! covers the rest of that rule for the session.
//!
//! `Ask` is the escape hatch for everything the built-in default gets wrong,
//! which is why this matters more than the config surface: most users will never
//! write a policy file. They will answer prompts and let the remembered decisions
//! accumulate.

use std::io::{self, Write};
use std::sync::Arc;

use async_trait::async_trait;
use policy::{PathAccess, Request, RuleSpec, Scope};
use sandbox::{Grant, Prompter, WritePreview};
use tokio::task;

use crate::console::Console;
use crate::policy::{savable, save_rule};

// ========================================================================
// Data Structures
// ========================================================================

/// Prompts on the terminal, reading the answer from the shared [`Console`].
///
/// The console is the same one the REPL reads commands from, because at a
/// terminal there is only one stdin. Sharing it explicitly is what keeps the two
/// readers from contending over a lock, which is the deadlock this seam exists to
/// prevent.
pub struct TerminalPrompter {
    console: Arc<dyn Console>,
}

// ========================================================================
// TerminalPrompter Implementations
// ========================================================================

impl TerminalPrompter {
    pub fn new(console: Arc<dyn Console>) -> Self {
        Self { console }
    }
}

#[async_trait]
impl Prompter for TerminalPrompter {
    /// Ask, on a blocking thread.
    ///
    /// The read is moved off the runtime with `spawn_blocking` because it waits
    /// on a human, which has no upper bound. The broker calls this from inside a
    /// spawned tool task, so blocking the worker here would hold it for as long
    /// as the person takes: with N workers and N pending prompts that is a
    /// deadlock, every session waiting on someone who is waiting on the screen.
    async fn ask(&self, request: &Request) -> Grant {
        let description = describe(request);
        let rule = rule_for(request);
        let offer = rule.as_ref().map(render_rule);
        // Offered only where the policy file can hold the same rule *and* this
        // process could write it. Under kernel confinement `~/.summer` is
        // outside the root, so the write fails with `EACCES`: the same rule that
        // stops the agent authoring its own policy stops the prompt persisting
        // one. Offering a key that cannot work is worse than not offering it.
        let savable = rule.as_ref().is_some_and(savable) && !crate::confined();
        let hint = rule.as_ref().is_some_and(super::policy::savable) && crate::confined();
        let console = self.console.clone();

        let answer = task::spawn_blocking(move || {
            read_answer(console.as_ref(), &description, offer.as_deref(), savable)
        })
        .await
        .unwrap_or(Answer::Deny);

        let Some(rule) = rule else {
            // Nothing to scope an answer to, so every rule-shaped answer
            // collapses to the one-off it can actually honour.
            return Grant::once(matches!(answer, Answer::Allow));
        };
        match answer {
            Answer::Allow => Grant::once(true),
            Answer::AllowRule => {
                // The session remembers it; say how to keep it, since the key
                // that would have done so is not on offer here.
                if hint {
                    let _ = writeln!(
                        io::stderr(),
                        "summer: remembered for this session. To keep it, add it to \
                         ~/.summer/policy.json, or answer `s` in a session started with \
                         --no-sandbox."
                    );
                }
                rule_grant(true, rule)
            }
            Answer::AllowRuleSaved => {
                // Saved first, reported on failure, and the session still gets
                // the grant either way: the user answered, and losing their
                // answer because a file could not be written would be a worse
                // outcome than not persisting it.
                if let Err(error) = save_rule(&rule) {
                    let _ = writeln!(io::stderr(), "summer: could not save the rule: {error}");
                } else {
                    let _ = writeln!(
                        io::stderr(),
                        "summer: saved to your policy; future sessions will not ask."
                    );
                }
                rule_grant(true, rule)
            }
            Answer::DenyRule => rule_grant(false, rule),
            Answer::Deny => Grant::once(false),
        }
    }

    /// Print what the write would change above the question, in the same
    /// `[-removed-]` / `{+added+}` word markers the TUI colours.
    async fn ask_previewed(&self, request: &Request, preview: &WritePreview) -> Grant {
        let diff = tools::hunk_diff(&preview.before, &preview.after);
        let _ = write!(io::stderr(), "{diff}");
        self.ask(request).await
    }
}

/// A remembered answer: the rule scope is what makes it outlive this request.
fn rule_grant(allow: bool, rule: RuleSpec) -> Grant {
    Grant {
        allow,
        scope: Scope::Rule(rule),
    }
}

// ========================================================================
// Data Structures
// ========================================================================

enum Answer {
    Allow,
    AllowRule,
    /// Allow the rule, and write it into the user's policy file so later
    /// sessions inherit it.
    AllowRuleSaved,
    Deny,
    /// Refuse the rule for the rest of the session, not just this request.
    DenyRule,
}

// ========================================================================
// Functions
// ========================================================================

/// Read one answer. Anything unrecognised, and end of input, is a denial: the
/// safe direction when nobody is there to answer.
fn read_answer(
    console: &dyn Console,
    description: &str,
    offer: Option<&str>,
    savable: bool,
) -> Answer {
    let stderr = io::stderr();
    let mut stderr = stderr.lock();
    let _ = writeln!(stderr, "\nsummer: the agent wants to {description}");
    match offer {
        Some(rule) => {
            let _ = write!(stderr, "  [y] once  [a] allow `{rule}`");
            if savable {
                let _ = write!(stderr, "  [s] allow + save");
            }
            let _ = write!(stderr, "  [n] deny  [d] deny `{rule}` > ");
        }
        None => {
            let _ = write!(stderr, "  [y] once  [n] deny > ");
        }
    }
    let _ = stderr.flush();

    match console.read_line() {
        Ok(Some(line)) => answer_of(line.trim(), offer.is_some(), savable),
        // End of input is a denial, not a default yes: a prompt nobody can
        // answer must not become an approval.
        Ok(None) | Err(_) => Answer::Deny,
    }
}

/// One typed key as an answer.
///
/// Anything unrecognised denies, and a rule-scoped key with no rule to scope to
/// denies as well rather than quietly becoming its one-off cousin: a user who
/// typed `d` asked for something the prompt did not offer, and guessing what
/// they meant is how a refusal turns into an approval.
fn answer_of(key: &str, has_rule: bool, savable: bool) -> Answer {
    match key {
        "y" | "Y" => Answer::Allow,
        "a" | "A" if has_rule => Answer::AllowRule,
        "s" | "S" if has_rule && savable => Answer::AllowRuleSaved,
        "d" | "D" if has_rule => Answer::DenyRule,
        _ => Answer::Deny,
    }
}

/// What the agent is asking for, in a sentence a person can judge.
pub(crate) fn describe(request: &Request) -> String {
    match request {
        Request::Read(path) => format!("read `{}`", path.display()),
        Request::Write(path) => format!("write `{}`", path.display()),
        Request::Exec { args, program } if args.is_empty() => format!("run `{program}`"),
        Request::Exec { args, program } => format!("run `{program} {}`", args.join(" ")),
        Request::Connect { host, port } => format!("connect to `{host}:{port}`"),
        Request::Listen { port } => format!("listen on port {port}"),
    }
}

/// The rule this request would be remembered under, if it can be generalized.
///
/// A path generalizes to its parent directory, which is the granularity a person
/// can actually reason about: "yes, this agent may write under `src/`".
pub(crate) fn rule_for(request: &Request) -> Option<RuleSpec> {
    match request {
        Request::Read(path) => path_rule(path, PathAccess::Read),
        Request::Write(path) => path_rule(path, PathAccess::Write),
        // The program alone, or the whole argv for a shell or a wrapper whose
        // arguments name what really runs: the rule offered has to be the
        // thing the user reads, and "always allow `sh`" reads as nothing.
        Request::Exec { args, program } => Some(RuleSpec::for_exec(program, args)),
        Request::Connect { host, .. } => Some(RuleSpec::Connect { host: host.clone() }),
        Request::Listen { port } => Some(RuleSpec::Listen { port: *port }),
    }
}

fn path_rule(path: &std::path::Path, access: PathAccess) -> Option<RuleSpec> {
    let parent = path.parent()?;
    Some(RuleSpec::Path {
        access,
        pattern: format!("{}/**", globset::escape(&parent.display().to_string())),
    })
}

pub(crate) fn render_rule(rule: &RuleSpec) -> String {
    match rule {
        RuleSpec::Path { access, pattern } => {
            let verb = match access {
                PathAccess::Read => "read",
                PathAccess::Write => "write",
            };
            format!("{verb} {pattern}")
        }
        RuleSpec::Exec { args, program } if args.is_empty() => format!("run {program}"),
        RuleSpec::Exec { args, program } => format!("run {program} {}", args.join(" ")),
        RuleSpec::Program { program } => format!("run {program} with any arguments"),
        RuleSpec::Connect { host } => format!("connect to {host}"),
        RuleSpec::Listen { port } => format!("listen on {port}"),
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn an_unrecognized_key_denies() {
        // The direction a typo has to fail in. Every unknown key, and end of
        // input, is a refusal rather than a guess at what was meant.
        assert!(matches!(answer_of("", true, true), Answer::Deny));
        assert!(matches!(answer_of("yes please", true, true), Answer::Deny));
    }

    #[test]
    fn a_rule_key_with_no_rule_to_scope_denies() {
        // `a` and `d` are only printed when a rule exists. Typed anyway, they
        // must not decay into their one-off cousins: the user asked for
        // something the prompt did not offer, and guessing turns a refusal into
        // an approval.
        assert!(matches!(answer_of("a", false, false), Answer::Deny));
        assert!(matches!(answer_of("d", false, false), Answer::Deny));
    }

    #[test]
    fn saving_is_only_accepted_where_it_was_offered() {
        // `s` is hidden for a rule the policy file cannot express faithfully,
        // and a hidden option must not be reachable by typing it: saving an
        // exec rule would persist the program without its argv.
        assert!(matches!(answer_of("s", true, true), Answer::AllowRuleSaved));
        assert!(matches!(answer_of("s", true, false), Answer::Deny));
    }

    #[test]
    fn a_denial_can_be_remembered_too() {
        // The asymmetry this closes: an allow could be taught and a refusal
        // could not, so a request the user keeps refusing keeps arriving.
        assert!(matches!(answer_of("d", true, true), Answer::DenyRule));
        let grant = rule_grant(
            false,
            RuleSpec::Path {
                access: PathAccess::Write,
                pattern: "/repo/**".into(),
            },
        );
        assert!(!grant.allow);
        assert!(
            matches!(grant.scope, Scope::Rule(_)),
            "a once-scoped denial is forgotten"
        );
    }

    #[test]
    fn a_write_generalizes_to_its_directory() {
        // The offer has to be something a person can judge. "Allow this exact
        // path" is the offer that makes them turn the policy off.
        let request = Request::Write(PathBuf::from("/repo/src/foo.rs"));
        let Some(RuleSpec::Path { access, pattern }) = rule_for(&request) else {
            panic!("a write should generalize");
        };
        assert_eq!(access, PathAccess::Write);
        assert_eq!(pattern, "/repo/src/**");
    }

    #[test]
    fn a_description_names_what_is_being_asked() {
        assert_eq!(
            describe(&Request::Write(PathBuf::from("/repo/a.rs"))),
            "write `/repo/a.rs`"
        );
        assert_eq!(
            describe(&Request::Exec {
                args: vec!["test".into()],
                program: "cargo".into()
            }),
            "run `cargo test`"
        );
        assert_eq!(
            describe(&Request::Listen { port: 8080 }),
            "listen on port 8080"
        );
    }
}

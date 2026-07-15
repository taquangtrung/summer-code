//! Properties of the evaluator, over generated input.
//!
//! **The invariants here are easier to state than to enumerate.** "No path that
//! leaves the root is ever allowed" is one sentence and an unbounded set of
//! strings, and the interesting members of that set are the ones nobody thinks
//! to write down: `..` in the middle rather than at the front, a dotfile two
//! directories deep, a denied host spelled with a trailing dot. The example
//! tests beside the evaluator cover the cases someone thought of; these cover
//! the shape.
//!
//! Every property is about the *default* policy or a two-rule policy stated
//! inline, because the subject is the evaluator's semantics and not any
//! particular configuration. A permissive policy is allowed to allow things:
//! that is what it is for, and no property here says otherwise.

use std::path::{Path, PathBuf};

use proptest::collection::vec;
use proptest::prelude::*;
use summer_policy::{
    CommandRule, Decision, Effect, HostRule, PathAccess, PathRule, Policy, PolicyEvaluator,
    Request, RulesEvaluator, normalize,
};

// ========================================================================
// Constants
// ========================================================================

/// How many segments a generated path carries. Enough for `..` to appear in the
/// middle and still leave the root, which is the case a one-segment path cannot
/// express.
const MAX_SEGMENTS: usize = 6;

/// The ceiling on a generated string that stands in for a hostile argument.
/// Length is not what breaks a glob compiler; the characters are.
const MAX_JUNK: usize = 16;

// ========================================================================
// Functions
// ========================================================================

/// An absolute root for a test policy.
///
/// `/repo` is absolute on Unix and *not* on Windows, where
/// [`Policy::validate`] refuses it, so the platform decides the prefix and every
/// property asserted about it stays the same.
fn root() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from("C:\\repo")
    } else {
        PathBuf::from("/repo")
    }
}

fn builtin() -> Policy {
    let policy = Policy::builtin(root());
    policy
        .validate()
        .expect("the shipped default must compile on every platform");
    policy
}

fn decide(policy: &Policy, request: &Request) -> Decision {
    RulesEvaluator::new().evaluate(policy, request)
}

fn is_allowed(decision: &Decision) -> bool {
    matches!(decision, Decision::Allow)
}

fn is_denied(decision: &Decision) -> bool {
    matches!(decision, Decision::Deny { .. })
}

/// Build a path under the root from generated segments.
fn under_root(segments: &[String]) -> PathBuf {
    let mut path = root();
    for segment in segments {
        path.push(segment);
    }
    path
}

// ========================================================================
// Strategies
// ========================================================================

/// One path segment, weighted towards the ones that make an escape.
///
/// `..` carries most of the weight because it is the mechanism; the ordinary
/// names are there so it appears in the middle of a plausible path rather than
/// only at the front.
fn segment() -> impl Strategy<Value = String> {
    prop_oneof![
        4 => Just("..".to_string()),
        2 => Just(".".to_string()),
        1 => Just(".git".to_string()),
        1 => Just(".summer".to_string()),
        4 => "[a-z][a-z0-9_.-]{0,7}",
    ]
}

/// A directory the built-in policy protects from the agent's writes.
fn protected_dir() -> impl Strategy<Value = String> {
    prop_oneof![Just(".git".to_string()), Just(".summer".to_string())]
}

/// A DNS label, in the mixed case and trailing-dot forms a resolver treats as
/// one name and a rule-writer never thinks to spell out.
fn host_spelling() -> impl Strategy<Value = String> {
    ("[a-zA-Z]{1,8}", 0_usize..3).prop_map(|(label, dots)| {
        let mut host = format!("{label}.eViL.com");
        for _ in 0..dots {
            host.push('.');
        }
        host
    })
}

// ========================================================================
// Tests
// ========================================================================

proptest! {
    /// The containment property, which is the one the whole path axis rests on.
    ///
    /// It would catch a `normalize` that stopped resolving `..` in the middle of
    /// a path, and a built-in allow rule that drifted from `<root>/**` to a bare
    /// `**`: the evaluator consults rules before the root check, so that one
    /// character would hand over `/etc/passwd` and no example test asserts
    /// against a path nobody wrote down.
    #[test]
    fn a_path_that_leaves_the_root_is_never_allowed(
        segments in vec(segment(), 1..MAX_SEGMENTS),
    ) {
        let policy = builtin();
        let path = under_root(&segments);
        prop_assume!(!normalize(&path).starts_with(root()));

        for request in [Request::Read(path.clone()), Request::Write(path.clone())] {
            let decision = decide(&policy, &request);
            prop_assert!(
                !is_allowed(&decision),
                "{} normalizes to {} and was allowed: {decision:?}",
                path.display(),
                normalize(&path).display(),
            );
        }
    }

    /// A path that stays inside the root is *not* refused for leaving it.
    ///
    /// The other half of containment, and the half a too-eager normalizer
    /// breaks: `a/../b` is `b`, and an implementation that popped one component
    /// too many would deny an ordinary source file. Writes are excluded because
    /// the protected directories below are legitimately denied.
    #[test]
    fn a_read_that_stays_inside_the_root_is_allowed(
        segments in vec(segment(), 1..MAX_SEGMENTS),
    ) {
        let policy = builtin();
        let path = under_root(&segments);
        prop_assume!(normalize(&path).starts_with(root()));
        prop_assume!(normalize(&path) != root());

        let decision = decide(&policy, &Request::Read(path.clone()));
        prop_assert!(
            is_allowed(&decision),
            "{} is inside the root and was refused: {decision:?}",
            path.display(),
        );
    }

    /// The write denies that bound what the agent can author for the *next*
    /// session to read, at any depth and under any file name.
    ///
    /// Reading is deliberately still allowed: both directories are read at
    /// session start and hashed into `policy_digest`, so a deny on reads would
    /// deny that. Asserting both halves is what stops the rule being "fixed"
    /// into a blanket deny.
    #[test]
    fn a_write_into_a_protected_directory_is_denied_at_any_depth(
        dir in protected_dir(),
        nested in vec("[a-z]{1,8}", 0..3),
        name in "[a-z][a-z0-9_.]{0,8}",
    ) {
        let policy = builtin();
        let mut segments = vec![dir];
        segments.extend(nested);
        segments.push(name);
        let path = under_root(&segments);

        prop_assert!(
            is_denied(&decide(&policy, &Request::Write(path.clone()))),
            "the agent could write {}",
            path.display(),
        );
        prop_assert!(
            is_allowed(&decide(&policy, &Request::Read(path.clone()))),
            "{} must stay readable: it is hashed into the policy digest",
            path.display(),
        );
    }

    /// A dotfile at the root is denied for writes whatever it is called, and a
    /// dotfile *below* the root is not.
    ///
    /// One rule with two edges, and the second edge is why the pattern is
    /// `<root>/.*` rather than `**/.*`: `.gitignore` at the root is
    /// configuration the harness trusts, while `src/.hidden` is an ordinary
    /// file the agent has every reason to write.
    #[test]
    fn a_root_dotfile_write_is_denied_but_a_nested_one_is_not(
        name in "[a-z][a-z0-9_.-]{0,10}",
        dir in "[a-z]{1,8}",
    ) {
        let policy = builtin();
        let at_root = under_root(&[format!(".{name}")]);
        prop_assert!(
            is_denied(&decide(&policy, &Request::Write(at_root.clone()))),
            "the agent could write the root dotfile {}",
            at_root.display(),
        );

        let nested = under_root(&[dir, format!(".{name}")]);
        prop_assume!(!normalize(&nested).starts_with(root().join(".git")));
        prop_assume!(!normalize(&nested).starts_with(root().join(".summer")));
        let decision = decide(&policy, &Request::Write(nested.clone()));
        prop_assert!(
            !is_denied(&decision),
            "{} is an ordinary file and was denied: {decision:?}",
            nested.display(),
        );
    }

    /// A deny rule for a host cannot be dodged by respelling the host.
    ///
    /// This is a regression property for a fail-open bug with teeth: with
    /// `[deny "*.evil.com", allow "*"]`, `bad.EVIL.com` and `bad.evil.com.`
    /// each matched no deny and fell through to the allow. Case and the root
    /// label's trailing dot are the same name to every resolver, so they have to
    /// be the same name here.
    #[test]
    fn a_denied_host_stays_denied_however_it_is_spelled(
        host in host_spelling(),
        port in any::<u16>(),
    ) {
        let mut policy = builtin();
        policy.network.connect = vec![
            HostRule { effect: Effect::Deny, host: "*.evil.com".to_string(), port: None },
            HostRule { effect: Effect::Allow, host: "*".to_string(), port: None },
        ];
        policy.validate().expect("both patterns compile");

        let decision = decide(&policy, &Request::Connect { host: host.clone(), port });
        prop_assert!(
            is_denied(&decision),
            "`{host}` reached the allow rule: {decision:?}",
        );
    }

    /// A deny rule for a command cannot be dodged by naming a path to it.
    ///
    /// The asymmetry the evaluator documents: a deny also matches the program's
    /// file name, so denying `rm` refuses `/bin/rm` and `./rm` too. An allow
    /// does not, which the second half asserts: allowing `cargo` must never
    /// admit a `cargo` the agent dropped somewhere of its own choosing.
    #[test]
    fn a_denied_command_stays_denied_behind_any_path(
        dirs in vec("[a-z]{1,8}", 1..4),
    ) {
        let mut policy = builtin();
        policy.commands = vec![
            CommandRule { effect: Effect::Deny, program: "rm".to_string() },
            CommandRule { effect: Effect::Allow, program: "cargo".to_string() },
        ];
        policy.validate().expect("both patterns compile");

        let prefix = dirs.join("/");
        let denied = format!("{prefix}/rm");
        let decision = decide(&policy, &Request::Exec { args: Vec::new(), program: denied.clone() });
        prop_assert!(
            is_denied(&decision),
            "`{denied}` dodged the deny on `rm`: {decision:?}",
        );

        let planted = format!("{prefix}/cargo");
        let decision = decide(&policy, &Request::Exec { args: Vec::new(), program: planted.clone() });
        prop_assert!(
            !is_allowed(&decision),
            "the allow on `cargo` admitted `{planted}`: {decision:?}",
        );
    }

    /// Every request gets an answer, whatever the agent put in it and whatever
    /// the rules were written as.
    ///
    /// The evaluator compiles globs from two untrusted sources: rule patterns
    /// out of a project file, and hosts, programs and paths out of the model.
    /// Today every compile goes through `is_ok_and`, so a malformed pattern
    /// matches nothing; the bug this property guards against is a future
    /// `unwrap` on that `Result`, which turns a typo in a policy file into a
    /// panic inside the broker with a tool call in flight.
    #[test]
    fn the_evaluator_answers_every_request_it_is_handed(
        pattern in prop::string::string_regex(&format!(".{{0,{MAX_JUNK}}}")).expect("a valid regex"),
        junk in prop::string::string_regex(&format!(".{{0,{MAX_JUNK}}}")).expect("a valid regex"),
        port in any::<u16>(),
    ) {
        let mut policy = builtin();
        // Deliberately not validated: the point is what happens when a rule
        // that `validate` would have rejected reaches the evaluator anyway.
        policy.paths.insert(0, PathRule {
            access: Some(PathAccess::Write),
            effect: Effect::Deny,
            pattern: pattern.clone(),
        });
        policy.commands.insert(0, CommandRule { effect: Effect::Deny, program: pattern.clone() });
        policy.network.connect = vec![
            HostRule { effect: Effect::Deny, host: pattern.clone(), port: None },
        ];

        let requests = [
            Request::Read(PathBuf::from(&junk)),
            Request::Write(Path::new(&junk).to_path_buf()),
            Request::Exec { args: vec![junk.clone()], program: junk.clone() },
            Request::Connect { host: junk.clone(), port },
            Request::Listen { port },
        ];
        for request in &requests {
            // The assertion is that this returns at all. A decision of any kind
            // is a pass; a panic is the failure, and proptest will shrink the
            // string that caused it.
            let _ = decide(&policy, request);
        }
    }
}

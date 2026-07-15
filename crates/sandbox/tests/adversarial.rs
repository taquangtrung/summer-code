//! The adversarial suite.
//!
//! **These tests find violations; they do not establish absence.** That is the
//! honest label and it is why they are only one of the three methods the plan
//! names. The typestate is what makes the property structural: a tool cannot open
//! a file because it cannot construct a handle. The runtime monitor checks every
//! run against the journal. This suite is the third leg: it takes the specific
//! attacks the threat model names and confirms each one is refused today.
//!
//! The adversary is the model, including prompt injection arriving through repo
//! contents and tool output. The user is the principal, not the attacker.

// Building the fixture repo and the symlinks that attack it is harness-driven
// I/O: it is the test setting the scene, not a tool reaching for the filesystem.
// The attacks themselves all go through the broker, which is the point.
#![allow(clippy::disallowed_methods)]
// `mkfifo` has no safe wrapper in `libc`, and a FIFO is the trap one of these
// tests needs the broker to refuse. A test binary is its own crate root, so the
// exemption in `lib.rs` does not reach here.
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};

use policy::{
    Decision, Effect, HostRule, ListenRule, PathAccess, PathRule, Policy, PolicyFile, Request,
    RuleSpec, Scope, merge,
};
use summer_sandbox::{
    Broker, BrokerBuilder, DecisionSource, Grant, PermissionRecord, Prompter, Supplied,
};
use tempfile::TempDir;

// ========================================================================
// Data Structures
// ========================================================================

/// A prompter that says yes to everything.
///
/// Deliberately the most permissive answer available, because it makes the tests
/// sharper: anything still refused below is refused by the *policy*, not by
/// nobody being there to approve it.
struct AlwaysAllow;

/// A prompter that records what it was asked, and allows.
struct RecordingPrompter {
    asked: std::sync::Mutex<Vec<String>>,
}

// ========================================================================
// Prompter Implementations
// ========================================================================

#[async_trait::async_trait]
impl Prompter for AlwaysAllow {
    async fn ask(&self, _request: &Request) -> Grant {
        Grant::once(true)
    }
}

#[async_trait::async_trait]
impl Prompter for RecordingPrompter {
    async fn ask(&self, request: &Request) -> Grant {
        self.asked
            .lock()
            .expect("lock")
            .push(format!("{request:?}"));
        Grant::once(true)
    }
}

// ========================================================================
// Functions
// ========================================================================

/// A repo with a `src/` tree, a `.git/`, and a `.env`.
fn repo() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir(root.join("src")).expect("src");
    std::fs::write(root.join("src/main.rs"), "fn main() {}").expect("main.rs");
    std::fs::create_dir(root.join(".git")).expect(".git");
    std::fs::write(root.join(".git/config"), "[core]").expect("git config");
    std::fs::write(root.join(".env"), "SECRET=1").expect(".env");
    std::fs::create_dir(root.join(".summer")).expect(".summer");
    std::fs::write(root.join(".summer/policy.json"), "").expect("policy");
    dir
}

/// The canonical root, which is what a real session uses. macOS `/tmp` is a
/// symlink, so skipping this would make every path look like an escape.
fn root_of(dir: &TempDir) -> PathBuf {
    std::fs::canonicalize(dir.path()).expect("canonical root")
}

fn broker_with(
    policy: Policy,
    prompter: Box<dyn Prompter>,
) -> (Broker, Receiver<PermissionRecord>) {
    let (decisions, permissions) = channel();
    let broker = BrokerBuilder::new(policy)
        .prompter(prompter)
        .decisions(decisions)
        // The `none` backend reports every dimension unenforced, which is the
        // point: these tests are about the broker's own check, with nothing
        // behind it.
        .build(true)
        .expect("allow_unenforced clears the startup check");
    (broker, permissions)
}

fn default_broker(root: &Path) -> (Broker, Receiver<PermissionRecord>) {
    broker_with(Policy::builtin(root.to_path_buf()), Box::new(AlwaysAllow))
}

fn records(permissions: &Receiver<PermissionRecord>) -> Vec<PermissionRecord> {
    let mut out = Vec::new();
    while let Ok(record) = permissions.try_recv() {
        out.push(record);
    }
    out
}

// ========================================================================
// Tests
// ========================================================================

#[tokio::test]
async fn path_traversal_out_of_the_root_is_denied() {
    let dir = repo();
    let root = root_of(&dir);
    let (broker, _rx) = default_broker(&root);

    let escape = root.join("src/../../../../../../etc/passwd");
    assert!(
        broker.read(&escape).await.is_err(),
        "a `..` chain walked out of the root"
    );
}

#[tokio::test]
async fn a_relative_traversal_from_cwd_is_denied() {
    // The `cd ..` shape: the tool resolves a model-supplied relative path
    // against cwd, and the result must still be checked.
    let dir = repo();
    let root = root_of(&dir);
    let (broker, _rx) = default_broker(&root);

    let asked = summer_sandbox::resolve(&root, Path::new("../../etc/passwd"));
    assert!(
        broker.read(&asked).await.is_err(),
        "`../..` escaped the root"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_out_of_the_root_is_denied() {
    // The escape lexical normalization cannot see. The path is under the root as
    // written; only the filesystem knows it is not.
    let dir = repo();
    let root = root_of(&dir);
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(outside.path().join("secret"), "shh").expect("secret");
    std::os::unix::fs::symlink(outside.path(), root.join("escape")).expect("symlink");

    let (broker, _rx) = default_broker(&root);
    assert!(
        broker.read(&root.join("escape/secret")).await.is_err(),
        "a symlink walked out of the root"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn writing_through_a_symlink_out_of_the_root_is_denied() {
    // The dangerous direction: the target does not exist, so a naive
    // canonicalize fails and a naive fallback would return the lexical path,
    // which still looks like it is inside the root.
    let dir = repo();
    let root = root_of(&dir);
    let outside = tempfile::tempdir().expect("outside");
    std::os::unix::fs::symlink(outside.path(), root.join("escape")).expect("symlink");

    let (broker, _rx) = default_broker(&root);
    assert!(
        broker
            .write(&root.join("escape/planted.txt"))
            .await
            .is_err(),
        "a create through a symlink escaped the root"
    );
    assert!(!outside.path().join("planted.txt").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_swapped_in_after_the_check_does_not_redirect_the_write() {
    // The check happens once; the open must not happen twice. A handle carrying
    // the *path* would be re-resolved whenever the tool got round to writing,
    // and the agent has that whole window to change what the name means: `cargo`
    // is on the built-in allow list and runs arbitrary `build.rs`. Here the swap
    // is instant and unconditional, which is the same attack with the race taken
    // out of it.
    let dir = repo();
    let root = root_of(&dir);
    let outside = tempfile::tempdir().expect("outside");
    let secret = outside.path().join("secret");
    std::fs::write(&secret, "shh").expect("secret");

    let approved = root.join("src/a.rs");
    std::fs::write(&approved, "original").expect("approved file");

    let (broker, _rx) = default_broker(&root);
    let handle = broker.write(&approved).await.expect("inside the root");

    // The check is done and the path is approved. Now the name changes meaning.
    std::fs::remove_file(&approved).expect("unlink");
    std::os::unix::fs::symlink(&secret, &approved).expect("symlink");

    let _ = handle.write(b"pwned");

    assert_eq!(
        std::fs::read_to_string(&secret).expect("secret still readable"),
        "shh",
        "the write followed a symlink planted after the policy check"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_swapped_in_after_the_check_does_not_redirect_the_read() {
    // The same window, in the direction that exfiltrates rather than destroys.
    let dir = repo();
    let root = root_of(&dir);
    let outside = tempfile::tempdir().expect("outside");
    let secret = outside.path().join("secret");
    std::fs::write(&secret, "shh").expect("secret");

    let approved = root.join("src/a.rs");
    std::fs::write(&approved, "original").expect("approved file");

    let (broker, _rx) = default_broker(&root);
    let handle = broker.read(&approved).await.expect("inside the root");

    std::fs::remove_file(&approved).expect("unlink");
    std::os::unix::fs::symlink(&secret, &approved).expect("symlink");

    assert_eq!(
        handle.read_to_string().expect("read"),
        "original",
        "the read followed a symlink planted after the policy check"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_fifo_inside_the_root_is_not_a_file_the_agent_can_read() {
    // The plan argues tool reads may be blocking because a local file read has
    // bounded latency. True of a regular file, and the agent picks the path: a
    // FIFO with no writer never returns, inside a spawned task, with no await
    // point for the dispatcher's abort to land on. The timeout reports
    // `TimedOut` and the thread stays parked for the life of the process. Repeat
    // until every worker is gone.
    //
    // This test would hang rather than fail if the guard regressed, which is the
    // honest shape of the bug: a 10s cap turns that into a failure.
    let dir = repo();
    let root = root_of(&dir);
    let fifo = root.join("trap");
    let path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).expect("cstring");
    assert_eq!(
        unsafe { libc::mkfifo(path.as_ptr(), 0o644) },
        0,
        "mkfifo failed"
    );

    let (broker, _rx) = default_broker(&root);
    let opened = tokio::time::timeout(std::time::Duration::from_secs(10), broker.read(&fifo))
        .await
        .expect("opening a FIFO must not block the broker");
    assert!(
        opened.is_err(),
        "a FIFO is not a regular file and must not be readable as one"
    );
}

#[tokio::test]
async fn a_directory_is_not_a_file_the_agent_can_read() {
    let dir = repo();
    let root = root_of(&dir);
    let (broker, _rx) = default_broker(&root);
    assert!(
        broker.read(&root.join("src")).await.is_err(),
        "a directory is not a regular file"
    );
}

#[tokio::test]
async fn the_agent_cannot_rewrite_its_own_policy() {
    // The prompt-injection case: repo content tells the model to widen the
    // policy. `.summer/policy.json` is on the built-in deny list, so the prompter
    // is never even reached.
    let dir = repo();
    let root = root_of(&dir);
    let (broker, permissions) = default_broker(&root);

    let denied = broker.write(&root.join(".summer/policy.json")).await;
    assert!(denied.is_err(), "the agent rewrote its own policy");

    // Denied by the policy, not by the prompt. The distinction matters: a rule
    // the user could click through is not a rule.
    let records = records(&permissions);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].source, DecisionSource::Policy);
    assert!(matches!(records[0].decision, Decision::Deny { .. }));
}

#[tokio::test]
async fn the_agent_cannot_write_the_git_directory() {
    let dir = repo();
    let root = root_of(&dir);
    let (broker, _rx) = default_broker(&root);
    assert!(broker.write(&root.join(".git/config")).await.is_err());
}

#[tokio::test]
async fn the_agent_cannot_write_a_root_dotfile() {
    let dir = repo();
    let root = root_of(&dir);
    let (broker, _rx) = default_broker(&root);
    assert!(broker.write(&root.join(".env")).await.is_err());
}

#[tokio::test]
async fn command_arguments_cannot_smuggle_a_second_command() {
    // There is no shell between the broker and the process: `exec` takes a
    // program and an argv, and the program is what is checked and what is
    // spawned. `;` and `&&` are just argument bytes.
    let dir = repo();
    let root = root_of(&dir);
    let mut policy = Policy::builtin(root.clone());
    policy.commands = vec![policy::CommandRule {
        effect: Effect::Allow,
        program: "echo".to_string(),
    }];
    let (broker, permissions) = broker_with(policy, Box::new(AlwaysAllow));

    let command = broker
        .exec(
            "echo",
            &["hi; rm -rf /".to_string(), "&& whoami".to_string()],
        )
        .await;
    assert!(command.is_ok(), "the allowed program should run");

    // The recorded access names `echo` and nothing else: the injected text never
    // became a program.
    let records = records(&permissions);
    assert_eq!(records.len(), 1);
    let Request::Exec { args, program } = &records[0].request else {
        panic!("expected an exec request");
    };
    assert_eq!(program, "echo");
    assert_eq!(args.len(), 2, "args stayed args");
}

#[tokio::test]
async fn a_disallowed_program_is_denied_even_when_named_in_an_allowed_ones_args() {
    let dir = repo();
    let root = root_of(&dir);
    let mut policy = Policy::builtin(root.clone());
    policy.commands = vec![policy::CommandRule {
        effect: Effect::Deny,
        program: "rm".to_string(),
    }];
    let (broker, _rx) = broker_with(policy, Box::new(AlwaysAllow));

    assert!(
        broker
            .exec("rm", &["-rf".to_string(), "/".to_string()])
            .await
            .is_err(),
        "a denied program ran"
    );
}

#[tokio::test]
async fn a_hostile_project_file_cannot_grant_itself_exec() {
    // A file that arrived with a clone is not a user speaking. Its allow becomes
    // a prompt: the repo proposes, the user grants.
    let dir = repo();
    let root = root_of(&dir);

    let project: PolicyFile =
        serde_json::from_str(r#"{"commands":[{"program":"curl","effect":"allow"}]}"#)
            .expect("parses");
    let project = project
        .into_policy(root.clone())
        .expect("valid project policy");
    let policy = merge(
        root.clone(),
        vec![Policy::builtin(root.clone())],
        Some(&project),
    );

    let prompter = Box::new(RecordingPrompter {
        asked: std::sync::Mutex::new(Vec::new()),
    });
    let (broker, permissions) = broker_with(policy, prompter);

    // It runs only because the prompter said yes. The point is that it *asked*.
    let _ = broker.exec("curl", &["http://evil".to_string()]).await;

    let records = records(&permissions);
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].source,
        DecisionSource::Prompt,
        "a project-file allow must surface as a prompt, not a grant"
    );
}

#[tokio::test]
async fn a_hostile_project_file_cannot_reach_past_a_trusted_deny() {
    // Where the user said no, a project file proposing yes does not even prompt.
    // Re-asking a settled question until someone clicks wrong is itself an
    // attack.
    let dir = repo();
    let root = root_of(&dir);

    let mut user = Policy::builtin(root.clone());
    user.commands = vec![policy::CommandRule {
        effect: Effect::Deny,
        program: "curl".to_string(),
    }];

    let project: PolicyFile =
        serde_json::from_str(r#"{"commands":[{"program":"curl","effect":"allow"}]}"#)
            .expect("parses");
    let project = project
        .into_policy(root.clone())
        .expect("valid project policy");
    let policy = merge(root.clone(), vec![user], Some(&project));

    let prompter = Box::new(RecordingPrompter {
        asked: std::sync::Mutex::new(Vec::new()),
    });
    let (broker, permissions) = broker_with(policy, prompter);

    assert!(
        broker.exec("curl", &[]).await.is_err(),
        "a project file reached past a trusted deny"
    );
    let records = records(&permissions);
    assert_eq!(
        records[0].source,
        DecisionSource::Policy,
        "it must not even prompt"
    );
}

#[tokio::test]
async fn a_tool_cannot_bind_the_wildcard_under_a_loopback_policy() {
    // The tool never calls `bind`. If it held a permit and bound the socket
    // itself, nothing would stop it choosing 0.0.0.0, and the Loopback/Any
    // distinction would be decoration.
    let dir = repo();
    let root = root_of(&dir);
    let mut policy = Policy::builtin(root.clone());
    policy.network.listen = ListenRule::Loopback { ports: vec![] };
    let (broker, permissions) = broker_with(policy, Box::new(AlwaysAllow));

    // Port 0 lets the OS choose, so the test does not depend on a free port.
    let listener = broker.listen(0).await.expect("loopback listen is allowed");
    assert!(
        listener.bound().ip().is_loopback(),
        "bound {} instead of loopback",
        listener.bound()
    );

    // And the audit trail says which one it was, not just the port.
    let records = records(&permissions);
    let Request::Listen { .. } = &records[0].request else {
        panic!("expected a listen request");
    };
    let Supplied::Bound(bound) = records[0].supplied else {
        panic!(
            "expected the bound address on the record: {:?}",
            records[0].supplied
        );
    };
    assert!(
        bound.is_loopback(),
        "the journal must record the address actually bound"
    );
}

#[tokio::test]
async fn listen_is_denied_under_a_deny_policy() {
    let dir = repo();
    let root = root_of(&dir);
    let mut policy = Policy::builtin(root.clone());
    policy.network.listen = ListenRule::Deny;
    let (broker, _rx) = broker_with(policy, Box::new(AlwaysAllow));

    assert!(
        broker.listen(0).await.is_err(),
        "listen was denied and bound anyway"
    );
}

#[tokio::test]
async fn outbound_network_is_off_by_default() {
    let dir = repo();
    let root = root_of(&dir);
    let (broker, _rx) = default_broker(&root);
    assert!(
        broker.connect("example.com", 443).await.is_err(),
        "the default policy reached the network"
    );
}

#[tokio::test]
async fn a_host_the_policy_denies_is_not_resolved_either() {
    // `lookup` is the half of `connect` that `web_fetch` uses, so a policy check
    // that only guarded the connect would leave the resolution ungoverned: the
    // name the user denied would still be put to the resolver, and the tool
    // would hold an address for it.
    let dir = repo();
    let root = root_of(&dir);
    let (broker, _rx) = default_broker(&root);
    assert!(
        broker.lookup("example.com", 443).await.is_err(),
        "a denied host was resolved anyway"
    );
}

#[tokio::test]
async fn a_resolved_host_carries_the_port_that_was_authorized() {
    // The addresses go to an HTTP client that connects to them directly, so a
    // lookup that dropped the port would hand it `127.0.0.1:0`: every fetch of
    // an allowed host fails, and it fails inside the client rather than here.
    let dir = repo();
    let root = root_of(&dir);
    let mut policy = Policy::builtin(root.clone());
    policy.network.connect.push(HostRule {
        effect: Effect::Allow,
        host: "localhost".to_string(),
        port: None,
    });
    let (broker, _rx) = broker_with(policy, Box::new(AlwaysAllow));

    let authorized = broker.lookup("localhost", 8080).await.expect("allowed");
    assert_eq!(authorized.host(), "localhost");
    assert_eq!(authorized.port(), 8080);
    assert!(
        !authorized.addrs().is_empty(),
        "an authorized host resolved to nothing"
    );
    for addr in authorized.addrs() {
        assert_eq!(addr.port(), 8080, "{addr} lost the authorized port");
        assert!(addr.ip().is_loopback(), "{addr} is not localhost");
    }
}

#[tokio::test]
async fn every_decision_is_recorded_including_the_ones_nobody_was_asked_about() {
    // The property behind "no write without a matching grant in the journal": the
    // broker records on every evaluation, not only when it prompts. If the prompt
    // emitted the record, a remembered auto-allow would execute unaudited.
    let dir = repo();
    let root = root_of(&dir);
    let (broker, permissions) = default_broker(&root);

    // An outright allow (a read under root), which never reaches a prompt.
    broker
        .read(&root.join("src/main.rs"))
        .await
        .expect("allowed");
    // An outright deny.
    let _ = broker.read(&root.join("../../etc/passwd")).await;

    let records = records(&permissions);
    assert_eq!(records.len(), 2, "both decisions must be recorded");
    assert!(records.iter().all(|r| r.source == DecisionSource::Policy));
}

#[tokio::test]
async fn a_rule_scoped_approval_answers_once_and_covers_its_scope() {
    // Per-access memory would ask three hundred times for one grep, and the user
    // would answer by turning the policy off.
    let dir = repo();
    let root = root_of(&dir);

    struct CountingPrompter {
        calls: std::sync::Mutex<u32>,
        pattern: String,
    }

    #[async_trait::async_trait]
    impl Prompter for CountingPrompter {
        async fn ask(&self, _request: &Request) -> Grant {
            *self.calls.lock().expect("lock") += 1;
            Grant {
                allow: true,
                scope: Scope::Rule(RuleSpec::Path {
                    access: PathAccess::Write,
                    pattern: self.pattern.clone(),
                }),
            }
        }
    }

    let mut policy = Policy::builtin(root.clone());
    // Strip the blanket allow so writes reach the Ask default.
    policy.paths.clear();

    let prompter = std::sync::Arc::new(CountingPrompter {
        calls: std::sync::Mutex::new(0),
        pattern: format!("{}/src/**", globset::escape(&root.display().to_string())),
    });

    struct Shared(std::sync::Arc<CountingPrompter>);
    #[async_trait::async_trait]
    impl Prompter for Shared {
        async fn ask(&self, request: &Request) -> Grant {
            self.0.ask(request).await
        }
    }

    let (broker, permissions) = broker_with(policy, Box::new(Shared(prompter.clone())));

    broker.write(&root.join("src/a.rs")).await.expect("allowed");
    broker
        .write(&root.join("src/b.rs"))
        .await
        .expect("covered by the rule");
    broker
        .write(&root.join("src/c.rs"))
        .await
        .expect("covered by the rule");

    assert_eq!(
        *prompter.calls.lock().expect("lock"),
        1,
        "the user should have been asked exactly once"
    );

    // But the audit trail stays per access: three writes, three records. That is
    // the whole reason `Scope` is recorded alongside `Access`.
    let records = records(&permissions);
    assert_eq!(records.len(), 3, "the audit trail is per access");
    assert_eq!(records[0].source, DecisionSource::Prompt);
    assert_eq!(records[1].source, DecisionSource::Remembered);
    assert_eq!(records[2].source, DecisionSource::Remembered);

    // **Every one of them carries the rule, not just the first.** The fold only
    // rebuilds a grant from a `Scope::Rule`, so if the remembered decisions
    // recorded `Scope::Once` the rule would live in exactly one record. Compact
    // that turn away and resume re-asks what the user already answered, which is
    // the failure `Scope`-on-the-record exists to prevent, one level down.
    for (index, record) in records.iter().enumerate() {
        assert!(
            matches!(&record.scope, Scope::Rule(_)),
            "record {index} recorded {:?}, losing the rule the user agreed to",
            record.scope
        );
    }
}

#[tokio::test]
async fn a_policy_allowed_read_outside_the_root_still_works() {
    // **The case `openat2` must not break.** In-root opens go through
    // `RESOLVE_BENEATH` against a held root descriptor, which refuses anything
    // that leaves the root -- and a user policy is entitled to allow a specific
    // path outside it (a shared toolchain file, a config the build reads). The
    // evaluator has already said yes by the time the open happens, so routing
    // *everything* through the beneath-check would silently revoke a grant the
    // user wrote explicitly.
    //
    // The two paths therefore differ by where the file is, not by how much is
    // trusted: containment is enforced by the kernel where containment is the
    // rule, and an explicit out-of-root allow falls back to the plain open.
    let dir = repo();
    let root = root_of(&dir);
    let outside_dir = tempfile::tempdir().expect("outside");
    let outside = std::fs::canonicalize(outside_dir.path())
        .expect("canonical")
        .join("shared.toml");
    std::fs::write(&outside, "allowed by the user").expect("write");

    let mut policy = Policy::builtin(root);
    // Highest precedence, ahead of the built-in root-anchored rules.
    policy.paths.insert(
        0,
        PathRule {
            access: Some(PathAccess::Read),
            effect: Effect::Allow,
            pattern: globset::escape(&outside.display().to_string()),
        },
    );
    let (broker, _permissions) = broker_with(policy, Box::new(AlwaysAllow));

    let handle = broker
        .read(&outside)
        .await
        .expect("the policy allows this path explicitly");
    assert_eq!(
        handle.read_to_string().expect("reads"),
        "allowed by the user"
    );
}

#[tokio::test]
async fn an_in_root_read_still_works_through_the_beneath_open() {
    // The other half: the ordinary case must not regress. Everything the agent
    // does is an in-root open, so a mistake here is not a corner case.
    let dir = repo();
    let root = root_of(&dir);
    let (broker, _permissions) = default_broker(&root);

    let handle = broker
        .read(&root.join("src/main.rs"))
        .await
        .expect("an in-root read is allowed");
    assert_eq!(handle.read_to_string().expect("reads"), "fn main() {}");
}

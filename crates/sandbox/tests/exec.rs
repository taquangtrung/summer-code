//! The exec seam, end to end against real child processes.
//!
//! **Every assertion here covers a bug that shipped because nothing asserted
//! it.** The broker spawned without configuring stdio, so the inherit default
//! applied: a child's output went to the operator's terminal and the model was
//! handed an empty string, and the agent loop ran blind against every command it
//! issued. Nothing caught it, because no test had ever looked at what `exec`
//! actually returns. The same silence covered the inherited environment (which
//! carried `ANTHROPIC_API_KEY` into every approved `sh`) and the missing exit
//! status.
//!
//! These are real processes rather than a mock. The failures being pinned are
//! properties of `Command` configuration, and a mock of `Command` would have
//! reproduced whatever the implementation believed rather than what the OS does.
//!
//! **The scripts run on every platform**, which is why they are constants rather
//! than inline strings. `cmd` and `sh` disagree about almost everything, and the
//! properties under test -- output is captured, the environment is scrubbed, the
//! cwd is the root -- are platform-independent even though nothing that
//! demonstrates them is. Where a program serves on both (`sort`), it is used on
//! both; where none does, the pair is named here and nowhere else.

// Staging fixtures is harness-driven I/O: the test setting the scene, not a tool
// reaching for the filesystem. Every exec below goes through the broker.
#![allow(clippy::disallowed_methods)]
// Setting `ANTHROPIC_API_KEY` in this process is how the test proves the child
// does not inherit it, and edition 2024 made `set_var` unsafe. A test binary is
// its own crate root, so the exemption in `lib.rs` does not reach here.
#![allow(unsafe_code)]

use std::path::PathBuf;
use std::sync::mpsc::channel;

use policy::{CommandRule, Effect, Policy, Request};
use summer_sandbox::{
    Broker, BrokerBuilder, CommandOutcome, CommandOutput, Grant, Prompter, canonicalize,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

// ========================================================================
// Constants
// ========================================================================

/// The shell these tests drive, and the flag that introduces a command line.
#[cfg(windows)]
const SHELL: (&str, &str) = ("cmd", "/C");
#[cfg(not(windows))]
const SHELL: (&str, &str) = ("sh", "-c");

/// Print one word to stdout.
const ECHO: &str = "echo hello";

/// Print one word to stderr and nothing to stdout.
#[cfg(windows)]
const ECHO_STDERR: &str = "echo oops 1>&2";
#[cfg(not(windows))]
const ECHO_STDERR: &str = "echo oops >&2";

/// Dump the environment, one `NAME=value` per line.
#[cfg(windows)]
const DUMP_ENV: &str = "set";
#[cfg(not(windows))]
const DUMP_ENV: &str = "env";

/// Print the working directory and nothing else.
#[cfg(windows)]
const PRINT_CWD: &str = "cd";
#[cfg(not(windows))]
const PRINT_CWD: &str = "pwd";

/// Sleep for longer than any test is willing to wait.
///
/// `ping` rather than `timeout` on Windows: `timeout` needs a console and fails
/// outright when stdin is redirected, which it always is here.
#[cfg(windows)]
const SLEEP_LONG: &str = "ping -n 31 127.0.0.1 >nul";
#[cfg(not(windows))]
const SLEEP_LONG: &str = "sleep 30";

/// Write far more than a pipe buffer holds.
#[cfg(windows)]
const FLOOD: &str = "for /L %i in (1,1,40000) do @echo a line of predictable length";
#[cfg(not(windows))]
const FLOOD: &str =
    "i=0; while [ $i -lt 40000 ]; do echo 'a line of predictable length'; i=$((i+1)); done";

/// One line of the flood, for the head/tail assertions.
const FLOOD_LINE: &str = "a line of predictable length";

/// Read stdin and write it back. `sort` ships with Windows and every Unix, and
/// on an empty input it prints nothing and exits zero.
const ECHO_STDIN: &str = "sort";

// ========================================================================
// Data Structures
// ========================================================================

/// Says yes to everything, so anything refused below is refused by the policy.
struct AlwaysAllow;

// ========================================================================
// Prompter Implementations
// ========================================================================

#[async_trait::async_trait]
impl Prompter for AlwaysAllow {
    async fn ask(&self, _request: &Request) -> Grant {
        Grant::once(true)
    }
}

// ========================================================================
// Functions
// ========================================================================

fn repo() -> TempDir {
    tempfile::tempdir().expect("tempdir")
}

/// The canonical root, resolved exactly as a session resolves it.
///
/// `summer_sandbox::canonicalize` rather than `fs::canonicalize`, because on
/// Windows the latter returns a `\\?\`-prefixed verbatim path and the child's
/// own idea of its working directory has no such prefix. Comparing the two
/// would fail on a difference the sandbox deliberately removes.
fn root_of(dir: &TempDir) -> PathBuf {
    canonicalize(dir.path()).expect("canonical root")
}

/// A broker rooted at `root` allowing the shell, with nothing behind it.
fn broker(root: PathBuf) -> Broker {
    let mut policy = Policy::builtin(root);
    policy.commands = vec![CommandRule {
        effect: Effect::Allow,
        program: SHELL.0.to_string(),
    }];
    let (decisions, _permissions) = channel();
    BrokerBuilder::new(policy)
        .prompter(Box::new(AlwaysAllow))
        .decisions(decisions)
        .build(true)
        .expect("allow_unenforced clears the startup check")
}

/// Run `script` through the platform shell and unwrap the exit.
async fn shell(broker: &Broker, script: &str) -> CommandOutput {
    let outcome = spawn(broker, script)
        .await
        .run(&CancellationToken::new())
        .await
        .expect("the child ran");
    let CommandOutcome::Exited(output) = outcome else {
        panic!("nothing cancelled this");
    };
    output
}

async fn spawn(broker: &Broker, script: &str) -> summer_sandbox::AuthorizedCommand {
    broker
        .exec(SHELL.0, &[SHELL.1.to_string(), script.to_string()])
        .await
        .expect("the policy allows the shell")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

// ========================================================================
// Tests
// ========================================================================

#[tokio::test]
async fn stdout_reaches_the_caller_instead_of_the_terminal() {
    // The bug this file exists for. Without `Stdio::piped()` the child inherits
    // the harness's stdout, so this text lands on the operator's screen and the
    // model is told the command produced nothing.
    let dir = repo();
    let output = shell(&broker(root_of(&dir)), ECHO).await;

    assert_eq!(text(&output.stdout.bytes).trim(), "hello");
    assert!(output.success());
}

#[tokio::test]
async fn stderr_is_captured_too() {
    // A failing build says why on stderr and nothing on stdout. Dropping this
    // stream leaves the model an empty result for the calls it most needs to
    // read.
    let dir = repo();
    let output = shell(&broker(root_of(&dir)), ECHO_STDERR).await;

    assert_eq!(text(&output.stderr.bytes).trim(), "oops");
}

#[tokio::test]
async fn a_non_zero_exit_is_reported() {
    // `Outcome::Ok` was returned for any process that merely ran, so a `git
    // commit` with nothing staged read to the model as a success.
    let dir = repo();
    let output = shell(&broker(root_of(&dir)), "exit 3").await;

    assert_eq!(output.code, Some(3));
    assert!(!output.success());
}

#[tokio::test]
async fn the_api_key_does_not_reach_a_child() {
    // The exfiltration path: the shell is one approved command away from copying
    // the credential into a repo file the agent then reads back. `env_clear`
    // plus an allowlist is what closes it, and this asserts the allowlist has
    // not quietly grown a hole.
    // SAFETY: single-threaded setup before any child is spawned, and the value
    // is removed again below.
    unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-should-not-escape") };
    let dir = repo();
    let output = shell(&broker(root_of(&dir)), DUMP_ENV).await;
    unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };

    let environment = text(&output.stdout.bytes);
    assert!(
        !environment.contains("sk-should-not-escape"),
        "the credential crossed into the child:\n{environment}"
    );
    // Case-insensitively: Windows reports the variable as `Path`, Unix as
    // `PATH`, and the point is that the child can resolve a program at all.
    assert!(
        environment.to_ascii_uppercase().contains("PATH="),
        "the allowlist must still pass PATH, or nothing resolves:\n{environment}"
    );
}

#[tokio::test]
async fn the_child_starts_in_the_policy_root() {
    // An inherited working directory is whatever the process last had, which
    // need not be the root every containment check is written against.
    let dir = repo();
    let root = root_of(&dir);
    let output = shell(&broker(root.clone()), PRINT_CWD).await;

    assert_eq!(
        text(&output.stdout.bytes).trim(),
        root.to_string_lossy().trim_end_matches(['/', '\\'])
    );
}

#[tokio::test]
async fn stdin_is_null_so_an_interactive_child_does_not_hang() {
    // With stdin inherited, a child that reads blocks until the dispatcher's
    // 120s timeout *and* eats the keystrokes meant for the permission prompt.
    // Reading from null gives EOF at once.
    let dir = repo();
    let output = shell(&broker(root_of(&dir)), ECHO_STDIN).await;

    assert!(
        output.success(),
        "the read returned EOF rather than blocking"
    );
    assert!(text(&output.stdout.bytes).trim().is_empty());
}

#[tokio::test]
async fn output_past_the_cap_is_elided_rather_than_deadlocking() {
    // Two failures at once. A child writing more than a pipe buffer deadlocks if
    // the parent waits for exit before reading, and an uncapped capture puts a
    // whole build log into the context window. Draining past the cap and
    // discarding the middle is what avoids both.
    let dir = repo();
    let output = shell(&broker(root_of(&dir)), FLOOD).await;

    assert!(output.success());
    assert!(output.stdout.elided > 0, "a megabyte of output was capped");
    let captured = text(&output.stdout.bytes);
    assert!(captured.contains("bytes elided"), "the cut is named");
    assert!(captured.starts_with(FLOOD_LINE), "the head is kept");
    assert!(
        captured.trim_end().ends_with(FLOOD_LINE),
        "the tail is kept"
    );
}

#[tokio::test]
async fn cancellation_stops_a_long_command() {
    // "Ctrl-C during dispatch is immediate" is documented behaviour; `git` did
    // not have it, and this pins the seam that now serves every exec tool.
    let dir = repo();
    let broker = broker(root_of(&dir));
    let cancel = CancellationToken::new();

    let handle = spawn(&broker, SLEEP_LONG).await;
    let token = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        token.cancel();
    });

    let started = std::time::Instant::now();
    let outcome = handle.run(&cancel).await.expect("ran");

    assert!(matches!(outcome, CommandOutcome::Interrupted));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "the wait was not interrupted"
    );
}

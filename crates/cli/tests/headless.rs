//! The headless contract, exercised against the real binary.
//!
//! **These are the promises a script depends on and no unit test can make.**
//! `-p` with `--output-format json|stream-json` and an exit code per stop reason
//! is an interface for programs, not for people: a wrapper reads the status,
//! parses stdout, and never sees the prose. Everything that makes that work is
//! outside the reach of a unit test, because it lives in the shape of the
//! process: which stream a line goes to, what the status is, and what has been
//! written to disk by the time either happens.
//!
//! **No model is reached, deliberately.** Every run here is isolated from the
//! developer's own credentials and home directory, so the turn fails locally
//! with "no credential" rather than making a request. That is not a limitation
//! of the tests: the no-credential path *is* the documented behaviour ("the
//! session still starts, and the turn the user attempts is what tells them what
//! to set"), and it reaches the same reporting code a real stop reason does.

// Spawning the binary under test and staging its home directory is harness
// I/O: the test setting the scene, not a tool reaching for the filesystem. A
// test binary is its own crate root, so `main.rs`'s exemption does not reach
// here.
#![allow(clippy::disallowed_methods)]

use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use tempfile::TempDir;

// ========================================================================
// Constants
// ========================================================================

/// Credentials removed from the child's environment, so a developer who has one
/// exported does not get a different test run from CI, or a live request.
const CREDENTIAL_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
];

/// Where the session looks for `~/.summer`. Windows has no `HOME`, so both are
/// set on every platform rather than one per `cfg`.
const HOME_VARS: &[&str] = &["HOME", "USERPROFILE"];

/// What a usage error costs, which is nothing: arguments are parsed before the
/// journal is opened, so a mistyped flag must not leave a directory behind.
const SUMMER_DIR: &str = ".summer";

// ========================================================================
// Functions
// ========================================================================

/// A `summer` invocation isolated from the machine it runs on.
///
/// The environment is *trimmed*, never cleared. A cleared environment does not
/// produce a hardened child on Windows, it produces one that cannot start:
/// `SystemRoot` is what loads the socket library, and `PATHEXT` is what makes a
/// program name resolve at all.
fn summer(home: &TempDir, work: &TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_summer"));
    command.current_dir(work.path());
    for var in CREDENTIAL_VARS {
        command.env_remove(var);
    }
    for var in HOME_VARS {
        command.env(var, home.path());
    }
    // Null, because a `-p` with no argument reads the prompt from stdin: an
    // inherited stdin would make that test wait on the terminal running it.
    command.stdin(Stdio::null());
    command
}

fn scratch() -> (TempDir, TempDir) {
    (
        tempfile::tempdir().expect("a temp home"),
        tempfile::tempdir().expect("a temp workspace"),
    )
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// Every line of stdout, parsed as JSON. Newline-delimited JSON is the format's
/// whole contract, so a line that does not parse is a failure and not something
/// to filter out.
fn json_lines(output: &Output) -> Vec<Value> {
    stdout_of(output)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("stdout line is not JSON: {error}: {line}"))
        })
        .collect()
}

/// The one `result` object a run ends with, found by its `kind` rather than by
/// its position, which is what a consumer is told to do.
fn result_object(output: &Output) -> Value {
    let mut results: Vec<Value> = json_lines(output)
        .into_iter()
        .filter(|line| line["kind"] == "result")
        .collect();
    assert_eq!(
        results.len(),
        1,
        "a run must end with exactly one result object: {}",
        stdout_of(output)
    );
    results.remove(0)
}

fn journal_exists(home: &Path) -> bool {
    let projects = home.join(SUMMER_DIR).join("projects");
    let Ok(entries) = std::fs::read_dir(&projects) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        std::fs::read_dir(entry.path().join("sessions"))
            .is_ok_and(|mut sessions| sessions.any(|session| session.is_ok()))
    })
}

// ========================================================================
// Tests
// ========================================================================

#[test]
fn the_version_flag_answers_with_this_crate_version() {
    // Asserted against `CARGO_PKG_VERSION` rather than a literal, so a release
    // bump cannot leave the binary reporting the old number.
    let (home, work) = scratch();
    let output = summer(&home, &work)
        .arg("--version")
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(
        stdout_of(&output).trim(),
        format!("summer {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn the_help_flag_answers_and_names_the_headless_flags() {
    // `--allow-unenforced` was documented in six source files and never parsed.
    // A usage text that names a flag nobody implemented is the same bug from the
    // other end, so the flags this suite depends on are checked to be in it.
    let (home, work) = scratch();
    let output = summer(&home, &work)
        .arg("--help")
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{}", stderr_of(&output));
    let usage = stdout_of(&output);
    for flag in ["--print", "--output-format"] {
        assert!(
            usage.contains(flag),
            "`{flag}` is missing from --help:\n{usage}"
        );
    }
}

#[test]
fn an_unknown_output_format_exits_two_and_says_what_is_valid() {
    // Exit 2 is `Usage`, distinct from 1: a wrapper that cannot tell "you called
    // me wrong" from "the turn failed" retries the first one forever.
    let (home, work) = scratch();
    let output = summer(&home, &work)
        .args(["--output-format", "yaml", "-p", "hi"])
        .output()
        .expect("the binary runs");

    assert_eq!(output.status.code(), Some(2), "{}", stderr_of(&output));
    let message = stderr_of(&output);
    for format in ["text", "json", "stream-json"] {
        assert!(
            message.contains(format),
            "the error must name the formats that work: {message}"
        );
    }
    assert!(
        stdout_of(&output).is_empty(),
        "a usage error belongs on stderr: {}",
        stdout_of(&output)
    );
}

#[test]
fn a_usage_error_opens_no_journal() {
    // The ordering `main` documents: arguments are parsed before anything reads
    // the environment, opens a journal, or enters a sandbox, so a mistyped flag
    // costs nothing. Nothing else asserts it, and the cost of losing it is a
    // session directory per typo.
    let (home, work) = scratch();
    let output = summer(&home, &work)
        .args(["--output-format", "yaml", "-p", "hi"])
        .output()
        .expect("the binary runs");

    assert_eq!(output.status.code(), Some(2));
    assert!(
        !home.path().join(SUMMER_DIR).exists(),
        "a usage error created {}",
        home.path().join(SUMMER_DIR).display()
    );
    assert!(
        std::fs::read_dir(work.path())
            .expect("the workspace is readable")
            .next()
            .is_none(),
        "a usage error wrote into the workspace"
    );
}

#[test]
fn a_headless_run_with_no_credential_reports_a_failed_result_object() {
    // The wire format, field by field. `print.rs` calls these names the wire
    // format for anything parsing this, which makes renaming one a breaking
    // change that no test would otherwise notice.
    let (home, work) = scratch();
    let output = summer(&home, &work)
        .args(["-p", "say hi", "--output-format", "json"])
        .output()
        .expect("the binary runs");

    assert_eq!(
        output.status.code(),
        Some(1),
        "a failed turn is exit 1: {}",
        stderr_of(&output)
    );
    let result = result_object(&output);
    assert_eq!(result["is_error"], true);
    assert_eq!(result["stop"], "failed");
    assert_eq!(result["tool_calls"], 0);
    assert_eq!(result["total_tokens"], 0);
    assert!(
        result["result"]
            .as_str()
            .is_some_and(|text| !text.is_empty()),
        "the result text is what a wrapper shows a human: {result}"
    );
    assert!(
        result["session_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "a run without a session id cannot be found again: {result}"
    );

    // The other half of the usage-error test: a real run *does* write a journal,
    // under the home this test staged rather than the developer's own.
    assert!(
        journal_exists(home.path()),
        "the run recorded no journal under {}",
        home.path().display()
    );
}

#[test]
fn stream_json_is_newline_delimited_json_ending_in_one_result() {
    // A consumer is told to pick the result out by `kind` rather than by taking
    // the last line, so the property worth pinning is that the kind is unique
    // and that every other line parses too.
    let (home, work) = scratch();
    let output = summer(&home, &work)
        .args(["-p", "say hi", "--output-format", "stream-json"])
        .output()
        .expect("the binary runs");

    let lines = json_lines(&output);
    assert!(
        lines.len() > 1,
        "stream-json must stream events, not just the result: {}",
        stdout_of(&output)
    );
    assert!(
        lines.iter().all(|line| line["kind"].is_string()),
        "every event needs a kind to be routed on: {}",
        stdout_of(&output)
    );
    assert_eq!(result_object(&output)["stop"], "failed");
}

#[test]
fn the_text_format_keeps_failure_prose_off_stdout() {
    // Text is the format a person reads, and it is still piped: a wrapper doing
    // `summer -p ... > out.txt` must not find an error message in the file it
    // expects the model's answer in.
    let (home, work) = scratch();
    let output = summer(&home, &work)
        .args(["-p", "say hi"])
        .output()
        .expect("the binary runs");

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stdout_of(&output).trim().is_empty(),
        "a failed text run wrote to stdout: {}",
        stdout_of(&output)
    );
    assert!(
        stderr_of(&output).contains("credential"),
        "the reason has to reach the user somewhere: {}",
        stderr_of(&output)
    );
}

//! Landlock `enter` actually restricts the kernel, not just the broker.
//!
//! **This runs in a subprocess, and it has to.** A Landlock ruleset is
//! irreversible: a thread cannot un-restrict itself. Calling `enter` in the test
//! process would confine the test harness for the rest of the run and fail every
//! test that came after it, including ones that have nothing to do with the
//! sandbox. So the child restricts itself, tries one thing, and reports through
//! its exit code.
//!
//! This is the check the runtime monitor structurally cannot make. The monitor
//! reads the journal, and the journal faithfully records that the broker decided
//! correctly; whether the *kernel* then backed that decision is invisible to it.

#![cfg(target_os = "linux")]
// The fixture and the subprocess plumbing are harness-driven, not agent-driven.
#![allow(clippy::disallowed_methods)]

use std::path::{Path, PathBuf};
use std::process::Command;

use policy::{Effect, Policy};
use summer_sandbox::{Enforcement, Harness, LandlockBackend, SandboxBackend};

// ========================================================================
// Constants
// ========================================================================

/// The child read the file it was supposed to be able to read.
const EXIT_INSIDE_OK: i32 = 10;
/// The child read a file outside the root. The sandbox did not hold.
const EXIT_ESCAPED: i32 = 11;
/// The child was refused outside the root, which is the point.
const EXIT_CONFINED: i32 = 12;
/// `enter` itself failed.
const EXIT_ENTER_FAILED: i32 = 13;

const CHILD_ENV: &str = "SUMMER_LANDLOCK_CHILD";

/// The path outside the root that the child tries to read.
///
/// **Passed in rather than hardcoded to a system file.** `/etc/hostname` used to
/// serve here, and it stopped being a valid probe once the ruleset started
/// granting `/etc`: the harness needs the resolver configuration and the CA
/// bundle to reach the model at all, so `/etc` is deliberately readable and a
/// test asserting otherwise would only be asserting that the sandbox is broken.
/// A file in a sibling temporary directory is outside every grant, which is the
/// property actually under test.
const OUTSIDE_ENV: &str = "SUMMER_LANDLOCK_OUTSIDE";

/// Set to skip `enter`, so the same child proves the read it is later refused
/// would otherwise have succeeded.
const CONTROL_ENV: &str = "SUMMER_LANDLOCK_CONTROL";

/// Set to make the child run a program instead of reading a file.
const EXEC_ENV: &str = "SUMMER_LANDLOCK_EXEC";

/// Set to make the child do its read from a tokio worker thread.
const WORKER_ENV: &str = "SUMMER_LANDLOCK_WORKER";

/// Set to make the child build the fixture crate.
const BUILD_ENV: &str = "SUMMER_LANDLOCK_BUILD";

// ========================================================================
// Functions
// ========================================================================

fn paths_are_enforced_here() -> bool {
    matches!(
        LandlockBackend::new()
            .capabilities(&Policy::builtin(std::path::PathBuf::from("/")))
            .paths,
        Enforcement::Enforced
    )
}

/// The child half: restrict, then try to read outside the root.
///
/// Lives inside the test binary and is selected by an env var, so there is no
/// second crate to build and keep in step.
fn run_child(root: &str, outside: &str) -> ! {
    let policy = Policy::builtin(root.into());
    let backend = LandlockBackend::new();

    // The worker mode reproduces the harness's own shape: build a multi-thread
    // runtime and do the read from a spawned task, which is where
    // `Dispatcher::dispatch` runs every tool. Whether that read is confined
    // depends entirely on which side of `enter` the runtime was built, so the
    // `CONTROL_ENV` flag moves the runtime rather than skipping `enter`.
    if std::env::var(WORKER_ENV).is_ok() {
        let runtime_first = std::env::var(CONTROL_ENV).is_ok();
        let early = runtime_first.then(build_runtime);
        if backend.enter(&policy, &Harness::none()).is_err() {
            std::process::exit(EXIT_ENTER_FAILED);
        }
        let runtime = early.unwrap_or_else(build_runtime);
        let outside = outside.to_string();
        let escaped = runtime.block_on(async move {
            tokio::spawn(async move { std::fs::read_to_string(&outside).is_ok() })
                .await
                .expect("the task ran")
        });
        std::process::exit(if escaped { EXIT_ESCAPED } else { EXIT_CONFINED });
    }

    // The control run skips `enter` entirely. Everything below is identical, so
    // if the control escapes and the real run does not, the difference is the
    // ruleset and nothing else.
    let restrict = std::env::var(CONTROL_ENV).is_err();
    if restrict && backend.enter(&policy, &Harness::none()).is_err() {
        std::process::exit(EXIT_ENTER_FAILED);
    }

    // Inside the root: must still work, or the sandbox is useless rather than
    // strict.
    if std::fs::read_to_string(Path::new(root).join("src/main.rs")).is_err() {
        std::process::exit(EXIT_INSIDE_OK + 100);
    }

    if std::env::var(BUILD_ENV).is_ok() {
        // The workflow the agent exists for. Executing `cargo` is only half of
        // it: the build also execs `rustc` out of `RUSTUP_HOME`, reads the
        // registry under `CARGO_HOME`, takes a lock on the package cache, and
        // writes `target/` inside the root. Any one of those refused turns
        // "run the tests" into an error the model cannot act on.
        // `TMPDIR` inside the root, exactly as `Broker::exec` sets it. Without
        // it the linker falls back to `/tmp`, which the ruleset does not grant,
        // and the build fails at the last step with "Cannot create temporary
        // file". That is not an artifact of the test: it is what makes the
        // root-confined scratch directory load-bearing rather than tidy, and
        // the reason this probe sets the variable rather than assuming the
        // toolchain needs nothing.
        let scratch = Path::new(root).join(".summer/tmp");
        std::fs::create_dir_all(&scratch).expect("scratch");
        let built = Command::new("cargo")
            .args(["build", "--offline", "--quiet"])
            .current_dir(root)
            .env("TMPDIR", &scratch)
            .status();
        let ok = match built {
            Ok(status) => status.success(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // No cargo on this machine: nothing to say about the ruleset.
                std::process::exit(EXIT_INSIDE_OK);
            }
            Err(_) => false,
        };
        std::process::exit(if ok { EXIT_INSIDE_OK } else { EXIT_CONFINED });
    }

    if std::env::var(EXEC_ENV).is_ok() {
        // **Every program the policy allows, not one that happens to be in
        // `/bin`.** This probe used to run `/bin/sh -c true`, which passes for a
        // reason that does not generalize: `/bin` is a system prefix, while
        // `cargo` resolves to `~/.cargo/bin/cargo` on every rustup install and
        // was refused outright. The agent's central job is to build the project
        // and run its tests, so a ruleset that cannot execute the toolchain is
        // not strict, it is broken -- and the only remedy was to switch off all
        // enforcement.
        for rule in &policy.commands {
            if rule.effect != Effect::Allow {
                continue;
            }
            let ran = Command::new(&rule.program)
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            match ran {
                // Exit status is irrelevant: `--version` may not be a flag the
                // program knows. What is being tested is that it *ran*.
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // Not installed here. Nothing to say about the ruleset.
                }
                Err(_) => {
                    eprintln!("refused: {}", rule.program);
                    std::process::exit(EXIT_CONFINED);
                }
            }
        }
        std::process::exit(EXIT_INSIDE_OK);
    }

    // Outside the root: the kernel must refuse, with no broker involved. This
    // read does not go through `sandbox` at all, which is the point:
    // it is what a bug in the broker, or code bypassing it, would do.
    match std::fs::read_to_string(outside) {
        Ok(_) => std::process::exit(EXIT_ESCAPED),
        Err(_) => std::process::exit(EXIT_CONFINED),
    }
}

fn spawn_child(root: &Path, outside: &Path, control: bool) -> i32 {
    let exe = std::env::current_exe().expect("test binary path");
    let mut command = Command::new(exe);
    command
        .env(CHILD_ENV, root)
        .env(OUTSIDE_ENV, outside)
        // Run only the child-side test in the spawned process.
        .args(["--exact", "landlock_child_entrypoint", "--nocapture"]);
    if control {
        command.env(CONTROL_ENV, "1");
    }
    let status = command.status().expect("spawn child");
    status.code().unwrap_or(-1)
}

/// The same child, asked to run a program rather than read a file.
fn spawn_child_exec(root: &Path, outside: &Path) -> i32 {
    spawn_child_mode(root, outside, EXEC_ENV, false)
}

/// The same child, asked to build the fixture crate.
fn spawn_child_build(root: &Path, outside: &Path) -> i32 {
    spawn_child_mode(root, outside, BUILD_ENV, false)
}

/// The same child, doing its read from a tokio worker. `runtime_first` builds
/// the runtime before `enter`, which is the shipped ordering.
fn spawn_child_worker(root: &Path, outside: &Path, runtime_first: bool) -> i32 {
    spawn_child_mode(root, outside, WORKER_ENV, runtime_first)
}

fn spawn_child_mode(root: &Path, outside: &Path, mode: &str, control: bool) -> i32 {
    let exe = std::env::current_exe().expect("test binary path");
    let mut command = Command::new(exe);
    command
        .env(CHILD_ENV, root)
        .env(OUTSIDE_ENV, outside)
        .env(mode, "1")
        .args(["--exact", "landlock_child_entrypoint", "--nocapture"]);
    if control {
        command.env(CONTROL_ENV, "1");
    }
    let status = command.status().expect("spawn child");
    status.code().unwrap_or(-1)
}

/// The runtime shape the CLI uses: multi-threaded, workers spawned eagerly.
fn build_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().expect("runtime")
}

/// A fixture repo, canonically rooted, plus a secret in a sibling directory that
/// no rule covers.
fn fixture() -> (tempfile::TempDir, tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = std::fs::canonicalize(dir.path()).expect("canonical");
    std::fs::create_dir(root.join("src")).expect("src");
    std::fs::write(root.join("src/main.rs"), "fn main() {}").expect("main.rs");
    // A real (dependency-free) crate, so the build probe below exercises the
    // toolchain rather than a manifest error.
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .expect("Cargo.toml");

    let elsewhere = tempfile::tempdir().expect("tempdir");
    let outside = std::fs::canonicalize(elsewhere.path())
        .expect("canonical")
        .join("secret");
    std::fs::write(&outside, "shh").expect("secret");
    (dir, elsewhere, root, outside)
}

// ========================================================================
// Tests
// ========================================================================

/// Not a test: the subprocess entry point. It exits before libtest sees it.
#[test]
fn landlock_child_entrypoint() {
    if let (Ok(root), Ok(outside)) = (std::env::var(CHILD_ENV), std::env::var(OUTSIDE_ENV)) {
        run_child(&root, &outside);
    }
}

#[test]
fn without_entering_the_same_read_succeeds() {
    // The control, and the reason the test below means anything. If the probe
    // file were simply unreadable here, the confinement test would pass without
    // Landlock doing a thing.
    if !paths_are_enforced_here() {
        eprintln!("skipping: this kernel does not enforce Landlock paths");
        return;
    }
    let (_dir, _elsewhere, root, outside) = fixture();
    assert_eq!(
        spawn_child(&root, &outside, true),
        EXIT_ESCAPED,
        "the control could not read outside the root, so the confinement test below proves nothing"
    );
}

#[test]
fn entering_confines_reads_to_the_root() {
    if !paths_are_enforced_here() {
        eprintln!("skipping: this kernel does not enforce Landlock paths");
        return;
    }
    let (_dir, _elsewhere, root, outside) = fixture();

    match spawn_child(&root, &outside, false) {
        EXIT_CONFINED => {}
        EXIT_ESCAPED => panic!("the kernel let a read outside the root through"),
        EXIT_ENTER_FAILED => panic!("enter() failed on a kernel that reports paths enforced"),
        other => panic!("child exited {other}: the read inside the root was refused too"),
    }
}

#[test]
fn an_allowed_program_can_still_run_under_the_ruleset() {
    // **The other half of confinement, and the half that was missing.** A
    // ruleset whose only rule is the root does not just deny escapes, it denies
    // `/bin/sh`, the dynamic loader, and `/etc/resolv.conf` -- so every program
    // on the command allow list fails with EACCES and the harness loses DNS and
    // TLS. The only remedy left was to drop `--sandbox`, giving up every
    // dimension of OS enforcement at once. A sandbox nobody can run under is one
    // nobody runs under.
    if !paths_are_enforced_here() {
        eprintln!("skipping: this kernel does not enforce Landlock paths");
        return;
    }
    let (_dir, _elsewhere, root, outside) = fixture();

    let code = spawn_child_exec(&root, &outside);
    assert_eq!(
        code, EXIT_INSIDE_OK,
        "a restricted child could not execute a program the policy allows \
         (the child prints which one)"
    );
}

#[test]
fn the_fixture_crate_builds_under_the_ruleset() {
    // **The end-to-end version of the exec probe, and the one that matters.**
    // A coding agent whose first instruction is "run the tests" is useless if
    // the toolchain cannot run, and the pieces it needs are spread across
    // `RUSTUP_HOME`, `CARGO_HOME` and the root. Checking that `cargo --version`
    // starts says nothing about whether a build completes.
    if !paths_are_enforced_here() {
        eprintln!("skipping: this kernel does not enforce Landlock paths");
        return;
    }
    let (_dir, _elsewhere, root, outside) = fixture();

    assert_eq!(
        spawn_child_build(&root, &outside),
        EXIT_INSIDE_OK,
        "a restricted child could not build a dependency-free crate"
    );
}

#[test]
fn a_tokio_worker_inherits_the_restriction() {
    // **The finding that made every other one exploitable.** A Landlock domain
    // is per-thread and inherited only by threads created *after* the
    // restriction. `Dispatcher::dispatch` runs every tool inside `tokio::spawn`,
    // so a runtime built before `enter` -- which is what `main` did -- left every
    // `read`, `edit` and `shell` call running on an unrestricted worker while
    // the startup banner reported `sandbox: landlock`.
    if !paths_are_enforced_here() {
        eprintln!("skipping: this kernel does not enforce Landlock paths");
        return;
    }
    let (_dir, _elsewhere, root, outside) = fixture();

    assert_eq!(
        spawn_child_worker(&root, &outside, true),
        EXIT_ESCAPED,
        "a runtime built before `enter` must escape, or this test proves nothing"
    );
    assert_eq!(
        spawn_child_worker(&root, &outside, false),
        EXIT_CONFINED,
        "a worker created after `enter` must inherit the domain"
    );
}

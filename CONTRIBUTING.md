# Contributing to Summer Code

Thank you for considering it. This file is the short version of what a patch is expected to come with; [`AGENTS.md`](AGENTS.md) records the reasoning behind the parts that look unusual, and [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) is the set of design contracts the code is expected to keep.

## Setup

A Rust toolchain is all you need; the pinned version arrives via [`rust-toolchain.toml`](rust-toolchain.toml) the moment you build.

```bash
make            # build (debug)
make check      # the gate: fmt, clippy -D warnings, doc, tests, contract assertion
```

## The gate

**Run `make check` before sending a patch, and fix what it flags.** The steps are `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo doc` with warnings denied, `cargo test --workspace`, and the contract-crate dependency assertion.

If you are opening a pull request, CI runs those same steps for you, and adds two your machine does not: a build against the declared minimum supported Rust version, and the full test suite on native macOS and Windows runners. Running the gate locally first is still worth it, because it fails in seconds where a CI run takes minutes.

If you have commit rights and are pushing to `main` directly, **your local run is the only gate**. CI is deliberately not wired to `push`, so nothing downstream will catch a lint failure or a broken contract assertion for you. The reasoning is in [`AGENTS.md`](AGENTS.md).

If your change touches platform-specific code (anything `#[cfg(windows)]`, `#[cfg(target_os = "macos")]`, or the sandbox backends), say so in the patch description. Through a pull request, the native runners exercise it; through a direct push, nothing does.

`scripts/check-platforms.sh` is a local pre-flight for that case. It cross-compiles rather than running anything, and needs a C toolchain per target, so on a plain Linux box it falls back to the pure-Rust crates and names what it skipped. It is not part of `make check`.

## What a change is expected to keep true

The documents are contracts, not marketing:

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) states the design contracts. If your change makes a sentence in it untrue, the sentence changes in the same patch. A doc that over-claims is treated as a bug; the sandbox backends report "unenforced" rather than "enforced" for exactly this reason.
- [`docs/PROGRESS.md`](docs/PROGRESS.md) records what has shipped, and its history includes corrections where a line claimed a capability the binary could not reach. Prefer deleting a claim over softening it.
- New behavior needs a test that would fail without it. The suite's shape to imitate: adversarial security tests in `crates/sandbox/tests/`, golden journal replay in `crates/engine/tests/`, and the byte-exact wire-format round-trip in `crates/journal/`.
- User-visible changes get a `CHANGELOG.md` entry under `## [Unreleased]`, in the section that fits (`Added`, `Changed`, `Removed`, `Fixed`, `Security`). Internal refactors that change no behavior do not.

## Commit style

Conventional prefixes, lowercase, imperative: `feat:`, `fix:`, `docs:`, `chore:`, `test:`. Look at `git log` for the voice; the one-sentence body that says *why* a change is shaped the way it is (not what it does) is the part worth copying.

## Reporting a security issue

Not here, and not in a public issue. [`SECURITY.md`](SECURITY.md) has the private reporting route and the threat model that decides what counts.

## Licensing

Contributions are accepted under the same terms as the project: MIT. See [`LICENSE-MIT`](LICENSE-MIT).

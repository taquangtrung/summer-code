## What this changes, and why

The *why* is the part worth writing. What the diff does is visible; what it is shaped that way for is not.

## Checklist

- [ ] `make check` passes locally (fmt, clippy `-D warnings`, doc, tests, contract assertion).
- [ ] New behaviour has a test that would fail without it.
- [ ] If this makes a sentence in `README.md` or `docs/ARCHITECTURE.md` untrue, that sentence changed in this patch.
- [ ] User-visible changes have a `CHANGELOG.md` entry under `## [Unreleased]`.

## Platform-specific code

Tick if this touches `#[cfg(windows)]`, `#[cfg(target_os = "macos")]`, or the sandbox backends, and say which platforms you tested on. CI runs the suite natively on macOS and Windows for every pull request, so it will exercise this either way.

- [ ] Touches platform-specific code

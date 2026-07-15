# Agent Instructions

## Before declaring work done

Run `make check` (fmt, clippy `-D warnings`, doc, tests, contract-deps assertion)
before considering any change complete, and fix whatever it flags rather than
leaving it for the next person. **On a direct push to `main` this is the only
gate there is**: CI runs on pull requests and nothing else, so nothing downstream
will catch a `clippy -D warnings` failure or a broken contract assertion for a
commit that went straight to the branch.

## On CI

**CI runs on pull requests only, and never on a push.** The original argument
here was that while one person ran `make check` before every commit, a workflow
would mostly re-run what had already been run locally, and the cost of forgetting
was carried by whoever was committing. That argument named its own expiry: a
second contributor, or a pull request from outside. It was right about the
expiry and right about the redundancy, and the trigger is where those two meet.

So the split is by who is committing, not by whether CI exists. A direct push to
`main` comes from someone with commit rights who ran the local gate, and re-running
it in a workflow re-verifies work already verified and mails them about it, which
is how a workflow becomes noise that gets switched off. A pull request comes from
someone who is not covered by a convention recorded in this file, and that is what
the gate is for.

The consequence to keep in mind: **a bad direct push is caught by nobody.** If you
are pushing to `main`, `make check` is not a courtesy, it is the gate.

`.github/workflows/ci.yml` mirrors `make check` step by step rather than calling
the make target, so a failure names the step that failed. Keep the two in step:
a check added to the Makefile and not the workflow is a check that stops
applying to contributors.

## On the platform checks

`scripts/check-platforms.sh` cross-compiles for Windows and macOS and needs a C
toolchain for each, so on a plain Linux box it fails inside `ring` and the
tree-sitter crates for reasons that have nothing to do with the change under
test. It is therefore not part of `make check`.

CI covers what that script could not, on the patches it sees: a pull request runs
the full test suite on native macOS and Windows runners, which have the
toolchains, so platform-specific code is *executed* on each platform rather than
merely compiled for it. A direct push gets none of that, so when you are touching
`#[cfg(windows)]`, `#[cfg(target_os = "macos")]`, or the sandbox backends on
`main`, this script is the only cross-platform signal available and worth running
by hand.

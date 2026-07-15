#!/usr/bin/env bash
# Cross-platform compilation assertion.
#
# The harness ships on Linux, macOS and Windows, and most of what differs
# between them is behind `#[cfg]`: the child environment allowlist, the shell the
# `shell` tool spawns, the home-directory variable, Windows' verbatim path
# prefix, the Landlock backend. Code behind a `cfg` that is false on this machine
# is not compiled here, so a Linux-only `cargo check` says nothing about it. This
# is what says something about it.
#
# **Compilation, not behaviour.** `cargo check` against a target does not run
# tests, so this catches "does not build on Windows" and not "builds and then
# does the wrong thing". Behaviour needs a native CI runner per OS; this is the
# fast local gate that stops the obvious half.
#
# Crates that pull `reqwest` transitively depend on `ring`, whose build script
# needs a C compiler *for the target*. Without a cross toolchain those crates are
# skipped and named, rather than failing the run: the alternative is a check
# nobody can pass locally, which is a check nobody runs.
set -euo pipefail

BLUE='\033[0;34m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
info()    { printf "${BLUE}[platforms]${NC} %s\n" "$1"; }
success() { printf "${GREEN}[platforms]${NC} %s\n" "$1"; }
warn()    { printf "${YELLOW}[platforms]${NC} %s\n" "$1" >&2; }

# The targets worth checking: one per platform family the harness claims.
TARGETS=(x86_64-pc-windows-msvc x86_64-apple-darwin)

# Crates whose dependency tree is pure Rust, so they check against any target
# with only `rustup target add`. `sandbox` is the one that earns this list: it
# holds the backends, and its C dependencies (libc, landlock) are behind
# `cfg(unix)` and `cfg(target_os = "linux")`, so a Windows check compiles the
# `none` path without needing a cross toolchain at all.
#
# `query` and `tools` are deliberately absent despite holding platform-specific
# code: query links the tree-sitter grammars and tools pulls reqwest (and so
# `ring`), both of which build C for the *target*. They are what the whole
# workspace check covers when a cross toolchain is present, and what this
# fallback gives up when one is not.
#
# Every name here must be a workspace member. `cargo check -p` on a package that
# does not exist fails the whole fallback, which is the path this list exists to
# make work; it used to name `summer-monitor` and `mcp`, neither of which was
# ever a crate (they are the `engine::monitor` and `tools::mcp` modules), so the
# fallback could only ever have errored. Cross-check against
# `cargo metadata --no-deps` when editing.
PORTABLE_CRATES=(
    summer-journal
    summer-policy
    summer-sandbox
)

status=0

# Whether rustup has the target's standard library installed.
has_target() {
    local target="$1"
    rustup target list --installed 2>/dev/null | grep -qx "$target"
}

# Check every crate for one target, falling back to the portable subset when the
# full workspace cannot build its C dependencies here.
check_target() {
    local target="$1"

    if ! has_target "$target"; then
        warn "$target: std not installed; run 'rustup target add $target' to include it"
        return 0
    fi

    if cargo check --workspace --all-targets --target "$target" >/dev/null 2>&1; then
        success "$target: whole workspace"
        return 0
    fi

    local args=()
    local crate
    for crate in "${PORTABLE_CRATES[@]}"; do
        args+=(-p "$crate")
    done

    if ! cargo check "${args[@]}" --all-targets --target "$target"; then
        warn "$target: the portable crates do not compile"
        return 1
    fi

    warn "$target: portable crates only (a cross C toolchain would cover the rest)"
    return 0
}

info "checking ${#TARGETS[@]} targets"
for target in "${TARGETS[@]}"; do
    check_target "$target" || status=1
done

if [[ "$status" -ne 0 ]]; then
    warn "cross-platform compilation failed; see the errors above"
fi

exit "$status"

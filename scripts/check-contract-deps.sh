#!/usr/bin/env bash
# Contract-crate dependency assertion.
#
# journal and policy must not depend on tokio, reqwest, or
# sandbox. That is the property that keeps the contract crates consumable
# by outside readers (Phase 9 telemetry, the runtime monitor) without dragging in
# an async runtime or the sandbox. It is a `cargo metadata` / `cargo tree`
# assertion over two crates, not a graph-wide philosophy.
#
# Crates that do not exist yet are skipped; this lets the check run from Phase 0
# (only journal) onward.
set -euo pipefail

forbidden='(^tokio$|^reqwest$|sandbox|tokio:|reqwest:)'
contract_crates=(summer-journal summer-policy)
status=0

for crate in "${contract_crates[@]}"; do
    if ! cargo metadata --no-deps --format-version=1 >/dev/null 2>&1; then
        echo "error: cargo metadata failed" >&2
        exit 1
    fi
    # Skip crates that are not workspace members yet.
    if ! cargo tree -p "$crate" >/dev/null 2>&1; then
        continue
    fi
    if cargo tree -p "$crate" | grep -E "$forbidden" >/dev/null; then
        echo "error: contract crate $crate pulls a forbidden dependency:" >&2
        cargo tree -p "$crate" | grep -E "$forbidden" >&2
        status=1
    fi
done

if [ "$status" -ne 0 ]; then
    echo >&2
    echo "Contract crates may not depend on tokio, reqwest, or sandbox." >&2
    echo "See docs/ARCHITECTURE.md, Workspace Layout." >&2
fi

exit "$status"

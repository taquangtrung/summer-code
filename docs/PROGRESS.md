# Summer Code Implementation Progress

This document tracks completed, verified phases and features in **Summer Code**.

## Phase 0: Foundations

* [x] Workspace, `[workspace.dependencies]`, Edition 2024, `rust-toolchain.toml`.
* [x] `journal`: schema v0 (ids, codec, two-phase walk, sink).
* [x] Wire format: every enum internally tagged on `kind`, `rename_all = "snake_case"`, `SystemTime` as RFC 3339 millis UTC.
* [x] `docs/journal-sample.jsonl`, one line per `EventKind`, byte-exact round-trip test.
* [x] Contract enforcement: `clippy.toml` disallowed methods and `scripts/check-contract-deps.sh`.

## Phase 1: MVP Turn Loop and Resilience

* [x] `sandbox`: `ToolCx`, `Broker`, handle types, Landlock backend on Linux.
* [x] `provider`: `Provider` trait, Anthropic streaming client, `Mock` provider.
* [x] `tools`: `read`, `edit`, `grep`, `shell`, `git`, `symbols`, JSON-Schema validation, serial `Dispatcher`.
* [x] `Limits` (turns, tokens, tool calls, cost) enforced at the top of the loop.
* [x] Exponential backoff retry with full jitter and `Retry-After` override parsing.
* [x] Mid-stream failure recovery discarding dead attempts (`TurnRestarted`).

## Phase 2: Policy Engine and Kernel Sandboxing

* [x] `policy`: `Policy`, `Effect`, `PathRule`, `CommandRule`, `NetworkRule`, `ListenRule`.
* [x] Built-in default policy, lexical normalization, path canonicalization.
* [x] Landlock ABI kernel enforcement on Linux (`capabilities()` probe and `enter` confinement).
* [x] Adversarial security suite: path traversal, symlink escapes, command injection, policy tampering.

## Phase 3: Journal Hardening and Time-Travel

* [x] Replay and resume from journal walks without re-asking granted permissions.
* [x] Interrupted call handling: synthetic outcome generation on resume.
* [x] Golden journal test suite asserting exact call tree reproduction.

## Phase 4: Context Management

* [x] Context window compaction: mid-range dropping, keeping the first user message and the recent window, with a mechanical summary (a count of the dropped turns and tool calls) recorded on the `Compacted` record. Model-written summaries landed later; see Phase 12.
* [x] Token budget enforcement distinguishing input/output tokens from cached tokens.

## Phase 5: Codebase Understanding and Fact Schema

* [x] `query` crate: content-addressed `SymbolId`, `FactStore`, `LineExtractor`.
* [x] `symbols` tool for code symbol search and extraction across the workspace.

## Phase 6: Terminal UI (Ratatui REPL)

* [x] Dual-paneRatatui TUI with live streaming token rendering and tool call spinners.
* [x] Modal prompt input and keyboard shortcuts.

## Phase 7: Model Context Protocol (MCP)

* [x] `tools::mcp`: JSON-RPC 2.0 client/server transport for external tools. Reachable from the CLI as of Phase 12; until then nothing depended on it, so this line claimed a capability the binary could not use. It was its own crate until it folded into `tools`, whose dispatcher every discovered tool goes through anyway.

## Phase 8: Cheap Model Code Optimization & In-Loop Verification

* [x] Skeleton Extractor ([`crates/query/src/skeleton.rs`](../crates/query/src/skeleton.rs)): Distills source files into interface header stubs for Rust, TypeScript, JavaScript, Python, Go, C, and C++.
* [x] Fast Post-Edit Compiler Diagnostics ([`crates/tools/src/verifier.rs`](../crates/tools/src/verifier.rs)): Executes `cargo check` / `tsc` automatically post-edit.
* [x] Automatic Diagnostic Self-Correction Loop ([`crates/engine/src/engine/mod.rs`](../crates/engine/src/engine/mod.rs)): Appends compiler diagnostics directly to `edit` tool results for 1-turn self-repair.
* [x] Cheap Model System Prompt ([`crates/engine/prompts/cheap_model_system.md`](../crates/engine/prompts/cheap_model_system.md)): Prompt tuned for precision context, TDD Discriminator Principle, and minimal blast radius.

## Phase 9: Latency, Extensibility, and Reproducibility

* [x] Batched tool dispatch: tools declare `Concurrency`, adjacent parallel-safe calls run together (capped at 8), journal order stays the request order.
* [x] Per-call tagging (`sandbox::tag_call`) so broker permission records stay attributable with several calls in flight.
* [x] `glob` tool: find files by name pattern, newest first, through the broker rather than `find`.
* [x] `web_fetch` and `web_search`: HTTP page reading with HTML-to-text extraction, each host authorized through the network policy, redirects re-authorized per hop.
* [x] Hooks: `preToolUse` / `postToolUse` / `stop`, user-tier config only, exit 2 to block.
* [x] Headless mode: `-p/--print` with `--output-format text|json|stream-json` and distinct exit codes per stop reason.
* [x] User-defined slash commands from `~/.summer/commands` and `.summer/commands`, with `$ARGUMENTS` substitution.
* [x] Kernel confinement behind `--sandbox`, auto-approving unlisted commands and journaling them as `confined_allow`. **The default is off**, and `--no-sandbox` is accepted and ignored for the scripts that still pass it. This line previously read "sandbox on by default", which was true when it was written and became the opposite of the code once the default moved; [`ARCHITECTURE.md`](ARCHITECTURE.md) has the reasoning for the posture.
* [x] Workspace snapshots and `/rewind [n]`, content-addressed with an append-only manifest.
* [x] OpenAI chat-completions transport, serving both the `openai` and `gemini` profiles.
* [x] `bench --replay <dir> [--against <dir>]`: recorded sessions as a regression corpus, scored with Wilson intervals.

## Phase 10: Two Extraction Mechanisms

* [x] `TreeSitterExtractor`: one extractor, seven grammars (Rust, TypeScript, JavaScript, Python, Go, C, C++), emitting `Defines` and `Calls`.
* [x] Symbol table on `FactStore`, so a query can name what a content-addressed id refers to.
* [x] `CodeGraph` keyed by name rather than by the `Debug` of a hash, returning `Answer` with declared `Completeness`.
* [x] `symbols` and `graph` rewired onto tree-sitter; the keyword scanner and `LineExtractor` deleted.
* [x] Persistent-child capability in the broker (`Broker::serve`): `mcp/stdio.rs` and the LSP client both start their long-lived children through it rather than around it.
* [x] `LspExtractor` against it: initialize, `documentSymbol`, shutdown, bounded by a whole-session timeout, with live tests in [`crates/query/tests/lsp_live.rs`](../crates/query/tests/lsp_live.rs). `callHierarchy` is not requested yet, so call facts still come from tree-sitter.

## Phase 11: Mechanisms That Are What They Say

* [x] Skill gate ([`crates/engine/src/skill_gate.rs`](../crates/engine/src/skill_gate.rs)): configured globs require a skill to have been loaded before an edit to a matching path runs.
* [x] Quality metrics in `bench` ([`crates/bench/src/quality.rs`](../crates/bench/src/quality.rs)): churn against the fixture, refused calls, tool calls, and the task's own lint count, reported as medians with their spread rather than through the Wilson interval, which is for proportions.
* [x] `bench` scores any configured provider, not only Anthropic.
* [x] Plan gate: a turn that ends with plan steps outstanding is reminded once, then stops `Stop::PlanIncomplete`.
* [x] `subagent` runs a real sub-session ([`crates/cli/src/subagent.rs`](../crates/cli/src/subagent.rs)): own context, own journal, shared workspace and broker, no `subagent` tool of its own. Offered to the model only when one can be run.
* [x] `/compact` compacts the context window through `Engine::compact_now` instead of clearing the on-screen transcript.
* [x] Session modes ([`crates/cli/src/mode.rs`](../crates/cli/src/mode.rs)): `--mode plan|ask|default|full`, each one a policy layer merged above the user's own file rather than a second permission system. Chosen at startup; the broker owns its policy by value, so planning and then acting is two sessions joined by `/resume`.
* [x] Hybrid Emacs/Vim Modal Keymap ([`crates/cli/src/tui/input.rs`](../crates/cli/src/tui/input.rs), [`crates/cli/src/tui/view.rs`](../crates/cli/src/tui/view.rs)): Emacs mode by default, seamless `/keymap <emacs|vim>` switching, Vim normal/insert modal motions, and visual status banner.
* [x] Structural AST Pattern Search (`pattern` tool) ([`crates/query/src/ast_pattern.rs`](../crates/query/src/ast_pattern.rs), [`crates/tools/src/pattern_tool.rs`](../crates/tools/src/pattern_tool.rs)): Tree-Sitter query S-expressions and node-kind matching across 7 supported languages.
* [ ] Ablation: run the task set with each quality mechanism disabled and delete the ones that do not move a metric.

## Phase 12: Wiring What Was Already Built

* [x] MCP servers reachable from the CLI ([`crates/cli/src/mcp.rs`](../crates/cli/src/mcp.rs)): `mcpServers` in `~/.summer/config.json` names a program per server, each started through `Broker::serve` and discovered via `tools/list`, with its tools joining the same dispatcher the built-ins use. User tier only, exactly like `hooks`, since an entry is a program the harness executes.
* [x] Discovered tools are namespaced `mcp__<server>__<tool>`, so an external server cannot shadow a built-in and two servers exporting the same tool name stay distinct. The server is still called by the name it advertised.
* [x] A server that fails to start or handshake is a warning and a skip, not a failed session, bounded by a 30s connect timeout.
* [x] Model-written compaction summaries ([`EngineBuilder::summarize_compactions`](../crates/engine/src/engine/mod.rs)): the dropped range is replayed to the model as its own messages and the reply is recorded on the `Compacted` record. Opt-in, because `Recorded` replay is positional over `Provider::turn` and an unasked-for call would desynchronize every later turn; `bench --replay` and the golden suite keep the mechanical count. Any failure falls back to it too.
* [x] The `web_fetch` DNS gap closed at the source ([`Broker::lookup`](../crates/sandbox/src/broker.rs), [`crates/tools/src/web/resolver.rs`](../crates/tools/src/web/resolver.rs)): the broker resolves an authorized host as part of the evaluation that prompts and journals it, and both web tools build their client over a resolver that replays only those addresses and refuses every other name. The policy check and the connection now rest on one lookup instead of two that were never compared. Proxies are off with it, since a proxied request reaches a host the policy never saw.
* [x] `inward-resolution`, a fourth runtime property ([`crates/engine/src/monitor/properties/inward_resolution.rs`](../crates/engine/src/monitor/properties/inward_resolution.rs)): a `Permission` record for a connect now carries the addresses the host resolved to, so the monitor can flag an allowed name that pointed at loopback or at the cloud metadata address. The policy matches hosts by name and cannot express this, and the broker will not overrule a rule the user wrote, so the honest mechanism is a report rather than a refusal.
* [x] Property-based tests for the evaluator ([`crates/policy/tests/evaluator_properties.rs`](../crates/policy/tests/evaluator_properties.rs)): containment in both directions, the `.git` and `.summer` write denies at arbitrary depth, root dotfiles, and the two fail-open shapes the evaluator documents (a host deny dodged by case or a trailing dot, a command deny dodged by a path prefix). Both mutations tried against them, a bare `**` in the built-in allow and a lost case fold in host normalization, are caught and shrunk to a one-element counterexample.
* [x] Model catalog corrections: the Copilot profile defaulted to `claude-3-5-sonnet` (retired 2025-10) and listed four more retired ids, so `/login github-copilot` plus one turn 404'd. Every Claude entry also mispinned its context window at 200K, which reported a 1M session as five times fuller than it was.

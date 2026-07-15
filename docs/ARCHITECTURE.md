# Summer Code Architecture

This document describes the architectural boundaries, crate organization, and design contracts of the **Summer Code** agentic system.

## Workspace Layout

Summer Code is organized into decoupled, single-responsibility workspace crates:

| Crate | Responsibility |
|---|---|
| `journal` | Wire format, schema, RFC 3339 event-sourced log codec, and replay mechanics |
| `policy` | Path, command, and network authorization rules, decision evaluation, and scope merging |
| `sandbox` | OS-level confinement and the execution broker. Landlock enforcement on Linux; the macOS and Windows backends are unimplemented stubs that say so at startup rather than claiming enforcement they cannot back |
| `query` | Codebase facts, symbol schemas, the tree-sitter and LSP extractors, and ripgrep integration |
| `tools` | Built-in tools (`read`, `edit`, `grep`, `glob`, `shell`, `git`, `symbols`, `graph`, `plan`, `subagent`, `web_fetch`, `web_search`), tool validation schemas, post-edit verifiers, and the MCP bridge |
| `provider` | Vendor-agnostic LLM streaming client interface: the Anthropic Messages API (Anthropic, Z.ai, GitHub Copilot) and OpenAI chat-completions (OpenAI, Gemini via Google's compatibility endpoint) |
| `engine` | Agent execution loop, batched tool dispatch, hooks, token limits, context window compaction, workspace snapshots, diagnostic self-correction feedback, and the runtime monitor |
| `bench` | Benchmarking harness, task runner, and Wilson interval effect size evaluator |
| `cli` | Interactive Ratatui TUI REPL frontend |

Two areas are modules inside those crates rather than crates of their own, because each had exactly one consumer and no independent build:

| Module | Responsibility |
|---|---|
| `tools::mcp` | Model Context Protocol client and server bridge. The client is a `ToolSource`, so a discovered tool inherits schema validation, the call-id ledger, timeouts, and the broker with no plumbing of its own; `cli` starts the configured servers. It lives in `tools` because everything it produces reaches a caller through that crate's dispatcher |
| `engine::monitor` | Real-time audit trail and policy violation checker. It reads journal records and policies and nothing else, so it stays usable on a finished journal without running the loop |

## Core Architectural Contracts

1. **Kernel-Enforced Sandbox & Zero-Trust Policy:**
   Every file access, command execution, and network connection is checked against the policy engine before execution. Landlock confines worker threads at the OS kernel level on Linux, and is **opt-in** (`--sandbox` enables it; the default runs unconfined with the broker checking every access and prompting for what no rule names).

   Confinement is what removes prompts rather than adding them. When the backend reports *both* path and connect enforcement as genuinely in force, a command the allowlist does not name is allowed without asking, because the child inherits the domain and reaches neither a path nor a host outside it. That decision is journaled as `confined_allow`, distinct from a rule-based `allowed`, so an auditor can tell which guarantee an access rested on.

   **Both dimensions, because the auto-allow creates the very process the second one governs.** A child's `connect` never reaches the broker, so the prompt a tool would get is no protection against a command that was waved through; only Landlock's own `ConnectTcp` rule is. That rule is not always present when path rules are: a policy naming a host without a port cannot be narrowed into one, which leaves `connect` at `Partial` while `paths` stays `Enforced` and the session still starts. Gating on `paths` alone auto-allowed every unlisted command with unrestricted egress behind it. Denied paths and tool-issued network connections still prompt either way.

2. **Batched Tool Dispatch:**
   Tools declare a `Concurrency`; only adjacent parallel-safe calls share a batch, capped at eight. Calls are journaled in the order the model asked and results are journaled in that same order once the batch finishes, so batching changes latency and never record order. Each call is tagged so the broker's permission records stay attributable while several are in flight.

3. **Hooks:**
   User-configured commands run at `preToolUse`, `postToolUse`, and `stop`. Exit 2 blocks; any other non-zero status is a broken hook, not a veto. Hooks are user-tier configuration only: a project file that could declare one would execute repo-authored code on `git clone` plus one session.

4. **Deterministic Event-Sourced Journaling:**
   Every turn, tool call, tool result, and permission decision is recorded as an immutable JSONL record. Any session can be replayed or resumed with 100% exact state restoration.

5. **Two Extraction Mechanisms, and Only Two:**
   Code understanding comes from tree-sitter (syntactic) or LSP (semantic), and from nothing else. `Extractor` is implemented once per mechanism, not per language or per query, so adding a language is a grammar and adding a query touches no extractor. Both write into one content-addressed `FactStore`, which is what lets their facts join.

   The two disagree on purpose. A grammar sees `foo()` and knows the text, not which `foo`, so tree-sitter emits `ModulePath::Unresolved` and its call edges join only within a file. A language server resolves properly and emits `Resolved` ids that join exactly. Neither is asked to pretend: every query result carries `Completeness`, so an answer built by name-matching is never labelled `Complete`, and an empty caller list is reported as "this mechanism found none" rather than "there are none".

   There is deliberately no heuristic tier. A keyword scanner reported `fn` inside string literals and comments as real symbols, and its results were indistinguishable from a parse to anything downstream.

   Alongside this, in-loop static verifiers catch compiler and lint errors post-edit, feeding diagnostic feedback back to cheap models for instant 1-turn self-correction.

6. **Context Window Management:**
   Context compaction uses mid-range dropping while preserving the initial user instructions. Token spend and context window size are tracked separately to support prompt caching.

   **What replaces the dropped range is a summary, and there are two kinds on purpose.** `context` plans the drop and produces a mechanical summary: a count of the turns and tool calls that went. That half stays pure and synchronous, so every compaction test is arithmetic over a history slice with no provider in sight. The engine may then replace it with a model-written handover note, asking the model to summarize its own dropped turns before the drain, and records whichever it ends up with on the `Compacted` record, so a resume folds back exactly what the live session went on to use.

   The model-written half is opt-in rather than default, and the reason is replay: `Recorded` answers the Nth `Provider::turn` with the Nth recorded turn and ignores the request, so one extra call would shift every later turn by one, silently, since the loop cannot tell `Recorded` from a live model. Interactive and headless sessions turn it on; `bench --replay` and the golden suite keep the mechanical count. A summarizer that fails falls back to it as well, because a compaction only happens when the window is already full, which is the worst moment to end a session over a failed side call.

7. **Workspace Snapshots and Rewind:**
   The broker captures a file's contents before every authorized write into a content-addressed store under `~/.summer/projects/<encoded-root>/snapshots` (Claude Code's central layout — nothing session-generated lands inside the repo; an `--sandbox` session, confined to the workspace root, keeps the in-project `.summer/snapshots` instead), with an append-only manifest beside the journal. `/rewind [n]` restores the workspace to the start of the nth-from-last turn, deleting files the agent created and restoring the ones it changed. Distinct from `/undo`, which reverses a git patch and therefore needs a tracked, committed tree. Session journals live under the same per-project store's `sessions/`, migrated once from the legacy in-project `.summer/sessions` on first unconfined run.

8. **Replay-Based Evaluation:**
   Because a journal records the prompt, every tool call, and how the turn stopped, a recorded session is already a test case. `bench --replay <dir> --against <dir>` reduces each session to the shape of its work (which tools, against which targets, in what order) and reports how much of one run the other reproduced, through the same Wilson interval as every other bench number. Recording a corpus under one model and re-recording it under another is a model comparison that needs no judge and no hand-written assertions.

9. **External Tools Are Ordinary Tools:**
   An MCP server named in `~/.summer/config.json` is started through `Broker::serve`, so the program is a policy decision like any other exec, and its tools arrive as a `ToolSource`, so they reach the model through the same dispatcher a built-in does: the same schema validation, the same call-id ledger, the same per-call timeout and cancellation. Nothing about a tool's origin changes how it is dispatched, journaled, or governed.

   Two rules keep that from being a way in. **Configuration is user tier only**, exactly as hooks are, because an entry names a program the harness executes at session start, and a project file that could name one would run repo-authored code on `git clone` plus one session. **Every discovered tool is namespaced** `mcp__<server>__<tool>`, which makes shadowing a built-in unrepresentable rather than merely detected: the dispatcher would refuse a duplicate name, but refusing is a startup failure, and one careless server should not cost a user every other one. The prefix is local; the server is still called by the name it advertised, since it never agreed to be renamed.

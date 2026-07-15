# Summer Code Technical Roadmap

This document outlines planned upcoming features and technical enhancements to expand **Summer** as a state-of-the-art terminal AI coding assistant. Items already built have moved to [`PROGRESS.md`](PROGRESS.md).

## 1. Structural Rewrite

* **Pattern-Based Rewrite:** Structural *search* landed with the `pattern` tool (tree-sitter query S-expressions and node-kind matching across the seven grammars; see Phase 11 in [`PROGRESS.md`](PROGRESS.md)). What does not exist yet is the write half: rewrite matches in place with capture references (`($X == null)` → `($X.is_none())`), the way structural search-and-replace works in an IDE. Until it does, structural finds still have to be turned into `edit` calls by hand, one match at a time.

**Correction, twice over:** a revision of this file once said "the LSP bridge landed in `query::LspExtractor`" when `extract` had an empty body, and the correction that replaced it is now itself out of date twice over. `LspExtractor` runs a real session (started through the broker, handshaked, `documentSymbol`, whole-session timeout, live tests in `crates/query/tests/lsp_live.rs`), and structural querying landed as the `pattern` tool. The rewrite is what remains missing here, not the search.

## 2. Multi-Model Parallel Comparison

* **Parallel Turn Dispatch:** Send a prompt simultaneously to two configured model providers. The provider side is ready: Anthropic, OpenAI, Gemini, Z.ai and Copilot are all reachable behind one `Provider` trait, and `bench --replay --against` already compares two recorded runs after the fact. What is missing is running both live in one session.
* **Side-by-Side TUI View:** Render streaming completions side-by-side in split panes to evaluate code quality, reasoning depth, and latency in real time.

## 3. Modal Editing

* **Custom Modal Keymaps:** The built-in hybrid Emacs/Vim keymap is done — `/keymap emacs|vim` switching, Vim normal/insert modal motions, and the visual status banner (Phase 11). What does not exist yet is *data-driven* modal state: a `~/.summer/keymap.json` that defines its own modes and per-mode bindings rather than selecting between the two built-ins. Until it does, a user who wants `jk` for insert→normal plus a custom normal mode has nowhere to write it down.

## 4. Speculative Dry-Run Sandboxing

* **Copy-on-Write Overlay:** Execute candidate shell commands in an isolated temporary copy-on-write overlay before committing filesystem changes to the workspace. Snapshots now make a *post-hoc* undo possible (`/rewind`); an overlay would make the dangerous command never touch the tree at all.

## 5. Closing the Web-Fetch Gap

**Broker-controlled resolution landed**, so what this section asked for is half done and the half that remains is narrower than the sentence it replaces. `Broker::lookup` now resolves the host itself, as part of the same evaluation that prompts and journals it, and both web tools build their `reqwest` client over a resolver that can only replay those addresses and fails every other name. There is no second lookup left to disagree with the first; see Phase 12 in [`PROGRESS.md`](PROGRESS.md).

* **Broker-Owned Transport:** What is still the client's rather than the broker's is the connect itself: `reqwest` opens a socket to an address the broker resolved, instead of being handed the `AuthorizedSocket` the broker already knows how to produce. That residue costs one unchecked TCP connect to a checked address, and closing it means owning the TLS handshake and the HTTP framing over a broker socket, which is a client library's job. The tractable version is a custom connector over `hyper`, not a hand-written client.

## 6. Session Branching

* **Fork From a Rewind:** `/rewind` restores the workspace but leaves the conversation intact. The journal already supports walking from any record id, so forking a genuine second branch from a rewind point (and switching between branches) is the natural next step.

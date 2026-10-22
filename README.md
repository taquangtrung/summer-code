# Summer Code

An agentic coding assistant for the terminal: a Ratatui TUI over an event-sourced agent loop, with a policy engine and kernel-enforced sandboxing between the model and your machine.

Every file read, command run, and network connection is checked against a policy before it happens, and every turn, tool call, tool result, and permission decision is recorded in the session as an immutable record, so any session can be replayed or resumed exactly.

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the design contracts and [`docs/PROGRESS.md`](docs/PROGRESS.md) for what has shipped.

## Install

Building needs a Rust toolchain; the pinned version is in [`rust-toolchain.toml`](rust-toolchain.toml).

```bash
# Linux, system-wide via a .deb (needs root)
make install

# Linux, current user only (~/.local/bin must be on PATH)
make install-files PREFIX=$HOME/.local

# Windows, per-user (needs Inno Setup 6)
make install
```

`make uninstall` reverses whichever was used. To run without installing, `make dev` builds and launches the CLI.

## Getting started

Set a credential and run `summer` in a repository:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
summer
```

A session starts without a credential too, so you can look around; model turns fail until one is set. `/login <provider>` stores one instead, and for GitHub Copilot it runs an OAuth device flow.

Providers: `anthropic`, `openai`, `gemini`, `zai`, `zai-cn`, `github-copilot`. Pick one for a session with `--provider <id>`, and a model with `--model <name>`.

`local` is any OpenAI-compatible server you run yourself, such as `llama-server`, vLLM, Ollama or LM Studio. It needs no key and defaults to `http://127.0.0.1:8080/v1/`, where `llama-server` listens; set `SUMMER_LOCAL_BASE_URL` to point it elsewhere (vLLM's default is `http://127.0.0.1:8000/v1/`), and `SUMMER_LOCAL_API_KEY` only if the server was started with a key. The variable moves the `local` provider only, so a hosted provider's credential is never sent to a host you named.

## Flags

| Flag | Meaning |
|---|---|
| `--sandbox` | Turn on OS-level confinement (Landlock on Linux; the macOS and Windows backends are stubs that say so at startup). Off by default; see [Sandboxing](#sandboxing) |
| `-c`, `--continue` | Resume this project's newest session before the first turn |
| `--mode <plan\|ask\|default\|full>` | How much the session may do without asking |
| `--provider <id>`, `--model <name>` | Override the stored defaults for this session |
| `--keymap <emacs\|vim>` | Input editor keymap; Emacs by default |
| `--plain` | Use the plain line REPL instead of the TUI |
| `-p`, `--print` | Headless: run one prompt and exit, with `--output-format text\|json\|stream-json`. With `--goal`, keep working until the model's plan closes, up to a bounded number of turns |

## Slash commands

`/help` lists them in the app. `?` is a synonym.

| Command | |
|---|---|
| `/status`, `/usage`, `/policy` | Session status, token usage, effective policy |
| `/sessions`, `/resume <id>` | List and resume past sessions |
| `/login <provider> [api-key]`, `/model`, `/effort <level>` | Credentials, model choice, reasoning effort |
| `/mode <auto\|interactive\|plan>` | Execution mode (also Shift+Tab) |
| `/diff`, `/checkpoint`, `/undo`, `/rewind [n]` | Workspace inspection and rollback |
| `/compact` | Drop older turns from the context window, keeping a summary |
| `/agent <task>`, `/review` | Delegate to a subagent; run a pre-commit review |
| `/keymap`, `/statusline`, `/thinking`, `/tools`, `/clear` | Appearance and input |

## Configuration

Two files, layered, and **they are not equally trusted**:

- `~/.summer/config.json` is yours, and may set anything here.
- `.summer/config.json` in the repository is **untrusted**, because it arrives with a `git clone`. It may set cosmetic fields and skill rules; anything that names a program to run is ignored with a warning.

```json
{
  "mode": "full",
  "provider": { "id": "anthropic", "model": "claude-opus-5", "effort": "xhigh" },
  "tools": { "shell": "bash" },
  "tui": {
    "keymap": "vim",
    "statusLine": { "type": "command", "command": "~/bin/my-statusline" }
  },
  "hooks": [
    { "event": "preToolUse", "matcher": "edit", "command": "./scripts/guard.sh" }
  ],
  "skills": [
    { "paths": "crates/**/*.rs", "skill": "rust-orchestrator" }
  ],
  "mcpServers": [
    { "name": "docs", "command": "docs-mcp-server", "args": ["--stdio"] }
  ],
  "notify": true
}
```

`hooks` and `mcpServers` are **user tier only**: both name a program the harness executes, so honouring one from repository content would mean a clone plus one session is enough to run its code. `mode` is user tier only too: it is the session mode used when `--mode` is not given, and `"full"` runs every write and command without asking, which a cloned repository must not be able to choose for you.

Alongside the config, `~/.summer/` also holds `policy.json` (path, command, and network rules), `skills/` (instructions the `skill` tool loads on demand), and `commands/` (your own slash commands, with `$ARGUMENTS` substitution). A repository may add its own `.summer/policy.json`, `.summer/skills/`, and `.summer/commands/`.

### MCP servers

An entry in `mcpServers` is started through the broker, so the program is a policy decision like any other command: the first run asks before launching it. Its tools then arrive through the same dispatcher the built-ins use, with the same schema validation, timeouts, and cancellation.

Discovered tools are namespaced `mcp__<server>__<tool>`, so a server cannot shadow a built-in such as `shell`, and two servers exporting the same tool name stay distinct. The server itself is still called by the name it advertised.

A server that fails to start or handshake is reported and skipped, not fatal to the session. Only stdio servers are supported.

## Sandboxing

The OS sandbox is **off by default** and `--sandbox` turns it on. Confinement's costs land on the paths a session shares with your machine rather than the repository: with it on, `/login` cannot store a credential under `~/.summer`, and the device flow cannot open a browser.

Either way the broker checks every file access, command, and network connection against the policy and asks about anything no rule covers. What confinement changes is that an unlisted *command* no longer needs a prompt, because the child inherits the domain and can reach neither a path nor a host outside it. Those are recorded in the session as `confined_allow` rather than `allowed`, so an audit can tell which guarantee an access rested on.

A `shell` call is judged by the programs its command line runs, not by the shell running it: `cargo test 2>&1 | tail -5` runs without asking because `cargo` and `tail` are both allowed, a line naming one unlisted program asks, and a line naming a denied program is denied. Only a small subset of shell is read this way (words, quotes, `;`, `&&`, `||`, `|`, and redirections that duplicate a descriptor or discard to `/dev/null`); a line using expansion, a variable assignment, a redirect to a file, or other grammar is asked about as a whole. Out of the box the allowed programs are `cargo`, `git`, `ls`, `rg`, and the read-only filters `cat`, `cut`, `grep`, `head`, `tail`, `tr` and `wc`. When a line has exactly one program no rule allows, the prompt asks about that program alone, and answering "always" remembers it with any arguments for the rest of the session, so `python3 -m pytest | tail` after `python3 -m unittest` does not ask again. Shells and wrappers whose arguments name the real program (`env`, `xargs`, `sudo`, `timeout`) are only ever remembered by their exact command line.

That drop-the-prompt behaviour needs the kernel backing *both* paths and outbound connections, and a policy that names a host without a port cannot be narrowed into a Landlock network rule. Give every `connect` rule a port if you want unlisted commands to run unprompted; without one the session still runs, and still asks.

## Development

```bash
make            # build (debug)
make check      # the gate: fmt, clippy -D warnings, doc, tests, contract assertion
make bench      # score a task set against a live model
make ablate     # run the ablation matrix over the quality mechanisms
```

`make check` is the gate. CI runs the same steps on every pull request, plus an MSRV build and the full suite on native macOS and Windows runners; it deliberately does not run on pushes, so a direct commit to `main` is gated by the local run alone. See [`AGENTS.md`](AGENTS.md) for why, and for what is expected before a change is called done. [Contributions are welcome](CONTRIBUTING.md); the same gate applies to patches from anyone. Security issues go through [`SECURITY.md`](SECURITY.md) rather than the issue tracker.

## License

MIT. See [`LICENSE-MIT`](LICENSE-MIT).

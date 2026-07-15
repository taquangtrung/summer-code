# Security Policy

## Reporting a vulnerability

Report privately, through [GitHub's private vulnerability reporting](https://github.com/taquangtrung/summer-code/security/advisories/new) on this repository. Please do not open a public issue for anything in the scope below.

Include what you need to make the finding reproducible: the version or commit, the platform and kernel (Landlock behaviour varies by ABI, so `uname -r` matters), the policy in force, and the smallest sequence of steps that shows the problem. A journal excerpt is often the fastest way to show what the broker decided and why; scrub it for credentials first.

Expect an acknowledgement within a few days. This is a small project with no security team behind it, so a fix may take longer than that; you will be told which it is rather than left waiting.

## What is in scope

The threat model is that **the model is untrusted and the repository is untrusted**, while you and your machine are not. Anything that lets either of those cross a boundary they are not supposed to cross is in scope:

- A tool performing file, process, or network access that the broker did not authorize, or that no `Permission` record accounts for.
- A path escaping the workspace root: traversal, a symlink, a race between the check and the open, or a `..` that normalization absorbs.
- Repository-tier content gaining user-tier trust. A `.summer/config.json` that arrives with a `git clone` may set cosmetic fields and skill rules; if it can cause a program to run, that is a vulnerability.
- An MCP server shadowing a built-in tool, or reaching outside what its own policy decision covered.
- A credential reaching a journal, a log line, a tracing span, a `Debug` output, or a child process's environment.
- A sandbox backend reporting `Enforced` for a dimension the kernel is not actually backing, or `capabilities()` clearing a policy the OS cannot back. Over-claiming enforcement is treated as a vulnerability, not a documentation bug.
- Anything that lets a resumed or replayed session acquire a grant the original session never obtained.

## What is not in scope

- **Running without `--sandbox`.** OS confinement is opt-in and off by default; the README says so. Without it the broker still checks every access and prompts for what no rule covers, but there is no kernel-level second line of defense, and that is the documented behaviour rather than a flaw.
- **A policy that permits what you did not want.** Policies are data, and a permissive rule is a configuration, not a bypass. If the evaluator applies the rules as written, that is working.
- **The macOS and Windows backends not confining anything.** Both report every dimension unenforced at startup and refuse to pretend otherwise; a `--sandbox` session on either declines to start rather than claiming protection it lacks. See `docs/ARCHITECTURE.md`.
- **A model producing bad, wrong, or destructive output** that the policy allowed and you approved. Approval fatigue is a real problem and worth reporting as an issue; it is not a vulnerability.
- Findings that require an attacker to already have code execution as your user, or write access to `~/.summer/`.

## Supported versions

The project is pre-1.0 and only the `main` branch is supported. Fixes land there; there are no backports.

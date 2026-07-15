//! User-configured hooks: commands the harness runs at fixed points in the loop.
//!
//! **A hook is the user's code, not the agent's, and that distinction decides
//! everything else here.** Hooks come from the user's own config file, which the
//! agent cannot write (`**/.summer/**` is on the built-in deny list). They
//! therefore run with the user's permissions and outside the broker, exactly as
//! the provider's own network access does. Routing them through the policy would
//! be theatre: the thing being protected from the agent is the config, and that
//! protection already happened when the agent was refused write access to it.
//!
//! **The gate this generalizes already existed.** The loop ran `cargo check`
//! after an edit and fed the diagnostics back to the model, hard-coded into the
//! turn. That is one instance of a general shape -- run something after a tool,
//! let what it says reach the model -- and every project that wanted a formatter,
//! a custom linter, or a policy check instead had to patch the engine. A
//! [`PostToolUse`] hook is that shape as configuration.
//!
//! # Protocol
//!
//! A hook receives a JSON object on stdin and answers with its exit status:
//!
//! | Status | Meaning |
//! |---|---|
//! | 0 | Proceed. `stdout`, if any, reaches the model as extra context. |
//! | 2 | Block. `stderr` becomes the reason, and the model sees it. |
//! | other | The hook itself failed. Logged; the loop proceeds. |
//!
//! Reserving one specific status for "block" is what keeps a crashing hook from
//! silently halting the agent: a hook that segfaults, or that is not installed,
//! exits non-zero for reasons that have nothing to do with the tool it was
//! screening, and treating every failure as a veto would make a broken hook
//! indistinguishable from a deliberate refusal.
//!
//! [`PostToolUse`]: HookEvent::PostToolUse

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use globset::{Glob, GlobMatcher};
use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::warn;

// ========================================================================
// Constants
// ========================================================================

/// The exit status that means "block this tool call".
const BLOCK_STATUS: i32 = 2;

/// How long a hook may take before it is abandoned.
///
/// A hook is on the critical path of every tool call it matches, so a hung one
/// hangs the agent. Abandoning it is the safe direction: the alternative is a
/// session that stops responding with no indication why.
const HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// How much hook output reaches the model.
///
/// A formatter that prints its diff, or a linter run over a whole tree, can emit
/// far more than the tool result it is annotating.
const MAX_OUTPUT_BYTES: usize = 8_000;

// ========================================================================
// Data Structures
// ========================================================================

/// Every hook configured for a session, indexed by when it fires.
#[derive(Default)]
pub struct HookSet {
    hooks: HashMap<HookEvent, Vec<Hook>>,
}

/// One configured hook.
pub struct Hook {
    /// The command line, run through the platform shell so a hook can be a
    /// pipeline without the config having to describe one.
    command: String,
    /// Which tools this hook fires for. `None` fires for every tool, and for
    /// the events that have no tool at all.
    matcher: Option<GlobMatcher>,
}

/// When a hook fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    /// After a tool ran, with its outcome. Output is appended to the result the
    /// model sees.
    PostToolUse,
    /// Before a tool runs. A block prevents it from running at all.
    PreToolUse,
    /// When the turn is about to end.
    Stop,
}

/// What the hooks had to say about one tool call.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct HookVerdict {
    /// Why the call was blocked, if it was.
    pub blocked: Option<String>,
    /// Output to show the model, from hooks that let the call proceed.
    pub context: String,
}

/// The payload a hook reads on stdin.
#[derive(Serialize)]
struct HookInput<'a> {
    /// The tool's arguments, for the events that have a tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    args: Option<&'a Value>,
    cwd: &'a Path,
    event: HookEvent,
    /// The tool's result, for [`HookEvent::PostToolUse`].
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool: Option<&'a str>,
}

// ========================================================================
// HookSet Implementations
// ========================================================================

impl HookSet {
    /// Add a hook firing on `event`, for tools matching `pattern`.
    ///
    /// An unparseable pattern is rejected here rather than silently matching
    /// nothing, because a hook that never fires looks exactly like a hook that
    /// approves of everything.
    pub fn add(
        &mut self,
        event: HookEvent,
        command: String,
        pattern: Option<&str>,
    ) -> Result<(), globset::Error> {
        let matcher = pattern
            .map(Glob::new)
            .transpose()?
            .map(|g| g.compile_matcher());
        self.hooks
            .entry(event)
            .or_default()
            .push(Hook { command, matcher });
        Ok(())
    }

    /// Whether any hook is configured for `event`, so the loop can skip the
    /// work of assembling a payload nothing will read.
    pub fn has(&self, event: HookEvent) -> bool {
        self.hooks
            .get(&event)
            .is_some_and(|hooks| !hooks.is_empty())
    }

    /// Run every hook matching `tool` for `event`, in configuration order.
    ///
    /// Stops at the first block: once a call is refused, running the rest is
    /// work whose output nothing will read.
    pub async fn run(
        &self,
        event: HookEvent,
        tool: Option<&str>,
        args: Option<&Value>,
        result: Option<&str>,
        cwd: &Path,
    ) -> HookVerdict {
        let mut verdict = HookVerdict::default();
        let Some(hooks) = self.hooks.get(&event) else {
            return verdict;
        };
        let input = HookInput {
            args,
            cwd,
            event,
            result,
            tool,
        };
        let payload = serde_json::to_vec(&input).unwrap_or_default();

        for hook in hooks.iter().filter(|hook| hook.matches(tool)) {
            match hook.run(&payload, cwd).await {
                Some(HookOutput::Blocked(reason)) => {
                    verdict.blocked = Some(reason);
                    return verdict;
                }
                Some(HookOutput::Proceed(output)) => verdict.absorb(&output),
                None => {}
            }
        }
        verdict
    }
}

// ========================================================================
// HookVerdict Implementations
// ========================================================================

impl HookVerdict {
    /// Append one hook's output to what earlier hooks said.
    ///
    /// Several hooks can match one tool, and their notes are concatenated. The
    /// separator is only inserted between them, so a single hook's output does
    /// not arrive with a leading blank line.
    fn absorb(&mut self, output: &str) {
        if output.is_empty() {
            return;
        }
        if !self.context.is_empty() {
            self.context.push('\n');
        }
        self.context.push_str(output);
    }
}

// ========================================================================
// Hook Implementations
// ========================================================================

impl Hook {
    /// Whether this hook fires for `tool`.
    fn matches(&self, tool: Option<&str>) -> bool {
        match (&self.matcher, tool) {
            (None, _) => true,
            (Some(matcher), Some(tool)) => matcher.is_match(tool),
            // A hook that names tools has nothing to say about an event that
            // has none.
            (Some(_), None) => false,
        }
    }

    /// Run the hook, returning what it decided, or `None` if it failed in a way
    /// that is not the tool's business.
    async fn run(&self, payload: &[u8], cwd: &Path) -> Option<HookOutput> {
        let (program, flag) = shell();
        let mut child = Command::new(program)
            .arg(flag)
            .arg(&self.command)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .inspect_err(|error| warn!("hook `{}` did not start: {error}", self.command))
            .ok()?;

        if let Some(mut stdin) = child.stdin.take() {
            // A hook that does not read stdin closes the pipe, and writing to a
            // closed pipe is an error about the hook's manners, not a failure.
            let _ = stdin.write_all(payload).await;
        }

        let output = match timeout(HOOK_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => {
                warn!("hook `{}` failed: {error}", self.command);
                return None;
            }
            Err(_elapsed) => {
                warn!(
                    "hook `{}` exceeded {HOOK_TIMEOUT:?} and was abandoned",
                    self.command
                );
                return None;
            }
        };

        match output.status.code() {
            Some(0) => Some(HookOutput::Proceed(clamp(&output.stdout))),
            Some(BLOCK_STATUS) => Some(HookOutput::Blocked(clamp(&output.stderr))),
            other => {
                warn!(
                    "hook `{}` exited with {other:?}: {}",
                    self.command,
                    clamp(&output.stderr)
                );
                None
            }
        }
    }
}

// ========================================================================
// Functions
// ========================================================================

/// The shell a hook command line is run through.
fn shell() -> (&'static str, &'static str) {
    if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    }
}

/// Hook output as text, trimmed and capped.
fn clamp(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    if text.len() <= MAX_OUTPUT_BYTES {
        return text.to_string();
    }
    let mut end = MAX_OUTPUT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[hook output truncated]", &text[..end])
}

// ========================================================================
// Data Structures
// ========================================================================

/// What one hook decided.
enum HookOutput {
    Blocked(String),
    Proceed(String),
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn cwd() -> &'static Path {
        Path::new(".")
    }

    #[tokio::test]
    async fn a_hook_exiting_two_blocks_the_call_with_its_stderr() {
        let mut hooks = HookSet::default();
        hooks
            .add(
                HookEvent::PreToolUse,
                "echo 'no edits during a freeze' >&2; exit 2".into(),
                None,
            )
            .expect("no pattern to compile");
        let verdict = hooks
            .run(HookEvent::PreToolUse, Some("edit"), None, None, cwd())
            .await;
        assert_eq!(
            verdict.blocked.as_deref(),
            Some("no edits during a freeze"),
            "a block must carry the reason to the model"
        );
    }

    #[tokio::test]
    async fn a_hook_exiting_nonzero_for_other_reasons_does_not_block() {
        // The distinction that keeps a broken hook from halting the agent: a
        // command that is not installed exits 127, and that is not a veto.
        let mut hooks = HookSet::default();
        hooks
            .add(
                HookEvent::PreToolUse,
                "definitely-not-a-real-command-xyz".into(),
                None,
            )
            .expect("no pattern to compile");
        let verdict = hooks
            .run(HookEvent::PreToolUse, Some("edit"), None, None, cwd())
            .await;
        assert_eq!(verdict.blocked, None, "a failing hook is not a refusal");
    }

    #[tokio::test]
    async fn stdout_from_a_passing_hook_becomes_context() {
        let mut hooks = HookSet::default();
        hooks
            .add(
                HookEvent::PostToolUse,
                "echo 'formatted 2 files'".into(),
                None,
            )
            .expect("no pattern to compile");
        let verdict = hooks
            .run(HookEvent::PostToolUse, Some("edit"), None, None, cwd())
            .await;
        assert_eq!(verdict.context, "formatted 2 files");
        assert_eq!(verdict.blocked, None);
    }

    #[tokio::test]
    async fn a_matcher_limits_the_hook_to_the_tools_it_names() {
        let mut hooks = HookSet::default();
        hooks
            .add(HookEvent::PreToolUse, "exit 2".into(), Some("edit"))
            .expect("`edit` compiles");
        let blocked = hooks
            .run(HookEvent::PreToolUse, Some("edit"), None, None, cwd())
            .await;
        let untouched = hooks
            .run(HookEvent::PreToolUse, Some("read"), None, None, cwd())
            .await;
        assert!(blocked.blocked.is_some(), "the named tool is screened");
        assert_eq!(
            untouched.blocked, None,
            "a hook scoped to `edit` must not screen `read`"
        );
    }

    #[tokio::test]
    async fn the_payload_reaches_the_hook_on_stdin() {
        // Without this the hook is a trigger with no information: it knows
        // something happened but not to what.
        let mut hooks = HookSet::default();
        hooks
            .add(HookEvent::PreToolUse, "cat".into(), None)
            .expect("no pattern to compile");
        let args = json!({ "path": "src/lib.rs" });
        let verdict = hooks
            .run(
                HookEvent::PreToolUse,
                Some("edit"),
                Some(&args),
                None,
                cwd(),
            )
            .await;
        assert!(verdict.context.contains("src/lib.rs"), "{verdict:?}");
        assert!(verdict.context.contains("pre_tool_use"), "{verdict:?}");
    }

    #[tokio::test]
    async fn the_first_block_stops_the_chain() {
        let mut hooks = HookSet::default();
        hooks
            .add(HookEvent::PreToolUse, "echo first >&2; exit 2".into(), None)
            .expect("no pattern to compile");
        hooks
            .add(HookEvent::PreToolUse, "echo second".into(), None)
            .expect("no pattern to compile");
        let verdict = hooks
            .run(HookEvent::PreToolUse, Some("edit"), None, None, cwd())
            .await;
        assert_eq!(verdict.blocked.as_deref(), Some("first"));
        assert!(
            verdict.context.is_empty(),
            "hooks after a block should not have run: {verdict:?}"
        );
    }

    #[test]
    fn an_unparseable_pattern_is_rejected_rather_than_matching_nothing() {
        // A hook that never fires is indistinguishable from one that approves of
        // everything, which is the worst possible failure for a security gate.
        let mut hooks = HookSet::default();
        assert!(
            hooks
                .add(HookEvent::PreToolUse, "true".into(), Some("edit["))
                .is_err()
        );
    }
}

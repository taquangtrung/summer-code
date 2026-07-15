//! `!command`: run a shell line yourself, and hand its output to the agent.
//!
//! The line is the user's, not the agent's, so it runs as the user would run
//! it at their own prompt: in the session's directory, without a permission
//! prompt, and without going through the policy that governs what the agent
//! may do. What the agent gets is the output, attached to the next message
//! sent, so a fact the user can produce in one command costs no model round
//! trip to discover.
//!
//! The output rides on the user message rather than being injected into the
//! engine's history on its own, so it is journaled, folded on resume and
//! compacted exactly like anything else the user wrote. The envelope is
//! stripped back off for display, so the transcript shows what was typed.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tools::ShellSpec;

use crate::tui::state::ToolStatus;

// ========================================================================
// Constants
// ========================================================================

/// How long a `!` command may run before it is killed. Long enough for a full
/// test suite; a command meant to run forever belongs in another terminal.
const SHELL_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The most output, in bytes, kept from one command. The tail is what is kept,
/// since a build or a test run says what went wrong at the end.
const MAX_OUTPUT_BYTES: usize = 16 * 1024;

const CONTEXT_OPEN: &str = "<shell-context>\n";
const CONTEXT_CLOSE: &str = "\n</shell-context>\n\n";

// ========================================================================
// Data Structures
// ========================================================================

/// One finished `!` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellRun {
    pub command: String,
    /// How it ended: an exit code, a signal, or the timeout.
    pub end: ShellEnd,
    /// Standard output then standard error, cut to its last
    /// [`MAX_OUTPUT_BYTES`].
    pub output: String,
}

/// How a `!` command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellEnd {
    /// It exited with this code, or `None` when a signal ended it.
    Exited(Option<i32>),
    /// It could not be started at all.
    NotStarted,
    /// It ran past [`SHELL_TIMEOUT`] and was killed.
    TimedOut,
}

// ========================================================================
// ShellRun Implementations
// ========================================================================

impl ShellRun {
    /// The transcript status this run shows with.
    pub fn status(&self) -> ToolStatus {
        match self.end {
            ShellEnd::Exited(Some(0)) => ToolStatus::Done,
            ShellEnd::Exited(_) | ShellEnd::NotStarted => ToolStatus::Failed,
            ShellEnd::TimedOut => ToolStatus::TimedOut,
        }
    }

    /// The run as the agent will read it on the next message.
    pub fn context_block(&self) -> String {
        let end = match self.end {
            ShellEnd::Exited(Some(code)) => format!("exit code {code}"),
            ShellEnd::Exited(None) => "killed by a signal".to_string(),
            ShellEnd::NotStarted => "could not be started".to_string(),
            ShellEnd::TimedOut => "timed out and was killed".to_string(),
        };
        format!(
            "The user ran `{}` in the session's directory ({end}). Output:\n{}",
            self.command,
            self.output.trim_end()
        )
    }
}

// ========================================================================
// Functions
// ========================================================================

/// The command a submitted line asks to run, or `None` when it is not a `!`
/// line or names nothing.
pub fn shell_command(line: &str) -> Option<&str> {
    let command = line.trim_start().strip_prefix('!')?.trim();
    (!command.is_empty()).then_some(command)
}

/// Run `command` with `shell` in `cwd`, killed at [`SHELL_TIMEOUT`] or when the
/// future is dropped, which is how Esc stops it.
pub async fn run_shell(command: String, cwd: PathBuf, shell: ShellSpec) -> ShellRun {
    let child = tokio::process::Command::new(&shell.program)
        .args(shell.argv(&command))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(error) => {
            return ShellRun {
                command,
                end: ShellEnd::NotStarted,
                output: error.to_string(),
            };
        }
    };
    match tokio::time::timeout(SHELL_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            ShellRun {
                command,
                end: ShellEnd::Exited(output.status.code()),
                output: tail(&text),
            }
        }
        Ok(Err(error)) => ShellRun {
            command,
            end: ShellEnd::NotStarted,
            output: error.to_string(),
        },
        Err(_) => ShellRun {
            command,
            end: ShellEnd::TimedOut,
            output: String::new(),
        },
    }
}

/// `text` cut to its last [`MAX_OUTPUT_BYTES`], on a character boundary, with
/// a note saying how much was dropped.
fn tail(text: &str) -> String {
    if text.len() <= MAX_OUTPUT_BYTES {
        return text.to_string();
    }
    let mut start = text.len() - MAX_OUTPUT_BYTES;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[… {start} earlier bytes cut]\n{}", &text[start..])
}

/// `text` with the pending command outputs in front of it, for the engine.
pub fn with_shell_context(context: &[String], text: &str) -> String {
    if context.is_empty() {
        return text.to_string();
    }
    format!(
        "{CONTEXT_OPEN}{}{CONTEXT_CLOSE}{text}",
        context.join("\n\n")
    )
}

/// What the user typed, with the command outputs [`with_shell_context`] put in
/// front of it taken back off, for the transcript.
pub fn strip_shell_context(text: &str) -> &str {
    text.strip_prefix(CONTEXT_OPEN)
        .and_then(|rest| rest.split_once(CONTEXT_CLOSE))
        .map_or(text, |(_, typed)| typed)
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_only_a_bang_line_with_a_command_runs() {
        assert_eq!(shell_command("!cargo test"), Some("cargo test"));
        assert_eq!(shell_command("  ! ls -la "), Some("ls -la"));
        assert_eq!(shell_command("!"), None, "nothing to run");
        assert_eq!(
            shell_command("why does !x fail"),
            None,
            "a ! mid-line is prose"
        );
    }

    #[test]
    fn test_the_context_goes_to_the_engine_and_comes_off_for_display() {
        let sent = with_shell_context(&["ran ls".to_string()], "now fix it");
        assert!(sent.contains("ran ls") && sent.ends_with("now fix it"));
        assert_eq!(strip_shell_context(&sent), "now fix it");
        assert_eq!(with_shell_context(&[], "plain"), "plain");
        assert_eq!(strip_shell_context("plain"), "plain");
    }

    #[test]
    fn test_long_output_keeps_its_tail() {
        // A build says what went wrong at the end.
        let text = format!("{}the error", "x".repeat(MAX_OUTPUT_BYTES * 2));
        let kept = tail(&text);
        assert!(kept.ends_with("the error"));
        assert!(kept.len() < MAX_OUTPUT_BYTES + 64, "{}", kept.len());
        assert!(kept.starts_with("[… "), "says that it cut");
    }

    #[tokio::test]
    async fn test_a_command_runs_in_the_session_directory_and_reports_its_exit() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("marker.txt"), "").expect("marker");
        let shell = ShellSpec::platform_default();
        let listing = if cfg!(windows) {
            "dir /b"
        } else {
            "ls; exit 3"
        };
        let run = run_shell(listing.to_string(), dir.path().to_path_buf(), shell).await;
        assert!(run.output.contains("marker.txt"), "{}", run.output);
        if !cfg!(windows) {
            assert_eq!(run.end, ShellEnd::Exited(Some(3)));
            assert_eq!(run.status(), ToolStatus::Failed);
        }
    }
}

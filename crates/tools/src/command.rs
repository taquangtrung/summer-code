//! Running an authorized child, and rendering what it did for the model.
//!
//! **One seam for every exec-based tool.** `grep`, `shell` and `git` each used to
//! do their own wait, and each got a different subset right: one raced
//! cancellation and one blocked until the dispatcher's timeout, one surfaced
//! stderr and one dropped it, and none reported the exit status, so a `git
//! commit` with nothing staged was reported to the model as a success with no
//! output. The divergence was the bug; a single function is the fix.
//!
//! Capturing, capping and killing belong to [`sandbox::AuthorizedCommand`]
//! rather than here: they are properties of the authorization, not of how a tool
//! chooses to phrase the result. What is left here is the phrasing.

use journal::Outcome;
use sandbox::{CommandOutcome, CommandOutput, ToolCx};

// ========================================================================
// Functions
// ========================================================================

/// Spawn `program` through the broker and run it to completion.
pub(crate) async fn run_command(cx: &ToolCx, program: &str, args: &[String]) -> Outcome {
    let handle = match cx.broker.exec(program, args).await {
        Ok(handle) => handle,
        Err(denied) => {
            return Outcome::Denied {
                reason: denied.reason,
            };
        }
    };
    match handle.run(&cx.cancel).await {
        Ok(CommandOutcome::Exited(output)) => Outcome::Ok {
            content: format_output(&output),
        },
        Ok(CommandOutcome::Interrupted) => Outcome::Err {
            message: "interrupted".into(),
        },
        Err(error) => Outcome::Err {
            message: format!("{program}: {error}"),
        },
    }
}

/// Render a finished command: its output, then how it ended.
///
/// **A non-zero exit stays an `Outcome::Ok` carrying its status**, rather than
/// becoming `Outcome::Err`. `Err` holds only a message, so mapping onto it would
/// discard the output in exactly the cases the model most needs it: a failing
/// `cargo test` exits 101 and its entire value is in what it printed. `rg`
/// exiting 1 for "no matches" is not an error either. The status is reported as a
/// fact and the model judges it.
fn format_output(output: &CommandOutput) -> String {
    let mut content = String::from_utf8_lossy(&output.stdout.bytes).into_owned();
    if !output.stderr.bytes.is_empty() {
        push_section(&mut content, "[stderr]");
        content.push_str(&String::from_utf8_lossy(&output.stderr.bytes));
    }
    if !output.success() {
        let status = match output.code {
            Some(code) => format!("[exit status {code}]"),
            None => "[killed by a signal]".to_string(),
        };
        push_section(&mut content, &status);
    }
    content
}

fn push_section(content: &mut String, heading: &str) {
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(heading);
    content.push('\n');
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use sandbox::CapturedStream;

    use super::*;

    fn output(code: Option<i32>, stdout: &str, stderr: &str) -> CommandOutput {
        CommandOutput {
            code,
            stderr: CapturedStream {
                bytes: stderr.as_bytes().to_vec(),
                elided: 0,
            },
            stdout: CapturedStream {
                bytes: stdout.as_bytes().to_vec(),
                elided: 0,
            },
        }
    }

    #[test]
    fn a_failing_command_reports_its_status_and_keeps_its_output() {
        // The `git commit` with nothing staged: it exits 1 and explains itself
        // on stderr. Reporting this as a bare success with empty content is what
        // let the model believe the commit landed.
        let rendered = format_output(&output(Some(1), "", "nothing to commit\n"));
        assert!(rendered.contains("nothing to commit"), "{rendered}");
        assert!(rendered.contains("exit status 1"), "{rendered}");
    }

    #[test]
    fn a_successful_command_is_just_its_output() {
        // No status noise on the common path, or every result carries a line the
        // model has to learn to ignore.
        assert_eq!(format_output(&output(Some(0), "ok\n", "")), "ok\n");
    }

    #[test]
    fn a_signal_death_is_not_success() {
        // `code` is `None` when a signal ended the child; treating "no code" as
        // zero would report an OOM-killed build as a clean run.
        let rendered = format_output(&output(None, "partial\n", ""));
        assert!(rendered.contains("killed by a signal"), "{rendered}");
    }
}

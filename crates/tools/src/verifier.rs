//! Fast in-loop static compiler verifier.
//!
//! Executes project static diagnostic checks (`cargo check`, `tsc`, `ruff`)
//! after code edits to provide immediate diagnostic error feedback for model
//! self-correction.
//!
//! **Errors only, never warnings.** Dead-code and unused warnings used to count
//! as failures, and the check covers the whole project, so a repository that
//! already carried one `unused` warning failed the gate after every edit the
//! model made, however correct, and sent it back to "fix" code it never
//! touched. A warning is the project's own business; an error is something
//! this turn may have broken.
//!
//! **Bounded on both ends.** Each check gets [`VERIFY_TIMEOUT`], after which it
//! is killed and the gate passes rather than holding the turn on a cold build,
//! and the feedback is capped at [`MAX_FEEDBACK_ERRORS`] lines and
//! [`MAX_FEEDBACK_BYTES`], so a cascade of errors from one bad edit cannot fill
//! the context it is meant to help.
//!
//! **Which checks run is decided by what the turn edited, not by what the
//! repository root contains.** The first version walked an `if` chain over
//! marker files (`Cargo.toml`, then `tsconfig.json`, then `go.mod`, then
//! `pyproject.toml`) and ran the first match. Two things followed from that, and
//! both were wrong. A `.py` script written into a Rust repository got `cargo
//! check` and no Python check at all, which is the common case for a helper
//! script. And a polyglot repository only ever verified whichever language its
//! root happened to name first, so the other half of the tree was unverified
//! forever.
//!
//! A language is checked when the turn touched a file of that language, and it
//! is checked with the project's own tool when the project claims that language,
//! or against the edited files alone when it does not. Every summary the turn
//! produces is merged, because a turn that broke the Rust and the Python halves
//! should be told about both.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use sandbox::{AuthorizedCommand, CommandOutcome, CommandOutput, ToolCx};

// ========================================================================
// Constants
// ========================================================================

/// How long one check may run before it is killed and treated as clean. A
/// cold `cargo check` of a large workspace can take minutes, and a gate that
/// waits that long after every edit costs more than the errors it catches.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(120);

/// The most diagnostic lines one round of feedback carries.
const MAX_FEEDBACK_ERRORS: usize = 50;

/// The most bytes of diagnostics one round of feedback carries.
const MAX_FEEDBACK_BYTES: usize = 8 * 1024;

/// How much of a failing check's raw output is kept when no line of it
/// parsed as a diagnostic.
const RAW_OUTPUT_CHARS: usize = 1000;

// ========================================================================
// Data Structures
// ========================================================================

/// A language a checker covers. Ordered so a turn touching several reports them
/// the same way twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Language {
    Go,
    Python,
    Rust,
    TypeScript,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticSummary {
    pub command: String,
    pub errors: Vec<String>,
}

impl DiagnosticSummary {
    /// The diagnostics as a message to the model, capped at 50 lines and
    /// 8 KiB, with a count
    /// of what was left out. The first errors are kept: in a cascade they are
    /// the cause, and the rest are its echoes.
    pub fn format_feedback(&self) -> String {
        let mut msg = format!(
            "Static verification command `{}` returned the following diagnostics:\n\n",
            self.command
        );
        let mut shown = 0;
        let mut bytes = 0;
        for err in self.errors.iter().take(MAX_FEEDBACK_ERRORS) {
            if bytes + err.len() > MAX_FEEDBACK_BYTES && shown > 0 {
                break;
            }
            msg.push_str("----------------------------------------\n");
            msg.push_str(err);
            msg.push('\n');
            bytes += err.len();
            shown += 1;
        }
        let hidden = self.errors.len() - shown;
        if hidden > 0 {
            msg.push_str(&format!("... and {hidden} more\n"));
        }
        msg.push_str("\nPlease fix these errors in your next turn.");
        msg
    }
}

// ========================================================================
// Functions
// ========================================================================

/// Run the static checks the turn's edits call for, merged into one summary.
///
/// `edited` is the paths the turn actually wrote, as the model named them.
/// Nothing runs for a turn that edited only files no checker covers, which is
/// the common case for a README or a JSON fixture and is why the check is worth
/// keying on the edits at all.
pub async fn verify_code_edits(
    cx: &ToolCx,
    root: &Path,
    edited: &[String],
) -> Option<DiagnosticSummary> {
    let mut summaries = Vec::new();
    for language in languages_of(edited) {
        let files = files_of(edited, language);
        if let Some(summary) = match language {
            Language::Go => verify_go(cx, root).await,
            Language::Python => verify_python(cx, root, &files).await,
            Language::Rust => verify_rust(cx, root).await,
            Language::TypeScript => verify_typescript(cx, root).await,
        } {
            summaries.push(summary);
        }
    }
    merge(summaries)
}

/// The languages a turn's edits touched, deduplicated and in a stable order.
fn languages_of(edited: &[String]) -> BTreeSet<Language> {
    edited.iter().filter_map(|path| language_of(path)).collect()
}

/// A path's language, by extension. `None` for anything no checker covers.
fn language_of(path: &str) -> Option<Language> {
    match Path::new(path).extension()?.to_str()? {
        "go" => Some(Language::Go),
        "py" | "pyi" => Some(Language::Python),
        "rs" => Some(Language::Rust),
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => Some(Language::TypeScript),
        _ => None,
    }
}

/// The edited paths belonging to one language, safe to pass as arguments.
fn files_of(edited: &[String], language: Language) -> Vec<String> {
    edited
        .iter()
        .filter(|path| language_of(path) == Some(language))
        .map(|path| argv_safe(path))
        .collect()
}

/// A path that cannot be read as a flag by whatever it is handed to.
///
/// **The path came from the model.** It is the `path` argument of an `edit` the
/// broker allowed, so a file really can be called `-x.py`, and `ruff check
/// -x.py` reads that as options rather than as a file: `--fix` would rewrite the
/// tree, `--config` would point the linter somewhere else. Prefixing `./` is the
/// portable answer, and it is better than a `--` separator here because the
/// fallback is `python -m py_compile`, whose argument parser has no `--` and
/// would try to compile a file of that name.
///
/// An absolute path already cannot look like a flag, and is left alone.
fn argv_safe(path: &str) -> String {
    if path.starts_with('-') {
        format!("./{path}")
    } else {
        path.to_string()
    }
}

/// Run a check to completion, or kill it at [`VERIFY_TIMEOUT`].
///
/// `None` when it was interrupted or timed out: either way there is no verdict,
/// and no verdict is a pass rather than a reason to send the model back. The
/// kill goes through cancellation rather than dropping the future, so the
/// child is reaped and not orphaned.
async fn run_bounded(cx: &ToolCx, handle: AuthorizedCommand) -> Option<CommandOutput> {
    let cancel = cx.cancel.child_token();
    let run = handle.run(&cancel);
    tokio::pin!(run);
    let outcome = tokio::select! {
        outcome = &mut run => outcome,
        () = tokio::time::sleep(VERIFY_TIMEOUT) => {
            tracing::warn!("static verification timed out; passing the gate");
            cancel.cancel();
            run.await
        }
    };
    match outcome {
        Ok(CommandOutcome::Exited(output)) => Some(output),
        Ok(CommandOutcome::Interrupted) | Err(_) => None,
    }
}

/// The errors in `cargo check --message-format=short` output. Warnings are
/// not errors, whatever they say; see the module docs.
fn rust_errors(output: &str) -> Vec<String> {
    output
        .lines()
        .filter(|line| {
            line.contains("error[E") || line.starts_with("error:") || line.contains(": error:")
        })
        .map(str::to_string)
        .collect()
}

/// The errors in `tsc --noEmit` output.
fn typescript_errors(output: &str) -> Vec<String> {
    output
        .lines()
        .filter(|line| line.contains("error TS"))
        .map(str::to_string)
        .collect()
}

/// One summary from several, or `None` when every check was clean.
///
/// The commands are joined rather than one winning: a turn that broke two
/// languages has two things to fix, and a feedback message naming one of them
/// sends the model back for a second round it did not need.
fn merge(summaries: Vec<DiagnosticSummary>) -> Option<DiagnosticSummary> {
    if summaries.is_empty() {
        return None;
    }
    let command = summaries
        .iter()
        .map(|summary| summary.command.as_str())
        .collect::<Vec<_>>()
        .join(" && ");
    let errors = summaries
        .into_iter()
        .flat_map(|summary| summary.errors)
        .collect();
    Some(DiagnosticSummary { command, errors })
}

/// `cargo check`, and only inside a cargo project.
///
/// A `.rs` file edited somewhere with no `Cargo.toml` has no crate to check: the
/// compiler needs the manifest, not the file. Nothing runs, and that is the
/// honest answer rather than an error about a missing manifest the model would
/// try to fix.
async fn verify_rust(cx: &ToolCx, root: &Path) -> Option<DiagnosticSummary> {
    if !root.join("Cargo.toml").exists() {
        return None;
    }
    let cmd = "cargo check --message-format=short";
    let args = vec!["check".to_string(), "--message-format=short".to_string()];
    let Ok(handle) = cx.broker.exec("cargo", &args).await else {
        return None;
    };

    let output = run_bounded(cx, handle).await?;

    let stderr = String::from_utf8_lossy(&output.stderr.bytes);
    let stdout = String::from_utf8_lossy(&output.stdout.bytes);

    let mut errors = rust_errors(&stderr);
    errors.extend(rust_errors(&stdout));

    if !output.success() && errors.is_empty() && !stderr.trim().is_empty() {
        errors.push(stderr.trim().chars().take(RAW_OUTPUT_CHARS).collect());
    }

    if errors.is_empty() {
        None
    } else {
        Some(DiagnosticSummary {
            command: cmd.to_string(),
            errors,
        })
    }
}

/// `tsc --noEmit`, and only where a `tsconfig.json` says how to compile.
async fn verify_typescript(cx: &ToolCx, root: &Path) -> Option<DiagnosticSummary> {
    if !root.join("tsconfig.json").exists() {
        return None;
    }
    let cmd = "npx tsc --noEmit";
    let args = vec!["tsc".to_string(), "--noEmit".to_string()];
    let Ok(handle) = cx.broker.exec("npx", &args).await else {
        return None;
    };

    let output = run_bounded(cx, handle).await?;

    let stdout = String::from_utf8_lossy(&output.stdout.bytes);
    let mut errors = typescript_errors(&stdout);

    if !output.success() && errors.is_empty() && !stdout.trim().is_empty() {
        errors.push(stdout.trim().chars().take(RAW_OUTPUT_CHARS).collect());
    }

    if errors.is_empty() {
        None
    } else {
        Some(DiagnosticSummary {
            command: cmd.to_string(),
            errors,
        })
    }
}

/// `ruff`, falling back to the interpreter's own syntax check.
///
/// **Scoped to the whole tree in a Python project, and to the edited files
/// anywhere else.** A script dropped into a Rust repository still gets checked,
/// which is the case the old root-keyed dispatch missed entirely, but linting
/// that repository's every `.py` file because one script was written would
/// report other people's findings as this turn's.
async fn verify_python(cx: &ToolCx, root: &Path, files: &[String]) -> Option<DiagnosticSummary> {
    let project = root.join("pyproject.toml").exists() || root.join("requirements.txt").exists();
    let targets: Vec<String> = if project {
        vec![".".to_string()]
    } else {
        files.to_vec()
    };
    if targets.is_empty() {
        return None;
    }
    let cmd = format!("ruff check {}", targets.join(" "));
    let mut args = vec!["check".to_string()];
    args.extend(targets.iter().cloned());
    let handle = if let Ok(h) = cx.broker.exec("ruff", &args).await {
        h
    } else {
        // `py_compile` over named files, `compileall` over a tree: the first
        // reports the file it could not parse, which is what a scoped check owes
        // the reader.
        let mut py_args = vec!["-m".to_string()];
        if project {
            py_args.extend(["compileall".to_string(), "-q".to_string(), ".".to_string()]);
        } else {
            py_args.push("py_compile".to_string());
            py_args.extend(targets.iter().cloned());
        }
        let Ok(h) = cx.broker.exec("python3", &py_args).await else {
            return None;
        };
        h
    };

    let output = run_bounded(cx, handle).await?;

    let stderr = String::from_utf8_lossy(&output.stderr.bytes);
    let stdout = String::from_utf8_lossy(&output.stdout.bytes);
    let mut errors: Vec<String> = Vec::new();

    for line in stderr.lines().chain(stdout.lines()) {
        if line.contains("SyntaxError")
            || line.contains("IndentationError")
            || line.contains("error:")
            || line.contains("Error:")
        {
            errors.push(line.to_string());
        }
    }

    if !output.success() && errors.is_empty() && !stderr.trim().is_empty() {
        errors.push(stderr.trim().chars().take(RAW_OUTPUT_CHARS).collect());
    }

    if errors.is_empty() {
        None
    } else {
        Some(DiagnosticSummary {
            command: cmd,
            errors,
        })
    }
}

/// `go vet ./...`, and only inside a module.
async fn verify_go(cx: &ToolCx, root: &Path) -> Option<DiagnosticSummary> {
    if !root.join("go.mod").exists() {
        return None;
    }
    let cmd = "go vet ./...";
    let args = vec!["vet".to_string(), "./...".to_string()];
    let Ok(handle) = cx.broker.exec("go", &args).await else {
        return None;
    };

    let output = run_bounded(cx, handle).await?;

    let stderr = String::from_utf8_lossy(&output.stderr.bytes);
    let mut errors: Vec<String> = Vec::new();

    for line in stderr.lines() {
        if line.contains(".go:") {
            errors.push(line.to_string());
        }
    }

    if !output.success() && errors.is_empty() && !stderr.trim().is_empty() {
        errors.push(stderr.trim().chars().take(RAW_OUTPUT_CHARS).collect());
    }

    if errors.is_empty() {
        None
    } else {
        Some(DiagnosticSummary {
            command: cmd.to_string(),
            errors,
        })
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_a_path_is_routed_by_its_extension() {
        assert_eq!(language_of("src/main.rs"), Some(Language::Rust));
        assert_eq!(language_of("tools/fix.py"), Some(Language::Python));
        assert_eq!(language_of("web/app.tsx"), Some(Language::TypeScript));
        assert_eq!(language_of("cmd/main.go"), Some(Language::Go));
        // No checker covers these, so a turn that only touched them runs
        // nothing at all, which is the point of keying on the edits.
        assert_eq!(language_of("README.md"), None);
        assert_eq!(language_of("Makefile"), None);
    }

    #[test]
    fn test_a_polyglot_turn_reports_every_language_it_touched() {
        // The bug this replaces: the old dispatch walked marker files and ran
        // the first match, so a repo with a Cargo.toml never checked its Python
        // no matter what the turn edited.
        let edited = vec![
            "src/lib.rs".to_string(),
            "tools/fix.py".to_string(),
            "src/other.rs".to_string(),
        ];
        let languages: Vec<Language> = languages_of(&edited).into_iter().collect();
        assert_eq!(languages, vec![Language::Python, Language::Rust]);
        assert_eq!(files_of(&edited, Language::Python), vec!["tools/fix.py"]);
    }

    #[test]
    fn test_merging_keeps_every_finding_and_names_every_command() {
        // A turn that broke two languages has two things to fix. Reporting one
        // sends the model back for a round it did not need.
        let merged = merge(vec![
            DiagnosticSummary {
                command: "cargo check".into(),
                errors: vec!["e1".into()],
            },
            DiagnosticSummary {
                command: "ruff check fix.py".into(),
                errors: vec!["e2".into()],
            },
        ])
        .expect("two summaries merge");
        assert_eq!(merged.errors, vec!["e1", "e2"]);
        assert!(merged.command.contains("cargo check"), "{}", merged.command);
        assert!(merged.command.contains("ruff check"), "{}", merged.command);
    }

    #[test]
    fn test_an_edited_path_cannot_be_read_as_a_flag() {
        // The path is the model's own `edit` argument, so a file really can be
        // called `-x.py`. Passed through, `ruff check -x.py` reads it as options
        // rather than as a file, and `--fix` would rewrite the tree.
        let edited = vec!["-x.py".to_string(), "src/fine.py".to_string()];
        let files = files_of(&edited, Language::Python);
        assert_eq!(files, vec!["./-x.py", "src/fine.py"]);
        assert!(
            files.iter().all(|file| !file.starts_with('-')),
            "a target still parses as a flag: {files:?}"
        );
    }

    #[test]
    fn test_clean_checks_produce_no_summary() {
        assert!(merge(Vec::new()).is_none());
    }

    #[test]
    fn test_a_warning_is_not_an_error() {
        // A project that already carried one `unused` warning failed the gate
        // after every edit, and the model was sent back to "fix" code it never
        // touched.
        let output = "src/lib.rs:1:4: warning: function `unused_foo` is never used\n\
                      src/lib.rs:3:9: warning: unused variable: `x`\n\
                      src/lib.rs:7:5: error[E0425]: cannot find value `y` in this scope\n\
                      error: could not compile `demo` (lib) due to 1 previous error\n";
        assert_eq!(
            rust_errors(output),
            vec![
                "src/lib.rs:7:5: error[E0425]: cannot find value `y` in this scope",
                "error: could not compile `demo` (lib) due to 1 previous error",
            ]
        );
        assert!(
            typescript_errors(
                "a.ts(1,7): error TS6133: 'x' is declared but its value is never read.\n"
            )
            .len()
                == 1,
            "a TS error the project's own config raised is still an error"
        );
    }

    #[test]
    fn test_feedback_is_capped_and_says_what_it_left_out() {
        // One bad edit can cascade into hundreds of errors, and all of them
        // went into the context the feedback was meant to help.
        let summary = DiagnosticSummary {
            command: "cargo check".to_string(),
            errors: (0..MAX_FEEDBACK_ERRORS + 7)
                .map(|n| format!("error[E0425]: cannot find value `v{n}`"))
                .collect(),
        };
        let feedback = summary.format_feedback();
        assert!(
            feedback.contains("`v0`"),
            "the first error is the cause and is kept"
        );
        assert!(!feedback.contains(&format!("`v{MAX_FEEDBACK_ERRORS}`")));
        assert!(feedback.contains("... and 7 more"), "{feedback}");

        let huge = DiagnosticSummary {
            command: "cargo check".to_string(),
            errors: vec!["x".repeat(MAX_FEEDBACK_BYTES); 3],
        };
        assert!(
            huge.format_feedback().len() < 2 * MAX_FEEDBACK_BYTES,
            "one oversized error is shown, the rest are counted"
        );
    }
}

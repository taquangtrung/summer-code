//! The built-in tools: `read`, `edit`, `grep`, `shell`, `git`.
//!
//! Each loads its [`ToolSpec`] from a checked-in `tools/*.json` (the description
//! is how the model routes, so it is a tuning surface and lives in a file). All
//! agent-driven I/O goes through the broker: `read`/`edit` take a path handle,
//! `grep`/`shell` spawn through `broker.exec`, so nothing here names `std::fs`
//! or `std::process` directly.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use journal::Outcome;
use sandbox::{OpenError, ToolCx, resolve};
use serde_json::Value;

use crate::command::run_command;
use crate::shell::ShellSpec;
use query::outline;

use crate::tool::{Concurrency, Tool, ToolSpec};
use crate::vcs::Vcs;

// ========================================================================
// Constants
// ========================================================================

const EDIT_SCHEMA: &str = include_str!("../tools/edit.json");
const GIT_SCHEMA: &str = include_str!("../tools/git.json");
const GREP_SCHEMA: &str = include_str!("../tools/grep.json");
const READ_SCHEMA: &str = include_str!("../tools/read.json");
const SHELL_SCHEMA: &str = include_str!("../tools/shell.json");
const SYMBOLS_SCHEMA: &str = include_str!("../tools/symbols.json");

/// What `shell.json` writes where the shell's flag belongs.
const SHELL_FLAG_PLACEHOLDER: &str = "{flag}";

/// What `shell.json` writes where the shell's program name belongs.
const SHELL_PROGRAM_PLACEHOLDER: &str = "{program}";

/// What `shell.json` writes where [`SHELL_DEFAULT_TIMEOUT`] belongs.
const SHELL_DEFAULT_TIMEOUT_PLACEHOLDER: &str = "{default_timeout}";

/// What `shell.json` writes where [`SHELL_MAX_TIMEOUT`] belongs.
const SHELL_MAX_TIMEOUT_PLACEHOLDER: &str = "{max_timeout}";

/// How long a command line runs when the call names no `timeout_secs`.
///
/// **Not the dispatcher's two minutes**, which is sized for a read or a grep. A
/// `make check` or a workspace `cargo test` routinely runs past that, and a
/// build killed at the cap leaves the model working around the harness: it
/// redirects to a log file, polls it over several turns, and reads a truncated
/// tail. Sessions on this repository did exactly that.
const SHELL_DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// The most a call may ask for. Bounded so a stuck command the model believes
/// is slow still ends inside one sitting rather than holding the turn forever.
const SHELL_MAX_TIMEOUT: Duration = Duration::from_secs(1800);

/// The argument a call names its own timeout with.
const TIMEOUT_ARG: &str = "timeout_secs";

/// What `read.json` writes where the skeleton-mode sentence belongs. The
/// sentence lives in the source rather than the file so the off-variant needs
/// no second description to keep in step, the same arrangement `shell.json`
/// has with its shell name.
const SKELETON_NOTE: &str = " Supports mode='skeleton', which returns the file with function \
                          bodies elided and every signature, type definition and doc comment \
                          kept: a large token saving on a file you are consulting rather \
                          than editing. Skeleton mode needs a tree-sitter grammar (Rust, \
                          TypeScript, JavaScript, Python, Go, C, C++) and returns the file \
                          unchanged for any other language.";

/// What `read.json` writes where the skeleton-mode sentence belongs.
const SKELETON_NOTE_PLACEHOLDER: &str = "{skeleton_note}";

/// The most text one `read` hands back.
///
/// The same reasoning as the exec capture cap: the result lands in the context
/// window and stays there. A minified bundle or a vendored lockfile is a single
/// `read` away from ending the session, and the model has no way to know the
/// size before it asks. The cap plus `offset`/`limit` is what makes a large file
/// navigable instead of fatal.
const READ_CAP_BYTES: usize = 64 * 1024;

/// Lines returned when the model does not ask for a window.
const READ_DEFAULT_LINES: usize = 2000;

/// The widest line `grep` prints. A minified bundle is one line, and a single
/// match in it used to fill the whole output cap with one row.
const GREP_MAX_COLUMNS: u32 = 300;

/// The most context lines `grep` shows around a match.
const GREP_MAX_CONTEXT: u64 = 10;

/// Unchanged lines [`hunk_diff`] keeps around each change: `diff -u`'s default,
/// enough to place an edit in its function without reprinting the file.
const HUNK_CONTEXT_LINES: usize = 3;

// ========================================================================
// Data Structures
// ========================================================================

/// Read a file's contents. `skeleton` says whether `mode: "skeleton"` elides
/// bodies or reads the text it was asked for; the ablation harness is what
/// constructs it off.
pub struct Read {
    skeleton: bool,
    spec: ToolSpec,
}

/// Overwrite a file (whole-file rewrite; the edit-strategy choice is deferred to
/// a `bench` baseline).
pub struct Edit {
    spec: ToolSpec,
}

/// Search the working tree with ripgrep.
pub struct Grep {
    spec: ToolSpec,
}

/// Run a shell command line.
pub struct Shell {
    shell: ShellSpec,
    spec: ToolSpec,
}

/// Run a `git` subcommand through the [`crate::Vcs`] seam, so a golden test can
/// substitute the binary.
pub struct GitTool {
    spec: ToolSpec,
    vcs: Arc<dyn Vcs>,
}

/// Extract or search semantic AST code symbols.
pub struct Symbols {
    spec: ToolSpec,
}

impl Symbols {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::load(SYMBOLS_SCHEMA),
        }
    }
}

impl Default for Symbols {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for Symbols {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Outcome {
        let path_arg = args.get("path").and_then(Value::as_str);
        let query_arg = args.get("query").and_then(Value::as_str);
        let kind_arg = args.get("kind").and_then(Value::as_str);

        let target_path = path_arg
            .map(|p| resolve(&cx.cwd, Path::new(p)))
            .unwrap_or_else(|| cx.cwd.to_path_buf());

        let mut symbol_lines = Vec::new();
        if target_path.is_file() {
            scan_file_symbols(
                cx,
                &target_path,
                &cx.cwd,
                query_arg,
                kind_arg,
                &mut symbol_lines,
            )
            .await;
        } else if target_path.is_dir() {
            scan_dir_symbols(
                cx,
                &target_path,
                &cx.cwd,
                query_arg,
                kind_arg,
                &mut symbol_lines,
                0,
            )
            .await;
        } else {
            return Outcome::Err {
                message: format!("path `{}` does not exist", target_path.display()),
            };
        }

        if symbol_lines.is_empty() {
            return Outcome::Ok {
                content: "No matching code symbols found.".to_string(),
            };
        }

        if symbol_lines.len() > 200 {
            symbol_lines.truncate(200);
            symbol_lines.push("[showing top 200 matching symbols]".to_string());
        }

        Outcome::Ok {
            content: symbol_lines.join("\n"),
        }
    }
}

async fn scan_dir_symbols(
    cx: &ToolCx,
    dir: &Path,
    base: &Path,
    query: Option<&str>,
    kind: Option<&str>,
    out: &mut Vec<String>,
    depth: usize,
) {
    if depth > 6 || out.len() >= 200 {
        return;
    }
    let Ok(entries) = cx.broker.read_dir(dir).await else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "target" || name == "node_modules" || name == "vendor" {
            continue;
        }
        if path.is_dir() {
            Box::pin(scan_dir_symbols(
                cx,
                &path,
                base,
                query,
                kind,
                out,
                depth + 1,
            ))
            .await;
        } else if path.is_file() {
            scan_file_symbols(cx, &path, base, query, kind, out).await;
        }
    }
}

async fn scan_file_symbols(
    cx: &ToolCx,
    path: &Path,
    base: &Path,
    query: Option<&str>,
    kind: Option<&str>,
    out: &mut Vec<String>,
) {
    let Ok(handle) = cx.broker.read(path).await else {
        return;
    };
    let Ok(content) = handle.read_to_string() else {
        return;
    };
    let rel_path = path.strip_prefix(base).unwrap_or(path).to_string_lossy();

    let query_lower = query.map(str::to_lowercase);
    let kind_lower = kind.map(str::to_lowercase);

    for entry in outline(path, &content) {
        let label = entry.kind.label();
        let kind_matches = kind_lower
            .as_ref()
            .is_none_or(|wanted| label.eq_ignore_ascii_case(wanted));
        let query_matches = query_lower.as_ref().is_none_or(|wanted| {
            entry.name.to_lowercase().contains(wanted)
                || entry.signature.to_lowercase().contains(wanted)
                || entry
                    .parent
                    .as_ref()
                    .is_some_and(|p| p.to_lowercase().contains(wanted))
        });
        if !kind_matches || !query_matches {
            continue;
        }

        let location = if entry.line_start == entry.line_end {
            entry.line_start.to_string()
        } else {
            format!("{}-{}", entry.line_start, entry.line_end)
        };
        let formatted_sig = match &entry.parent {
            Some(parent) => format!("{parent}::{}", entry.signature),
            None => entry.signature.clone(),
        };
        out.push(format!("{rel_path}:{location}: [{label}] {formatted_sig}"));
    }
}

// ========================================================================
// Read Implementations
// ========================================================================

impl Read {
    pub fn new() -> Self {
        Self::with_skeleton(true)
    }

    /// The `read` tool with `mode: "skeleton"` on or off.
    ///
    /// Off, the tool stops advertising the mode and a read that asks for it
    /// answers with the plain text: the affordance is gone, not a trap that
    /// errors on a mode the description no longer mentions.
    pub fn with_skeleton(skeleton: bool) -> Self {
        let mut spec = ToolSpec::load(READ_SCHEMA);
        spec.description = spec.description.replace(
            SKELETON_NOTE_PLACEHOLDER,
            if skeleton { SKELETON_NOTE } else { "" },
        );
        Self { skeleton, spec }
    }
}

impl Default for Read {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for Read {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Outcome {
        let Some(path) = args.get("path").and_then(Value::as_str) else {
            return missing("path");
        };
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(1);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(READ_DEFAULT_LINES as u64);
        let full = resolve(&cx.cwd, Path::new(path));
        match cx.broker.read(&full).await {
            Ok(handle) => match handle.read_to_string() {
                Ok(content) => {
                    let mode = args.get("mode").and_then(Value::as_str).unwrap_or("text");
                    if mode == "skeleton" && self.skeleton {
                        // The extension picks the grammar; there is no second
                        // language table here to drift out of step with it.
                        let skeleton = query::extract_skeleton(Path::new(path), &content);
                        Outcome::Ok {
                            content: format!("/// Skeleton of {path} (bodies elided):\n{skeleton}"),
                        }
                    } else {
                        Outcome::Ok {
                            content: excerpt(&content, offset, limit),
                        }
                    }
                }
                Err(error) => Outcome::Err {
                    message: format!("{path}: {error}"),
                },
            },
            Err(OpenError::Denied(denied)) => Outcome::Denied {
                reason: denied.reason,
            },
            Err(OpenError::Io(error)) => Outcome::Err {
                message: format!("{path}: {error}"),
            },
        }
    }
}

// ========================================================================
// Edit Implementations
// ========================================================================

impl Edit {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::load(EDIT_SCHEMA),
        }
    }
}

impl Default for Edit {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for Edit {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    /// Edit, then tell the model which local rules govern what it just changed.
    ///
    /// Only on success, and only when a directory between the file and the root
    /// states any: a failed edit has nothing to govern, and a tree with no
    /// subtree rules pays nothing.
    async fn call(&self, args: Value, cx: &ToolCx) -> Outcome {
        let outcome = self.edit(args.clone(), cx).await;
        let Outcome::Ok { content } = outcome else {
            return outcome;
        };
        let Some(path) = args.get("path").and_then(Value::as_str) else {
            return Outcome::Ok { content };
        };
        let full = resolve(&cx.cwd, Path::new(path));
        match crate::subtree_rules::for_path(cx, &full).await {
            Some(rules) => Outcome::Ok {
                content: format!("{content}\n\n{rules}"),
            },
            None => Outcome::Ok { content },
        }
    }
}

impl Edit {
    async fn edit(&self, args: Value, cx: &ToolCx) -> Outcome {
        let Some(path) = args.get("path").and_then(Value::as_str) else {
            return missing("path");
        };

        // **An absent `action` is never `overwrite`.** It used to be: a call
        // that named a path and content, and forgot to say what it meant, was
        // read as "replace the whole file". That made the laziest possible call
        // the most destructive one available, and a truncated argument stream is
        // exactly the shape that produces it. The anchored edit is the safe
        // default, and asking for a whole-file rewrite now costs one word.
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("str_replace");

        let full = resolve(&cx.cwd, Path::new(path));

        match action {
            "str_replace" => {
                let Some(old_string) = args.get("old_string").and_then(Value::as_str) else {
                    return missing("old_string");
                };
                let Some(new_string) = args.get("new_string").and_then(Value::as_str) else {
                    return missing("new_string");
                };

                let handle = match cx.broker.read(&full).await {
                    Ok(h) => h,
                    Err(OpenError::Denied(denied)) => {
                        return Outcome::Denied {
                            reason: denied.reason,
                        };
                    }
                    Err(OpenError::Io(error)) => {
                        return Outcome::Err {
                            message: format!("{path}: {error}"),
                        };
                    }
                };

                let existing = match handle.read_to_string() {
                    Ok(c) => c,
                    Err(e) => {
                        return Outcome::Err {
                            message: format!("{path}: {e}"),
                        };
                    }
                };

                let replace_all = args
                    .get("replace_all")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let matches = existing.matches(old_string).count();
                let (updated, how) = match matches {
                    0 => match normalized_matches(&existing, old_string).as_slice() {
                        [range] => {
                            let mut updated = existing.clone();
                            updated.replace_range(range.clone(), new_string);
                            (
                                updated,
                                " (matched after normalizing quotes and whitespace)".to_string(),
                            )
                        }
                        [] => {
                            return Outcome::Err {
                                message: format!(
                                    "{path}: target `old_string` was not found in file. Read the file to get exact lines."
                                ),
                            };
                        }
                        several => {
                            return Outcome::Err {
                                message: format!(
                                    "{path}: target `old_string` was not found exactly, and matched {} places once quotes and whitespace were normalized. Read the file to get exact lines.",
                                    several.len()
                                ),
                            };
                        }
                    },
                    1 => (existing.replacen(old_string, new_string, 1), String::new()),
                    _ if replace_all => (
                        existing.replace(old_string, new_string),
                        format!(" ({matches} occurrences)"),
                    ),
                    _ => {
                        return Outcome::Err {
                            message: format!(
                                "{path}: target `old_string` matched {matches} times. Include more surrounding lines to make it unique, or pass replace_all: true to change every one."
                            ),
                        };
                    }
                };
                match cx.broker.write_previewed(&full, &updated).await {
                    Ok(handle) => match handle.write(updated.as_bytes()) {
                        Ok(()) => Outcome::Ok {
                            content: format!(
                                "Successfully replaced in {path}{how}:\n{}",
                                hunk_diff(&existing, &updated)
                            ),
                        },
                        Err(error) => Outcome::Err {
                            message: format!("{path}: {error}"),
                        },
                    },
                    Err(OpenError::Denied(denied)) => Outcome::Denied {
                        reason: denied.reason,
                    },
                    Err(OpenError::Io(error)) => Outcome::Err {
                        message: format!("{path}: {error}"),
                    },
                }
            }
            "create" => {
                if cx.broker.read(&full).await.is_ok() {
                    return Outcome::Err {
                        message: format!(
                            "{path}: file already exists. Use 'str_replace' or 'overwrite'."
                        ),
                    };
                }
                let Some(content) = args.get("content").and_then(Value::as_str) else {
                    return missing("content");
                };
                match cx.broker.write_previewed(&full, content).await {
                    Ok(handle) => match handle.write(content.as_bytes()) {
                        Ok(()) => Outcome::Ok {
                            content: format!("created {path} ({} bytes)", content.len()),
                        },
                        Err(error) => Outcome::Err {
                            message: format!("{path}: {error}"),
                        },
                    },
                    Err(OpenError::Denied(denied)) => Outcome::Denied {
                        reason: denied.reason,
                    },
                    Err(OpenError::Io(error)) => Outcome::Err {
                        message: format!("{path}: {error}"),
                    },
                }
            }
            "overwrite" => {
                let Some(content) = args.get("content").and_then(Value::as_str) else {
                    return missing("content");
                };
                match cx.broker.write_previewed(&full, content).await {
                    Ok(handle) => match handle.write(content.as_bytes()) {
                        Ok(()) => Outcome::Ok {
                            content: format!("wrote {} bytes to {path}", content.len()),
                        },
                        Err(error) => Outcome::Err {
                            message: format!("{path}: {error}"),
                        },
                    },
                    Err(OpenError::Denied(denied)) => Outcome::Denied {
                        reason: denied.reason,
                    },
                    Err(OpenError::Io(error)) => Outcome::Err {
                        message: format!("{path}: {error}"),
                    },
                }
            }
            _ => Outcome::Err {
                message: format!(
                    "unknown action '{action}'. Specify action as 'str_replace', 'create', or 'overwrite'."
                ),
            },
        }
    }
}

// ========================================================================
// Grep Implementations
// ========================================================================

impl Grep {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::load(GREP_SCHEMA),
        }
    }
}

impl Default for Grep {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for Grep {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Outcome {
        let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
            return missing("pattern");
        };
        // `-e`, not positional: a pattern that begins with `-` is parsed as an
        // `rg` flag otherwise, so searching for `--foo` fails as a usage error
        // and the model reads the empty result as "no matches".
        let mut rg_args = vec![
            "--line-number".to_string(),
            format!("--max-columns={GREP_MAX_COLUMNS}"),
            "--max-columns-preview".into(),
        ];
        if args.get("ignore_case").and_then(Value::as_bool) == Some(true) {
            rg_args.push("--ignore-case".into());
        }
        if args.get("files_only").and_then(Value::as_bool) == Some(true) {
            rg_args.push("--files-with-matches".into());
        }
        if let Some(context) = args.get("context").and_then(Value::as_u64) {
            rg_args.push("--context".into());
            rg_args.push(context.min(GREP_MAX_CONTEXT).to_string());
        }
        rg_args.push("-e".into());
        rg_args.push(pattern.to_string());
        if let Some(glob) = args.get("glob").and_then(Value::as_str) {
            rg_args.push("--glob".into());
            rg_args.push(glob.to_string());
        }
        // After `--`, so a path that begins with `-` is a path.
        rg_args.push("--".into());
        match args.get("path").and_then(Value::as_str) {
            Some(path) => {
                if let Err(outcome) = authorize_search(cx, path).await {
                    return outcome;
                }
                rg_args.push(path.to_string());
            }
            None => rg_args.push(".".into()),
        }
        run_command(cx, "rg", &rg_args).await
    }
}

// ========================================================================
// Shell Implementations
// ========================================================================

impl Shell {
    pub fn new() -> Self {
        Self::with_shell(ShellSpec::platform_default())
    }

    /// Run command lines through `shell`.
    ///
    /// The shell is named in the tool's *description*, not just used, because
    /// the description is how the model decides what syntax to write. A model
    /// told only "run a shell command" will reach for `&&` and `$(...)` on a
    /// machine running `cmd`, and every one of those calls fails in a way it
    /// cannot diagnose from the error.
    pub fn with_shell(shell: ShellSpec) -> Self {
        // The one spec whose description is not the file verbatim: the shell is
        // chosen at runtime, so `shell.json` writes placeholders and they are
        // filled here. The wording still lives in the file, which is the point
        // of the file.
        let mut spec = ToolSpec::load(SHELL_SCHEMA);
        spec.description = spec
            .description
            .replace(SHELL_PROGRAM_PLACEHOLDER, &shell.program)
            .replace(SHELL_FLAG_PLACEHOLDER, &shell.flag)
            .replace(
                SHELL_DEFAULT_TIMEOUT_PLACEHOLDER,
                &SHELL_DEFAULT_TIMEOUT.as_secs().to_string(),
            )
            .replace(
                SHELL_MAX_TIMEOUT_PLACEHOLDER,
                &SHELL_MAX_TIMEOUT.as_secs().to_string(),
            );
        Self { shell, spec }
    }
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for Shell {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Outcome {
        let Some(command) = args.get("command").and_then(Value::as_str) else {
            return missing("command");
        };
        run_command(cx, &self.shell.program, &self.shell.argv(command)).await
    }

    /// The call's `timeout_secs`, clamped to `SHELL_MAX_TIMEOUT`, or
    /// `SHELL_DEFAULT_TIMEOUT` when it names none.
    ///
    /// Clamped rather than refused: a model asking for an hour wants "long", and
    /// failing the call over the number costs a turn to learn the cap.
    fn timeout(&self, args: &Value) -> Option<Duration> {
        let asked = args
            .get(TIMEOUT_ARG)
            .and_then(Value::as_u64)
            .map(Duration::from_secs);
        Some(asked.map_or(SHELL_DEFAULT_TIMEOUT, |asked| asked.min(SHELL_MAX_TIMEOUT)))
    }
}

// ========================================================================
// GitTool Implementations
// ========================================================================

impl GitTool {
    pub fn new(vcs: Arc<dyn Vcs>) -> Self {
        Self {
            spec: ToolSpec::load(GIT_SCHEMA),
            vcs,
        }
    }
}

#[async_trait]
impl Tool for GitTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Outcome {
        let Some(array) = args.get("args").and_then(Value::as_array) else {
            return Outcome::Err {
                message: "expected `args` array".into(),
            };
        };
        let sub_args = array
            .iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect::<Vec<_>>();
        self.vcs.run(cx, &sub_args).await
    }
}

// ========================================================================
// Functions
// ========================================================================

/// A window of `content`: `limit` lines from 1-based `offset`, under the cap,
/// each prefixed with its line number and a tab.
///
/// The numbers are what let a read be followed: an `offset` to continue from,
/// a grep hit to find, an edit's hunk header to check, all without counting.
///
/// The marker is not decoration. Without it a truncated read is indistinguishable
/// from a short file, and the model edits against a file it believes it has seen
/// all of.
fn excerpt(content: &str, offset: u64, limit: u64) -> String {
    let total = content.lines().count() as u64;
    let start = offset.max(1);

    let mut out = String::new();
    let mut kept = 0u64;
    for (number, line) in content
        .lines()
        .enumerate()
        .skip((start - 1) as usize)
        .take(limit as usize)
    {
        let numbered = format!("{}\t{line}\n", number + 1);
        if out.len() + numbered.len() > READ_CAP_BYTES {
            break;
        }
        out.push_str(&numbered);
        kept += 1;
    }

    if kept == total {
        return out;
    }
    if kept == 0 {
        return format!("[the file has {total} lines; line {start} is past the end]\n");
    }
    let last = start + kept - 1;
    out.push_str(&format!("\n[showing lines {start}-{last} of {total}"));
    if last < total {
        out.push_str(&format!(
            "; call `read` again with offset={} to continue",
            last + 1
        ));
    }
    out.push_str("]\n");
    out
}

/// Authorize `grep` to search `path`, as the read it is.
///
/// `rg` opens what it is pointed at itself, so a `path` the broker never saw
/// would let a search read outside the root the policy allows, or a file a
/// `deny_read` rule covers. The broker checks it as a directory listing or a
/// file read, whichever it is, and the search runs only if that is allowed.
async fn authorize_search(cx: &ToolCx, path: &str) -> Result<(), Outcome> {
    let full = resolve(&cx.cwd, Path::new(path));
    let denied = |error: OpenError| match error {
        OpenError::Denied(denied) => Outcome::Denied {
            reason: denied.reason,
        },
        OpenError::Io(error) => Outcome::Err {
            message: format!("{path}: {error}"),
        },
    };
    match cx.broker.read_dir(&full).await {
        Ok(_) => Ok(()),
        Err(OpenError::Io(_)) => cx.broker.read(&full).await.map(|_| ()).map_err(denied),
        Err(error) => Err(denied(error)),
    }
}

/// Where `needle` occurs in `haystack` once both are normalized, as byte
/// ranges of the original `haystack`.
///
/// The fallback for an `edit` whose `old_string` matched nowhere exactly. The
/// misses it recovers are the ones a model makes copying text it was shown:
/// smart quotes and Unicode dashes or spaces where the file has ASCII, CRLF
/// line ends, and trailing whitespace. Each used to cost a re-read and a retry.
/// Indentation is not normalized: a block at the wrong depth is a different
/// edit, and matching it anyway would put code at the wrong level.
fn normalized_matches(haystack: &str, needle: &str) -> Vec<std::ops::Range<usize>> {
    let (needle, _) = normalize_for_match(needle);
    if needle.is_empty() {
        return Vec::new();
    }
    let (normalized, spans) = normalize_for_match(haystack);
    normalized
        .match_indices(&needle)
        .map(|(start, found)| spans[start].start..spans[start + found.len() - 1].end)
        .collect()
}

/// `text` normalized for [`normalized_matches`], with the original byte range
/// of the character each normalized byte came from.
fn normalize_for_match(text: &str) -> (String, Vec<std::ops::Range<usize>>) {
    let mut out = String::with_capacity(text.len());
    let mut spans = Vec::with_capacity(text.len());
    // Spaces and tabs held back until it is known whether they end a line.
    let mut pending: Vec<(char, std::ops::Range<usize>)> = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((at, ch)) = chars.next() {
        let span = at..at + ch.len_utf8();
        let next = chars.peek().map(|&(_, next)| next);
        let ch = match ch {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            '\r' if next == Some('\n') => continue,
            other => other,
        };
        if ch == ' ' || ch == '\t' {
            pending.push((ch, span));
            continue;
        }
        if ch != '\n' {
            for (held, held_span) in pending.drain(..) {
                push_mapped(&mut out, &mut spans, held, held_span);
            }
        }
        pending.clear();
        push_mapped(&mut out, &mut spans, ch, span);
    }
    (out, spans)
}

fn push_mapped(
    out: &mut String,
    spans: &mut Vec<std::ops::Range<usize>>,
    ch: char,
    span: std::ops::Range<usize>,
) {
    out.push(ch);
    spans.extend(std::iter::repeat_n(span, ch.len_utf8()));
}

/// The `Outcome` for a required argument the caller did not supply.
fn missing(name: &str) -> Outcome {
    Outcome::Err {
        message: format!("expected a string `{name}` argument"),
    }
}

/// A line diff of a whole file with the specific words that changed marked:
/// `[-deleted-]` around a removed word, `{+added+}` around an added one --
/// `git diff --word-diff`'s convention, plain enough that a reader (model or
/// person) does not need the markers explained, and the one the `cli` crate's
/// transcript parses back out to bold the changed words on top of the line's
/// red/green.
///
/// Cut to the changed hunks: each opens with its `@@ -a,b +c,d @@` header and
/// keeps three lines of context, so a one-word edit in a long file reads as a
/// few lines with that word marked, located by line number. Empty when nothing
/// changed.
///
/// What an `edit` reports back and what a write's permission prompt shows.
/// The line numbers are the point of using it for the result too: the model
/// learns where its change landed without reading the file again, and the
/// person reading the transcript sees it the way the prompt showed it.
pub fn hunk_diff(before: &str, after: &str) -> String {
    let diff = similar::TextDiff::from_lines(before, after);
    let mut out = String::new();
    for hunk in diff
        .unified_diff()
        .context_radius(HUNK_CONTEXT_LINES)
        .iter_hunks()
    {
        out.push_str(&format!("{}\n", hunk.header()));
        for op in hunk.ops() {
            push_inline_changes(&mut out, &diff, op);
        }
    }
    out
}

/// Append `op`'s lines to `out`, one per line, signed and word-marked the way
/// [`hunk_diff`] documents.
fn push_inline_changes<'a>(
    out: &mut String,
    diff: &'a similar::TextDiff<'a, 'a, 'a, str>,
    op: &similar::DiffOp,
) {
    for change in diff.iter_inline_changes(op) {
        let (prefix, open, close) = match change.tag() {
            similar::ChangeTag::Delete => ("-", "[-", "-]"),
            similar::ChangeTag::Insert => ("+", "{+", "+}"),
            similar::ChangeTag::Equal => (" ", "", ""),
        };
        out.push_str(prefix);
        out.push(' ');
        for (emphasized, value) in change.iter_strings_lossy() {
            if emphasized {
                out.push_str(open);
                out.push_str(&value);
                out.push_str(close);
            } else {
                out.push_str(&value);
            }
        }
        if change.missing_newline() {
            out.push('\n');
        }
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
// Test-fixture fs (staging a file to edit), not agent-driven work.
#[allow(clippy::disallowed_methods)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use sandbox::Broker;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::*;

    fn cx_at(root: &Path) -> ToolCx {
        ToolCx::new(
            Arc::new(Broker::permissive(root.to_path_buf())),
            Arc::from(root),
            CancellationToken::new(),
            Instant::now(),
        )
    }

    #[tokio::test]
    async fn an_edit_that_names_no_action_does_not_rewrite_the_file() {
        // The lazy call must not be the destructive one. A call carrying a path
        // and content but no `action` used to mean "replace the whole file",
        // which is also the shape a truncated argument stream produces: the
        // model meant to anchor an edit, the stream was cut, and the file was
        // replaced by a fragment.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("keep.txt");
        std::fs::write(&path, "original\n").expect("write");

        let outcome = Edit::new()
            .edit(
                json!({"path": "keep.txt", "content": "clobbered"}),
                &cx_at(dir.path()),
            )
            .await;

        assert!(
            matches!(outcome, Outcome::Err { .. }),
            "an unanchored edit must be refused, got {outcome:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "original\n",
            "the file was rewritten by a call that never asked for a rewrite"
        );
    }

    #[test]
    fn a_near_miss_matches_only_where_quotes_and_whitespace_differ() {
        // What a model copying shown text gets wrong: curly quotes, a Unicode
        // dash, CRLF, trailing spaces. Each used to cost a re-read and a retry.
        let file = "let s = \"it's\";  \r\nlet d = 1 - 2;\n";
        let ranges = normalized_matches(
            file,
            "let s = \u{201C}it\u{2019}s\u{201D};\nlet d = 1 \u{2013} 2;",
        );
        assert_eq!(ranges.len(), 1);
        assert_eq!(
            &file[ranges[0].clone()],
            "let s = \"it's\";  \r\nlet d = 1 - 2;"
        );

        // Indentation is a different edit, not a near miss.
        assert!(normalized_matches("    fn f() {}\n", "  fn f() {}").len() <= 1);
        assert!(normalized_matches("fn f() {}\n", "fn  g() {}").is_empty());
    }

    #[tokio::test]
    async fn an_edit_recovers_a_near_miss_and_replace_all_renames_every_occurrence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "let msg = \"hi\";\nold(); old();\n").expect("write");
        let cx = cx_at(dir.path());

        let outcome = Edit::new()
            .edit(
                json!({"path": "a.rs", "old_string": "let msg = \u{201C}hi\u{201D};", "new_string": "let msg = \"hello\";"}),
                &cx,
            )
            .await;
        assert!(
            matches!(&outcome, Outcome::Ok { content } if content.contains("normalizing")),
            "{outcome:?}"
        );

        let refused = Edit::new()
            .edit(
                json!({"path": "a.rs", "old_string": "old()", "new_string": "new()"}),
                &cx,
            )
            .await;
        assert!(
            matches!(&refused, Outcome::Err { message } if message.contains("replace_all")),
            "{refused:?}"
        );

        let renamed = Edit::new()
            .edit(
                json!({"path": "a.rs", "old_string": "old()", "new_string": "new()", "replace_all": true}),
                &cx,
            )
            .await;
        assert!(matches!(renamed, Outcome::Ok { .. }), "{renamed:?}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "let msg = \"hello\";\nnew(); new();\n"
        );
    }

    #[tokio::test]
    async fn a_grep_path_outside_the_root_is_refused_before_rg_runs() {
        // `rg` opens what it is pointed at itself, so a path the broker never
        // checked would let a search read outside the policy's root.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = sandbox::canonicalize(dir.path()).expect("canonical root");
        let outside = tempfile::tempdir().expect("another tempdir");
        let broker = sandbox::BrokerBuilder::new(policy::Policy::builtin(root.clone()))
            .build(true)
            .expect("unenforced is allowed in a test");
        let cx = ToolCx::new(
            Arc::new(broker),
            Arc::from(root.as_path()),
            CancellationToken::new(),
            Instant::now(),
        );
        let outcome = Grep::new()
            .call(
                json!({"pattern": "x", "path": outside.path().to_string_lossy()}),
                &cx,
            )
            .await;
        assert!(matches!(outcome, Outcome::Denied { .. }), "{outcome:?}");
    }

    #[test]
    fn a_shell_call_gets_a_build_sized_timeout_and_a_bounded_override() {
        // Two bugs, one per direction: the two-minute dispatcher cap killing
        // `make check` when no timeout is named, and a call asking for an hour
        // holding the turn for an hour.
        let shell = Shell::new();
        assert_eq!(
            shell.timeout(&json!({ "command": "make check" })),
            Some(SHELL_DEFAULT_TIMEOUT)
        );
        assert_eq!(
            shell.timeout(&json!({ "command": "make check", "timeout_secs": 3600 })),
            Some(SHELL_MAX_TIMEOUT)
        );
        assert_eq!(
            shell.timeout(&json!({ "command": "sleep 5", "timeout_secs": 5 })),
            Some(Duration::from_secs(5))
        );
    }

    #[test]
    fn the_shell_description_names_its_timeouts() {
        // An unfilled placeholder reaches the model as `{max_timeout}`, which
        // it can only guess at.
        let shell = Shell::new();
        let description = &shell.spec().description;
        assert!(!description.contains('{'), "{description}");
        assert!(
            description.contains(&SHELL_MAX_TIMEOUT.as_secs().to_string()),
            "{description}"
        );
    }

    fn numbered(lines: usize) -> String {
        (1..=lines)
            .map(|n| format!("line {n}\n"))
            .collect::<String>()
    }

    #[test]
    fn a_short_file_is_returned_whole_and_unannotated() {
        // The common case. A marker here would train the model to ignore it.
        let content = numbered(3);
        assert_eq!(
            excerpt(&content, 1, READ_DEFAULT_LINES as u64),
            "1\tline 1\n2\tline 2\n3\tline 3\n"
        );
    }

    #[test]
    fn a_window_is_numbered_from_its_offset() {
        // The numbers are the file's, not the window's: a read from line 11
        // that numbered from 1 would send every later edit to the wrong place.
        let rendered = excerpt(&numbered(20), 11, 2);
        assert!(
            rendered.starts_with("11\tline 11\n12\tline 12\n"),
            "{rendered}"
        );
    }

    #[test]
    fn a_truncated_read_says_so_and_says_where_to_resume() {
        // Without this the model cannot distinguish a truncated read from a
        // short file, so it edits against a file it believes it has seen all of.
        let rendered = excerpt(&numbered(500), 1, 10);
        assert!(rendered.starts_with("1\tline 1\n"));
        assert!(rendered.contains("line 10\n"));
        assert!(!rendered.contains("line 11\n"));
        assert!(rendered.contains("showing lines 1-10 of 500"), "{rendered}");
        assert!(rendered.contains("offset=11"), "{rendered}");
    }

    #[test]
    fn the_last_window_does_not_invite_a_read_past_the_end() {
        // The window is partial (so it is annotated) but there is nothing after
        // it, and suggesting an offset would send the model round again.
        let rendered = excerpt(&numbered(20), 11, 100);
        assert!(rendered.contains("showing lines 11-20 of 20"), "{rendered}");
        assert!(!rendered.contains("offset="), "{rendered}");
    }

    #[test]
    fn the_byte_cap_bounds_a_file_with_no_line_breaks() {
        // The shape `limit` cannot bound: a minified bundle is one enormous
        // line, so a line count alone still puts the whole file in the window.
        let rendered = excerpt(
            &"x".repeat(READ_CAP_BYTES * 4),
            1,
            READ_DEFAULT_LINES as u64,
        );
        assert!(
            rendered.len() < READ_CAP_BYTES,
            "capped: {}",
            rendered.len()
        );
        assert!(rendered.contains("past the end") || rendered.contains("showing lines"));
    }

    #[test]
    fn an_offset_past_the_end_explains_itself() {
        let rendered = excerpt(&numbered(5), 99, 10);
        assert!(rendered.contains("has 5 lines"), "{rendered}");
        assert!(rendered.contains("past the end"), "{rendered}");
    }

    #[test]
    fn hunk_diff_cuts_a_long_file_to_the_changed_word_and_its_context() {
        // A one-word edit on line 10 of 20: the prompt must show where it is
        // and what changed, not reprint the file or mark the whole line.
        let before: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        let after = before.replace("line 10\n", "line ten\n");
        let diff = hunk_diff(&before, &after);
        assert!(diff.starts_with("@@ -7,7 +7,7 @@\n"), "{diff}");
        assert!(diff.contains("- line [-10-]\n"), "{diff}");
        assert!(diff.contains("+ line {+ten+}\n"), "{diff}");
        assert!(
            diff.contains("  line 7\n") && diff.contains("  line 13\n"),
            "{diff}"
        );
        assert!(
            !diff.contains("line 6\n") && !diff.contains("line 14\n"),
            "context past three lines is cut: {diff}"
        );
        assert_eq!(hunk_diff(&before, &before), "", "no change, no hunk");
    }

    #[test]
    fn hunk_diff_marks_only_the_word_that_changed() {
        // The bug the old two-line dump had: replacing one word in a line (or
        // block) marked the whole thing changed. A reader should see exactly
        // `foo` and `bar` picked out, not the whole line.
        let diff = hunk_diff("let x = foo();\n", "let x = bar();\n");
        assert!(diff.contains("[-foo-]"), "{diff}");
        assert!(diff.contains("{+bar+}"), "{diff}");
        assert!(
            diff.contains("let x = "),
            "the common prefix is not re-marked: {diff}"
        );
        assert!(
            !diff.contains("[-let"),
            "the unchanged prefix must not fall inside a delete marker: {diff}"
        );
    }

    #[tokio::test]
    async fn symbols_tool_extracts_functions_and_structs() {
        use sandbox::{Broker, ToolCx};
        use std::sync::Arc;
        use std::time::Instant;
        use tokio_util::sync::CancellationToken;

        // A permissive broker stands in for policy: the test's subject is the
        // AST scan, not authorization, and routing the fixture through the
        // broker keeps the "tools never name std::fs" rule true even here.
        let dir = tempfile::TempDir::new().unwrap();
        let file_path = dir.path().join("test.rs");
        let cx = ToolCx::new(
            Arc::new(Broker::permissive(dir.path().to_path_buf())),
            Arc::from(dir.path()),
            CancellationToken::new(),
            Instant::now(),
        );
        cx.broker
            .write(&file_path)
            .await
            .unwrap()
            .write(b"pub struct MyStruct;\npub fn my_func() {}\n")
            .unwrap();

        let mut out = Vec::new();
        scan_file_symbols(&cx, &file_path, dir.path(), Some("my_func"), None, &mut out).await;
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("[fn] pub fn my_func()"));
    }

    #[tokio::test]
    async fn symbols_tool_extracts_nested_methods_with_parent_scope() {
        use sandbox::{Broker, ToolCx};
        use std::sync::Arc;
        use std::time::Instant;
        use tokio_util::sync::CancellationToken;

        let dir = tempfile::TempDir::new().unwrap();
        let file_path = dir.path().join("test.rs");
        let cx = ToolCx::new(
            Arc::new(Broker::permissive(dir.path().to_path_buf())),
            Arc::from(dir.path()),
            CancellationToken::new(),
            Instant::now(),
        );
        cx.broker
            .write(&file_path)
            .await
            .unwrap()
            .write(b"pub struct Worker;\nimpl Worker {\n    pub fn run(&self) {}\n}\n")
            .unwrap();

        let mut out = Vec::new();
        scan_file_symbols(&cx, &file_path, dir.path(), Some("run"), None, &mut out).await;
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("Worker::pub fn run(&self)"));

        let mut matched_by_parent = Vec::new();
        scan_file_symbols(
            &cx,
            &file_path,
            dir.path(),
            Some("Worker"),
            None,
            &mut matched_by_parent,
        )
        .await;
        assert_eq!(
            matched_by_parent.len(),
            2,
            "both struct and its method match parent query"
        );
    }
}

//! Finding files by name, the counterpart to `grep`'s finding them by content.
//!
//! **Why not shell out to `find`?** Because every path this walks goes through
//! the broker, so a pattern cannot enumerate outside the policy root, and the
//! results are the same on every platform. Handing the model a `find` invocation
//! instead would put the traversal outside the sandbox and make the tool's
//! behaviour depend on which `find` is installed.
//!
//! Matches come back newest first. A model looking for "the test file for this
//! module" is nearly always looking for one that was touched recently, and an
//! alphabetical list buries it behind whatever sorts early.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use globset::{GlobBuilder, GlobMatcher};
use journal::Outcome;
use sandbox::{ToolCx, resolve};
use serde_json::Value;

use crate::tool::{Concurrency, Tool, ToolSpec};

// ========================================================================
// Constants
// ========================================================================

const GLOB_SCHEMA: &str = include_str!("../tools/glob.json");

/// How many matches are returned before the walk stops.
///
/// A cap rather than a page: a pattern this broad is a pattern that needs
/// narrowing, and saying so is more useful than paging through it.
const MAX_MATCHES: usize = 200;

/// How deep the walk descends below the search root.
const MAX_DEPTH: usize = 12;

/// Directories never descended into.
///
/// Not a policy decision -- the broker already decides what may be read. This is
/// about signal: a `**/*.rs` that returns nine thousand paths from `target/` has
/// answered no question anyone asked.
const SKIPPED_DIRS: &[&str] = &[".git", "node_modules", "target", "vendor"];

// ========================================================================
// Data Structures
// ========================================================================

/// Find files whose path matches a glob pattern.
pub struct GlobTool {
    spec: ToolSpec,
}

/// One matched file, ordered newest first and then by path.
///
/// `Ord` is what sorts the results, so the field order is the sort order:
/// `modified` descends via [`std::cmp::Reverse`] at insertion, and `path`
/// breaks ties so that two files written in the same clock tick still come back
/// in a stable order rather than an arbitrary one.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Match {
    modified: std::cmp::Reverse<SystemTime>,
    path: PathBuf,
}

// ========================================================================
// GlobTool Implementations
// ========================================================================

impl GlobTool {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::load(GLOB_SCHEMA),
        }
    }
}

impl Default for GlobTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Outcome {
        let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
            return Outcome::Err {
                message: "glob requires a `pattern` argument".into(),
            };
        };
        // `literal_separator` is what makes `*` stop at a directory boundary.
        // globset defaults it off, so without this `*.rs` and `**/*.rs` return
        // the same set and the distinction the description promises is a lie.
        let matcher = match GlobBuilder::new(pattern).literal_separator(true).build() {
            Ok(glob) => glob.compile_matcher(),
            Err(error) => {
                return Outcome::Err {
                    message: format!("`{pattern}` is not a valid glob: {error}"),
                };
            }
        };

        let root = match args.get("path").and_then(Value::as_str) {
            Some(path) => resolve(&cx.cwd, Path::new(path)),
            None => cx.cwd.to_path_buf(),
        };

        let mut found = BTreeSet::new();
        walk(cx, &root, &root, &matcher, &mut found, 0).await;

        if found.is_empty() {
            return Outcome::Ok {
                content: format!("no files match `{pattern}`"),
            };
        }
        let listing: Vec<String> = found
            .iter()
            .take(MAX_MATCHES)
            .map(|entry| entry.path.display().to_string())
            .collect();
        let truncated = if found.len() > MAX_MATCHES {
            format!(
                "\n\n({} more matches; narrow the pattern to see them)",
                found.len() - MAX_MATCHES
            )
        } else {
            String::new()
        };
        Outcome::Ok {
            content: format!("{}{truncated}", listing.join("\n")),
        }
    }
}

// ========================================================================
// Functions
// ========================================================================

/// Walk `dir`, collecting files whose path relative to `base` matches.
///
/// Recursion stops at [`MAX_DEPTH`] and once [`MAX_MATCHES`] is exceeded. The
/// set is allowed to grow one past the cap so the caller can tell "exactly the
/// cap" from "more than the cap" and say so.
async fn walk(
    cx: &ToolCx,
    base: &Path,
    dir: &Path,
    matcher: &GlobMatcher,
    found: &mut BTreeSet<Match>,
    depth: usize,
) {
    if depth > MAX_DEPTH || found.len() > MAX_MATCHES {
        return;
    }
    let Ok(entries) = cx.broker.read_dir(dir).await else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || SKIPPED_DIRS.contains(&name.as_str()) {
            continue;
        }
        if path.is_dir() {
            Box::pin(walk(cx, base, &path, matcher, found, depth + 1)).await;
        } else if path.is_file() {
            let relative = path.strip_prefix(base).unwrap_or(&path);
            if matcher.is_match(relative) {
                let modified = entry
                    .metadata()
                    .and_then(|meta| meta.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                found.insert(Match {
                    modified: std::cmp::Reverse(modified),
                    path: relative.to_path_buf(),
                });
            }
        }
    }
}

// ========================================================================
// Tests
// ========================================================================

// Test-fixture fs (staging a tree to search), not agent-driven work. Stated
// here rather than crate-wide: this crate's whole point is that its *tools* go
// through the broker, and a crate-level allow would retire that check.
#[allow(clippy::disallowed_methods)]
#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::time::Instant;

    use sandbox::Broker;
    use serde_json::json;
    use tempfile::TempDir;
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

    fn tree() -> TempDir {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path();
        fs::create_dir_all(root.join("src/deep")).expect("create src/deep");
        fs::create_dir_all(root.join("target")).expect("create target");
        fs::write(root.join("top.rs"), "").expect("write top.rs");
        fs::write(root.join("src/lib.rs"), "").expect("write src/lib.rs");
        fs::write(root.join("src/deep/mod.rs"), "").expect("write src/deep/mod.rs");
        fs::write(root.join("src/notes.md"), "").expect("write src/notes.md");
        fs::write(root.join("target/built.rs"), "").expect("write target/built.rs");
        dir
    }

    async fn glob_for(root: &Path, pattern: &str) -> String {
        let outcome = GlobTool::new()
            .call(json!({ "pattern": pattern }), &cx_at(root))
            .await;
        match outcome {
            Outcome::Ok { content } => content,
            other => panic!("expected a match listing, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn double_star_crosses_directories_and_single_star_does_not() {
        // The bug this catches is matching against the absolute path or the file
        // name instead of the path relative to the search root: either one makes
        // `*.rs` and `**/*.rs` return the same set, and both are wrong.
        let dir = tree();
        let deep = glob_for(dir.path(), "**/*.rs").await;
        assert!(deep.contains("src/deep/mod.rs"), "got: {deep}");
        assert!(deep.contains("top.rs"), "got: {deep}");

        let shallow = glob_for(dir.path(), "*.rs").await;
        assert!(shallow.contains("top.rs"), "got: {shallow}");
        assert!(
            !shallow.contains("src/lib.rs"),
            "a single star must not cross a directory: {shallow}"
        );
    }

    #[tokio::test]
    async fn build_output_is_not_searched() {
        let dir = tree();
        let found = glob_for(dir.path(), "**/*.rs").await;
        assert!(
            !found.contains("built.rs"),
            "target/ should be skipped: {found}"
        );
    }

    #[tokio::test]
    async fn the_most_recently_modified_match_comes_first() {
        // Ordering is the tool's whole ergonomic claim. Without the `Reverse`,
        // this returns the oldest file first and the test fails.
        let dir = tree();
        let newest = dir.path().join("src/deep/mod.rs");
        let later = SystemTime::now() + std::time::Duration::from_secs(60);
        fs::File::open(&newest)
            .expect("open the file to re-stamp")
            .set_modified(later)
            .expect("set mtime");

        let found = glob_for(dir.path(), "**/*.rs").await;
        let first = found.lines().next().expect("at least one match");
        assert_eq!(first, "src/deep/mod.rs", "full listing: {found}");
    }

    #[tokio::test]
    async fn an_invalid_pattern_is_an_error_not_an_empty_result() {
        let dir = tree();
        let outcome = GlobTool::new()
            .call(json!({ "pattern": "src/[" }), &cx_at(dir.path()))
            .await;
        assert!(
            matches!(outcome, Outcome::Err { .. }),
            "an unparseable glob should say so rather than look like no matches"
        );
    }
}

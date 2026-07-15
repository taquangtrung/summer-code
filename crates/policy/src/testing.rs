//! Test-only helpers shared across this crate's `mod tests` blocks.
//!
//! **A policy root has to be absolute, and what that means is a platform
//! question.** [`Policy::validate`](crate::Policy::validate) refuses a relative
//! root, because normalization absorbs leading `..`s and only an absolute root
//! makes the containment check mean what it reads as. `/repo` satisfies that on
//! Unix and *fails* it on Windows, where a path is absolute only with a drive or
//! UNC prefix -- so tests that hardcode `/repo` do not merely read oddly on
//! Windows, they fail, and one of them (`a_malformed_pattern_...`) would keep
//! passing for entirely the wrong reason.
//!
//! So the root comes from here. The value differs; every property asserted about
//! it does not.

use std::path::PathBuf;

// ========================================================================
// Functions
// ========================================================================

/// An absolute path for tests, built from `/`-separated `segments`.
///
/// `root("repo")` is `/repo` on Unix and `C:\repo` on Windows;
/// `root("home/x/my[project]")` follows the same shape. Glob metacharacters pass
/// through untouched, because several tests exist precisely to check that a
/// checkout path containing them is escaped rather than compiled.
pub(crate) fn root(segments: &str) -> PathBuf {
    let mut path = if cfg!(windows) {
        PathBuf::from("C:\\")
    } else {
        PathBuf::from("/")
    };
    for segment in segments.split('/').filter(|part| !part.is_empty()) {
        path.push(segment);
    }
    path
}

/// The same path as a string, for the tests that assert on rendered patterns.
///
/// Rendered with the platform's own separator, which is what
/// `Path::display` produces and therefore what a pattern spliced from a root
/// actually contains.
pub(crate) fn root_str(segments: &str) -> String {
    root(segments).display().to_string()
}

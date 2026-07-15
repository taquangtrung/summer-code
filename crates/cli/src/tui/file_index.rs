//! The file list behind `@` completion: built once, matched fuzzily.
//!
//! The completion menu used to walk the directory tree on every call, and the
//! view asks for completions on every frame, so a draft holding `@foo` walked
//! the filesystem once per streamed token. It also stopped collecting at 200
//! entries, so in a large repository most files could never be offered. The
//! list is now built once (from `git ls-files`, which honors `.gitignore`, or a
//! bounded walk outside a repository), refreshed after each turn so files the
//! agent created appear, and matched as a fuzzy subsequence: `@mkdn` finds
//! `src/tui/markdown.rs`.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, RwLock};

// ========================================================================
// Constants
// ========================================================================

/// The most files indexed. Past this a repository is big enough that the user
/// is typing a real path, and the index would cost more memory than it saves.
const MAX_INDEXED: usize = 50_000;

/// How deep the fallback walk goes outside a git repository.
const WALK_DEPTH: usize = 8;

/// Directories the fallback walk never enters.
const SKIPPED_DIRS: &[&str] = &["build", "dist", "node_modules", "target", "vendor"];

/// The most matches the menu offers.
const MAX_MATCHES: usize = 20;

/// Score for a query character matched right after the previous one.
const CONSECUTIVE_BONUS: i64 = 5;

/// Score for a query character matched at the start of a path segment or word.
const BOUNDARY_BONUS: i64 = 8;

/// Score when the whole query appears in the file name itself.
const NAME_BONUS: i64 = 20;

static INDEX: RwLock<Option<Arc<Vec<Entry>>>> = RwLock::new(None);

thread_local! {
    /// The last query and its matches: the view asks again on every frame.
    static LAST: RefCell<Option<(String, Vec<Entry>)>> = const { RefCell::new(None) };
}

// ========================================================================
// Data Structures
// ========================================================================

/// One completable path, relative to the session's directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Whether it is a directory, shown with a trailing `/`.
    pub is_dir: bool,
    pub path: String,
}

// ========================================================================
// Functions
// ========================================================================

/// The best `@` matches for `query` in the current directory's index, best
/// first. Builds the index on first use if [`warm`] has not already.
pub fn matches(query: &str) -> Vec<Entry> {
    if let Some(hit) = LAST.with(|last| {
        last.borrow()
            .as_ref()
            .filter(|(previous, _)| previous == query)
            .map(|(_, entries)| entries.clone())
    }) {
        return hit;
    }
    let index = index();
    let found = best_matches(&index, query);
    LAST.with(|last| *last.borrow_mut() = Some((query.to_string(), found.clone())));
    found
}

/// Build the index now, on this thread, so the first `@` does not wait for it.
pub fn warm() {
    let _ = index();
}

/// Forget the index, so the next `@` sees files created since it was built.
pub fn invalidate() {
    if let Ok(mut index) = INDEX.write() {
        *index = None;
    }
    LAST.with(|last| *last.borrow_mut() = None);
}

fn index() -> Arc<Vec<Entry>> {
    if let Some(index) = INDEX.read().ok().and_then(|index| index.clone()) {
        return index;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let built = Arc::new(build(&cwd));
    if let Ok(mut index) = INDEX.write() {
        *index = Some(built.clone());
    }
    built
}

/// Every file under `root` and every directory holding one, sorted.
fn build(root: &Path) -> Vec<Entry> {
    entries_from(git_files(root).unwrap_or_else(|| walked_files(root)))
}

/// `files` and every directory holding one, as sorted entries.
fn entries_from(files: Vec<String>) -> Vec<Entry> {
    let mut dirs = BTreeSet::new();
    for file in &files {
        let mut parent = Path::new(file).parent();
        while let Some(dir) = parent.filter(|dir| !dir.as_os_str().is_empty()) {
            if !dirs.insert(format!("{}/", dir.to_string_lossy())) {
                break;
            }
            parent = dir.parent();
        }
    }
    let mut entries: Vec<Entry> = dirs
        .into_iter()
        .map(|path| Entry { is_dir: true, path })
        .chain(files.into_iter().map(|path| Entry {
            is_dir: false,
            path,
        }))
        .collect();
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    entries
}

/// The files git tracks or would track under `root`, or `None` outside a
/// repository (or without git).
fn git_files(root: &Path) -> Option<Vec<String>> {
    let output = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .take(MAX_INDEXED)
            .map(str::to_string)
            .collect(),
    )
}

/// The files under `root` by a bounded walk, skipping hidden directories and
/// build output.
fn walked_files(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut pending = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                if depth < WALK_DEPTH && !SKIPPED_DIRS.contains(&name.as_str()) {
                    pending.push((path, depth + 1));
                }
            } else if let Ok(relative) = path.strip_prefix(root) {
                files.push(relative.to_string_lossy().into_owned());
                if files.len() >= MAX_INDEXED {
                    return files;
                }
            }
        }
    }
    files
}

/// The [`MAX_MATCHES`] best entries for `query`, best first; for an empty
/// query, the shortest paths.
fn best_matches(index: &[Entry], query: &str) -> Vec<Entry> {
    let mut scored: Vec<(i64, &Entry)> = index
        .iter()
        .filter_map(|entry| fuzzy_score(&entry.path, query).map(|score| (score, entry)))
        .collect();
    scored.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then(left.path.len().cmp(&right.path.len()))
            .then(left.path.cmp(&right.path))
    });
    scored
        .into_iter()
        .take(MAX_MATCHES)
        .map(|(_, entry)| entry.clone())
        .collect()
}

/// How well `path` matches `query` as a case-blind subsequence, or `None`
/// when it does not. Characters matched in a run, or at the start of a
/// segment or word, score higher, and so does the query appearing whole in the
/// file name; shorter paths win ties.
fn fuzzy_score(path: &str, query: &str) -> Option<i64> {
    let path_lower = path.to_lowercase();
    let query_lower = query.to_lowercase();
    let path_chars: Vec<char> = path_lower.chars().collect();
    let mut score = 0;
    let mut cursor = 0;
    let mut previous: Option<usize> = None;
    for wanted in query_lower.chars() {
        let found = (cursor..path_chars.len()).find(|&at| path_chars[at] == wanted)?;
        score += 1;
        if previous.is_some_and(|prior| prior + 1 == found) {
            score += CONSECUTIVE_BONUS;
        }
        if found == 0 || matches!(path_chars[found - 1], '/' | '_' | '-' | '.') {
            score += BOUNDARY_BONUS;
        }
        previous = Some(found);
        cursor = found + 1;
    }
    let name = path_lower
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    if !query_lower.is_empty() && name.contains(&query_lower) {
        score += NAME_BONUS;
    }
    Some(score)
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str) -> Entry {
        Entry {
            is_dir: path.ends_with('/'),
            path: path.into(),
        }
    }

    #[test]
    fn test_a_fuzzy_query_finds_the_file_it_abbreviates() {
        let index = vec![
            entry("crates/cli/src/tui/markdown.rs"),
            entry("crates/cli/src/tui/mod.rs"),
            entry("docs/ROADMAP.md"),
        ];
        let found = best_matches(&index, "mkdn");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].path, "crates/cli/src/tui/markdown.rs");
        assert!(best_matches(&index, "zzz").is_empty());
    }

    #[test]
    fn test_a_match_in_the_file_name_beats_one_spread_over_the_path() {
        // `view` names a file; the same letters scattered through directory
        // names are a worse guess at what the user means.
        let index = vec![
            entry("very/interesting/elsewhere/web.rs"),
            entry("crates/cli/src/tui/view.rs"),
        ];
        assert_eq!(
            best_matches(&index, "view")[0].path,
            "crates/cli/src/tui/view.rs"
        );
    }

    #[test]
    fn test_the_index_lists_files_and_the_directories_holding_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src/deep")).expect("dirs");
        std::fs::write(dir.path().join("src/deep/a.rs"), "").expect("file");
        std::fs::create_dir_all(dir.path().join("target/debug")).expect("build output");
        std::fs::write(dir.path().join("target/debug/out.rs"), "").expect("build file");
        // The walk, not git: a tempdir may sit inside some other repository.
        let paths: Vec<String> = entries_from(walked_files(dir.path()))
            .into_iter()
            .map(|entry| entry.path)
            .collect();
        assert_eq!(paths, ["src/", "src/deep/", "src/deep/a.rs"]);
    }
}

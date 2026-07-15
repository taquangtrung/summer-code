//! ripgrep-free grep: a line scan over files read through the broker.
//!
//! Grep stays first-class (from Phase 1): fast, no index, no staleness, every
//! language, hits comments, strings, and config. It is the baseline the
//! fact/embedding layers add to, never replace.

use sandbox::ToolCx;

use crate::extract::store::FileSet;
use crate::facts::ids::FileId;

// ========================================================================
// Data Structures
// ========================================================================

/// One textual hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub file: FileId,
    /// 0-based line number.
    pub line: u32,
    /// The matched line's text.
    pub text: String,
}

// ========================================================================
// Functions
// ========================================================================

/// Search `files` for `pattern` as a literal substring, line by line. Reads
/// each file through the broker; a path the policy excludes is skipped, not an
/// error.
pub async fn grep(cx: &ToolCx, files: &FileSet, pattern: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    if pattern.is_empty() {
        return hits;
    }
    for file_id in files.ids() {
        let Some(path) = files.path(file_id) else {
            continue;
        };
        let Ok(handle) = cx.broker.read(path).await else {
            continue;
        };
        let Ok(content) = handle.read_to_string() else {
            continue;
        };
        for (index, line) in content.lines().enumerate() {
            if line.contains(pattern) {
                hits.push(Hit {
                    file: file_id,
                    line: index as u32,
                    text: line.to_string(),
                });
            }
        }
    }
    hits
}

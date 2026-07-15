//! Prompt history that outlives the session: `~/.summer/history`, one JSON
//! string per line so a multi-line prompt comes back whole.
//!
//! **Best effort, on purpose.** A history file that cannot be read or written
//! costs the user recall, never the session: under `--sandbox` the process
//! cannot write `~/.summer` at all, and failing a submit over it would be the
//! tail wagging the dog.

use std::io::Write;
use std::path::Path;

// ========================================================================
// Constants
// ========================================================================

const HISTORY_FILE: &str = "history";

/// How many of the most recent entries a session loads. Older lines stay in
/// the file but are not worth the memory or the Up presses.
const LOADED_ENTRIES: usize = 1000;

/// Lines whose text can hold a secret: `/login <provider> <api-key>` puts the
/// key on the line itself, and history is the last place it belongs.
const UNRECORDED_PREFIXES: &[&str] = &["/login"];

// ========================================================================
// Functions
// ========================================================================

/// Whether `line` may be kept in history, in memory or on disk.
pub(crate) fn is_recordable(line: &str) -> bool {
    !UNRECORDED_PREFIXES
        .iter()
        .any(|prefix| line.trim_start().starts_with(prefix))
}

/// The most recent entries in `dir`'s history file, oldest first. A missing or
/// unreadable file, or a corrupt line, is skipped rather than reported.
pub(crate) fn load(dir: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(dir.join(HISTORY_FILE)) else {
        return Vec::new();
    };
    let entries: Vec<String> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<String>(line).ok())
        .filter(|entry| is_recordable(entry))
        .collect();
    let skip = entries.len().saturating_sub(LOADED_ENTRIES);
    entries.into_iter().skip(skip).collect()
}

/// Append `line` to `dir`'s history file, if it may be recorded.
pub(crate) fn append(dir: &Path, line: &str) {
    if !is_recordable(line) {
        return;
    }
    let Ok(encoded) = serde_json::to_string(line) else {
        return;
    };
    let _ = std::fs::create_dir_all(dir);
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(HISTORY_FILE))
    else {
        return;
    };
    let _ = writeln!(file, "{encoded}");
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("summer-history-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_multi_line_prompt_comes_back_whole() {
        let dir = scratch("multiline");
        append(&dir, "first");
        append(&dir, "fix this:\n  and that");
        assert_eq!(load(&dir), vec!["first", "fix this:\n  and that"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_login_line_never_reaches_the_file() {
        // `/login openai sk-...` carries the key on the line itself.
        let dir = scratch("login");
        append(&dir, "/login openai sk-secret");
        append(&dir, "hello");
        let text = std::fs::read_to_string(dir.join(HISTORY_FILE)).unwrap_or_default();
        assert!(!text.contains("sk-secret"), "{text}");
        assert_eq!(load(&dir), vec!["hello"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_most_recent_entries_are_loaded() {
        let dir = scratch("cap");
        for n in 0..LOADED_ENTRIES + 5 {
            append(&dir, &format!("line {n}"));
        }
        let loaded = load(&dir);
        assert_eq!(loaded.len(), LOADED_ENTRIES);
        assert_eq!(loaded[0], "line 5");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

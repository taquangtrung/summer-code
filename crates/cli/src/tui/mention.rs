//! Expanding `@path` mentions in a submitted line.
//!
//! A mention is inlined before the turn starts, so what the model receives is the
//! file's text rather than a path it would have to spend a `read` call on. A
//! mention naming a directory expands to the text files directly under it.
//!
//! **Reads here are harness reads, not agent reads.** The user typed the path, so
//! no tool call caused it and nothing asks the broker: this is the same trust
//! boundary as the user pasting the file's contents by hand.

use crate::tui::state::TuiState;

// ========================================================================
// Functions
// ========================================================================

/// Detect `@file` or `@folder` references in text, load their contents, and attach to prompt context.
pub(super) fn expand_at_file_mentions(
    cwd: &std::path::Path,
    text: &str,
    state: &mut TuiState,
) -> String {
    let mut expanded = text.to_string();
    let mut attached_files = Vec::new();

    for token in text.split_whitespace() {
        if let Some(rel_path) = token.strip_prefix('@') {
            let clean_path = rel_path.trim_matches(|c: char| {
                !c.is_alphanumeric() && c != '.' && c != '/' && c != '_' && c != '-'
            });
            if clean_path.is_empty() {
                continue;
            }
            let full_path = cwd.join(clean_path);
            if full_path.is_file() {
                if let Ok(content) = std::fs::read_to_string(&full_path) {
                    attached_files.push((clean_path.to_string(), content));
                }
            } else if full_path.is_dir() {
                let mut dir_files = Vec::new();
                collect_dir_text_files(&full_path, cwd, &mut dir_files, 0);
                let count = dir_files.len();
                for (rel, content) in dir_files {
                    attached_files.push((rel, content));
                }
                state.notify(format!(
                    "attached @{clean_path} ({count} files) to prompt context"
                ));
            }
        }
    }

    if !attached_files.is_empty() {
        for (path_str, content) in &attached_files {
            state.notify(format!("attached @{path_str} to prompt context"));
            expanded.push_str(&format!(
                "\n\n[Attached Context: {path_str}]\n```\n{content}\n```"
            ));
        }
    }

    expanded
}

fn collect_dir_text_files(
    dir: &std::path::Path,
    base: &std::path::Path,
    files: &mut Vec<(String, String)>,
    depth: usize,
) {
    if depth > 5 || files.len() >= 50 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "target" || name == "node_modules" || name == "vendor" {
            continue;
        }
        if path.is_dir() {
            collect_dir_text_files(&path, base, files, depth + 1);
        } else if path.is_file()
            && let Ok(content) = std::fs::read_to_string(&path)
            && let Ok(rel) = path.strip_prefix(base)
        {
            files.push((rel.to_string_lossy().to_string(), content));
        }
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {

    use crate::tui::testing::state_with;

    #[test]
    fn expand_at_file_mentions_attaches_file_and_folder_content() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let sub_dir = dir.path().join("sub");
        std::fs::create_dir(&sub_dir).unwrap();

        let file_path = dir.path().join("hello.txt");
        std::fs::write(&file_path, "hello world content").unwrap();

        let sub_file = sub_dir.join("nested.rs");
        std::fs::write(&sub_file, "fn nested() {}").unwrap();

        let mut state = state_with(0, &[]);
        let prompt = super::expand_at_file_mentions(
            dir.path(),
            "explain @hello.txt and @sub please",
            &mut state,
        );

        assert!(prompt.contains("[Attached Context: hello.txt]"));
        assert!(prompt.contains("hello world content"));
        assert!(prompt.contains("[Attached Context: sub/nested.rs]"));
        assert!(prompt.contains("fn nested() {}"));
    }
}

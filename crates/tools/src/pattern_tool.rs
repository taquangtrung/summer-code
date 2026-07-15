//! The `pattern` tool: structural AST pattern and node kind search across files.
//!
//! Parses candidate files with Tree-Sitter grammars and matches structural
//! patterns (e.g. `(call_expression function: (identifier) @call)` or direct node
//! kinds like `impl_item`, `function_definition`).

use async_trait::async_trait;
use journal::Outcome;
use query::{AstMatch, Language, search_ast};
use sandbox::ToolCx;
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::tool::{Concurrency, Tool, ToolSpec};

// ========================================================================
// Constants
// ========================================================================

const PATTERN_SCHEMA: &str = include_str!("../tools/pattern.json");

const MAX_DEPTH: usize = 12;
const MAX_MATCHES: usize = 50;
const SKIPPED_DIRS: &[&str] = &[".git", "node_modules", "target", "vendor", "dist", "build"];

// ========================================================================
// Data Structures
// ========================================================================

/// Search for structural AST patterns or node kinds.
pub struct PatternTool {
    spec: ToolSpec,
}

// ========================================================================
// PatternTool Implementations
// ========================================================================

impl PatternTool {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::load(PATTERN_SCHEMA),
        }
    }
}

impl Default for PatternTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for PatternTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Outcome {
        let pattern = match args.get("pattern").and_then(Value::as_str) {
            Some(p) if !p.trim().is_empty() => p.trim(),
            _ => {
                return Outcome::Err {
                    message: "invalid arguments: `pattern` is required".to_string(),
                };
            }
        };

        let target_path_str = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let target_path = Path::new(target_path_str);

        let lang_filter = args
            .get("language")
            .and_then(Value::as_str)
            .and_then(parse_language_filter);

        let mut files_to_scan = Vec::new();
        if target_path.is_file() {
            files_to_scan.push(target_path.to_path_buf());
        } else {
            collect_candidate_files(cx, target_path, 0, &mut files_to_scan).await;
        }

        if files_to_scan.is_empty() {
            return Outcome::Ok {
                content: format!("no source files found under `{}`", target_path.display()),
            };
        }

        let mut all_matches: Vec<AstMatch> = Vec::new();
        for file in files_to_scan {
            let Ok(handle) = cx.broker.read(&file).await else {
                continue;
            };
            let Ok(source) = handle.read_to_string() else {
                continue;
            };
            let matches = search_ast(&file, &source, pattern, lang_filter);
            all_matches.extend(matches);
            if all_matches.len() >= MAX_MATCHES * 2 {
                break;
            }
        }

        if all_matches.is_empty() {
            return Outcome::Ok {
                content: format!(
                    "no AST matches found for pattern `{pattern}` in `{target_path_str}`"
                ),
            };
        }

        let total = all_matches.len();
        let display_matches = &all_matches[..all_matches.len().min(MAX_MATCHES)];

        let mut output = format!("found {total} AST match(es) for `{pattern}`:\n\n");
        for m in display_matches {
            let capture_str = m
                .capture
                .as_ref()
                .map(|c| format!(" [@{c}]"))
                .unwrap_or_default();
            output.push_str(&format!(
                "• {}:{}:{}{} ({})\n",
                m.file.display(),
                m.start_line,
                m.start_col,
                capture_str,
                m.kind,
            ));
            for line in m.snippet.lines() {
                output.push_str(&format!("    {line}\n"));
            }
            output.push('\n');
        }

        if total > MAX_MATCHES {
            output.push_str(&format!(
                "(truncated {} additional matches past the {MAX_MATCHES} limit)\n",
                total - MAX_MATCHES
            ));
        }

        Outcome::Ok {
            content: output.trim_end().to_string(),
        }
    }
}

// ========================================================================
// Helpers
// ========================================================================

fn parse_language_filter(s: &str) -> Option<Language> {
    match s.trim().to_lowercase().as_str() {
        "rust" | "rs" => Some(Language::Rust),
        "typescript" | "ts" | "tsx" => Some(Language::TypeScript),
        "javascript" | "js" | "jsx" => Some(Language::JavaScript),
        "python" | "py" => Some(Language::Python),
        "go" | "golang" => Some(Language::Go),
        "c" => Some(Language::C),
        "cpp" | "c++" | "cxx" | "cc" => Some(Language::Cpp),
        _ => None,
    }
}

async fn collect_candidate_files(cx: &ToolCx, dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = cx.broker.read_dir(dir).await else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if file_type.is_dir() {
            if SKIPPED_DIRS.contains(&name_str.as_ref()) {
                continue;
            }
            Box::pin(collect_candidate_files(cx, &path, depth + 1, out)).await;
        } else if file_type.is_file() && is_source_file(&path) {
            out.push(path);
        }
    }
}

fn is_source_file(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
        return false;
    };
    matches!(
        ext,
        "rs" | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "py"
            | "pyi"
            | "go"
            | "c"
            | "h"
            | "cpp"
            | "cc"
            | "cxx"
            | "hpp"
            | "hh"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_pattern_tool_spec() {
        let tool = PatternTool::new();
        assert_eq!(tool.spec().name, "pattern");
    }
}

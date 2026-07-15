//! Structural AST pattern matching and search across source code.
//!
//! Enables structural AST matching via Tree-sitter query expressions (e.g.
//! `(call_expression function: (identifier) @call)`) or direct AST node kind
//! searches (e.g. `impl_item`, `function_definition`, `if_expression`).

use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator};

use crate::extract::treesitter::{language_for_path, tree_sitter_language};
use crate::facts::symbol::Language;

// ========================================================================
// Data Structures
// ========================================================================

/// A matched AST node in a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AstMatch {
    pub file: PathBuf,
    pub language: Language,
    pub kind: String,
    pub start_line: usize,
    pub start_col: usize,
    pub end_line: usize,
    pub end_col: usize,
    pub snippet: String,
    pub capture: Option<String>,
}

// ========================================================================
// Public Functions
// ========================================================================

/// Search for AST patterns in a single source file.
pub fn search_ast(
    path: &Path,
    source: &str,
    pattern: &str,
    lang_override: Option<Language>,
) -> Vec<AstMatch> {
    let language = lang_override.or_else(|| language_for_path(path));
    let Some(lang) = language else {
        return Vec::new();
    };

    let Some(ts_lang) = tree_sitter_language(lang) else {
        return Vec::new();
    };

    let mut parser = Parser::new();
    if parser.set_language(&ts_lang).is_err() {
        return Vec::new();
    }

    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };

    let trimmed_pattern = pattern.trim();
    let mut matches = Vec::new();

    // 1. Try Tree-Sitter S-expression query if pattern starts with '(' or contains S-expr syntax
    if (trimmed_pattern.starts_with('(') || trimmed_pattern.contains('@'))
        && let Ok(query) = Query::new(&ts_lang, trimmed_pattern)
    {
        let mut cursor = QueryCursor::new();
        let mut query_matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
        while let Some(m) = query_matches.next() {
            for capture in m.captures {
                let node = capture.node;
                let capture_name = query.capture_names()[capture.index as usize].to_string();
                let start_point = node.start_position();
                let end_point = node.end_position();

                let snippet = node_snippet(source, node);
                matches.push(AstMatch {
                    file: path.to_path_buf(),
                    language: lang,
                    kind: node.kind().to_string(),
                    start_line: start_point.row + 1,
                    start_col: start_point.column + 1,
                    end_line: end_point.row + 1,
                    end_col: end_point.column + 1,
                    snippet,
                    capture: Some(capture_name),
                });
            }
        }
        if !matches.is_empty() {
            return matches;
        }
    }

    // 2. Fallback: Search for AST nodes matching the kind name (case-insensitively or exact)
    let target_kind = trimmed_pattern.to_lowercase();
    walk_node_kind(
        tree.root_node(),
        source,
        path,
        lang,
        &target_kind,
        &mut matches,
    );

    matches
}

// ========================================================================
// Private Helpers
// ========================================================================

fn walk_node_kind(
    node: Node,
    source: &str,
    path: &Path,
    lang: Language,
    target_kind: &str,
    results: &mut Vec<AstMatch>,
) {
    let kind = node.kind();
    if kind.eq_ignore_ascii_case(target_kind)
        || (!target_kind.is_empty() && kind.to_lowercase().contains(target_kind))
    {
        let start_point = node.start_position();
        let end_point = node.end_position();
        let snippet = node_snippet(source, node);

        results.push(AstMatch {
            file: path.to_path_buf(),
            language: lang,
            kind: kind.to_string(),
            start_line: start_point.row + 1,
            start_col: start_point.column + 1,
            end_line: end_point.row + 1,
            end_col: end_point.column + 1,
            snippet,
            capture: None,
        });
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_node_kind(child, source, path, lang, target_kind, results);
    }
}

fn node_snippet(source: &str, node: Node) -> String {
    let start = node.start_byte();
    let end = node.end_byte().min(source.len());
    if start >= end || start >= source.len() {
        return String::new();
    }
    let text = &source[start..end];
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= 3 {
        lines.join("\n")
    } else {
        format!("{}\n  ...\n{}", lines[0], lines[lines.len() - 1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn searches_ast_by_node_kind() {
        let code = r#"
fn add(a: i32, b: i32) -> i32 {
    a + b
}

struct Point {
    x: i32,
    y: i32,
}
"#;
        let matches = search_ast(
            Path::new("test.rs"),
            code,
            "struct_item",
            Some(Language::Rust),
        );
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].kind, "struct_item");
        assert_eq!(matches[0].start_line, 6);
    }

    #[test]
    fn searches_ast_by_tree_sitter_query() {
        let code = r#"
fn foo() {
    bar();
    baz(1, 2);
}
"#;
        let query = "(call_expression function: (identifier) @fn_call)";
        let matches = search_ast(Path::new("test.rs"), code, query, Some(Language::Rust));
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].capture.as_deref(), Some("fn_call"));
    }
}

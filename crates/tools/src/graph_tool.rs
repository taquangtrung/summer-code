//! The `graph` tool: who calls a symbol, what it calls, and a 1-hop diagram.
//!
//! **This is a query, not a mechanism.** It reads whatever the extractors put in
//! the fact store and never parses anything itself. Which mechanism answered,
//! and how completely, comes back with the result rather than being assumed:
//! a caller list from a syntactic parse can conflate same-named methods, and the
//! output says so instead of presenting the list as exhaustive.
//!
//! That last part is the whole reason the tool is worth having. Its previous
//! version reported "Callers of `foo` (0)" for every symbol in every codebase,
//! which reads as a fact and was not one; a model can reasonably delete a
//! function it believes has no callers.

use async_trait::async_trait;
use journal::Outcome;
use query::{Answer, CodeGraph, Completeness, Extractor, FactStore, FileSet, TreeSitterExtractor};
use sandbox::ToolCx;
use serde_json::Value;

use crate::tool::{Concurrency, Tool, ToolSpec};

// ========================================================================
// Constants
// ========================================================================

const GRAPH_SCHEMA: &str = include_str!("../tools/graph.json");

/// How deep the workspace scan descends.
const MAX_DEPTH: usize = 12;

/// Directories never scanned. Not policy (the broker decides that): signal.
const SKIPPED_DIRS: &[&str] = &[".git", "node_modules", "target", "vendor"];

// ========================================================================
// Data Structures
// ========================================================================

/// Query the call graph built from extracted facts.
pub struct GraphTool {
    spec: ToolSpec,
}

/// What the caller asked for.
///
/// An enum rather than a `match` on the raw string, so an unrecognized mode is
/// an error the model can correct. It used to fall through a `_` arm to the
/// diagram, so `"callee"` silently answered a different question than it asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Callees,
    Callers,
    Mermaid,
}

// ========================================================================
// GraphTool Implementations
// ========================================================================

impl GraphTool {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::load(GRAPH_SCHEMA),
        }
    }
}

impl Default for GraphTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for GraphTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Outcome {
        let Some(symbol) = args.get("symbol").and_then(Value::as_str) else {
            return Outcome::Err {
                message: "graph requires a `symbol` argument".into(),
            };
        };
        let mode = match args.get("mode").and_then(Value::as_str) {
            None => Mode::Mermaid,
            Some(raw) => match Mode::parse(raw) {
                Some(mode) => mode,
                None => {
                    return Outcome::Err {
                        message: format!(
                            "unknown mode `{raw}`: expected `callers`, `callees`, or `mermaid`"
                        ),
                    };
                }
            },
        };

        let depth = args
            .get("depth")
            .and_then(Value::as_u64)
            .map(|d| (d as usize).clamp(1, 5))
            .unwrap_or(1);

        let root = cx.broker.policy().root.clone();
        let mut files = FileSet::new();
        collect_source_files(cx, &root, &mut files, 0).await;

        let mut store = FactStore::new();
        let extractor = TreeSitterExtractor::new();
        extractor.extract(cx, &files, &mut store).await;
        let graph = CodeGraph::from_store(&store, extractor.name());

        let content = if depth > 1 {
            match mode {
                Mode::Callees => {
                    let callees = graph.callees_of_depth(symbol, depth);
                    let listing = if callees.is_empty() {
                        format!("nothing found that `{symbol}` calls")
                    } else {
                        let mut lines = vec![format!(
                            "`{symbol}` calls ({} up to depth {depth}):",
                            callees.len()
                        )];
                        for (callee, d) in &callees {
                            let indent = "  ".repeat(*d);
                            lines.push(format!("{indent}- {callee} (hop {d})"));
                        }
                        lines.join("\n")
                    };
                    format!("{listing}\n\n{}", caveat(&graph.callees(symbol)))
                }
                Mode::Callers => {
                    let callers = graph.callers_of_depth(symbol, depth);
                    let listing = if callers.is_empty() {
                        format!("nothing found that calls `{symbol}`")
                    } else {
                        let mut lines = vec![format!(
                            "`{symbol}` is called by ({} up to depth {depth}):",
                            callers.len()
                        )];
                        for (caller, d) in &callers {
                            let indent = "  ".repeat(*d);
                            lines.push(format!("{indent}- {caller} (hop {d})"));
                        }
                        lines.join("\n")
                    };
                    format!("{listing}\n\n{}", caveat(&graph.callers(symbol)))
                }
                Mode::Mermaid => format!(
                    "{depth}-hop call graph for `{symbol}`:\n\n{}\n\n{}",
                    graph.format_mermaid_depth(symbol, depth),
                    caveat(&graph.callers(symbol))
                ),
            }
        } else {
            match mode {
                Mode::Callees => render(symbol, "calls", &graph.callees(symbol)),
                Mode::Callers => render(symbol, "is called by", &graph.callers(symbol)),
                Mode::Mermaid => format!(
                    "1-hop call graph for `{symbol}`:\n\n{}\n\n{}",
                    graph.format_mermaid(symbol),
                    caveat(&graph.callers(symbol))
                ),
            }
        };
        Outcome::Ok { content }
    }
}

// ========================================================================
// Mode Implementations
// ========================================================================

impl Mode {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "callees" => Some(Self::Callees),
            "callers" => Some(Self::Callers),
            "mermaid" => Some(Self::Mermaid),
            _ => None,
        }
    }
}

// ========================================================================
// Functions
// ========================================================================

/// Render an answer, with what it could not see attached.
fn render(symbol: &str, relation: &str, answer: &Answer<String>) -> String {
    let listing = if answer.items.is_empty() {
        format!("nothing found that `{symbol}` {relation}")
    } else {
        format!(
            "`{symbol}` {relation} ({}):\n{}",
            answer.items.len(),
            answer.items.join("\n")
        )
    };
    format!("{listing}\n\n{}", caveat(answer))
}

/// What the answer's completeness means, in words the model should act on.
///
/// **An empty list and a limited mechanism are different claims**, and the
/// difference decides whether deleting an apparently-uncalled function is safe.
fn caveat(answer: &Answer<String>) -> String {
    match answer.completeness {
        Completeness::Complete => String::new(),
        Completeness::BestEffort { reason } => format!(
            "(best-effort, via {:?}: {reason:?}. Symbols are matched by name, so \
             same-named methods on different types may be conflated and dynamic \
             or trait-dispatched calls are not seen. Do not treat an empty result \
             as proof that nothing calls this.)",
            answer.via
        ),
    }
}

/// Collect the source files under `dir` into `files`, through the broker.
///
/// Directory listing goes through `broker.read_dir` like every other
/// agent-driven access. It previously used `std::fs::read_dir`, which enumerated
/// the tree outside the policy the rest of the tool obeys.
async fn collect_source_files(
    cx: &ToolCx,
    dir: &std::path::Path,
    files: &mut FileSet,
    depth: usize,
) {
    if depth > MAX_DEPTH {
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
            Box::pin(collect_source_files(cx, &path, files, depth + 1)).await;
        } else if path.is_file() {
            files.insert(path);
        }
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_graph_tool_spec() {
        assert_eq!(GraphTool::new().spec().name, "graph");
    }

    #[test]
    fn an_unknown_mode_does_not_silently_become_a_diagram() {
        // `"callee"` is one character from `"callees"`, and the previous `_` arm
        // answered it with a mermaid diagram: a different question than asked,
        // reported as a success.
        assert_eq!(Mode::parse("callees"), Some(Mode::Callees));
        assert_eq!(Mode::parse("callers"), Some(Mode::Callers));
        assert_eq!(Mode::parse("mermaid"), Some(Mode::Mermaid));
        assert_eq!(Mode::parse("callee"), None);
    }

    #[test]
    fn an_empty_result_is_not_presented_as_proof() {
        // The hazard this tool used to be: "Callers of `foo` (0)" reads as a
        // fact, and a model may delete a function it believes is dead.
        let answer = Answer {
            completeness: Completeness::BestEffort {
                reason: query::BestEffortReason::ExtractorLimit,
            },
            items: Vec::new(),
            via: query::Mechanism::Facts {
                extractor: "tree-sitter",
            },
        };
        let rendered = render("foo", "is called by", &answer);
        assert!(rendered.contains("nothing found"), "{rendered}");
        assert!(rendered.contains("best-effort"), "{rendered}");
        assert!(
            rendered.contains("Do not treat an empty result"),
            "{rendered}"
        );
    }

    #[tokio::test]
    async fn multi_hop_graph_query_works() {
        use sandbox::Broker;
        use std::sync::Arc;
        use std::time::Instant;
        use tokio_util::sync::CancellationToken;

        let dir = tempfile::TempDir::new().unwrap();
        let file_path = dir.path().join("a.rs");
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
            .write(b"fn a() { b(); }\nfn b() { c(); }\nfn c() {}\n")
            .unwrap();

        let tool = GraphTool::new();
        let outcome = tool
            .call(
                serde_json::json!({
                    "symbol": "a",
                    "mode": "callees",
                    "depth": 2
                }),
                &cx,
            )
            .await;

        match outcome {
            Outcome::Ok { content } => {
                assert!(
                    content.contains("`a` calls (2 up to depth 2):"),
                    "{content}"
                );
                assert!(content.contains("- b (hop 1)"), "{content}");
                assert!(content.contains("- c (hop 2)"), "{content}");
            }
            other => panic!("expected Ok outcome, got {other:?}"),
        }
    }
}

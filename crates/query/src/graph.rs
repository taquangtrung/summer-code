//! Caller and callee queries over the facts an extractor produced.
//!
//! **The graph is keyed by symbol name, and the facts are not.** A fact names a
//! [`SymbolId`], which is a hash of a key that includes the module path; a
//! syntactic extractor cannot resolve that path, so the same function called
//! from two files carries two different ids. Joining on the id would therefore
//! return a caller list that stops at the file boundary, which is not what
//! "who calls this" means to anyone asking.
//!
//! So the join happens here, on the name, and the answer says what that cost.
//! Two same-named methods on different types collapse into one node, and a
//! caller list may include calls to the other one. That is a real limitation and
//! it is reported rather than hidden: every result comes back as an [`Answer`]
//! carrying [`Completeness`], and a name-joined graph is never `Complete`.
//!
//! A semantic extractor resolves module paths properly, and its facts join on
//! the id exactly. When one fills the store, this layer's join stops being the
//! weakest link without any change here.

use std::collections::{HashMap, HashSet};

use crate::extract::store::FactStore;
use crate::facts::Fact;
use crate::facts::ids::SymbolId;
use crate::query::{Answer, BestEffortReason, Completeness, Mechanism};

// ========================================================================
// Data Structures
// ========================================================================

/// A directed symbol dependency graph.
#[derive(Debug, Clone)]
pub struct CodeGraph {
    /// Maps symbol names/ids to symbols they call (callees).
    outgoing: HashMap<String, HashSet<String>>,
    /// Maps symbol names/ids to symbols calling them (callers).
    incoming: HashMap<String, HashSet<String>>,
    /// Maps symbol IDs to names.
    symbol_names: HashMap<SymbolId, String>,
    /// Which extractor's facts this was built from, for [`Answer::via`].
    via: Mechanism,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphNeighborhood {
    pub root: String,
    pub callers: Vec<String>,
    pub callees: Vec<String>,
}

// ========================================================================
// CodeGraph Implementations
// ========================================================================

impl Default for CodeGraph {
    /// An empty graph, attributed to no extractor.
    ///
    /// `Mechanism` has no meaningful default -- an answer has to say which
    /// mechanism produced it -- so a graph built edge by edge (which only tests
    /// do) reports the one that did so.
    fn default() -> Self {
        Self {
            incoming: HashMap::new(),
            outgoing: HashMap::new(),
            symbol_names: HashMap::new(),
            via: Mechanism::Facts {
                extractor: "manual",
            },
        }
    }
}

impl CodeGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a graph from an extracted [`FactStore`], produced by `extractor`.
    ///
    /// **Names come from the store's symbol table, not from the id.** An earlier
    /// version rendered each node with `format!("{symbol:?}")`, which is the
    /// `Debug` of a 64-bit content hash: every node was named `SymbolId(…)`, so
    /// a lookup by any name a user could type matched nothing and every query
    /// returned empty. A symbol with no registered key is skipped rather than
    /// given a hash for a name, because a node nobody can name is a node nobody
    /// can ask about.
    pub fn from_store(store: &FactStore, extractor: &'static str) -> Self {
        let mut graph = Self {
            via: Mechanism::Facts { extractor },
            ..Self::new()
        };

        for fact in store.facts() {
            if let Fact::Defines { symbol, .. } = fact
                && let Some(name) = store.name(*symbol)
            {
                graph.symbol_names.insert(*symbol, name.to_string());
            }
        }

        for fact in store.facts() {
            if let Fact::Calls { from, to, .. } = fact
                && let Some(caller) = store.name(*from)
                && let Some(callee) = store.name(*to)
            {
                let (caller, callee) = (caller.to_string(), callee.to_string());
                graph.add_edge(caller, callee);
            }
        }

        graph
    }

    /// Add a directed call edge from `caller` to `callee`.
    pub fn add_edge(&mut self, caller: impl Into<String>, callee: impl Into<String>) {
        let u = caller.into();
        let v = callee.into();

        self.outgoing
            .entry(u.clone())
            .or_default()
            .insert(v.clone());
        self.incoming.entry(v).or_default().insert(u);
    }

    /// Who calls `symbol`, and how much the answer can be trusted.
    pub fn callers(&self, symbol: &str) -> Answer<String> {
        self.answer(self.callers_of(symbol))
    }

    /// What `symbol` calls, and how much the answer can be trusted.
    pub fn callees(&self, symbol: &str) -> Answer<String> {
        self.answer(self.callees_of(symbol))
    }

    /// Wrap `items` in the completeness this graph can honestly claim.
    ///
    /// Never `Complete`: the join is by name, so a result may conflate
    /// same-named symbols and may miss a call the extractor could not see.
    /// Saying so is the point of the type.
    fn answer(&self, items: Vec<String>) -> Answer<String> {
        Answer {
            completeness: Completeness::BestEffort {
                reason: BestEffortReason::ExtractorLimit,
            },
            items,
            via: self.via,
        }
    }

    /// Find callers of `symbol`.
    pub fn callers_of(&self, symbol: &str) -> Vec<String> {
        self.incoming
            .get(symbol)
            .map(|set| {
                let mut v: Vec<_> = set.iter().cloned().collect();
                v.sort();
                v
            })
            .unwrap_or_default()
    }

    /// Find callees of `symbol`.
    pub fn callees_of(&self, symbol: &str) -> Vec<String> {
        self.outgoing
            .get(symbol)
            .map(|set| {
                let mut v: Vec<_> = set.iter().cloned().collect();
                v.sort();
                v
            })
            .unwrap_or_default()
    }

    /// Find callees up to `depth` hops away with cycle avoidance. Returns (symbol, hop_count).
    pub fn callees_of_depth(&self, symbol: &str, depth: usize) -> Vec<(String, usize)> {
        let mut results = Vec::new();
        let mut visited = HashSet::new();
        visited.insert(symbol.to_string());
        let mut queue = std::collections::VecDeque::new();
        queue.push_back((symbol.to_string(), 0));

        while let Some((curr, d)) = queue.pop_front() {
            if d >= depth {
                continue;
            }
            if let Some(callees) = self.outgoing.get(&curr) {
                let mut sorted: Vec<_> = callees.iter().cloned().collect();
                sorted.sort();
                for callee in sorted {
                    if visited.insert(callee.clone()) {
                        results.push((callee.clone(), d + 1));
                        queue.push_back((callee, d + 1));
                    }
                }
            }
        }
        results
    }

    /// Find callers up to `depth` hops away with cycle avoidance. Returns (symbol, hop_count).
    pub fn callers_of_depth(&self, symbol: &str, depth: usize) -> Vec<(String, usize)> {
        let mut results = Vec::new();
        let mut visited = HashSet::new();
        visited.insert(symbol.to_string());
        let mut queue = std::collections::VecDeque::new();
        queue.push_back((symbol.to_string(), 0));

        while let Some((curr, d)) = queue.pop_front() {
            if d >= depth {
                continue;
            }
            if let Some(callers) = self.incoming.get(&curr) {
                let mut sorted: Vec<_> = callers.iter().cloned().collect();
                sorted.sort();
                for caller in sorted {
                    if visited.insert(caller.clone()) {
                        results.push((caller.clone(), d + 1));
                        queue.push_back((caller, d + 1));
                    }
                }
            }
        }
        results
    }

    /// Extract 1-hop neighborhood for a target symbol.
    pub fn neighborhood(&self, symbol: &str) -> GraphNeighborhood {
        GraphNeighborhood {
            root: symbol.to_string(),
            callers: self.callers_of(symbol),
            callees: self.callees_of(symbol),
        }
    }

    /// Format neighborhood as a Mermaid flowchart diagram for LLM prompt injection.
    pub fn format_mermaid(&self, symbol: &str) -> String {
        self.format_mermaid_depth(symbol, 1)
    }

    /// Format multi-hop call graph as a Mermaid flowchart diagram up to `depth`.
    pub fn format_mermaid_depth(&self, symbol: &str, depth: usize) -> String {
        let mut out = String::from("```mermaid\ngraph TD\n");
        let mut edges = HashSet::new();
        let mut visited = HashSet::new();
        visited.insert(symbol.to_string());
        let mut queue = std::collections::VecDeque::new();
        queue.push_back((symbol.to_string(), 0));

        while let Some((curr, d)) = queue.pop_front() {
            if d >= depth {
                continue;
            }
            if let Some(callees) = self.outgoing.get(&curr) {
                for callee in callees {
                    edges.insert((curr.clone(), callee.clone()));
                    if visited.insert(callee.clone()) {
                        queue.push_back((callee.clone(), d + 1));
                    }
                }
            }
            if let Some(callers) = self.incoming.get(&curr) {
                for caller in callers {
                    edges.insert((caller.clone(), curr.clone()));
                    if visited.insert(caller.clone()) {
                        queue.push_back((caller.clone(), d + 1));
                    }
                }
            }
        }

        let mut sorted_edges: Vec<_> = edges.into_iter().collect();
        sorted_edges.sort();

        for (from, to) in &sorted_edges {
            out.push_str(&format!("    \"{from}\" --> \"{to}\"\n"));
        }

        if sorted_edges.is_empty() {
            out.push_str(&format!("    \"{symbol}\"\n"));
        }

        out.push_str("```");
        out
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse `source` into a store, then build a graph from it. The whole
    /// pipeline a `graph` query runs, minus the broker.
    fn graph_of(path: &str, source: &str) -> CodeGraph {
        let store = crate::extract::treesitter::facts_for_test(path, source);
        CodeGraph::from_store(&store, "tree-sitter")
    }

    #[test]
    fn a_caller_is_found_by_the_name_a_user_would_type() {
        // The bug this closes: nodes were named `format!("{symbol:?}")`, the
        // Debug of a content hash, so every lookup by a real name matched
        // nothing and the tool reported "0 callers" for everything.
        let graph = graph_of(
            "a.rs",
            "fn helper() -> u32 { 42 }\nfn caller() -> u32 { helper() }",
        );
        assert_eq!(graph.callers_of("helper"), vec!["caller"]);
        assert_eq!(graph.callees_of("caller"), vec!["helper"]);
    }

    #[test]
    fn an_answer_from_a_name_join_is_never_complete() {
        // The join conflates same-named symbols across types and modules. A
        // `Complete` label on that is the overclaim `Answer` exists to prevent.
        let graph = graph_of("a.rs", "fn a() { b(); }\nfn b() {}");
        let answer = graph.callers("b");
        assert_eq!(answer.items, vec!["a"]);
        assert!(!answer.is_complete(), "{:?}", answer.completeness);
        assert_eq!(
            answer.via,
            Mechanism::Facts {
                extractor: "tree-sitter"
            }
        );
    }

    #[test]
    fn an_empty_result_still_says_which_mechanism_looked() {
        // "No callers" from a mechanism that cannot see them, and "no callers"
        // because there really are none, are different claims. The answer
        // carries enough to tell them apart.
        let graph = graph_of("a.rs", "fn lonely() {}");
        let answer = graph.callers("lonely");
        assert!(answer.items.is_empty());
        assert!(!answer.is_complete());
    }

    #[test]
    fn builds_and_queries_call_graph() {
        let mut graph = CodeGraph::new();
        graph.add_edge("main", "parse_config");
        graph.add_edge("main", "run_engine");
        graph.add_edge("run_engine", "dispatch_tool");

        assert_eq!(graph.callees_of("main"), vec!["parse_config", "run_engine"]);
        assert_eq!(graph.callers_of("run_engine"), vec!["main"]);
        assert_eq!(graph.callees_of("run_engine"), vec!["dispatch_tool"]);
    }

    #[test]
    fn formats_mermaid_diagram_clearly() {
        let mut graph = CodeGraph::new();
        graph.add_edge("cli", "engine");
        graph.add_edge("engine", "sandbox");

        let diagram = graph.format_mermaid("engine");
        assert!(diagram.contains("```mermaid"));
        assert!(diagram.contains("\"cli\" --> \"engine\""));
        assert!(diagram.contains("\"engine\" --> \"sandbox\""));
    }

    #[test]
    fn traverses_multi_hop_call_graphs_with_cycle_avoidance() {
        let mut graph = CodeGraph::new();
        // A -> B -> C -> A (cycle)
        graph.add_edge("A", "B");
        graph.add_edge("B", "C");
        graph.add_edge("C", "A");

        let callees_2 = graph.callees_of_depth("A", 2);
        assert_eq!(callees_2, vec![("B".to_string(), 1), ("C".to_string(), 2)]);

        let callers_2 = graph.callers_of_depth("C", 2);
        assert_eq!(callers_2, vec![("B".to_string(), 1), ("A".to_string(), 2)]);

        let mermaid = graph.format_mermaid_depth("A", 2);
        assert!(mermaid.contains("\"A\" --> \"B\""));
        assert!(mermaid.contains("\"B\" --> \"C\""));
    }
}

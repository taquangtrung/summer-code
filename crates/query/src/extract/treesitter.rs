//! The syntactic extraction mechanism: real parses, via tree-sitter grammars.
//!
//! **One extractor, every grammar.** [`Extractor`] is implemented once per
//! mechanism, not per language, so this dispatches on the file's extension and
//! parses with whichever grammar fits. Adding a language is a grammar in the
//! table below and nothing else: no new extractor, no change to the fact schema,
//! and no query touched.
//!
//! # Why the module paths come out `Unresolved`
//!
//! A syntactic parse sees `foo()` and knows the text `foo`. It does not know
//! which `foo`, because that needs `use` resolution, trait dispatch, and type
//! inference, none of which a grammar performs. Emitting
//! [`ModulePath::Resolved`] would be a claim this mechanism cannot support: two
//! same-named functions in different modules would hash to one id and their
//! callers would silently merge.
//!
//! So every symbol here is [`ModulePath::Unresolved`], carrying the name as
//! written and the file it was written in. Those hash to distinct ids on
//! purpose. The consequence is deliberate and worth stating plainly: **call
//! edges from this extractor join only within a file.** A call in `a.rs` to a
//! function defined in `b.rs` produces two different ids, and no edge.
//!
//! That is not the end of the story, and it is not meant to be. Joining those
//! across files is a *query-layer* judgement -- match on name, accept that two
//! same-named methods on different types may be conflated, and say so through
//! [`Completeness::BestEffort`]. The semantic mechanism (LSP) resolves properly
//! and emits `Resolved` ids that join exactly. Both write into the same store,
//! which is what the content-addressed id is for.
//!
//! [`Completeness::BestEffort`]: crate::query::Completeness::BestEffort

use async_trait::async_trait;
use sandbox::ToolCx;
use tree_sitter::{Node, Parser, Tree};

use crate::extract::Extractor;
use crate::extract::store::{FactStore, FileSet};
use crate::facts::ids::{FileId, Location, ModulePath, Span, SymbolKey};
use crate::facts::symbol::{Language, SymbolKind};
use crate::facts::{Condition, Fact, FactKind};

// ========================================================================
// Constants
// ========================================================================

/// What this mechanism can produce.
const TREE_SITTER_KINDS: &[FactKind] = &[FactKind::Defines, FactKind::Calls];

const C_CALLS: &[&str] = &["call_expression"];

const C_DEFINITIONS: &[(&str, SymbolKind)] = &[
    ("enum_specifier", SymbolKind::Enum),
    ("function_definition", SymbolKind::Function),
    ("struct_specifier", SymbolKind::Struct),
    ("type_definition", SymbolKind::TypeAlias),
    ("union_specifier", SymbolKind::Struct),
];

const CPP_CALLS: &[&str] = &["call_expression"];

const CPP_DEFINITIONS: &[(&str, SymbolKind)] = &[
    ("alias_declaration", SymbolKind::TypeAlias),
    ("class_specifier", SymbolKind::Struct),
    ("enum_specifier", SymbolKind::Enum),
    ("function_definition", SymbolKind::Function),
    ("namespace_definition", SymbolKind::Module),
    ("struct_specifier", SymbolKind::Struct),
    ("type_definition", SymbolKind::TypeAlias),
    ("union_specifier", SymbolKind::Struct),
];

/// The ECMAScript family, shared by the JavaScript and TypeScript grammars.
const ECMA_CALLS: &[&str] = &["call_expression", "new_expression"];

const ECMA_DEFINITIONS: &[(&str, SymbolKind)] = &[
    ("class_declaration", SymbolKind::Struct),
    ("function_declaration", SymbolKind::Function),
    ("generator_function_declaration", SymbolKind::Function),
    ("interface_declaration", SymbolKind::Trait),
    ("method_definition", SymbolKind::Function),
    ("type_alias_declaration", SymbolKind::TypeAlias),
];

const GO_CALLS: &[&str] = &["call_expression"];

const GO_DEFINITIONS: &[(&str, SymbolKind)] = &[
    ("const_spec", SymbolKind::Constant),
    ("function_declaration", SymbolKind::Function),
    ("method_declaration", SymbolKind::Function),
    ("type_spec", SymbolKind::Struct),
    ("var_spec", SymbolKind::Variable),
];

const PYTHON_CALLS: &[&str] = &["call"];

const PYTHON_DEFINITIONS: &[(&str, SymbolKind)] = &[
    ("class_definition", SymbolKind::Struct),
    ("function_definition", SymbolKind::Function),
];

/// Grammar node names for a call site, per family.
const RUST_CALLS: &[&str] = &["call_expression", "macro_invocation"];

/// Grammar node names that introduce a definition, and what kind it is.
const RUST_DEFINITIONS: &[(&str, SymbolKind)] = &[
    ("const_item", SymbolKind::Constant),
    ("enum_item", SymbolKind::Enum),
    ("function_item", SymbolKind::Function),
    ("function_signature_item", SymbolKind::Function),
    ("macro_definition", SymbolKind::Macro),
    ("mod_item", SymbolKind::Module),
    ("static_item", SymbolKind::Constant),
    ("struct_item", SymbolKind::Struct),
    ("trait_item", SymbolKind::Trait),
    ("type_item", SymbolKind::TypeAlias),
    ("union_item", SymbolKind::Struct),
];

// ========================================================================
// Data Structures
// ========================================================================

/// Parses files with a tree-sitter grammar and emits definition and call facts.
pub struct TreeSitterExtractor;

/// One language's grammar and the node names that matter in it.
struct Grammar {
    calls: &'static [&'static str],
    definitions: &'static [(&'static str, SymbolKind)],
    language: Language,
}

/// What a single file's walk accumulates.
///
/// Carried rather than returned so the recursion stays a plain walk: the
/// enclosing definition is the one piece of state a call site needs, and
/// threading it as a parameter is what makes `from` correct without a second
/// pass.
struct Walk<'a> {
    facts: Vec<Fact>,
    file: FileId,
    grammar: &'a Grammar,
    source: &'a [u8],
}

// ========================================================================
// TreeSitterExtractor Implementations
// ========================================================================

impl TreeSitterExtractor {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TreeSitterExtractor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Extractor for TreeSitterExtractor {
    fn name(&self) -> &'static str {
        "tree-sitter"
    }

    fn kinds(&self) -> &'static [FactKind] {
        TREE_SITTER_KINDS
    }

    async fn extract(&self, cx: &ToolCx, files: &FileSet, store: &mut FactStore) {
        for file in files.ids() {
            let Some(path) = files.path(file) else {
                continue;
            };
            let Some(grammar) = Grammar::for_path(path) else {
                // No grammar for this extension. Not an error: the mechanism
                // declares what it covers, and a query says so via
                // `Completeness` rather than this pretending otherwise.
                continue;
            };
            // Through the broker, like every other read: extraction is
            // agent-driven and subject to the same policy as a tool call.
            let Ok(handle) = cx.broker.read(path).await else {
                continue;
            };
            let Ok(source) = handle.read_to_string() else {
                continue;
            };
            let Some(tree) = grammar.parse(&source) else {
                continue;
            };

            let mut walk = Walk {
                facts: Vec::new(),
                file,
                grammar,
                source: source.as_bytes(),
            };
            walk.visit(tree.root_node(), None, store);
            store.extend(walk.facts);
        }
    }
}

// ========================================================================
// Grammar Implementations
// ========================================================================

impl Grammar {
    /// The grammar for a path's extension, if there is one.
    fn for_path(path: &std::path::Path) -> Option<&'static Self> {
        /// One table, so adding a language is one entry.
        static C: Grammar = Grammar {
            calls: C_CALLS,
            definitions: C_DEFINITIONS,
            language: Language::C,
        };
        static CPP: Grammar = Grammar {
            calls: CPP_CALLS,
            definitions: CPP_DEFINITIONS,
            language: Language::Cpp,
        };
        static GO: Grammar = Grammar {
            calls: GO_CALLS,
            definitions: GO_DEFINITIONS,
            language: Language::Go,
        };
        static JAVASCRIPT: Grammar = Grammar {
            calls: ECMA_CALLS,
            definitions: ECMA_DEFINITIONS,
            language: Language::JavaScript,
        };
        static PYTHON: Grammar = Grammar {
            calls: PYTHON_CALLS,
            definitions: PYTHON_DEFINITIONS,
            language: Language::Python,
        };
        static RUST: Grammar = Grammar {
            calls: RUST_CALLS,
            definitions: RUST_DEFINITIONS,
            language: Language::Rust,
        };
        static TYPESCRIPT: Grammar = Grammar {
            calls: ECMA_CALLS,
            definitions: ECMA_DEFINITIONS,
            language: Language::TypeScript,
        };

        match language_for_path(path)? {
            Language::C => Some(&C),
            Language::Cpp => Some(&CPP),
            Language::Go => Some(&GO),
            Language::JavaScript => Some(&JAVASCRIPT),
            Language::Python => Some(&PYTHON),
            Language::Rust => Some(&RUST),
            Language::TypeScript => Some(&TYPESCRIPT),
            Language::Other => None,
        }
    }

    /// Parse `source`, or `None` if the grammar cannot be loaded.
    fn parse(&self, source: &str) -> Option<Tree> {
        let language = tree_sitter_language(self.language)?;
        let mut parser = Parser::new();
        parser.set_language(&language).ok()?;
        // tree-sitter recovers from syntax errors and returns a partial tree, so
        // an unparseable file still yields whatever it could read. That is the
        // opposite of the scanner's failure mode, which silently mis-parsed.
        parser.parse(source, None)
    }

    /// The kind this grammar assigns to a node, if it is a definition.
    fn definition_kind(&self, node: &Node) -> Option<SymbolKind> {
        self.definitions
            .iter()
            .find(|(name, _)| *name == node.kind())
            .map(|(_, kind)| *kind)
    }

    fn is_call(&self, node: &Node) -> bool {
        self.calls.contains(&node.kind())
    }
}

/// Detect the programming language for a given file path by extension.
pub fn language_for_path(path: &std::path::Path) -> Option<Language> {
    match path.extension()?.to_str()? {
        "c" | "h" => Some(Language::C),
        "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" => Some(Language::Cpp),
        "go" => Some(Language::Go),
        "js" | "jsx" | "mjs" | "cjs" => Some(Language::JavaScript),
        "py" | "pyi" => Some(Language::Python),
        "rs" => Some(Language::Rust),
        "ts" | "tsx" => Some(Language::TypeScript),
        _ => None,
    }
}

/// Get the tree-sitter language definition for a [`Language`].
pub fn tree_sitter_language(language: Language) -> Option<tree_sitter::Language> {
    match language {
        Language::C => Some(tree_sitter_c::LANGUAGE.into()),
        Language::Cpp => Some(tree_sitter_cpp::LANGUAGE.into()),
        Language::Go => Some(tree_sitter_go::LANGUAGE.into()),
        Language::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
        Language::Python => Some(tree_sitter_python::LANGUAGE.into()),
        Language::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
        Language::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        Language::Other => None,
    }
}

// ========================================================================
// Walk Implementations
// ========================================================================

impl<'a> Walk<'a> {
    /// Visit `node`, recording definitions and the calls inside them.
    ///
    /// `enclosing` is the definition this node sits inside, which is what a call
    /// site's `from` must be. A call at file scope has none and is skipped:
    /// `Fact::Calls` needs both ends, and inventing a synthetic module-level
    /// caller would put edges in the graph that name nothing.
    fn visit(&mut self, node: Node, enclosing: Option<SymbolKey>, store: &mut FactStore) {
        let mut here = enclosing;

        if let Some(kind) = self.grammar.definition_kind(&node)
            && let Some(name) = self.name_of(&node)
        {
            let key = self.key(kind, name);
            let symbol = store.register(key.clone());
            self.facts.push(Fact::Defines {
                at: self.location(&node),
                symbol,
            });
            here = Some(key);
        } else if self.grammar.is_call(&node)
            && let Some(from) = here.clone()
            && let Some(callee) = self.callee_of(&node)
        {
            let to = store.register(self.key(SymbolKind::Function, callee));
            self.facts.push(Fact::Calls {
                at: self.location(&node),
                from: store.register(from),
                to,
                // A `#[cfg]`-gated call is conditional, and a grammar can see
                // the attribute but not evaluate it. Reporting `Always` here
                // would be the overclaim `Condition` exists to prevent, so
                // resolving it is left to the semantic mechanism.
                when: Condition::Always,
            });
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.visit(child, here.clone(), store);
        }
    }

    /// The declared name of a definition node.
    fn name_of(&self, node: &Node) -> Option<String> {
        node_name(node, self.source)
    }

    /// The name being called at a call site.
    ///
    /// A method call's function is a field expression (`a.b()`), and the useful
    /// half is the final segment: `b`. Taking the whole text would make
    /// `self.parse()` and `other.parse()` different symbols, which is the
    /// opposite of what a caller lookup wants.
    fn callee_of(&self, node: &Node) -> Option<String> {
        let callee = node
            .child_by_field_name("function")
            .or_else(|| node.child_by_field_name("macro"))
            .or_else(|| node.child_by_field_name("constructor"))?;
        let text = self.text(&callee)?;
        let tail = text
            .rsplit(['.', ':'])
            .next()
            .unwrap_or(&text)
            .trim()
            .to_string();
        (!tail.is_empty()).then_some(tail)
    }

    fn text(&self, node: &Node) -> Option<String> {
        node.utf8_text(self.source).ok().map(str::to_string)
    }

    /// A key for `name`, always module-unresolved. See the module header.
    fn key(&self, kind: SymbolKind, name: String) -> SymbolKey {
        SymbolKey {
            kind,
            language: self.grammar.language,
            module: ModulePath::Unresolved {
                as_written: name.clone(),
                file: self.file,
            },
            name,
        }
    }

    fn location(&self, node: &Node) -> Location {
        Location::new(
            self.file,
            Span::new(node.start_byte() as u32, node.end_byte() as u32),
        )
    }
}

/// One definition, as an outline entry: what a symbol listing shows.
///
/// A projection of the same parse the facts come from, rendered for a person
/// (or a model) to read rather than for a query to join. Line numbers are
/// 1-based, matching what an editor and a compiler diagnostic both use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outline {
    pub kind: SymbolKind,
    pub line_end: usize,
    pub line_start: usize,
    pub name: String,
    /// The enclosing type, trait, or class name, if this definition is nested.
    pub parent: Option<String>,
    /// The declaration's first line, trimmed. The signature without the body,
    /// which is what makes a listing scannable.
    pub signature: String,
}

/// Every definition in `source`, parsed as `path`'s language.
///
/// Empty when no grammar covers the extension, which a caller should report as
/// "not covered" rather than "no symbols": the two are different answers.
pub fn outline(path: &std::path::Path, source: &str) -> Vec<Outline> {
    let Some(grammar) = Grammar::for_path(path) else {
        return Vec::new();
    };
    let Some(tree) = grammar.parse(source) else {
        return Vec::new();
    };
    let lines = LineIndex::new(source);
    let mut out = Vec::new();
    collect_outline(tree.root_node(), grammar, source, &lines, None, &mut out);
    out
}

/// Walk `node`, appending every definition it contains.
fn collect_outline(
    node: Node,
    grammar: &Grammar,
    source: &str,
    lines: &LineIndex,
    parent: Option<&str>,
    out: &mut Vec<Outline>,
) {
    let mut child_parent = parent.map(str::to_string);
    if let Some(kind) = grammar.definition_kind(&node)
        && let Some(name) = node_name(&node, source.as_bytes())
    {
        let text = node.utf8_text(source.as_bytes()).unwrap_or_default();
        out.push(Outline {
            kind,
            line_end: lines.line_of(node.end_byte()),
            line_start: lines.line_of(node.start_byte()),
            name: name.clone(),
            parent: parent.map(str::to_string),
            signature: text.lines().next().unwrap_or_default().trim().to_string(),
        });
        if matches!(
            kind,
            SymbolKind::Struct | SymbolKind::Trait | SymbolKind::Enum | SymbolKind::Module
        ) {
            child_parent = Some(name);
        }
    } else if let Some(scope) = scope_name(&node, source.as_bytes()) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            collect_outline(child, grammar, source, lines, Some(&scope), out);
        }
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_outline(child, grammar, source, lines, child_parent.as_deref(), out);
    }
}

fn node_name(node: &Node, source: &[u8]) -> Option<String> {
    if let Some(name) = node.child_by_field_name("name") {
        return name.utf8_text(source).ok().map(str::to_string);
    }
    if let Some(declarator) = node.child_by_field_name("declarator") {
        let mut curr = declarator;
        while let Some(inner) = curr.child_by_field_name("declarator") {
            curr = inner;
        }
        let text = curr.utf8_text(source).ok()?;
        let tail = text
            .rsplit(['.', ':'])
            .next()
            .unwrap_or(text)
            .trim()
            .to_string();
        if !tail.is_empty() {
            return Some(tail);
        }
    }
    None
}

fn scope_name(node: &Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "impl_item" => {
            let type_name = node
                .child_by_field_name("type")
                .and_then(|t| t.utf8_text(source).ok())?;
            if let Some(trait_node) = node.child_by_field_name("trait")
                && let Ok(trait_name) = trait_node.utf8_text(source)
            {
                Some(format!("{trait_name} for {type_name}"))
            } else {
                Some(type_name.to_string())
            }
        }
        "method_declaration" => {
            let receiver = node.child_by_field_name("receiver")?;
            let text = receiver.utf8_text(source).ok()?;
            let cleaned = text.trim_matches(['(', ')', '*', ' ']);
            let type_name = cleaned.split_whitespace().last().unwrap_or(cleaned);
            Some(type_name.to_string())
        }
        "class_declaration"
        | "class_definition"
        | "class_specifier"
        | "interface_declaration"
        | "struct_item"
        | "struct_specifier"
        | "trait_item"
        | "enum_item"
        | "enum_specifier"
        | "mod_item"
        | "namespace_definition" => node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source).ok())
            .map(str::to_string),
        _ => None,
    }
}

/// Byte offset to 1-based line number.
///
/// Built once per file. Tree-sitter reports byte offsets and every consumer
/// wants lines; scanning the source per node would make an outline quadratic in
/// file size.
struct LineIndex {
    /// The byte offset each line starts at, ascending.
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(source: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(
            source
                .bytes()
                .enumerate()
                .filter(|(_, byte)| *byte == b'\n')
                .map(|(at, _)| at + 1),
        );
        // A source ending in a newline yields a final start at `source.len()`,
        // which begins no line. Left in, an offset at end-of-file reports a
        // phantom line one past the content -- and tree-sitter's `end_byte` is
        // exactly such an offset for any node that ends at EOF, so every
        // last-in-file symbol would report the wrong end line.
        if starts.last() == Some(&source.len()) && starts.len() > 1 {
            starts.pop();
        }
        Self { starts }
    }

    fn line_of(&self, offset: usize) -> usize {
        // The line whose start is the last one at or before `offset`.
        self.starts.partition_point(|start| *start <= offset).max(1)
    }
}

/// The byte extent of every elidable body in `source`, sorted and disjoint.
///
/// "Elidable" means a function or method body: the implementation a skeleton
/// drops. A type's fields are deliberately not included, because they *are* the
/// interface a skeleton exists to show.
///
/// Only outermost bodies are returned. A nested function's body lies inside its
/// parent's, and returning both would make a single-pass elision emit the inner
/// span twice.
pub fn body_spans(path: &std::path::Path, source: &str) -> Vec<(usize, usize)> {
    let Some(grammar) = Grammar::for_path(path) else {
        return Vec::new();
    };
    let Some(tree) = grammar.parse(source) else {
        return Vec::new();
    };
    let mut spans = Vec::new();
    collect_bodies(tree.root_node(), grammar, &mut spans);
    spans.sort_unstable();
    spans
}

/// Walk `node`, appending outermost function bodies and not descending into one.
fn collect_bodies(node: Node, grammar: &Grammar, spans: &mut Vec<(usize, usize)>) {
    let is_callable = grammar
        .definition_kind(&node)
        .is_some_and(|kind| kind == SymbolKind::Function);
    if is_callable && let Some(body) = node.child_by_field_name("body") {
        spans.push((body.start_byte(), body.end_byte()));
        // Not descending: an inner body is inside this one, already elided.
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_bodies(child, grammar, spans);
    }
}

/// Parse `source` as `path` into a store, for tests in this crate.
///
/// The trait's `extract` needs a `ToolCx` to reach the broker; a test whose
/// subject is the parse or a query over it should not have to stage one.
#[cfg(test)]
pub(crate) fn facts_for_test(path: &str, source: &str) -> FactStore {
    let mut store = FactStore::new();
    let Some(grammar) = Grammar::for_path(std::path::Path::new(path)) else {
        return store;
    };
    let Some(tree) = grammar.parse(source) else {
        return store;
    };
    let mut walk = Walk {
        facts: Vec::new(),
        file: FileId::new(0),
        grammar,
        source: source.as_bytes(),
    };
    walk.visit(tree.root_node(), None, &mut store);
    store.extend(walk.facts);
    store
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse `source` as `path` and return the facts, without a broker.
    ///
    /// The trait's `extract` needs a `ToolCx` to read files; every assertion
    /// below is about the *parse*, so the walk is driven directly and the I/O
    /// seam stays out of it.
    fn facts_for(path: &str, source: &str) -> (Vec<Fact>, FactStore) {
        let grammar = Grammar::for_path(std::path::Path::new(path)).expect("a known extension");
        let tree = grammar.parse(source).expect("the grammar loads");
        let mut store = FactStore::new();
        let mut walk = Walk {
            facts: Vec::new(),
            file: FileId::new(0),
            grammar,
            source: source.as_bytes(),
        };
        walk.visit(tree.root_node(), None, &mut store);
        (walk.facts, store)
    }

    fn defined_names(facts: &[Fact], store: &FactStore) -> Vec<String> {
        let mut names: Vec<String> = facts
            .iter()
            .filter_map(|fact| match fact {
                Fact::Defines { symbol, .. } => store.name(*symbol).map(str::to_string),
                _ => None,
            })
            .collect();
        names.sort();
        names
    }

    fn call_edges(facts: &[Fact], store: &FactStore) -> Vec<(String, String)> {
        let mut edges: Vec<(String, String)> = facts
            .iter()
            .filter_map(|fact| match fact {
                Fact::Calls { from, to, .. } => {
                    Some((store.name(*from)?.to_string(), store.name(*to)?.to_string()))
                }
                _ => None,
            })
            .collect();
        edges.sort();
        edges
    }

    #[test]
    fn a_keyword_in_a_string_is_not_a_definition() {
        // The scanner this replaces used `line.contains("fn ")`, so this file
        // reported two definitions that do not exist. A grammar sees a
        // `string_literal` node and never descends into it as code.
        let source = r#"
            fn real() -> u32 { 1 }
            fn holder() { let s = "fn fake() {}"; let t = "struct Fake;"; }
        "#;
        let (facts, store) = facts_for("a.rs", source);
        assert_eq!(defined_names(&facts, &store), ["holder", "real"]);
    }

    #[test]
    fn a_keyword_in_a_comment_is_not_a_definition() {
        // The scanner only skipped lines *starting* with a comment prefix, so a
        // trailing comment mentioning `fn` still produced a phantom symbol.
        let source = r#"
            fn real() {} // TODO: add fn helper() later
            // fn commented_out() {}
        "#;
        let (facts, store) = facts_for("a.rs", source);
        assert_eq!(defined_names(&facts, &store), ["real"]);
    }

    #[test]
    fn call_edges_carry_the_enclosing_function_as_their_source() {
        // The fact the graph is built from. `from` must be the function the call
        // sits inside, not the file or the previous definition.
        let source = r#"
            fn helper() -> u32 { 42 }
            fn caller() -> u32 { helper() + helper() }
        "#;
        let (facts, store) = facts_for("a.rs", source);
        assert_eq!(
            call_edges(&facts, &store),
            [
                ("caller".to_string(), "helper".to_string()),
                ("caller".to_string(), "helper".to_string()),
            ],
            "both call sites belong to `caller`"
        );
    }

    #[test]
    fn a_method_call_is_recorded_under_its_final_segment() {
        // `self.parse()` and `other.parse()` must be the same callee name, or a
        // caller lookup for `parse` finds neither.
        let source = r#"
            fn run(&self, other: &Thing) { self.parse(); other.parse(); }
        "#;
        let (facts, store) = facts_for("a.rs", source);
        let callees: Vec<String> = call_edges(&facts, &store)
            .into_iter()
            .map(|(_, to)| to)
            .collect();
        assert_eq!(callees, ["parse", "parse"]);
    }

    #[test]
    fn a_call_at_file_scope_produces_no_edge() {
        // An edge needs both ends. A synthetic module-level caller would put a
        // node in the graph that names nothing a user could look up.
        let source = "let x = compute();";
        let (facts, store) = facts_for("a.js", source);
        assert!(call_edges(&facts, &store).is_empty(), "{facts:?}");
    }

    #[test]
    fn python_definitions_and_calls_are_found() {
        let source = "
def helper():
    return 42

def caller():
    return helper()
";
        let (facts, store) = facts_for("a.py", source);
        assert_eq!(defined_names(&facts, &store), ["caller", "helper"]);
        assert_eq!(
            call_edges(&facts, &store),
            [("caller".to_string(), "helper".to_string())]
        );
    }

    #[test]
    fn typescript_definitions_and_calls_are_found() {
        let source = "
            function helper(): number { return 42; }
            function caller(): number { return helper(); }
        ";
        let (facts, store) = facts_for("a.ts", source);
        assert_eq!(defined_names(&facts, &store), ["caller", "helper"]);
        assert_eq!(
            call_edges(&facts, &store),
            [("caller".to_string(), "helper".to_string())]
        );
    }

    #[test]
    fn go_definitions_and_calls_are_found() {
        let source = "
package main

func helper() int { return 42 }
func caller() int { return helper() }
";
        let (facts, store) = facts_for("a.go", source);
        assert_eq!(defined_names(&facts, &store), ["caller", "helper"]);
        assert_eq!(
            call_edges(&facts, &store),
            [("caller".to_string(), "helper".to_string())]
        );
    }

    #[test]
    fn c_and_cpp_definitions_and_calls_are_found() {
        let c_source = "
int helper(void) { return 42; }
int caller(void) { return helper(); }
";
        let (facts, store) = facts_for("a.c", c_source);
        assert_eq!(defined_names(&facts, &store), ["caller", "helper"]);
        assert_eq!(
            call_edges(&facts, &store),
            [("caller".to_string(), "helper".to_string())]
        );

        let cpp_source = "
namespace math {
    int helper() { return 42; }
    int caller() { return helper(); }
}
";
        let (facts_cpp, store_cpp) = facts_for("a.cpp", cpp_source);
        assert!(defined_names(&facts_cpp, &store_cpp).contains(&"helper".to_string()));
        assert!(defined_names(&facts_cpp, &store_cpp).contains(&"caller".to_string()));
    }

    #[test]
    fn a_file_with_a_syntax_error_still_yields_what_parsed() {
        // tree-sitter recovers rather than failing whole. A half-typed file is
        // the normal state of a file an agent is editing, and returning nothing
        // for it would make the tool useless exactly when it is needed.
        let source = r#"
            fn good() -> u32 { 1 }
            fn broken( -> {{{
        "#;
        let (facts, store) = facts_for("a.rs", source);
        assert!(
            defined_names(&facts, &store).contains(&"good".to_string()),
            "{facts:?}"
        );
    }

    #[test]
    fn an_outline_carries_names_kinds_and_line_numbers() {
        let source = "fn first() {}\n\nstruct Second {\n    field: u32,\n}\n";
        let entries = outline(std::path::Path::new("a.rs"), source);
        let named: Vec<(&str, SymbolKind, usize)> = entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.kind, entry.line_start))
            .collect();
        assert_eq!(
            named,
            [
                ("first", SymbolKind::Function, 1),
                ("Second", SymbolKind::Struct, 3),
            ]
        );
    }

    #[test]
    fn an_outline_captures_enclosing_type_scopes() {
        let rust_source = "struct Foo;\nimpl Foo {\n    pub fn bar(&self) {}\n}\n";
        let entries = outline(std::path::Path::new("a.rs"), rust_source);
        let bar = entries
            .iter()
            .find(|e| e.name == "bar")
            .expect("bar exists");
        assert_eq!(bar.parent.as_deref(), Some("Foo"));

        let py_source = "class Greeter:\n    def greet(self):\n        pass\n";
        let py_entries = outline(std::path::Path::new("a.py"), py_source);
        let greet = py_entries
            .iter()
            .find(|e| e.name == "greet")
            .expect("greet exists");
        assert_eq!(greet.parent.as_deref(), Some("Greeter"));
    }

    #[test]
    fn an_outline_signature_is_the_declaration_not_the_body() {
        // A listing is only scannable if each line is one declaration. The
        // scanner this replaces accumulated lines until the braces balanced, so
        // a multi-line function printed its whole body into the listing.
        let source = "fn wide(\n    a: u32,\n) -> u32 {\n    a + 1\n}\n";
        let entries = outline(std::path::Path::new("a.rs"), source);
        assert_eq!(entries[0].signature, "fn wide(");
        assert_eq!(entries[0].line_start, 1);
        assert_eq!(entries[0].line_end, 5, "the span still covers the body");
    }

    #[test]
    fn line_numbers_are_one_based_and_survive_a_trailing_newline() {
        // Off-by-one here sends the model to the wrong line of the right file,
        // which is worse than no location at all.
        let index = LineIndex::new("a\nbb\nccc\n");
        assert_eq!(index.line_of(0), 1, "first byte is line 1");
        assert_eq!(index.line_of(2), 2, "first byte of line 2");
        assert_eq!(index.line_of(5), 3);
        assert_eq!(index.line_of(9), 3, "the last byte is still on line 3");
    }

    #[test]
    fn an_outline_of_an_uncovered_extension_is_empty() {
        assert!(outline(std::path::Path::new("notes.txt"), "fn nope() {}").is_empty());
    }

    #[test]
    fn an_unknown_extension_has_no_grammar() {
        assert!(Grammar::for_path(std::path::Path::new("notes.txt")).is_none());
        assert!(Grammar::for_path(std::path::Path::new("Makefile")).is_none());
    }

    #[test]
    fn the_same_name_in_two_files_does_not_join() {
        // The documented consequence of `ModulePath::Unresolved`. Asserted so it
        // stays a deliberate property rather than becoming a surprise: joining
        // these is a query-layer judgement, reported as best-effort, not
        // something this mechanism may claim.
        let one = SymbolKey {
            kind: SymbolKind::Function,
            language: Language::Rust,
            module: ModulePath::Unresolved {
                as_written: "parse".into(),
                file: FileId::new(0),
            },
            name: "parse".into(),
        };
        let two = SymbolKey {
            module: ModulePath::Unresolved {
                as_written: "parse".into(),
                file: FileId::new(1),
            },
            ..one.clone()
        };
        assert_ne!(one.id(), two.id());
    }
}

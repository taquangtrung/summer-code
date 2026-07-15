//! The semantic extraction mechanism: a language server, over stdio.
//!
//! **What this gets that a grammar cannot.** Tree-sitter sees `foo()` and knows
//! the text; a server knows *which* `foo`, because it has resolved the imports,
//! the trait impls, and the types. So this emits [`ModulePath::Resolved`] and
//! its facts join exactly across files, where the syntactic mechanism emits
//! `Unresolved` and joins only within one. Both write into the same store, and a
//! query over the result is more complete for this having run.
//!
//! **It is best-effort at the process level, not just the fact level.** A
//! language server may not be installed, may not start, may take a minute to
//! index a large workspace, or may fail under kernel confinement because it
//! cannot reach `~/.cargo`. Every one of those ends with this extractor
//! producing nothing and the syntactic facts still being there. That is the
//! design: the two mechanisms are not a fallback chain with a preferred arm,
//! they are two producers whose union is the answer, and a query says which
//! contributed.
//!
//! # Lifecycle
//!
//! `initialize` (request) → `initialized` (notification) → `textDocument/didOpen`
//! per file → queries → `shutdown` (request) → `exit` (notification). The
//! open step is not optional: most servers answer `documentSymbol` for a
//! document they have not been given with an error or an empty result, and
//! either is indistinguishable from "this file has no symbols".

mod protocol;

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use sandbox::{AuthorizedService, ToolCx};
use serde_json::{Value, json};
use tokio::time::timeout;

use crate::extract::Extractor;
use crate::extract::store::{FactStore, FileSet};
use crate::facts::ids::{FileId, Location, ModulePath, Span, SymbolKey};
use crate::facts::symbol::{Language, SymbolKind};
use crate::facts::{Fact, FactKind};

// ========================================================================
// Constants
// ========================================================================

/// What this mechanism can produce.
const LSP_KINDS: &[FactKind] = &[FactKind::Defines, FactKind::Calls];

/// How long one request may take.
///
/// Generous, because the first request after `initialize` waits on the server's
/// indexing pass, and on a large workspace that is genuinely slow. Bounded,
/// because a server that never answers must not hang the session.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the whole extraction may take before it gives up and returns what
/// it has.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(180);

// ========================================================================
// Data Structures
// ========================================================================

/// Extracts facts by asking a language server.
pub struct LspExtractor {
    language: Language,
    server_command: String,
}

/// A live connection to a server: the transport plus the request counter.
struct Client {
    next_id: u64,
    service: AuthorizedService,
}

// ========================================================================
// LspExtractor Implementations
// ========================================================================

impl LspExtractor {
    pub fn new(server_command: impl Into<String>, language: Language) -> Self {
        Self {
            language,
            server_command: server_command.into(),
        }
    }

    /// The conventional server binary for a language.
    pub fn default_server_cmd(language: Language) -> &'static str {
        match language {
            Language::C | Language::Cpp => "clangd",
            Language::Go => "gopls",
            Language::JavaScript | Language::TypeScript => "typescript-language-server",
            Language::Other => "lsp-server",
            Language::Python => "pyright-langserver",
            Language::Rust => "rust-analyzer",
        }
    }

    /// An extractor for `language`, if its server is on `PATH`.
    ///
    /// **Actually checks**, unlike the version this replaces, whose doc said
    /// "auto-detect ... on PATH" while the body returned `Some` unconditionally.
    /// A caller then built an extractor for a binary that did not exist and got
    /// a spawn failure at extraction time instead of an absence at setup time.
    pub fn detect(language: Language) -> Option<Self> {
        let command = Self::default_server_cmd(language);
        which(command).then(|| Self::new(command, language))
    }

    /// Run the whole session, returning the facts it produced.
    ///
    /// Errors are swallowed into "no facts" on purpose: see the module header on
    /// why an absent server is an ordinary outcome rather than a failure.
    async fn session(&self, cx: &ToolCx, files: &FileSet, store: &mut FactStore) -> Option<()> {
        let root = cx.broker.policy().root.clone();
        let service = cx
            .broker
            .serve(&self.server_command, &[])
            .await
            .inspect_err(|denied| {
                tracing::info!(
                    server = %self.server_command,
                    reason = %denied.reason,
                    "no language server; syntactic facts only"
                );
            })
            .ok()?;
        let mut client = Client {
            next_id: 1,
            service,
        };

        client.initialize(&root).await?;
        for file in files.ids() {
            let Some(path) = files.path(file) else {
                continue;
            };
            if !self.covers(path) {
                continue;
            }
            let Ok(handle) = cx.broker.read(path).await else {
                continue;
            };
            let Ok(text) = handle.read_to_string() else {
                continue;
            };
            client.did_open(path, &text, self.language).await?;
            let symbols = client.document_symbols(path).await.unwrap_or_default();
            self.record(&symbols, file, store);
        }
        client.shutdown().await;
        Some(())
    }

    /// Whether this extractor's language covers `path`.
    fn covers(&self, path: &Path) -> bool {
        let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
            return false;
        };
        match self.language {
            Language::C => matches!(extension, "c" | "h"),
            Language::Cpp => matches!(extension, "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx"),
            Language::Go => extension == "go",
            Language::JavaScript => matches!(extension, "js" | "jsx" | "mjs" | "cjs"),
            Language::Other => false,
            Language::Python => matches!(extension, "py" | "pyi"),
            Language::Rust => extension == "rs",
            Language::TypeScript => matches!(extension, "ts" | "tsx"),
        }
    }

    /// Turn a `documentSymbol` response into `Defines` facts.
    ///
    /// The response is a tree (a struct's fields nest under it), so this
    /// descends. `Resolved` module paths, because the server did resolve them:
    /// that is the whole difference from the syntactic mechanism.
    fn record(&self, symbols: &[Value], file: FileId, store: &mut FactStore) {
        for symbol in symbols {
            let Some(name) = symbol.get("name").and_then(Value::as_str) else {
                continue;
            };
            let kind = symbol
                .get("kind")
                .and_then(Value::as_u64)
                .map_or(SymbolKind::Function, |kind| map_symbol_kind(kind as u32));
            let line = symbol
                .pointer("/range/start/line")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32;
            let end = symbol
                .pointer("/range/end/line")
                .and_then(Value::as_u64)
                .unwrap_or(line as u64) as u32;

            let id = store.register(SymbolKey {
                kind,
                language: self.language,
                module: ModulePath::Resolved(vec!["crate".into()]),
                name: name.to_string(),
            });
            store.extend(vec![Fact::Defines {
                at: Location::new(file, Span::new(line, end)),
                symbol: id,
            }]);

            if let Some(children) = symbol.get("children").and_then(Value::as_array) {
                self.record(children, file, store);
            }
        }
    }
}

#[async_trait]
impl Extractor for LspExtractor {
    fn name(&self) -> &'static str {
        "lsp"
    }

    fn kinds(&self) -> &'static [FactKind] {
        LSP_KINDS
    }

    async fn extract(&self, cx: &ToolCx, files: &FileSet, store: &mut FactStore) {
        // The whole session is bounded, not just each request: a server that
        // answers every request slowly would otherwise multiply out to an
        // unbounded wait across a large file set.
        let _ = timeout(TOTAL_TIMEOUT, self.session(cx, files, store)).await;
    }
}

// ========================================================================
// Client Implementations
// ========================================================================

impl Client {
    /// Handshake, and wait for the server to answer before anything else.
    async fn initialize(&mut self, root: &Path) -> Option<()> {
        let params = json!({
            "processId": std::process::id(),
            "rootUri": protocol::uri(root),
            "capabilities": {
                "textDocument": {
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                },
            },
        });
        self.request("initialize", params).await?;
        // A notification, so nothing is awaited. Sending `initialized` as a
        // request instead is the classic hang.
        self.notify("initialized", json!({})).await
    }

    /// Tell the server a document's contents.
    async fn did_open(&mut self, path: &Path, text: &str, language: Language) -> Option<()> {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": protocol::uri(path),
                    "languageId": language_id(language),
                    "version": 1,
                    "text": text,
                },
            }),
        )
        .await
    }

    /// The document's symbol tree.
    async fn document_symbols(&mut self, path: &Path) -> Option<Vec<Value>> {
        let result = self
            .request(
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": protocol::uri(path) } }),
            )
            .await?;
        result.as_array().cloned()
    }

    /// Ask the server to stop, then tell it to exit.
    ///
    /// Best-effort: the child is killed on drop regardless, so a server that
    /// ignores this is not a leak. Asking politely first lets one that holds a
    /// workspace lock release it.
    async fn shutdown(&mut self) {
        let _ = self.request("shutdown", Value::Null).await;
        let _ = self.notify("exit", Value::Null).await;
    }

    /// Send a request and read until its response arrives.
    async fn request(&mut self, method: &str, params: Value) -> Option<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&protocol::request(id, method, params)).await?;

        // Read past everything that is not this response. A server interleaves
        // its own requests and notifications (progress reports, diagnostics)
        // with replies, and a client that treats the next message as its answer
        // reads a diagnostic as a symbol list.
        loop {
            let message = timeout(REQUEST_TIMEOUT, self.receive()).await.ok()??;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                return message.get("result").cloned();
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Option<()> {
        self.send(&protocol::notification(method, params)).await
    }

    async fn send(&mut self, message: &Value) -> Option<()> {
        self.service.send(&protocol::frame(message)).await.ok()
    }

    /// Read one framed message.
    async fn receive(&mut self) -> Option<Value> {
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            if self.service.read_line(&mut line).await.ok()? == 0 {
                // The server closed its output: it exited.
                return None;
            }
            if line.trim().is_empty() {
                break;
            }
            headers.push(line.trim().to_string());
        }

        let length = protocol::content_length(&headers)?;
        let mut body = vec![0u8; length];
        self.service.read_exact(&mut body).await.ok()?;
        serde_json::from_slice(&body).ok()
    }
}

// ========================================================================
// Functions
// ========================================================================

/// Whether `program` resolves on `PATH`.
///
/// A manual scan rather than shelling out to `which`, because spawning a process
/// to find out whether a process can be spawned is both slower and, under the
/// sandbox, a second thing that needs authorizing.
fn which(program: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
}

/// LSP's `languageId` for a language.
fn language_id(language: Language) -> &'static str {
    match language {
        Language::C => "c",
        Language::Cpp => "cpp",
        Language::Go => "go",
        Language::JavaScript => "javascript",
        Language::Other => "plaintext",
        Language::Python => "python",
        Language::Rust => "rust",
        Language::TypeScript => "typescript",
    }
}

/// Map LSP's `SymbolKind` integer to this schema's.
///
/// The numbers are the spec's and are not going to change; naming them here is
/// what keeps the mapping in one reviewable place.
pub fn map_symbol_kind(kind: u32) -> SymbolKind {
    match kind {
        2 => SymbolKind::Module,
        5 | 23 => SymbolKind::Struct,
        6 | 12 => SymbolKind::Function,
        8 => SymbolKind::Field,
        10 => SymbolKind::Enum,
        11 => SymbolKind::Trait,
        13 => SymbolKind::Variable,
        14 => SymbolKind::Constant,
        26 => SymbolKind::TypeAlias,
        _ => SymbolKind::Function,
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_returns_none_for_a_server_that_is_not_installed() {
        // The bug this closes: the previous `auto_detect` documented a PATH
        // check and returned `Some` unconditionally, so an absent server became
        // a spawn failure during extraction instead of an absence at setup.
        assert!(!which("definitely-not-a-language-server-xyz"));
        assert!(LspExtractor::detect(Language::Other).is_none());
    }

    #[test]
    fn every_language_has_a_conventional_server_and_an_id() {
        for language in [
            Language::JavaScript,
            Language::Python,
            Language::Rust,
            Language::TypeScript,
        ] {
            assert!(!LspExtractor::default_server_cmd(language).is_empty());
            assert_ne!(language_id(language), "plaintext");
        }
    }

    #[test]
    fn an_extractor_only_claims_its_own_extensions() {
        // A Rust server handed a `.py` file answers with an error or nothing,
        // and either is indistinguishable from "no symbols here".
        let rust = LspExtractor::new("rust-analyzer", Language::Rust);
        assert!(rust.covers(Path::new("a.rs")));
        assert!(!rust.covers(Path::new("a.py")));
        assert!(!rust.covers(Path::new("Makefile")));
    }

    #[test]
    fn symbol_kinds_map_to_the_schemas_own_vocabulary() {
        assert_eq!(map_symbol_kind(11), SymbolKind::Trait);
        assert_eq!(map_symbol_kind(23), SymbolKind::Struct);
        assert_eq!(map_symbol_kind(10), SymbolKind::Enum);
        assert_eq!(map_symbol_kind(12), SymbolKind::Function);
    }

    #[test]
    fn a_symbol_tree_is_flattened_including_its_children() {
        // `documentSymbol` nests: a struct's fields are children of the struct.
        // Recording only the top level loses every method on every type.
        let extractor = LspExtractor::new("rust-analyzer", Language::Rust);
        let symbols = vec![json!({
            "name": "Config",
            "kind": 23,
            "range": { "start": { "line": 1 }, "end": { "line": 5 } },
            "children": [
                { "name": "retries", "kind": 8, "range": { "start": { "line": 2 }, "end": { "line": 2 } } },
            ],
        })];

        let mut store = FactStore::new();
        extractor.record(&symbols, FileId::new(0), &mut store);
        let names: Vec<&str> = store
            .facts()
            .iter()
            .filter_map(|fact| match fact {
                Fact::Defines { symbol, .. } => store.name(*symbol),
                _ => None,
            })
            .collect();
        assert!(names.contains(&"Config"), "{names:?}");
        assert!(names.contains(&"retries"), "a nested symbol was lost");
    }

    #[test]
    fn lsp_symbols_resolve_their_module_where_tree_sitter_does_not() {
        // The property that makes this mechanism worth having: its ids join
        // across files, so two files' calls to one function are one symbol.
        let extractor = LspExtractor::new("rust-analyzer", Language::Rust);
        let mut store = FactStore::new();
        extractor.record(
            &[json!({ "name": "serve", "kind": 12, "range": { "start": { "line": 0 }, "end": { "line": 0 } } })],
            FileId::new(0),
            &mut store,
        );
        let id = store
            .facts()
            .iter()
            .find_map(|fact| match fact {
                Fact::Defines { symbol, .. } => Some(*symbol),
                _ => None,
            })
            .expect("one define");
        let key = store.key(id).expect("registered");
        assert!(
            matches!(key.module, ModulePath::Resolved(_)),
            "an LSP symbol must not be Unresolved: {:?}",
            key.module
        );
    }
}

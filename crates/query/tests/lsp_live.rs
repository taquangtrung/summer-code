//! The LSP extractor against a real language server.
//!
//! **Skipped when `rust-analyzer` is not installed**, rather than failing. A
//! test that requires a language server on every machine that runs `cargo test`
//! would be a test people delete; one that reports what it skipped is one they
//! keep. The skip prints, so a silent no-op cannot masquerade as a pass.
//!
//! What this covers that the unit tests cannot: the framing survives a real
//! server's interleaved progress notifications, the handshake completes, and the
//! symbols that come back are the ones in the file.

// Test-fixture fs (staging a crate for the server to read), not agent-driven.
#![allow(clippy::disallowed_methods)]

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use sandbox::{Broker, ToolCx};
use summer_query::{Extractor, Fact, FactStore, FileSet, Language, LspExtractor, ModulePath};
use tokio_util::sync::CancellationToken;

/// Whether `program` is on `PATH`.
fn installed(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

/// A minimal crate for the server to index, in a temp directory.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src")).expect("src");
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub struct Config {\n    pub retries: u32,\n}\n\npub fn serve() -> u32 {\n    7\n}\n",
    )
    .expect("source");
    dir
}

fn cx_at(root: &Path) -> ToolCx {
    ToolCx::new(
        Arc::new(Broker::permissive(root.to_path_buf())),
        Arc::from(root),
        CancellationToken::new(),
        Instant::now(),
    )
}

#[tokio::test]
async fn rust_analyzer_answers_with_the_symbols_in_the_file() {
    if !installed("rust-analyzer") {
        println!("SKIPPED: rust-analyzer is not on PATH");
        return;
    }

    let dir = fixture();
    let cx = cx_at(dir.path());
    let mut files = FileSet::new();
    files.insert(dir.path().join("src/lib.rs"));

    let mut store = FactStore::new();
    LspExtractor::new("rust-analyzer", Language::Rust)
        .extract(&cx, &files, &mut store)
        .await;

    let names: Vec<&str> = store
        .facts()
        .iter()
        .filter_map(|fact| match fact {
            Fact::Defines { symbol, .. } => store.name(*symbol),
            _ => None,
        })
        .collect();

    assert!(
        names.contains(&"serve") && names.contains(&"Config"),
        "the server returned {names:?}, which should include `serve` and `Config`"
    );
}

#[tokio::test]
async fn lsp_symbols_carry_resolved_module_paths() {
    // The property that makes the semantic mechanism worth running alongside
    // the syntactic one: its ids join across files, where tree-sitter's are
    // deliberately file-scoped.
    if !installed("rust-analyzer") {
        println!("SKIPPED: rust-analyzer is not on PATH");
        return;
    }

    let dir = fixture();
    let cx = cx_at(dir.path());
    let mut files = FileSet::new();
    files.insert(dir.path().join("src/lib.rs"));

    let mut store = FactStore::new();
    LspExtractor::new("rust-analyzer", Language::Rust)
        .extract(&cx, &files, &mut store)
        .await;

    let resolved = store.facts().iter().any(|fact| match fact {
        Fact::Defines { symbol, .. } => store
            .key(*symbol)
            .is_some_and(|key| matches!(key.module, ModulePath::Resolved(_))),
        _ => false,
    });
    assert!(resolved, "no symbol came back with a resolved module path");
}

#[tokio::test]
async fn an_absent_server_yields_no_facts_rather_than_failing() {
    // The design claim in the module header: a missing server is an ordinary
    // outcome, and the syntactic facts are still there. This must hold on every
    // machine, so it does not depend on anything being installed.
    let dir = fixture();
    let cx = cx_at(dir.path());
    let mut files = FileSet::new();
    files.insert(dir.path().join("src/lib.rs"));

    let mut store = FactStore::new();
    LspExtractor::new("definitely-not-a-language-server-xyz", Language::Rust)
        .extract(&cx, &files, &mut store)
        .await;

    assert!(store.is_empty(), "an absent server produced facts");
}

#[tokio::test]
async fn the_live_test_is_not_passing_vacuously() {
    // A guard on the guard. `installed` finds a file on PATH, which under
    // rustup is a shim that fails for some flags; if it ever stops being a
    // working server, the two tests above must fail loudly rather than quietly
    // asserting over an empty set.
    if !installed("rust-analyzer") {
        println!("SKIPPED: rust-analyzer is not on PATH");
        return;
    }
    let dir = fixture();
    let cx = cx_at(dir.path());
    let mut files = FileSet::new();
    files.insert(dir.path().join("src/lib.rs"));

    let mut store = FactStore::new();
    LspExtractor::new("rust-analyzer", Language::Rust)
        .extract(&cx, &files, &mut store)
        .await;

    let names: Vec<&str> = store
        .facts()
        .iter()
        .filter_map(|fact| match fact {
            Fact::Defines { symbol, .. } => store.name(*symbol),
            _ => None,
        })
        .collect();
    println!(
        "rust-analyzer returned {} symbol(s): {names:?}",
        names.len()
    );
    assert!(
        !names.is_empty(),
        "the server is on PATH but returned nothing: the live tests would be vacuous"
    );
}

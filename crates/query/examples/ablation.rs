//! The Phase 5 ablation: grep alone, then grep plus facts. Links `query` alone
//! and needs no agent loop, so it runs as a spike that decides a design
//! question.
//!
//! Run it with `cargo run -p summer-query --example ablation`. It seeds a tiny
//! repo in a temp directory, then answers the same question ("where is `serve`
//! defined, and who might call it?") both ways and prints what each could and
//! could not establish.
//!
//! **There was a third arm, and it is gone rather than stubbed.** It ran a null
//! embedder whose only behaviour was to report embeddings as disabled, which
//! demonstrated nothing a comment could not: no real embedder was ever written,
//! so the arm compared grep against an absence. The two mechanisms this project
//! actually ships are tree-sitter and LSP, and the ablation now compares the one
//! it has against the baseline it beats.

// Harness code: walking the fixture repo is setup, not agent-driven I/O. Reads
// of file contents during extraction go through the broker.
#![allow(clippy::disallowed_methods)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use sandbox::{Broker, ToolCx};
use summer_query::{Extractor, FactKind, FileSet, TreeSitterExtractor, grep};
use tokio_util::sync::CancellationToken;

// ========================================================================
// Functions
// ========================================================================

#[tokio::main]
async fn main() {
    let repo = seed_repo();
    let files = collect_rust_files(&repo);
    let cx = context(&repo);

    let pattern = "serve";

    println!("== mode 1: grep only ==");
    let hits = grep(&cx, &files, pattern).await;
    println!("  {} textual hit(s) for `{pattern}`:", hits.len());
    for hit in &hits {
        println!("    line {}: {}", hit.line + 1, hit.text.trim());
    }
    println!("  (grep finds text anywhere: comments, strings, call sites. it");
    println!("   cannot tell a definition from a call without parsing.)");

    println!();
    println!("== mode 2: grep + facts ==");
    let mut store = summer_query::FactStore::new();
    TreeSitterExtractor::new()
        .extract(&cx, &files, &mut store)
        .await;
    let defines: Vec<_> = store
        .facts()
        .iter()
        .filter(|fact| fact.kind() == FactKind::Defines)
        .collect();
    println!("  {} Defines fact(s) extracted:", defines.len());
    for fact in &defines {
        let summer_query::Fact::Defines { symbol, at } = fact else {
            continue;
        };
        let path = files
            .path(at.file)
            .map(Path::display)
            .map(|p| p.to_string())
            .unwrap_or_default();
        let name = store.name(*symbol).unwrap_or("<unnamed>");
        println!("    {name} at {}:{}", path, at.span.start);
    }
    println!("  (facts give exact symbols and joinable ids; grep and embeddings");
    println!("   do not. a richer extractor adds calls/type-flow on the same schema.)");

    println!();
    println!("verdict: see docs/ARCHITECTURE.md");
}

/// Seed a tiny repo so the spike is self-contained.
fn seed_repo() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "summer-ablation-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("server.rs"),
        "// the entry point\nfn serve() { listen(); }\nfn listen() {}\n",
    )
    .unwrap();
    std::fs::write(dir.join("callers.rs"), "fn main() { serve(); }\n").unwrap();
    dir
}

/// Walk the repo for `.rs` files. Harness setup, not agent-driven extraction.
fn collect_rust_files(repo: &Path) -> FileSet {
    let mut files = FileSet::new();
    walk(repo, &mut |path| {
        if path.extension().is_some_and(|ext| ext == "rs") {
            files.insert(path);
        }
    });
    files
}

fn walk(dir: &Path, visit: &mut impl FnMut(PathBuf)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, visit);
        } else {
            visit(path);
        }
    }
}

/// A permissive broker rooted at the repo, so extraction's reads are subject to
/// the same policy as a tool call (and pass it here).
fn context(repo: &Path) -> ToolCx {
    ToolCx::new(
        Arc::new(Broker::permissive(repo.to_path_buf())),
        Arc::from(repo),
        CancellationToken::new(),
        Instant::now() + std::time::Duration::from_secs(60),
    )
}

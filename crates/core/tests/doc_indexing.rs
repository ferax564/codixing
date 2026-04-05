//! Integration test: index a repo with markdown docs and verify search finds them.

use std::fs;

use tempfile::TempDir;

use codixing_core::{DocFilter, Engine, IndexConfig, SearchQuery, Strategy};

/// Return an `IndexConfig` with embeddings disabled (BM25-only mode).
fn bm25_config(root: &std::path::Path) -> IndexConfig {
    let mut cfg = IndexConfig::new(root);
    cfg.embedding.enabled = false;
    cfg
}

#[test]
fn index_and_search_markdown_doc() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    // Create a minimal Rust file.
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/engine.rs"),
        "/// Initialize the engine.\npub fn init() { todo!() }\n",
    )
    .unwrap();

    // Create a markdown doc that references the code.
    fs::write(
        root.join("README.md"),
        "# My Project\n\n## Getting Started\n\nCall `init()` to start the engine.\n\n## Installation\n\nRun `cargo install`.\n",
    )
    .unwrap();

    // Index with embeddings disabled to avoid model downloads.
    let engine = Engine::init(root, bm25_config(root)).unwrap();

    // Search for a doc concept.
    let results = engine
        .search(
            SearchQuery::new("getting started")
                .with_limit(5)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();

    // Should find the README section.
    assert!(
        results.iter().any(|r| r.file_path.contains("README.md")),
        "Expected README.md in results, got: {:?}",
        results.iter().map(|r| &r.file_path).collect::<Vec<_>>()
    );

    // Verify scope_chain contains section breadcrumb.
    let doc_result = results
        .iter()
        .find(|r| r.file_path.contains("README.md"))
        .unwrap();
    assert!(
        !doc_result.scope_chain.is_empty(),
        "Expected non-empty scope_chain for doc result"
    );
}

#[test]
fn doc_type_filter_works() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn search() {}\n").unwrap();
    fs::write(root.join("README.md"), "# Search\n\nHow to search.\n").unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    // Search with docs-only filter.
    let sq = SearchQuery::new("search")
        .with_limit(10)
        .with_strategy(Strategy::Instant)
        .with_doc_filter(DocFilter::DocsOnly);
    let results = engine.search(sq).unwrap();

    for r in &results {
        assert!(
            r.language == "Markdown" || r.language == "HTML",
            "Expected only doc results, got language={}",
            r.language
        );
    }
}

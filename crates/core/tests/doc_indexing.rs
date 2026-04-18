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
            r.language == "Markdown" || r.language == "HTML" || r.language == "reStructuredText",
            "Expected only doc results, got language={}",
            r.language
        );
    }
}

#[test]
fn index_and_search_rst_doc() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/engine.rs"),
        "/// Initialize the engine.\npub fn init() { todo!() }\n",
    )
    .unwrap();

    fs::write(
        root.join("guide.rst"),
        "Project Guide\n=============\n\nIntro paragraph describing the project.\n\nGetting Started\n---------------\n\nCall ``init()`` to start the engine. See ``add_chunk`` for details.\n\nInstallation\n------------\n\nRun ``cargo install`` to build.\n",
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let results = engine
        .search(
            SearchQuery::new("getting started")
                .with_limit(5)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();

    assert!(
        results.iter().any(|r| r.file_path.contains("guide.rst")),
        "Expected guide.rst in results, got: {:?}",
        results.iter().map(|r| &r.file_path).collect::<Vec<_>>()
    );

    let doc_result = results
        .iter()
        .find(|r| r.file_path.contains("guide.rst"))
        .unwrap();
    assert!(
        !doc_result.scope_chain.is_empty(),
        "Expected non-empty scope_chain for RST doc result"
    );
    assert_eq!(doc_result.language, "reStructuredText");
}

#[test]
fn index_and_search_asciidoc() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    fs::write(
        root.join("guide.adoc"),
        "= Project Guide\n\nIntro paragraph describing the project.\n\n== Getting Started\n\nCall `init()` to start the engine. See `add_chunk` for details.\n\n== Installation\n\nRun `cargo install` to build.\n",
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let results = engine
        .search(
            SearchQuery::new("getting started")
                .with_limit(5)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();

    assert!(
        results.iter().any(|r| r.file_path.contains("guide.adoc")),
        "Expected guide.adoc in results, got: {:?}",
        results.iter().map(|r| &r.file_path).collect::<Vec<_>>()
    );
    let hit = results
        .iter()
        .find(|r| r.file_path.contains("guide.adoc"))
        .unwrap();
    assert_eq!(hit.language, "AsciiDoc");
    assert!(hit.is_doc());
}

#[test]
fn index_and_search_plain_text_readme() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    // README with no extension — canonical plain-text project metadata.
    fs::write(
        root.join("README"),
        "My Cool Project\n\nThis is a short description of the project goals.\n\nIt supports reproducible builds and comes with sensible defaults.\n",
    )
    .unwrap();
    // Code so the index doesn't collapse into the readme-only path.
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn build() { todo!() }\n").unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let sq = SearchQuery::new("reproducible builds")
        .with_limit(5)
        .with_strategy(Strategy::Instant)
        .with_doc_filter(DocFilter::DocsOnly);
    let results = engine.search(sq).unwrap();

    assert!(
        results.iter().any(|r| r.file_path.contains("README")),
        "Expected README in docs-only results, got: {:?}",
        results.iter().map(|r| &r.file_path).collect::<Vec<_>>()
    );
    let hit = results
        .iter()
        .find(|r| r.file_path.contains("README"))
        .unwrap();
    assert_eq!(hit.language, "Plain text");
}

#[test]
fn changelog_mode_returns_version_section_not_whole_file() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    fs::write(
        root.join("CHANGELOG.md"),
        "# Changelog\n\nAll notable changes.\n\n## [0.40.0] — 2026-04-18\n\n### Added\n\n- RST indexing\n- AsciiDoc and plain-text support\n\n## [0.39.0] — 2026-04-18\n\n### Added\n\n- LSP tests\n\n## [0.38.0] — 2026-04-15\n\n### Fixed\n\n- Windows Tantivy flakes\n",
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    // Search for content that only appears in the v0.40 release block.
    let results = engine
        .search(
            SearchQuery::new("RST indexing AsciiDoc")
                .with_limit(5)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();

    assert!(
        results.iter().any(|r| r.file_path.contains("CHANGELOG.md")),
        "Expected CHANGELOG.md in results"
    );
    let hit = results
        .iter()
        .find(|r| r.file_path.contains("CHANGELOG.md"))
        .unwrap();
    // The v0.40 section should dominate the result — its heading is in
    // scope_chain. (Small release sections may be merged by the doc
    // chunker, so the chunk body can still contain adjacent versions;
    // the breadcrumb is the more reliable signal.)
    let breadcrumb = hit.scope_chain.join(" > ");
    assert!(
        breadcrumb.contains("0.40.0"),
        "Expected 0.40.0 in scope_chain, got: {}",
        breadcrumb
    );
    assert!(
        hit.content.contains("RST indexing"),
        "Expected v0.40 content in body"
    );
}

#[test]
fn rst_docs_only_filter() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn search() {}\n").unwrap();
    fs::write(
        root.join("docs.rst"),
        "Search\n======\n\nHow to search the index.\n",
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let sq = SearchQuery::new("search")
        .with_limit(10)
        .with_strategy(Strategy::Instant)
        .with_doc_filter(DocFilter::DocsOnly);
    let results = engine.search(sq).unwrap();

    assert!(
        results.iter().any(|r| r.file_path.contains("docs.rst")),
        "Expected docs.rst in docs-only filtered results"
    );
    for r in &results {
        assert!(
            r.language == "Markdown" || r.language == "HTML" || r.language == "reStructuredText",
            "Expected only doc results, got language={}",
            r.language
        );
    }
}

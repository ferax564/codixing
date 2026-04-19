//! Integration test: index a Jupyter notebook and verify per-cell dispatch.
//!
//! The notebook dispatcher parses `.ipynb` JSON, routes code cells through
//! tree-sitter by `metadata.kernelspec.language`, routes markdown cells
//! through the Markdown `DocLanguageSupport`, and skips raw + output cells.
//! Output cells frequently contain secrets leaked by execution; this test
//! asserts they do not land in any chunk's searchable content.

use std::fs;

use tempfile::TempDir;

use codixing_core::{Engine, IndexConfig, SearchQuery, Strategy};

fn bm25_config(root: &std::path::Path) -> IndexConfig {
    let mut cfg = IndexConfig::new(root);
    cfg.embedding.enabled = false;
    cfg
}

/// Fixture with one markdown cell, one python code cell (with an output
/// stream carrying a synthetic secret), and one raw cell.
const SAMPLE_NOTEBOOK: &str = r##"{
  "nbformat": 4, "nbformat_minor": 5,
  "metadata": {"kernelspec": {"language": "python", "name": "python3"}},
  "cells": [
    {"cell_type": "markdown", "id": "intro",
     "source": "# Widget Analysis\n\nThis notebook explores widget batch processing."},
    {"cell_type": "code", "id": "impl",
     "source": "def process_widget(w):\n    return w * 2\n",
     "outputs": [{"output_type": "stream", "text": "SECRET_TOKEN_abc123"}]},
    {"cell_type": "raw", "id": "legacy",
     "source": "verbatim_raw_placeholder_abracadabra"}
  ]
}"##;

fn contains_file(results: &[impl AsRef<str>], needle: &str) -> bool {
    results.iter().any(|s| s.as_ref().contains(needle))
}

#[test]
fn index_and_search_jupyter_notebook_cells() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::write(root.join("analysis.ipynb"), SAMPLE_NOTEBOOK).unwrap();
    // Add a sibling code file so the index has at least one non-notebook chunk.
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn hello() {}\n").unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    // Markdown cell content is searchable, attributed to the real notebook path.
    let md_hits = engine
        .search(
            SearchQuery::new("widget analysis batch")
                .with_limit(10)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();
    let md_paths: Vec<_> = md_hits.iter().map(|r| r.file_path.clone()).collect();
    assert!(
        contains_file(&md_paths, "analysis.ipynb"),
        "expected analysis.ipynb in markdown results, got {md_paths:?}",
    );

    // Code cell symbol is findable.
    let code_hits = engine
        .search(
            SearchQuery::new("process_widget")
                .with_limit(10)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();
    let code_paths: Vec<_> = code_hits.iter().map(|r| r.file_path.clone()).collect();
    assert!(
        contains_file(&code_paths, "analysis.ipynb"),
        "expected process_widget hit in analysis.ipynb, got {code_paths:?}",
    );

    // Scope chain on a notebook hit starts with `cell-<id>`.
    let nb_hit = md_hits
        .iter()
        .find(|r| r.file_path.contains("analysis.ipynb"))
        .expect("notebook hit");
    assert!(
        nb_hit
            .scope_chain
            .first()
            .is_some_and(|s| s.starts_with("cell-")),
        "expected scope_chain to start with cell-<id>, got {:?}",
        nb_hit.scope_chain,
    );
}

#[test]
fn jupyter_output_and_raw_cells_do_not_contaminate_chunks() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::write(root.join("analysis.ipynb"), SAMPLE_NOTEBOOK).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn hello() {}\n").unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    // The synthetic secret from the output cell must not land in any
    // chunk produced for the notebook.
    let leak_hits = engine
        .search(
            SearchQuery::new("SECRET_TOKEN_abc123")
                .with_limit(10)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();
    let leak_contents: Vec<_> = leak_hits.iter().map(|r| r.content.clone()).collect();
    let notebook_leak = leak_hits
        .iter()
        .any(|r| r.file_path.contains("analysis.ipynb"));
    assert!(
        !notebook_leak,
        "output cell text leaked into notebook chunks: {leak_contents:?}",
    );

    // Raw cell body should not be emitted as chunk content either.
    let raw_hits = engine
        .search(
            SearchQuery::new("verbatim_raw_placeholder_abracadabra")
                .with_limit(10)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();
    let raw_leak = raw_hits
        .iter()
        .any(|r| r.file_path.contains("analysis.ipynb"));
    assert!(
        !raw_leak,
        "raw cell content leaked into notebook chunks: {:?}",
        raw_hits.iter().map(|r| &r.content).collect::<Vec<_>>(),
    );
}

#[test]
fn malformed_notebook_does_not_abort_init() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::write(root.join("broken.ipynb"), "not json at all").unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn hello() {}\n").unwrap();

    let engine = Engine::init(root, bm25_config(root));
    assert!(
        engine.is_ok(),
        "init should succeed even when one notebook is malformed: {:?}",
        engine.err()
    );

    // The sibling code file must still be searchable.
    let hits = engine
        .unwrap()
        .search(
            SearchQuery::new("hello")
                .with_limit(10)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();
    let paths: Vec<_> = hits.iter().map(|r| r.file_path.clone()).collect();
    assert!(
        paths.iter().any(|p| p.contains("src/lib.rs")),
        "expected src/lib.rs in results after malformed notebook, got {paths:?}",
    );
}

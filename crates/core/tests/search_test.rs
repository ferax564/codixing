//! Integration tests for BM25 search.
//!
//! These tests focus on BM25 correctness and use `Strategy::Instant` to
//! ensure they do not depend on the embedding model being downloaded.

mod common;

use std::sync::Mutex;

use codixing_core::{Engine, IndexConfig, SearchQuery, Strategy};
use tempfile::tempdir;

/// Create an `IndexConfig` with embeddings disabled (BM25-only mode).
fn bm25_config(root: &std::path::Path) -> IndexConfig {
    let mut cfg = IndexConfig::new(root);
    cfg.embedding.enabled = false;
    cfg
}

#[test]
fn search_finds_rust_function() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let results = engine
        .search(
            SearchQuery::new("add")
                .with_limit(10)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();

    assert!(!results.is_empty(), "expected search results for 'add'");
    assert!(
        results.iter().any(|r| r.file_path.contains("main.rs")),
        "expected at least one result from main.rs, got files: {:?}",
        results.iter().map(|r| &r.file_path).collect::<Vec<_>>()
    );
}

#[test]
fn search_finds_python_class() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let results = engine
        .search(
            SearchQuery::new("Validator")
                .with_limit(10)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();

    assert!(
        !results.is_empty(),
        "expected search results for 'Validator'"
    );
    assert!(
        results.iter().any(|r| r.file_path.contains("utils.py")),
        "expected at least one result from utils.py, got files: {:?}",
        results.iter().map(|r| &r.file_path).collect::<Vec<_>>()
    );
}

#[test]
fn search_finds_typescript_class() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let results = engine
        .search(
            SearchQuery::new("App")
                .with_limit(10)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();

    assert!(!results.is_empty(), "expected search results for 'App'");
    assert!(
        results.iter().any(|r| r.file_path.contains("index.ts")),
        "expected at least one result from index.ts, got files: {:?}",
        results.iter().map(|r| &r.file_path).collect::<Vec<_>>()
    );
}

#[test]
fn search_with_file_filter() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    // Search with a file filter that restricts to Python files.
    let results = engine
        .search(
            SearchQuery::new("Validator")
                .with_limit(10)
                .with_strategy(Strategy::Instant)
                .with_file_filter(".py"),
        )
        .unwrap();

    for result in &results {
        assert!(
            result.file_path.ends_with(".py"),
            "expected only .py results when file_filter is '.py', got: {}",
            result.file_path
        );
    }

    // Search with a file filter for Rust files should not return Python results.
    let results = engine
        .search(
            SearchQuery::new("Validator")
                .with_limit(10)
                .with_strategy(Strategy::Instant)
                .with_file_filter(".rs"),
        )
        .unwrap();

    assert!(
        results.is_empty(),
        "expected no results when searching for 'Validator' in .rs files"
    );
}

#[test]
fn search_no_results() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let results = engine
        .search(
            SearchQuery::new("xyzzy_nonexistent_gibberish_42")
                .with_limit(10)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();

    assert!(
        results.is_empty(),
        "expected no results for nonsense query, got {} results",
        results.len()
    );
}

#[test]
fn search_with_progress_reports_bm25_phase() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let phases = Mutex::new(Vec::new());
    let query = SearchQuery::new("add")
        .with_limit(10)
        .with_strategy(Strategy::Instant);

    let results = engine.search_with_progress(query, |phase, partial| {
        phases
            .lock()
            .unwrap()
            .push((phase.to_string(), partial.len()));
    });

    assert!(results.is_ok());
    let results = results.unwrap();
    assert!(!results.is_empty(), "expected results for 'add'");

    let phases = phases.into_inner().unwrap();
    assert!(!phases.is_empty(), "should have at least one phase");
    assert_eq!(phases[0].0, "bm25", "first phase should be BM25");
    assert!(phases[0].1 > 0, "BM25 phase should have results");

    // Instant strategy should only produce the bm25 phase.
    assert_eq!(phases.len(), 1, "Instant should have exactly one phase");
}

#[test]
fn search_with_progress_reports_fused_phase_for_fast() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let phases = Mutex::new(Vec::new());
    let query = SearchQuery::new("add")
        .with_limit(10)
        .with_strategy(Strategy::Fast);

    let results = engine.search_with_progress(query, |phase, partial| {
        phases
            .lock()
            .unwrap()
            .push((phase.to_string(), partial.len()));
    });

    assert!(results.is_ok());

    let phases = phases.into_inner().unwrap();
    assert!(
        phases.len() >= 2,
        "Fast strategy should report at least bm25 + fused phases, got: {phases:?}"
    );
    assert_eq!(phases[0].0, "bm25", "first phase should be BM25");
    assert_eq!(phases[1].0, "fused", "second phase should be fused");
}

#[test]
fn search_with_progress_no_results_still_reports_bm25() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let phases = Mutex::new(Vec::new());
    let query = SearchQuery::new("xyzzy_nonexistent_gibberish_42")
        .with_limit(10)
        .with_strategy(Strategy::Instant);

    let results = engine.search_with_progress(query, |phase, partial| {
        phases
            .lock()
            .unwrap()
            .push((phase.to_string(), partial.len()));
    });

    assert!(results.is_ok());
    let phases = phases.into_inner().unwrap();
    assert_eq!(
        phases.len(),
        1,
        "even with no results, bm25 phase should fire"
    );
    assert_eq!(phases[0].0, "bm25");
    assert_eq!(phases[0].1, 0, "no results expected for nonsense query");
}

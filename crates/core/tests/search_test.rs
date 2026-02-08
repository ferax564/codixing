//! Integration tests for BM25 search.

mod common;

use codeforge_core::{Engine, IndexConfig, SearchQuery};
use tempfile::tempdir;

#[test]
fn search_finds_rust_function() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let config = IndexConfig::new(root);
    let engine = Engine::init(root, config).unwrap();

    let results = engine
        .search(SearchQuery::new("add").with_limit(10))
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

    let config = IndexConfig::new(root);
    let engine = Engine::init(root, config).unwrap();

    let results = engine
        .search(SearchQuery::new("Validator").with_limit(10))
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

    let config = IndexConfig::new(root);
    let engine = Engine::init(root, config).unwrap();

    let results = engine
        .search(SearchQuery::new("App").with_limit(10))
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

    let config = IndexConfig::new(root);
    let engine = Engine::init(root, config).unwrap();

    // Search with a file filter that restricts to Python files.
    let results = engine
        .search(
            SearchQuery::new("Validator")
                .with_limit(10)
                .with_file_filter(".py"),
        )
        .unwrap();

    // All results should be from .py files.
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

    let config = IndexConfig::new(root);
    let engine = Engine::init(root, config).unwrap();

    let results = engine
        .search(SearchQuery::new("xyzzy_nonexistent_gibberish_42").with_limit(10))
        .unwrap();

    assert!(
        results.is_empty(),
        "expected no results for nonsense query, got {} results",
        results.len()
    );
}

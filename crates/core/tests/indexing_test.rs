//! Integration tests for multi-language indexing.

mod common;

use std::fs;

use codeforge_core::{Engine, IndexConfig, SearchQuery};
use tempfile::tempdir;

#[test]
fn multi_language_indexing() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let config = IndexConfig::new(root);
    let engine = Engine::init(root, config).unwrap();
    let stats = engine.stats();

    assert!(
        stats.file_count >= 5,
        "expected at least 5 files indexed, got {}",
        stats.file_count
    );
    assert!(
        stats.chunk_count > 0,
        "expected at least 1 chunk, got {}",
        stats.chunk_count
    );
    assert!(
        stats.symbol_count > 0,
        "expected at least 1 symbol, got {}",
        stats.symbol_count
    );
}

#[test]
fn respects_language_filter() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let mut config = IndexConfig::new(root);
    config.languages.insert("rust".to_string());

    let engine = Engine::init(root, config).unwrap();
    let stats = engine.stats();

    // Only the two Rust files (main.rs, lib.rs) should be indexed.
    assert_eq!(
        stats.file_count, 2,
        "expected exactly 2 Rust files indexed, got {}",
        stats.file_count
    );

    // Verify no Python/TypeScript/Go symbols are present.
    let py_syms = engine.symbols("parse_config", None).unwrap();
    assert!(
        py_syms.is_empty(),
        "expected no Python symbols when filtering to Rust only"
    );
}

#[test]
fn exclude_patterns_work() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    // Create files in a node_modules directory (should be excluded by default).
    let nm_dir = root.join("node_modules").join("some-lib");
    fs::create_dir_all(&nm_dir).unwrap();
    fs::write(
        nm_dir.join("index.ts"),
        "export function libHelper(): void {}",
    )
    .unwrap();

    let config = IndexConfig::new(root);
    let engine = Engine::init(root, config).unwrap();

    // The node_modules file should NOT be indexed.
    let syms = engine.symbols("libHelper", None).unwrap();
    assert!(
        syms.is_empty(),
        "expected node_modules files to be excluded from indexing"
    );

    // But normal files should be indexed.
    let syms = engine.symbols("add", None).unwrap();
    assert!(
        !syms.is_empty(),
        "expected normal project files to be indexed"
    );
}

#[test]
fn reindex_updates_existing() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let config = IndexConfig::new(root);
    let mut engine = Engine::init(root, config).unwrap();

    // Verify the initial content is searchable.
    let results = engine
        .search(SearchQuery::new("add").with_limit(10))
        .unwrap();
    assert!(!results.is_empty(), "expected 'add' to be searchable");

    // Modify main.rs to add a new unique function.
    fs::write(
        root.join("src/main.rs"),
        r#"/// Entry point for the application.
fn main() {
    println!("Modified!");
}

/// A unique function added during reindex test.
pub fn reindex_sentinel_function() -> bool {
    true
}
"#,
    )
    .unwrap();

    engine
        .reindex_file(std::path::Path::new("src/main.rs"))
        .unwrap();

    // The new function should be searchable.
    let results = engine
        .search(SearchQuery::new("reindex_sentinel_function").with_limit(5))
        .unwrap();
    assert!(
        !results.is_empty(),
        "expected newly added function to be searchable after reindex"
    );
}

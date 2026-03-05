//! Integration tests for multi-language indexing.

mod common;

use std::collections::HashSet;
use std::fs;

use codeforge_core::{Engine, IndexConfig, SearchQuery, Strategy};
use tempfile::tempdir;

/// Return an `IndexConfig` with embeddings disabled.
/// Integration tests for indexing correctness don't need vector embeddings
/// and should not trigger model downloads.
fn no_embed_config(root: &std::path::Path) -> IndexConfig {
    let mut cfg = IndexConfig::new(root);
    cfg.embedding.enabled = false;
    cfg
}

#[test]
fn multi_language_indexing() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let engine = Engine::init(root, no_embed_config(root)).unwrap();
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

    let mut config = no_embed_config(root);
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

    let engine = Engine::init(root, no_embed_config(root)).unwrap();

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

    let mut engine = Engine::init(root, no_embed_config(root)).unwrap();

    // Verify the initial content is searchable.
    let results = engine
        .search(
            SearchQuery::new("add")
                .with_limit(10)
                .with_strategy(Strategy::Instant),
        )
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
        .search(
            SearchQuery::new("reindex_sentinel_function")
                .with_limit(5)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();
    assert!(
        !results.is_empty(),
        "expected newly added function to be searchable after reindex"
    );
}

/// Verify that `extra_roots` causes files from a second directory to be indexed
/// alongside the primary root.  Paths from the extra root must be prefixed with
/// the extra root's directory base name so they remain distinct.
#[test]
fn multi_root_indexes_both_roots() {
    let tmp1 = tempdir().unwrap();
    let tmp2 = tempdir().unwrap();

    // Write a Rust file to root 1 (primary).
    fs::write(
        tmp1.path().join("auth.rs"),
        r#"
/// Authenticate a user token.
pub fn authenticate(token: &str) -> bool {
    !token.is_empty()
}
"#,
    )
    .unwrap();

    // Write a Rust file to root 2 (extra root).
    fs::write(
        tmp2.path().join("payments.rs"),
        r#"
/// Charge a payment card.
pub fn charge_card(amount: f64, card: &str) -> Result<(), String> {
    let _ = (amount, card);
    Ok(())
}
"#,
    )
    .unwrap();

    let mut config = IndexConfig::new(tmp1.path());
    config.extra_roots = vec![tmp2.path().to_path_buf()];
    config.embedding.enabled = false;

    let engine = Engine::init(tmp1.path(), config).expect("init failed");

    let stats = engine.stats();
    assert_eq!(
        stats.file_count, 2,
        "expected 2 files total (one per root), got {}",
        stats.file_count
    );

    // The extra-root file path must carry the directory prefix.
    let extra_prefix = tmp2
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let all_files: HashSet<String> = engine
        .symbols("", None)
        .unwrap()
        .into_iter()
        .map(|s| s.file_path)
        .collect();

    // auth.rs is from the primary root — no prefix expected.
    assert!(
        all_files.iter().any(|f| f.contains("auth")),
        "expected auth.rs in index, got: {all_files:?}"
    );

    // payments.rs is from the extra root — must have the prefix.
    let expected_payments = format!("{}/payments.rs", extra_prefix);
    assert!(
        all_files.iter().any(|f| f.contains(&expected_payments)),
        "expected {expected_payments} in index, got: {all_files:?}"
    );

    // BM25 search should find content from both roots.
    let results_auth = engine
        .search(
            SearchQuery::new("authenticate token")
                .with_limit(5)
                .with_strategy(Strategy::Instant),
        )
        .unwrap_or_default();
    let files_auth: HashSet<_> = results_auth.iter().map(|r| r.file_path.as_str()).collect();
    assert!(
        files_auth.iter().any(|f| f.contains("auth")),
        "search for 'authenticate token' should surface auth.rs: {files_auth:?}"
    );

    let results_pay = engine
        .search(
            SearchQuery::new("charge card payment")
                .with_limit(5)
                .with_strategy(Strategy::Instant),
        )
        .unwrap_or_default();
    let files_pay: HashSet<_> = results_pay.iter().map(|r| r.file_path.as_str()).collect();
    assert!(
        files_pay.iter().any(|f| f.contains("payments")),
        "search for 'charge card payment' should surface payments.rs: {files_pay:?}"
    );
}

/// Verify that `IndexConfig::normalize_path` returns correct prefixed / non-prefixed
/// strings for the primary and extra roots.
#[test]
fn normalize_path_prefixes_extra_roots() {
    use std::path::PathBuf;

    let primary = PathBuf::from("/home/user/myproject");
    let extra = PathBuf::from("/home/user/shared-lib");

    let mut config = IndexConfig::new(&primary);
    config.extra_roots = vec![extra.clone()];

    // File under primary root: no prefix.
    let abs1 = primary.join("src/engine.rs");
    assert_eq!(
        config.normalize_path(&abs1),
        Some("src/engine.rs".to_string())
    );

    // File under extra root: prefixed with "shared-lib".
    let abs2 = extra.join("src/types.rs");
    assert_eq!(
        config.normalize_path(&abs2),
        Some("shared-lib/src/types.rs".to_string())
    );

    // File under neither root: returns None.
    let abs3 = PathBuf::from("/tmp/other.rs");
    assert_eq!(config.normalize_path(&abs3), None);
}

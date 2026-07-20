//! Tests that `concept_index` and `reformulations` are lazy-loaded on first
//! access rather than eagerly during `Engine::open`.
//!
//! These two artifacts used to reach ~2.1 GB and ~528 MB on the Linux kernel;
//! they are now bounded and string-interned, but decoding them still does not
//! belong on commands that never use semantic expansion. The
//! v0.37 refactor moved both behind `OnceLock<Option<T>>` getters; these
//! tests pin that behavior so future regressions trip CI rather than
//! silently re-introducing the cold-start tax.

mod common;

use std::fs;

use codixing_core::{Engine, IndexConfig};
use tempfile::tempdir;

fn no_embed_config(root: &std::path::Path) -> IndexConfig {
    let mut cfg = IndexConfig::new(root);
    cfg.embedding.enabled = false;
    cfg
}

#[test]
fn concept_index_lazy_loaded_on_open() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    // Build the index once so concepts.bin / reformulations.bin exist on disk.
    let engine = Engine::init(root, no_embed_config(root)).unwrap();
    drop(engine);

    // Re-open: the OnceLock for concept_index must NOT be initialized yet.
    let engine = Engine::open(root).unwrap();
    assert!(
        !engine.__test_concept_loaded(),
        "concept_index OnceLock should be unset immediately after Engine::open — \
         got eager load, defeating the v0.37 lazy-load refactor"
    );

    // First access lazy-loads it — and the loaded value must be present, not
    // just a cached None from a failed deserialize. This catches the case
    // where the OnceLock transitions to `Some(None)` silently on parse error.
    assert!(
        engine.__test_force_load_concept(),
        "concept_index did not materialise on lazy load — concepts.bin missing \
         or failed to deserialize. The multi-language test project should \
         always produce a non-empty concept index."
    );
    assert!(
        engine.__test_concept_loaded(),
        "concept_index OnceLock should be set after the first get_concept_index() call"
    );

    // Second access is a cache hit (OnceLock stays set; semantic invariant only).
    let _ = engine.__test_force_load_concept();
    assert!(engine.__test_concept_loaded());
}

#[test]
fn reformulations_lazy_loaded_on_open() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let engine = Engine::init(root, no_embed_config(root)).unwrap();
    drop(engine);

    let engine = Engine::open(root).unwrap();
    assert!(
        !engine.__test_reformulations_loaded(),
        "reformulations OnceLock should be unset immediately after Engine::open — \
         got eager load, defeating the v0.37 lazy-load refactor"
    );

    // Reformulations may legitimately be `None` on tiny test projects (the
    // builder skips empty outputs), so just verify the OnceLock transitioned
    // from unset to set — that's the lazy-load invariant we care about.
    let _ = engine.__test_force_load_reformulations();
    assert!(
        engine.__test_reformulations_loaded(),
        "reformulations OnceLock should be set after the first get_reformulations() call"
    );
}

#[test]
fn init_then_sync_refreshes_added_changed_and_removed_semantics() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let source_dir = root.join("src");
    fs::create_dir_all(&source_dir).unwrap();
    let source = source_dir.join("semantic.rs");
    fs::write(
        &source,
        r#"
/// Authentication credential handler.
pub fn auth_login() {}
/// Authentication credential validator.
pub fn auth_token() {}
"#,
    )
    .unwrap();

    let mut engine = Engine::init(root, no_embed_config(root)).unwrap();
    assert!(
        engine
            .__test_concept_symbols("authentication")
            .iter()
            .any(|symbol| symbol == "auth_login")
    );
    assert!(!engine.__test_reformulation_expansions("auth").is_empty());

    fs::write(
        &source,
        r#"
/// Caching storage reader.
pub fn cache_read() {}
/// Caching storage writer.
pub fn cache_write() {}
"#,
    )
    .unwrap();
    engine.sync().unwrap();
    assert!(engine.__test_concept_symbols("authentication").is_empty());
    assert!(
        engine
            .__test_concept_symbols("caching")
            .iter()
            .any(|symbol| symbol == "cache_read")
    );
    assert!(engine.__test_reformulation_expansions("auth").is_empty());
    assert!(!engine.__test_reformulation_expansions("cache").is_empty());

    fs::remove_file(&source).unwrap();
    engine.sync().unwrap();
    assert!(engine.__test_concept_symbols("caching").is_empty());
    assert!(engine.__test_reformulation_expansions("cache").is_empty());

    drop(engine);
    let reopened = Engine::open(root).unwrap();
    assert!(reopened.__test_concept_symbols("caching").is_empty());
    assert!(reopened.__test_reformulation_expansions("cache").is_empty());
}

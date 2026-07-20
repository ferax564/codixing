use std::fs;

use codixing_core::index::TantivyIndex;
use codixing_core::persistence::IndexStore;
use codixing_core::watcher::{ChangeKind, FileChange};
use codixing_core::{Engine, IndexConfig, SearchQuery, Strategy};
use tempfile::tempdir;

fn no_embed_config(root: &std::path::Path) -> IndexConfig {
    let mut config = IndexConfig::new(root);
    config.embedding.enabled = false;
    config
}

fn instant_has(engine: &Engine, query: &str) -> bool {
    !engine
        .search(
            SearchQuery::new(query)
                .with_strategy(Strategy::Instant)
                .with_limit(20),
        )
        .unwrap()
        .is_empty()
}

fn notebook_with_code(code: &str) -> String {
    serde_json::json!({
        "cells": [{
            "cell_type": "code",
            "execution_count": null,
            "metadata": {},
            "outputs": [],
            "source": [code]
        }],
        "metadata": {},
        "nbformat": 4,
        "nbformat_minor": 5
    })
    .to_string()
}

fn tantivy_documents(root: &std::path::Path) -> Vec<(u64, String)> {
    let store = IndexStore::open(root).expect("open active store");
    let config = store.load_config().expect("load active config");
    TantivyIndex::open_read_only_with_config(&store.tantivy_dir(), config.bm25)
        .expect("open active Tantivy generation")
        .all_chunk_ids_and_content()
        .expect("read active Tantivy documents")
}

#[test]
fn repeated_identical_init_replaces_instead_of_appending() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(
        root.join("lib.rs"),
        "pub fn generational_unique_value() -> usize { 41 }\n",
    )
    .unwrap();

    let first = Engine::init(root, no_embed_config(root)).unwrap();
    let first_stats = first.stats();
    drop(first);
    let first_store = IndexStore::open(root).unwrap();
    let first_generation = first_store.generation().unwrap().to_string();
    drop(first_store);
    let first_documents = tantivy_documents(root);

    let second = Engine::init(root, no_embed_config(root)).unwrap();
    let second_stats = second.stats();
    drop(second);
    let second_store = IndexStore::open(root).unwrap();
    let second_generation = second_store.generation().unwrap().to_string();
    drop(second_store);
    let second_documents = tantivy_documents(root);
    let audit = IndexStore::audit_layout(root);

    assert_ne!(first_generation, second_generation);
    assert_eq!(first_stats.file_count, second_stats.file_count);
    assert_eq!(first_stats.chunk_count, second_stats.chunk_count);
    assert_eq!(first_documents.len(), second_documents.len());
    assert_eq!(audit.layout_kind, "generational");
    assert_eq!(audit.generation_count, 1);
    assert!(audit.abandoned_generations.is_empty());
}

#[cfg(feature = "internal-testing")]
#[test]
fn failed_fresh_init_never_exposes_partially_staged_tantivy_documents() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(
        root.join("lib.rs"),
        "pub fn retained_active_value() -> usize { 41 }\n",
    )
    .unwrap();
    drop(Engine::init(root, no_embed_config(root)).unwrap());
    let before_generation = IndexStore::active_generation(root).unwrap();
    let before_documents = tantivy_documents(root);

    fs::write(
        root.join("good.rs"),
        "pub fn unpublished_good_sibling() -> usize { 42 }\n",
    )
    .unwrap();
    fs::write(
        root.join("bad.rs"),
        "pub fn unpublished_bad_marker() { let _ = \"codixing-test-fail-after-first-tantivy-add\"; }\n",
    )
    .unwrap();

    let error = match Engine::init(root, no_embed_config(root)) {
        Ok(_) => panic!("injected late Tantivy failure must abort fresh init"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("injected failure after first Tantivy add")
    );
    assert_eq!(
        IndexStore::active_generation(root).unwrap(),
        before_generation
    );
    assert_eq!(tantivy_documents(root), before_documents);

    let retained = Engine::open(root).unwrap();
    assert!(instant_has(&retained, "retained_active_value"));
    assert!(!instant_has(&retained, "unpublished_good_sibling"));
    assert!(!instant_has(&retained, "unpublished_bad_marker"));
}

#[cfg(feature = "internal-testing")]
#[test]
fn file_local_parse_failure_skips_only_that_source() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(
        root.join("good.rs"),
        "pub fn healthy_init_sibling() -> usize { 42 }\n",
    )
    .unwrap();
    fs::write(
        root.join("bad.rs"),
        "pub fn skipped_init_source() { let _ = \"codixing-test-skip-before-index-publication\"; }\n",
    )
    .unwrap();

    let engine = Engine::init(root, no_embed_config(root)).unwrap();

    assert!(instant_has(&engine, "healthy_init_sibling"));
    assert!(!instant_has(&engine, "skipped_init_source"));
    assert_eq!(engine.stats().file_count, 1);
}

#[test]
fn fresh_generation_uses_one_shared_trigram_artifact() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(
        root.join("lib.rs"),
        "pub fn shared_trigram_sentinel() -> usize { 1 }\n",
    )
    .unwrap();

    drop(Engine::init(root, no_embed_config(root)).unwrap());
    let store = IndexStore::open(root).unwrap();

    assert!(store.file_trigram_path().is_file());
    assert!(
        !store.chunk_trigram_path().exists(),
        "fresh generations must not duplicate exact-search postings"
    );
    store.validate_for_publication().unwrap();
}

#[test]
fn rebuild_does_not_retain_deleted_source_documents() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let stale = root.join("stale.rs");
    fs::write(
        &stale,
        "pub fn stale_generation_sentinel() -> &'static str { \"stale\" }\n",
    )
    .unwrap();
    fs::write(root.join("kept.rs"), "pub fn kept_value() -> usize { 7 }\n").unwrap();

    drop(Engine::init(root, no_embed_config(root)).unwrap());
    assert!(
        tantivy_documents(root)
            .iter()
            .any(|(_, body)| body.contains("stale_generation_sentinel"))
    );

    fs::remove_file(stale).unwrap();
    drop(Engine::init(root, no_embed_config(root)).unwrap());

    assert!(
        !tantivy_documents(root)
            .iter()
            .any(|(_, body)| body.contains("stale_generation_sentinel"))
    );
    let engine = Engine::open_read_only(root).unwrap();
    let results = engine
        .search(
            SearchQuery::new("stale_generation_sentinel")
                .with_strategy(Strategy::Instant)
                .with_limit(20),
        )
        .unwrap();
    assert!(results.is_empty());
}

#[test]
fn incomplete_generation_cannot_replace_searchable_active_index() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(
        root.join("lib.rs"),
        "pub fn preserved_generation_sentinel() -> usize { 99 }\n",
    )
    .unwrap();

    drop(Engine::init(root, no_embed_config(root)).unwrap());
    let active_before = IndexStore::open(root)
        .unwrap()
        .generation()
        .unwrap()
        .to_string();

    // Beginning a generation creates only isolated staging data. Publishing
    // it fails validation, exactly as an interrupted or out-of-space rebuild
    // would, without touching the active manifest.
    let mut incomplete = IndexStore::begin_generation(root, &no_embed_config(root)).unwrap();
    assert!(incomplete.publish_generation().is_err());

    let active_after = IndexStore::open(root)
        .unwrap()
        .generation()
        .unwrap()
        .to_string();
    assert_eq!(active_before, active_after);
    let engine = Engine::open_read_only(root).unwrap();
    let results = engine
        .search(
            SearchQuery::new("preserved_generation_sentinel")
                .with_strategy(Strategy::Instant)
                .with_limit(20),
        )
        .unwrap();
    assert!(!results.is_empty());

    let audit = IndexStore::audit_layout(root);
    assert_eq!(
        audit.active_generation.as_deref(),
        Some(active_before.as_str())
    );
    assert_eq!(audit.abandoned_generations.len(), 1);
}

#[test]
fn active_reader_keeps_superseded_generation_searchable_until_drop() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let source = root.join("lib.rs");
    fs::write(
        &source,
        "pub fn old_reader_generation_sentinel() -> usize { 1 }\n",
    )
    .unwrap();
    drop(Engine::init(root, no_embed_config(root)).unwrap());

    let old_reader = Engine::open_read_only(root).unwrap();
    fs::write(
        &source,
        "pub fn new_reader_generation_sentinel() -> usize { 2 }\n",
    )
    .unwrap();
    drop(Engine::init(root, no_embed_config(root)).unwrap());

    assert_eq!(IndexStore::audit_layout(root).generation_count, 2);
    let old_results = old_reader
        .search(
            SearchQuery::new("old_reader_generation_sentinel")
                .with_strategy(Strategy::Instant)
                .with_limit(20),
        )
        .unwrap();
    assert!(!old_results.is_empty());

    let new_reader = Engine::open_read_only(root).unwrap();
    let new_results = new_reader
        .search(
            SearchQuery::new("new_reader_generation_sentinel")
                .with_strategy(Strategy::Instant)
                .with_limit(20),
        )
        .unwrap();
    assert!(!new_results.is_empty());
    drop(new_reader);

    drop(old_reader);
    assert_eq!(IndexStore::audit_layout(root).generation_count, 1);
}

#[test]
fn lexical_reader_keeps_lean_profile_across_generation_reload() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let source = root.join("lib.rs");
    fs::write(
        &source,
        "pub fn lexical_generation_old_marker() -> usize { 1 }\n",
    )
    .unwrap();
    drop(Engine::init(root, no_embed_config(root)).unwrap());

    let mut lexical = Engine::open_read_only_lexical(root).unwrap();
    assert!(lexical.graph_stats().is_none());

    fs::write(
        &source,
        "pub fn lexical_generation_new_marker() -> usize { 2 }\n",
    )
    .unwrap();
    drop(Engine::init(root, no_embed_config(root)).unwrap());

    lexical.set_reload_interval(std::time::Duration::ZERO);
    assert!(lexical.reload_if_stale().unwrap());
    assert!(
        lexical.graph_stats().is_none(),
        "generation replacement must preserve the lexical load profile"
    );

    let full = Engine::open_read_only(root).unwrap();
    assert!(full.graph_stats().is_some());
    for strategy in [Strategy::Exact, Strategy::Instant] {
        let query = || {
            SearchQuery::new("lexical_generation_new_marker")
                .with_strategy(strategy)
                .with_limit(20)
        };
        let full_results = full.search(query()).unwrap();
        let lexical_results = lexical.search(query()).unwrap();
        assert!(!lexical_results.is_empty());
        assert_eq!(
            serde_json::to_vec(&lexical_results).unwrap(),
            serde_json::to_vec(&full_results).unwrap()
        );
    }
}

#[test]
fn long_lived_read_only_engine_reopens_new_active_generation() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let source = root.join("lib.rs");
    fs::write(&source, "pub fn cobaltarchipelagoquasar() -> usize { 1 }\n").unwrap();
    drop(Engine::init(root, no_embed_config(root)).unwrap());

    let mut refreshing_reader = Engine::open_read_only(root).unwrap();
    let retained_old_reader = Engine::open_read_only(root).unwrap();
    let generation_a = IndexStore::audit_layout(root)
        .active_generation
        .expect("generation A");

    fs::write(&source, "pub fn vermilionthunderbadger() -> usize { 2 }\n").unwrap();
    drop(Engine::init(root, no_embed_config(root)).unwrap());
    let generation_b = IndexStore::audit_layout(root)
        .active_generation
        .expect("generation B");
    assert_ne!(generation_a, generation_b);
    assert_eq!(IndexStore::audit_layout(root).generation_count, 2);

    refreshing_reader.set_reload_interval(std::time::Duration::ZERO);
    assert!(refreshing_reader.reload_if_stale().unwrap());

    let beta_results = refreshing_reader
        .search(
            SearchQuery::new("vermilionthunderbadger")
                .with_strategy(Strategy::Instant)
                .with_limit(20),
        )
        .unwrap();
    assert!(!beta_results.is_empty());
    let alpha_results = refreshing_reader
        .search(
            SearchQuery::new("cobaltarchipelagoquasar")
                .with_strategy(Strategy::Instant)
                .with_limit(20),
        )
        .unwrap();
    assert!(alpha_results.is_empty());

    let retained_results = retained_old_reader
        .search(
            SearchQuery::new("cobaltarchipelagoquasar")
                .with_strategy(Strategy::Instant)
                .with_limit(20),
        )
        .unwrap();
    assert!(!retained_results.is_empty());

    drop(retained_old_reader);
    assert_eq!(IndexStore::audit_layout(root).generation_count, 1);
    assert_eq!(
        IndexStore::active_generation(root).unwrap().as_deref(),
        Some(generation_b.as_str())
    );
}

#[test]
fn incremental_symbol_delta_preserves_generation_isolation_and_reloads() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let changed = root.join("changed.rs");
    fs::write(
        &changed,
        "/// Old symbol docs.\npub fn overlay_old() {}\npub fn overlay_removed() {}\n",
    )
    .unwrap();
    fs::write(root.join("untouched.rs"), "pub fn overlay_untouched() {}\n").unwrap();
    drop(Engine::init(root, no_embed_config(root)).unwrap());

    let old_store = IndexStore::open_read_only(root).unwrap();
    #[cfg(unix)]
    let old_base_identity = {
        use std::os::unix::fs::MetadataExt;

        let metadata = fs::metadata(old_store.symbols_v2_path()).unwrap();
        (metadata.dev(), metadata.ino())
    };
    let retained_old_reader = Engine::open_read_only(root).unwrap();
    let mut refreshing_reader = Engine::open_read_only(root).unwrap();
    let mut writer = Engine::open(root).unwrap();

    fs::write(
        &changed,
        "/// New full-fidelity docs.\npub fn overlay_new() -> usize { 7 }\n",
    )
    .unwrap();
    writer
        .apply_changes(&[FileChange {
            path: changed.canonicalize().unwrap(),
            kind: ChangeKind::Modified,
        }])
        .unwrap();

    let has_symbol = |engine: &Engine, name: &str| {
        engine
            .symbols(name, None)
            .unwrap()
            .iter()
            .any(|symbol| symbol.name == name)
    };
    assert!(has_symbol(&writer, "overlay_new"));
    assert!(!has_symbol(&writer, "overlay_old"));
    assert!(!has_symbol(&writer, "overlay_removed"));
    assert!(has_symbol(&writer, "overlay_untouched"));

    let active_store = IndexStore::open_read_only(root).unwrap();
    assert!(active_store.symbols_delta_path().is_file());
    assert!(
        fs::metadata(active_store.symbols_delta_path())
            .unwrap()
            .len()
            <= 8 * 1024 * 1024
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let metadata = fs::metadata(active_store.symbols_v2_path()).unwrap();
        assert_eq!(
            (metadata.dev(), metadata.ino()),
            old_base_identity,
            "a small checkpoint must retain the hard-linked mmap symbol base"
        );
    }

    let fresh_reader = Engine::open_read_only(root).unwrap();
    assert!(has_symbol(&fresh_reader, "overlay_new"));
    assert!(!has_symbol(&fresh_reader, "overlay_old"));
    assert!(!has_symbol(&fresh_reader, "overlay_removed"));
    assert!(has_symbol(&fresh_reader, "overlay_untouched"));
    let replacement = fresh_reader.symbols("overlay_new", None).unwrap();
    let replacement = replacement
        .iter()
        .find(|symbol| symbol.name == "overlay_new")
        .unwrap();
    assert!(
        replacement
            .doc_comment
            .as_deref()
            .is_some_and(|comment| comment.contains("New full-fidelity docs"))
    );

    refreshing_reader.set_reload_interval(std::time::Duration::ZERO);
    assert!(refreshing_reader.reload_if_stale().unwrap());
    assert!(has_symbol(&refreshing_reader, "overlay_new"));
    assert!(!has_symbol(&refreshing_reader, "overlay_old"));

    assert!(has_symbol(&retained_old_reader, "overlay_old"));
    assert!(has_symbol(&retained_old_reader, "overlay_removed"));
    assert!(!has_symbol(&retained_old_reader, "overlay_new"));
    assert_eq!(IndexStore::audit_layout(root).generation_count, 2);

    drop(active_store);
    drop(fresh_reader);
    drop(retained_old_reader);
    drop(old_store);
    assert_eq!(IndexStore::audit_layout(root).generation_count, 1);
}

#[test]
fn fresh_generation_uses_full_fidelity_mmap_without_bitcode_duplicate() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(
        root.join("api.rs"),
        "/// Full-fidelity mmap documentation.\npub fn public_mmap_api() {}\n",
    )
    .unwrap();
    drop(Engine::init(root, no_embed_config(root)).unwrap());

    let store = IndexStore::open_read_only(root).unwrap();
    assert!(store.symbols_v2_path().is_file());
    assert!(!store.symbols_path().exists());
    drop(store);

    let engine = Engine::open_read_only(root).unwrap();
    let symbols = engine.symbols("public_mmap_api", None).unwrap();
    let symbol = symbols
        .iter()
        .find(|symbol| symbol.name == "public_mmap_api")
        .unwrap();
    assert_eq!(
        symbol.visibility,
        codixing_core::language::Visibility::Public
    );
    assert!(
        symbol
            .doc_comment
            .as_deref()
            .is_some_and(|comment| comment.contains("Full-fidelity mmap documentation"))
    );
}

#[test]
fn explicit_read_only_open_does_not_create_session_artifacts() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("lib.rs"), "pub fn read_only_sessions() {}\n").unwrap();
    drop(Engine::init(root, no_embed_config(root)).unwrap());

    let control_dir = root.join(".codixing");
    let session_path = control_dir.join("session.json");
    let shared_session_path = control_dir.join("shared_session.jsonl");
    for path in [&session_path, &shared_session_path] {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("failed to remove test artifact: {error}"),
        }
    }

    drop(Engine::open_read_only(root).unwrap());
    assert!(!session_path.exists());
    assert!(!shared_session_path.exists());
}

#[test]
fn malformed_generation_manifest_does_not_destroy_open_reader() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(
        root.join("lib.rs"),
        "pub fn malformed_manifest_reader_sentinel() -> usize { 3 }\n",
    )
    .unwrap();
    drop(Engine::init(root, no_embed_config(root)).unwrap());

    let mut reader = Engine::open_read_only(root).unwrap();
    reader.set_reload_interval(std::time::Duration::ZERO);
    let manifest_path = root.join(".codixing").join("active-generation.json");
    let original_manifest = fs::read(&manifest_path).unwrap();
    fs::write(&manifest_path, b"{malformed").unwrap();

    assert!(reader.reload_if_stale().is_err());
    let results = reader
        .search(
            SearchQuery::new("malformed_manifest_reader_sentinel")
                .with_strategy(Strategy::Instant)
                .with_limit(20),
        )
        .unwrap();
    assert!(!results.is_empty());

    fs::write(manifest_path, original_manifest).unwrap();
}

#[test]
fn legacy_flat_index_opens_and_migrates_on_rebuild() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(
        root.join("lib.rs"),
        "pub fn legacy_migration_sentinel() -> usize { 5 }\n",
    )
    .unwrap();
    drop(Engine::init(root, no_embed_config(root)).unwrap());

    // Convert the fixture to the pre-generation flat layout without changing
    // any index bytes, reproducing an index created by an older Codixing.
    let store = IndexStore::open(root).unwrap();
    let generation_dir = store.codixing_dir();
    let control_dir = store.control_dir();
    drop(store);
    for entry in fs::read_dir(&generation_dir).unwrap() {
        let entry = entry.unwrap();
        fs::rename(entry.path(), control_dir.join(entry.file_name())).unwrap();
    }
    fs::remove_file(control_dir.join("active-generation.json")).unwrap();
    fs::remove_dir_all(control_dir.join("generations")).unwrap();

    let legacy = IndexStore::open(root).unwrap();
    assert_eq!(legacy.generation(), None);
    assert_eq!(legacy.codixing_dir(), control_dir);
    drop(Engine::open_read_only(root).unwrap());
    drop(legacy);

    drop(Engine::init(root, no_embed_config(root)).unwrap());
    let migrated = IndexStore::open(root).unwrap();
    assert!(migrated.generation().is_some());
    assert_eq!(IndexStore::audit_layout(root).layout_kind, "generational");
    assert!(!control_dir.join("tantivy").exists());
}

#[test]
fn next_builder_reclaims_abandoned_generation_without_touching_active() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("lib.rs"), "pub fn cleanup_sentinel() {}\n").unwrap();
    drop(Engine::init(root, no_embed_config(root)).unwrap());

    let store = IndexStore::open(root).unwrap();
    let active = store.generation().unwrap().to_string();
    let generations = store.control_dir().join("generations");
    drop(store);

    let abandoned = generations.join("gen-abandoned-test");
    fs::create_dir(&abandoned).unwrap();
    fs::write(abandoned.join("partial"), b"interrupted").unwrap();

    let unpublished = IndexStore::begin_generation(root, &no_embed_config(root)).unwrap();
    assert!(!abandoned.exists());
    assert!(generations.join(active).is_dir());
    drop(unpublished);
}

#[cfg(unix)]
#[test]
fn abandoned_cleanup_never_follows_generation_symlinks() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().unwrap();
    let root = dir.path();
    let outside = tempdir().unwrap();
    fs::write(outside.path().join("must-survive"), b"safe").unwrap();

    let control = root.join(".codixing");
    let generations = control.join("generations");
    fs::create_dir_all(&generations).unwrap();
    let hostile = generations.join("gen-hostile-symlink");
    symlink(outside.path(), &hostile).unwrap();

    let unpublished = IndexStore::begin_generation(root, &no_embed_config(root)).unwrap();
    assert!(outside.path().join("must-survive").is_file());
    assert!(hostile.is_symlink());
    drop(unpublished);
}

#[cfg(unix)]
#[test]
fn rebuild_rejects_symlinked_generations_directory() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().unwrap();
    let root = dir.path();
    let outside = tempdir().unwrap();
    let control = root.join(".codixing");
    fs::create_dir(&control).unwrap();
    symlink(outside.path(), control.join("generations")).unwrap();

    let error = IndexStore::begin_generation(root, &no_embed_config(root)).unwrap_err();
    assert!(error.to_string().contains("real directory"));
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[test]
fn no_op_sync_keeps_the_active_generation() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("lib.rs"), "pub fn no_op_generation() {}\n").unwrap();
    let mut engine = Engine::init(root, no_embed_config(root)).unwrap();
    let before = IndexStore::active_generation(root).unwrap();

    let stats = engine.sync().unwrap();

    assert_eq!(stats.added + stats.modified + stats.removed, 0);
    assert_eq!(IndexStore::active_generation(root).unwrap(), before);
}

#[test]
fn direct_change_publishes_once_and_retains_the_old_reader_snapshot() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let source = root.join("lib.rs");
    fs::write(&source, "pub fn checkpoint_old_value() -> usize { 1 }\n").unwrap();
    let mut writer = Engine::init(root, no_embed_config(root)).unwrap();
    let old_reader = Engine::open_read_only(root).unwrap();
    let before = IndexStore::active_generation(root).unwrap();

    fs::write(&source, "pub fn checkpoint_new_value() -> usize { 2 }\n").unwrap();
    writer
        .apply_changes(&[FileChange {
            path: source,
            kind: ChangeKind::Modified,
        }])
        .unwrap();

    let after = IndexStore::active_generation(root).unwrap();
    assert_ne!(after, before);
    assert!(instant_has(&writer, "checkpoint_new_value"));
    assert!(instant_has(&old_reader, "checkpoint_old_value"));
    assert!(!instant_has(&old_reader, "checkpoint_new_value"));
    let new_reader = Engine::open_read_only(root).unwrap();
    assert!(instant_has(&new_reader, "checkpoint_new_value"));
}

#[test]
fn deferred_changes_are_invisible_until_checkpoint_publication() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let source = root.join("lib.rs");
    fs::write(&source, "pub fn deferred_old_value() -> usize { 1 }\n").unwrap();
    let mut writer = Engine::init(root, no_embed_config(root)).unwrap();
    let retained_reader = Engine::open_read_only(root).unwrap();
    let before = IndexStore::active_generation(root).unwrap();

    fs::write(&source, "pub fn deferred_new_value() -> usize { 2 }\n").unwrap();
    writer
        .apply_changes_deferred(&[FileChange {
            path: source,
            kind: ChangeKind::Modified,
        }])
        .unwrap();

    assert_eq!(IndexStore::active_generation(root).unwrap(), before);
    assert!(instant_has(&writer, "deferred_new_value"));
    assert!(instant_has(&retained_reader, "deferred_old_value"));
    let pre_checkpoint_reader = Engine::open_read_only(root).unwrap();
    assert!(instant_has(&pre_checkpoint_reader, "deferred_old_value"));
    assert!(!instant_has(&pre_checkpoint_reader, "deferred_new_value"));

    writer.checkpoint_pending_changes().unwrap();
    assert_ne!(IndexStore::active_generation(root).unwrap(), before);
    let published_reader = Engine::open_read_only(root).unwrap();
    assert!(instant_has(&published_reader, "deferred_new_value"));
    assert!(instant_has(&retained_reader, "deferred_old_value"));
}

#[test]
fn stable_writer_lease_allows_only_one_mutating_engine() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let source = root.join("lib.rs");
    fs::write(&source, "pub fn single_writer_old_value() {}\n").unwrap();
    drop(Engine::init(root, no_embed_config(root)).unwrap());

    let mut writer = Engine::open(root).unwrap();
    assert!(!writer.is_read_only());
    fs::write(&source, "pub fn single_writer_new_value() {}\n").unwrap();
    writer
        .apply_changes_deferred(&[FileChange {
            path: source,
            kind: ChangeKind::Modified,
        }])
        .unwrap();

    // The writer now owns an unpublished working generation and no longer
    // holds Tantivy's lock on the active generation. The stable repository
    // lease must still prevent a second mutating engine from opening A.
    let contender = Engine::open(root).unwrap();
    assert!(contender.is_read_only());
    drop(contender);
    drop(writer);

    let next_writer = Engine::open(root).unwrap();
    assert!(!next_writer.is_read_only());
}

#[test]
fn pending_journal_replays_after_an_unpublished_writer_is_dropped() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let source = root.join("lib.rs");
    fs::write(&source, "pub fn recovery_old_value() -> usize { 1 }\n").unwrap();
    drop(Engine::init(root, no_embed_config(root)).unwrap());
    let before = IndexStore::active_generation(root).unwrap();

    let mut interrupted = Engine::open(root).unwrap();
    fs::write(&source, "pub fn recovery_new_value() -> usize { 2 }\n").unwrap();
    interrupted
        .apply_changes_deferred(&[FileChange {
            path: source,
            kind: ChangeKind::Modified,
        }])
        .unwrap();
    let journal = IndexStore::open(root).unwrap().dirty_paths_path();
    assert!(journal.is_file());
    assert_eq!(IndexStore::active_generation(root).unwrap(), before);
    drop(interrupted);

    let recovered = Engine::open(root).unwrap();
    assert!(!recovered.is_read_only());
    assert!(instant_has(&recovered, "recovery_new_value"));
    assert_ne!(IndexStore::active_generation(root).unwrap(), before);
    assert!(!journal.exists());
}

#[test]
fn failed_checkpoint_aborts_whole_batch_and_retries_dirty_paths() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let source = root.join("lib.rs");
    let notebook = root.join("analysis.ipynb");
    fs::write(&source, "pub fn partial_old_value() -> usize { 1 }\n").unwrap();
    fs::write(&notebook, notebook_with_code("value = 1\n")).unwrap();
    let mut writer = Engine::init(root, no_embed_config(root)).unwrap();
    let before = IndexStore::active_generation(root).unwrap();

    fs::write(&source, "pub fn partial_new_value() -> usize { 2 }\n").unwrap();
    fs::write(&notebook, notebook_with_code("value = 2\n")).unwrap();
    let source = source.canonicalize().unwrap();
    let notebook = notebook.canonicalize().unwrap();
    let error = writer
        .apply_changes(&[
            FileChange {
                path: source.clone(),
                kind: ChangeKind::Modified,
            },
            FileChange {
                path: notebook.clone(),
                kind: ChangeKind::Modified,
            },
        ])
        .unwrap_err();
    assert!(error.to_string().contains("analysis.ipynb"));

    assert_eq!(IndexStore::active_generation(root).unwrap(), before);
    assert!(instant_has(&writer, "partial_old_value"));
    assert!(!instant_has(&writer, "partial_new_value"));
    let retained = Engine::open_read_only(root).unwrap();
    assert!(instant_has(&retained, "partial_old_value"));
    assert!(!instant_has(&retained, "partial_new_value"));

    let dirty = IndexStore::open_read_only(root)
        .unwrap()
        .load_dirty_paths()
        .unwrap();
    assert_eq!(dirty.len(), 2);
    assert!(dirty.contains(&source));
    assert!(dirty.contains(&notebook));

    // Removing the unsupported notebook lets the journal replay the entire
    // batch, including the source file that had succeeded before the abort.
    fs::remove_file(&notebook).unwrap();
    writer.apply_changes(&[]).unwrap();
    assert_ne!(IndexStore::active_generation(root).unwrap(), before);
    assert!(instant_has(&writer, "partial_new_value"));
    assert!(!instant_has(&writer, "partial_old_value"));
    assert!(
        IndexStore::open_read_only(root)
            .unwrap()
            .load_dirty_paths()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn latest_deferred_failure_revokes_an_earlier_success_for_the_same_path() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let source = root.join("lib.rs");
    fs::write(&source, "pub fn latest_old_value() -> usize { 1 }\n").unwrap();
    let mut writer = Engine::init(root, no_embed_config(root)).unwrap();
    let before = IndexStore::active_generation(root).unwrap();

    fs::write(
        &source,
        "pub fn latest_intermediate_value() -> usize { 2 }\n",
    )
    .unwrap();
    writer
        .apply_changes_deferred(&[FileChange {
            path: source.clone(),
            kind: ChangeKind::Modified,
        }])
        .unwrap();

    fs::remove_file(&source).unwrap();
    fs::create_dir(&source).unwrap();
    assert!(
        writer
            .apply_changes_deferred(&[FileChange {
                path: source.clone(),
                kind: ChangeKind::Modified,
            }])
            .is_err()
    );
    assert!(
        writer.checkpoint_pending_changes().is_ok(),
        "the failed deferred batch already aborted and cleared pending state"
    );
    assert_eq!(IndexStore::active_generation(root).unwrap(), before);
    assert_eq!(
        IndexStore::open_read_only(root)
            .unwrap()
            .load_dirty_paths()
            .unwrap(),
        vec![source.canonicalize().unwrap()]
    );

    fs::remove_dir(&source).unwrap();
    fs::write(&source, "pub fn latest_recovered_value() -> usize { 3 }\n").unwrap();
    writer.apply_changes(&[]).unwrap();
    let published = Engine::open_read_only(root).unwrap();
    assert!(instant_has(&published, "latest_recovered_value"));
}

#[test]
fn graph_rebuild_flushes_deferred_changes_before_its_own_publication() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let source = root.join("lib.rs");
    fs::write(&source, "pub fn graph_flush_old_value() -> usize { 1 }\n").unwrap();
    let mut writer = Engine::init(root, no_embed_config(root)).unwrap();
    let before = IndexStore::active_generation(root).unwrap();

    fs::write(&source, "pub fn graph_flush_new_value() -> usize { 2 }\n").unwrap();
    writer
        .apply_changes_deferred(&[FileChange {
            path: source,
            kind: ChangeKind::Modified,
        }])
        .unwrap();
    assert_eq!(IndexStore::active_generation(root).unwrap(), before);

    writer.rebuild_graph_from_disk().unwrap();
    assert_ne!(IndexStore::active_generation(root).unwrap(), before);
    let published = Engine::open_read_only(root).unwrap();
    assert!(instant_has(&published, "graph_flush_new_value"));
}

#[test]
fn graph_rebuild_reports_an_all_failed_deferred_batch() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let notebook = root.join("analysis.ipynb");
    fs::write(&notebook, notebook_with_code("value = 1\n")).unwrap();
    let mut writer = Engine::init(root, no_embed_config(root)).unwrap();
    let before = IndexStore::active_generation(root).unwrap();

    fs::write(&notebook, notebook_with_code("value = 2\n")).unwrap();
    assert!(
        writer
            .apply_changes_deferred(&[FileChange {
                path: notebook,
                kind: ChangeKind::Modified,
            }])
            .is_err()
    );
    assert!(writer.rebuild_graph_from_disk().is_err());
    assert_eq!(IndexStore::active_generation(root).unwrap(), before);
}

#[test]
fn first_checkpoint_migrates_a_legacy_v1_only_hash_index() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let source = root.join("lib.rs");
    fs::write(&source, "pub fn legacy_hash_value() -> usize { 1 }\n").unwrap();
    drop(Engine::init(root, no_embed_config(root)).unwrap());

    let store = IndexStore::open(root).unwrap();
    let legacy_hashes: Vec<_> = store
        .load_tree_hashes_v2()
        .unwrap()
        .into_iter()
        .map(|(path, entry)| (path, entry.content_hash))
        .collect();
    store.save_tree_hashes(&legacy_hashes).unwrap();
    fs::remove_file(store.tree_hashes_v2_path()).unwrap();
    drop(store);

    let mut writer = Engine::open(root).unwrap();
    fs::write(&source, "pub fn migrated_hash_value() -> usize { 2 }\n").unwrap();
    writer
        .apply_changes(&[FileChange {
            path: source,
            kind: ChangeKind::Modified,
        }])
        .unwrap();

    let active = IndexStore::open(root).unwrap();
    assert!(active.tree_hashes_v2_path().is_file());
    drop(active);
    let published = Engine::open_read_only(root).unwrap();
    assert!(instant_has(&published, "migrated_hash_value"));
}

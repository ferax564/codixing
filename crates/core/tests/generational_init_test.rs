use std::fs;

use codixing_core::index::TantivyIndex;
use codixing_core::persistence::IndexStore;
use codixing_core::{Engine, IndexConfig, SearchQuery, Strategy};
use tempfile::tempdir;

fn no_embed_config(root: &std::path::Path) -> IndexConfig {
    let mut config = IndexConfig::new(root);
    config.embedding.enabled = false;
    config
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

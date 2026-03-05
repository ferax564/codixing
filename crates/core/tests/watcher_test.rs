//! Integration tests for the file watcher lifecycle.

mod common;

use std::fs;
use std::time::Duration;

use codixing_core::watcher::{ChangeKind, FileWatcher};
use codixing_core::{Engine, IndexConfig, SearchQuery, Strategy};
use tempfile::tempdir;

fn no_embed_config(root: &std::path::Path) -> IndexConfig {
    let mut cfg = IndexConfig::new(root);
    cfg.embedding.enabled = false;
    cfg
}

#[test]
fn watcher_create_and_poll() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("existing.rs"), "fn existing() {}").unwrap();

    let config = no_embed_config(root);
    let watcher = FileWatcher::new(root, &config).unwrap();

    fs::write(src.join("new_file.rs"), "fn new_func() {}").unwrap();

    let changes = watcher.poll_changes(Duration::from_secs(2));
    assert!(
        !changes.is_empty(),
        "expected at least one change event after creating a file"
    );

    let new_file_change = changes
        .iter()
        .find(|c| c.path.to_string_lossy().contains("new_file.rs"));
    assert!(
        new_file_change.is_some(),
        "expected to find new_file.rs in changes, got: {:?}",
        changes.iter().map(|c| &c.path).collect::<Vec<_>>()
    );
    assert_eq!(
        new_file_change.unwrap().kind,
        ChangeKind::Modified,
        "new file creation should be reported as Modified"
    );
}

#[test]
fn watcher_integrates_with_engine() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let mut engine = Engine::init(root, no_embed_config(root)).unwrap();

    // Verify the sentinel does not exist yet.
    let results = engine
        .search(
            SearchQuery::new("watcher_sentinel_function")
                .with_limit(5)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();
    assert!(
        results.is_empty(),
        "sentinel should not exist before file creation"
    );

    let watcher = engine.watch().unwrap();

    fs::write(
        root.join("src/watcher_new.rs"),
        r#"/// A sentinel function for the watcher integration test.
pub fn watcher_sentinel_function() -> bool {
    true
}
"#,
    )
    .unwrap();

    let changes = watcher.poll_changes(Duration::from_secs(2));
    assert!(
        !changes.is_empty(),
        "expected at least one change from the watcher"
    );

    engine.apply_changes(&changes).unwrap();

    let results = engine
        .search(
            SearchQuery::new("watcher_sentinel_function")
                .with_limit(5)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();
    assert!(
        !results.is_empty(),
        "expected watcher_sentinel_function to be searchable after apply_changes"
    );
    assert!(
        results
            .iter()
            .any(|r| r.file_path.contains("watcher_new.rs")),
        "expected result from watcher_new.rs"
    );
}

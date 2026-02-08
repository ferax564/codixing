//! Integration tests for the file watcher lifecycle.

mod common;

use std::fs;
use std::time::Duration;

use codeforge_core::watcher::{ChangeKind, FileWatcher};
use codeforge_core::{Engine, IndexConfig, SearchQuery};
use tempfile::tempdir;

#[test]
fn watcher_create_and_poll() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    // Create an initial Rust file so the watcher has something to watch.
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("existing.rs"), "fn existing() {}").unwrap();

    let config = IndexConfig::new(root);
    let watcher = FileWatcher::new(root, &config).unwrap();

    // Create a new file while the watcher is active.
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

    let config = IndexConfig::new(root);
    let mut engine = Engine::init(root, config).unwrap();

    // Verify the sentinel does not exist yet.
    let results = engine
        .search(SearchQuery::new("watcher_sentinel_function").with_limit(5))
        .unwrap();
    assert!(
        results.is_empty(),
        "sentinel should not exist before file creation"
    );

    // Start watching.
    let watcher = engine.watch().unwrap();

    // Create a new file with a unique function.
    fs::write(
        root.join("src/watcher_new.rs"),
        r#"/// A sentinel function for the watcher integration test.
pub fn watcher_sentinel_function() -> bool {
    true
}
"#,
    )
    .unwrap();

    // Poll for changes.
    let changes = watcher.poll_changes(Duration::from_secs(2));
    assert!(
        !changes.is_empty(),
        "expected at least one change from the watcher"
    );

    // Apply changes to the engine.
    engine.apply_changes(&changes).unwrap();

    // The new function should now be searchable.
    let results = engine
        .search(SearchQuery::new("watcher_sentinel_function").with_limit(5))
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

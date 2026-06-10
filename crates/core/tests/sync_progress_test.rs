//! Tests for [`Engine::sync_with_progress`].

use std::sync::{Arc, Mutex};

use codixing_core::{Engine, IndexConfig};
use tempfile::tempdir;

#[test]
fn sync_with_progress_reports_messages() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    // Create a simple Rust file so the engine has something to index.
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("main.rs"), "fn main() { println!(\"hello\"); }\n").unwrap();

    let config = IndexConfig::new(root);
    let mut engine = Engine::init(root, config).unwrap();

    // Collect progress messages.
    let messages: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let msgs = Arc::clone(&messages);

    let stats = engine
        .sync_with_progress(move |msg| {
            msgs.lock().unwrap().push(msg.to_string());
        })
        .unwrap();

    let msgs = messages.lock().unwrap();

    // Should have received at least the key progress stages.
    assert!(
        msgs.iter().any(|m| m.contains("scanning")),
        "expected 'scanning' message, got: {msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| m.contains("found")),
        "expected 'found' message, got: {msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| m.contains("sync complete")),
        "expected 'sync complete' message, got: {msgs:?}"
    );

    // Verify the sync ran and produced valid stats.
    assert!(
        stats.added + stats.modified + stats.unchanged > 0 || stats.removed > 0,
        "expected sync to process files: {stats:?}"
    );
}

#[test]
fn first_sync_after_init_is_a_no_op_for_doc_and_config_files() {
    // init's change baseline must cover doc/config files (Markdown, TOML,
    // LICENSE, …), not just AST-parsed code — otherwise the first sync after
    // every init re-classifies all of them as "added" and re-indexes them.
    let dir = tempdir().unwrap();
    let root = dir.path();

    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(root.join("README.md"), "# Title\n\nSome docs.\n").unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(root.join("LICENSE"), "MIT License\n").unwrap();

    let config = IndexConfig::new(root);
    let mut engine = Engine::init(root, config).unwrap();

    let stats = engine.sync().unwrap();
    assert_eq!(
        (stats.added, stats.modified, stats.removed),
        (0, 0, 0),
        "first sync after init must be a no-op, got: {stats:?}"
    );
    assert!(
        stats.unchanged >= 4,
        "all indexed files should report unchanged: {stats:?}"
    );
}

#[cfg(unix)]
#[test]
fn sync_is_a_no_op_when_init_root_was_a_symlinked_path() {
    // Reproduces the macOS CI failure: tempdirs live under /var -> /private/var,
    // so the init root is non-canonical. init walked the canonical root but kept
    // the caller's config.root, making every subsequent sync see all files as
    // added+removed. Engine::init must canonicalize config.root.
    let dir = tempdir().unwrap();
    let real_root = dir.path().join("real");
    std::fs::create_dir_all(real_root.join("src")).unwrap();
    std::fs::write(real_root.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(real_root.join("README.md"), "# Docs\n").unwrap();

    let link_root = dir.path().join("link");
    std::os::unix::fs::symlink(&real_root, &link_root).unwrap();

    let config = IndexConfig::new(&link_root);
    let mut engine = Engine::init(&link_root, config).unwrap();

    let stats = engine.sync().unwrap();
    assert_eq!(
        (stats.added, stats.modified, stats.removed),
        (0, 0, 0),
        "sync after init via a symlinked root must be a no-op, got: {stats:?}"
    );
}

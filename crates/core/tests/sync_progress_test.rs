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
    std::fs::write(
        src.join("main.rs"),
        "fn main() { println!(\"hello\"); }\n",
    )
    .unwrap();

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

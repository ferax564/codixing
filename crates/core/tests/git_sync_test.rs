//! Integration tests for [`Engine::git_sync`].
//!
//! These tests spin up a real temporary git repository, build an index,
//! make a new commit, and verify that `git_sync` picks up the change
//! without a full re-index.
//!
//! **Note on Tantivy locking**: Tantivy acquires a file-level lock on the
//! index writer.  Tests that init + re-open the same directory must run
//! sequentially; we enforce this with the `serial_test` crate — tests that
//! share a Tantivy writer are annotated with `#[serial]` to prevent
//! concurrent lock contention.

use codixing_core::{Engine, IndexConfig, SearchQuery, Strategy};
use serial_test::serial;
use tempfile::tempdir;

/// Helper: run a git command in `cwd`; panics on failure.
fn git(cwd: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"))
        .status;
    assert!(status.success(), "git {args:?} failed in {}", cwd.display());
}

/// Returns `false` if git is not in `PATH` (CI / minimal environments).
fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// BM25-only config (no model download in tests).
fn bm25_config(root: &std::path::Path) -> IndexConfig {
    let mut cfg = IndexConfig::new(root);
    cfg.embedding.enabled = false;
    cfg
}

#[test]
fn git_sync_reindexes_changed_files() {
    if !git_available() {
        eprintln!("git_sync_reindexes_changed_files: skipped (git not in PATH)");
        return;
    }

    let dir = tempdir().unwrap();
    let root = dir.path();

    // Initialise a git repo with an initial commit.
    git(root, &["init"]);
    git(root, &["config", "user.email", "test@test.com"]);
    git(root, &["config", "user.name", "Test User"]);

    std::fs::write(root.join("lib.rs"), "pub fn original() {}").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "initial"]);

    // Build index; save() records HEAD in meta.
    // Drop the engine immediately so the Tantivy writer lock is released.
    drop(Engine::init(root, bm25_config(root)).unwrap());

    // Modify the file and create a second commit.
    std::fs::write(
        root.join("lib.rs"),
        "pub fn updated_sentinel_xyz() { /* modified */ }",
    )
    .unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "update"]);

    // Re-open and call git_sync — it should detect and apply the change.
    let mut engine = Engine::open(root).unwrap();
    let stats = engine.git_sync().unwrap();

    assert!(!stats.unchanged, "git_sync should detect the new commit");
    assert_eq!(stats.modified, 1, "exactly one file was modified");
    assert_eq!(stats.removed, 0, "no files were removed");

    // The updated function should now be searchable.
    let q = SearchQuery::new("updated_sentinel_xyz")
        .with_limit(5)
        .with_strategy(Strategy::Instant);
    let results = engine.search(q).unwrap();
    assert!(
        !results.is_empty(),
        "updated_sentinel_xyz should be findable after git_sync"
    );
}

#[test]
fn git_sync_handles_deleted_files() {
    if !git_available() {
        eprintln!("git_sync_handles_deleted_files: skipped (git not in PATH)");
        return;
    }

    let dir = tempdir().unwrap();
    let root = dir.path();

    git(root, &["init"]);
    git(root, &["config", "user.email", "test@test.com"]);
    git(root, &["config", "user.name", "Test User"]);

    std::fs::write(root.join("alpha.rs"), "pub fn alpha() {}").unwrap();
    std::fs::write(root.join("beta.rs"), "pub fn beta() {}").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "initial"]);

    // Build index, drop engine to release Tantivy lock.
    drop(Engine::init(root, bm25_config(root)).unwrap());

    // Delete one file and commit.
    std::fs::remove_file(root.join("beta.rs")).unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "remove beta"]);

    let mut engine = Engine::open(root).unwrap();
    let stats = engine.git_sync().unwrap();

    assert!(!stats.unchanged, "git_sync should detect the deletion");
    assert_eq!(stats.removed, 1, "one file was deleted");
}

#[test]
fn git_sync_handles_renamed_files() {
    if !git_available() {
        eprintln!("git_sync_handles_renamed_files: skipped (git not in PATH)");
        return;
    }

    let dir = tempdir().unwrap();
    let root = dir.path();

    git(root, &["init"]);
    git(root, &["config", "user.email", "test@test.com"]);
    git(root, &["config", "user.name", "Test User"]);

    std::fs::write(root.join("old.rs"), "pub fn old_sentinel_xyz() {}").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "initial"]);

    {
        let engine = Engine::init(root, bm25_config(root)).unwrap();
        engine.save().unwrap();
    }

    git(root, &["mv", "old.rs", "new.rs"]);
    std::fs::write(root.join("new.rs"), "pub fn new_sentinel_xyz() {}").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "rename old->new"]);

    let mut engine = Engine::open(root).unwrap();
    let stats = engine.git_sync().unwrap();
    assert!(!stats.unchanged, "git_sync should detect the rename");
    assert_eq!(stats.modified, 1, "new file path should be re-indexed");
    assert_eq!(stats.removed, 1, "old file path should be removed");

    let old_results = engine
        .search(
            SearchQuery::new("old_sentinel_xyz")
                .with_limit(5)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();
    assert!(
        old_results.is_empty(),
        "old symbol should disappear after rename"
    );

    let new_results = engine
        .search(
            SearchQuery::new("new_sentinel_xyz")
                .with_limit(5)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();
    assert!(
        !new_results.is_empty(),
        "renamed file content should be indexed after git_sync"
    );
}

#[test]
#[serial]
fn git_sync_no_op_when_already_current() {
    if !git_available() {
        eprintln!("git_sync_no_op_when_already_current: skipped (git not in PATH)");
        return;
    }

    let dir = tempdir().unwrap();
    let root = dir.path();

    git(root, &["init"]);
    git(root, &["config", "user.email", "test@test.com"]);
    git(root, &["config", "user.name", "Test User"]);

    std::fs::write(root.join("lib.rs"), "pub fn hello() {}").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "initial"]);

    // Build index, drop engine to release Tantivy lock.
    drop(Engine::init(root, bm25_config(root)).unwrap());

    // No new commits — git_sync should be a no-op.
    let mut engine = Engine::open(root).unwrap();
    let stats = engine.git_sync().unwrap();

    assert!(
        stats.unchanged,
        "git_sync should report unchanged when HEAD == stored commit"
    );
    assert_eq!(stats.modified, 0);
    assert_eq!(stats.removed, 0);
}

#[test]
#[serial]
fn git_sync_no_op_without_git() {
    // When the project is not in a git repo, git_sync must return
    // unchanged=true without panicking.
    let dir = tempdir().unwrap();
    let root = dir.path();

    // Plain directory — not a git repo.
    std::fs::write(root.join("lib.rs"), "pub fn foo() {}").unwrap();

    // Build index, drop engine to release Tantivy lock.
    drop(Engine::init(root, bm25_config(root)).unwrap());

    // Open and call git_sync — must not fail.
    let mut engine = Engine::open(root).unwrap();
    let stats = engine.git_sync().unwrap();

    // No stored commit (non-git dir) → must be a graceful no-op.
    assert!(
        stats.unchanged,
        "git_sync should be a no-op for non-git directories"
    );
}

#[test]
#[serial]
fn serial_engine_open_no_lock_contention() {
    let dir1 = tempdir().unwrap();
    let root1 = dir1.path();
    std::fs::write(root1.join("a.rs"), "fn a() {}").unwrap();
    drop(Engine::init(root1, bm25_config(root1)).unwrap());
    let _e1 = Engine::open(root1).unwrap();

    let dir2 = tempdir().unwrap();
    let root2 = dir2.path();
    std::fs::write(root2.join("b.rs"), "fn b() {}").unwrap();
    drop(Engine::init(root2, bm25_config(root2)).unwrap());
    let _e2 = Engine::open(root2).unwrap();
}

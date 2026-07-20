/// Integration test for `codixing repair`.
///
/// Builds a real index, deletes the metadata files to reproduce the failure
/// mode reported in issue #100, and confirms `codixing repair` recreates
/// them and leaves the index in a usable state.
use std::process::Command;
use std::time::SystemTime;

fn write_expired(path: &std::path::Path, contents: &[u8]) {
    std::fs::write(path, contents).unwrap();
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
        .unwrap();
}

#[test]
fn repair_self_heals_partial_index_directory() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();

    // Build a real, openable index.
    std::fs::write(
        root.join("hello.rs"),
        "pub fn greet() -> &'static str { \"hi\" }\n",
    )
    .unwrap();
    let mut cfg = codixing_core::IndexConfig::new(&root);
    cfg.embedding.enabled = false;
    let engine = codixing_core::Engine::init(&root, cfg).unwrap();
    engine.save().unwrap();
    drop(engine);

    // Reproduce the issue: index dir exists, but config.json + meta.json are gone.
    let active_index_dir = codixing_core::persistence::IndexStore::open(&root)
        .unwrap()
        .codixing_dir()
        .to_path_buf();
    std::fs::remove_file(active_index_dir.join("config.json")).unwrap();
    std::fs::remove_file(active_index_dir.join("meta.json")).unwrap();

    // Pre-condition: search should now fail with PartialIndex / actionable error.
    let pre = Command::new(env!("CARGO_BIN_EXE_codixing"))
        .args(["search", "greet", "--count"])
        .current_dir(&root)
        .output()
        .unwrap();
    let pre_stderr = String::from_utf8_lossy(&pre.stderr).into_owned();
    assert!(!pre.status.success(), "search should fail on partial index");
    assert!(
        pre_stderr.contains("codixing repair") || pre_stderr.contains("incomplete"),
        "error should point at repair: {pre_stderr}"
    );

    // Run repair.
    let out = Command::new(env!("CARGO_BIN_EXE_codixing"))
        .args(["repair", root.to_str().unwrap()])
        .current_dir(&root)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "repair should succeed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("created") && stdout.contains("config.json"),
        "stdout should list recreated files: {stdout}"
    );

    // Post-condition: search should now work again.
    let post = Command::new(env!("CARGO_BIN_EXE_codixing"))
        .args(["search", "greet", "--count"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        post.status.success(),
        "search should succeed after repair\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&post.stdout),
        String::from_utf8_lossy(&post.stderr)
    );
}

#[test]
fn repair_complete_layout_reclaims_expired_vector_crash_debris_without_sync() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("hello.rs"), "pub fn greet() {}\n").unwrap();
    let mut config = codixing_core::IndexConfig::new(&root);
    config.embedding.enabled = false;
    drop(codixing_core::Engine::init(&root, config).unwrap());

    let store = codixing_core::persistence::IndexStore::open(&root).unwrap();
    let vectors = store.vectors_dir();
    drop(store);
    let generation = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-aaaaaaaa-aaaaaaaaaaaaaaaa";
    let orphan_index = vectors.join(format!("index.usearch.generation-{generation}"));
    let orphan_chunks = vectors.join(format!("index.usearch.file-chunks.generation-{generation}"));
    write_expired(&orphan_index, b"orphan index");
    write_expired(&orphan_chunks, b"orphan chunks");

    let out = Command::new(env!("CARGO_BIN_EXE_codixing"))
        .args(["repair", root.to_str().unwrap(), "--no-sync"])
        .current_dir(&root)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    assert!(
        out.status.success(),
        "cleanup-only repair should succeed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("Reclaimed expired unpublished vector artifacts"));
    assert!(stdout.contains("Published index contents were unchanged"));
    assert!(!stdout.contains("Running `sync`"));
    assert!(!orphan_index.exists());
    assert!(!orphan_chunks.exists());
}

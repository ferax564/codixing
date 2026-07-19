//! Regression coverage for concurrent CLI readers.

use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn search_does_not_retry_the_writer_lock() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("locked.rs"),
        "pub fn lock_free_reader_marker() -> bool { true }\n",
    )
    .unwrap();

    let mut config = codixing_core::IndexConfig::new(root);
    config.embedding.enabled = false;
    // Keep the writer alive while the CLI opens the same index. A read command
    // that calls Engine::open first pays the fixed ~1.02 s retry schedule
    // before falling back; Engine::open_read_only bypasses it entirely.
    let writer = codixing_core::Engine::init(root, config).unwrap();
    assert!(!writer.is_read_only());

    // Warm the executable and its dynamic libraries so the timing below
    // measures index-lock behavior rather than first-process paging on CI.
    let warmup = Command::new(env!("CARGO_BIN_EXE_codixing"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(warmup.status.success());

    let started = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_codixing"))
        .args([
            "search",
            "lock_free_reader_marker",
            "--strategy",
            "exact",
            "--json",
        ])
        .current_dir(root)
        .output()
        .unwrap();
    let elapsed = started.elapsed();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "concurrent search failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("lock_free_reader_marker"), "{stdout}");
    assert!(
        elapsed < Duration::from_secs(1),
        "read-only search waited for the writer retry loop: {elapsed:?}"
    );
}

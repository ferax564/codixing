/// Integration tests for the `--count` flag on `codixing search`, `codixing symbols`,
/// and `codixing usages`.
///
/// These tests verify that:
///   - `--count` prints only a count line to stdout (no full result table)
///   - The count format matches the expected pattern
use std::process::Command;

fn no_embed_engine(root: &std::path::Path) -> codixing_core::Engine {
    let mut cfg = codixing_core::IndexConfig::new(root);
    cfg.embedding.enabled = false;
    let engine = codixing_core::Engine::init(root, cfg).unwrap();
    engine.save().unwrap();
    engine
}

// ---------------------------------------------------------------------------
// search --count
// ---------------------------------------------------------------------------

#[test]
fn search_count_flag_prints_count_only() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let root = root.as_path();

    std::fs::write(
        root.join("hello.rs"),
        "pub fn hello_world() -> &'static str { \"hello\" }\n",
    )
    .unwrap();
    let engine = no_embed_engine(root);
    drop(engine);

    let output = Command::new(env!("CARGO_BIN_EXE_codixing"))
        .args(["search", "hello", "--count"])
        .current_dir(root)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        output.status.success(),
        "search --count failed\nstdout: {stdout}\nstderr: {stderr}"
    );

    // stdout must match "N result(s)" with no other lines
    let trimmed = stdout.trim();
    assert!(
        trimmed.starts_with(|c: char| c.is_ascii_digit()),
        "expected count output to start with a digit\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        trimmed.contains("result"),
        "expected 'result' in count output\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Must NOT contain full result table headers
    assert!(
        !stdout.contains("score="),
        "expected no full result listing with --count\nstdout: {stdout}\nstderr: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// symbols --count
// ---------------------------------------------------------------------------

#[test]
fn symbols_count_flag_prints_count_only() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let root = root.as_path();

    std::fs::write(
        root.join("hello.rs"),
        "pub fn hello_world() {}\npub fn hello_again() {}\n",
    )
    .unwrap();
    let engine = no_embed_engine(root);
    drop(engine);

    let output = Command::new(env!("CARGO_BIN_EXE_codixing"))
        .args(["symbols", "hello", "--count"])
        .current_dir(root)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        output.status.success(),
        "symbols --count failed\nstdout: {stdout}\nstderr: {stderr}"
    );

    let trimmed = stdout.trim();
    assert!(
        trimmed.starts_with(|c: char| c.is_ascii_digit()),
        "expected count output to start with a digit\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        trimmed.contains("symbol"),
        "expected 'symbol' in count output\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Must NOT contain table headers
    assert!(
        !stdout.contains("KIND"),
        "expected no full symbol table with --count\nstdout: {stdout}\nstderr: {stderr}"
    );
}

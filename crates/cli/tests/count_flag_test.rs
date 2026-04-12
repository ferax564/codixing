/// Integration tests for the `--count` flag on `codixing search`, `codixing symbols`,
/// and `codixing usages`.
///
/// These tests verify that:
///   - `--count` prints exactly one count line to stdout (no full result table)
///   - The count format matches the expected pattern
use std::process::Command;

fn no_embed_engine(root: &std::path::Path) -> codixing_core::Engine {
    let mut cfg = codixing_core::IndexConfig::new(root);
    cfg.embedding.enabled = false;
    let engine = codixing_core::Engine::init(root, cfg).unwrap();
    engine.save().unwrap();
    engine
}

/// Pull the single non-empty line of stdout, asserting there is exactly one.
/// Returns the line without surrounding whitespace.
fn assert_single_stdout_line(stdout: &str, stderr: &str, context: &str) -> String {
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        1,
        "{context}: expected exactly one non-empty stdout line\nstdout: {stdout}\nstderr: {stderr}"
    );
    lines[0].trim().to_string()
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

    let line = assert_single_stdout_line(&stdout, &stderr, "search --count");
    assert!(
        line.starts_with(|c: char| c.is_ascii_digit()),
        "expected count line to start with a digit\nline: {line}"
    );
    assert!(
        line.contains("result") && line.contains("found"),
        "expected 'N result(s) found' format\nline: {line}"
    );
    assert!(
        !stdout.contains("score="),
        "expected no full result listing with --count\nstdout: {stdout}"
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

    let line = assert_single_stdout_line(&stdout, &stderr, "symbols --count");
    assert!(
        line.starts_with(|c: char| c.is_ascii_digit()),
        "expected count line to start with a digit\nline: {line}"
    );
    assert!(
        line.contains("symbol"),
        "expected 'symbol' in count line\nline: {line}"
    );
    assert!(
        !stdout.contains("KIND"),
        "expected no full symbol table with --count\nstdout: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// usages --count
// ---------------------------------------------------------------------------

#[test]
fn usages_count_flag_prints_count_only() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let root = root.as_path();

    std::fs::write(
        root.join("hello.rs"),
        "pub fn hello_world() {}\npub fn call_hello() { hello_world(); }\n",
    )
    .unwrap();
    let engine = no_embed_engine(root);
    drop(engine);

    let output = Command::new(env!("CARGO_BIN_EXE_codixing"))
        .args(["usages", "hello_world", "--count"])
        .current_dir(root)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        output.status.success(),
        "usages --count failed\nstdout: {stdout}\nstderr: {stderr}"
    );

    let line = assert_single_stdout_line(&stdout, &stderr, "usages --count");
    assert!(
        line.starts_with(|c: char| c.is_ascii_digit()),
        "expected count line to start with a digit\nline: {line}"
    );
    assert!(
        line.contains("usage"),
        "expected 'usage' in count line\nline: {line}"
    );
    assert!(
        !stdout.contains("LOCATION"),
        "expected no full usages table with --count\nstdout: {stdout}"
    );
}

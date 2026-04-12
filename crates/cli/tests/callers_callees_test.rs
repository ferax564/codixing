/// Integration tests for `codixing callers` and `codixing callees` commands.
///
/// These tests verify the in-process fallback path (no daemon running).
/// When a daemon is not running, `daemon_proxy::try_callers` / `try_callees`
/// return `None` and the commands fall through to the engine-backed path.
use std::fs;
use std::process::Command;

fn no_embed_engine(root: &std::path::Path) -> codixing_core::Engine {
    let mut cfg = codixing_core::IndexConfig::new(root);
    cfg.embedding.enabled = false;
    let engine = codixing_core::Engine::init(root, cfg).unwrap();
    engine.save().unwrap();
    engine
}

/// Set up a small two-file Rust project where `main.rs` imports `lib.rs`.
fn setup_two_file_project(root: &std::path::Path) {
    fs::write(root.join("lib.rs"), "pub fn helper() -> i32 { 42 }\n").unwrap();
    fs::write(
        root.join("main.rs"),
        "mod lib;\nfn main() { let _ = lib::helper(); }\n",
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// cmd_callers — in-process path (no daemon)
// ---------------------------------------------------------------------------

#[test]
fn callers_in_process_finds_importer() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let root = root.as_path();

    setup_two_file_project(root);
    let engine = no_embed_engine(root);
    drop(engine);

    let output = Command::new(env!("CARGO_BIN_EXE_codixing"))
        .args(["callers", "lib.rs"])
        .current_dir(root)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        output.status.success(),
        "callers command failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("main.rs"),
        "expected main.rs in callers output for lib.rs\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("caller(s) found"),
        "expected caller count in stderr\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
fn callers_in_process_empty_for_leaf() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let root = root.as_path();

    setup_two_file_project(root);
    let engine = no_embed_engine(root);
    drop(engine);

    // main.rs is not imported by anyone, so callers should be empty.
    let output = Command::new(env!("CARGO_BIN_EXE_codixing"))
        .args(["callers", "main.rs"])
        .current_dir(root)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        output.status.success(),
        "callers command failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    // stdout should be empty (no caller file paths printed)
    assert!(
        stdout.trim().is_empty(),
        "expected empty stdout for main.rs callers\nstdout: {stdout}\nstderr: {stderr}"
    );
    // stderr should report "No callers found"
    assert!(
        stderr.contains("No callers found"),
        "expected 'No callers found' message in stderr\nstdout: {stdout}\nstderr: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// cmd_callees — in-process path (no daemon)
// ---------------------------------------------------------------------------

#[test]
fn callees_in_process_finds_dependency() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let root = root.as_path();

    setup_two_file_project(root);
    let engine = no_embed_engine(root);
    drop(engine);

    let output = Command::new(env!("CARGO_BIN_EXE_codixing"))
        .args(["callees", "main.rs"])
        .current_dir(root)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        output.status.success(),
        "callees command failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("lib.rs"),
        "expected lib.rs in callees output for main.rs\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("dependency"),
        "expected dependency count in stderr\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
fn callees_in_process_empty_for_leaf() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let root = root.as_path();

    setup_two_file_project(root);
    let engine = no_embed_engine(root);
    drop(engine);

    // lib.rs imports nothing, so callees should be empty.
    let output = Command::new(env!("CARGO_BIN_EXE_codixing"))
        .args(["callees", "lib.rs"])
        .current_dir(root)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        output.status.success(),
        "callees command failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    // stdout should be empty
    assert!(
        stdout.trim().is_empty(),
        "expected empty stdout for lib.rs callees\nstdout: {stdout}\nstderr: {stderr}"
    );
    // stderr should report "No dependencies found"
    assert!(
        stderr.contains("No dependencies found"),
        "expected 'No dependencies found' in stderr\nstdout: {stdout}\nstderr: {stderr}"
    );
}

//! Integration tests for `codixing grep` — the CLI surface added in v0.36.
//!
//! Exercises the path:line:col:text default format plus `--count`,
//! `--files-with-matches`, `--json`, `--ignore-case`, `--invert`,
//! `--literal`, asymmetric context, and `--glob`.

use std::process::Command;

fn no_embed_engine(root: &std::path::Path) -> codixing_core::Engine {
    let mut cfg = codixing_core::IndexConfig::new(root);
    cfg.embedding.enabled = false;
    let engine = codixing_core::Engine::init(root, cfg).unwrap();
    engine.save().unwrap();
    engine
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join("src")).unwrap();

    std::fs::write(
        root.join("src/auth.rs"),
        "fn authenticate_user(token: &str) -> bool {\n    token.len() > 0\n}\n\n\
         fn logout() {}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/handler.rs"),
        "use crate::auth::authenticate_user;\n\n\
         fn handle(req: &Request) {\n    authenticate_user(&req.token);\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/config.rs"),
        "fn load_config() { /* TODO: read from env */ }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("notes.py"),
        "# TODO: port to rust\ndef stub():\n    pass\n",
    )
    .unwrap();

    let engine = no_embed_engine(&root);
    drop(engine);

    dir
}

fn run(root: &std::path::Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_codixing"))
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status.success(), stdout, stderr)
}

#[test]
fn grep_literal_default_format() {
    let dir = fixture();
    let (ok, stdout, stderr) = run(dir.path(), &["grep", "authenticate_user", "--literal"]);
    assert!(ok, "grep failed\n{stdout}\n{stderr}");
    // Expect path:line:col:text lines — one per match
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(
        lines.len() >= 3,
        "expected 3+ matches, got {}: {stdout}",
        lines.len()
    );
    for line in &lines {
        let parts: Vec<&str> = line.splitn(4, ':').collect();
        assert_eq!(parts.len(), 4, "malformed path:line:col:text line: {line}");
        assert!(parts[0].ends_with(".rs"));
        assert!(parts[1].parse::<u32>().is_ok(), "line not u32: {line}");
        assert!(parts[2].parse::<u32>().is_ok(), "col not u32: {line}");
    }
}

#[test]
fn grep_count_prints_summary() {
    let dir = fixture();
    let (ok, stdout, stderr) = run(
        dir.path(),
        &["grep", "authenticate_user", "--literal", "--count"],
    );
    assert!(ok, "grep --count failed\n{stdout}\n{stderr}");
    let line = stdout.trim();
    assert!(
        line.contains("matches across") && line.contains("files"),
        "expected count summary, got: {line}"
    );
}

#[test]
fn grep_files_with_matches_prints_paths() {
    let dir = fixture();
    let (ok, stdout, _) = run(
        dir.path(),
        &["grep", "TODO", "--literal", "--files-with-matches"],
    );
    assert!(ok);
    let files: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(files.iter().any(|f| f.ends_with("config.rs")));
    assert!(files.iter().any(|f| f.ends_with("notes.py")));
    // Sorted, deduped
    let mut sorted = files.clone();
    sorted.sort();
    assert_eq!(files, sorted, "files-with-matches output not sorted");
}

#[test]
fn grep_ignore_case_matches_mixed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("mixed.rs"),
        "let WIDGET = 1;\nlet widget = 2;\nlet Widget = 3;\n",
    )
    .unwrap();
    let engine = no_embed_engine(root);
    drop(engine);

    let (ok, stdout, _) = run(
        root,
        &["grep", "widget", "--literal", "--ignore-case", "--count"],
    );
    assert!(ok);
    assert!(stdout.contains("3 matches"), "got: {stdout}");
}

#[test]
fn grep_invert_emits_nonmatching_lines() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("file.rs"),
        "use foo;\nfn a() {}\nuse bar;\nfn b() {}\n",
    )
    .unwrap();
    let engine = no_embed_engine(root);
    drop(engine);

    let (ok, stdout, stderr) = run(root, &["grep", "^use ", "--invert"]);
    assert!(ok, "{stdout}\n{stderr}");
    // Expect the two `fn` lines, neither `use` line
    assert!(stdout.contains("fn a()"));
    assert!(stdout.contains("fn b()"));
    assert!(!stdout.contains("use foo"));
    assert!(!stdout.contains("use bar"));
}

#[test]
fn grep_json_output_one_object_per_line() {
    let dir = fixture();
    let (ok, stdout, _) = run(
        dir.path(),
        &[
            "grep",
            "authenticate_user",
            "--literal",
            "--json",
            "--limit",
            "3",
        ],
    );
    assert!(ok);
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(!lines.is_empty());
    for line in &lines {
        let v: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("bad JSON `{line}`: {e}"));
        assert!(v.get("path").is_some());
        assert!(v.get("line").and_then(|n| n.as_u64()).is_some());
        assert!(v.get("col").and_then(|n| n.as_u64()).is_some());
        assert!(v.get("text").and_then(|n| n.as_str()).is_some());
    }
}

#[test]
fn grep_glob_restricts_scan() {
    let dir = fixture();
    let (ok, stdout, _) = run(
        dir.path(),
        &[
            "grep",
            "TODO",
            "--literal",
            "--glob",
            "*.py",
            "--files-with-matches",
        ],
    );
    assert!(ok);
    let files: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(files.iter().all(|f| f.ends_with(".py")), "{files:?}");
    assert!(files.iter().any(|f| f.ends_with("notes.py")));
}

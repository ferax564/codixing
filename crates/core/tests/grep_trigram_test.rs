//! Integration tests for `grep_code` with trigram pre-filtering.
//!
//! These tests verify the full pipeline: Engine::init → FileTrigramIndex built
//! → grep_code uses trigram pre-filter → correct results.

mod common;

use std::fs;

use codixing_core::{Engine, GrepOptions, IndexConfig};
use tempfile::tempdir;

/// Create an `IndexConfig` with embeddings disabled (BM25-only mode).
fn bm25_config(root: &std::path::Path) -> IndexConfig {
    let mut cfg = IndexConfig::new(root);
    cfg.embedding.enabled = false;
    cfg
}

// ── E2E grep_code with trigram pre-filter ────────────────────────────────────

#[test]
fn grep_literal_uses_trigram_prefilter() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    fs::write(
        src.join("auth.rs"),
        "fn authenticate_user(token: &str) -> bool { true }\n",
    )
    .unwrap();
    fs::write(
        src.join("config.rs"),
        "fn load_config(path: &str) -> Config { todo!() }\n",
    )
    .unwrap();
    fs::write(
        src.join("handler.rs"),
        "fn handle_request(req: Request) { authenticate_user(&req.token); }\n",
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    // Literal search: only auth.rs and handler.rs contain "authenticate_user"
    let results = engine
        .grep_code("authenticate_user", true, None, 0, 50)
        .unwrap();

    assert!(
        !results.is_empty(),
        "expected grep results for 'authenticate_user'"
    );
    let files: Vec<&str> = results.iter().map(|m| m.file_path.as_str()).collect();
    assert!(
        files.iter().any(|f| f.contains("auth.rs")),
        "expected auth.rs in results, got: {files:?}"
    );
    assert!(
        files.iter().any(|f| f.contains("handler.rs")),
        "expected handler.rs in results, got: {files:?}"
    );
    assert!(
        !files.iter().any(|f| f.contains("config.rs")),
        "config.rs should NOT match 'authenticate_user', got: {files:?}"
    );
}

#[test]
fn grep_regex_or_pattern_uses_query_plan() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    fs::write(
        src.join("tokio_runtime.rs"),
        "use tokio::runtime::Runtime;\nfn run_tokio() {}\n",
    )
    .unwrap();
    fs::write(
        src.join("asyncstd_tasks.rs"),
        "use async_std::task;\nfn run_async_std() {}\n",
    )
    .unwrap();
    fs::write(
        src.join("blocking.rs"),
        "fn run_blocking() { std::thread::sleep(Duration::from_secs(1)); }\n",
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    // OR pattern: (tokio|async_std) should find both async files via QueryPlan OR
    let results = engine
        .grep_code("(tokio|async_std)", false, None, 0, 50)
        .unwrap();

    let files: Vec<&str> = results.iter().map(|m| m.file_path.as_str()).collect();
    assert!(
        files.iter().any(|f| f.contains("tokio_runtime.rs")),
        "expected tokio_runtime.rs in results, got: {files:?}"
    );
    assert!(
        files.iter().any(|f| f.contains("asyncstd_tasks.rs")),
        "expected asyncstd_tasks.rs in results, got: {files:?}"
    );
    assert!(
        !files.iter().any(|f| f.contains("blocking.rs")),
        "blocking.rs should NOT match, got: {files:?}"
    );
}

#[test]
fn grep_literal_with_regex_metacharacters() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    fs::write(
        src.join("array.rs"),
        "let val = items[0].field;\nlet other = items[1].field;\n",
    )
    .unwrap();
    fs::write(
        src.join("unrelated.rs"),
        "fn process(x: i32) -> i32 { x + 1 }\n",
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    // Literal mode: "items[0].field" should NOT be interpreted as regex
    let results = engine
        .grep_code("items[0].field", true, None, 0, 50)
        .unwrap();

    assert!(
        !results.is_empty(),
        "expected grep results for literal 'items[0].field'"
    );
    let files: Vec<&str> = results.iter().map(|m| m.file_path.as_str()).collect();
    assert!(
        files.iter().any(|f| f.contains("array.rs")),
        "expected array.rs in results, got: {files:?}"
    );
    assert!(
        !files.iter().any(|f| f.contains("unrelated.rs")),
        "unrelated.rs should NOT match, got: {files:?}"
    );
}

// ── Glob + trigram interaction ───────────────────────────────────────────────

#[test]
fn grep_with_glob_and_trigram_filter() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    // Both files contain "parse_config" but only the .py file matches the glob
    fs::write(
        src.join("config.rs"),
        "fn parse_config(path: &str) -> Config { todo!() }\n",
    )
    .unwrap();
    fs::write(
        src.join("config.py"),
        "def parse_config(path: str) -> dict:\n    return {}\n",
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    // Trigram narrows to both files, glob further narrows to .py only
    let results = engine
        .grep_code("parse_config", true, Some("*.py"), 0, 50)
        .unwrap();

    assert!(
        !results.is_empty(),
        "expected grep results for 'parse_config' with *.py glob"
    );
    for m in &results {
        assert!(
            m.file_path.ends_with(".py"),
            "expected only .py results, got: {}",
            m.file_path
        );
    }
}

// ── GrepOptions: case-insensitive, invert, asymmetric context ────────────────

fn opts(pattern: &str) -> GrepOptions {
    GrepOptions::from_simple(pattern, true, None, 0, 50)
}

#[test]
fn grep_case_insensitive_literal_matches_mixed_case() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    fs::write(
        src.join("mixed.rs"),
        "struct WidgetFactory;\nfn make_widgetfactory() {}\nfn WIDGETFACTORY_PANIC() {}\n",
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let mut o = opts("widgetfactory");
    o.case_insensitive = true;
    let results = engine.grep_code_opts(&o).unwrap();

    assert_eq!(
        results.len(),
        3,
        "expected 3 case-insensitive matches, got {}: {results:?}",
        results.len()
    );
}

#[test]
fn grep_case_insensitive_regex_builds_via_regex_builder() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    fs::write(
        src.join("todo.rs"),
        "// TODO: fix\n// todo(alice): here\n// Normal line\n",
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let mut o = GrepOptions::from_simple("todo", false, None, 0, 50);
    o.case_insensitive = true;
    let results = engine.grep_code_opts(&o).unwrap();

    assert_eq!(results.len(), 2);
}

#[test]
fn grep_invert_returns_non_matching_lines() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    fs::write(
        src.join("mixed.rs"),
        "use foo::bar;\nfn a() {}\nuse baz::qux;\nfn b() {}\n",
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let mut o = GrepOptions::from_simple("^use ", false, None, 0, 50);
    o.invert = true;
    let results = engine.grep_code_opts(&o).unwrap();

    let lines: Vec<&str> = results.iter().map(|m| m.line.as_str()).collect();
    assert!(lines.iter().all(|l| !l.starts_with("use ")), "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("fn a()")));
    assert!(lines.iter().any(|l| l.contains("fn b()")));
}

#[test]
fn grep_asymmetric_before_after_context() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    let content = (0..10)
        .map(|i| format!("line_{i}_content"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(src.join("numbered.rs"), content).unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let mut o = opts("line_5_content");
    o.before_context = 2;
    o.after_context = 4;
    let results = engine.grep_code_opts(&o).unwrap();

    assert_eq!(results.len(), 1);
    let m = &results[0];
    assert_eq!(m.before.len(), 2, "expected 2 before-context lines");
    assert_eq!(m.after.len(), 4, "expected 4 after-context lines");
    assert_eq!(m.before[0], "line_3_content");
    assert_eq!(m.before[1], "line_4_content");
    assert_eq!(m.after[0], "line_6_content");
    assert_eq!(m.after[3], "line_9_content");
}

// ── Chunk boundary trigrams ──────────────────────────────────────────────────

#[test]
fn grep_finds_pattern_near_chunk_boundary() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    // Create a large file where the target pattern appears deep into the content.
    // This tests that the file-level trigram index (built from full content)
    // correctly includes trigrams regardless of how the chunker splits the file.
    let mut content = String::new();
    for i in 0..200 {
        content.push_str(&format!("fn function_{i}(x: i32) -> i32 {{ x + {i} }}\n"));
    }
    // Insert the target pattern after many functions
    content.push_str("fn unique_boundary_marker_xyz() -> bool { true }\n");
    for i in 200..400 {
        content.push_str(&format!("fn function_{i}(x: i32) -> i32 {{ x + {i} }}\n"));
    }

    fs::write(src.join("large.rs"), &content).unwrap();
    fs::write(src.join("small.rs"), "fn unrelated() -> bool { false }\n").unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let results = engine
        .grep_code("unique_boundary_marker_xyz", true, None, 0, 50)
        .unwrap();

    assert_eq!(
        results.len(),
        1,
        "expected exactly 1 match for unique pattern, got: {}",
        results.len()
    );
    assert!(
        results[0].file_path.contains("large.rs"),
        "expected match in large.rs, got: {}",
        results[0].file_path
    );
}

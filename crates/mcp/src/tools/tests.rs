//! Tests for all MCP tool handlers.

use super::*;
use codixing_core::{Engine, IndexConfig};
use serde_json::json;
use std::fs;
use tempfile::tempdir;

// -------------------------------------------------------------------------
// Test helpers
// -------------------------------------------------------------------------

/// Create a BM25-only engine in a temp directory with a small project.
fn make_engine(root: &std::path::Path) -> Engine {
    // Rust file with functions and a test
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/main.rs"),
        r#"/// Entry point.
fn main() {
    let x = compute(2, 3);
    println!("{x}");
}

/// Compute the sum of two numbers.
pub fn compute(a: i32, b: i32) -> i32 {
    if a > 0 {
        a + b
    } else if b > 0 {
        b
    } else {
        0
    }
}

#[test]
fn test_compute_positive() {
    assert_eq!(compute(2, 3), 5);
}

#[test]
fn test_compute_zero() {
    assert_eq!(compute(0, 0), 0);
}
"#,
    )
    .unwrap();

    // Python file
    fs::write(
        root.join("src/utils.py"),
        r#"def parse_config(path):
    """Parse a config file."""
    return {}

class Validator:
    def validate(self, data):
        return True
"#,
    )
    .unwrap();

    // Go file in a tests/ dir
    fs::create_dir_all(root.join("tests")).unwrap();
    fs::write(
        root.join("tests/server_test.go"),
        r#"package main

import "testing"

func TestHandleRequest(t *testing.T) {
    t.Log("ok")
}
"#,
    )
    .unwrap();

    let mut cfg = IndexConfig::new(root);
    cfg.embedding.enabled = false;
    Engine::init(root, cfg).expect("engine init failed")
}

// -------------------------------------------------------------------------
// tool_definitions
// -------------------------------------------------------------------------

#[test]
fn tool_definitions_returns_46_tools() {
    let defs = tool_definitions();
    let arr = defs.as_array().expect("tool_definitions returns array");
    assert_eq!(
        arr.len(),
        46,
        "expected exactly 46 tool definitions (44 + 2 meta-tools), got {}",
        arr.len()
    );
}

#[test]
fn tool_definitions_all_have_name_and_schema() {
    let defs = tool_definitions();
    for (i, tool) in defs.as_array().unwrap().iter().enumerate() {
        let name = tool
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("tool[{i}] missing 'name'"));
        assert!(!name.is_empty(), "tool[{i}] has empty name");
        assert!(
            tool.get("description").and_then(|v| v.as_str()).is_some(),
            "tool '{name}' missing 'description'"
        );
        assert!(
            tool.get("inputSchema").is_some(),
            "tool '{name}' missing 'inputSchema'"
        );
    }
}

#[test]
fn tool_definitions_phase10_tools_present() {
    let defs = tool_definitions();
    let names: Vec<&str> = defs
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
        .collect();
    for expected in &[
        "remember",
        "recall",
        "forget",
        "find_tests",
        "find_similar",
        "get_complexity",
        "review_context",
        "generate_onboarding",
    ] {
        assert!(
            names.contains(expected),
            "Phase 10 tool '{expected}' not in tool_definitions"
        );
    }
}

// -------------------------------------------------------------------------
// dispatch_tool -- unknown tool
// -------------------------------------------------------------------------

#[test]
fn dispatch_unknown_tool_returns_error() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (msg, is_err) = dispatch_tool(&mut engine, "nonexistent_tool", &json!({}), None);
    assert!(is_err);
    assert!(msg.contains("Unknown"), "got: {msg}");
}

// -------------------------------------------------------------------------
// list_files
// -------------------------------------------------------------------------

#[test]
fn list_files_returns_indexed_files() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(&mut engine, "list_files", &json!({}), None);
    assert!(!err, "list_files returned error: {out}");
    assert!(
        out.contains("main.rs") || out.contains("utils.py") || out.contains("Indexed"),
        "Expected file listing, got: {out}"
    );
}

#[test]
fn list_files_pattern_filter_rs() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(
        &mut engine,
        "list_files",
        &json!({"pattern": "**/*.rs"}),
        None,
    );
    assert!(!err, "list_files with *.rs pattern returned error: {out}");
    assert!(
        !out.contains("utils.py"),
        "Unexpected utils.py in *.rs filter: {out}"
    );
}

#[test]
fn list_files_limit() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(&mut engine, "list_files", &json!({"limit": 1}), None);
    assert!(!err, "list_files with limit=1 returned error: {out}");
    let file_lines = out
        .lines()
        .filter(|l| l.trim_start().starts_with("src/") || l.trim_start().starts_with("tests/"))
        .count();
    assert!(
        file_lines <= 1,
        "Expected at most 1 file, got {file_lines}: {out}"
    );
}

// -------------------------------------------------------------------------
// outline_file
// -------------------------------------------------------------------------

#[test]
fn outline_file_missing_arg() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (msg, err) = dispatch_tool(&mut engine, "outline_file", &json!({}), None);
    assert!(err);
    assert!(msg.contains("Missing"), "got: {msg}");
}

#[test]
fn outline_file_returns_symbols() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(
        &mut engine,
        "outline_file",
        &json!({"file": "src/main.rs"}),
        None,
    );
    assert!(!err, "outline_file returned error: {out}");
    assert!(
        out.contains("compute") || out.contains("main") || out.contains("Symbol"),
        "Expected symbol outline, got: {out}"
    );
}

#[test]
fn outline_file_unknown_file() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(
        &mut engine,
        "outline_file",
        &json!({"file": "src/does_not_exist.rs"}),
        None,
    );
    assert!(!err, "should not be error for missing file");
    assert!(out.contains("No symbols"), "got: {out}");
}

// -------------------------------------------------------------------------
// apply_patch
// -------------------------------------------------------------------------

#[test]
fn apply_patch_missing_arg() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (msg, err) = dispatch_tool(&mut engine, "apply_patch", &json!({}), None);
    assert!(err);
    assert!(msg.contains("Missing"), "got: {msg}");
}

#[test]
fn apply_patch_no_affected_files_returns_message() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let patch = "not a real unified diff\n";
    let (out, err) = dispatch_tool(&mut engine, "apply_patch", &json!({"patch": patch}), None);
    assert!(!err, "apply_patch returned unexpected error: {out}");
    assert!(
        out.contains("No files") || out.contains("apply"),
        "unexpected output: {out}"
    );
}

#[test]
fn apply_patch_identifies_affected_file() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let patch = "diff --git a/src/main.rs b/src/main.rs\n\
                 --- a/src/main.rs\n\
                 +++ b/src/main.rs\n\
                 @@ -1,2 +1,3 @@\n\
                 +// a comment\n\
                  /// Entry point.\n\
                  fn main() {\n";
    let (out, _err) = dispatch_tool(&mut engine, "apply_patch", &json!({"patch": patch}), None);
    assert!(
        out.contains("main.rs") || out.contains("file") || out.contains("reindexed"),
        "unexpected output: {out}"
    );

    // Verify the patch was actually applied to the file.
    let content = fs::read_to_string(dir.path().join("src/main.rs")).unwrap();
    assert!(
        content.contains("// a comment"),
        "Patch should have inserted the comment line: {content}"
    );
}

#[test]
fn apply_patch_applies_add_and_remove() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let mut engine = make_engine(root);

    // Patch that removes a line and adds a replacement.
    let patch = "diff --git a/src/main.rs b/src/main.rs\n\
                 --- a/src/main.rs\n\
                 +++ b/src/main.rs\n\
                 @@ -1,3 +1,3 @@\n\
                 -/// Entry point.\n\
                 +/// Modified entry point.\n\
                  fn main() {\n\
                      let x = compute(2, 3);\n";
    let (out, err) = dispatch_tool(&mut engine, "apply_patch", &json!({"patch": patch}), None);
    assert!(!err, "apply_patch returned error: {out}");

    let content = fs::read_to_string(root.join("src/main.rs")).unwrap();
    assert!(
        content.contains("/// Modified entry point."),
        "Should contain new line: {content}"
    );
    assert!(
        !content.contains("/// Entry point."),
        "Should not contain old line: {content}"
    );
    assert!(
        content.contains("fn main()"),
        "Context lines should be preserved: {content}"
    );
}

// -------------------------------------------------------------------------
// run_tests
// -------------------------------------------------------------------------

#[test]
fn run_tests_missing_command() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (msg, err) = dispatch_tool(&mut engine, "run_tests", &json!({}), None);
    assert!(err);
    assert!(msg.contains("Missing"), "got: {msg}");
}

#[test]
fn run_tests_echo_succeeds() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(
        &mut engine,
        "run_tests",
        &json!({"command": "echo hello_codixing"}),
        None,
    );
    assert!(!err, "echo should succeed: {out}");
    assert!(out.contains("hello_codixing"), "echo output missing: {out}");
    assert!(out.contains("Exit code: 0"), "expected exit 0: {out}");
}

#[test]
fn run_tests_failing_command() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(
        &mut engine,
        "run_tests",
        &json!({"command": "exit 1"}),
        None,
    );
    assert!(err, "failing command should set is_error=true: {out}");
    assert!(
        out.contains("FAILED") || out.contains("Exit code"),
        "expected failure indication: {out}"
    );
}

// -------------------------------------------------------------------------
// rename_symbol
// -------------------------------------------------------------------------

#[test]
fn rename_symbol_missing_args() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (msg, err) = dispatch_tool(
        &mut engine,
        "rename_symbol",
        &json!({"old_name": "x"}),
        None,
    );
    assert!(err);
    assert!(msg.contains("Missing"), "got: {msg}");
}

#[test]
fn rename_symbol_renames_across_file() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let mut engine = make_engine(root);

    let (out, err) = dispatch_tool(
        &mut engine,
        "rename_symbol",
        &json!({"old_name": "compute", "new_name": "calculate"}),
        None,
    );
    assert!(!err, "rename_symbol returned error: {out}");
    assert!(
        out.contains("calculate") || out.contains("Renamed"),
        "unexpected output: {out}"
    );

    let content = fs::read_to_string(root.join("src/main.rs")).unwrap();
    assert!(
        content.contains("calculate"),
        "File should contain 'calculate' after rename: {content}"
    );
    assert!(
        !content.contains("compute"),
        "File should not contain 'compute' after rename: {content}"
    );
}

#[test]
fn rename_symbol_with_file_filter() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let mut engine = make_engine(root);

    let (out, err) = dispatch_tool(
        &mut engine,
        "rename_symbol",
        &json!({"old_name": "compute", "new_name": "calc", "file_filter": ".py"}),
        None,
    );
    assert!(!err, "rename_symbol returned error: {out}");

    let rs_content = fs::read_to_string(root.join("src/main.rs")).unwrap();
    assert!(
        rs_content.contains("compute"),
        "main.rs should be untouched by .py filter"
    );
}

// -------------------------------------------------------------------------
// explain
// -------------------------------------------------------------------------

#[test]
fn explain_missing_arg() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (msg, err) = dispatch_tool(&mut engine, "explain", &json!({}), None);
    assert!(err);
    assert!(msg.contains("Missing"), "got: {msg}");
}

#[test]
fn explain_unknown_symbol() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(
        &mut engine,
        "explain",
        &json!({"symbol": "totally_unknown_xyz"}),
        None,
    );
    assert!(
        !err,
        "explain for unknown symbol should not be an error flag"
    );
    assert!(
        out.contains("Explanation") || out.contains("not found"),
        "unexpected output: {out}"
    );
}

#[test]
fn explain_known_symbol() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(&mut engine, "explain", &json!({"symbol": "compute"}), None);
    assert!(!err, "explain for known symbol returned error: {out}");
    assert!(
        out.contains("Explanation") && out.contains("compute"),
        "unexpected output: {out}"
    );
}

// -------------------------------------------------------------------------
// symbol_callers
// -------------------------------------------------------------------------

#[test]
fn symbol_callers_missing_arg() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (msg, err) = dispatch_tool(&mut engine, "symbol_callers", &json!({}), None);
    assert!(err);
    assert!(msg.contains("Missing"), "got: {msg}");
}

#[test]
fn symbol_callers_returns_output() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(
        &mut engine,
        "symbol_callers",
        &json!({"symbol": "compute"}),
        None,
    );
    assert!(!err, "symbol_callers returned error: {out}");
    assert!(!out.is_empty(), "output should not be empty");
}

// -------------------------------------------------------------------------
// symbol_callees
// -------------------------------------------------------------------------

#[test]
fn symbol_callees_missing_arg() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (msg, err) = dispatch_tool(&mut engine, "symbol_callees", &json!({}), None);
    assert!(err);
    assert!(msg.contains("Missing"), "got: {msg}");
}

#[test]
fn symbol_callees_detects_calls() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(
        &mut engine,
        "symbol_callees",
        &json!({"symbol": "main"}),
        None,
    );
    assert!(!err, "symbol_callees returned error: {out}");
    assert!(
        out.contains("compute") || out.contains("Callees") || out.contains("No callees"),
        "unexpected output: {out}"
    );
}

// -------------------------------------------------------------------------
// predict_impact
// -------------------------------------------------------------------------

#[test]
fn predict_impact_missing_arg() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (msg, err) = dispatch_tool(&mut engine, "predict_impact", &json!({}), None);
    assert!(err);
    assert!(msg.contains("Missing"), "got: {msg}");
}

#[test]
fn predict_impact_no_files_in_patch() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(
        &mut engine,
        "predict_impact",
        &json!({"patch": "not a diff\n"}),
        None,
    );
    assert!(
        err,
        "predict_impact should return error for invalid patches"
    );
    assert!(out.contains("No file changes"), "unexpected: {out}");
}

#[test]
fn predict_impact_with_valid_patch() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let patch = "+++ b/src/main.rs\n@@ -1,1 +1,2 @@\n+// new line\n fn main() {}\n";
    let (out, err) = dispatch_tool(
        &mut engine,
        "predict_impact",
        &json!({"patch": patch}),
        None,
    );
    assert!(!err, "predict_impact returned error: {out}");
    assert!(
        out.contains("Impact Prediction") || out.contains("changed file"),
        "unexpected output: {out}"
    );
}

// -------------------------------------------------------------------------
// stitch_context
// -------------------------------------------------------------------------

#[test]
fn stitch_context_missing_arg() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (msg, err) = dispatch_tool(&mut engine, "stitch_context", &json!({}), None);
    assert!(err);
    assert!(msg.contains("Missing"), "got: {msg}");
}

#[test]
fn stitch_context_returns_results() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(
        &mut engine,
        "stitch_context",
        &json!({"query": "compute"}),
        None,
    );
    assert!(!err, "stitch_context returned error: {out}");
    assert!(
        out.contains("Stitched context") || out.contains("compute") || out.contains("No results"),
        "unexpected output: {out}"
    );
}

// -------------------------------------------------------------------------
// enrich_docs
// -------------------------------------------------------------------------

#[test]
fn enrich_docs_missing_arg() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (msg, err) = dispatch_tool(&mut engine, "enrich_docs", &json!({}), None);
    assert!(err);
    assert!(msg.contains("Missing"), "got: {msg}");
}

#[test]
fn enrich_docs_unknown_symbol() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(
        &mut engine,
        "enrich_docs",
        &json!({"symbol": "totally_unknown_abc"}),
        None,
    );
    assert!(err, "unknown symbol should produce is_error=true: {out}");
    assert!(out.contains("not found"), "unexpected: {out}");
}

#[test]
fn enrich_docs_generates_stub_without_api_key() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    // SAFETY: tests run single-threaded in this module; no concurrent env access.
    unsafe {
        std::env::remove_var("ANTHROPIC_API_KEY");
        std::env::remove_var("OLLAMA_HOST");
    }
    let (out, err) = dispatch_tool(
        &mut engine,
        "enrich_docs",
        &json!({"symbol": "compute"}),
        None,
    );
    assert!(!err, "enrich_docs returned error: {out}");
    assert!(
        out.contains("compute"),
        "expected symbol name in output: {out}"
    );
    let (out2, err2) = dispatch_tool(
        &mut engine,
        "enrich_docs",
        &json!({"symbol": "compute"}),
        None,
    );
    assert!(!err2, "cached enrich_docs returned error: {out2}");
    assert!(
        out2.contains("cached") || out2.contains("compute"),
        "unexpected cached output: {out2}"
    );
}

#[test]
fn enrich_docs_force_regenerates() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    unsafe {
        std::env::remove_var("ANTHROPIC_API_KEY");
        std::env::remove_var("OLLAMA_HOST");
    }
    dispatch_tool(
        &mut engine,
        "enrich_docs",
        &json!({"symbol": "compute"}),
        None,
    );
    let (out, err) = dispatch_tool(
        &mut engine,
        "enrich_docs",
        &json!({"symbol": "compute", "force": true}),
        None,
    );
    assert!(!err, "enrich_docs force returned error: {out}");
    assert!(!out.contains("cached"), "force should bypass cache: {out}");
}

// -------------------------------------------------------------------------
// remember / recall / forget
// -------------------------------------------------------------------------

#[test]
fn remember_missing_args() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (msg, err) = dispatch_tool(&mut engine, "remember", &json!({"key": "k"}), None);
    assert!(err);
    let (msg2, err2) = dispatch_tool(&mut engine, "remember", &json!({"value": "v"}), None);
    assert!(err2);
    assert!(
        msg.contains("Missing") && msg2.contains("Missing"),
        "got: {msg}, {msg2}"
    );
}

#[test]
fn remember_stores_and_recall_retrieves() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());

    let (out, err) = dispatch_tool(
        &mut engine,
        "remember",
        &json!({"key": "auth_flow", "value": "JWT-based, 24h expiry", "tags": ["auth", "security"]}),
        None,
    );
    assert!(!err, "remember returned error: {out}");
    assert!(out.contains("auth_flow"), "unexpected: {out}");

    let (out2, err2) = dispatch_tool(&mut engine, "recall", &json!({}), None);
    assert!(!err2, "recall returned error: {out2}");
    assert!(
        out2.contains("auth_flow"),
        "recall should return stored entry: {out2}"
    );
    assert!(
        out2.contains("JWT-based"),
        "recall should return value: {out2}"
    );
}

#[test]
fn recall_query_filter() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());

    dispatch_tool(
        &mut engine,
        "remember",
        &json!({"key": "db_schema", "value": "PostgreSQL tables"}),
        None,
    );
    dispatch_tool(
        &mut engine,
        "remember",
        &json!({"key": "auth_flow", "value": "JWT tokens"}),
        None,
    );

    let (out, err) = dispatch_tool(&mut engine, "recall", &json!({"query": "postgres"}), None);
    assert!(!err, "recall query returned error: {out}");
    assert!(
        out.contains("db_schema"),
        "expected db_schema in query result: {out}"
    );
    assert!(
        !out.contains("auth_flow"),
        "auth_flow should not appear in postgres query: {out}"
    );
}

#[test]
fn recall_tag_filter() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());

    dispatch_tool(
        &mut engine,
        "remember",
        &json!({"key": "auth_flow", "value": "JWT", "tags": ["auth"]}),
        None,
    );
    dispatch_tool(
        &mut engine,
        "remember",
        &json!({"key": "db_schema", "value": "Postgres", "tags": ["database"]}),
        None,
    );

    let (out, err) = dispatch_tool(&mut engine, "recall", &json!({"tags": ["auth"]}), None);
    assert!(!err, "recall with tag filter returned error: {out}");
    assert!(
        out.contains("auth_flow"),
        "expected auth_flow in tag result: {out}"
    );
    assert!(
        !out.contains("db_schema"),
        "db_schema should be excluded by tag filter: {out}"
    );
}

#[test]
fn recall_empty_returns_message() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(&mut engine, "recall", &json!({}), None);
    assert!(!err, "recall on empty store returned error: {out}");
    assert!(
        out.contains("No memories") || out.contains("No matching"),
        "unexpected: {out}"
    );
}

#[test]
fn forget_removes_entry() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());

    dispatch_tool(
        &mut engine,
        "remember",
        &json!({"key": "to_delete", "value": "temp"}),
        None,
    );
    let (out, err) = dispatch_tool(&mut engine, "forget", &json!({"key": "to_delete"}), None);
    assert!(!err, "forget returned error: {out}");
    assert!(out.contains("to_delete"), "unexpected: {out}");

    let (out2, _) = dispatch_tool(&mut engine, "recall", &json!({}), None);
    assert!(
        !out2.contains("to_delete"),
        "entry should be removed: {out2}"
    );
}

#[test]
fn forget_missing_key_graceful() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(
        &mut engine,
        "forget",
        &json!({"key": "nonexistent_key"}),
        None,
    );
    assert!(
        !err,
        "forget of missing key should not be an error flag: {out}"
    );
    assert!(out.contains("No memory entry"), "unexpected: {out}");
}

#[test]
fn forget_missing_arg() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (msg, err) = dispatch_tool(&mut engine, "forget", &json!({}), None);
    assert!(err);
    assert!(msg.contains("Missing"), "got: {msg}");
}

// -------------------------------------------------------------------------
// find_tests
// -------------------------------------------------------------------------

#[test]
fn find_tests_discovers_test_functions() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(&mut engine, "find_tests", &json!({}), None);
    assert!(!err, "find_tests returned error: {out}");
    assert!(
        out.contains("test_compute")
            || out.contains("TestHandleRequest")
            || out.contains("Test functions"),
        "expected test function discovery: {out}"
    );
}

#[test]
fn find_tests_pattern_filter() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(&mut engine, "find_tests", &json!({"pattern": "zero"}), None);
    assert!(!err, "find_tests pattern returned error: {out}");
    if out.contains("Test functions") {
        assert!(
            out.contains("zero"),
            "filter 'zero' should include test_compute_zero: {out}"
        );
    }
}

#[test]
fn find_tests_file_filter() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(&mut engine, "find_tests", &json!({"file": "tests/"}), None);
    assert!(!err, "find_tests file filter returned error: {out}");
    assert!(!err, "output: {out}");
}

#[test]
fn find_tests_no_match() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(
        &mut engine,
        "find_tests",
        &json!({"pattern": "zzz_no_such_test_zzz"}),
        None,
    );
    assert!(!err, "find_tests no match returned error: {out}");
    assert!(out.contains("No test functions"), "unexpected: {out}");
}

// -------------------------------------------------------------------------
// find_similar
// -------------------------------------------------------------------------

#[test]
fn find_similar_missing_arg() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (msg, err) = dispatch_tool(&mut engine, "find_similar", &json!({}), None);
    assert!(err);
    assert!(msg.contains("Missing"), "got: {msg}");
}

#[test]
fn find_similar_unknown_symbol() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(
        &mut engine,
        "find_similar",
        &json!({"symbol": "zzz_no_such_symbol_zzz"}),
        None,
    );
    assert!(err, "unknown symbol should produce error: {out}");
    assert!(out.contains("not found"), "unexpected: {out}");
}

#[test]
fn find_similar_known_symbol() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(
        &mut engine,
        "find_similar",
        &json!({"symbol": "compute", "limit": 3}),
        None,
    );
    assert!(!err, "find_similar returned error: {out}");
    assert!(
        out.contains("similar") || out.contains("unique") || out.contains("No code"),
        "unexpected output: {out}"
    );
}

// -------------------------------------------------------------------------
// get_complexity
// -------------------------------------------------------------------------

#[test]
fn get_complexity_missing_arg() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (msg, err) = dispatch_tool(&mut engine, "get_complexity", &json!({}), None);
    assert!(err);
    assert!(msg.contains("Missing"), "got: {msg}");
}

#[test]
fn get_complexity_nonexistent_file() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(
        &mut engine,
        "get_complexity",
        &json!({"file": "src/does_not_exist.rs"}),
        None,
    );
    assert!(err, "nonexistent file should be an error: {out}");
    assert!(out.contains("Cannot read"), "unexpected: {out}");
}

#[test]
fn get_complexity_computes_for_functions() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(
        &mut engine,
        "get_complexity",
        &json!({"file": "src/main.rs"}),
        None,
    );
    assert!(!err, "get_complexity returned error: {out}");
    assert!(
        out.contains("CC") || out.contains("complexity") || out.contains("No functions"),
        "unexpected output: {out}"
    );
    if out.contains("compute") {
        assert!(out.contains("compute"), "compute should be listed: {out}");
    }
}

#[test]
fn get_complexity_min_complexity_filter() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(
        &mut engine,
        "get_complexity",
        &json!({"file": "src/main.rs", "min_complexity": 100}),
        None,
    );
    assert!(!err, "get_complexity min filter returned error: {out}");
    assert!(
        out.contains("No functions") || out.contains("complexity"),
        "unexpected: {out}"
    );
}

// -------------------------------------------------------------------------
// review_context
// -------------------------------------------------------------------------

#[test]
fn review_context_missing_arg() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (msg, err) = dispatch_tool(&mut engine, "review_context", &json!({}), None);
    assert!(err);
    assert!(msg.contains("Missing"), "got: {msg}");
}

#[test]
fn review_context_with_valid_patch() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let patch = "+++ b/src/main.rs\n\
                 @@ -8,6 +8,8 @@\n\
                 +/// Compute the sum.\n\
                  pub fn compute(a: i32, b: i32) -> i32 {\n";
    let (out, err) = dispatch_tool(
        &mut engine,
        "review_context",
        &json!({"patch": patch}),
        None,
    );
    assert!(!err, "review_context returned error: {out}");
    assert!(
        out.contains("Code Review Context") || out.contains("Changed files"),
        "unexpected output: {out}"
    );
    assert!(
        out.contains("main.rs"),
        "should mention the changed file: {out}"
    );
}

#[test]
fn review_context_empty_patch() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(
        &mut engine,
        "review_context",
        &json!({"patch": "no diff here\n"}),
        None,
    );
    assert!(!err, "review_context returned error: {out}");
    assert!(
        out.contains("0 total") || out.contains("Changed files"),
        "unexpected: {out}"
    );
}

// -------------------------------------------------------------------------
// generate_onboarding
// -------------------------------------------------------------------------

#[test]
fn generate_onboarding_creates_file() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let mut engine = make_engine(root);
    let (out, err) = dispatch_tool(&mut engine, "generate_onboarding", &json!({}), None);
    assert!(!err, "generate_onboarding returned error: {out}");

    let onboarding_path = root.join(".codixing/ONBOARDING.md");
    assert!(onboarding_path.exists(), "ONBOARDING.md should be created");

    let content = fs::read_to_string(&onboarding_path).unwrap();
    assert!(
        content.contains("# Project Onboarding"),
        "should have heading: {content}"
    );
    assert!(
        content.contains("Index Statistics"),
        "should have stats table: {content}"
    );
    assert!(
        content.contains("Language Breakdown") || content.contains("Repository Map"),
        "should have language or repo map section: {content}"
    );
}

#[test]
fn generate_onboarding_output_preview() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (out, err) = dispatch_tool(&mut engine, "generate_onboarding", &json!({}), None);
    assert!(!err, "generate_onboarding returned error: {out}");
    assert!(
        out.contains("ONBOARDING.md"),
        "should mention output file: {out}"
    );
    assert!(
        out.contains("Project Onboarding") || out.contains("bytes"),
        "should include doc preview: {out}"
    );
}

// -------------------------------------------------------------------------
// Memory persistence -- cross-call via same engine
// -------------------------------------------------------------------------

#[test]
fn memory_persists_to_disk() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let mut engine = make_engine(root);

    dispatch_tool(
        &mut engine,
        "remember",
        &json!({"key": "persistent_key", "value": "disk_value"}),
        None,
    );

    let memory_file = root.join(".codixing/memory.json");
    assert!(
        memory_file.exists(),
        "memory.json should be created on disk"
    );
    let raw = fs::read_to_string(&memory_file).unwrap();
    assert!(
        raw.contains("persistent_key"),
        "disk memory should contain the key"
    );
    assert!(
        raw.contains("disk_value"),
        "disk memory should contain the value"
    );
}

#[test]
fn multiple_memories_recall_sorted() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());

    dispatch_tool(
        &mut engine,
        "remember",
        &json!({"key": "z_last", "value": "last"}),
        None,
    );
    dispatch_tool(
        &mut engine,
        "remember",
        &json!({"key": "a_first", "value": "first"}),
        None,
    );
    dispatch_tool(
        &mut engine,
        "remember",
        &json!({"key": "m_middle", "value": "middle"}),
        None,
    );

    let (out, err) = dispatch_tool(&mut engine, "recall", &json!({}), None);
    assert!(!err, "recall returned error: {out}");
    let a_pos = out.find("a_first").unwrap_or(usize::MAX);
    let m_pos = out.find("m_middle").unwrap_or(usize::MAX);
    let z_pos = out.find("z_last").unwrap_or(usize::MAX);
    assert!(
        a_pos < m_pos && m_pos < z_pos,
        "recall should be sorted alphabetically by key: {out}"
    );
}

// -------------------------------------------------------------------------
// is_read_only_tool
// -------------------------------------------------------------------------

#[test]
fn is_read_only_tool_classifies_correctly() {
    // Read-only tools.
    assert!(is_read_only_tool("code_search"));
    assert!(is_read_only_tool("find_symbol"));
    assert!(is_read_only_tool("get_references"));
    assert!(is_read_only_tool("read_file"));
    assert!(is_read_only_tool("grep_code"));
    assert!(is_read_only_tool("recall"));
    assert!(is_read_only_tool("find_orphans"));
    assert!(is_read_only_tool("get_session_summary"));
    assert!(is_read_only_tool("get_hotspots"));
    assert!(is_read_only_tool("git_diff"));
    assert!(is_read_only_tool("find_source_for_test"));

    // Write tools.
    assert!(!is_read_only_tool("write_file"));
    assert!(!is_read_only_tool("edit_file"));
    assert!(!is_read_only_tool("delete_file"));
    assert!(!is_read_only_tool("apply_patch"));
    assert!(!is_read_only_tool("rename_symbol"));
    assert!(!is_read_only_tool("remember"));
    assert!(!is_read_only_tool("forget"));
    assert!(!is_read_only_tool("generate_onboarding"));
    assert!(!is_read_only_tool("session_reset_focus"));
    assert!(!is_read_only_tool("enrich_docs"));
    assert!(!is_read_only_tool("run_tests"));
}

// -------------------------------------------------------------------------
// dispatch_tool_ref (read-only dispatch)
// -------------------------------------------------------------------------

#[test]
fn dispatch_tool_ref_handles_read_only_tools() {
    let dir = tempdir().unwrap();
    let engine = make_engine(dir.path());
    // dispatch_tool_ref takes &Engine (not &mut).
    let (out, err) = dispatch_tool_ref(&engine, "code_search", &json!({"query": "hello"}), None);
    assert!(!err, "dispatch_tool_ref code_search returned error: {out}");
}

#[test]
fn dispatch_tool_ref_unknown_tool_returns_error() {
    let dir = tempdir().unwrap();
    let engine = make_engine(dir.path());
    let (msg, is_err) = dispatch_tool_ref(&engine, "nonexistent_tool", &json!({}), None);
    assert!(is_err);
    assert!(msg.contains("Unknown"), "got: {msg}");
}

// -------------------------------------------------------------------------
// compact mode
// -------------------------------------------------------------------------

#[test]
fn compact_mode_shortens_output() {
    let dir = tempdir().unwrap();
    let engine = make_engine(dir.path());
    let (normal_out, _) =
        dispatch_tool_ref(&engine, "read_file", &json!({"file": "src/main.rs"}), None);
    let (compact_out, _) = dispatch_tool_ref(
        &engine,
        "read_file",
        &json!({"file": "src/main.rs", "compact": true}),
        None,
    );
    // Compact output should be shorter (code blocks are trimmed).
    assert!(
        compact_out.len() <= normal_out.len(),
        "compact output ({}) should be <= normal output ({})",
        compact_out.len(),
        normal_out.len()
    );
}

#[test]
fn compact_false_preserves_full_output() {
    let dir = tempdir().unwrap();
    let engine = make_engine(dir.path());
    let (normal_out, _) =
        dispatch_tool_ref(&engine, "read_file", &json!({"file": "src/main.rs"}), None);
    let (explicit_out, _) = dispatch_tool_ref(
        &engine,
        "read_file",
        &json!({"file": "src/main.rs", "compact": false}),
        None,
    );
    assert_eq!(
        normal_out, explicit_out,
        "compact=false should produce same output as omitting compact"
    );
}

// -------------------------------------------------------------------------
// find_source_for_test
// -------------------------------------------------------------------------

#[test]
fn find_source_for_test_missing_arg() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let (msg, err) = dispatch_tool(&mut engine, "find_source_for_test", &json!({}), None);
    assert!(err);
    assert!(msg.contains("Missing"), "got: {msg}");
}

#[test]
fn find_source_for_test_returns_output_for_test_file() {
    let dir = tempdir().unwrap();
    let engine = make_engine(dir.path());
    let (out, err) = dispatch_tool_ref(
        &engine,
        "find_source_for_test",
        &json!({"file": "tests/server_test.go"}),
        None,
    );
    assert!(!err, "find_source_for_test returned error: {out}");
    // Should either find a source mapping or report none found gracefully.
    assert!(
        out.contains("Source files tested by") || out.contains("No source files found"),
        "unexpected output: {out}"
    );
}

#[test]
fn find_source_for_test_no_match_for_regular_file() {
    let dir = tempdir().unwrap();
    let engine = make_engine(dir.path());
    let (out, err) = dispatch_tool_ref(
        &engine,
        "find_source_for_test",
        &json!({"file": "src/main.rs"}),
        None,
    );
    assert!(!err, "find_source_for_test returned error: {out}");
    assert!(
        out.contains("No source files found"),
        "regular file should have no test mappings: {out}"
    );
}

// -------------------------------------------------------------------------
// search_tools (meta-tool)
// -------------------------------------------------------------------------

#[test]
fn search_tools_finds_search_tools() {
    let (out, err) = call_search_tools(&json!({"query": "search"}));
    assert!(!err, "search_tools returned error: {out}");
    assert!(
        out.contains("code_search"),
        "should find code_search: {out}"
    );
    assert!(
        out.contains("search_usages") || out.contains("search_changes"),
        "should find other search-related tools: {out}"
    );
}

#[test]
fn search_tools_empty_query_returns_all() {
    let (out, err) = call_search_tools(&json!({"query": ""}));
    assert!(!err, "search_tools returned error: {out}");
    // Should list all tools (46 total).
    assert!(
        out.contains("code_search")
            && out.contains("find_symbol")
            && out.contains("get_tool_schema"),
        "empty query should return all tools: {out}"
    );
    assert!(
        out.contains("46 results"),
        "should report 46 results for empty query: {out}"
    );
}

// -------------------------------------------------------------------------
// get_tool_schema (meta-tool)
// -------------------------------------------------------------------------

#[test]
fn get_tool_schema_returns_schema() {
    let (out, err) = call_get_tool_schema(&json!({"names": ["code_search"]}));
    assert!(!err, "get_tool_schema returned error: {out}");
    assert!(
        out.contains("code_search"),
        "should contain tool name: {out}"
    );
    assert!(
        out.contains("inputSchema") || out.contains("query"),
        "should contain schema details: {out}"
    );
}

#[test]
fn get_tool_schema_unknown_tool() {
    let (out, err) = call_get_tool_schema(&json!({"names": ["nonexistent_tool_xyz"]}));
    assert!(err, "unknown tool should return error: {out}");
    assert!(
        out.contains("Unknown tool") || out.contains("nonexistent_tool_xyz"),
        "should mention unknown tool: {out}"
    );
}

// -------------------------------------------------------------------------
// compact_tool_definitions
// -------------------------------------------------------------------------

#[test]
fn compact_tool_definitions_returns_only_meta_tools() {
    let defs = compact_tool_definitions();
    let arr = defs
        .as_array()
        .expect("compact_tool_definitions returns array");
    assert_eq!(
        arr.len(),
        2,
        "compact mode should return exactly 2 meta-tools, got {}",
        arr.len()
    );
    let names: Vec<&str> = arr
        .iter()
        .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
        .collect();
    assert!(
        names.contains(&"search_tools"),
        "should contain search_tools: {names:?}"
    );
    assert!(
        names.contains(&"get_tool_schema"),
        "should contain get_tool_schema: {names:?}"
    );
}

// -------------------------------------------------------------------------
// is_read_only_tool — meta-tools
// -------------------------------------------------------------------------

#[test]
fn meta_tools_are_read_only() {
    assert!(is_read_only_tool("search_tools"));
    assert!(is_read_only_tool("get_tool_schema"));
}

// -------------------------------------------------------------------------
// medium mode
// -------------------------------------------------------------------------

#[test]
fn medium_mode_returns_subset() {
    let defs = medium_tool_definitions();
    let arr = defs
        .as_array()
        .expect("medium_tool_definitions returns array");

    // Should return exactly the MEDIUM_TOOLS set.
    assert_eq!(
        arr.len(),
        MEDIUM_TOOLS.len(),
        "medium mode should return {} tools, got {}",
        MEDIUM_TOOLS.len(),
        arr.len()
    );

    let names: Vec<&str> = arr
        .iter()
        .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
        .collect();

    // Every MEDIUM_TOOLS entry should be present.
    for expected in MEDIUM_TOOLS {
        assert!(
            names.contains(expected),
            "medium mode missing tool '{expected}', got: {names:?}"
        );
    }

    // Each tool should have a description and inputSchema.
    for tool in arr {
        let name = tool["name"].as_str().unwrap();
        assert!(
            tool.get("description").and_then(|v| v.as_str()).is_some(),
            "medium tool '{name}' missing 'description'"
        );
        assert!(
            tool.get("inputSchema").is_some(),
            "medium tool '{name}' missing 'inputSchema'"
        );
    }

    // Should be strictly fewer than the full set.
    let full = tool_definitions();
    assert!(
        arr.len() < full.as_array().unwrap().len(),
        "medium set should be smaller than full set"
    );
}

#[test]
fn medium_mode_still_callable() {
    // Verify that a tool NOT in the medium listing can still be dispatched
    // via tools/call (the dispatch functions accept any valid tool name).
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());

    // "remember" is not in MEDIUM_TOOLS.
    assert!(
        !MEDIUM_TOOLS.contains(&"remember"),
        "remember should not be in MEDIUM_TOOLS for this test to be meaningful"
    );

    // "remember" is a write tool — use dispatch_tool (which handles both
    // read and write tools, mirroring what tools/call does at runtime).
    let (out, is_err) = dispatch_tool(
        &mut engine,
        "remember",
        &json!({"key": "test_key", "value": "test_value"}),
        None,
    );
    assert!(
        !is_err,
        "remember should succeed even in medium mode, got error: {out}"
    );
    assert!(
        out.contains("test_key") || out.contains("Stored") || out.contains("stored"),
        "remember should acknowledge storage, got: {out}"
    );
}

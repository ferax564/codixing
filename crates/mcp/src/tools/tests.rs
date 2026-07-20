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
fn tool_definitions_returns_64_tools() {
    let defs = tool_definitions();
    let arr = defs.as_array().expect("tool_definitions returns array");
    assert_eq!(
        arr.len(),
        64,
        "expected exactly 64 tool definitions (60 + 4 meta-tools), got {}",
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
fn code_search_schema_exposes_context_and_staleness_controls() {
    let defs = tool_definitions();
    let code_search = defs
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "code_search")
        .expect("code_search schema");
    let properties = &code_search["inputSchema"]["properties"];
    assert!(
        properties.get("token_budget").is_some(),
        "code_search must advertise its final response budget"
    );
    assert!(
        properties.get("check_staleness").is_some(),
        "code_search must make the repository walk explicit"
    );
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
        "get_mcp_profile",
        "set_mcp_profile",
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

#[test]
fn list_files_includes_symbol_free_docs() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(
        root.join("README.md"),
        "# Project Notes\n\nThis document has no code symbols.\n",
    )
    .unwrap();
    let mut engine = make_engine(root);

    let (out, err) = dispatch_tool(
        &mut engine,
        "list_files",
        &json!({"pattern": "README.md"}),
        None,
    );
    assert!(!err, "list_files returned error: {out}");
    assert!(
        out.contains("README.md"),
        "symbol-free README.md should be listed: {out}"
    );
    assert!(out.contains("chunks"), "chunk count should be shown: {out}");
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

#[cfg(unix)]
#[test]
fn mutation_tools_and_rename_reject_symlink_escapes() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    let outside = dir.path().join("outside.rs");
    let dangling_target = dir.path().join("not-created.rs");
    let mut engine = make_engine(&root);
    fs::write(
        &outside,
        "pub fn compute() -> &'static str { \"secret\" }\n",
    )
    .unwrap();
    fs::remove_file(root.join("src/main.rs")).unwrap();
    symlink(&outside, root.join("src/main.rs")).unwrap();
    symlink(&dangling_target, root.join("src/dangling.rs")).unwrap();

    let (_, write_error) = files::call_write_file(
        &mut engine,
        &json!({"file": "src/main.rs", "content": "pub fn overwritten() {}\n"}),
    );
    assert!(write_error, "write_file must reject an outside symlink");

    let (_, edit_error) = files::call_edit_file(
        &mut engine,
        &json!({
            "file": "src/main.rs",
            "old_string": "compute",
            "new_string": "stolen"
        }),
    );
    assert!(edit_error, "edit_file must reject an outside symlink");

    let patch = "diff --git a/src/main.rs b/src/main.rs\n\
                 --- a/src/main.rs\n\
                 +++ b/src/main.rs\n\
                 @@ -1 +1 @@\n\
                 -pub fn compute() -> &'static str { \"secret\" }\n\
                 +pub fn patched() {}\n";
    let (_, patch_error) = files::call_apply_patch(&mut engine, &json!({"patch": patch}));
    assert!(patch_error, "apply_patch must reject an outside symlink");

    let (_, dangling_error) = files::call_write_file(
        &mut engine,
        &json!({"file": "src/dangling.rs", "content": "pub fn created() {}\n"}),
    );
    assert!(dangling_error, "write_file must reject a dangling symlink");

    let (_, rename_error) = analysis::call_rename_symbol(
        &mut engine,
        &json!({"old_name": "compute", "new_name": "renamed_outside"}),
    );
    assert!(
        !rename_error,
        "unsafe indexed paths should be skipped cleanly"
    );

    assert_eq!(
        fs::read_to_string(&outside).unwrap(),
        "pub fn compute() -> &'static str { \"secret\" }\n"
    );
    assert!(!dangling_target.exists());
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

#[test]
fn rename_symbol_dry_run_does_not_modify_files() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let mut engine = make_engine(root);

    let (out, err) = dispatch_tool(
        &mut engine,
        "rename_symbol",
        &json!({"old_name": "compute", "new_name": "calculate", "dry_run": true}),
        None,
    );
    assert!(!err, "rename_symbol dry_run returned error: {out}");
    assert!(out.contains("Dry run"), "unexpected output: {out}");
    assert!(out.contains("No files changed"), "unexpected output: {out}");

    let content = fs::read_to_string(root.join("src/main.rs")).unwrap();
    assert!(
        content.contains("compute"),
        "dry_run should not modify source: {content}"
    );
    assert!(
        !content.contains("calculate"),
        "dry_run should not write new name: {content}"
    );
}

// -------------------------------------------------------------------------
// graph
// -------------------------------------------------------------------------

#[test]
fn cross_imports_pattern_filters_alias_imports() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src/plugin-sdk")).unwrap();
    fs::create_dir_all(root.join("extensions/openai")).unwrap();
    fs::create_dir_all(root.join("extensions/noisy")).unwrap();
    fs::write(
        root.join("src/plugin-sdk/plugin-entry.ts"),
        "export function definePluginEntry() {}\n",
    )
    .unwrap();
    fs::write(
        root.join("src/plugin-sdk/runtime.ts"),
        "export function runtimeOnly() {}\n",
    )
    .unwrap();
    fs::write(
        root.join("extensions/openai/index.ts"),
        r#"import { definePluginEntry } from "openclaw/plugin-sdk/plugin-entry";
export const plugin = definePluginEntry();
"#,
    )
    .unwrap();
    fs::write(
        root.join("extensions/noisy/index.ts"),
        r#"import { runtimeOnly } from "openclaw/plugin-sdk/runtime";
export const value = runtimeOnly();
"#,
    )
    .unwrap();

    let mut cfg = IndexConfig::new(root);
    cfg.embedding.enabled = false;
    let mut engine = Engine::init(root, cfg).expect("engine init failed");

    let (out, err) = dispatch_tool(
        &mut engine,
        "cross_imports",
        &json!({
            "from": "extensions",
            "to": "src/plugin-sdk",
            "pattern": "import.*definePluginEntry.*from.*plugin-sdk"
        }),
        None,
    );

    assert!(!err, "cross_imports returned error: {out}");
    assert!(
        out.contains("extensions/openai/index.ts"),
        "expected matching plugin entry importer, got: {out}"
    );
    assert!(
        !out.contains("extensions/noisy/index.ts"),
        "pattern filter should remove unrelated plugin-sdk import: {out}"
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
fn get_complexity_rejects_traversal_and_absolute_paths() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    let outside = dir.path().join("outside.rs");
    fs::write(&outside, "fn outside() -> bool { true }\n").unwrap();
    let mut engine = make_engine(&root);

    for candidate in ["../outside.rs".to_string(), outside.display().to_string()] {
        let (out, err) = dispatch_tool(
            &mut engine,
            "get_complexity",
            &json!({"file": candidate}),
            None,
        );
        assert!(err, "outside path should be rejected: {out}");
        assert!(
            out.contains("outside the configured roots"),
            "unexpected: {out}"
        );
    }
}

#[cfg(unix)]
#[test]
fn get_complexity_rejects_symlink_outside_root() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    let outside = dir.path().join("outside.rs");
    fs::write(&outside, "fn outside() -> bool { true }\n").unwrap();
    let mut engine = make_engine(&root);
    symlink(&outside, root.join("src/escape.rs")).unwrap();

    let (out, err) = dispatch_tool(
        &mut engine,
        "get_complexity",
        &json!({"file": "src/escape.rs"}),
        None,
    );
    assert!(err, "outside symlink should be rejected: {out}");
    assert!(
        out.contains("outside the configured roots"),
        "unexpected: {out}"
    );
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
    assert!(is_read_only_tool("agent_context_pack"));

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
fn agent_context_pack_returns_stable_json_schema() {
    let dir = tempdir().unwrap();
    let engine = make_engine(dir.path());
    let (out, err) = dispatch_tool_ref(
        &engine,
        "agent_context_pack",
        &json!({
            "task": "change compute behavior",
            "mode": "edit",
            "token_budget": 3000,
            "changed_files": ["src/main.rs"],
            "branch": "codex/test-pack",
            "risk_level": "high"
        }),
        None,
    );
    assert!(!err, "agent_context_pack returned error: {out}");
    let pack: serde_json::Value =
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("bad JSON: {e}\n{out}"));
    assert_eq!(pack["schema_version"], 1);
    assert_eq!(pack["mode"], "edit");
    assert_eq!(pack["branch"], "codex/test-pack");
    assert!(
        pack["must_read"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["path"] == "src/main.rs"),
        "changed file should be represented in must_read: {out}"
    );
    assert!(
        pack["recommended_next_tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool.as_str().unwrap_or("").starts_with("change_impact(")),
        "edit mode should recommend change_impact: {out}"
    );
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

#[test]
fn final_envelope_uses_default_and_hard_maximum_budgets() {
    let huge = "\u{1f642} codixing context ".repeat(20_000);

    let default_limited = enforce_tool_output_budget(&huge, &json!({}));
    assert!(default_limited.contains("truncated"));
    assert!(
        codixing_core::formatter::count_tokens(&default_limited) <= DEFAULT_TOOL_TOKEN_BUDGET,
        "default envelope exceeded {} tokens",
        DEFAULT_TOOL_TOKEN_BUDGET
    );

    let hard_limited = enforce_tool_output_budget(&huge, &json!({"token_budget": 1_000_000}));
    assert!(hard_limited.contains("truncated"));
    assert_eq!(
        requested_tool_token_budget(&json!({"token_budget": 1_000_000})),
        MAX_TOOL_TOKEN_BUDGET
    );
    assert!(
        codixing_core::formatter::count_tokens(&hard_limited) <= MAX_TOOL_TOKEN_BUDGET,
        "hard envelope exceeded {} tokens",
        MAX_TOOL_TOKEN_BUDGET
    );
}

#[test]
fn final_envelope_preserves_a_valid_partial_json_preview() {
    let json_output = serde_json::to_string(&json!({
        "schema_version": 1,
        "items": (0..2_000)
            .map(|i| format!("large structured item {i} 🙂"))
            .collect::<Vec<_>>(),
    }))
    .unwrap();

    let bounded = enforce_tool_output_budget(&json_output, &json!({"token_budget": 53}));
    let parsed: serde_json::Value = serde_json::from_str(&bounded).unwrap();
    assert_eq!(parsed["truncated"], true);
    assert_eq!(parsed["partial"]["schema_version"], 1);
    assert!(parsed["partial"]["items"].is_array());
    assert!(
        parsed["partial"]["items"].as_array().unwrap().len() < 2_000,
        "preview should retain a bounded prefix instead of dropping all data"
    );
    assert!(codixing_core::formatter::count_tokens(&bounded) <= 53);
}

#[test]
fn final_envelope_keeps_nearly_all_of_a_slightly_oversized_array() {
    let value = json!({
        "schema_version": 1,
        "items": (0..200).map(|index| format!("item-{index:03}")).collect::<Vec<_>>(),
    });
    let output = serde_json::to_string(&value).unwrap();
    let full_tokens = codixing_core::formatter::count_tokens(&output);
    let budget = full_tokens.saturating_sub(5);
    let bounded = enforce_tool_output_budget(&output, &json!({"token_budget": budget}));
    let parsed: Value = serde_json::from_str(&bounded).unwrap();
    let retained = parsed["partial"]["items"].as_array().unwrap().len();

    assert!(
        retained >= 180,
        "slight overflow retained only {retained}/200 items"
    );
    assert!(codixing_core::formatter::count_tokens(&bounded) <= budget);
}

#[test]
fn final_envelope_handles_wide_scalar_objects_with_a_valid_exact_cap() {
    let fields: serde_json::Map<String, Value> = (0..2_000)
        .map(|index| (format!("field_{index:04}"), json!(index)))
        .collect();
    let output = Value::Object(fields).to_string();
    let budget = 211;
    let bounded = enforce_tool_output_budget(&output, &json!({"token_budget": budget}));
    let parsed: Value = serde_json::from_str(&bounded).unwrap();

    assert_eq!(parsed["truncated"], true);
    assert!(
        parsed["partial"]
            .as_object()
            .is_some_and(|fields| !fields.is_empty())
    );
    assert!(codixing_core::formatter::count_tokens(&bounded) <= budget);
}

#[test]
fn final_envelope_clamps_zero_budget_and_stays_valid_json() {
    let args = json!({"token_budget": 0});
    let bounded = enforce_tool_output_budget(
        &serde_json::to_string(&json!({"items": ["large payload".repeat(100)]})).unwrap(),
        &args,
    );

    assert_eq!(requested_tool_token_budget(&args), 1);
    serde_json::from_str::<serde_json::Value>(&bounded).unwrap();
    assert!(codixing_core::formatter::count_tokens(&bounded) <= 1);
}

#[test]
fn both_dispatch_paths_apply_exact_utf8_safe_envelope() {
    let dir = tempdir().unwrap();
    let mut engine = make_engine(dir.path());
    let args = json!({"query": "", "token_budget": 53});

    let (read_only_out, read_only_err) = dispatch_tool_ref(&engine, "search_tools", &args, None);
    assert!(!read_only_err, "read-only dispatch failed: {read_only_out}");
    assert!(read_only_out.contains("truncated"));
    assert!(
        codixing_core::formatter::count_tokens(&read_only_out) <= 53,
        "read-only dispatch exceeded budget: {} tokens",
        codixing_core::formatter::count_tokens(&read_only_out)
    );

    let (write_path_out, write_path_err) = dispatch_tool(&mut engine, "search_tools", &args, None);
    assert!(
        !write_path_err,
        "write dispatch path failed: {write_path_out}"
    );
    assert!(write_path_out.contains("truncated"));
    assert!(
        codixing_core::formatter::count_tokens(&write_path_out) <= 53,
        "write dispatch path exceeded budget: {} tokens",
        codixing_core::formatter::count_tokens(&write_path_out)
    );
}

#[test]
fn code_search_only_checks_staleness_when_requested() {
    let dir = tempdir().unwrap();
    let engine = make_engine(dir.path());
    for i in 0..12 {
        fs::write(
            dir.path().join(format!("new_{i}.rs")),
            format!("pub fn newly_added_{i}() {{}}\n"),
        )
        .unwrap();
    }

    let (default_out, default_err) = dispatch_tool_ref(
        &engine,
        "code_search",
        &json!({"query": "compute", "strategy": "instant"}),
        None,
    );
    assert!(!default_err, "default search failed: {default_out}");
    assert!(
        !default_out.contains("Index is stale"),
        "default search must skip the full repository staleness walk"
    );

    let (checked_out, checked_err) = dispatch_tool_ref(
        &engine,
        "code_search",
        &json!({
            "query": "compute",
            "strategy": "instant",
            "check_staleness": true
        }),
        None,
    );
    assert!(!checked_err, "checked search failed: {checked_out}");
    assert!(
        checked_out.contains("Index is stale"),
        "explicit staleness check should report newly added files: {checked_out}"
    );
}

#[test]
fn code_search_rejects_blank_and_oversized_queries() {
    let dir = tempdir().unwrap();
    let engine = make_engine(dir.path());

    let (blank_out, blank_err) =
        dispatch_tool_ref(&engine, "code_search", &json!({"query": "  \n\t  "}), None);
    assert!(blank_err);
    assert!(blank_out.contains("must not be blank"));

    let oversized = "x".repeat(1_025);
    let (oversized_out, oversized_err) =
        dispatch_tool_ref(&engine, "code_search", &json!({"query": oversized}), None);
    assert!(oversized_err);
    assert!(oversized_out.contains("too long"));
}

#[test]
fn read_only_ingress_bounds_numeric_text_and_array_arguments() {
    let mut array = vec![json!(false), json!({"unexpected": true})];
    array.extend((0..80).map(|i| json!(format!("src/file_{i}.rs"))));
    let unicode_query = "🙂".repeat(MAX_TOOL_INPUT_CHARS);
    assert!(unicode_query.len() > MAX_TOOL_INPUT_CHARS);

    let bounded = bounded_read_only_args(&json!({
        "query": unicode_query,
        "token_budget": u64::MAX,
        "limit": u64::MAX,
        "max_files": u64::MAX,
        "depth": u64::MAX,
        "context_lines": u64::MAX,
        "before_context": u64::MAX,
        "after_context": u64::MAX,
        "line_start": u64::MAX,
        "line_end": u64::MAX,
        "days": u64::MAX,
        "changed_files": array,
    }))
    .expect("valid bounded ingress");

    assert_eq!(bounded["token_budget"], MAX_TOOL_TOKEN_BUDGET);
    assert_eq!(bounded["limit"], MAX_TOOL_RESULT_COUNT);
    assert_eq!(bounded["max_files"], MAX_TOOL_RESULT_COUNT);
    assert_eq!(bounded["depth"], MAX_TOOL_TRAVERSAL_DEPTH);
    assert_eq!(bounded["context_lines"], MAX_TOOL_CONTEXT_LINES);
    assert_eq!(bounded["before_context"], MAX_TOOL_CONTEXT_LINES);
    assert_eq!(bounded["after_context"], MAX_TOOL_CONTEXT_LINES);
    assert_eq!(bounded["line_start"], MAX_TOOL_LINE_NUMBER);
    assert_eq!(bounded["line_end"], MAX_TOOL_LINE_NUMBER);
    assert_eq!(bounded["days"], MAX_TOOL_TIME_WINDOW_DAYS);
    let changed_files = bounded["changed_files"].as_array().unwrap();
    assert_eq!(changed_files.len(), MAX_TOOL_ARRAY_ITEMS);
    assert!(changed_files.iter().all(serde_json::Value::is_string));

    let oversized = "🙂".repeat(MAX_TOOL_INPUT_CHARS + 1);
    let err = bounded_read_only_args(&json!({"task": oversized})).unwrap_err();
    assert!(err.contains("maximum: 1024 characters"));

    let patch_at_limit = "p".repeat(MAX_TOOL_PATCH_CHARS);
    assert!(bounded_read_only_args(&json!({"patch": patch_at_limit})).is_ok());
    let oversized_patch = "p".repeat(MAX_TOOL_PATCH_CHARS + 1);
    assert!(bounded_read_only_args(&json!({"patch": oversized_patch})).is_err());
}

#[test]
fn generated_schemas_advertise_read_only_ingress_bounds() {
    fn properties<'a>(defs: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
        &defs
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap()["inputSchema"]["properties"]
    }

    let defs = tool_definitions();
    let search = properties(&defs, "code_search");
    assert_eq!(search["query"]["maxLength"], MAX_TOOL_INPUT_CHARS);
    assert_eq!(search["limit"]["maximum"], MAX_TOOL_RESULT_COUNT);
    assert_eq!(search["token_budget"]["maximum"], MAX_TOOL_TOKEN_BUDGET);

    let grep = properties(&defs, "grep_code");
    assert_eq!(grep["context_lines"]["maximum"], MAX_TOOL_CONTEXT_LINES);
    assert_eq!(grep["pattern"]["maxLength"], MAX_TOOL_INPUT_CHARS);

    let context_pack = properties(&defs, "agent_context_pack");
    assert_eq!(
        context_pack["changed_files"]["maxItems"],
        MAX_TOOL_ARRAY_ITEMS
    );
    assert_eq!(
        context_pack["changed_files"]["items"]["maxLength"],
        MAX_TOOL_INPUT_CHARS
    );

    let transitive = properties(&defs, "get_transitive_deps");
    assert_eq!(transitive["depth"]["maximum"], MAX_TOOL_TRAVERSAL_DEPTH);

    let read_file = properties(&defs, "read_file");
    assert_eq!(read_file["line_end"]["maximum"], MAX_TOOL_LINE_NUMBER);
}

#[test]
fn handler_request_builders_clamp_before_core_work() {
    let oversized = json!({"token_budget": u64::MAX, "limit": u64::MAX});
    let (context_budget, context_limit) = context::context_request_bounds(&oversized);
    assert_eq!(context_budget, MAX_TOOL_TOKEN_BUDGET);
    assert_eq!(context_limit, MAX_TOOL_RESULT_COUNT);
    assert_eq!(
        context::context_request_bounds(&json!({})).0,
        DEFAULT_TOOL_TOKEN_BUDGET
    );
    assert_eq!(
        requested_structured_tool_token_budget(&oversized),
        MAX_TOOL_TOKEN_BUDGET * 3 / 4
    );

    let repo_map = graph::repo_map_options(&oversized);
    assert_eq!(repo_map.token_budget, MAX_TOOL_TOKEN_BUDGET);
}

#[test]
fn read_file_and_core_range_are_safe_for_extreme_bounds() {
    let dir = tempdir().unwrap();
    let engine = make_engine(dir.path());
    fs::write(
        dir.path().join("src/main.rs"),
        "🙂 very large line\n".repeat(20_000),
    )
    .unwrap();

    let (out, err) = dispatch_tool_ref(
        &engine,
        "read_file",
        &json!({
            "file": "src/main.rs",
            "line_start": 0,
            "line_end": u64::MAX,
            "token_budget": u64::MAX,
        }),
        None,
    );
    assert!(!err, "extreme read_file bounds failed: {out}");
    assert!(codixing_core::formatter::count_tokens(&out) <= MAX_TOOL_TOKEN_BUDGET);

    let full = engine
        .read_file_range("src/main.rs", None, Some(u64::MAX))
        .unwrap()
        .unwrap();
    assert!(full.contains("very large line"));
    let reversed = engine
        .read_file_range("src/main.rs", Some(u64::MAX), Some(0))
        .unwrap()
        .unwrap();
    assert!(reversed.is_empty());
}

#[test]
fn large_agent_context_pack_remains_valid_json_under_default_and_hard_budgets() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    for i in 0..80 {
        fs::write(
            dir.path().join(format!("src/generated_{i}.rs")),
            format!("pub fn generated_{i}(value: usize) -> usize {{\n    value + {i}\n}}\n"),
        )
        .unwrap();
    }
    let engine = make_engine(dir.path());
    let changed_files: Vec<String> = (0..80).map(|i| format!("src/generated_{i}.rs")).collect();

    for args in [
        json!({
            "task": "understand and review every generated function and its dependencies",
            "mode": "review",
            "changed_files": changed_files,
            "compact": true,
        }),
        json!({
            "task": "understand and review every generated function and its dependencies",
            "mode": "review",
            "changed_files": (0..80).map(|i| format!("src/generated_{i}.rs")).collect::<Vec<_>>(),
            "token_budget": u64::MAX,
        }),
    ] {
        let requested_budget = requested_tool_token_budget(&args);
        let (out, err) = dispatch_tool_ref(&engine, "agent_context_pack", &args, None);
        assert!(!err, "agent_context_pack failed: {out}");
        let parsed: serde_json::Value = serde_json::from_str(&out)
            .unwrap_or_else(|e| panic!("agent context pack is invalid JSON: {e}\n{out}"));
        assert!(parsed.is_object());
        assert!(codixing_core::formatter::count_tokens(&out) <= requested_budget);
    }
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
fn search_tools_alias_blast_radius_finds_change_impact() {
    let (out, err) = call_search_tools(&json!({"query": "blast radius"}));
    assert!(!err, "search_tools returned error: {out}");
    assert!(
        out.contains("change_impact"),
        "alias 'blast radius' should map to change_impact: {out}"
    );
}

#[test]
fn search_tools_empty_query_returns_all() {
    let (out, err) = call_search_tools(&json!({"query": ""}));
    assert!(!err, "search_tools returned error: {out}");
    // Should list all tools (64 core + 6 federation + 1 deprecated list_projects = 71).
    assert!(
        out.contains("code_search")
            && out.contains("find_symbol")
            && out.contains("get_tool_schema")
            && out.contains("get_mcp_profile")
            && out.contains("set_mcp_profile"),
        "empty query should return all tools: {out}"
    );
    assert!(
        out.contains("71 results"),
        "should report 71 results for empty query: {out}"
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
// is_read_only_tool — meta-tools
// -------------------------------------------------------------------------

#[test]
fn meta_tools_are_read_only() {
    assert!(is_read_only_tool("search_tools"));
    assert!(is_read_only_tool("get_tool_schema"));
}

// -------------------------------------------------------------------------
// check_staleness suggestion routing
// -------------------------------------------------------------------------

#[test]
fn check_staleness_suggests_sync_index_tool_when_engine_holds_writer() {
    let dir = tempdir().unwrap();
    let engine = make_engine(dir.path());
    // Make the index stale: a file the index has never seen.
    fs::write(dir.path().join("src/late_arrival.rs"), "pub fn n() {}\n").unwrap();

    let (out, is_err) = analysis::call_check_staleness(&engine);

    assert!(!is_err);
    assert!(out.contains("STALE"), "expected stale report, got: {out}");
    assert!(
        out.contains("sync_index"),
        "a writer-holding server blocks `codixing sync` — it must point at its own \
         sync_index tool, got: {out}"
    );
}

#[test]
fn check_staleness_suggests_cli_sync_when_engine_read_only() {
    let dir = tempdir().unwrap();
    drop(make_engine(dir.path()));
    let engine = Engine::open_read_only(dir.path()).unwrap();
    fs::write(dir.path().join("src/late_arrival.rs"), "pub fn n() {}\n").unwrap();

    let (out, is_err) = analysis::call_check_staleness(&engine);

    assert!(!is_err);
    assert!(
        out.contains("codixing sync"),
        "a read-only server holds no writer lock — the CLI sync works and is the \
         right suggestion, got: {out}"
    );
}

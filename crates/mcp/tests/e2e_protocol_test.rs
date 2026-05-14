//! End-to-end MCP protocol tests.
//!
//! These tests spawn the actual `codixing-mcp` binary as a subprocess,
//! pipe JSON-RPC over stdin/stdout, and verify the protocol works end-to-end.

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Spawn `codixing-mcp` with the given CLI args, send JSON-RPC request lines
/// over stdin, close stdin, wait for the process to exit, and return all
/// parsed JSON response lines from stdout.
fn run_mcp(args: &[&str], requests: &[Value]) -> Vec<Value> {
    let bin = env!("CARGO_BIN_EXE_codixing-mcp");

    // Always disable daemon auto-fork in tests — we want direct mode
    // so the process exits cleanly when stdin is closed.
    let mut full_args: Vec<&str> = vec!["--no-daemon-fork"];
    full_args.extend_from_slice(args);

    let mut child = Command::new(bin)
        .args(&full_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn codixing-mcp");

    // Write all requests as newline-delimited JSON, then close stdin.
    {
        let stdin = child.stdin.as_mut().expect("failed to open stdin");
        for req in requests {
            serde_json::to_writer(&mut *stdin, req).expect("failed to write request");
            stdin.write_all(b"\n").expect("failed to write newline");
        }
        // stdin is dropped here, sending EOF to the child.
    }

    let output = child
        .wait_with_output()
        .expect("failed to wait for codixing-mcp");

    // Parse each non-empty stdout line as JSON.
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("invalid JSON in stdout: {e}\nline: {line}"))
        })
        .collect()
}

/// Create a temporary project directory with a `.codixing` index (BM25-only)
/// containing a single `lib.rs` file.
fn setup_indexed_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("failed to create tempdir");

    // Write a small Rust source file for the index to discover.
    let lib_rs = dir.path().join("lib.rs");
    std::fs::write(&lib_rs, "pub fn greet() -> &'static str { \"hi\" }\n")
        .expect("failed to write lib.rs");

    // Initialize a BM25-only index (no embeddings, no ONNX needed).
    let mut config = codixing_core::IndexConfig::new(dir.path());
    config.embedding = codixing_core::EmbeddingConfig {
        enabled: false,
        ..codixing_core::EmbeddingConfig::default()
    };
    codixing_core::Engine::init(dir.path(), config).expect("engine init should succeed");

    dir
}

/// Standard initialize request.
fn initialize_request(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": { "capabilities": {} }
    })
}

/// Standard tools/list request.
fn tools_list_request(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/list"
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn e2e_initialize_and_tools_list() {
    let project = setup_indexed_project();
    let root = project.path().to_str().unwrap();

    let responses = run_mcp(
        &["--root", root],
        &[initialize_request(1), tools_list_request(2)],
    );

    // We expect exactly 2 responses (one per request with an id).
    assert!(
        responses.len() >= 2,
        "expected at least 2 responses, got {}",
        responses.len()
    );

    // Response 1: initialize
    let init_resp = responses
        .iter()
        .find(|r| r["id"] == 1)
        .expect("missing initialize response");
    assert_eq!(
        init_resp["result"]["serverInfo"]["name"], "codixing",
        "serverInfo.name should be 'codixing'"
    );
    assert_eq!(
        init_resp["result"]["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION"),
        "serverInfo.version should match the codixing-mcp package version"
    );
    assert!(
        init_resp["result"]["protocolVersion"].is_string(),
        "protocolVersion should be present"
    );

    // Response 2: tools/list
    let list_resp = responses
        .iter()
        .find(|r| r["id"] == 2)
        .expect("missing tools/list response");
    let tools = list_resp["result"]["tools"]
        .as_array()
        .expect("tools should be an array");
    assert!(
        tools.len() >= 40,
        "full mode should list 40+ tools, got {}",
        tools.len()
    );

    // Spot-check a few well-known tools.
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"code_search"), "missing code_search tool");
    assert!(names.contains(&"find_symbol"), "missing find_symbol tool");
    assert!(names.contains(&"get_repo_map"), "missing get_repo_map tool");
    assert!(
        !names.contains(&"write_file"),
        "default reviewer profile should hide write_file"
    );
}

#[test]
fn e2e_editor_profile_lists_non_destructive_write_tools() {
    let project = setup_indexed_project();
    let root = project.path().to_str().unwrap();

    let responses = run_mcp(
        &["--root", root, "--profile", "editor"],
        &[tools_list_request(1)],
    );

    let list_resp = responses
        .iter()
        .find(|r| r["id"] == 1)
        .expect("missing tools/list response");
    let tools = list_resp["result"]["tools"]
        .as_array()
        .expect("tools should be an array");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"write_file"), "missing write_file");
    assert!(names.contains(&"edit_file"), "missing edit_file");
    assert!(
        !names.contains(&"delete_file"),
        "editor profile should hide destructive delete_file"
    );
}

#[test]
fn e2e_tools_call_code_search() {
    let project = setup_indexed_project();
    let root = project.path().to_str().unwrap();

    let responses = run_mcp(
        &["--root", root],
        &[
            initialize_request(1),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "code_search",
                    "arguments": { "query": "greet", "limit": 5 }
                }
            }),
        ],
    );

    // Find the tools/call response.
    let call_resp = responses
        .iter()
        .find(|r| r["id"] == 2)
        .expect("missing tools/call response");

    let result = &call_resp["result"];
    assert_eq!(result["isError"], false, "code_search should not error");

    let text = result["content"][0]["text"]
        .as_str()
        .expect("content text should be a string");
    assert!(
        text.contains("greet"),
        "search result should contain 'greet', got: {text}"
    );
}

#[test]
fn e2e_compact_flag_is_rejected() {
    // --compact was soft-deprecated in v0.33 (issue #67) and hard-removed
    // in v0.34. Passing it should now fail at argument parsing with a
    // clap error, not silently produce the full list like v0.33.
    let project = setup_indexed_project();
    let root = project.path().to_str().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_codixing-mcp"))
        .args(["--root", root, "--compact"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to spawn codixing-mcp");

    assert!(
        !output.status.success(),
        "--compact should be rejected in v0.34 (exit non-zero), \
         but codixing-mcp exited with status {:?}",
        output.status.code()
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--compact") || stderr.contains("unexpected argument"),
        "stderr should mention the unrecognized --compact flag, got: {stderr}"
    );
}

#[test]
fn e2e_medium_flag_is_rejected() {
    // --medium was removed in favor of always shipping the full tool set.
    // clap should reject it at argument parsing.
    let project = setup_indexed_project();
    let root = project.path().to_str().unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_codixing-mcp"))
        .args(["--root", root, "--medium", "--no-daemon-fork"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("failed to spawn codixing-mcp");

    assert!(
        !output.status.success(),
        "codixing-mcp should reject --medium; got exit code {:?}",
        output.status.code()
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--medium") || stderr.contains("unexpected argument"),
        "stderr should mention the unrecognized --medium flag, got: {stderr}"
    );
}

#[test]
fn e2e_progress_notifications() {
    let project = setup_indexed_project();
    let root = project.path().to_str().unwrap();

    let progress_token = "test-progress-42";

    let responses = run_mcp(
        &["--root", root],
        &[
            initialize_request(1),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "code_search",
                    "arguments": { "query": "greet", "limit": 5 },
                    "_meta": { "progressToken": progress_token }
                }
            }),
        ],
    );

    // The final response for our request must exist.
    let call_resp = responses
        .iter()
        .find(|r| r["id"] == 2)
        .expect("missing tools/call response");
    assert_eq!(
        call_resp["result"]["isError"], false,
        "code_search with progress token should not error"
    );

    // On a tiny project, progress notifications may not be emitted.
    // If they were, verify they carry the correct token.
    let progress_notifications: Vec<&Value> = responses
        .iter()
        .filter(|r| {
            r.get("method")
                .and_then(|m| m.as_str())
                .is_some_and(|m| m == "notifications/progress")
        })
        .collect();

    for notification in &progress_notifications {
        let token = notification
            .get("params")
            .and_then(|p| p.get("progressToken"))
            .and_then(|t| t.as_str());
        assert_eq!(
            token,
            Some(progress_token),
            "progress notification should carry the correct token"
        );
    }
}

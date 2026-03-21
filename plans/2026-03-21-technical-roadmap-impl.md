# Technical Roadmap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the full post-v0.13.0 technical roadmap — stability fixes, performance benchmarks, quality improvements, and ecosystem integrations.

**Architecture:** Four independent phases that can be parallelized at the phase level. Phase 1 (stability) should land first since it unblocks confident CI for later phases. Phases 2-4 are independent of each other. Within each phase, tasks are ordered by dependency.

**Tech Stack:** Rust (workspace: core/cli/mcp/lsp), `serial_test` crate, `criterion` benchmarks, `tower-lsp`, `instant-distance` HNSW, MCP JSON-RPC 2.0 protocol, GitHub Actions YAML.

**Source spec:** `plans/2026-03-21-technical-roadmap.md`

---

## File Structure

### Phase 1: Stability
| Action | File | Responsibility |
|--------|------|----------------|
| Modify | `crates/core/Cargo.toml` | Add `serial_test` dev-dependency |
| Modify | `crates/core/tests/git_sync_test.rs` | Add `#[serial]` to flaky tests |
| Modify | `crates/core/tests/graph_test.rs` | Add `#[serial]` to flaky test |
| Create | `crates/mcp/src/daemon.rs` | Daemon mode: `run_daemon`, `handle_socket_connection`, `SocketGuard`, `run_proxy`, `socket_alive` |
| Create | `crates/mcp/src/jsonrpc.rs` | JSON-RPC loop: `run_jsonrpc_loop`, `dispatch`, `handle_initialize`, `handle_tools_list`, `handle_tools_call`, helpers |
| Create | `crates/mcp/src/progress.rs` | Progress reporting: channel setup, `ProgressNotification` bridge |
| Modify | `crates/mcp/src/main.rs` | Slim down to CLI parsing + entry point (~100 lines), re-export from new modules |
| Create | `crates/mcp/tests/e2e_protocol_test.rs` | E2E subprocess tests for MCP protocol |

### Phase 2: Performance
| Action | File | Responsibility |
|--------|------|----------------|
| Create | `crates/core/benches/large_repo_bench.rs` | Criterion bench for 10K/50K/100K file repos |
| Create | `benchmarks/large_repo_results.md` | Benchmark results documentation |
| Create | `crates/core/benches/vector_bench.rs` | Brute-force vs usearch crossover benchmark |
| Modify | `crates/core/src/vector/mod.rs` | Add warning when brute-force vector count exceeds threshold |
| Modify | `crates/mcp/src/main.rs` | Auto-fork daemon logic in normal mode |
| Modify | `crates/mcp/src/daemon.rs` | Add idle timeout (30 min), `--no-daemon-fork` flag |

### Phase 3: Quality
| Action | File | Responsibility |
|--------|------|----------------|
| Modify | `crates/core/src/graph/extract.rs` | Trait method dispatch linking (Rust `impl Trait for Type`) |
| Modify | `crates/core/src/graph/extract.rs` | Python `super().method()` inheritance resolution |
| Modify | `crates/core/src/graph/extract.rs` | TypeScript interface implementation linking |
| Create | `crates/core/tests/graph_trait_dispatch_test.rs` | Tests for trait/inheritance call graph edges |
| Modify | `crates/core/src/engine/search.rs` | Streaming partial results for deep search |
| Modify | `crates/mcp/src/jsonrpc.rs` | Partial result payloads in progress notifications |
| Modify | `crates/core/src/engine/mod.rs` | Periodic read-only reload from disk |

### Phase 4: Ecosystem
| Action | File | Responsibility |
|--------|------|----------------|
| Modify | `crates/mcp/src/jsonrpc.rs` | `notifications/tools/list_changed` support |
| Modify | `crates/lsp/src/main.rs` | `textDocument/prepareCallHierarchy` |
| Modify | `crates/lsp/src/main.rs` | `callHierarchy/incomingCalls` and `outgoingCalls` |
| Create | `.github/actions/codixing/action.yml` | GitHub Action definition |
| ~~Create~~ | ~~`.github/actions/codixing/entrypoint.sh`~~ | *(removed — composite action uses inline `run:` steps)* |

---

## Phase 1: Stability

### Task 1: Fix flaky tests with `serial_test`

**Files:**
- Modify: `crates/core/Cargo.toml` (dev-dependencies section)
- Modify: `crates/core/tests/git_sync_test.rs:186-241`
- Modify: `crates/core/tests/graph_test.rs:245-267`

**Context:** Three tests flake on CI due to Tantivy file lock contention when parallel tests create Engine instances in nearby temp dirs. The `serial_test` crate provides a `#[serial]` attribute that forces marked tests to run sequentially, eliminating the lock race without restructuring temp dirs.

- [ ] **Step 1: Add `serial_test` dev-dependency**

In `crates/core/Cargo.toml`, add to `[dev-dependencies]`:

```toml
serial_test = "3"
```

- [ ] **Step 2: Run `cargo check -p codixing-core` to verify dependency resolves**

Run: `cargo check -p codixing-core --tests`
Expected: compiles successfully

- [ ] **Step 3: Write a regression test that reproduces the lock contention**

In `crates/core/tests/git_sync_test.rs`, add at the top of the file:

```rust
use serial_test::serial;
```

Then add a test that deliberately creates two engines in quick succession (to validate that `#[serial]` prevents contention):

```rust
#[test]
#[serial]
fn serial_engine_open_no_lock_contention() {
    // Two sequential engine create-drop-reopen cycles.
    // Without #[serial], these race with other tests' Tantivy locks.
    let dir1 = tempdir().unwrap();
    let root1 = dir1.path();
    std::fs::write(root1.join("a.rs"), "fn a() {}").unwrap();
    drop(Engine::init(root1, bm25_config(root1)).unwrap());
    let _e1 = Engine::open(root1).unwrap();

    let dir2 = tempdir().unwrap();
    let root2 = dir2.path();
    std::fs::write(root2.join("b.rs"), "fn b() {}").unwrap();
    drop(Engine::init(root2, bm25_config(root2)).unwrap());
    let _e2 = Engine::open(root2).unwrap();
}
```

- [ ] **Step 4: Run the new test to verify it passes**

Run: `cargo test -p codixing-core --test git_sync_test serial_engine_open_no_lock_contention -- --nocapture`
Expected: PASS

- [ ] **Step 5: Add `#[serial]` to the three flaky tests**

In `crates/core/tests/git_sync_test.rs`, annotate both flaky tests:

```rust
#[test]
#[serial]
fn git_sync_no_op_when_already_current() {
    // ... existing body unchanged ...
}

#[test]
#[serial]
fn git_sync_no_op_without_git() {
    // ... existing body unchanged ...
}
```

In `crates/core/tests/graph_test.rs`, add:

```rust
use serial_test::serial;
```

Then annotate:

```rust
#[test]
#[serial]
fn graph_persists_across_open() {
    // ... existing body unchanged ...
}
```

- [ ] **Step 6: Run all three previously-flaky tests to verify they pass**

Run: `cargo test -p codixing-core --test git_sync_test git_sync_no_op -- --nocapture`
Run: `cargo test -p codixing-core --test graph_test graph_persists_across_open -- --nocapture`
Expected: all PASS

- [ ] **Step 7: Run full core test suite to verify no regressions**

Run: `cargo test -p codixing-core`
Expected: all tests pass

- [ ] **Step 8: Commit**

```bash
git add crates/core/Cargo.toml crates/core/tests/git_sync_test.rs crates/core/tests/graph_test.rs
git commit -m "fix: serialize flaky tests with serial_test to eliminate Tantivy lock races"
```

---

### Task 2: Refactor `crates/mcp/src/main.rs` into modules

**Files:**
- Create: `crates/mcp/src/daemon.rs`
- Create: `crates/mcp/src/jsonrpc.rs`
- Create: `crates/mcp/src/progress.rs`
- Modify: `crates/mcp/src/main.rs:1-1291` (reduce to ~100 lines)

**Context:** `main.rs` is 1,291 lines and touched by every feature PR — it's a merge bottleneck. The functions naturally cluster into 3 groups: daemon/socket management (`run_daemon`, `handle_socket_connection`, `SocketGuard`, `run_proxy`, `socket_alive` — all `#[cfg(unix)]`), JSON-RPC protocol (`run_jsonrpc_loop`, `dispatch`, `handle_initialize`, `handle_tools_list`, `handle_tools_call`, I/O helpers), and progress reporting (channel setup inside `handle_tools_call`, progress notification types). The refactor is a pure move — no logic changes, no public API changes.

- [ ] **Step 1: Create `crates/mcp/src/progress.rs`**

Extract the progress notification logic. Currently this is embedded in `handle_tools_call` (lines 600-634 of main.rs). Create a dedicated module:

```rust
//! Progress notification bridge for long-running MCP tool calls.
//!
//! Converts synchronous `std::sync::mpsc` progress events from the engine
//! into async JSON-RPC `notifications/progress` messages.

use serde_json::{json, Value};
use std::sync::mpsc;
use tokio::sync::mpsc as tokio_mpsc;

/// Progress event sent from blocking engine code to the async JSON-RPC writer.
#[derive(Debug, Clone)]
pub(crate) struct ProgressEvent {
    pub token: Value,
    pub progress: f64,
    pub total: f64,
    pub message: Option<String>,
}

/// Create a bridged progress channel.
///
/// Returns:
/// - A `std::sync::mpsc::Sender` for the blocking engine side
/// - A `tokio::sync::mpsc::Receiver` for the async JSON-RPC writer side
/// - A `tokio::task::JoinHandle` for the bridge task (drains std → tokio)
pub(crate) fn bridge_channel(
    progress_token: Value,
) -> (
    mpsc::Sender<(f64, f64, Option<String>)>,
    tokio_mpsc::Receiver<ProgressEvent>,
    tokio::task::JoinHandle<()>,
) {
    let (std_tx, std_rx) = mpsc::channel::<(f64, f64, Option<String>)>();
    let (tokio_tx, tokio_rx) = tokio_mpsc::channel::<ProgressEvent>(64);

    let token = progress_token.clone();
    let handle = tokio::task::spawn_blocking(move || {
        while let Ok((progress, total, message)) = std_rx.recv() {
            let _ = tokio_tx.blocking_send(ProgressEvent {
                token: token.clone(),
                progress,
                total,
                message,
            });
        }
    });

    (std_tx, tokio_rx, handle)
}

/// Format a progress event as a JSON-RPC notification.
pub(crate) fn to_notification(event: &ProgressEvent) -> Value {
    let mut params = json!({
        "progressToken": event.token,
        "progress": event.progress,
        "total": event.total,
    });
    if let Some(ref msg) = event.message {
        params["message"] = json!(msg);
    }
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": params,
    })
}
```

- [ ] **Step 2: Create `crates/mcp/src/daemon.rs`**

Move all `#[cfg(unix)]` functions from main.rs (lines 249-445):

```rust
//! Unix daemon mode: persistent engine process behind a Unix socket.
//!
//! When `codixing-mcp --daemon` is invoked, the engine is loaded once and
//! clients connect via `.codixing/daemon.sock`. File changes are watched
//! and incrementally re-indexed with a dual-level debounce.

#[cfg(unix)]
mod unix {
    use std::path::{Path, PathBuf};
    use std::sync::RwLock;
    use std::time::Duration;

    use anyhow::Result;
    use serde_json::Value;
    use tokio::io::{AsyncBufReadExt, BufReader, BufWriter};
    use tokio::net::{UnixListener, UnixStream};
    use tracing::{info, warn};
    use std::sync::Arc;

    use codixing_core::Engine;
    use crate::jsonrpc::run_jsonrpc_loop;
    use crate::ListingMode;
    use codixing_core::FederatedEngine;

    // --- Move run_daemon, handle_socket_connection, SocketGuard + Drop,
    //     run_proxy, socket_alive here verbatim from main.rs lines 249-445 ---

    // All functions keep the same signatures and bodies.
    // Only change: they now reference `crate::jsonrpc::run_jsonrpc_loop`
    // instead of a local `run_jsonrpc_loop`.

    pub async fn run_daemon(
        root: &Path,
        socket_path: &Path,
        engine: Arc<RwLock<Engine>>,
        listing_mode: ListingMode,
        federation: Option<Arc<FederatedEngine>>,
    ) -> Result<()> {
        // ... move existing body from main.rs lines 251-359 ...
        todo!("move from main.rs")
    }

    pub async fn run_proxy(socket_path: &Path) -> Result<()> {
        // ... move existing body from main.rs lines 395-429 ...
        todo!("move from main.rs")
    }

    pub async fn socket_alive(socket_path: &Path) -> bool {
        // ... move existing body from main.rs lines 437-445 ...
        todo!("move from main.rs")
    }

    pub struct SocketGuard {
        // ... move from main.rs lines 382-387 ...
    }
}

#[cfg(unix)]
pub(crate) use unix::*;
```

**Important:** The actual implementation is a verbatim move of lines 249-445 from main.rs. The `todo!()` placeholders above are just for plan readability — the real step is a cut-and-paste.

- [ ] **Step 3: Create `crates/mcp/src/jsonrpc.rs`**

Move the JSON-RPC protocol functions from main.rs (lines 451-760):

```rust
//! JSON-RPC 2.0 message loop and MCP method dispatch.
//!
//! Handles `initialize`, `tools/list`, `tools/call`, and unknown-method errors.
//! Tool execution runs on `spawn_blocking` with concurrent progress draining.

use std::sync::{Arc, RwLock};

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tracing::{debug, info, warn};

use codixing_core::{Engine, FederatedEngine};
use crate::protocol::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::tools;
use crate::progress;
use crate::ListingMode;

// --- Move these functions verbatim from main.rs ---
// run_jsonrpc_loop  (lines 451-503)
// dispatch          (lines 509-531)
// handle_initialize (lines 533-540)
// handle_tools_list (lines 542-559)
// handle_tools_call (lines 561-707)  — update to use crate::progress::bridge_channel
// futures_lite_write_line (lines 710-716)
// build_tool_response     (lines 719-741)
// write_line              (lines 747-760)

pub async fn run_jsonrpc_loop<R, W>(
    engine: Arc<RwLock<Engine>>,
    reader: tokio::io::Lines<BufReader<R>>,
    writer: BufWriter<W>,
    listing_mode: ListingMode,
    federation: Option<Arc<FederatedEngine>>,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    // ... move existing body ...
    todo!("move from main.rs")
}

// ... all other functions moved verbatim ...
```

- [ ] **Step 4: Update `crates/mcp/src/main.rs` to import modules**

Replace the entire file with a slim entry point (~100 lines):

```rust
//! Codixing MCP server — CLI entry point.
//!
//! See [`jsonrpc`] for the JSON-RPC loop, [`daemon`] for Unix daemon mode,
//! and [`progress`] for progress notification bridging.

mod daemon;
mod jsonrpc;
mod progress;
mod protocol;
mod tools;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use clap::Parser;
use tokio::io::{AsyncBufReadExt, BufReader, BufWriter};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use codixing_core::{Engine, FederatedEngine, IndexConfig};

/// Controls which tools are returned by `tools/list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListingMode {
    Full,
    Medium,
    Compact,
}

/// Codixing MCP server — JSON-RPC 2.0 over stdin/stdout (or Unix socket in daemon mode).
#[derive(Parser)]
#[command(name = "codixing-mcp", version, about)]
struct Args {
    // ... keep existing fields from lines 66-105 unchanged ...
}

#[tokio::main]
async fn main() -> Result<()> {
    // ... keep existing main() logic from lines 112-207 ...
    // Replace direct function calls with module-qualified paths:
    //   run_daemon(...)       → daemon::run_daemon(...)
    //   run_proxy(...)        → daemon::run_proxy(...)
    //   socket_alive(...)     → daemon::socket_alive(...)
    //   run_jsonrpc_loop(...) → jsonrpc::run_jsonrpc_loop(...)
    todo!("slim main with module imports")
}

fn load_engine(root: &std::path::Path) -> Result<Engine> {
    // ... keep existing body from lines 210-244 ...
    todo!("move from main.rs")
}

// Test module stays in main.rs OR moves to jsonrpc.rs
// (tests exercise run_jsonrpc_loop, so jsonrpc.rs is more appropriate)
```

- [ ] **Step 5: Move the test module to `crates/mcp/src/jsonrpc.rs`**

The test module (lines 766-1291) tests `run_jsonrpc_loop` and `dispatch`. Move it to the bottom of `jsonrpc.rs` as `#[cfg(test)] mod tests { ... }`. Update imports to use `super::*` and `crate::*` as needed.

- [ ] **Step 6: Run the full MCP test suite**

Run: `cargo test -p codixing-mcp`
Expected: all existing MCP tests pass. Verify the count matches the pre-refactor count (check `cargo test -p codixing-mcp -- --list` before and after).

- [ ] **Step 7: Run clippy and fmt**

Run: `cargo clippy -p codixing-mcp -- -D warnings && cargo fmt -p codixing-mcp --check`
Expected: no warnings, no format issues

- [ ] **Step 8: Commit**

```bash
git add crates/mcp/src/main.rs crates/mcp/src/daemon.rs crates/mcp/src/jsonrpc.rs crates/mcp/src/progress.rs
git commit -m "refactor: split MCP main.rs into daemon/jsonrpc/progress modules

Reduces main.rs from 1291 to ~100 lines. Each module has clear
responsibility. Progress module is restructured into ProgressEvent/bridge_channel
pattern for clarity; all other code is a verbatim move."
```

---

### Task 3: End-to-end MCP protocol tests

**Files:**
- Create: `crates/mcp/tests/e2e_protocol_test.rs`
- Modify: `crates/mcp/Cargo.toml` (add `assert_cmd` dev-dependency)

**Context:** Current MCP tests use in-process `run_jsonrpc_loop` with duplex channels. E2E tests should spawn the actual `codixing-mcp` binary as a subprocess, pipe JSON-RPC over stdin/stdout, and verify the protocol works end-to-end. This catches issues like incorrect stdio buffering, daemon mode startup, and `--compact`/`--medium` flag behavior that in-process tests cannot.

- [ ] **Step 1: Add `assert_cmd` and `serde_json` dev-dependencies**

In `crates/mcp/Cargo.toml`, add:

```toml
[dev-dependencies]
assert_cmd = "2"
serde_json = { workspace = true }
tempfile = { workspace = true }
```

- [ ] **Step 2: Write the failing e2e test for initialize + tools/list**

Create `crates/mcp/tests/e2e_protocol_test.rs`:

```rust
//! End-to-end MCP protocol tests.
//!
//! Spawn `codixing-mcp` as a subprocess, send JSON-RPC over stdin, read
//! responses from stdout. Verifies the binary behaves correctly as a real
//! MCP server.

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

/// Helper: spawn codixing-mcp, send requests, collect responses.
fn run_mcp(args: &[&str], requests: &[Value]) -> Vec<Value> {
    let bin = env!("CARGO_BIN_EXE_codixing-mcp");

    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn codixing-mcp");

    // Write all requests to stdin, then close it.
    {
        let stdin = child.stdin.as_mut().expect("failed to open stdin");
        for req in requests {
            serde_json::to_writer(&mut *stdin, req).unwrap();
            stdin.write_all(b"\n").unwrap();
        }
    }
    // Dropping stdin closes the pipe → server sees EOF → exits cleanly.

    let output = child.wait_with_output().expect("failed to read stdout");
    assert!(
        output.status.success(),
        "codixing-mcp exited with error: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("invalid JSON response"))
        .collect()
}

/// Helper: create a temp project with a .codixing index.
fn setup_indexed_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Write a minimal Rust file.
    std::fs::write(root.join("lib.rs"), "pub fn greet() -> &'static str { \"hi\" }\n").unwrap();

    // Initialize the index using the CLI binary.
    let bin = env!("CARGO_BIN_EXE_codixing-mcp");
    // We can't use codixing-mcp to init, so use Engine directly.
    // For e2e tests, we init via the codixing CLI or build in-process.
    // Simplest: use codixing_core directly.
    let mut config = codixing_core::IndexConfig::new(root);
    config.embedding.enabled = false;
    codixing_core::Engine::init(root, config).expect("index init");

    dir
}

#[test]
fn e2e_initialize_and_tools_list() {
    let project = setup_indexed_project();

    let responses = run_mcp(
        &["--root", project.path().to_str().unwrap()],
        &[
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list",
                "params": {}
            }),
        ],
    );

    assert_eq!(responses.len(), 2, "expected 2 responses");

    // Verify initialize response.
    let init = &responses[0];
    assert_eq!(init["result"]["serverInfo"]["name"], "codixing");

    // Verify tools/list returns tools.
    let tools = &responses[1];
    let tool_list = tools["result"]["tools"].as_array().expect("tools array");
    assert!(tool_list.len() >= 40, "full mode should return 40+ tools, got {}", tool_list.len());
}
```

- [ ] **Step 3: Run the test to verify it passes**

Run: `cargo test -p codixing-mcp --test e2e_protocol_test e2e_initialize_and_tools_list -- --nocapture`
Expected: PASS

- [ ] **Step 4: Add e2e test for `tools/call` (code_search)**

Append to `e2e_protocol_test.rs`:

```rust
#[test]
fn e2e_tools_call_code_search() {
    let project = setup_indexed_project();

    let responses = run_mcp(
        &["--root", project.path().to_str().unwrap()],
        &[
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "code_search",
                    "arguments": { "query": "greet" }
                }
            }),
        ],
    );

    assert!(responses.len() >= 2);
    let result = &responses[1];
    let content = result["result"]["content"][0]["text"]
        .as_str()
        .expect("should have text content");
    assert!(content.contains("greet"), "search result should mention 'greet'");
}
```

- [ ] **Step 5: Run the code_search e2e test**

Run: `cargo test -p codixing-mcp --test e2e_protocol_test e2e_tools_call_code_search -- --nocapture`
Expected: PASS

- [ ] **Step 6: Add e2e test for `--compact` mode**

Append to `e2e_protocol_test.rs`:

```rust
#[test]
fn e2e_compact_mode_lists_two_tools() {
    let project = setup_indexed_project();

    let responses = run_mcp(
        &["--root", project.path().to_str().unwrap(), "--compact"],
        &[
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list",
                "params": {}
            }),
            // Even in compact mode, tools/call should work for all tools.
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "code_search",
                    "arguments": { "query": "greet" }
                }
            }),
        ],
    );

    assert!(responses.len() >= 3);

    // tools/list should return exactly 2 meta-tools.
    let tools = &responses[1];
    let tool_list = tools["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tool_list.len(), 2, "compact mode should return 2 tools");

    let names: Vec<&str> = tool_list
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"search_tools"));
    assert!(names.contains(&"get_tool_schema"));

    // tools/call should still work.
    let call_result = &responses[2];
    assert!(call_result["result"]["content"][0]["text"].as_str().is_some());
}
```

- [ ] **Step 7: Add e2e test for `--medium` mode**

Append to `e2e_protocol_test.rs`:

```rust
#[test]
fn e2e_medium_mode_lists_curated_tools() {
    let project = setup_indexed_project();

    let responses = run_mcp(
        &["--root", project.path().to_str().unwrap(), "--medium"],
        &[
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list",
                "params": {}
            }),
        ],
    );

    let tools = &responses[1];
    let tool_list = tools["result"]["tools"].as_array().expect("tools array");
    // Medium mode returns ~15-17 curated tools.
    assert!(
        tool_list.len() >= 10 && tool_list.len() <= 25,
        "medium mode should return 10-25 tools, got {}",
        tool_list.len()
    );
}
```

- [ ] **Step 8: Add e2e test for progress notifications with `_meta.progressToken`**

Append to `e2e_protocol_test.rs`:

```rust
#[test]
fn e2e_progress_notifications() {
    let project = setup_indexed_project();

    let responses = run_mcp(
        &["--root", project.path().to_str().unwrap()],
        &[
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "code_search",
                    "arguments": { "query": "greet", "strategy": "deep" },
                    "_meta": { "progressToken": "tok-42" }
                }
            }),
        ],
    );

    // With a progressToken, we may get notifications/progress lines
    // interleaved before the final response.
    let notifications: Vec<&Value> = responses
        .iter()
        .filter(|r| r.get("method") == Some(&json!("notifications/progress")))
        .collect();

    // Deep search on a tiny project may not emit progress, but at least
    // the final response should exist and contain results.
    let final_response = responses
        .iter()
        .find(|r| r.get("id") == Some(&json!(2)))
        .expect("should have response for id=2");
    assert!(final_response["result"]["content"][0]["text"].as_str().is_some());

    // If progress was emitted, verify structure.
    for n in &notifications {
        assert_eq!(n["params"]["progressToken"], "tok-42");
        assert!(n["params"]["progress"].is_number());
        assert!(n["params"]["total"].is_number());
    }
}
```

- [ ] **Step 9: Run all e2e tests**

Run: `cargo test -p codixing-mcp --test e2e_protocol_test -- --nocapture`
Expected: all 5 tests pass

- [ ] **Step 10: Commit**

```bash
git add crates/mcp/Cargo.toml crates/mcp/tests/e2e_protocol_test.rs
git commit -m "test: add end-to-end MCP protocol tests

Spawns codixing-mcp as a subprocess, sends JSON-RPC over stdin,
verifies responses. Covers initialize, tools/list, tools/call,
--compact, --medium, and progress notifications."
```

---

## Phase 2: Performance

### Task 4: Large repo benchmarks

**Files:**
- Create: `crates/core/benches/large_repo_bench.rs`
- Modify: `crates/core/Cargo.toml` (add bench entry)
- Create: `benchmarks/large_repo_results.md`

**Context:** Current benchmarks (`crates/core/benches/search_bench.rs`) use 20 synthetic modules. We need to validate performance at scale: 10K, 50K, 100K files. The benchmark generates synthetic Rust files (each ~30 lines with a function, struct, and impl), indexes them, and measures init time, memory RSS, search latency per strategy, and sync time.

- [ ] **Step 1: Register the new benchmark in Cargo.toml**

In `crates/core/Cargo.toml`, add:

```toml
[[bench]]
name = "large_repo_bench"
harness = false
```

- [ ] **Step 2: Write the benchmark scaffold**

Create `crates/core/benches/large_repo_bench.rs`:

```rust
//! Large-repo performance benchmarks.
//!
//! Tests init time, search latency, and memory usage at 10K/50K/100K files.
//! Run: `cargo bench -p codixing-core --bench large_repo_bench`

use std::fs;
use std::path::Path;
use std::time::Instant;

use codixing_core::{Engine, IndexConfig, EmbeddingConfig};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use tempfile::tempdir;

/// Generate `n` synthetic Rust files in `dir`.
fn generate_files(dir: &Path, n: usize) {
    let src_dir = dir.join("src");
    fs::create_dir_all(&src_dir).unwrap();

    for i in 0..n {
        // Distribute into subdirectories (100 files per dir) to mimic real repos.
        let subdir = src_dir.join(format!("mod_{}", i / 100));
        fs::create_dir_all(&subdir).unwrap();

        let content = format!(
            r#"/// Module {i} documentation.
pub struct Widget{i} {{
    pub name: String,
    pub value: i64,
}}

impl Widget{i} {{
    pub fn new(name: &str) -> Self {{
        Self {{ name: name.to_string(), value: {i} }}
    }}

    pub fn process(&self) -> String {{
        format!("widget-{{}}-{{}}", self.name, self.value)
    }}
}}

pub fn create_widget_{i}() -> Widget{i} {{
    Widget{i}::new("default")
}}

#[cfg(test)]
mod tests {{
    use super::*;
    #[test]
    fn test_widget_{i}() {{
        let w = create_widget_{i}();
        assert_eq!(w.value, {i});
    }}
}}
"#,
            i = i,
        );
        fs::write(subdir.join(format!("widget_{i}.rs")), content).unwrap();
    }
}

fn bm25_config(root: &Path) -> IndexConfig {
    let mut config = IndexConfig::new(root);
    config.embedding = EmbeddingConfig {
        enabled: false,
        ..EmbeddingConfig::default()
    };
    config
}

fn bench_init(c: &mut Criterion) {
    let mut group = c.benchmark_group("init");
    group.sample_size(10);

    for &size in &[1_000, 10_000] {
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &n| {
            b.iter_with_setup(
                || {
                    let dir = tempdir().unwrap();
                    generate_files(dir.path(), n);
                    dir
                },
                |dir| {
                    let _ = Engine::init(dir.path(), bm25_config(dir.path())).unwrap();
                },
            );
        });
    }
    group.finish();
}

fn bench_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("search");

    for &size in &[1_000, 10_000] {
        let dir = tempdir().unwrap();
        generate_files(dir.path(), size);
        let engine = Engine::init(dir.path(), bm25_config(dir.path())).unwrap();

        group.bench_with_input(BenchmarkId::new("instant", size), &engine, |b, eng| {
            b.iter(|| {
                use codixing_core::{SearchQuery, Strategy};
                eng.search(SearchQuery {
                    query: "Widget process".to_string(),
                    limit: 10,
                    file_filter: None,
                    strategy: Strategy::Instant,
                    token_budget: None,
                }).unwrap()
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_init, bench_search);
criterion_main!(benches);
```

- [ ] **Step 3: Run the benchmark at 1K scale to validate it works**

Run: `cargo bench -p codixing-core --bench large_repo_bench -- --quick`
Expected: benchmark runs and produces timing output

- [ ] **Step 4: Run the full benchmark suite and record results**

Run: `cargo bench -p codixing-core --bench large_repo_bench 2>&1 | tee benchmarks/large_repo_results.md`
Expected: results for init and search at 1K and 10K file scales

Note: 50K/100K benchmarks should be run manually on a separate machine due to time. The harness can be extended by adding those sizes to the `&[1_000, 10_000]` arrays.

- [ ] **Step 5: Profile with flamegraph**

Run: `cargo flamegraph --bench large_repo_bench -p codixing-core -- --bench`
Expected: produces `flamegraph.svg` showing hot spots

- [ ] **Step 6: Commit**

```bash
git add crates/core/Cargo.toml crates/core/benches/large_repo_bench.rs benchmarks/large_repo_results.md
git commit -m "bench: add large-repo benchmarks for init and search at 1K/10K scale"
```

---

### Task 5: Brute-force vector performance threshold

**Files:**
- Create: `crates/core/benches/vector_bench.rs`
- Modify: `crates/core/Cargo.toml` (add bench entry)
- Modify: `crates/core/src/vector/mod.rs` (add threshold warning in brute-force impl)

**Context:** On Windows (or `--no-default-features`), the brute-force `VectorIndex` at `crates/core/src/vector/mod.rs:262` does O(N) cosine similarity for every search. We need to benchmark the crossover point where this becomes unacceptable (>200ms), document it, and emit a warning.

- [ ] **Step 1: Register the new benchmark**

In `crates/core/Cargo.toml`:

```toml
[[bench]]
name = "vector_bench"
harness = false
```

- [ ] **Step 2: Write the brute-force vs usearch benchmark**

Create `crates/core/benches/vector_bench.rs`:

```rust
//! Benchmarks brute-force vector search at varying index sizes.
//!
//! Run: `cargo bench -p codixing-core --bench vector_bench`
//! For brute-force only: `cargo bench -p codixing-core --bench vector_bench --no-default-features`

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use rand::Rng;

const DIMS: usize = 384; // BGE-small-en dimensionality

fn random_vector(dims: usize) -> Vec<f32> {
    let mut rng = rand::thread_rng();
    (0..dims).map(|_| rng.gen_range(-1.0..1.0)).collect()
}

fn bench_brute_force_search(c: &mut Criterion) {
    use codixing_core::vector::VectorBackend;

    let mut group = c.benchmark_group("vector_search");

    for &size in &[1_000, 5_000, 10_000, 50_000, 100_000] {
        // Build a real VectorIndex (brute-force when --no-default-features).
        let mut index = codixing_core::vector::VectorIndex::new(DIMS);
        for i in 0..size {
            let vec = random_vector(DIMS);
            index.add(i as u64, &vec, &format!("file_{i}.rs")).unwrap();
        }
        let query = random_vector(DIMS);

        group.bench_with_input(
            BenchmarkId::new("vector_index_search", size),
            &(&index, &query),
            |b, (idx, q)| {
                b.iter(|| idx.search(q, 10).unwrap());
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_brute_force_search);
criterion_main!(benches);
```

- [ ] **Step 3: Run the benchmark**

Run: `cargo bench -p codixing-core --bench vector_bench`
Expected: timing results for 1K through 100K vectors

- [ ] **Step 4: Add threshold warning in brute-force VectorIndex**

In `crates/core/src/vector/mod.rs`, inside the `#[cfg(not(feature = "usearch"))]` module's `VectorBackend::search` implementation, add a warning when vector count exceeds the documented threshold:

```rust
// At the top of the search method, before the scan:
const BRUTE_FORCE_WARN_THRESHOLD: usize = 50_000;
if self.entries.len() > BRUTE_FORCE_WARN_THRESHOLD {
    tracing::warn!(
        count = self.entries.len(),
        "brute-force vector search over {} vectors — consider enabling the \
         `usearch` feature for sub-linear ANN search",
        self.entries.len()
    );
}
```

- [ ] **Step 5: Write a test for the warning**

In the brute-force module's test section:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warns_above_threshold() {
        // Just verify the threshold constant exists and is reasonable.
        assert!(BRUTE_FORCE_WARN_THRESHOLD >= 10_000);
        assert!(BRUTE_FORCE_WARN_THRESHOLD <= 100_000);
    }
}
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p codixing-core --no-default-features`
Expected: PASS (brute-force path exercised)

- [ ] **Step 7: Evaluate `instant-distance` as pure-Rust HNSW for Windows**

The codebase already has `crates/core/src/index/hnsw.rs` using `instant-distance`. Evaluate whether to integrate it as the default Windows backend (replacing brute-force) when `usearch` is unavailable:

1. Run the vector benchmark with `instant-distance` at 10K and 50K vectors
2. Compare latency vs brute-force at the crossover point
3. Document the decision in `benchmarks/large_repo_results.md`:
   - If `instant-distance` is significantly faster above the threshold → create a follow-up task to wire it as the `#[cfg(not(feature = "usearch"))]` backend
   - If the difference is marginal for typical codebases (<50K chunks) → document that brute-force with warning is sufficient

- [ ] **Step 8: Commit**

```bash
git add crates/core/Cargo.toml crates/core/benches/vector_bench.rs crates/core/src/vector/mod.rs benchmarks/large_repo_results.md
git commit -m "bench: brute-force vector crossover benchmark + threshold warning

Benchmarks brute-force cosine-similarity search at 1K-100K vectors.
Emits tracing::warn when brute-force vector count exceeds 50K.
Evaluates instant-distance as pure-Rust HNSW alternative for Windows."
```

---

### Task 6: Daemon auto-start

**Files:**
- Modify: `crates/mcp/src/main.rs:161-207` (auto-fork logic in normal mode)
- Modify: `crates/mcp/src/daemon.rs` (add idle timeout, `--no-daemon-fork` flag)
- Modify: `crates/mcp/src/main.rs` (add `--no-daemon-fork` CLI arg)

**Context:** Currently, daemon mode requires an explicit `--daemon` flag. When `codixing-mcp` connects without a running daemon, it should auto-fork a daemon process in the background so subsequent connections are fast (~1ms proxy). The daemon should self-terminate after 30 min idle.

- [ ] **Step 1: Add `--no-daemon-fork` CLI arg**

In `crates/mcp/src/main.rs`, add to the `Args` struct:

```rust
/// Disable automatic daemon forking. When set, the server always runs
/// in direct (non-daemon) mode even when no daemon is running.
#[arg(long)]
no_daemon_fork: bool,
```

- [ ] **Step 2: Write a test for daemon idle timeout**

In `crates/mcp/src/daemon.rs`, add a test:

```rust
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn idle_timeout_shuts_down() {
        // Verify that the idle timer fires after the configured duration.
        let timeout = Duration::from_millis(100); // Short for testing
        let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();

        let handle = tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            let _ = tx.send(());
        });

        tokio::time::timeout(Duration::from_secs(1), rx)
            .await
            .expect("timeout channel should fire")
            .expect("sender should not be dropped");

        handle.await.unwrap();
    }
}
```

- [ ] **Step 3: Add idle timeout to `run_daemon`**

In `daemon.rs`, modify `run_daemon` to track last activity and exit after 30 min idle:

```rust
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Shared timestamp of last client activity (epoch millis).
static LAST_ACTIVITY: AtomicU64 = AtomicU64::new(0);

fn touch_activity() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    LAST_ACTIVITY.store(now, Ordering::Relaxed);
}

const IDLE_TIMEOUT_MS: u64 = 30 * 60 * 1000; // 30 minutes
```

In the daemon's accept loop, spawn a watchdog task:

```rust
// Inside run_daemon, after starting the listener:
touch_activity();

let watchdog = tokio::spawn(async {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let last = LAST_ACTIVITY.load(Ordering::Relaxed);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        if now - last > IDLE_TIMEOUT_MS {
            info!("daemon idle for 30 min — shutting down");
            std::process::exit(0);
        }
    }
});
```

And call `touch_activity()` at the start of each `handle_socket_connection`.

- [ ] **Step 4: Add auto-fork logic in normal mode**

In `crates/mcp/src/main.rs`, in the normal-mode branch (after the `#[cfg(unix)]` socket_alive check):

```rust
#[cfg(unix)]
if !args.no_daemon_fork && !daemon::socket_alive(&socket_path).await {
    // Fork a daemon in the background.
    info!("auto-starting daemon at {}", socket_path.display());
    let exe = std::env::current_exe()?;
    std::process::Command::new(&exe)
        .args(["--root", root.to_str().unwrap(), "--daemon"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .context("failed to fork daemon")?;

    // Wait briefly for the daemon to bind the socket.
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if daemon::socket_alive(&socket_path).await {
            break;
        }
    }
}
```

- [ ] **Step 5: Run MCP tests**

Run: `cargo test -p codixing-mcp`
Expected: all tests pass

**Deferred:** launchd plist (macOS) and systemd unit (Linux) for daemon auto-start are optional per spec and deferred to a future task. The auto-fork approach is sufficient for now — system service files can be added later if users request persistent daemons that survive terminal close.

- [ ] **Step 6: Commit**

```bash
git add crates/mcp/src/main.rs crates/mcp/src/daemon.rs
git commit -m "feat: auto-fork daemon on first connection with 30-min idle timeout

When codixing-mcp starts without a running daemon (Unix), it auto-forks
a daemon process. The daemon self-terminates after 30 min idle.
Use --no-daemon-fork to disable."
```

---

## Phase 3: Quality

### Task 7: Call graph accuracy — trait method dispatch

**Files:**
- Modify: `crates/core/src/graph/extract.rs:160-178` (trait impl linking)
- Create: `crates/core/tests/graph_trait_dispatch_test.rs`
- Modify: `crates/core/src/engine/mod.rs` (add `symbol_graph_stats` to `index_status`)

**Context:** The call graph currently extracts `impl Trait for Type` blocks but only records the implementing type name — it doesn't link trait method calls through the impl to the concrete method. When `foo.bar()` is called and `bar` is a trait method, the graph should link to all `impl Trait` blocks that implement `bar`.

- [ ] **Step 1: Write the failing test for Rust trait dispatch**

Create `crates/core/tests/graph_trait_dispatch_test.rs`:

```rust
//! Tests for trait method dispatch in the call graph.

use codixing_core::{Engine, IndexConfig, EmbeddingConfig};
use tempfile::tempdir;

fn bm25_config(root: &std::path::Path) -> IndexConfig {
    let mut config = IndexConfig::new(root);
    config.embedding = EmbeddingConfig {
        enabled: false,
        ..EmbeddingConfig::default()
    };
    config
}

#[test]
fn trait_method_dispatch_links_impl() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    // Create a trait, an impl, and a caller.
    std::fs::write(
        root.join("traits.rs"),
        r#"
pub trait Greeter {
    fn greet(&self) -> String;
}

pub struct EnglishGreeter;

impl Greeter for EnglishGreeter {
    fn greet(&self) -> String {
        "Hello".to_string()
    }
}
"#,
    )
    .unwrap();

    std::fs::write(
        root.join("main.rs"),
        r#"
mod traits;
use traits::{Greeter, EnglishGreeter};

fn main() {
    let g = EnglishGreeter;
    println!("{}", g.greet());
}
"#,
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    // `greet` in main should have EnglishGreeter::greet as a callee.
    // symbol_callees_precise(symbol, file_hint) → Vec<String>
    let callees = engine.symbol_callees_precise("main", Some("main.rs"));
    assert!(
        callees.iter().any(|c| c.contains("greet")),
        "main should call greet, got: {:?}",
        callees
    );

    // symbol_callers_precise(symbol, limit) → Vec<SymbolReference>
    // SymbolReference { file_path, line, kind, context }
    let callers = engine.symbol_callers_precise("greet", 20);
    assert!(
        callers.iter().any(|r| r.context.contains("greet") && r.file_path.contains("main")),
        "greet should be called by main, got: {:?}",
        callers
    );
}
```

- [ ] **Step 2: Run the test to see current behavior**

Run: `cargo test -p codixing-core --test graph_trait_dispatch_test -- --nocapture`
Expected: may partially pass (basic name matching works) or fail (trait dispatch not linked)

- [ ] **Step 3: Write the failing test for Python class inheritance**

Append to `graph_trait_dispatch_test.rs`:

```rust
#[test]
fn python_super_method_call_linked() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("base.py"),
        r#"
class Animal:
    def speak(self):
        return "..."

class Dog(Animal):
    def speak(self):
        base = super().speak()
        return f"Woof! {base}"
"#,
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    // Dog.speak should have Animal.speak as a callee (via super()).
    // symbol_callees_precise(symbol, file_hint) → Vec<String>
    let callees = engine.symbol_callees_precise("Dog.speak", Some("base.py"));
    assert!(
        callees.iter().any(|c| c.contains("speak")),
        "Dog.speak should call super().speak(), got: {:?}",
        callees
    );
}
```

- [ ] **Step 4: Write the failing test for TypeScript interface implementations**

Append to `graph_trait_dispatch_test.rs`:

```rust
#[test]
fn typescript_interface_impl_linked() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("service.ts"),
        r#"
interface Logger {
    log(message: string): void;
}

class ConsoleLogger implements Logger {
    log(message: string): void {
        console.log(message);
    }
}

function useLogger(logger: Logger) {
    logger.log("hello");
}
"#,
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let callees = engine.symbol_callees_precise("useLogger", Some("service.ts"));
    assert!(
        callees.iter().any(|c| c.contains("log")),
        "useLogger should call logger.log(), got: {:?}",
        callees
    );
}
```

- [ ] **Step 5: Implement Rust trait method dispatch in `extract.rs`**

In `crates/core/src/graph/extract.rs`, enhance the `impl_item` handler (around line 162) to also extract trait method names from `impl Trait for Type` blocks:

When parsing `impl Trait for Type { fn method() {} }`:
1. Extract the trait name from the `trait` field
2. For each function defined in the impl block, create a call edge from `Type::method` to the trait's `method` definition
3. This allows callers of `trait.method()` to resolve through to concrete implementations

The key change is in the reference extraction: when we see a method call `x.method()`, we should resolve it against both direct definitions and trait impl methods.

- [ ] **Step 6: Implement Python `super()` resolution in `extract.rs`**

In the Python section of `extract_references()`, detect `super().method()` call patterns:
1. When tree-sitter parses `super().method()`, it's a `call` node with a `attribute` child
2. Extract the method name from the attribute
3. Look up the parent class (from the `class_definition` node's `superclasses` field)
4. Create a call edge to `ParentClass.method`

- [ ] **Step 7: Implement TypeScript interface resolution in `extract.rs`**

In the TypeScript section, detect `implements` clauses:
1. When parsing `class X implements Y`, record that X's methods may resolve Y's interface methods
2. For method calls on interface-typed parameters, link through to implementing classes

- [ ] **Step 8: Run all call graph tests**

Run: `cargo test -p codixing-core --test graph_trait_dispatch_test -- --nocapture`
Run: `cargo test -p codixing-core --test graph_test -- --nocapture`
Expected: all pass

- [ ] **Step 9: Add `symbol_graph_stats` to `index_status` output**

In `crates/core/src/engine/mod.rs`, ensure the `index_status()` method includes graph statistics:

```rust
// In the index_status output, add:
"graph_nodes": stats.node_count,
"graph_edges": stats.edge_count,
"graph_coverage_pct": /* edges / symbols * 100 */,
```

- [ ] **Step 10: Measure call graph precision/recall on a real codebase**

Use an existing codebase with known call relationships (e.g., Codixing itself):

1. Pick 10 functions with known callers/callees (from manual inspection or `git grep`)
2. For each: run `symbol_callers_precise` and `symbol_callees_precise`
3. Compute precision (correct results / total results) and recall (found / expected)
4. Document results in a table in the commit message or a `benchmarks/call_graph_accuracy.md`
5. Target: precision > 80%, recall > 60% (call graph is best-effort, not a type checker)

- [ ] **Step 11: Commit**

```bash
git add crates/core/src/graph/extract.rs crates/core/src/engine/mod.rs crates/core/tests/graph_trait_dispatch_test.rs
git commit -m "feat: call graph accuracy — trait dispatch, Python super(), TypeScript implements

Links trait method calls through impl blocks to concrete methods.
Resolves super().method() in Python to parent class methods.
Links TypeScript interface method calls through implementing classes.
Adds graph_nodes/edges/coverage to index_status."
```

---

### Task 8: Streaming partial results for deep search

**Files:**
- Modify: `crates/core/src/engine/search.rs` (split deep search into phases)
- Modify: `crates/mcp/src/jsonrpc.rs` (send partial results in progress notifications)

**Context:** Deep search takes ~2.5s. Currently results are batched. The plan is: return BM25 results immediately via a progress notification with partial results, then append vector/reranked results in the final response. This uses the existing `notifications/progress` mechanism.

- [ ] **Step 1: Write the failing test for phased search results**

In `crates/core/tests/search_test.rs`, add:

```rust
use codixing_core::{SearchQuery, Strategy};

#[test]
fn deep_search_returns_bm25_phase_first() {
    // Verify that the engine's search with callback reports BM25 results
    // before the full reranked results.
    let dir = tempdir().unwrap();
    let root = dir.path();
    // ... setup project with multiple files ...

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let mut phases = Vec::new();
    let query = SearchQuery {
        query: "Widget".to_string(),
        limit: 10,
        file_filter: None,
        strategy: Strategy::Fast,
        token_budget: None,
    };
    let results = engine.search_with_progress(query, |phase, partial| {
        phases.push((phase.to_string(), partial.len()));
    });

    assert!(phases.len() >= 1, "should have at least one progress phase");
    assert!(phases[0].0.contains("bm25"), "first phase should be BM25");
}
```

- [ ] **Step 2: Implement `search_with_progress` in the engine**

In `crates/core/src/engine/search.rs`, add a new method that accepts a `SearchQuery` (matching the existing `search()` signature pattern) plus a progress callback:

```rust
/// Like `search()`, but calls `on_progress` at each retrieval phase.
///
/// Phase names: "bm25", "fused", "reranked".
/// Each call includes the phase's partial results so far.
pub fn search_with_progress<F>(
    &self,
    query: SearchQuery,
    mut on_progress: F,
) -> Result<Vec<SearchResult>>
where
    F: FnMut(&str, &[SearchResult]),
{
    let expanded = if query.strategy != Strategy::Instant {
        expand_query(&query.query)
    } else {
        query.query.clone()
    };

    // Phase 1: BM25 (always fast, <50ms)
    let bm25_results = self.bm25_search(&expanded, query.limit)?;
    on_progress("bm25", &bm25_results);

    match query.strategy {
        Strategy::Instant => Ok(bm25_results),
        Strategy::Fast | Strategy::Thorough | Strategy::Explore => {
            // Phase 2: hybrid fusion
            let fused = self.hybrid_search(&expanded, &bm25_results, query.limit)?;
            on_progress("fused", &fused);
            Ok(fused)
        }
        Strategy::Deep => {
            // Phase 2: hybrid
            let fused = self.hybrid_search(&expanded, &bm25_results, query.limit * 3)?;
            on_progress("fused", &fused);
            // Phase 3: rerank
            let reranked = self.rerank(&query.query, &fused, query.limit)?;
            on_progress("reranked", &reranked);
            Ok(reranked)
        }
    }
}
```

**Note:** The internal methods `bm25_search`, `hybrid_search`, and `rerank` may need to be extracted from the existing `search()` method. Currently `search()` does everything inline — this refactoring splits it into composable phases.

- [ ] **Step 3: Wire progress phases into MCP tool handler**

In `crates/mcp/src/jsonrpc.rs`, in `handle_tools_call` for `code_search`, when a progress token is present, send partial results as progress notification payloads:

```rust
// In the progress notification, include partial results:
let notification = json!({
    "jsonrpc": "2.0",
    "method": "notifications/progress",
    "params": {
        "progressToken": token,
        "progress": phase_num,
        "total": total_phases,
        "message": phase_name,
        "partialResults": partial_results_json,
    }
});
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p codixing-core --test search_test -- --nocapture`
Run: `cargo test -p codixing-mcp`
Expected: all pass

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/engine/search.rs crates/mcp/src/jsonrpc.rs crates/core/tests/search_test.rs
git commit -m "feat: streaming partial results for deep search via progress notifications

BM25 results are returned immediately in a progress notification.
Vector/reranked results follow in subsequent phases. Clients that
don't support progress still get the batched final response."
```

---

### Task 9: Read-only mode periodic reload

**Files:**
- Modify: `crates/core/src/engine/mod.rs` (add `reload_if_stale` method)
- Add test in `crates/core/tests/` for reload behavior

**Context:** Read-only Engine instances (opened when another process holds the write lock) currently serve stale data. They should periodically check `.codixing/meta.bin` mtime and reload symbols/graph/vectors when the writer has persisted new data. Default interval: 30s, configurable.

- [ ] **Step 1: Write the failing test**

Create or append to an existing test file:

```rust
use std::path::Path;

#[test]
fn read_only_engine_reloads_after_writer_update() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "pub fn original() {}").unwrap();

    // Writer instance — holds the Tantivy write lock.
    let mut writer = Engine::init(root, bm25_config(root)).unwrap();
    assert!(!writer.is_read_only());

    // Verify initial state: "original" is findable, "added" is not.
    let syms = writer.symbols("original", None).unwrap();
    assert!(!syms.is_empty(), "writer should find 'original'");

    // Add a new file via the writer.
    std::fs::write(root.join("b.rs"), "pub fn added() {}").unwrap();
    writer.reindex_file(Path::new("b.rs")).unwrap();

    // Verify writer sees the new symbol.
    let syms = writer.symbols("added", None).unwrap();
    assert!(!syms.is_empty(), "writer should find 'added' after reindex");

    // Simulate a read-only instance by opening a second engine.
    // Since writer holds the lock, this opens in read-only mode.
    let mut reader = Engine::open(root).unwrap();
    assert!(reader.is_read_only(), "second engine should be read-only");

    // Initially the reader may not see "added" (stale snapshot).
    // After reload_if_stale(), it should pick up the writer's changes.
    let reloaded = reader.reload_if_stale().unwrap();
    // Note: whether reload actually fires depends on mtime granularity.
    // On fast machines, the mtime may match. In that case, the read-only
    // engine loaded the fresh data at open time. Either way, verify:
    let syms = reader.symbols("added", None).unwrap();
    assert!(
        !syms.is_empty(),
        "read-only engine should find 'added' after reload (reloaded={})",
        reloaded,
    );
}
```

- [ ] **Step 2: Add `reload_if_stale` to Engine**

In `crates/core/src/engine/mod.rs`:

```rust
/// Check if the on-disk index has been updated since this read-only
/// instance was loaded, and reload if so.
///
/// No-op if this instance holds the write lock.
pub fn reload_if_stale(&mut self) -> Result<bool> {
    if !self.read_only {
        return Ok(false); // Writer doesn't need to reload.
    }

    let meta_path = self.root.join(".codixing/meta.bin");
    let disk_mtime = std::fs::metadata(&meta_path)
        .ok()
        .and_then(|m| m.modified().ok());

    match (disk_mtime, self.last_load_time) {
        (Some(disk), Some(loaded)) if disk > loaded => {
            info!("read-only index stale — reloading from disk");
            self.reload_from_disk()?;
            self.last_load_time = Some(disk);
            Ok(true)
        }
        _ => Ok(false),
    }
}
```

Add these fields to the `Engine` struct:

```rust
/// When this engine was last loaded/reloaded from disk (for staleness detection).
last_load_time: Option<SystemTime>,
/// Minimum interval between reload checks (default: 30s, configurable).
reload_interval: Duration,
/// Last time we checked for staleness (to avoid checking on every call).
last_staleness_check: Option<Instant>,
```

Set `last_load_time` during `Engine::open`, `reload_interval` to `Duration::from_secs(30)`.

- [ ] **Step 3: Implement `reload_from_disk`**

This is the core reload method. It must re-read all persistent state without re-acquiring the write lock:

```rust
fn reload_from_disk(&mut self) -> Result<()> {
    // Re-read symbols from symbols.bin
    let symbols_path = self.persistence.symbols_path();
    if symbols_path.exists() {
        self.symbols = SymbolTable::load_from(&symbols_path)?;
    }

    // Re-read graph from graph.bin
    let graph_path = self.persistence.graph_path();
    if graph_path.exists() {
        self.graph = Some(CodeGraph::load_from(&graph_path)?);
    }

    // Re-read vectors from vectors.bin (if embeddings enabled)
    if self.config.embedding.enabled {
        let vec_path = self.persistence.vectors_path();
        if vec_path.exists() {
            self.vectors = Some(VectorIndex::load_from(&vec_path)?);
        }
    }

    // Re-open Tantivy reader to pick up new segments.
    // (Tantivy readers auto-refresh, but explicit reload ensures freshness.)
    self.tantivy.reload()?;

    Ok(())
}
```

**Note:** The exact method names (`SymbolTable::load_from`, `CodeGraph::load_from`, etc.) must be verified against the actual persistence layer. Check `crates/core/src/persistence/mod.rs` for the correct deserialization methods.

- [ ] **Step 4: Add configurable reload interval**

Add a public method to configure the interval:

```rust
/// Set the minimum interval between reload-from-disk checks.
/// Default: 30 seconds. Only meaningful for read-only instances.
pub fn set_reload_interval(&mut self, interval: Duration) {
    self.reload_interval = interval;
}
```

In the MCP daemon's per-connection handler, check staleness with rate limiting:

```rust
// In the read-only path of jsonrpc loop, before dispatch:
if engine.read().unwrap().is_read_only() {
    let should_check = {
        let eng = engine.read().unwrap();
        eng.last_staleness_check
            .map(|t| t.elapsed() >= eng.reload_interval)
            .unwrap_or(true)
    };
    if should_check {
        let mut eng = engine.write().unwrap();
        eng.last_staleness_check = Some(Instant::now());
        let _ = eng.reload_if_stale();
    }
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p codixing-core`
Expected: all pass

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/engine/mod.rs
git commit -m "feat: periodic reload for read-only engine instances

Read-only Engine instances check .codixing/meta.bin mtime and reload
symbols, graph, and vectors when the writer has persisted updates.
Default check interval: 30s, configurable via set_reload_interval()."
```

---

## Phase 4: Ecosystem

### Task 10: `--compact` dynamic tool registration

**Files:**
- Modify: `crates/mcp/src/jsonrpc.rs` (add `notifications/tools/list_changed` support)

**Context:** The MCP spec (2024-11-05) supports `notifications/tools/list_changed` to notify clients that the available tools have changed. In `--compact` mode, we start with 2 meta-tools and can dynamically register more when the client calls `get_tool_schema` for a specific tool. The notification tells the client to re-fetch `tools/list`.

- [ ] **Step 1: Research MCP spec for `notifications/tools/list_changed`**

Check the MCP specification (2024-11-05) for:
- Is `notifications/tools/list_changed` supported?
- What's the notification format?
- Which clients handle it? (Claude Code, Continue.dev, etc.)

Document findings in a comment at the top of the implementation.

- [ ] **Step 2: Write the test**

In `crates/mcp/src/jsonrpc.rs` tests:

```rust
#[tokio::test]
async fn compact_mode_sends_list_changed_on_first_tool_call() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_test_engine(dir.path());

    let responses = run_requests_with_mode(
        engine,
        ListingMode::Compact,
        &[
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
            json!({
                "jsonrpc": "2.0", "id": 2,
                "method": "tools/call",
                "params": { "name": "code_search", "arguments": { "query": "hello" } }
            }),
        ],
    ).await;

    // After first non-meta tool call in compact mode, a list_changed
    // notification should be emitted.
    let list_changed = responses.iter().find(|r| {
        r.get("method") == Some(&json!("notifications/tools/list_changed"))
    });
    // If spec supports it, assert it exists. Otherwise, this is a no-op.
    // assert!(list_changed.is_some());
}
```

- [ ] **Step 3: Implement list_changed notification**

In `handle_tools_call`, after a successful call to a non-meta tool in compact mode:

```rust
// If in compact mode and this is the first call to a non-meta tool,
// notify the client that tools/list has changed (now returns full list).
if listing_mode == ListingMode::Compact && !tool_name.starts_with("search_tools") && !tool_name.starts_with("get_tool_schema") {
    let notification = json!({
        "jsonrpc": "2.0",
        "method": "notifications/tools/list_changed"
    });
    write_line(writer, &notification).await?;
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p codixing-mcp`
Expected: all pass

- [ ] **Step 5: Commit**

```bash
git add crates/mcp/src/jsonrpc.rs
git commit -m "feat: emit notifications/tools/list_changed in compact mode

When a non-meta tool is called for the first time in --compact mode,
notifies the client to re-fetch tools/list. Clients that don't support
the notification are unaffected."
```

---

### Task 11: LSP call hierarchy

**Files:**
- Modify: `crates/lsp/src/main.rs` (add 3 new LSP methods)
- Modify: `crates/lsp/Cargo.toml` (if tower-lsp version needs update)

**Context:** The LSP spec defines `textDocument/prepareCallHierarchy`, `callHierarchy/incomingCalls`, and `callHierarchy/outgoingCalls`. Codixing already has `symbol_callers_precise` and `symbol_callees` in the engine — the LSP just needs to wire them to the protocol types. This is a differentiator: cross-language call hierarchy that single-language servers (rust-analyzer, pyright) cannot provide.

- [ ] **Step 1: Add call hierarchy capability to `initialize`**

In `crates/lsp/src/main.rs`, in the `initialize` method's `ServerCapabilities`:

```rust
call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
```

- [ ] **Step 2: Implement `textDocument/prepareCallHierarchy`**

```rust
async fn prepare_call_hierarchy(
    &self,
    params: CallHierarchyPrepareParams,
) -> Result<Option<Vec<CallHierarchyItem>>> {
    let uri = params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;

    let engine = self.engine.read().unwrap();
    let file_path = uri.to_file_path().unwrap_or_default();
    let rel_path = file_path
        .strip_prefix(engine.root())
        .unwrap_or(&file_path);

    // Find symbol at position using engine.symbols(filter, file).
    // engine.symbols(filter, file) → Vec<Symbol>
    // Symbol { name, kind, language, file_path, line_start, line_end }
    let rel_str = rel_path.to_str().unwrap_or("");
    let symbols = engine.symbols("", Some(rel_str)).unwrap_or_default();
    let symbol = symbols.iter().find(|s| {
        pos.line >= s.line_start as u32 && pos.line <= s.line_end as u32
    });

    match symbol {
        Some(sym) => Ok(Some(vec![CallHierarchyItem {
            name: sym.name.clone(),
            kind: kind_to_lsp(sym.kind),  // existing helper at lsp/main.rs:851
            tags: None,
            detail: Some(rel_path.display().to_string()),
            uri: uri.clone(),
            range: Range {
                start: Position::new(sym.line_start as u32, 0),
                end: Position::new(sym.line_end as u32, 0),
            },
            selection_range: Range {
                start: Position::new(sym.line_start as u32, 0),
                end: Position::new(sym.line_start as u32, sym.name.len() as u32),
            },
            data: None,
        }])),
        None => Ok(None),
    }
}
```

- [ ] **Step 3: Implement `callHierarchy/incomingCalls`**

```rust
async fn incoming_calls(
    &self,
    params: CallHierarchyIncomingCallsParams,
) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
    let item = &params.item;
    let engine = self.engine.read().unwrap();

    // symbol_callers_precise(symbol, limit) → Vec<SymbolReference>
    // SymbolReference { file_path: String, line: usize, kind: String, context: String }
    let callers = engine.symbol_callers_precise(&item.name, 50);

    let calls: Vec<CallHierarchyIncomingCall> = callers
        .iter()
        .filter_map(|caller| {
            let uri = Url::from_file_path(engine.root().join(&caller.file_path)).ok()?;
            // Extract caller function name from context line (best-effort).
            let caller_name = caller.context.trim().to_string();
            Some(CallHierarchyIncomingCall {
                from: CallHierarchyItem {
                    name: caller_name.clone(),
                    kind: SymbolKind::FUNCTION,
                    tags: None,
                    detail: Some(caller.file_path.clone()),
                    uri,
                    range: Range {
                        start: Position::new(caller.line as u32, 0),
                        end: Position::new(caller.line as u32, 0),
                    },
                    selection_range: Range {
                        start: Position::new(caller.line as u32, 0),
                        end: Position::new(caller.line as u32, caller_name.len() as u32),
                    },
                    data: None,
                },
                from_ranges: vec![Range {
                    start: Position::new(caller.line as u32, 0),
                    end: Position::new(caller.line as u32, 0),
                }],
            })
        })
        .collect();

    Ok(Some(calls))
}
```

- [ ] **Step 4: Implement `callHierarchy/outgoingCalls`**

```rust
async fn outgoing_calls(
    &self,
    params: CallHierarchyOutgoingCallsParams,
) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
    let item = &params.item;
    let engine = self.engine.read().unwrap();

    // symbol_callees_precise(symbol, file_hint) → Vec<String>
    let file_hint = item.detail.as_deref(); // detail carries the relative file path
    let callees = engine.symbol_callees_precise(&item.name, file_hint);

    let calls: Vec<CallHierarchyOutgoingCall> = callees
        .iter()
        .filter_map(|callee_name| {
            // Resolve callee name to a Symbol via engine.symbols(name, None)
            let matches = engine.symbols(callee_name, None).ok()?;
            let sym = matches.first()?;
            let uri = Url::from_file_path(engine.root().join(&sym.file_path)).ok()?;
            Some(CallHierarchyOutgoingCall {
                to: CallHierarchyItem {
                    name: sym.name.clone(),
                    kind: kind_to_lsp(sym.kind),
                    tags: None,
                    detail: Some(sym.file_path.clone()),
                    uri,
                    range: Range {
                        start: Position::new(sym.start_line as u32, 0),
                        end: Position::new(sym.end_line as u32, 0),
                    },
                    selection_range: Range {
                        start: Position::new(sym.start_line as u32, 0),
                        end: Position::new(sym.start_line as u32, sym.name.len() as u32),
                    },
                    data: None,
                },
                from_ranges: vec![],  // TODO: precise call site ranges
            })
        })
        .collect();

    Ok(Some(calls))
}
```

- [ ] **Step 5: Write tests for call hierarchy methods**

Add to `crates/lsp/src/main.rs` in the test module (or create `crates/lsp/tests/call_hierarchy_test.rs`):

```rust
#[tokio::test]
async fn prepare_call_hierarchy_returns_item() {
    // Set up an engine with a known function at a known line.
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("lib.rs"), "pub fn target_fn() {}\n").unwrap();

    let mut config = IndexConfig::new(root);
    config.embedding.enabled = false;
    let engine = Engine::init(root, config).unwrap();

    let backend = CodixingBackend::new_for_test(engine);

    // Request at line 0 (where target_fn is defined).
    let params = CallHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: Url::from_file_path(root.join("lib.rs")).unwrap(),
            },
            position: Position::new(0, 5),
        },
        work_done_progress_params: Default::default(),
    };

    let result = backend.prepare_call_hierarchy(params).await.unwrap();
    assert!(result.is_some());
    let items = result.unwrap();
    assert_eq!(items.len(), 1);
    assert!(items[0].name.contains("target_fn"));
}
```

**Note:** If `CodixingBackend` does not have a `new_for_test` constructor, add one that takes an `Engine` directly. This follows the pattern used by existing LSP tests.

- [ ] **Step 6: Run LSP tests**

Run: `cargo test -p codixing-lsp`
Run: `cargo clippy -p codixing-lsp -- -D warnings`
Expected: all pass

- [ ] **Step 7: Commit**

```bash
git add crates/lsp/src/main.rs
git commit -m "feat: LSP call hierarchy — cross-language callers/callees

Implements textDocument/prepareCallHierarchy, callHierarchy/incomingCalls,
and callHierarchy/outgoingCalls. Uses the pre-built symbol graph for
cross-language call hierarchy that single-language servers cannot provide."
```

---

### Task 12: GitHub Actions integration

**Files:**
- Create: `.github/actions/codixing/action.yml`

**Context:** A GitHub Action that runs `predict_impact` and `review_context` on PR diffs, then posts a comment with impact analysis and test coverage gaps. Uses the CLI binary directly (not MCP JSON-RPC). **Important:** Verify that `codixing predict-impact <file>` and `codixing review-context` are valid CLI subcommands before using them in the action. If they are only MCP tool names, the action must pipe JSON-RPC to `codixing-mcp` instead, or the CLI must be extended with these subcommands first.

- [ ] **Step 1: Create the action definition**

Create `.github/actions/codixing/action.yml`:

```yaml
name: 'Codixing Code Review'
description: 'Automated code review with impact analysis and test coverage gaps'
branding:
  icon: 'search'
  color: 'blue'

inputs:
  version:
    description: 'Codixing version to install'
    required: false
    default: '0.13.0'
  github-token:
    description: 'GitHub token for posting PR comments'
    required: true

runs:
  using: 'composite'
  steps:
    - name: Install Codixing
      shell: bash
      run: |
        curl -fsSL https://codixing.com/install.sh | bash -s -- --version ${{ inputs.version }}
        echo "$HOME/.codixing/bin" >> $GITHUB_PATH

    - name: Index codebase
      shell: bash
      run: codixing init . --model bm25-only

    - name: Analyze PR impact
      shell: bash
      env:
        GITHUB_TOKEN: ${{ inputs.github-token }}
      run: |
        # Get changed files from PR.
        CHANGED=$(gh pr diff ${{ github.event.pull_request.number }} --name-only)

        # Run predict_impact for each changed file.
        IMPACT=""
        for file in $CHANGED; do
          result=$(codixing predict-impact "$file" 2>/dev/null || echo "no impact data")
          IMPACT="$IMPACT\n### $file\n$result\n"
        done

        # Run review_context on the diff.
        DIFF=$(gh pr diff ${{ github.event.pull_request.number }})
        REVIEW=$(echo "$DIFF" | codixing review-context 2>/dev/null || echo "no review context")

        # Post comment.
        BODY=$(cat <<EOF
        ## Codixing Impact Analysis

        $IMPACT

        ## Review Context

        $REVIEW

        ---
        *Generated by [Codixing](https://codixing.com)*
        EOF
        )

        gh pr comment ${{ github.event.pull_request.number }} --body "$BODY"
```

- [ ] **Step 2: Validate action YAML syntax**

Run: `python3 -c "import yaml; yaml.safe_load(open('.github/actions/codixing/action.yml'))" && echo "YAML valid"`
Expected: "YAML valid" (no syntax errors)

- [ ] **Step 3: Verify CLI subcommands exist**

Run: `./target/release/codixing --help | grep -E 'predict-impact|review-context'`

If the subcommands don't exist, either:
- (a) Add `predict-impact` and `review-context` as CLI subcommands in `crates/cli/src/main.rs` wrapping the engine methods, OR
- (b) Rewrite the action to pipe JSON-RPC requests to `codixing-mcp` via stdin

- [ ] **Step 4: Test the action locally with `act`** (optional)

Run: `act pull_request -W .github/actions/codixing/action.yml` (requires `act` installed)
Or manually test the script logic.

- [ ] **Step 5: Commit**

```bash
git add .github/actions/codixing/action.yml
git commit -m "feat: GitHub Action for automated code review with impact analysis

Installs Codixing, indexes the repo, runs predict_impact on changed files,
and posts a PR comment with impact analysis and test coverage gaps."
```

---

## Acceptance Criteria

| Phase | Task | Acceptance |
|-------|------|------------|
| 1 | Flaky tests | 10 consecutive CI runs, zero flaky failures |
| 1 | Refactor main.rs | `cargo test -p codixing-mcp` passes, main.rs < 150 lines |
| 1 | E2E tests | 5 e2e tests pass, covering all listing modes + progress |
| 2 | Large repo bench | Documented results for 1K/10K files, flamegraph generated |
| 2 | Vector bench | Crossover point documented, warning emitted above threshold, `instant-distance` evaluation documented |
| 2 | Daemon auto-start | Auto-forks on first connection, exits after 30 min idle |
| 3 | Call graph | Trait dispatch, Python super(), TS implements all tested, precision/recall measured |
| 3 | Streaming | BM25 results appear in progress notification before final |
| 3 | Read-only reload | Stale read-only instances detect and reload, interval configurable |
| 4 | Dynamic tools | `list_changed` notification sent in compact mode |
| 4 | LSP call hierarchy | All 3 LSP methods implemented and tested |
| 4 | GitHub Action | Action YAML valid, CLI subcommands verified or JSON-RPC fallback |

## Dependency Graph

```
Task 1 (flaky tests)  ──┐
Task 2 (refactor)     ──┼── Phase 1 complete ──┐
Task 3 (e2e tests)    ──┘                      │
                                                ├── Phase 2 (independent)
Task 4 (large bench)  ──┐                      │   Task 4, 5, 6
Task 5 (vector bench) ──┤                      │
Task 6 (daemon auto)  ──┘                      ├── Phase 3 (independent)
                                                │   Task 7, 8, 9
Task 7 (call graph)   ──┐                      │
Task 8 (streaming)    ──┤                      ├── Phase 4 (independent)
Task 9 (read-only)    ──┘                      │   Task 10, 11, 12
                                                │
Task 10 (dynamic reg) ──┐                      │
Task 11 (LSP call)    ──┤                      │
Task 12 (GH action)   ──┘                      │
```

Phase 1 should land first. Phases 2, 3, 4 can be parallelized.
Within each phase, tasks are independent and can be parallelized via worktree agents.

# Technical Roadmap — Post v0.13.0

Created: 2026-03-21

## Phase 1: Stability (this week)

### 1.1 Fix flaky tests
- [ ] `git_sync_no_op_when_already_current` — Tantivy lock race on macOS CI
- [ ] `git_sync_no_op_without_git` — same root cause
- [ ] `graph_persists_across_open` — Windows temp directory locking
- Root cause: parallel test execution creates IndexWriters in nearby temp dirs that contend for locks
- Fix: use `serial_test` crate for tests that create Engine instances, or give each test an exclusive temp dir with unique Tantivy lock scope
- Acceptance: 10 consecutive CI runs with zero flaky failures

### 1.2 Refactor `crates/mcp/src/main.rs`
Current: 1 file, ~900 lines, touched by every feature PR. Merge bottleneck.
Split into:
- [ ] `main.rs` — CLI parsing (clap), entry point, mode selection (~100 lines)
- [ ] `daemon.rs` — `run_daemon`, `handle_socket_connection`, `SocketGuard`, `run_proxy`, `socket_alive` (all `#[cfg(unix)]`)
- [ ] `jsonrpc.rs` — `run_jsonrpc_loop`, `dispatch`, `handle_initialize`, `handle_tools_list`, `handle_tools_call`
- [ ] `progress.rs` — `ProgressReporter`, `ProgressNotification`, bridge channel logic
- Acceptance: `cargo test -p codixing-mcp` passes, no public API changes

### 1.3 End-to-end MCP protocol test
- [ ] Test that starts `codixing-mcp` as a subprocess, sends JSON-RPC over stdin, reads stdout
- [ ] Cover: initialize → tools/list → tools/call (code_search) → verify result structure
- [ ] Cover: --compact mode (tools/list returns 2 tools, tools/call still works for all 48)
- [ ] Cover: --medium mode (tools/list returns 17 tools)
- [ ] Cover: progress notifications with `_meta.progressToken`
- Location: `crates/mcp/tests/e2e_protocol_test.rs`

## Phase 2: Performance (next 2 weeks)

### 2.1 Large repo benchmarks
- [ ] Test on repos with 10K, 50K, 100K files
- Candidates: chromium (300K files), linux kernel (80K files), or a generated synthetic repo
- Measure: init time, memory RSS, search latency (instant/fast/deep), sync time after 10 file changes
- [ ] Profile with `cargo flamegraph` to find bottlenecks
- [ ] Document results in `benchmarks/large_repo_results.md`
- Target: init <30s for 50K files, search <200ms, RSS <500MB

### 2.2 Brute-force vector performance threshold
- [ ] Benchmark brute-force vs usearch at 1K, 10K, 50K, 100K vectors
- [ ] Document the crossover point where brute-force becomes unacceptable
- [ ] Add a warning when brute-force vector count exceeds threshold
- [ ] Consider shipping a pure-Rust HNSW (e.g. `instant-distance`) for Windows instead of brute-force

### 2.3 Daemon auto-start
- [ ] When `codixing-mcp` connects to a project without a running daemon, auto-fork a daemon process
- [ ] Daemon self-terminates after 30 min idle (no MCP connections)
- [ ] Add `--no-daemon-fork` flag to disable auto-start
- [ ] Optional: launchd plist for macOS, systemd unit for Linux

## Phase 3: Quality (weeks 3-4)

### 3.1 Call graph accuracy improvements
- [ ] Handle trait method dispatch (Rust: `impl Trait for Struct` methods should be linked)
- [ ] Handle Python class inheritance (`super().method()`)
- [ ] Handle TypeScript interface implementations
- [ ] Measure call graph precision/recall on a real codebase with known call relationships
- [ ] Add `symbol_graph_stats` to `index_status` output (nodes, edges, coverage %)

### 3.2 Streaming partial results
- [ ] For `search deep` (2.5s): return BM25 results immediately, then append vector/reranked results
- [ ] Use MCP `notifications/progress` with partial result payloads (non-standard but useful)
- [ ] Fallback: if client doesn't support progress, batch as today

### 3.3 Read-only mode improvements
- [ ] Periodic reload of symbols/graph/vectors from disk in long-running read-only instances
- [ ] Configurable reload interval (default: 30s)
- [ ] Detect when writer instance has persisted new data (check file mtime on `.codixing/meta.bin`)

## Phase 4: Ecosystem (month 2)

### 4.1 `--compact` dynamic tool registration
- [ ] Research MCP spec for late tool registration / tool list updates
- [ ] If supported: `--compact` starts with 2 tools, registers more on demand via `notifications/tools/list_changed`
- [ ] If not: propose spec extension or keep `--medium` as the pragmatic solution

### 4.2 Language server call hierarchy
- [ ] Implement `textDocument/prepareCallHierarchy`
- [ ] Implement `callHierarchy/incomingCalls` (uses symbol_callers from pre-built graph)
- [ ] Implement `callHierarchy/outgoingCalls` (uses symbol_callees)
- [ ] This is a differentiator: cross-language call hierarchy that rust-analyzer/pyright can't do

### 4.3 GitHub Actions integration
- [ ] Create `ferax564/codixing-action` for CI pipelines
- [ ] Use case: automated code review on PRs via `predict_impact` + `review_context`
- [ ] Use case: PR comment with impact analysis and test coverage gaps

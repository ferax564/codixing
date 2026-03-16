# Codixing — Claude Instructions

## Code Search & Navigation

**Always use Codixing MCP tools** instead of `grep`, `find`, `cat`, or `rg` for code exploration tasks:

| Instead of... | Use... |
|---|---|
| `grep -r "symbol"` | `mcp__codixing__search` |
| `cat file.rs` | `mcp__codixing__read_file` |
| `find . -name "*.rs"` | `mcp__codixing__list_files` |
| `grep -rn "fn foo"` to find a definition | `mcp__codixing__find_symbol` |
| Manual call-site hunting | `mcp__codixing__symbol_callers` |
| Manual dependency tracing | `mcp__codixing__callers` / `mcp__codixing__callees` |

For broad codebase exploration, always try a Codixing tool first. Fall back to Bash only if the tool doesn't cover the case.

### When to use which tool

- **Understanding a symbol** → `explain` (assembles definition + callers + callees in one call)
- **Finding where something is defined** → `find_symbol`
- **Searching by concept / natural language** → `search`
- **Listing files by glob** → `list_files`
- **Impact analysis before a change** → `predict_impact`
- **Seeing all callers of a function** → `symbol_callers`
- **Seeing what a function calls** → `symbol_callees`
- **Rename across codebase** → `rename_symbol`
- **Test coverage discovery** → `find_tests`
- **Finding code that a test covers** → `find_source_for_test`
- **Cyclomatic complexity** → `get_complexity`
- **Code review context** → `review_context`
- **Context-aware repo map** → `focus_map` (PPR seeded by recent edits)
- **Index freshness check** → `check_staleness`
- **Multi-agent session status** → `session_status`
- **Assembled context for a task** → `get_context_for_task`

## Project Structure

- `crates/core/` — engine: AST parsing, BM25, graph, embeddings, PageRank, test mapping, shared sessions
- `crates/cli/` — `codixing` CLI binary
- `crates/mcp/` — MCP server (`codixing-mcp`), 47 tools in `src/tools/` (use `--compact` for 96.7% token reduction)
- `crates/core/src/federation/` — cross-repo federated search (`--federation config.json`)
- `crates/lsp/` — LSP server (`codixing-lsp`), hover/go-to-def/refs/symbols/complexity diagnostics, file-local symbol ranking
- `crates/server/` — REST API server (`codixing-server`)
- `.codixing/` — index data (do not edit manually)

## Build & Test

```bash
cargo build --release --bin codixing-mcp   # build MCP server
cargo test --workspace                      # run all tests
cargo clippy --workspace -- -D warnings     # lint (must pass)
cargo fmt --check                           # format check (must pass)
```

## MCP Index Maintenance

The Codixing index lives in `.codixing/`. After significant file changes, sync it:

```bash
ORT_DYLIB_PATH=~/.local/lib/libonnxruntime.so LD_LIBRARY_PATH=~/.local/lib \
  ./target/release/codixing sync .
```

To rebuild from scratch (BgeSmallEn is the recommended model — fastest init, good retrieval):
```bash
ORT_DYLIB_PATH=~/.local/lib/libonnxruntime.so LD_LIBRARY_PATH=~/.local/lib \
  ./target/release/codixing init . --model bge-small-en
```

## Embedding Model Benchmark

**AMD Rembrandt CPU (86 files, 667 chunks):**

| Model        | Init time | Dims | Per-query overhead | Notes |
|---|---|---|---|---|
| BM25-only    | 0.2s      | —    | 13ms (cold)        | Best for exact keyword queries |
| **BgeSmallEn** | **76s** | 384  | ~1ms (daemon)      | **Recommended** — best speed/quality tradeoff |
| BgeBaseEn    | 192s      | 768  | ~1ms (daemon)      | 2.5× slower init, no quality gain on this codebase |
| Qwen3        | N/A       | 1024 | N/A                | Memory leak in candle — grows to 24GB RSS, OOM killed |

**Apple M4 (127 files, 1054 chunks):**

| Model        | Init time | Dims | Cold start (MCP) | Warm search | Notes |
|---|---|---|---|---|---|
| BM25-only    | 0.3s      | —    | ~115ms           | ~35ms       | No ONNX needed |
| **BgeSmallEn** | **110s** | 384  | ~107ms           | ~35ms       | **Recommended** — hybrid search shines on NL queries |

ONNX Runtime lives at `~/.local/lib/`:
- macOS: `~/.local/lib/libonnxruntime.dylib` (v1.24.3, installed via pip's `onnxruntime` package)
- Linux: `~/.local/lib/libonnxruntime.so` (v1.23.2+)

Set `ORT_DYLIB_PATH` (macOS) or `LD_LIBRARY_PATH` (Linux) for BgeSmallEn/BgeBaseEn to work.
The `.mcp.json` already sets this for the MCP server.

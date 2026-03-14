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
- **Cyclomatic complexity** → `get_complexity`
- **Code review context** → `review_context`

## Project Structure

- `crates/core/` — engine: AST parsing, BM25, graph, embeddings
- `crates/cli/` — `codixing` CLI binary
- `crates/mcp/` — MCP server (`codixing-mcp`), tools in `src/tools.rs`
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

## Embedding Model Benchmark (86 files, 667 chunks, AMD Rembrandt CPU)

| Model        | Init time | Dims | Per-query overhead | Notes |
|---|---|---|---|---|
| BM25-only    | 0.2s      | —    | 13ms (cold)        | Best for exact keyword queries |
| **BgeSmallEn** | **76s** | 384  | ~1ms (daemon)      | **Recommended** — best speed/quality tradeoff |
| BgeBaseEn    | 192s      | 768  | ~1ms (daemon)      | 2.5× slower init, no quality gain on this codebase |
| Qwen3        | N/A       | 1024 | N/A                | Memory leak in candle — grows to 24GB RSS, OOM killed |

ONNX Runtime 1.23.2 lives at `~/.local/lib/libonnxruntime.so`. Must be on `LD_LIBRARY_PATH` for
BgeSmallEn/BgeBaseEn to work. The `.mcp.json` already sets this for the MCP server.

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
- **Searching by concept / natural language** → `code_search` (auto-detects strategy; use `kind` param to filter by type)
- **Searching by symbol type** → `code_search` with `kind` param (`function`, `struct`, `enum`, `trait`, `impl`, `const`)
- **Discovering available tools** → `search_tools` (keyword search over tool names/descriptions)
- **Getting tool schemas** → `get_tool_schema` (lazy schema loading, used with `--compact`)
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
- `crates/mcp/` — MCP server (`codixing-mcp`), 48 tools in `src/tools/` (use `--compact` or `--medium` for token reduction)
- `crates/core/src/federation/` — cross-repo federated search (`--federation config.json`)
- `crates/lsp/` — LSP server (`codixing-lsp`), hover/go-to-def/refs/symbols/call hierarchy/complexity diagnostics
- `claude-plugin/` — Claude Code plugin with 3 skills + MCP server config
- `.codixing/` — index data (do not edit manually)

## Build & Test

```bash
cargo build --release --workspace          # build all binaries
cargo test --workspace                      # run all tests (678)
cargo clippy --workspace -- -D warnings     # lint (must pass)
cargo fmt --check                           # format check (must pass)

# Windows (no usearch):
cargo build --workspace --no-default-features
cargo test --workspace --no-default-features
```

## Version Locations

When bumping the version, update ALL of these files:

1. `Cargo.toml` — `workspace.package.version`
2. `npm/package.json` — `version`
3. `docs/install.sh` — `VERSION`
4. `claude-plugin/.claude-plugin/plugin.json` — `version`
5. `.claude-plugin/marketplace.json` — `metadata.version` AND `plugins[0].version`

## Development Workflow — Lessons Learned

### Parallel feature branches

When launching multiple feature branches in parallel (e.g. via worktree agents):

1. **Plan merge order upfront.** Identify which files are shared (especially `crates/mcp/src/main.rs` — it's a bottleneck) and decide merge order from smallest/most-independent to largest.
2. **Each PR must include its own docs updates.** Don't batch docs updates after all merges — update README, website, test counts, and CLAUDE.md as part of each feature PR.
3. **Merge one at a time, wait for CI.** After each squash-merge, pull main, verify CI passes on main, THEN rebase the next PR. Never merge multiple PRs without checking CI between them.
4. **Rebase conflicts in `main.rs` are common.** The MCP dispatch loop, `run_jsonrpc_loop`, and tool definitions are all in one file. When multiple PRs touch it, use a dedicated agent to rebase — don't attempt manual 3-way conflict resolution.

### CI checklist before merging

Before merging any PR:
- [ ] All CI checks green (macOS + Ubuntu + Windows if applicable)
- [ ] No flaky test failures in the log (re-run if `git_sync` or Tantivy lock tests fail)
- [ ] Review comments addressed and responded to
- [ ] Test count in README/website matches actual `cargo test` output
- [ ] Version in Cargo.toml matches what will be released

### Release checklist

Before tagging a release:
- [ ] All 5 version locations updated (see above)
- [ ] `cargo test --workspace` passes locally
- [ ] README Key Features section reflects new features
- [ ] docs/index.html test count and tool count are correct
- [ ] docs/docs.html has no stale references
- [ ] Plugin version matches in both `claude-plugin/` and `.claude-plugin/marketplace.json`

### Git history hygiene

When rewriting git history (e.g. before going public):
- **`git-filter-repo` ordering matters:** Do file path removals (`--path --invert-paths`) first, blob replacements (`--replace-text`) second, message rewrites (`--message-callback`) third, and mailmap (`--mailmap`) LAST. Each pass rewrites all commits, undoing previous mailmap changes.
- **Include ALL identity variants in one mailmap file.** Don't run mailmap multiple times — collect all `old → new` mappings in a single file.
- **Verify with `git log --all -p -S "string"`** after each pass. `-S` searches reachable blobs, not just HEAD.

### Known flaky tests

These tests previously flaked due to file locking. The first three are now serialized via `serial_test`:
- `git_sync_no_op_when_already_current` — fixed with `#[serial]`
- `git_sync_no_op_without_git` — fixed with `#[serial]`
- `graph_persists_across_open` — fixed with `#[serial]`
- Tier 2 retrieval tests — Windows `Access is denied` (marked `#[cfg_attr(windows, ignore)]`)

Re-run failed CI if Tier 2 retrieval tests are the only failures.

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

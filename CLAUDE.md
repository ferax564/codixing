# Codixing — Claude Instructions

## Code Search & Navigation

**MANDATORY: Always use Codixing MCP tools** instead of `grep`, `find`, `cat`, or `rg` for code exploration tasks. This applies to ALL agents, including subagents dispatched for implementation, review, or exploration. Include this instruction when dispatching any subagent:

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
- `crates/mcp/` — MCP server (`codixing-mcp`), 53 tools in `src/tools/` (use `--compact` or `--medium` for token reduction)
- `crates/server/` — HTTP API server (`codixing-server`), REST endpoints with SSE streaming for sync
- `crates/core/src/federation/` — cross-repo federated search (`--federation config.json`)
- `crates/lsp/` — LSP server (`codixing-lsp`), hover/go-to-def/refs/symbols/call hierarchy/complexity diagnostics/rename/semantic tokens
- `claude-plugin/` — Claude Code plugin with 3 skills + MCP server config
- `.codixing/` — index data (do not edit manually)

## Build & Test

```bash
cargo build --release --workspace          # build all binaries
cargo test --workspace                      # run all tests (787+)
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

## Development Workflow — Quality Rules

### Mandatory verification before every commit

Every commit MUST pass all 3 checks. No exceptions:

```bash
cargo test --workspace                      # ALL tests must pass
cargo clippy --workspace -- -D warnings     # zero warnings
cargo fmt --check                           # zero diffs
```

Subagents and worktree agents MUST run these checks before committing. If any check fails, fix the issue before committing — never skip.

### Documentation is part of the feature

Every feature commit MUST include documentation updates:
- Update test count in README.md, CLAUDE.md, and docs/index.html
- Update feature descriptions in README.md Key Features if applicable
- Update CLAUDE.md if the change affects project structure, tools, or capabilities
- Update docs/docs.html if LSP or MCP capabilities change

Never batch documentation updates after implementation — document as you go.

### Subagent rules

When dispatching subagents (implementation, review, or any task):

1. **Always use Codixing MCP tools for code exploration.** Subagents MUST use `mcp__codixing__code_search`, `mcp__codixing__find_symbol`, `mcp__codixing__read_file`, etc. instead of `grep`, `cat`, `find`. Include this instruction in every subagent prompt.
2. **Always spec-review every task.** No task is "too simple" to skip review.
3. **Always run the full verification triad** (test + clippy + fmt) before committing.
4. **Never run background tests against the main repo** while working in a worktree. Always run tests from the worktree directory.
5. **Never put generated files in `docs/`** — the `docs/` directory is served by Jekyll for GitHub Pages. Markdown with code blocks containing `{{ }}` will break the build. Plans, internal docs, and generated files go in `plans/` or the repo root.

### Plan quality

Implementation plans MUST use actual API signatures, not guesses:
- `grep` for real method signatures before writing plan code snippets
- Verify struct field names against the actual source
- When a plan reviewer finds API mismatches, fix ALL of them — not just the ones flagged

### Parallel feature branches

When launching multiple feature branches in parallel (e.g. via worktree agents):

1. **Plan merge order upfront.** Identify which files are shared and decide merge order from smallest/most-independent to largest.
2. **Each PR must include its own docs updates.** Update README, website, test counts, and CLAUDE.md as part of each feature PR.
3. **Merge one at a time, wait for CI.** After each squash-merge, pull main, verify CI passes on main (including the Jekyll/Pages build), THEN rebase the next PR.
4. **Check for behavioral interactions.** When planning features that change binary behavior (e.g., daemon auto-fork), explicitly note impacts on existing tests that spawn the binary as a subprocess.

### CI jobs

The CI workflow (`.github/workflows/ci.yml`) has the following jobs:

- **test** — builds and tests on Ubuntu, macOS, and Windows (matrix); runs clippy and fmt check
- **audit** — runs `cargo-audit` on Ubuntu only; `continue-on-error: true` (non-blocking while advisories are triaged)
- **coverage** — runs `cargo-llvm-cov` on Ubuntu only; uploads `lcov.info` as the `coverage-report` artifact
- **benchmarks** — runs `cargo bench` on Ubuntu only; uploads `bench-results.txt` as the `benchmark-results` artifact; depends on `test` (only runs after tests pass)

### CI checklist before merging

Before merging any PR:
- [ ] All CI checks green (macOS + Ubuntu + Windows)
- [ ] GitHub Pages build passes (no Jekyll/Liquid errors)
- [ ] Review comments addressed and responded to
- [ ] Test count in README/CLAUDE.md/website matches actual `cargo test` output
- [ ] Documentation updated for all new features

### Release checklist

Before tagging a release:
- [ ] All 5 version locations updated (see above)
- [ ] `cargo test --workspace` passes locally
- [ ] `cargo clippy --workspace -- -D warnings` passes
- [ ] `cargo fmt --check` passes
- [ ] README Key Features section reflects new features
- [ ] docs/index.html test count and tool count are correct
- [ ] docs/docs.html has no stale references
- [ ] Plugin version matches in both `claude-plugin/` and `.claude-plugin/marketplace.json`
- [ ] GitHub Pages build succeeds (check the deploy workflow)

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

Windows Tantivy flakes are broader than just Tier 2 — any integration test touching the index can flake (`search_finds_python_class`, `trait_method_dispatch_links_impl`, etc.). Different test fails each run. Re-run failed CI if only Windows tests fail and Ubuntu+macOS are green.

### Adding a new crate to the workspace

When adding a new crate that depends on `codixing-core`, ALWAYS:
1. Use `codixing-core = { path = "../core", default-features = false }` (NOT bare path)
2. Add `usearch = ["codixing-core/usearch"]` to the crate's `[features]`
3. Set `default = ["usearch"]`
4. Verify with `cargo build --workspace --no-default-features` (simulates Windows CI)

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

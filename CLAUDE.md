# Codixing — Claude Instructions

## Code Search & Navigation

**MANDATORY: Always use the Codixing CLI** (via Bash) instead of `Grep`, `grep`, `find`, `cat`, or `rg` for code exploration tasks. This applies to ALL agents, including subagents. A PreToolUse hook enforces this — Grep on code/doc/config files is **denied** and redirected to the appropriate codixing command.

| Instead of... | Use (via Bash)... |
|---|---|
| `grep -rn "literal"` | `codixing grep "literal"` |
| `grep -c "pattern" file` | `codixing grep "pattern" --file file --count` |
| `Grep "symbol" **/*.rs` | `codixing search "symbol"` |
| `Grep "fn foo"` to find a definition | `codixing symbols foo` |
| Manual call-site hunting | `codixing usages foo` |
| Manual dependency tracing | `codixing callers <file>` / `codixing callees <file>` |
| `find . -name "*.rs"` | `Glob` tool (Codixing doesn't replace file finding) |
| `cat file.rs` / `Read` tool | `Read` tool (Codixing doesn't replace file reading) |

**CLI commands** (run from repo root via Bash):
```bash
codixing search "rate limiting"     # semantic code search
codixing grep "TODO" --literal      # literal/regex text scan with line numbers
codixing symbols Widget             # find symbol definitions
codixing usages add_chunk           # find call sites and imports
codixing callers src/engine/mod.rs  # who imports this file
codixing callees src/engine/mod.rs  # what this file imports
codixing graph --map                # repo architecture map
codixing impact src/engine/mod.rs   # blast radius analysis
codixing api src/engine/mod.rs      # public API surface
codixing types Engine               # type relationships
codixing examples add_chunk         # usage examples from tests + callers
codixing context src/engine/mod.rs  # cross-file context assembly
```

The MCP server is also available when connected to an editor, but the CLI is preferred — it's simpler, works for subagents, and dogfoods the search quality directly.

For broad codebase exploration, always try Codixing first. Fall back to Grep/Bash only if the CLI doesn't cover the case.

**Before editing ANY file**, run `codixing impact <file>` to check downstream dependencies. This applies to all files — Rust, HTML, config, docs — not just code. Even "simple" changes like CSS updates can break references in other files.

### When to use which command

- **Understanding a symbol** → `codixing usages <name>` (call sites and imports)
- **Finding where something is defined** → `codixing symbols <name>`
- **Literal or regex text scan** → `codixing grep "<pattern>"` (emits `path:line:col:text`; supports `--count`, `--files-with-matches`, `-i`, `--invert`, `--glob`, `--file`, `--json`)
- **Searching by concept / natural language** → `codixing search "<query>"`
- **Searching by symbol type** → `codixing search "<query>" --kind function` (function, struct, enum, trait, impl, const)
- **Listing files by glob** → `Glob` tool (Codixing doesn't replace file finding)
- **Impact analysis before a change** → `codixing impact <file>` (blast radius + affected tests)
- **Seeing all callers of a function** → `codixing usages <name>`
- **Public API surface of a file** → `codixing api <file>`
- **Type relationships for a symbol** → `codixing types <name>`
- **Usage examples for a symbol** → `codixing examples <name>` (tests + callers + doc blocks)
- **Cross-file context for understanding** → `codixing context <file> --line N`
- **Architecture overview** → `codixing graph --map`
- **Test coverage discovery** → `codixing search "test <name>"`
- **Index freshness / stale files** → `codixing audit`
- **Incremental re-index after changes** → `codixing sync`
- **Code/config results only** → `codixing search "<query>" --code-only`
- **Documentation results only** → `codixing search "<query>" --docs-only`

## Project Structure

- `crates/core/` — engine: AST parsing, BM25, graph, embeddings, PageRank, test mapping, shared sessions, queue-based embedding (optional `rustqueue` feature), doc indexing (Markdown + HTML with section-aware chunking and doc-to-code graph edges), change impact analysis, semantic concept graph, API surface analysis, type relations, usage example mining, cross-file context assembly, behavioral signatures, query-personalized PageRank, learned query reformulation, output filter pipeline (TOML-based, tee recovery)
- `crates/cli/` — `codixing` CLI binary
- `crates/mcp/` — MCP server (`codixing-mcp`), 67 tools in `src/tools/` (use `--medium` to curate the list for clients without dynamic tool discovery)
- `crates/server/` — HTTP API server (`codixing-server`), REST endpoints with SSE streaming for sync
- `crates/core/src/federation/` — cross-repo federated search (`--federation config.json`)
- `crates/lsp/` — LSP server (`codixing-lsp`), hover/go-to-def/refs/symbols/call hierarchy/complexity diagnostics/rename/semantic tokens
- `claude-plugin/` — Claude Code plugin with 5 skills + MCP server config
- `.codixing/` — index data (do not edit manually)

## Build & Test

```bash
cargo build --release --workspace          # build all binaries
cargo test --workspace                      # run all tests (1107)
cargo clippy --workspace -- -D warnings     # lint (must pass)
cargo fmt --check                           # format check (must pass)

# Windows (no usearch):
cargo build --workspace --no-default-features
cargo test --workspace --no-default-features

# With RustQueue queue-based embedding:
cargo build -p codixing-core --features rustqueue
cargo test -p codixing-core --features rustqueue

# Embedding speed benchmarking:
cargo run --release -p codixing-cli -- bench-embed /path/to/repo --model bge-small-en
```

## Plugin Skills

The Codixing Claude Code plugin provides 5 slash commands:

| Skill | Purpose |
|-------|---------|
| `/codixing-setup` | Index a new project and register the MCP server |
| `/codixing-explore` | Deep codebase exploration with architecture overview. Includes **preflight existence scan** (Step 2) to catch duplicate feature proposals |
| `/codixing-review` | Code review with dependency graph context. Includes **claim verification** (Step 7) to flag unverified accuracy claims in diffs |
| `/codixing-preflight` | Mandatory before proposing new features — searches for existing implementations (Gate 1) and verifies benchmark claims (Gate 2) |
| `/codixing-release` | Complete release pipeline — version bump, tests, docs, CI review, benchmark, blog, X post, tag, publish. All steps mandatory |

### Release workflow

Use `/codixing-release [version]` to ship. It handles everything:
1. Pre-flight checks (clean state, tests, clippy, fmt)
2. Version bump in all 5 locations
3. Documentation update (README, CLAUDE.md, docs/index.html)
4. PR creation + CI monitoring + review comment fixes
5. Merge + tag (with auto-tag re-push workaround)
6. GitHub Release notes
7. Blog post (asks for angle first)
8. X post via automarketing repo

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
cargo test --workspace                      # ALL tests must pass (1107)
cargo clippy --workspace -- -D warnings     # zero warnings
cargo fmt --check                           # zero diffs
```

Subagents and worktree agents MUST run these checks before committing. If any check fails, fix the issue before committing — never skip.

### Visual changes require visual verification

After editing any HTML/CSS file in `docs/` (index.html, docs.html, blog.html):
1. Serve locally: `cd docs && python3 -m http.server 8080`
2. Use `/browse` to open `http://localhost:8080` and take screenshots
3. Check at least desktop (>1024px) and mobile (<600px) widths
4. Verify interactive elements (tabs, steps, forms, scroll animations)
5. Only then commit and create the PR

Never ship visual changes without visual verification.

### Documentation is part of the feature

Every feature commit MUST include documentation updates:
- Update test count in README.md, CLAUDE.md, and docs/index.html
- Update feature descriptions in README.md Key Features if applicable
- Update CLAUDE.md if the change affects project structure, tools, or capabilities
- Update docs/docs.html if LSP or MCP capabilities change

Never batch documentation updates after implementation — document as you go.

### Subagent rules

When dispatching subagents (implementation, review, or any task):

1. **Always use the Codixing CLI for code exploration.** Subagents MUST use `codixing search`, `codixing symbols`, `codixing usages`, `codixing callers`, `codixing callees` instead of `grep`, `cat`, `find`. Include this instruction in every subagent prompt.
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
- **vscode** — compiles the VS Code extension on Ubuntu
- **release-build** — builds optimized release binaries for `x86_64-linux`, `aarch64-darwin`, and `x86_64-windows-msvc` (no-default-features). Runs only on `main` pushes and `v*` tag pushes — never on PRs. Uploads artifacts named `binaries-<suffix>` with 14-day retention. `needs: test` so broken code never produces binaries. Artifacts are downloaded by `release.yml` on tag push instead of rebuilding from scratch (saves ~25 min per release).
- **audit** — runs `cargo-audit` on Ubuntu only; `continue-on-error: true` (non-blocking while advisories are triaged)
- **coverage** — runs `cargo-llvm-cov` on Ubuntu only; uploads `lcov.info` as the `coverage-report` artifact
- **benchmarks** — runs `cargo bench` on Ubuntu only; uploads `bench-results.txt` as the `benchmark-results` artifact; depends on `test` (only runs after tests pass)

**CI → release coupling invariant:** `release.yml` references `workflow: ci.yml` when downloading artifacts via `dawidd6/action-download-artifact`. If you rename `ci.yml`, update `release.yml` at the same time or release.yml will fail to find binaries. Same rule applies to the `release-build` job name and the `binaries-<suffix>` artifact naming convention — both are contractual with the downloader.

### CI checklist before merging

Before merging any PR:
- [ ] All CI checks green (macOS + Ubuntu + Windows)
- [ ] GitHub Pages build passes (no Jekyll/Liquid errors)
- [ ] Review comments addressed and responded to
- [ ] Test count in README/CLAUDE.md/website matches actual `cargo test` output
- [ ] Documentation updated for all new features

### Release checklist

Before tagging a release:
- [ ] All pending PRs merged to main (`gh pr list --state open --base main` must be empty)
- [ ] All 5 version locations updated (see above)
- [ ] `cargo test --workspace` passes locally
- [ ] `cargo clippy --workspace -- -D warnings` passes
- [ ] `cargo fmt --check` passes
- [ ] README Key Features section reflects new features
- [ ] docs/index.html test count and tool count are correct
- [ ] docs/docs.html has no stale references
- [ ] Plugin version matches in both `claude-plugin/` and `.claude-plugin/marketplace.json`
- [ ] GitHub Pages build succeeds (check the deploy workflow)
- [ ] **CI run on the merge commit produced `binaries-linux-x86_64`, `binaries-macos-aarch64`, and `binaries-windows-x86_64` artifacts** — `release.yml` downloads these by commit SHA instead of rebuilding. Check the Actions tab for the CI run on the target commit. If the release-build job failed or never ran, re-trigger CI (push an empty commit) before tagging, or `release.yml` will fail to find artifacts. Artifact retention is 14 days.

### Git history hygiene

When rewriting git history (e.g. before going public):
- **`git-filter-repo` ordering matters:** Do file path removals (`--path --invert-paths`) first, blob replacements (`--replace-text`) second, message rewrites (`--message-callback`) third, and mailmap (`--mailmap`) LAST. Each pass rewrites all commits, undoing previous mailmap changes.
- **Include ALL identity variants in one mailmap file.** Don't run mailmap multiple times — collect all `old → new` mappings in a single file.
- **Verify with `git log --all -p -S "string"`** after each pass. `-S` searches reachable blobs, not just HEAD.

### Known flaky tests (resolved)

These tests previously flaked due to Tantivy file locking on Windows:
- `git_sync_no_op_when_already_current` — fixed with `#[serial]`
- `git_sync_no_op_without_git` — fixed with `#[serial]`
- `graph_persists_across_open` — fixed with `#[serial]`

**Windows Tantivy flakes — permanently fixed**: Windows CI now runs tests single-threaded (`--test-threads=1`), eliminating all Tantivy file lock contention. All `#[cfg_attr(windows, ignore)]` annotations have been removed — tests now run on all platforms with full coverage.

### Adding a new crate to the workspace

When adding a new crate that depends on `codixing-core`, ALWAYS:
1. Use `codixing-core = { path = "../core", default-features = false }` (NOT bare path)
2. Add `usearch = ["codixing-core/usearch"]` to the crate's `[features]`
3. Set `default = ["usearch"]`
4. Verify with `cargo build --workspace --no-default-features` (simulates Windows CI)

## MCP Server Configuration

The `.mcp.json` configures the Codixing MCP server for Claude Code. **Required flags:**

- `--medium` — exposes 27 core tools directly. Useful for MCP clients that cannot do dynamic tool discovery (e.g. Codex CLI).
- `--no-daemon-fork` — prevents stale daemon socket issues that silently kill the MCP connection

Example `.mcp.json`:
```json
{
  "mcpServers": {
    "codixing": {
      "type": "stdio",
      "command": "./target/release/codixing-mcp",
      "args": ["--root", ".", "--medium", "--no-daemon-fork"]
    }
  }
}
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

## Grep Trigram Benchmark

**Apple M4 — trigram pre-filtering vs full scan:**

| Repo size | Pattern type | Trigram | Full scan | Speedup |
|---|---|---|---|---|
| 1K files | Literal (`Widget_500`) | 74µs | 8.1ms | **110×** |
| 1K files | Regex (`process_widget_\d+`) | 525µs | 420µs | ~1× (matches all files) |
| 10K files | Literal (`Widget_500`) | 1.1ms | 58ms | **52×** |
| 10K files | Regex (`process_widget_\d+`) | 2.6ms | 1.2ms | ~1× (matches all files) |
| 20 files | Literal (`process_batch`) | 258µs | 263µs | ~1× (too few files) |

Trigram pre-filtering provides massive speedups for **selective** patterns (identifiers, specific strings) at scale. Patterns that match most files see no benefit — this is expected and correct (the trigram index can't eliminate candidates that genuinely match).

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

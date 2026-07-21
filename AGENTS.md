# Codixing — Codex Instructions

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
codixing agent-context-pack "task"  # stable JSON context pack for agents
codixing ask "task"                 # recommended agent entrypoint (context pack)
codixing symbols Widget --defs-only # definitions only (no Import rows)
codixing impact path --full         # full blast radius (default is compact)
codixing doctor --fix-path          # PATH binary version gate
codixing bench-tokens               # token-savings harness vs grep+read
```

The MCP server is also available when connected to an editor, but the CLI is preferred — it's simpler, works for subagents, and dogfoods the search quality directly.

For broad codebase exploration, always try Codixing first. Fall back to Grep/Bash only if the CLI doesn't cover the case.

**Before editing ANY file**, run `codixing impact <file>` to check downstream dependencies. This applies to all files — Rust, HTML, config, docs — not just code. Even "simple" changes like CSS updates can break references in other files.

### When to use which command

- **Understanding a symbol** → `codixing usages <name>` (call sites and imports)
- **Finding where something is defined** → `codixing symbols <name>`
- **Literal or regex text scan** → `codixing grep "<pattern>"` (emits `path:line:col:text`; supports `--count`, `--files-with-matches`, `-i`, `--invert`, `--glob`, `--file`, `--json`)
- **Searching by concept / natural language** → `codixing search "<query>"`
- **Listing files by glob** → `Glob` tool (Codixing doesn't replace file finding)
- **Impact analysis before a change** → `codixing impact <file>` (blast radius + affected tests)
- **Seeing all callers of a function** → `codixing usages <name>`
- **Public API surface of a file** → `codixing api <file>`
- **Type relationships for a symbol** → `codixing types <name>`
- **Usage examples for a symbol** → `codixing examples <name>` (tests + callers + doc blocks)
- **Cross-file context for understanding** → `codixing context <file> --line N`
- **Task-local agent context pack** → `codixing ask "<task>"` or `agent-context-pack "<task>" --mode edit`
- **Finding a definition** → `codixing search Name --strategy goto` or `symbols Name --defs-only`
- **Architecture overview** → `codixing graph --map --token-budget 1500`
- **Token-savings proof** → `codixing bench-tokens`
- **Test coverage discovery** → `codixing search "test <name>"`
- **Index freshness / stale files** → `codixing audit`
- **Incremental re-index after changes** → `codixing sync`
- **Code/config results only** → `codixing search "<query>" --code-only`
- **Documentation results only** → `codixing search "<query>" --docs-only`

## Project Structure

- `crates/core/` — engine: AST parsing, BM25, graph, embeddings, PageRank, test mapping, shared sessions, queue-based embedding (optional `rustqueue` feature), doc indexing (Markdown + HTML + reStructuredText + AsciiDoc + plain-text + OpenAPI/Swagger + Jupyter notebook dispatcher + optional PDF via `--features pdf`), change impact analysis, semantic concept graph, API surface analysis, type relations, usage example mining, cross-file context assembly, behavioral signatures, query-personalized PageRank (with LRU cache for repeat-seed BM25 sets), learned query reformulation (identifier co-occurrence + doc-to-code + session mining algorithm), output filter pipeline (TOML-based, tee recovery)
- `crates/cli/` — `codixing` CLI binary
- `crates/mcp/` — MCP server (`codixing-mcp`) with generated tool definitions and profile-filtered discovery (`tool_defs/*.toml`)
- `crates/server/` — HTTP API server (`codixing-server`), REST endpoints with SSE streaming for sync
- `crates/core/src/federation/` — cross-repo federated search (`--federation config.json`)
- `crates/core/src/persistence/` — immutable generation publication, stable
  single-writer lease, copy-on-write incremental checkpoints, and durable
  changed-path recovery journal
- `crates/lsp/` — LSP server (`codixing-lsp`), hover/go-to-def/refs/symbols/call hierarchy/complexity diagnostics/rename/semantic tokens
- `claude-plugin/` — Claude Code plugin with 5 skills + MCP server config
- `.codixing/` — index control files plus atomically activated data generations
  under `generations/` (do not edit manually). Rebuilds temporarily need space
  for both the active and new generation; failed rebuilds preserve the active one.

## Build & Test

```bash
cargo build --release --workspace          # build all binaries
cargo test --workspace                      # run the full workspace suite
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

The Codixing Codex plugin provides 5 slash commands:

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
2. Version bump in all 14 fields across 7 files
3. Documentation update (README, AGENTS.md, docs/index.html)
4. PR creation + CI monitoring + review comment fixes
5. Merge + one automatic version tag; main CI dispatches the release after artifacts build
6. GitHub Release notes
7. Blog post (asks for angle first)
8. X post via automarketing repo

## Version Locations

When bumping the version, update ALL of these files:

1. `Cargo.toml` — `workspace.package.version`
2. `Cargo.lock` — the source-less package versions for `codixing`, `codixing-core`, `codixing-lsp`, `codixing-mcp`, and `codixing-server`
3. `npm/package.json` — `version`
4. `editors/vscode/package.json` — `version`
5. `editors/vscode/package-lock.json` — top-level `version` AND `packages[""].version`
6. `claude-plugin/.claude-plugin/plugin.json` — `version`
7. `.claude-plugin/marketplace.json` — `metadata.version`, the Codixing plugin version, AND immutable `source.ref` (`vX.Y.Z`)

### Dual plugin manifest

The plugin lives in two files that must stay version-synced:
- `.claude-plugin/marketplace.json` — registry entry for the Claude Code marketplace (what `claude plugin marketplace add` reads).
- `claude-plugin/.claude-plugin/plugin.json` — the actual plugin bundle that ships with hooks + skills.

Both are covered by `scripts/bump_version.py`; edit by hand only if you know why.

## Development Workflow — Quality Rules

### Mandatory verification before every commit

Every commit MUST pass all 3 checks. No exceptions:

```bash
cargo test --workspace                      # ALL tests must pass
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
- Keep durable test-suite wording accurate; do not hand-maintain an exact count
- Update feature descriptions in README.md Key Features if applicable
- Update AGENTS.md if the change affects project structure, tools, or capabilities
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
- Use `codixing symbols`, `codixing context`, or a targeted file read to verify real method signatures before writing plan code snippets
- Verify struct field names against the actual source
- When a plan reviewer finds API mismatches, fix ALL of them — not just the ones flagged

### Parallel feature branches

When launching multiple feature branches in parallel (e.g. via worktree agents):

1. **Plan merge order upfront.** Identify which files are shared and decide merge order from smallest/most-independent to largest.
2. **Each PR must include its own docs updates.** Update README, website, and AGENTS.md as part of each feature PR.
3. **Merge one at a time, wait for CI.** After each squash-merge, pull main, verify CI passes on main (including the Jekyll/Pages build), THEN rebase the next PR.
4. **Check for behavioral interactions.** When planning features that change binary behavior (e.g., daemon auto-fork), explicitly note impacts on existing tests that spawn the binary as a subprocess.

### CI jobs

The CI workflow (`.github/workflows/ci.yml`) has the following jobs:

- **test** — blocking Ubuntu/macOS/Windows matrix. All legs validate JSON and formatting; Ubuntu and macOS build/test with default features and run clippy, while Windows builds/tests with `--no-default-features`. Ubuntu also runs the README ↔ CLI drift check and `scripts/self_audit.sh`.
- **vscode** — blocking reusable `vsix.yml` job. It compiles, packages, and inspects the extension icon, license, and embedded version on every CI event; on `main` pushes it also uploads `vsix-package` with 14-day retention.
- **msrv** — blocking Ubuntu check of all workspace targets at Rust 1.88 with `--no-default-features`.
- **npm-installer** — blocking Ubuntu/Node 18 tests for release-version consistency and rollback, exact npm tarball comparison, downloader and signal forwarding, npm package contents, and the checksum-verified shell installer.
- **audit** — blocking Ubuntu `cargo-audit` run. Its explicit advisory ignores are justified in `audit.toml`; there is no `continue-on-error` escape hatch.
- **coverage** — blocking Ubuntu coverage generation and `coverage-report` artifact upload. The final Codecov upload alone is configured not to fail CI on a Codecov service error.
- **release-build** — blocking three-platform matrix on `main` pushes only (never PRs or tag pushes). It has `needs: [test, npm-installer]`, builds the four binaries for Linux x86_64, macOS arm64, and Windows x86_64 (Windows uses `--no-default-features`), and uploads `binaries-<suffix>` artifacts with 14-day retention.
- **benchmarks** — blocking Ubuntu job with no `needs` dependency. It runs the
  registered Criterion benches and the machine-readable large-repository gate,
  then uploads both results as `benchmark-results`. Pull requests and main
  pushes use the regression-only 10K profile. Weekly and manually selected
  100K runs use strict-claim mode: they require a trusted `origin/main`
  baseline revision plus separate commit-bound external-quality JSON for the
  baseline and candidate (positive task count, dataset digest, MRR, and
  Recall@10). Baseline and candidate init/sync commands use an explicit fixed
  eight-worker cap; missing or mismatched worker telemetry, missing quality
  evidence, or stale evidence fails closed. The published speed scope is the
  five-operation geometric mean, not a claim that every operation is 2x faster.

The auto-tag workflow waits for the entire CI workflow to conclude successfully, so every blocking job above gates release tagging even though `release-build` itself directly depends only on `test` and `npm-installer`.

**CI → release coupling invariant:** `release.yml` resolves a successful `ci.yml` run for the exact tagged commit and downloads `binaries-linux-x86_64`, `binaries-macos-aarch64`, `binaries-windows-x86_64`, and `vsix-package` with `gh run download`. If the CI filename, artifact names, or release-build/VSIX publication behavior changes, update `release.yml` in the same change.

**Release → Pages invariant:** `release.yml` dispatches `pages.yml` from
protected `main` and passes the immutable release tag as an input. `pages.yml`
checks out that tag and verifies the checkout commit before deployment. Do not
dispatch the workflow at the tag ref unless the `github-pages` environment is
also configured to allow release tags.

### CI checklist before merging

Before merging any PR:
- [ ] All CI checks green (macOS + Ubuntu + Windows)
- [ ] GitHub Pages build passes (no Jekyll/Liquid errors)
- [ ] Review comments addressed and responded to
- [ ] Documentation describes the current test matrix without a stale hand-maintained exact count
- [ ] Documentation updated for all new features

### Release checklist

Before tagging a release:
- [ ] All pending PRs merged to main (`gh pr list --state open --base main` must be empty)
- [ ] All 14 version fields across the 7 files above are updated
- [ ] `cargo test --workspace` passes locally
- [ ] `cargo clippy --workspace -- -D warnings` passes
- [ ] `cargo fmt --check` passes
- [ ] README Key Features section reflects new features
- [ ] docs/index.html describes the current CLI/MCP/test surfaces without stale exact counts
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

The `.mcp.json` configures the Codixing MCP server for Codex. **Recommended for editor stdio:**

- `--no-daemon-fork` — prevents stale daemon socket issues that silently kill the MCP connection

The `--medium` flag was removed in v0.38. `codixing-mcp` generates its catalog
from `tool_defs/*.toml` and filters `tools/list` by the active profile. The April 2026 agent benchmark found
`--medium` was hiding `get_complexity`, `review_context`, and other showcase
tools — fixing the curation restored the March "66% fewer tokens / 66% fewer
calls" headline. See `docs/research-recall-stickiness-2026-04-13.md` §4.19–4.24.
Agents can call `get_mcp_profile` and `set_mcp_profile` at runtime to inspect or
switch within the server's startup safety ceiling. Minimal/reviewer startup is
read-only; `--allow-profile-escalation` must be explicit if runtime promotion to
a write-capable profile is intended.

Example `.mcp.json`:
```json
{
  "mcpServers": {
    "codixing": {
      "type": "stdio",
      "command": "./target/release/codixing-mcp",
      "args": ["--root", ".", "--no-daemon-fork"]
    }
  }
}
```

## MCP Index Maintenance

The Codixing index lives in `.codixing/`. After significant file changes, sync it:

```bash
./target/release/codixing sync .
```

The MCP daemon batches watcher mutations into an unpublished checkpoint and
publishes after 2 seconds idle, 30 seconds maximum age, or 256 changed paths.
Do not write generation artifacts in place: fork through `IndexStore`, replace
mutable sidecars atomically, and make the active-generation manifest the last
durable write. A no-op sync must not fork or publish. On filesystems without
hard-link support, incremental fallback copying is capped at 64 MiB; use a full
`codixing init` rather than duplicating a large index.

To rebuild from scratch (BgeSmallEn is the recommended model — fastest init, good retrieval):
```bash
ORT_DYLIB_PATH=/absolute/path/to/libonnxruntime.so \
  ./target/release/codixing init . --embed --model bge-small-en
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

Trigram pre-filtering provides massive speedups for **selective** patterns (identifiers, specific strings) at scale. Patterns that match most files see no benefit — this is expected and correct (the trigram index can't eliminate candidates that genuinely match). The persisted file-level index is shared by `grep` and `Strategy::Exact`; exact search streams candidate paths in bounded batches and verifies stored Tantivy chunks. Fresh indexes do not write a separate `chunk_trigram.bin`.

### Release-to-release perf comparison

Use criterion's **named baselines** — not the default `base/change/` dirs, which get overwritten on every run and are useless for "did vX.Y regress vs vX.(Y-1)".

```bash
# At release time, capture numbers for the released tag.
git checkout v0.41.0
cargo bench --bench search_bench -- --save-baseline v0.41

# Before cutting the next release, diff against the baseline.
git checkout main
cargo bench --bench search_bench -- --baseline v0.41
```

Criterion emits the delta inline:

```text
bm25_search_identifier  time:   [72.4 µs 72.5 µs 72.6 µs]
                        change: [-1.2% +0.1% +1.5%] (p = 0.87 > 0.05)
                        No change in performance detected.
```

See `benchmarks/results/README.md` § "Release-to-release performance comparison" for the full workflow. Named baselines live under `target/criterion/<bench>/<name>/`, but `cargo clean` removes `target/`, including Criterion baselines and reports.

## Embedding Model Benchmark

**Apple M4 (127 files, 1054 chunks):**

| Model        | Init time | Dims | Cold start (MCP) | Warm search | Notes |
|---|---|---|---|---|---|
| BM25-only    | 0.3s      | —    | ~115ms           | ~35ms       | No ONNX needed |
| **BgeSmallEn** | **110s** | 384  | ~107ms           | ~35ms       | **Recommended** — hybrid search shines on NL queries |

For BgeSmallEn/BgeBaseEn, set `ORT_DYLIB_PATH` on every platform to the exact
absolute shared-library file (`libonnxruntime.dylib`, `libonnxruntime.so`, or
`onnxruntime.dll`). `LD_LIBRARY_PATH` may still be needed on Linux when that
library has dependencies outside the loader's normal search path. Run
`codixing doctor` to verify the resolved runtime; BM25-only operation needs no
ONNX Runtime.

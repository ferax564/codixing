# Changelog

All notable changes to Codixing will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added
- **`codixing grep` CLI command** — literal or regex text scan across indexed files, trigram-accelerated. Emits `path:line:col:text` by default (1-indexed line/col to match `grep -n`). Supports `--literal`, `-i/--ignore-case`, `--invert`, `--file`, `--glob`, `-C/-B/-A` (symmetric or asymmetric context), `--count`, `--files-with-matches`, `--json`, and `--limit`. Closes the last grep-fallback gap surfaced during v0.35 polish work. Fast-path auto-proxies through a running `codixing-mcp` daemon when available.
- **`Engine::grep_code_opts(&GrepOptions)`** — structured variant of `Engine::grep_code` that adds case-insensitive matching (via `regex::RegexBuilder`), inverted line selection, and asymmetric before/after context. Legacy positional `Engine::grep_code(...)` remains as a thin forwarder for backward compatibility.
- **MCP `grep_code` tool gains new params** — `case_insensitive`, `invert`, `before_context`, `after_context`, `count_only`, `files_with_matches`. Existing `context_lines` still accepted as a symmetric shorthand.

### Changed
- **Bash dogfooding hook shrink** — `claude-plugin/hooks/pretool-bash-codixing.sh` drops the single-file, `| wc -l`, and version-string passthroughs (127 → 112 lines). All three cases are now native `codixing grep` features, so the compliance leaks close. Deny message now suggests `codixing grep "<pattern>"` first.

## [0.34.0] — 2026-04-12

Audit-driven release bundling v0.33 prep and v0.34 follow-ups. 8 PRs: #69, #70, #71, #72, #73, #74, #75, #76. Skipping the v0.33.0 tag — all v0.33 work is included here.

### Added
- **`codixing filter` CLI subcommand** — validate `.codixing/filters.toml` and run the filter pipeline on stdin without booting the MCP server. `check` and `run` actions. (#75)
- **`codixing sync --no-embed`** — escape hatch that temporarily stashes the embedder for the duration of sync, preventing runaway CPU on existing hybrid indexes. Canonical bad case: Linux kernel sync hit 68 min CPU before kill in v0.32. (#75)
- **CLI daemon proxy full coverage** — `symbols`, `usages`, `impact`, `graph --map` now auto-proxy through a running `codixing-mcp` daemon. ~10× speedup on warm daemon, matching v0.33's `search` speedup. (#75, builds on #73)
- **Windows named-pipe client** — the daemon proxy now works on Windows via `std::fs::OpenOptions` on `\\.\pipe\codixing-<hash>`. First time Windows users see the warm-daemon speedup. (#75)
- **Sync progress output** — `codixing sync` now emits `[sync +Xs] <stage>` lines instead of running silent for minutes. (#69)
- **Bash dogfooding hook** — new `PreToolUse` matcher on `Bash` that catches agents shelling out to `grep`/`rg`/`find`/`cat` against indexed files and redirects to the codixing CLI. Closes the biggest bypass in the v0.32 hook. (#69)
- **Plugin ships dogfooding hooks** — `claude-plugin/hooks/` now contains both hook scripts and `plugin.json` registers them, so downstream plugin users get enforcement automatically. (#69)
- **`codixing callers <file>` diagnostics** — distinguishes four cases: file not on disk, file on disk but not in graph (stale index), has callees but no callers (entry point), normal listing. (#74)

### Changed
- **`codixing init` default flipped to BM25+graph-only.** Embeddings are now opt-in via `--embed`. Rationale: embedding a 10K-file repo took 14 min in v0.32, and 63K Linux kernel files took 25 min — unusable defaults. Agent code exploration via `symbols`/`usages`/`callers`/`impact` works fine on BM25+graph alone. (#71) **Breaking for users who expected embedding by default.**
- **`Engine::open` writer-lock retry loop** — retries 10× with exponential backoff (1ms → 512ms, ~1s total) before falling back to read-only mode. Absorbs the intra-process drop-then-reopen race that caused the macOS `git_sync_no_op_when_already_current` flake. Tests no longer need `thread::sleep(100ms)` workarounds between `drop(Engine::init)` and `Engine::open`. (#76)
- **Warning on sync with missing graph** — older indexes predate graph support; sync now warns that graph-dependent features (`impact`, `callers`, `callees`, `graph --map`) will return empty until `codixing init` rebuilds. (#75)

### Removed
- **`codixing-mcp --compact`** — hard-removed. v0.33 accepted+ignored it with a warning; v0.34 rejects it at argument parsing with "unexpected argument". Users with `--compact` in `.mcp.json` must migrate to `--medium` (for clients without dynamic tool discovery) or remove the flag. Closes issue #67. (#70 deprecated, #76 hard-removed)
- **`codixing init --no-embeddings`** — same deprecation cycle as `--compact`. BM25+graph-only is now the default, so the flag has no remaining semantics. (#71 deprecated, #76 hard-removed)

### Fixed
- **Audit reported 0 files on populated graphs** — on large indexes (observed on the Linux kernel), `chunk_meta` hydration could partially fail, leaving `file_chunk_counts` empty while the graph was fully populated. `audit_freshness` now unions `file_chunk_counts.keys()` with `graph.file_paths()` so it never under-reports. Regression test included. (#72)
- **GraphML namespace URI** — was emitting `xmlns="http://graphml.graphstruct.org/graphml"` (a misspelled placeholder). Fixed to the official `http://graphml.graphdrawing.org/xmlns` so Gephi/yEd accept the output without schema warnings. Regression test pins the correct URI and asserts the old wrong one is absent. (#69)
- **Tantivy `Access is denied` flakes on Windows** — intermittent failures in `Engine::init` and `TantivyIndex::commit` when Windows Defender scans newly-created segment files. New `crates/core/src/index/windows_retry.rs` helper retries operations up to 10× with exponential backoff on recognized transient error codes (5, 32, 33). Zero-cost on Unix. Covers `create_in_dir_with_config`, `open_in_dir_with_config`, `commit()`, and `IndexReader::reload`. (#74)
- **`claude-plugin/.claude-plugin/plugin.json` and `.claude-plugin/marketplace.json`** had duplicate `"version"` keys (technically invalid JSON). Same issue fixed in `npm/package.json` and `docs/install.sh` this release. (#69, #76)
- **Docs drift** — README, CLAUDE.md, docs/index.html now report 1077 tests (was 1019) and 26 CLI commands (was 24/25), add the v0.31.1/v0.32 features that were undocumented on the landing page (GraphML/Cypher/Obsidian exports, git hooks, caller cascade, filter pipeline), and correct the Linux kernel file count from 73K to 63K C/H. (#69)

### Known gaps (v0.35 backlog)
- **Stale codixing index after Edit/Write** — the plugin doesn't yet auto-update the index after file edits, so `codixing symbols` and `codixing usages` can return stale line numbers between syncs. Tracked in issue #77. Workaround: run `codixing sync` or `codixing update` manually after a batch of edits.
- **Daemon proxy for `callers` and `callees`** — the MCP `symbol_callers`/`symbol_callees` tools are symbol-level while CLI works on files. No clean mapping yet; commands stay in-process.
- **`codixing sync` doesn't rebuild a missing graph** — only warns. A true rebuild is effectively a full init and the user should invoke it explicitly.

## [0.31.0] — 2026-04-11

### Added
- **Community detection** — Pure-Rust Louvain algorithm on the import graph; `codixing graph --communities` shows natural module clusters with modularity score
- **Shortest path** — `codixing path <from> <to>` finds the shortest import chain between two files via BFS
- **Surprise detection** — Scores edges by unexpectedness (cross-community, PageRank disparity, cross-directory, low confidence); `codixing graph --surprises N`
- **HTML graph export** — `codixing graph --html` generates a self-contained interactive visualization with force-directed layout, community coloring, confidence-styled edges, and surprise highlights
- **Edge confidence** — Every dependency edge tagged `Verified`/`High`/`Medium`/`Low` based on extraction method
- **PreToolUse hook** — Plugin ships a deterministic hook that denies Grep on code/doc/config files and redirects to codixing CLI (replaces the advisory PostToolUse reminder)

### Fixed
- Shortest path BFS excludes `__ext__` pseudo-nodes to prevent false paths through shared external imports
- Legacy graph deserialization derives edge confidence from edge kind instead of defaulting all to Verified
- HTML export escapes `</script>` in embedded JSON to prevent XSS breakout

## [0.21.0] — 2026-03-28

### Changed
- **Refactor:** Split `engine/mod.rs` (2,981 lines) into 4 focused submodules: `init.rs`, `indexing.rs`, `reload.rs`, `validation.rs` — mod.rs now ~1,100 lines
- **Fix:** Replace 28 lock `.expect()`/`.unwrap()` calls with poison-recovery across SharedSession, FederatedEngine, HNSW, and parallel grep
- **Feat:** Implement `Reranker` trait on concrete fastembed Reranker struct, unifying the reranker interface
- **Test:** Add 7 HTTP server integration tests (reindex, remove, repo-map, callees, call-graph, export, view) — 20/21 routes now covered
- **Docs:** Fix tool count inconsistency (was 49/53/54, now consistently 54), update test counts to 845+

## [0.19.0] — 2026-03-27

### Changed
- **Perf:** Kernel-scale performance — 11× smaller chunk_meta, lazy trigram loading via OnceLock, content dedup
- **Perf:** Mmap symbol table — zero-deserialization loading from flat binary
- **Perf:** 306× faster trigram build via batch mode + disk persistence

## [0.18.0] — 2026-03-25

### Added
- Multi-query RRF fusion for natural language queries
- Recency boost stage (+10% linear decay over 180 days)
- File path boosting (2.5× for explicit paths and backtick refs)
- Overlapping chunks at AST boundaries

## [0.17.0] — 2026-03-24

### Added
- Trigram pre-filtering for grep_code (110× faster at 1K files)

### Fixed
- Windows CI permanently fixed (single-threaded test runner)

## [0.16.0] — 2026-03-23

### Added
- 15 features: field BM25, search pipeline, LSP rename, complexity diagnostics, semantic tokens, CI coverage, federation auto-discovery, daemon mode, and more
- HTTP server with 21 REST endpoints + SSE streaming
- VS Code extension

## [0.15.1] — 2026-03-22

### Fixed
- Fix 2 security vulnerabilities: lz4_flex (RUSTSEC-2026-0041) and rustls-webpki (RUSTSEC-2026-0049) via dep update
- Fix Windows CI build failure: server crate now proxies usearch feature (matches mcp/lsp/cli pattern)
- Make cargo-audit CI job blocking (was `continue-on-error`) with explicit `--ignore` for unfixable transitive deps

### Changed
- Updated all transitive dependencies via `cargo update`
- Added `audit.toml` documenting ignored advisories with justification and resolution plan
- Document broader Windows Tantivy flake surface in CLAUDE.md
- Add "Adding a new crate" checklist to CLAUDE.md

### Known Issues
- `time 0.3.45` (RUSTSEC-2026-0009, medium severity) pinned by tantivy 0.22 — resolution planned for v0.16.0 tantivy bump

## [0.14.0] — 2026-03-21

### Added
- Post-v0.13.0 technical roadmap for stability, performance, quality, and ecosystem
- Quality rules in CLAUDE.md: mandatory verification triad, documentation-with-every-feature

### Fixed
- Ignore `multi_root_indexes_both_roots` test on Windows (Tantivy lock flake)
- Move implementation plans out of `docs/` to prevent Jekyll build failures

## [0.13.0] — 2026-03-15

### Added
- Symbol-level call graph for precise callers/callees with trait dispatch resolution
- Windows support via brute-force vector fallback (no usearch dependency)
- Read-only index access for concurrent engine instances
- MCP progress notifications for long-running tool calls
- `--medium` compact mode for MCP tool listing (between full and `--compact`)
- Claude Code plugin with 3 skills: `/codixing-setup`, `/codixing-explore`, `/codixing-review`
- Plugin marketplace manifest for self-hosted install
- OpenAI Codex CLI integration instructions

## [0.12.1] — 2026-03-10

### Added
- Initial public release
- 20 language support with full AST parsing via tree-sitter
- Hybrid search (BM25 + optional vector embeddings with RRF fusion)
- 53 MCP tools across 7 categories
- Daemon mode with Unix socket IPC and auto-fork
- Cross-repo federation with RRF fusion
- LSP server with hover, go-to-def, references, call hierarchy, complexity diagnostics
- GitHub Action for automated code review
- VS Code extension with LSP integration
- CLI binary with search, symbols, callers/callees commands
- Dynamic tool discovery with `--compact` mode (96.7% token reduction)
- Token budget management with adaptive truncation
- Single binary distribution (no external dependencies)

### Fixed
- Strip build paths from release binaries

# Changelog

All notable changes to Codixing will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

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

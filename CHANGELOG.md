# Changelog

All notable changes to Codixing will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

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
- 48 MCP tools across 7 categories
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

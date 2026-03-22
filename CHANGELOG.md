# Changelog

All notable changes to Codixing are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.15.0] — 2026-03-21

### Added
- 4 new languages: Bash/Shell, Matlab, Mermaid diagrams, XML/Draw.io (24 total)
- HTTP server with REST API and SSE streaming for sync progress (`crates/server/`)
- Federation CLI subcommands (`codixing federation init/add/remove/list/search`)
- 5 new federation MCP tools (federation_init, federation_add_project, federation_remove_project, federation_list, federation_search)
- VSIX packaging for VS Code extension (attached to GitHub releases)
- CI: cargo-audit security scanning, code coverage reports, benchmark regression tracking
- CHANGELOG.md with retrospective entries

### Changed
- Deprecated `list_projects` MCP tool in favor of `federation_list`
- Server crate description updated (removed gRPC mention)

## [0.14.0] — 2026-03-16

### Added
- Technical roadmap: stability, performance, quality, and ecosystem planning
- Cross-repo federated search engine (`crates/core/src/federation/`)
- Federation CLI: `--federation config.json` flag
- `federation_search` MCP tool for cross-repo queries
- `list_projects` MCP tool

### Changed
- Bumped to v0.14.0 for technical roadmap release

## [0.13.0] — 2026-03-14

### Added
- 48 MCP tools total (44 core + 2 meta-tools + 2 session tools)
- `search_tools` and `get_tool_schema` meta-tools for dynamic tool discovery
- `session_status` and `get_session_summary` for multi-agent coordination
- `--compact` flag: reduces tools/list from ~6666 to ~218 tokens (96.7% reduction)
- `--medium` flag for intermediate token budget
- Contextual chunk embedding with `build_context_prefix()`
- Adaptive result truncation with score-cliff detection at 35% threshold
- Query-to-code reformulation: 18 NL-to-code pattern mappings
- BGE query prefix support via `embed_query()`
- Type filter `kind` param on `code_search`
- LSP server: hover, go-to-def, references, call hierarchy, complexity diagnostics

### Changed
- Definition boost increased to 3.5×
- RRF fusion switched to HashMap O(N+M)
- Session boost applied in MCP layer (not engine layer)

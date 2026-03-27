# Codixing

[![CI](https://github.com/ferax564/codixing/actions/workflows/ci.yml/badge.svg)](https://github.com/ferax564/codixing/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/ferax564/codixing/graph/badge.svg)](https://codecov.io/gh/ferax564/codixing)

**Website: [codixing.com](https://codixing.com)** · **[Docs](https://codixing.com/docs)**

Ultra-fast code retrieval engine for AI agents — beats `grep` at its own game.

## Install

### Claude Code

```bash
# Plugin — includes MCP server + 3 slash commands (recommended)
claude plugin marketplace add ferax564/codixing
claude plugin install codixing@codixing
```

Restart Claude Code after installing. You get 54 MCP tools plus `/codixing-setup`, `/codixing-explore`, and `/codixing-review`.

Alternatively, register just the MCP server without the plugin:

```bash
claude mcp add codixing -- npx -y codixing-mcp --root .
```

### OpenAI Codex CLI

Install the binary first, then register the MCP server:

```bash
curl -fsSL https://codixing.com/install.sh | sh
codex mcp add codixing -- codixing-mcp --root .
```

> **Note:** Codex requires the binary installed locally — `npx` is not supported. Do not use `--compact` with Codex as it needs all 54 tools visible in the tool list.

### Cursor / Windsurf

Add to your project's `.mcp.json` (or global MCP settings):

```json
{
  "mcpServers": {
    "codixing": {
      "command": "npx",
      "args": ["-y", "codixing-mcp", "--root", "."]
    }
  }
}
```

### Continue.dev

Add to `~/.continue/config.json` under `mcpServers`:

```json
{
  "mcpServers": [
    {
      "name": "codixing",
      "command": "npx",
      "args": ["-y", "codixing-mcp", "--root", "."]
    }
  ]
}
```

### Binary install

For CLI usage or when `npx` isn't available:

```sh
curl -fsSL https://codixing.com/install.sh | sh
```

Installs `codixing`, `codixing-mcp`, and `codixing-lsp` to `/usr/local/bin`. macOS (Apple Silicon) and Linux (x86_64). Binaries also on the [releases page](https://github.com/ferax564/codixing/releases).

---

## Why Not Just Grep?

AI coding agents use `grep`, `find`, and `cat` for code navigation. These tools return **everything, always** — a single `rg b2Vec2` on a real codebase returns 2,240 hits (225 KB), burning context before any reasoning happens.

Codixing returns the top 20 results in 1.3 KB — same signal, **99% less waste**.

| Capability | grep/rg | Codixing |
|-----------|---------|----------|
| Bounded, ranked output | No | Yes (BM25 + PageRank) |
| Symbol definitions (not just mentions) | No | Yes (AST-parsed symbol table) |
| Dependency graph queries | No | Yes (transitive imports, call graph) |
| Natural language search | No | Yes (BM25 + optional embeddings) |
| Token budget management | No | Yes (auto-truncation) |

---

## Quick Start

```bash
# Index a codebase (BM25 only — fast, no dependencies)
codixing init .

# Search
codixing search "authentication handler"

# Symbol lookup
codixing symbols Engine

# Dependency graph
codixing callers src/engine.rs
codixing callees src/engine.rs

# Incremental sync (re-indexes only changed files)
codixing sync
```

---

## MCP Tools

49 tools across 7 categories (54 with federation):

| Category | Tools |
|----------|-------|
| **Search** | code_search, find_symbol, grep_code, search_usages, read_symbol, find_similar, stitch_context |
| **Graph** | get_repo_map, focus_map, get_references, get_transitive_deps, symbol_callers, symbol_callees, predict_impact, find_orphans, explain |
| **Files** | read_file, write_file, edit_file, delete_file, apply_patch, list_files, outline_file |
| **Analysis** | find_tests, find_source_for_test, get_complexity, review_context, rename_symbol, run_tests, get_context_for_task, check_staleness, generate_onboarding |
| **Git** | git_diff, get_hotspots, search_changes, get_blame |
| **Session** | remember, recall, forget, get_session_summary, session_status, session_reset_focus |
| **Meta** | index_status, search_tools, get_tool_schema, enrich_docs |

Full reference: [codixing.com/docs](https://codixing.com/docs)

### Daemon mode

Daemon mode loads the engine once and serves calls over a Unix socket (or named pipe on Windows) — **4-5x faster**.
The daemon auto-starts on first connection and self-terminates after 30 minutes idle:

```bash
codixing-mcp --root /path/to/project          # auto-starts daemon
codixing-mcp --root /path/to/project --daemon  # explicit daemon start
codixing-mcp --root /path/to/project --no-daemon-fork  # disable auto-start
```

The daemon auto-updates the index within ~100ms of any file save.

---

## LSP Server

`codixing-lsp` brings code intelligence to any LSP-capable editor — VS Code, Neovim, Emacs, Sublime Text, JetBrains.

**Capabilities:** Hover, Go-to-definition, References, Call hierarchy (incoming/outgoing), Workspace symbols, Document symbols, Live reindex on save, Cyclomatic complexity diagnostics, Code actions, Inlay hints, Completions, Signature help, Rename refactoring, Semantic tokens.

```bash
codixing-lsp --root /path/to/project
```

**Neovim:**
```lua
{ cmd = { "codixing-lsp", "--root", vim.fn.getcwd() } }
```

**Emacs (eglot):**
```elisp
(add-to-list 'eglot-server-programs
  '((rust-mode python-mode) . ("codixing-lsp" "--root" "/your/project")))
```

---

## VS Code / Cursor Extension

The `editors/vscode/` directory contains a TypeScript extension with: Index Workspace, Sync Index, Search, Show Repo Map, Start Daemon, Register MCP Server.

```bash
cd editors/vscode && npm install && npm run compile
# Then F5 in VS Code to launch the Extension Development Host
```

**Pre-built VSIX:** Download `codixing.vsix` from the [releases page](https://github.com/ferax564/codixing/releases) and install:

```bash
code --install-extension codixing.vsix
```

---

## Performance

| Metric | BM25-only | Hybrid (BgeSmallEn) |
|--------|-----------|---------------------|
| Init (138 files) | **0.21s** | 120s (one-time) |
| MCP cold start | **24ms** | 107ms |
| Search latency | 30-42ms | 36-40ms |
| Top-1 accuracy | 7/10 | **10/10** |

**Large codebase** (368K LoC, 7,607 files): Init 7.9s, search 94ms, 99% token reduction vs grep.

**SWE-bench Lite** (300 tasks, 12 repos): Recall@5 = 74.3% (vs grep 41.3%).

See [benchmarks/](benchmarks/) for detailed methodology and reproduction scripts.

---

## Key Features

- **24 languages** — Full AST parsing via tree-sitter (Rust, Python, TypeScript, Go, Java, C, C++, C#, Ruby, Swift, Kotlin, Scala, Zig, PHP, Bash, Matlab + config/diagram formats)
- **Hybrid search** — BM25 + optional vector embeddings, fused with Reciprocal Rank Fusion
- **Symbol-level call graph** — Function-to-function call edges extracted from AST, including Rust trait dispatch, Python class inheritance, and TypeScript interface implementations
- **Dependency graph** — Import + call extraction, PageRank scoring, Personalized PageRank for focus-aware maps
- **54 MCP tools** — Search, graph traversal, file operations, code review, git analysis, session memory, federation discovery
- **Daemon mode** — Engine stays in memory, auto-starts on first connection, Unix socket (macOS/Linux) or named pipe (Windows) IPC, file watcher for live index updates, 30-min idle timeout
- **Field-weighted BM25** — Configurable per-field boosting (entity_names 3×, signature 2×, scope_chain 1.5×, content 1×)
- **Search pipeline** — Composable search stages (definition boost, test demotion, path match, graph boost, recency boost, deduplication, truncation) with 6 strategies including trigram exact-match
- **Multi-query RRF fusion** — Auto-generates query reformulations for natural-language queries (3+ words) and fuses results via Reciprocal Rank Fusion; also available via explicit `queries` parameter on `code_search`
- **Git recency signal** — Mildly boosts recently modified files (+10% linear decay over 180 days) via lazy-loaded git log timestamps
- **Overlapping chunks** — Bridge chunks at AST-aware chunk boundaries capture cross-function context; configurable `overlap_ratio` (default 0.0)
- **File path boosting** — Detects explicit file paths and backtick code references in queries and boosts matching results (2.5×)
- **Kernel-scale performance** — Tested on the Linux kernel (73K files, 30M lines): 1.57s cold-start search, 0.79s warm. Mmap symbol table (zero-deserialization), compact chunk metadata (11× smaller), lazy trigram loading
- **Trigram pre-filtering** — File-level trigram inverted index (Russ Cox/trigrep technique) skips files before disk I/O; **110× faster** literal grep at 1K files, **52× faster** at 10K files; persistent bitcode storage, regex HIR walking with OR-branch support, parallel rayon verification
- **LSP rename + semantic tokens** — Cross-file rename refactoring with conflict detection; semantic highlighting for Rust, Python, TypeScript, Go
- **Streaming embeddings** — Fixed-window batch processing (256 chunks) with progress reporting; incremental vector reuse via content hashing
- **Federation auto-discovery** — Auto-detects Cargo, npm, pnpm, Go workspaces, git submodules, and nested projects
- **Read-only concurrent access** — Multiple instances share the same index; periodic reload detects writer updates automatically
- **Incremental embedding** — `sync` skips re-embedding unchanged chunks (content hash comparison)
- **Progress notifications** — Long-running MCP tools emit `notifications/progress` with streaming partial results so agents see live status
- **Windows support** — Named pipe daemon, brute-force vector fallback when usearch (POSIX-only) is unavailable
- **Dynamic tool discovery** — `--compact` mode emits `notifications/tools/list_changed` when new tools are used
- **GitHub Action** — Automated code review with impact analysis on PRs
- **Token budgets** — All output respects token limits; adaptive truncation at score cliffs
- **Cross-repo federation** — Unified search across multiple indexed projects with CLI management and workspace auto-discovery (`codixing federation init/add/remove/list/search/discover`)
- **HTTP API server** — REST endpoints (search, symbols, grep, hotspots, complexity, outline, graph) with SSE streaming (`crates/server/`)
- **Single binary** — No JVM, no Docker, no external databases, no API keys. macOS, Linux, and Windows

---

## Supported Languages

| Tier | Languages |
|------|-----------|
| **Tier 1** (full AST + graph) | Rust, Python, TypeScript, TSX, JavaScript, Go, Java, C, C++, C# |
| **Tier 2** (full AST + graph) | Ruby, Swift, Kotlin, Scala |
| **Tier 3** (full AST + graph) | Zig, PHP, Bash, Matlab |
| **Config** (symbol extraction) | YAML, TOML, Dockerfile, Makefile |
| **Diagram / Markup** (symbol extraction) | Mermaid, XML/Draw.io |

---

## Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                        Codixing Engine                            │
│                                                                   │
│  Tree-sitter  →  cAST Chunker  →  Tantivy (BM25)                │
│  AST Parser      (18 langs)       + Code Tokenizer               │
│                                                                   │
│  Symbol Table (DashMap)    Code Graph (petgraph + PageRank)      │
│                                                                   │
│  Retriever: BM25 · Hybrid (RRF) · Thorough (MMR) · Explore      │
│  + Exact (trigram) · Graph boost · Definition 3.5× · Session     │
│  SearchPipeline: composable stages, 6 strategies                  │
│                                                                   │
│  API: CLI · MCP (49+ tools, JSON-RPC 2.0) · LSP · HTTP Server   │
│       Daemon (Unix socket / Windows named pipe) · File Watcher   │
└──────────────────────────────────────────────────────────────────┘
```

---

## Development

```bash
cargo build --workspace
cargo test --workspace        # 834+ tests
cargo clippy --workspace -- -D warnings
cargo fmt --check
```

---

## License

Codixing Business Source License 1.0. Free for:
- Open-source projects
- Personal and educational use
- Teams of 5 or fewer developers

Commercial license required for larger teams. Contact [hello@codixing.com](mailto:hello@codixing.com).

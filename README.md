# Codixing

**Website: [codixing.com](https://codixing.com)** · **[Docs](https://codixing.com/docs)**

Ultra-fast code retrieval engine for AI agents — beats `grep` at its own game.

## Install

### Claude Code

```bash
# Plugin — includes MCP server + 3 slash commands (recommended)
claude plugin marketplace add ferax564/codixing
claude plugin install codixing@codixing
```

Restart Claude Code after installing. You get 48 MCP tools plus `/codixing-setup`, `/codixing-explore`, and `/codixing-review`.

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

> **Note:** Codex requires the binary installed locally — `npx` is not supported. Do not use `--compact` with Codex as it needs all 48 tools visible in the tool list.

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

48 tools across 7 categories:

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

Daemon mode loads the engine once and serves calls over a Unix socket — **4-5x faster**:

```bash
codixing-mcp --root /path/to/project --daemon &
```

The daemon auto-updates the index within ~100ms of any file save.

---

## LSP Server

`codixing-lsp` brings code intelligence to any LSP-capable editor — VS Code, Neovim, Emacs, Sublime Text, JetBrains.

**Capabilities:** Hover, Go-to-definition, References, Workspace symbols, Document symbols, Live reindex on save, Cyclomatic complexity diagnostics.

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

- **20 languages** — Full AST parsing via tree-sitter (Rust, Python, TypeScript, Go, Java, C, C++, C#, Ruby, Swift, Kotlin, Scala, Zig, PHP + config formats)
- **Hybrid search** — BM25 + optional vector embeddings, fused with Reciprocal Rank Fusion
- **Dependency graph** — Import + call extraction, PageRank scoring, Personalized PageRank for focus-aware maps
- **48 MCP tools** — Search, graph traversal, file operations, code review, git analysis, session memory
- **Daemon mode** — Engine stays in memory, Unix socket IPC, file watcher for live index updates
- **Token budgets** — All output respects token limits; adaptive truncation at score cliffs
- **Cross-repo federation** — Unified search across multiple indexed projects
- **Single binary** — No JVM, no Docker, no external databases, no API keys

---

## Supported Languages

| Tier | Languages |
|------|-----------|
| **Tier 1** (full AST + graph) | Rust, Python, TypeScript, TSX, JavaScript, Go, Java, C, C++, C# |
| **Tier 2** (full AST + graph) | Ruby, Swift, Kotlin, Scala |
| **Tier 3** (full AST + graph) | Zig, PHP |
| **Config** (symbol extraction) | YAML, TOML, Dockerfile, Makefile |

---

## Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                        Codixing Engine                            │
│                                                                   │
│  Tree-sitter  →  cAST Chunker  →  Tantivy (BM25)                │
│  AST Parser      (16 langs)       + Code Tokenizer               │
│                                                                   │
│  Symbol Table (DashMap)    Code Graph (petgraph + PageRank)      │
│                                                                   │
│  Retriever: BM25 · Hybrid (RRF) · Thorough (MMR) · Explore      │
│  + Graph boost · Definition 3.5× · Session boost                 │
│                                                                   │
│  API: CLI · MCP (48 tools, JSON-RPC 2.0) · LSP Server           │
│       Daemon (Unix socket) · File Watcher                        │
└──────────────────────────────────────────────────────────────────┘
```

---

## Development

```bash
cargo build --workspace
cargo test --workspace        # 625 tests
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

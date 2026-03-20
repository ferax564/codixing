# Codixing

**Website: [codixing.com](https://codixing.com)** · **[Docs](https://codixing.com/docs)**

Ultra-fast code retrieval engine for AI agents — beats `grep` at its own game.

## Install

```sh
curl -fsSL https://codixing.com/install.sh | sh
```

Installs `codixing`, `codixing-mcp`, and `codixing-lsp` to `/usr/local/bin`. macOS (Apple Silicon) and Linux (x86_64). Binaries also available on the [releases page](https://github.com/ferax564/codixing/releases).

## Why Not Just Grep?

AI coding agents currently use `grep`, `find`, and `cat` for code navigation. These tools return **everything, always** — a single `rg b2Vec2` on a real codebase returns 2,240 hits (225 KB), burning context before any reasoning happens.

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

## MCP Integration

Codixing exposes 48 tools via the [Model Context Protocol](https://modelcontextprotocol.io) — any MCP-compatible client picks them up automatically.

### Claude Code — Plugin (recommended)

```bash
claude plugin install codixing
```

Installs the MCP server plus three slash commands:

| Command | What it does |
|---------|-------------|
| `/codixing-setup` | Index the current project and register the MCP server |
| `/codixing-explore` | Deep architecture overview — PageRank-sorted modules, dependencies, key symbols |
| `/codixing-review` | Code review with impact analysis, caller tracking, and test coverage |

### Claude Code — MCP only

```bash
claude mcp add codixing -- npx -y codixing-mcp --root .
```

### Cursor / Windsurf / other MCP clients

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

### Daemon mode

Daemon mode loads the engine once and serves all calls over a Unix socket — **4-5x faster** for cheap operations:

```bash
codixing-mcp --root /path/to/project --daemon &
```

The daemon auto-updates the index within ~100ms of any file save.

### MCP tools

48 tools across 7 categories: **Search** (code_search, find_symbol, grep_code, explain, stitch_context), **Graph** (get_repo_map, symbol_callers, symbol_callees, predict_impact, find_orphans), **Files** (read_file, write_file, edit_file, apply_patch), **Analysis** (find_tests, get_complexity, review_context, rename_symbol), **Git** (git_diff, get_hotspots, get_blame), **Session** (remember, recall, session_status), **Meta** (index_status, search_tools).

Full reference: [codixing.com/docs](https://codixing.com/docs)

---

## LSP Server

`codixing-lsp` brings code intelligence to any LSP-capable editor — VS Code, Neovim, Emacs, Sublime Text, JetBrains.

**Capabilities:** Hover, Go-to-definition, References, Workspace symbols, Document symbols, Live reindex on save, Cyclomatic complexity diagnostics.

```bash
codixing-lsp --root /path/to/project

# Neovim
{ cmd = { "codixing-lsp", "--root", vim.fn.getcwd() } }

# Emacs (eglot)
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

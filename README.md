# Codixing

[![CI](https://github.com/ferax564/codixing/actions/workflows/ci.yml/badge.svg)](https://github.com/ferax564/codixing/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/ferax564/codixing/graph/badge.svg)](https://codecov.io/gh/ferax564/codixing)

**Website: [codixing.com](https://codixing.com)** · **[Docs](https://codixing.com/docs)**

Code retrieval engine that saves your AI agent 73% of its token budget. Replaces grep with ranked, AST-aware search — so models spend tokens reasoning, not reading.

## Install

```sh
curl -fsSL https://codixing.com/install.sh | sh
```

Installs `codixing` to `/usr/local/bin`. macOS (Apple Silicon) and Linux (x86_64). Binaries also on the [releases page](https://github.com/ferax564/codixing/releases).

### Claude Code plugin (optional)

```bash
claude plugin marketplace add ferax564/codixing
claude plugin install codixing@codixing
```

Adds 5 slash commands: `/codixing-setup`, `/codixing-explore`, `/codixing-review`, `/codixing-preflight`, `/codixing-release`.

### MCP server (optional — for Cursor, Windsurf, Continue.dev, Codex)

Add to your project's `.mcp.json`:

```json
{
  "mcpServers": {
    "codixing": {
      "command": "npx",
      "args": ["-y", "codixing-mcp", "--root", ".", "--medium", "--no-daemon-fork"]
    }
  }
}
```

Or for OpenAI Codex CLI: `codex mcp add codixing -- codixing-mcp --root .`

---

## Why Not Just Grep?

AI coding agents use `grep`, `find`, and `cat` for code navigation. These tools return **everything, always** — a single `rg b2Vec2` on a real codebase returns 2,240 hits (225 KB), burning context before any reasoning happens.

Codixing returns the top 20 results in 1.3 KB — same signal, **99% less waste**.

### The cost of noise

Tested on 6 real-world repos (tokio, ripgrep, axum, django, fastapi, react — 9,493 files):

| Metric | grep/cat/find | Codixing | Savings |
|--------|---------------|----------|---------|
| Tool calls per session | 58 | 26 | **55% fewer** |
| Output tokens | ~84,600 | ~22,900 | **73% fewer** |
| Est. cost (Opus @ $15/M) | $1.27 | $0.34 | **$0.93/session** |

At 50 agent sessions/day, that's **$1,400/month** back in your pocket — and the agent finds the right code more often.

### What you get

| Capability | grep/rg | Codixing |
|-----------|---------|----------|
| Bounded, ranked output | No | Yes (BM25 + PageRank) |
| Symbol definitions (not just mentions) | No | Yes (AST-parsed symbol table) |
| Dependency graph queries | No | Yes (transitive imports, call graph) |
| Natural language search | No | Yes (BM25 + optional embeddings) |
| Token budget management | No | Yes (auto-truncation) |

---

## Getting Started

### 60-second setup

```bash
# 1. Install
curl -fsSL https://codixing.com/install.sh | sh

# 2. Index your project
codixing init .
# ✓ Indexed 2,847 files, 14,203 chunks, 8,891 symbols in 1.2s

# 3. Search
codixing search "authentication handler"
# ► src/auth/handler.rs:42  [score: 0.94]
#   pub fn handle_auth_request(req: Request) -> Result<Token>
```

That's it. Your agent now uses ranked search instead of grep.

### CLI commands

```bash
# Search (natural language or symbol names)
codixing search "error handling middleware"

# Symbol lookup (definitions only, not mentions)
codixing symbols Engine

# Dependency graph
codixing callers src/engine.rs    # who imports this file?
codixing callees src/engine.rs    # what does this file import?

# Keep index fresh (re-indexes only changed files)
codixing sync

# Architecture map
codixing graph --token-budget 4000
```

### Hybrid search (optional)

BM25-only works great for most queries. For natural-language queries ("how does the auth flow work?"), add semantic embeddings:

```bash
codixing init . --model bge-small-en    # one-time, ~2 min
codixing search "how does auth work" --strategy fast
```

Requires ONNX Runtime (`pip install onnxruntime` or download from GitHub).

---

## CLI Commands

19 commands for code intelligence:

```bash
codixing search "query"          # Semantic code search
codixing symbols Widget          # Find symbol definitions
codixing usages add_chunk        # Find call sites and imports
codixing callers src/engine.rs   # Who imports this file
codixing callees src/engine.rs   # What this file imports
codixing graph --map             # Architecture overview
codixing init .                  # Index a project
codixing sync                    # Incremental re-index
codixing audit                   # Find stale files
```

Full reference: [codixing.com/docs](https://codixing.com/docs)

### MCP server (optional)

For editors with MCP support, the `codixing-mcp` binary exposes 57 JSON-RPC 2.0 tools:

| Category | Tools |
|----------|-------|
| **Search** | code_search, find_symbol, grep_code, search_usages, read_symbol, find_similar, stitch_context |
| **Graph** | get_repo_map, focus_map, get_references, get_transitive_deps, symbol_callers, symbol_callees, predict_impact, find_orphans, explain |
| **Files** | read_file, write_file, edit_file, delete_file, apply_patch, list_files, outline_file |
| **Analysis** | find_tests, find_source_for_test, get_complexity, review_context, rename_symbol, run_tests, get_context_for_task, check_staleness, generate_onboarding, audit_freshness |
| **Git** | git_diff, get_hotspots, search_changes, get_blame |
| **Session** | remember, recall, forget, get_session_summary, session_status, session_reset_focus |
| **Meta** | index_status, search_tools, get_tool_schema, enrich_docs |

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

**Retrieval accuracy** (OpenClaw, 9.4K files): Codixing R@10 = **0.763** vs grep R@10 = 0.345 — **2× better retrieval** (MRR = 0.706).

**Large codebase** (368K LoC, 7,607 files): Init 7.9s, search 94ms, 99% token reduction vs grep.

**Linux kernel** (73K files, 30M lines): 1.57s cold-start search, 0.79s warm. Zero-deserialization mmap for instant startup.

**SWE-bench Lite** (300 tasks, 12 repos): Recall@5 = 74.3% (vs grep 41.3%).

See [benchmarks/](benchmarks/) for detailed methodology and reproduction scripts.

---

## Key Features

- **26 languages** — Full AST parsing via tree-sitter (Rust, Python, TypeScript, Go, Java, C, C++, C#, Ruby, Swift, Kotlin, Scala, Zig, PHP, Bash, Matlab + config/diagram formats + Markdown and HTML doc indexing)
- **Documentation indexing** — indexes Markdown and HTML docs alongside code with section-aware chunking, breadcrumb metadata, and doc-to-code graph linking; use `--docs-only` to restrict results to docs or `--code-only` to exclude them
- **Hybrid search** — BM25 + optional vector embeddings, fused with Reciprocal Rank Fusion
- **Symbol-level call graph** — Function-to-function call edges extracted from AST, including Rust trait dispatch, Python class inheritance, and TypeScript interface implementations
- **Dependency graph** — Import + call extraction, PageRank scoring, Personalized PageRank for focus-aware maps
- **Ranked cross-imports** — PageRank + git recency scoring for relevance-ranked graph queries across directory boundaries
- **Memory relations** — `memory_relate` tool creates typed edges between agent memory entries, enabling associative recall across sessions
- **Feature hub** — One-call feature exploration combining search + callers + callees + tests for unified understanding
- **CLI + MCP** — 19 CLI commands for direct use; 57 MCP tools for editor integration (search, graph traversal, file operations, code review, git analysis, session memory, federation discovery)
- **File freshness audit** — `audit_freshness` tool identifies stale and orphaned files across releases
- **Preflight gates** — Plugin enforces existence scanning before proposing new features
- **TypeScript import resolution** — Resolve `.js` → `.ts` imports with node16/bundler moduleResolution support, enabling 0.8+ R@10 on cross-package code discovery
- **Background embedding drain** — Instant BM25 search after `codixing init`, hybrid vector search transparently upgrades as embeddings complete in the background
- **Model2Vec with code-aware preprocessing** — Static embeddings via `potion-base-8M` (no ONNX needed, instant init). CamelCase/snake_case splitting before tokenization reduces subword fragments by 50-70%, achieving MRR 1.000 on concept queries
- **Jina Code Int8** — `jina-embeddings-v2-base-code` int8-quantized for ARM64 (768 dims, 8ms/query, nDCG@10 0.949). Set `JINA_CODE_INT8_ONNX` env var to the model path
- **Embedding speed measurement** — New `bench-embed` CLI subcommand for profiling embedding performance across custom models
- **Daemon mode** — Engine stays in memory, auto-starts on first connection, Unix socket (macOS/Linux) or named pipe (Windows) IPC, file watcher for live index updates, 30-min idle timeout
- **Field-weighted BM25** — Configurable per-field boosting (entity_names 3×, signature 2×, scope_chain 1.5×, content 1×)
- **Search pipeline** — Composable search stages (definition boost, test demotion, path match, graph boost, recency boost, graph semantic propagation via GraphPropagationStage, file-level dedup via FileDedupStage, truncation) with 6 strategies including trigram exact-match
- **Multi-query RRF fusion** — Auto-generates query reformulations for natural-language queries (3+ words) and fuses results via Reciprocal Rank Fusion; also available via explicit `queries` parameter on `code_search`
- **Git recency signal** — Mildly boosts recently modified files (+10% linear decay over 180 days) via lazy-loaded git log timestamps
- **Overlapping chunks** — Bridge chunks at AST-aware chunk boundaries capture cross-function context; configurable `overlap_ratio` (default 0.0)
- **File path boosting** — Detects explicit file paths and backtick code references in queries and boosts matching results (2.5×)
- **Kernel-scale performance** — Tested on the Linux kernel (73K files, 30M lines): 1.57s cold-start search, 0.79s warm. Mmap symbol table AND trigram index (zero-deserialization), compact chunk metadata (11× smaller), lazy trigram loading
- **Trigram pre-filtering** — File-level trigram inverted index (Russ Cox/trigrep technique) skips files before disk I/O; **110× faster** literal grep at 1K files, **52× faster** at 10K files; persistent bitcode storage, regex HIR walking with OR-branch support, parallel rayon verification
- **LSP rename + semantic tokens** — Cross-file rename refactoring with conflict detection; semantic highlighting for Rust, Python, TypeScript, Go
- **Queue-based embedding** — Optional RustQueue-backed pipeline with crash recovery, parallel ONNX workers (N× throughput), deferred embedding (`--defer-embeddings`), and streaming mpsc pattern that fixes OOM on large repos
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
- **Cross-package import graph** — `cross-imports` command finds files in one directory that import from another via single O(E) graph walk
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
| **Docs** (section-aware chunking) | Markdown, HTML |

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
│  API: CLI (19 cmds) · MCP (57 tools, JSON-RPC 2.0) · LSP · HTTP  │
│       Daemon (Unix socket / Windows named pipe) · File Watcher   │
└──────────────────────────────────────────────────────────────────┘
```

---

## Development

```bash
cargo build --workspace
cargo test --workspace        # 931+ tests
cargo clippy --workspace -- -D warnings
cargo fmt --check
```

---

## License

Licensed under the [Apache License, Version 2.0](LICENSE). See [LICENSE](LICENSE) for the full text.

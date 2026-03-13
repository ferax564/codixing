# Codixing

**Website: [codixing.com](https://codixing.com)** · **[Docs](https://codixing.com/docs)**

Ultra-fast code retrieval engine for AI agents — beats `grep` at its own game.

## Install

```sh
curl -fsSL https://codixing.com/install.sh | sh
```

Installs `codixing`, `codixing-mcp`, and `codixing-server` to `/usr/local/bin`. macOS and Linux only. Windows binaries are on the [releases page](https://github.com/ferax564/codixing/releases).

Codixing is a Rust-native engine that gives AI coding agents precisely the right context from any codebase, regardless of size. It combines structure-aware AST parsing (tree-sitter), hybrid search (BM25 + vector), a live code dependency graph with PageRank scoring, and AI-optimized token-budgeted output into a single, blazing-fast binary.

## Why Not Just Grep?

Claude Code and similar agents currently use `grep`, `find`, and `cat` for code navigation. These tools are fast, but they have a fundamental problem: **they return everything, always**. A single `rg b2Vec2` on a real C++ game codebase returns 2,240 hits — 225,343 bytes — burning context before any reasoning happens.

Codixing solves this with three properties grep cannot replicate:

1. **Bounded output** — `limit=20` caps results so context never overflows
2. **Structural awareness** — finds where a symbol is *defined*, not just where it appears
3. **Graph intelligence** — answers "who imports this file?" and "what does changing this break?" transitively

---

## Benchmark: Codixing Daemon vs Native Shell Tools

Measured on [OpenClaw](https://github.com/pjasicek/OpenClaw) — 246,000 lines of C++, 770 files. Codixing running in daemon mode (engine pre-loaded, Unix socket IPC ~6ms overhead).

| Operation | Native tool | Native | Codixing | Speed | Tokens |
|-----------|------------|-------:|----------:|------:|-------:|
| Literal search | `rg` (all hits) | 23ms | 24ms | ≈ same | **−61%** |
| Regex + file filter (4,102 hits) | `rg --type cpp` | 18ms | 10ms | **1.8×** | **−99%** |
| High-freq pattern (2,240 hits) | `rg` (unbounded) | 20ms | 7ms | **2.9×** | **−99%** |
| Find class definition | `rg -n 'class ...'` | 16ms | 8ms | **1.9×** | structured |
| Read large file | `cat file` (full) | 3ms | 6ms | −1.8× | **−91%** |
| Reverse dependency lookup | `rg -rl` | 13ms | 7ms | **1.8×** | **−99%** |
| Transitive dep chain (depth 2) | manual multi-hop grep | n/a | 7ms | structural | −66% |
| Architecture overview | `find + wc -l \| sort` | n/a | 109ms | PageRank-sorted | structural |
| Semantic / conceptual search | keyword-guessing grep | n/a | 38ms | **natural language** | structured |

> **The b2Vec2 case is the decisive number.** Raw `rg b2Vec2` returns 225,343 bytes (2,240 lines) — Codixing returns the top 20 in 1,332 bytes. Same signal, **99% less waste**, band-merged by adjacent-chunk deduplication.

### What grep cannot do at all

- PageRank-ranked architecture map (importance ≠ file size)
- Transitive import graph at arbitrary depth
- Semantic / conceptual search (BM25 understands intent, not just strings)
- Automatic token budget management (grep overflows; Codixing caps)
- Symbol-table lookup (definition vs. every mention)

---

## Quick Start

```bash
# Build from source
cargo build --release --workspace

# Index a codebase (BM25 only — fast, no GPU needed)
./target/release/codixing init . --no-embeddings

# Or with semantic search (BGE-Base-EN-v1.5, local ONNX inference)
./target/release/codixing init .

# Search
codixing search "authentication handler"
codixing search "parse config" --strategy thorough

# Symbol lookup
codixing symbols Engine
codixing symbols --file src/main.rs

# Dependency graph
codixing callers src/engine.rs          # who imports this?
codixing callees src/engine.rs          # what does this import?
codixing dependencies src/main.rs --depth 2

# Multi-repo: index a second codebase alongside the primary
codixing init . --also ../shared-lib --also ../api-server

# Incremental sync (re-indexes only changed files)
codixing sync
```

### Graph Atlas Viewer

Start the HTTP server to inspect the live graph atlas in a browser:

```bash
# Serve the atlas UI and graph endpoints
codixing-server --host 127.0.0.1 --port 3000 /path/to/project

# Then open:
# http://127.0.0.1:3000/graph/view
```

The atlas opens at subsystem scale (cluster nodes) and lets you drill into individual files via the `Topology Groups` panel. Recent git commits are overlaid on the same graph. Controls in the left panel:

| Control | Description |
|---|---|
| **Refresh mode** | `sync` (filesystem), `git` (commits only), or `manual` |
| **Include external** | Toggle external library nodes on/off |
| **Polling** | Auto-refresh every N seconds |
| **Poll interval** | 5 s / 15 s / 60 s |
| **3D mode** | Toggle between 3D orbit and flat 2D layout |
| **Call Graph Layer** | Overlay symbol-level call edges (cyan) on the import graph |
| **Search** | Jump to a file or symbol by name |
| **Edge filter** | Show all / imports only / calls only / external only |

The viewer reads from:

- `GET /graph/view` — browser UI
- `GET /graph/export?refresh=none|sync|git&include_external=true&symbol_limit=4` — graph snapshot with clusters, edges, and symbol previews (auto-raises to `symbol_limit=12` when the Call Graph Layer is active)
- `GET /graph/history?limit=18&include_files=true` — recent commits and touched indexed files
- `GET /graph/call-graph` — symbol-level call edges (requires graph support at index time)

---

## Claude Code Integration (MCP)

Codixing exposes all its tools via the [Model Context Protocol](https://modelcontextprotocol.io) — Claude Code picks them up automatically.

### Register once

```bash
claude mcp add --scope user --transport stdio codixing \
  -- /path/to/codixing-mcp --root /path/to/your/project
```

Or edit `~/.claude.json` directly:

```json
{
  "mcpServers": {
    "codixing": {
      "type": "stdio",
      "command": "/path/to/codixing-mcp",
      "args": ["--root", "/path/to/your/project"]
    }
  }
}
```

### Daemon mode (recommended)

Normal mode spawns a new process per call (~30ms cold start). Daemon mode loads the engine once and serves all calls over a Unix socket (~6ms IPC overhead) — **4–5× faster for cheap operations**.

```bash
# Start daemon (keeps running, auto-updates index on file saves)
codixing-mcp --root /path/to/project --daemon &

# All subsequent codixing-mcp calls auto-proxy through the daemon
```

The daemon runs a background file watcher. When you save a file, the index updates within ~100ms. Claude Code always queries a fresh index.

### Session tracking

The MCP server tracks agent interactions (file reads, edits, symbol lookups, searches) and uses them to boost search relevance. Recently-touched files rank higher, and graph neighbors of active files receive propagated boosts. After 5+ interactions in the same directory, **progressive focus** automatically narrows search results.

```bash
# Disable session tracking (for benchmarking or privacy)
codixing-mcp --root /path/to/project --no-session
```

### Available MCP tools (34)

| Tool | What it does |
|------|-------------|
| `code_search` | BM25 + graph-boosted search; `instant`/`fast`/`thorough`/`explore` strategies (results cached 60s) |
| `grep_code` | Regex or literal search across indexed files; bounded output, glob filter, context lines |
| `find_symbol` | Structured symbol lookup — returns definition location + signature |
| `read_symbol` | Full source of a named symbol |
| `read_file` | Token-budgeted file reader with line range |
| `outline_file` | Per-file symbol tree sorted by line number — token-efficient alternative to `read_file` |
| `get_repo_map` | PageRank-ranked architecture overview within a token budget |
| `get_references` | Who imports a file (callers) + what it imports (callees) |
| `get_transitive_deps` | Multi-hop dependency chain to arbitrary depth |
| `search_usages` | All usage sites of a symbol across the codebase |
| `index_status` | Current index statistics (files, chunks, symbols, graph, session) |
| `list_files` | List all indexed files with optional glob filter and chunk counts |
| `write_file` | Write a file and immediately reindex it |
| `edit_file` | Exact find-and-replace in a file with immediate reindex |
| `delete_file` | Delete a file and remove it from the index |
| `apply_patch` | Apply a unified git diff across one or more files with auto-reindex |
| `run_tests` | Execute a test command in the project root and return stdout + exit code |
| `rename_symbol` | Project-wide identifier rename with auto-reindex of all affected files |
| `explain` | Assembled context package: definition + file deps + usage sites + session context for any symbol |
| `symbol_callers` | Symbol-level call graph: which functions directly call a given symbol |
| `symbol_callees` | Symbol-level call graph: which functions a given symbol directly calls |
| `predict_impact` | Given a unified diff, rank files most likely to need changes (call graph + import graph) |
| `stitch_context` | Search + automatically attach callee definitions for cross-file context in one call |
| `enrich_docs` | LLM-generated doc summaries per symbol, stored in `.codixing/symbol_docs.json` (Anthropic or Ollama) |
| `remember` | Store a persistent key/value note in `.codixing/memory.json` — survives restarts |
| `recall` | Retrieve stored memory entries by keyword or tag filter |
| `forget` | Remove a memory entry by key |
| `find_tests` | Discover test functions across languages by naming conventions and annotations |
| `find_similar` | Find semantically similar code chunks using vector embeddings or BM25 fallback |
| `get_complexity` | Cyclomatic complexity (McCabe) per function — risk-banded table sorted by complexity |
| `review_context` | Given a git diff: changed symbols, impact prediction, and cross-file context for code review |
| `generate_onboarding` | Assemble index stats, language breakdown, and PageRank repo map into `.codixing/ONBOARDING.md` |
| `get_session_summary` | Structured summary of current session: files read/edited, symbols explored, grouped by directory |
| `session_reset_focus` | Clear progressive focus that narrows search to the most-interacted directory |

---

## LSP Server

`codixing-lsp` implements the Language Server Protocol, bringing Codixing's symbol search to **any LSP-capable editor** — Neovim, Emacs, Sublime Text, JetBrains, and more.

**Capabilities**: `workspace/symbol` (global search) · `textDocument/documentSymbol` (per-file outline)

```bash
# Neovim (lspconfig / lazy.nvim)
{
  cmd = { "codixing-lsp", "--root", vim.fn.getcwd() },
  filetypes = { "rust", "python", "typescript", "go", "java", "php", "zig" },
  root_dir = require("lspconfig.util").root_pattern(".codixing"),
}

# Emacs (eglot)
(add-to-list 'eglot-server-programs
  '((rust-mode python-mode) . ("codixing-lsp" "--root" "/your/project")))
```

---

## VS Code / Cursor Extension

The `editors/vscode/` directory contains a TypeScript extension that integrates Codixing directly into your editor.

**Commands** (`Ctrl+Shift+P` / `Cmd+Shift+P`):

| Command | What it does |
|---------|-------------|
| `Codixing: Index Workspace` | Build or rebuild the index for the current project |
| `Codixing: Sync Index` | Re-index only changed files since last sync |
| `Codixing: Search` | Interactive code search with inline results |
| `Codixing: Show Repo Map` | Display PageRank-sorted architecture overview |
| `Codixing: Start Daemon` | Launch the daemon for faster subsequent MCP calls |
| `Codixing: Register MCP Server` | Add codixing to `~/.claude.json` and `~/.cursor/mcp.json` |

**Status bar** shows `$(check) indexed` when a `.codixing/` index exists, `$(circle-slash) not indexed` otherwise.

**Install from source:**
```bash
cd editors/vscode
npm install
npm run compile
# Then F5 in VS Code to launch the Extension Development Host
```

---

## Performance

All numbers measured on [OpenClaw](https://github.com/pjasicek/OpenClaw) — a real C++ game engine, 246K lines across 770 files.

| Metric | Result |
|--------|--------|
| **Init speed (BM25 + graph)** | **0.87s** for 246K LoC / 770 files |
| **Init speed (with BGE-Base embeddings)** | ~25s (ONNX inference dominates; one-time cost) |
| **Incremental reindex (single file)** | <150ms |
| **Batch reindex (N files, e.g. after git pull)** | Single PageRank pass — N× faster than N individual reindexes |
| **File watcher latency** | ≤100ms from save to queryable |
| **Daemon IPC overhead** | ~6ms per call (Unix socket round-trip) |
| **BM25 search** | <10ms p99 |
| **Test suite** | 260 tests (including retrieval quality regression suite) |

### Init speed breakdown (0.87s on 246K LoC)

| Stage | Time | Notes |
|-------|------|-------|
| File discovery | ~5ms | Directory walk, 770 files |
| Parse + chunk + BM25 index | ~600ms | rayon parallel, all CPU cores |
| Graph build (imports + PageRank) | ~200ms | Parallel resolution, single sequential insert pass |
| Persist to `.codixing/` | ~50ms | bitcode + Tantivy flush |

> **Why it's fast:** `build_graph()` reuses the import lists extracted during the parallel parse phase — no second file read, no second tree-sitter parse. Files are parsed exactly once.

---

## Embedding Model Selection

Numbers measured on this repository (86 files, 667 chunks, AMD Rembrandt CPU).

| Model | Dims | Init time | Cold query | MRR@10 | Recall@4 | Notes |
|-------|------|-----------|------------|--------|----------|-------|
| **BM25-only** | — | **<1s** | **12ms** | **0.750** | **100%** | Default; no ONNX required |
| BgeSmall-EN-v1.5 | 384 | 73s | 376ms¹ | 0.592 | 90% | Daemon query ~1ms |
| BgeBase-EN-v1.5 | 768 | 186s | 7ms | 0.592 | 90% | 2.5× slower init, same quality |
| BgeLarge-EN-v1.5 | 1024 | 418s | 44ms | 0.767 | 100% | Quality matches Arctic/Qwen3 |
| Snowflake-Arctic-L | 1024 | 428s | 44ms | 0.767 | 100% | SOTA MTEB at 335M params |
| Qwen3-0.6B | 1024 | 580s | 8ms | 0.767 | 100% | Best quality; needs `--features qwen3` |

¹ First cold process loads the ONNX model; daemon mode reduces this to ~1ms.

### How to choose

| Situation | Recommendation |
|-----------|----------------|
| Good identifiers and docstrings | **BM25-only** (default) — fast, no GPU/ONNX, retrieval quality matches or beats small embedders |
| Natural-language queries matter | **BgeLarge** or **Snowflake-Arctic-L** — 0.767 MRR, 100% recall; 7-minute one-time init |
| Fast init + some semantic search | **BgeSmall** — 73s init, run as daemon to eliminate cold-start |
| Maximum quality, no init budget | **Qwen3** — same MRR as BgeLarge but smaller binary; requires `qwen3` Cargo feature |

```bash
# BM25-only (default — recommended for most codebases)
codixing init .

# Enable embeddings
codixing init . --model bge-small-en
codixing init . --model bge-large-en
codixing init . --model snowflake-arctic-l
```

---

## Key Features

- **AST-aware chunking** — Tree-sitter parsing across 10 language families; never splits a function in half
- **BM25 full-text search** — Tantivy-backed with a custom code tokenizer; `signature` field ×3.0 and `entity_names` ×2.0 field boosts ensure definitions rank above mentions
- **Hybrid retrieval** — BM25 + vector (fastembed BGE-Base-EN-v1.5, 768 dims) fused with asymmetric Reciprocal Rank Fusion; identifier queries route BM25-dominant, natural language routes vector-dominant
- **Code dependency graph** — Import + call extraction for all 10 languages, petgraph `DiGraph`, PageRank scoring; transparently boosts search result ranking
- **Band merging** — Adjacent same-file result chunks within 3 lines are merged before rendering; reduces token output by 25–91% on typical codebases
- **Repo map generation** — Aider-style, token-budgeted output sorted by PageRank (importance) not file size
- **Live index freshness** — Daemon file watcher updates the in-memory engine within 100ms of any file save; no restart needed
- **`.gitignore`-aware indexing** — File walker respects `.gitignore`, `.ignore`, and global gitignore (same as ripgrep); no manual exclude lists needed
- **Hash-based incremental sync** — `codixing sync` diffs xxh3 content hashes and re-indexes only changed files; no git required
- **MCP server** — 34 tools exposed via JSON-RPC 2.0; Claude Code registers with one command
- **Session-aware retrieval** — Tracks agent interactions (reads, edits, searches) and boosts recently-touched files in search results with graph-propagated context
- **Concurrent symbol table** — DashMap-backed with exact, prefix, and substring matching
- **Single binary, zero runtime deps** — No JVM, no Docker, no external databases

---

## Supported Languages

| Tier | Languages |
|------|-----------|
| **Tier 1** (full AST + graph) | Rust, Python, TypeScript, TSX, JavaScript, Go, Java, C, C++, C# |
| **Tier 2** (full AST + graph) | Ruby, Swift, Kotlin, Scala |
| **Tier 3** (full AST + graph) | Zig, PHP |

---

## Architecture

```
┌────────────────────────────────────────────────────────────────────────┐
│                           Codixing Engine                              │
│                                                                         │
│  ┌─────────────┐  ┌──────────┐  ┌──────────────┐  ┌─────────────────┐  │
│  │ Tree-sitter  │  │ Tantivy  │  │    Symbol    │  │   Code Graph    │  │
│  │ AST Parser   │  │  (BM25)  │  │    Table     │  │  (petgraph)     │  │
│  └──────┬───────┘  └────┬─────┘  └──────┬───────┘  └───────┬─────────┘  │
│         │               │               │                   │            │
│  ┌──────▼───────┐  ┌────▼──────┐  ┌─────▼──────┐  ┌────────▼─────────┐  │
│  │     cAST     │  │   Code    │  │  DashMap   │  │ ImportExtractor  │  │
│  │   Chunker    │  │ Tokenizer │  │  (conc.)   │  │   + PageRank     │  │
│  └──────────────┘  └────┬──────┘  └────────────┘  └────────┬─────────┘  │
│                          │                                  │            │
│  ┌───────────────────────▼──────────────────────────────────▼──────────┐ │
│  │      Retriever: BM25 · Hybrid (RRF) · Thorough (MMR) · Explore     │ │
│  │                   + Graph PageRank score boost                      │ │
│  └──────────────────────────────────────────────────────────────────── ┘ │
│                                                                         │
│  ┌─────────────────────────────────────────────────────────────────────┐ │
│  │   API Layer: CLI (clap) · REST (axum) · MCP (JSON-RPC 2.0)         │ │
│  │              + Daemon (Unix socket) · File Watcher                  │ │
│  └─────────────────────────────────────────────────────────────────────┘ │
└────────────────────────────────────────────────────────────────────────┘
```

---

## Retrieval Strategies

| Strategy | Method | Graph boost | Latency |
|----------|--------|-------------|---------|
| `instant` | BM25 only | No | <10ms |
| `fast` | BM25 + vector (RRF) | Yes | <50ms |
| `thorough` | Hybrid + MMR dedup | Yes | <200ms |
| `explore` | BM25 + graph neighbor expansion | Yes | <100ms |

---

## Tech Stack

| Component | Crate | Purpose |
|-----------|-------|---------|
| AST Parsing | `tree-sitter` 0.26 | Incremental, multi-language parsing |
| Full-text search | `tantivy` 0.22 | BM25 scoring, inverted index |
| Vector embeddings | `fastembed` 5 | BGE-Base-EN-v1.5 (768d), local ONNX |
| Vector index | `usearch` 2 | HNSW approximate nearest-neighbour + int8 quantization |
| Vector index (Qdrant) | `qdrant-client` 1 | Optional distributed backend; enable with `--features qdrant` |
| Code graph | `petgraph` 0.8 | `DiGraph` + PageRank |
| Token counting | `tiktoken-rs` 0.9 | cl100k_base budget enforcement |
| HTTP server | `axum` 0.8 | Async REST API |
| Symbol table | `dashmap` 6 | Lock-free concurrent hash map |
| Parallelism | `rayon` 1 | Parallel file processing |
| File watching | `notify` 8 | Cross-platform fs event monitoring |
| Serialization | `bitcode` 0.6 | Fast binary persistence |
| Content hashing | `xxhash-rust` 0.8 | Change detection (xxh3) |
| IPC | tokio `UnixListener` | Daemon socket server |
| CLI | `clap` 4 | Command-line interface |
| Logging | `tracing` 0.1 | Structured logging (stderr only in MCP mode) |

---

## Roadmap

| Phase | Status | Highlights |
|-------|--------|-----------|
| **Phase 1: Foundation** | ✅ Complete | AST parsing, BM25, CLI, file watcher — 111 tests |
| **Phase 2: Semantic Search** | ✅ Complete | BGE-Base embeddings, hybrid RRF+MMR, REST API |
| **Phase 3: Graph Intelligence** | ✅ Complete | Import graph, PageRank, repo map — 165 tests |
| **Phase 4: Agent Integration** | ✅ Complete | MCP (24 tools), daemon mode, 2.6× faster init, live watcher — 222 tests |
| **Phase 5: Production Hardening** | ✅ Complete | Field boosts, band merging, asymmetric RRF, call graph edges, sync, .gitignore walker — 232 tests |
| **Phase 6: Ecosystem Expansion** | ✅ Complete | Tier 2 languages (Ruby/Swift/Kotlin/Scala), multi-repo, VS Code extension, CI matrix, Qdrant backend — 244 tests |
| **Phase 7: Git Sync + Qwen3 + Eval** | ✅ Complete | Git-aware incremental sync, Qwen3 candle backend, embedding eval harness — 260 tests |
| **Phase 8: Productivity + Ecosystem** | ✅ Complete | 24 MCP tools (apply_patch, run_tests, outline_file, rename_symbol, explain, symbol_callers, symbol_callees, predict_impact, stitch_context, enrich_docs), LSP server, Zig+PHP, Docker, Homebrew, 60s search cache — 260 tests |
| **Phase 10: Developer Intelligence** | ✅ Complete | 32 MCP tools (remember, recall, forget, find_tests, find_similar, get_complexity, review_context, generate_onboarding), persistent memory store, cyclomatic complexity, onboarding doc generation — 210 tests |
| **Phase 12+13a: Distribution + Session Intelligence** | ✅ Complete | 34 MCP tools (+get_session_summary, +session_reset_focus), session event tracking, session-boosted search with graph propagation, progressive focus, session-aware explain, session persistence across restarts, --no-session opt-out — 286 tests |
| **Phase 11: IDE Integration** | ✅ Complete | LSP server (`codixing-lsp`) with hover, go-to-def, references, symbols, document sync, live reindex, cyclomatic complexity diagnostics; VS Code LSP client; BM25-only default; Tier 2 retrieval quality regression suite — 334 tests |

---

## Development

```bash
cargo build --workspace
cargo test --workspace        # 334 tests
cargo clippy --workspace -- -D warnings
cargo fmt --all
```

---

## License

MIT

# CodeForge

Ultra-fast code retrieval engine for AI agents — beats `grep` at its own game.

CodeForge is a Rust-native engine that gives AI coding agents precisely the right context from any codebase, regardless of size. It combines structure-aware AST parsing (tree-sitter), hybrid search (BM25 + vector), a live code dependency graph with PageRank scoring, and AI-optimized token-budgeted output into a single, blazing-fast binary.

## Why Not Just Grep?

Claude Code and similar agents currently use `grep`, `find`, and `cat` for code navigation. These tools are fast, but they have a fundamental problem: **they return everything, always**. A single `rg b2Vec2` on a real C++ game codebase returns 2,240 hits — 225,343 bytes — burning context before any reasoning happens.

CodeForge solves this with three properties grep cannot replicate:

1. **Bounded output** — `limit=20` caps results so context never overflows
2. **Structural awareness** — finds where a symbol is *defined*, not just where it appears
3. **Graph intelligence** — answers "who imports this file?" and "what does changing this break?" transitively

---

## Benchmark: CodeForge Daemon vs Native Shell Tools

Measured on [OpenClaw](https://github.com/pjasicek/OpenClaw) — 246,000 lines of C++, 770 files. CodeForge running in daemon mode (engine pre-loaded, Unix socket IPC ~6ms overhead).

| Operation | Native tool | Native | CodeForge | Speed | Tokens |
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

> **The b2Vec2 case is the decisive number.** Raw `rg b2Vec2` returns 225,343 bytes (2,240 lines) — CodeForge returns the top 20 in 1,332 bytes. Same signal, **99% less waste**, band-merged by adjacent-chunk deduplication.

### What grep cannot do at all

- PageRank-ranked architecture map (importance ≠ file size)
- Transitive import graph at arbitrary depth
- Semantic / conceptual search (BM25 understands intent, not just strings)
- Automatic token budget management (grep overflows; CodeForge caps)
- Symbol-table lookup (definition vs. every mention)

---

## Quick Start

```bash
# Build from source
cargo build --release --workspace

# Index a codebase (BM25 only — fast, no GPU needed)
./target/release/codeforge init . --no-embeddings

# Or with semantic search (BGE-Base-EN-v1.5, local ONNX inference)
./target/release/codeforge init .

# Search
codeforge search "authentication handler"
codeforge search "parse config" --strategy thorough

# Symbol lookup
codeforge symbols Engine
codeforge symbols --file src/main.rs

# Dependency graph
codeforge callers src/engine.rs          # who imports this?
codeforge callees src/engine.rs          # what does this import?
codeforge dependencies src/main.rs --depth 2
```

---

## Claude Code Integration (MCP)

CodeForge exposes all its tools via the [Model Context Protocol](https://modelcontextprotocol.io) — Claude Code picks them up automatically.

### Register once

```bash
claude mcp add --scope user --transport stdio codeforge \
  -- /path/to/codeforge-mcp --root /path/to/your/project
```

Or edit `~/.claude.json` directly:

```json
{
  "mcpServers": {
    "codeforge": {
      "type": "stdio",
      "command": "/path/to/codeforge-mcp",
      "args": ["--root", "/path/to/your/project"]
    }
  }
}
```

### Daemon mode (recommended)

Normal mode spawns a new process per call (~30ms cold start). Daemon mode loads the engine once and serves all calls over a Unix socket (~6ms IPC overhead) — **4–5× faster for cheap operations**.

```bash
# Start daemon (keeps running, auto-updates index on file saves)
codeforge-mcp --root /path/to/project --daemon &

# All subsequent codeforge-mcp calls auto-proxy through the daemon
```

The daemon runs a background file watcher. When you save a file, the index updates within ~100ms. Claude Code always queries a fresh index.

### Available MCP tools (10)

| Tool | What it does |
|------|-------------|
| `code_search` | BM25 + graph-boosted search; `instant`/`fast`/`thorough`/`explore` strategies |
| `grep_code` | Regex or literal search across indexed files; bounded output, glob filter, context lines |
| `find_symbol` | Structured symbol lookup — returns definition location + signature |
| `read_symbol` | Full source of a named symbol |
| `read_file` | Token-budgeted file reader with line range |
| `get_repo_map` | PageRank-ranked architecture overview within a token budget |
| `get_references` | Who imports a file (callers) + what it imports (callees) |
| `get_transitive_deps` | Multi-hop dependency chain to arbitrary depth |
| `search_usages` | All usage sites of a symbol across the codebase |
| `index_status` | Current index statistics (files, chunks, symbols, graph) |

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
| **Test suite** | 232 tests (including retrieval quality regression suite) |

### Init speed breakdown (0.87s on 246K LoC)

| Stage | Time | Notes |
|-------|------|-------|
| File discovery | ~5ms | Directory walk, 770 files |
| Parse + chunk + BM25 index | ~600ms | rayon parallel, all CPU cores |
| Graph build (imports + PageRank) | ~200ms | Parallel resolution, single sequential insert pass |
| Persist to `.codeforge/` | ~50ms | bitcode + Tantivy flush |

> **Why it's fast:** `build_graph()` reuses the import lists extracted during the parallel parse phase — no second file read, no second tree-sitter parse. Files are parsed exactly once.

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
- **Hash-based incremental sync** — `codeforge sync` diffs xxh3 content hashes and re-indexes only changed files; no git required
- **MCP server** — 10 tools exposed via JSON-RPC 2.0; Claude Code registers with one command
- **Concurrent symbol table** — DashMap-backed with exact, prefix, and substring matching
- **Single binary, zero runtime deps** — No JVM, no Docker, no external databases

---

## Supported Languages

| Tier | Languages |
|------|-----------|
| **Tier 1** (full AST + graph) | Rust, Python, TypeScript, TSX, JavaScript, Go, Java, C, C++, C# |
| **Tier 2** (full AST + graph) | Ruby, Swift, Kotlin, Scala |

---

## Architecture

```
┌────────────────────────────────────────────────────────────────────────┐
│                           CodeForge Engine                              │
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
| **Phase 4: Agent Integration** | ✅ Complete | MCP (10 tools), daemon mode, 2.6× faster init, live watcher — 222 tests |
| **Phase 5: Production Hardening** | ✅ Complete | Field boosts, band merging, asymmetric RRF, call graph edges, sync, .gitignore walker — 232 tests |
| **Phase 6: Ecosystem Expansion** | ✅ Complete | Tier 2 languages (Ruby/Swift/Kotlin/Scala), multi-repo, VS Code extension, CI matrix, Qdrant backend — 243 tests |

---

## Development

```bash
cargo build --workspace
cargo test --workspace        # 232 tests
cargo clippy --workspace -- -D warnings
cargo fmt --all
```

---

## License

MIT

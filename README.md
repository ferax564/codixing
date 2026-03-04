# CodeForge

Ultra-fast code retrieval engine for AI agents.

CodeForge is a Rust-native engine that gives AI coding agents precisely the right context from any codebase, regardless of size. It combines structure-aware parsing (tree-sitter), hybrid search (BM25 + vector), a live code dependency graph with PageRank scoring, and AI-optimized output into a single, blazing-fast binary.

## Why

AI coding agents live or die by context quality. Current approaches — naive text search, pure vector search, file-level stuffing — waste tokens and miss relevant code. CodeForge solves this by understanding code as *structure*, not text.

## Quick Start

```bash
# Install (from source)
cargo install --path crates/cli

# Index a codebase (builds BM25 index, vector embeddings, and dependency graph)
codeforge init .

# Search with natural language (graph-boosted by default)
codeforge search "authentication handler"

# Search with file filter and specific strategy
codeforge search "parse config" --file "src/" --limit 5 --strategy thorough

# List symbols
codeforge symbols Engine
codeforge symbols --file src/main.rs

# Explore the dependency graph
codeforge graph                          # graph stats
codeforge graph --map --token-budget 2000  # token-budgeted repo map
codeforge callers src/engine.rs          # files that import engine.rs
codeforge callees src/engine.rs          # files that engine.rs imports
codeforge dependencies src/engine.rs --depth 2   # transitive dependency tree
```

## Example Output

```
$ codeforge init .
Indexing /home/user/my-project...
Indexed 28 files, 136 chunks, 370 symbols, 512 vectors in 1.4s

$ codeforge search "authentication handler" --strategy fast
1. src/auth/handler.rs [L12-L45] (Rust) score=8.931
   pub async fn authenticate(req: Request) -> Response
   | pub async fn authenticate(req: Request) -> Response {
   |     let token = req.headers().get("Authorization");
   |     ...

$ codeforge graph
Graph Statistics
  Nodes (files):     28
  Edges (imports):   47
  Resolved edges:    31
  External edges:    16

$ codeforge callers src/parser.rs
src/engine.rs
src/main.rs

  2 caller(s) found.

$ codeforge symbols Config
KIND         NAME                 FILE                    LINES
--------------------------------------------------------------
Struct       Config               src/engine.rs           L35-L44
Import       Config               src/lib.rs              L13-L14
```

## Key Features

- **AST-aware chunking** — Tree-sitter parsing across 10 language families; never splits a function in half
- **BM25 full-text search** — Tantivy-backed with a custom code tokenizer (camelCase, snake_case, dot.path splitting)
- **Hybrid retrieval** — BM25 + vector (fastembed BGE-Small) fused with Reciprocal Rank Fusion; MMR deduplication on `thorough` strategy
- **Code dependency graph** — Import extraction for all 10 languages, petgraph `DiGraph`, PageRank scoring; transparently boosts search result ranking
- **Repo map generation** — Aider-style, token-budgeted output sorted by PageRank for AI agent context
- **Concurrent symbol table** — DashMap-backed with exact, prefix, and pattern matching
- **Incremental indexing** — Sub-500ms updates via file watcher with debouncing; graph edges updated automatically
- **Single binary, zero runtime deps** — No JVM, no Docker, no external databases

## Supported Languages

| Tier | Languages | Capabilities |
|------|-----------|-------------|
| **Tier 1** | Rust, Python, TypeScript, TSX, JavaScript, Go, Java, C, C++, C# | Full AST parsing + entity extraction + symbol resolution |
| **Tier 2** | *(Phase 2)* Ruby, PHP, Swift, Kotlin, Scala, Zig, Elixir, Lua, Bash, SQL | AST chunking + basic indexing |
| **Tier 3** | Any tree-sitter grammar (40+) | Text-mode fallback |

## Architecture

```
┌────────────────────────────────────────────────────────────────────┐
│                         CodeForge Engine                            │
│                                                                     │
│  ┌────────────┐  ┌──────────┐  ┌──────────┐  ┌──────────────────┐  │
│  │ Tree-sitter │  │ Tantivy  │  │  Symbol  │  │  Code Graph      │  │
│  │ AST Parser  │  │  (BM25)  │  │  Table   │  │  (petgraph)      │  │
│  └─────┬──────┘  └────┬─────┘  └────┬─────┘  └────────┬─────────┘  │
│        │              │              │                  │            │
│  ┌─────▼──────┐  ┌────▼─────┐  ┌────▼─────┐  ┌────────▼─────────┐  │
│  │   cAST     │  │ Code     │  │ DashMap  │  │  ImportExtractor  │  │
│  │  Chunker   │  │ Tokenizer│  │ (conc.)  │  │  PageRank         │  │
│  └────────────┘  └────┬─────┘  └──────────┘  └────────┬─────────┘  │
│                        │                               │            │
│  ┌─────────────────────▼───────────────────────────────▼──────────┐ │
│  │         Retriever: BM25 | Hybrid (RRF) | Thorough (MMR)        │ │
│  │                  + Graph PageRank boost                         │ │
│  └────────────────────────────────────────────────────────────────┘ │
│                                                                     │
│  ┌──────────────────────────────────────────────────────────────── ┐ │
│  │       API Layer: CLI (clap) / REST (axum) / File Watcher        │ │
│  └──────────────────────────────────────────────────────────────── ┘ │
└────────────────────────────────────────────────────────────────────┘
```

## Rust Library API

```rust
use codeforge_core::{Engine, IndexConfig, RepoMapOptions, SearchQuery, Strategy};

// Index a project (graph built automatically)
let config = IndexConfig::new("./my-project");
let engine = Engine::init("./my-project", config)?;

// Search — graph PageRank boost applied automatically for Fast/Thorough
let results = engine.search(
    SearchQuery::new("authentication handler")
        .with_limit(10)
        .with_strategy(Strategy::Fast)
        .with_file_filter("src/")
)?;

for result in &results {
    println!("{}:{}-{} score={:.2}",
        result.file_path, result.line_start, result.line_end, result.score);
}

// Symbol lookup
let symbols = engine.symbols("Config", None)?;

// Graph navigation
let callers = engine.callers("src/parser.rs");      // direct importers
let callees = engine.callees("src/engine.rs");      // direct imports
let deps    = engine.dependencies("src/main.rs", 2); // transitive, depth 2

// Repo map for AI context window
let map = engine.repo_map(RepoMapOptions {
    token_budget: 4096,
    ..Default::default()
});

// Graph stats
let stats = engine.graph_stats(); // Option<GraphStats>

// Incremental re-indexing (graph edges update automatically)
let mut engine = engine;
engine.reindex_file(Path::new("src/main.rs"))?;
```

## Performance

| Metric | Measured | Target |
|--------|----------|--------|
| Index speed (BM25 only) | 28 files in 0.27s (~100 files/s) | >800 files/s |
| Index speed (hybrid + graph) | 28 files in ~1.4s (embedding dominates) | — |
| Incremental update | <500ms | <500ms |
| BM25 search latency | <5ms p99 | <10ms p99 |
| Hybrid search latency | <50ms p99 | <50ms p99 |
| Test suite | 165 tests in ~4s | <30s |
| Binary size | TBD | <50MB |

## Tech Stack

| Component | Crate | Purpose |
|-----------|-------|---------|
| AST Parsing | `tree-sitter` 0.26 | Incremental, multi-language parsing |
| Full-text search | `tantivy` 0.22 | BM25 scoring, inverted index |
| Vector embeddings | `fastembed` 5 | BGE-Small-EN-v1.5, local ONNX inference |
| Vector index | `usearch` 2 | HNSW approximate nearest-neighbour |
| Code graph | `petgraph` 0.8 | `DiGraph` + PageRank for dependency graph |
| Token counting | `tiktoken-rs` 0.9 | cl100k_base token budget enforcement |
| HTTP server | `axum` 0.8 | Async REST API |
| Symbol table | `dashmap` 6 | Lock-free concurrent hash map |
| Parallelism | `rayon` 1 | Parallel file processing |
| File watching | `notify` 8 | Cross-platform fs event monitoring |
| Serialization | `bitcode` 0.6 | Fast binary serialization |
| Content hashing | `xxhash-rust` 0.8 | Change detection (xxh3) |
| CLI | `clap` 4 | Command-line interface |
| Logging | `tracing` 0.1 | Structured logging |

## Roadmap

See [ROADMAP.md](ROADMAP.md) for the full phased delivery plan.

| Phase | Status | Description |
|-------|--------|-------------|
| **Phase 1: Foundation** | **Complete** | AST parsing, BM25 search, CLI, file watcher — 111 tests |
| **Phase 2: Semantic Search** | **Complete** | Vector search, hybrid retrieval (RRF+MMR), REST API — 131 tests |
| **Phase 3: Graph Intelligence** | **Complete** | Code graph, PageRank, repo map, graph-boosted search — 165 tests |
| Phase 4: Agent Integration | Planned | MCP server, gRPC, multi-repo |
| Phase 5: Production Hardening | Planned | Benchmarks, fuzzing, Tier 2 languages |

## Development

```bash
# Build
cargo build --workspace

# Test
cargo test --workspace

# Lint
cargo clippy --workspace -- -D warnings

# Format
cargo fmt --all
```

## Retrieval Strategies

| Strategy | Description | Graph Boost | Target Latency |
|----------|-------------|-------------|----------------|
| `instant` | BM25 only — no embeddings, no graph | No | <10ms p99 |
| `fast` | BM25 + vector (RRF fusion) + PageRank boost | Yes | <50ms p99 |
| `thorough` | Hybrid + MMR deduplication + PageRank boost | Yes | <200ms p99 |
| `deep` | *(Phase 4)* Multi-hop graph traversal | — | <2s p99 |

## REST API

The `codeforge-server` binary exposes a REST API on port 3000 by default.

```
POST   /search            search with strategy + optional token budget
POST   /symbols           symbol lookup with name/file filter
POST   /index/reindex     re-index a specific file
DELETE /index/file        remove a file from the index
GET    /status            index statistics (files, chunks, symbols, vectors, graph)
GET    /health            liveness probe

POST   /graph/repo-map    generate token-budgeted repo map
GET    /graph/callers     ?file=src/parser.rs&depth=1
GET    /graph/callees     ?file=src/engine.rs&depth=1
GET    /graph/stats       { node_count, edge_count, resolved_edges, external_edges }
```

## 2026 Priority Alignment

- **P0:** ~~ship Phase 1 MVP~~ — **DONE** (111 tests, 10 languages, BM25 search, CLI, file watcher)
- **P1:** ~~semantic + hybrid retrieval and REST integration~~ — **DONE** (Phase 2, hybrid+MMR, REST API)
- **P2:** ~~graph intelligence, PageRank, repo map~~ — **DONE** (Phase 3, 165 tests)
- **P3:** MCP/gRPC depth, multi-repo, production benchmark hardening.

See `ROADMAP.md` for the synchronized project roadmap.

## License

MIT

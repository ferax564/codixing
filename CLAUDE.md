# CodeForge — Claude Code Instructions

## Project Overview

CodeForge is a Rust-native code retrieval engine for AI agents. It combines tree-sitter AST parsing, hybrid search (BM25 + vector), a live code dependency graph with PageRank scoring, and AI-optimized output into a single binary.

**PRD**: `PRD.md` at repo root — the authoritative specification.
**Roadmap**: `ROADMAP.md` — phased delivery plan with status.

## Current Status

**Phase 4A (Agent Integration): IN PROGRESS** — 167 tests, version 0.4.0

### What's been delivered across all phases

**Phase 1 — Foundation**
- Tree-sitter AST parsing for 10 language variants (Rust, Python, TS, TSX, JS, Go, Java, C, C++, C#)
- cAST recursive split-then-merge chunker
- Tantivy BM25 full-text index with custom CodeTokenizer
- DashMap-based symbol table with bitcode persistence
- Engine facade: `init`, `open`, `search`, `symbols`, `reindex_file`, `remove_file`, `watch`, `save`
- CLI: `codeforge init`, `codeforge search`, `codeforge symbols`
- File watcher with debounced incremental re-indexing
- Index persistence to `.codeforge/` directory

**Phase 2 — Semantic Search**
- fastembed BGE-Small-EN-v1.5 local embedding inference (ONNX) → upgraded to BGE-Base-EN-v1.5 (768 dims) as default
- usearch HNSW vector index with incremental updates; int8 quantization (8× memory reduction)
- Hybrid retrieval: BM25 + vector with Reciprocal Rank Fusion (RRF)
- Maximal Marginal Relevance (MMR) deduplication (`thorough` strategy)
- Contextual embeddings: file path + scope chain + signature prepended to chunk content (+35% recall)
- Token budget management with tiktoken-rs (cl100k_base)
- AI-optimized context formatter
- REST API server (axum): `/search`, `/symbols`, `/index/reindex`, `/index/file`, `/status`, `/health`

**Phase 3 — Graph Intelligence**
- Import extractor: tree-sitter AST walker across all 10 languages
- Import resolver: per-language raw import → indexed file path resolution
- `CodeGraph`: petgraph `DiGraph` wrapped with `path_to_node` HashMap for stable lookups
- `GraphData`: flat bitcode serialization format (stable across rebuilds)
- PageRank: custom iterative power method, dangling-node redistribution, normalized max=1.0
- Graph boost: `score *= 1 + 0.3 * pagerank` on `fast`/`thorough` strategies
- Repo map: Aider-style, token-budgeted, sorted by PageRank
- Graph persistence: `.codeforge/graph/graph.bin`; incremental updates on reindex/remove
- CLI: `codeforge graph`, `codeforge callers`, `codeforge callees`, `codeforge dependencies`
- REST: `POST /graph/repo-map`, `GET /graph/callers`, `GET /graph/callees`, `GET /graph/stats`

**Phase 4A — Agent Integration (partial, in progress)**
- MCP server binary (`codeforge-mcp`): JSON-RPC 2.0 over stdin/stdout for Claude Code integration
- 7 MCP tools: `code_search`, `find_symbol`, `get_references`, `get_repo_map`, `search_usages`, `get_transitive_deps`, `index_status`
- `explore` strategy: BM25 first-pass + graph neighbor expansion (RepoHyper Search-then-Expand pattern)
- `Engine::search_usages()`: BM25 + graph boost for symbol reference lookup
- CLI: `codeforge usages` subcommand; `--strategy explore` added to `search`

## Language & Toolchain

- **Language**: Rust (stable toolchain, 2024 edition)
- **Build**: `cargo build --workspace`
- **Test**: `cargo test --workspace`
- **Linting**: `cargo clippy --workspace -- -D warnings`
- **Formatting**: `cargo fmt --check` / `cargo fmt --all`

## Architecture

```
codeforge/
├── Cargo.toml             # Workspace root (virtual manifest)
├── crates/
│   ├── core/              # Engine library
│   │   ├── src/
│   │   │   ├── lib.rs          # Re-exports: Engine, IndexStats, SearchQuery, GraphStats, etc.
│   │   │   ├── engine.rs       # Engine facade — all public API
│   │   │   ├── error.rs        # CodeforgeError (thiserror)
│   │   │   ├── config.rs       # IndexConfig, ChunkConfig, EmbeddingConfig, GraphConfig
│   │   │   ├── language/       # 10 languages, LanguageSupport trait, registry
│   │   │   ├── parser/         # tree-sitter parser + DashMap tree cache
│   │   │   ├── chunker/        # cAST + line-based fallback
│   │   │   ├── index/          # Tantivy schema + CodeTokenizer + BM25
│   │   │   ├── retriever/      # BM25Retriever, HybridRetriever (RRF), MMR
│   │   │   ├── embedder/       # fastembed BGE-Small-EN wrapper
│   │   │   ├── vector/         # usearch HNSW index
│   │   │   ├── graph/          # NEW: CodeGraph, extractor, resolver, pagerank, repomap
│   │   │   ├── symbols/        # DashMap symbol table + bitcode persistence
│   │   │   ├── persistence/    # .codeforge/ directory management (now includes graph/)
│   │   │   ├── formatter/      # AI-optimized context output
│   │   │   └── watcher/        # notify-based debounced file watcher
│   │   └── tests/              # Integration tests (indexing, search, graph, watcher, chunker)
│   ├── cli/               # CLI binary (clap)
│   ├── server/            # REST API server (axum)
│   │   └── src/routes/    # search, symbols, index, graph handlers
│   └── mcp/               # MCP server binary (JSON-RPC 2.0 over stdio)
│       └── src/
│           ├── main.rs    # Async message loop, Engine::open, spawn_blocking
│           ├── protocol.rs # JsonRpcRequest/Response/Error serde types
│           └── tools.rs   # 7 tool definitions + dispatch to engine methods
├── PRD.md                 # Product requirements
├── ROADMAP.md             # Delivery roadmap
├── CLAUDE.md              # This file
└── README.md
```

## Key Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `tree-sitter` | 0.26 | AST parsing (10 language grammars) |
| `tantivy` | 0.22 | BM25 full-text search |
| `fastembed` | 5 | BGE-Small-EN-v1.5 local ONNX embeddings |
| `usearch` | 2 | HNSW approximate nearest-neighbour index |
| `petgraph` | 0.8 | `DiGraph` code dependency graph (serde-1 feature) |
| `tiktoken-rs` | 0.9 | cl100k token counting for repo map budget |
| `axum` | 0.8 | Async REST API server |
| `notify` | 8 | File watching |
| `clap` | 4 | CLI (derive macros) |
| `dashmap` | 6 | Concurrent symbol table |
| `rayon` | 1 | Parallel file processing |
| `serde` + `serde_json` | 1 | JSON serialization |
| `bitcode` | 0.6 | Binary serialization (serde feature) |
| `xxhash-rust` | 0.8 | Content hashing (xxh3) |
| `thiserror` | 2 | Library error types |
| `anyhow` | 1 | Binary error handling |
| `tracing` | 0.1 | Structured logging |

**Notes:**
- `bitcode` uses `bitcode::serialize`/`bitcode::deserialize` (serde feature), NOT `bitcode::encode`/`bitcode::decode`.
- `tree_sitter::Parser` is `!Send` — create fresh per call, never store in structs.
- `petgraph` `NodeIndex` is fragile across `remove_node()` (swap-remove) — always keep `path_to_node: HashMap<String, NodeIndex>` in sync.

## Coding Standards

- No `unsafe` in application code (vendored C deps for tree-sitter are acceptable)
- Use `thiserror` for library errors, `anyhow` for binary crates
- All public APIs must have doc comments
- Prefer `impl Trait` over dynamic dispatch where possible
- Use `tracing` for all logging (not `println!` or `eprintln!`)
- Test with both unit tests and integration tests
- `tree_sitter::Parser` is `!Send` — create fresh per call, never store in structs

## Performance Requirements

- Retrieval: <50ms p99 for `fast` strategy on 1M+ LoC
- Incremental index update: <500ms from file save
- Memory: <2GB for 1M LoC indexed
- Binary: <50MB statically linked

## Design Principles

1. **Structure over text** — code is a tree, not a string
2. **Hybrid by default** — no single retrieval method wins
3. **Incremental everything** — full re-indexing is a failure mode
4. **AI-native output** — results formatted for LLM comprehension
5. **Zero-config start** — `codeforge init .` just works
6. **Single binary** — no runtime dependencies

## Workflow

- Run `cargo test --workspace` before committing
- Run `cargo clippy --workspace -- -D warnings` to catch lint issues
- Format with `cargo fmt --all`
- Commit messages: imperative mood, concise (<72 chars first line)

## Priority Alignment (2026)

- **P0:** ~~implement Phase 1 MVP~~ — **DONE** (111 tests, 10 languages, BM25 search, CLI, file watcher)
- **P1:** ~~semantic + hybrid retrieval and REST server~~ — **DONE** (Phase 2, 131 tests, hybrid+MMR, REST API)
- **P2:** ~~graph intelligence~~ — **DONE** (Phase 3, 165 tests, PageRank, repo map, graph CLI/REST)
- **P3:** ~~MCP server + enhanced embeddings + explore strategy~~ — **DONE** (Phase 4A, 167 tests, 7 MCP tools, contextual embeddings, int8 quantization, explore strategy)
- **P4:** gRPC depth, multi-repo support, cross-encoder reranker, production benchmark hardening.

Roadmap reference: `ROADMAP.md`.

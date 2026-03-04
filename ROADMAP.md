# CodeForge Roadmap

## Organization Goal

Ship CodeForge as the retrieval backbone for ForgePipe AI workflows. Prioritize retrieval correctness, indexing stability, and predictable latency before advanced features.

## Task Mapping

| Task | Description | Status | Dependencies |
|------|-------------|--------|--------------|
| `CF-A1` | Phase 1 scaffold + BM25 contract-compatible stub for ForgePipe integration | **Done** | — |
| `CF-A2` | Hybrid retrieval + REST API for ForgePipe worker integration | **Done** | `FP-A2` contract schema freeze |
| `CF-A3` | Code dependency graph + PageRank + repo map for structural context | **Done** | — |
| `CF-A4` | MCP server + BGE-Base embeddings + contextual embeddings + explore strategy | **In Progress** | — |

## Success Gates

### Phase 1 (Met)
- [x] MVP returns relevant symbol-aware results reliably on real repositories
- [x] Index updates are incremental and stable under active file changes

### Phase 2 (Met)
- [x] ForgePipe can execute a code-aware workflow template using CodeForge as a worker
- [x] Hybrid retrieval (BM25 + vector) measurably improves recall over BM25-only
- [x] REST API serves <50ms p99 on 1M+ LoC codebases

### Phase 3 (Met)
- [x] Code dependency graph built from tree-sitter import extraction across all 10 languages
- [x] PageRank scores transparently boost `fast`/`thorough` search ranking
- [x] Repo map generation respects token budget for AI agent context windows
- [x] Graph persists across index open/close and updates incrementally on file change

---

## Phase 1: Foundation — COMPLETE

**Delivered:** February 2026
**Tests:** 97 unit + 14 integration = 111 total

Core indexing and BM25 retrieval end-to-end.

- [x] Cargo workspace scaffold (core, cli, server crates)
- [x] Error types (`CodeforgeError` with thiserror) and configuration (`IndexConfig`, `ChunkConfig`)
- [x] Language trait + Rust implementation (reference pattern)
- [x] Tier 1 languages: Python, TypeScript/TSX/JS, Go, Java, C, C++, C#
- [x] Tree-sitter parser with DashMap-based incremental tree cache
- [x] cAST recursive split-then-merge chunker (AST-aware, never splits functions)
- [x] Tantivy BM25 index with custom CodeTokenizer (camelCase/snake_case/dot.path splitting)
- [x] DashMap-based symbol table with bitcode persistence
- [x] Index persistence to `.codeforge/` directory (JSON config/meta, bitcode symbols/hashes, tantivy native)
- [x] Engine facade: `init`, `open`, `search`, `symbols`, `reindex_file`, `remove_file`, `watch`, `save`
- [x] BM25Retriever implementing the Retriever trait
- [x] CLI commands: `codeforge init`, `codeforge search`, `codeforge symbols`
- [x] File watcher (notify) with 100ms debounce, exclude patterns, supported-extension filtering
- [x] Integration test suite: multi-language indexing, search accuracy, chunker verification, watcher lifecycle

---

## Phase 2: Semantic Search — COMPLETE

**Delivered:** March 2026
**Tests:** 117 unit + 14 integration = 131 total

- [x] ONNX Runtime integration for local embedding inference (fastembed-rs, BGE-Small-EN-v1.5)
- [x] HNSW vector index with incremental updates (usearch)
- [x] Hybrid retrieval: BM25 + vector with Reciprocal Rank Fusion (RRF)
- [x] Maximal Marginal Relevance (MMR) deduplication (`thorough` strategy)
- [x] Token budget management with `tiktoken-rs` (cl100k_base)
- [x] Context enrichment (scope chains, signatures in output)
- [x] AI-optimized output formatter
- [x] REST API server (axum): POST /search, POST /symbols, POST /index/reindex, DELETE /index/file, GET /status, GET /health
- [x] Retrieval strategy presets: `instant` (BM25), `fast` (hybrid, default), `thorough` (hybrid+MMR)
- [x] Vector + chunk_meta persistence to `.codeforge/vectors/`

---

## Phase 3: Graph Intelligence — COMPLETE

**Delivered:** March 2026
**Tests:** 142 unit + 24 integration = 165 total (includes graph unit + integration tests)

- [x] Import extractor: tree-sitter AST walker for all 10 language variants
- [x] Import resolver: per-language raw import → indexed file path resolution
- [x] petgraph `DiGraph`-backed `CodeGraph` with `path_to_node` lookup table
- [x] Flat `GraphData` serialization (bitcode) for stable cross-rebuild persistence
- [x] PageRank: custom iterative power method, dangling-node redistribution, normalized max=1.0
- [x] Graph-boosted retrieval: `score *= 1 + 0.3 * pagerank` on `fast`/`thorough` strategies
- [x] Repo map generation: token-budgeted Aider-style output sorted by PageRank
- [x] Graph persistence to `.codeforge/graph/graph.bin`; incremental updates on reindex/remove
- [x] CLI: `codeforge graph`, `codeforge callers`, `codeforge callees`, `codeforge dependencies`
- [x] REST API: `POST /graph/repo-map`, `GET /graph/callers`, `GET /graph/callees`, `GET /graph/stats`

---

## Phase 4A: Agent Integration — IN PROGRESS

**Delivered:** March 2026
**Tests:** 167 total

- [x] MCP server binary (`codeforge-mcp`): JSON-RPC 2.0 over stdin/stdout
- [x] 7 MCP tools: `code_search`, `find_symbol`, `get_references`, `get_repo_map`, `search_usages`, `get_transitive_deps`, `index_status`
- [x] `explore` strategy: BM25 first-pass + graph neighbor expansion (Search-then-Expand)
- [x] `Engine::search_usages()` public API for symbol reference lookup
- [x] CLI `codeforge usages` subcommand
- [x] Upgraded embedding model: BGE-Base-EN-v1.5 (768 dims, ~79% code MRR vs 70% for Small)
- [x] Contextual embeddings: file/scope/signature header prepended to chunk text (+35% recall)
- [x] int8 quantization for HNSW index (8× memory reduction, critical for 3M+ LoC)
- [ ] gRPC API for high-performance integrations
- [ ] Cross-encoder reranking (optional, via ONNX)
- [ ] Multi-repo support (index and query across repositories)
- [ ] Git-aware features (branch-relative search, blame, diff-aware re-indexing)
- [ ] WebSocket streaming for real-time index updates

---

## Phase 5: Production Hardening — Planned

**Goal:** Enterprise-ready reliability and performance.

- [ ] Comprehensive benchmarks (vs. Aider, Cline, Cursor retrieval layers)
- [ ] Fuzzing and property-based testing
- [ ] Memory profiling and optimization
- [ ] Cross-platform CI (Linux, macOS, Windows)
- [ ] Documentation: architecture guide, API reference, integration tutorials
- [ ] Plugin/extension system for custom retrieval strategies
- [ ] Tier 2 language support (Ruby, PHP, Swift, Kotlin, Scala, Zig, etc.)
- [ ] Optional Qdrant backend for distributed deployments
- [ ] Telemetry and observability (OpenTelemetry)

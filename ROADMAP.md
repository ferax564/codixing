# CodeForge Roadmap

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

## Phase 2: Semantic Search — Planned

**Goal:** Vector search and hybrid retrieval operational.

- [ ] ONNX Runtime integration for local embedding inference
- [ ] HNSW vector index with incremental updates (`hnsw_rs` or `instant-distance`)
- [ ] Dual-embedding pipeline (NLP + code embeddings)
- [ ] Hybrid retrieval: BM25 + vector with Reciprocal Rank Fusion (RRF)
- [ ] Maximal Marginal Relevance (MMR) deduplication
- [ ] Token budget management with `tiktoken-rs`
- [ ] Context enrichment (scope chains, signatures, imports in output)
- [ ] AI-optimized output formatter
- [ ] REST API server (axum)
- [ ] Retrieval strategy presets: `instant`, `fast`, `thorough`

---

## Phase 3: Graph Intelligence — Planned

**Goal:** Code graph unlocks structural understanding.

- [ ] Definition/reference extraction from tree-sitter ASTs
- [ ] Import resolvers for Tier 1 languages
- [ ] petgraph-based code graph with persistence
- [ ] PageRank scoring for file/symbol relevance
- [ ] Repo map generation (Aider-style, token-budgeted)
- [ ] Graph-boosted retrieval (graph signal fused into ranking)
- [ ] CLI: `graph`, `callers`, `callees`, `dependencies`
- [ ] Incremental graph updates on file change

---

## Phase 4: Agent Integration — Planned

**Goal:** First-class AI agent support.

- [ ] MCP server implementation (code_search, find_symbol, get_references, get_repo_map)
- [ ] gRPC API for high-performance integrations
- [ ] Cross-encoder reranking (optional, via ONNX)
- [ ] Multi-repo support (index and query across repositories)
- [ ] Git-aware features (branch-relative search, blame, diff-aware re-indexing)
- [ ] WebSocket streaming for real-time index updates
- [ ] `deep` retrieval strategy (multi-hop graph traversal)

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

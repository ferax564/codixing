# CodeForge Roadmap

## Organization Goal

Ship CodeForge as the retrieval backbone for ForgePipe AI workflows. Prioritize retrieval correctness, indexing stability, and predictable latency before advanced features.

## Task Mapping

| Task | Description | Status | Dependencies |
|------|-------------|--------|--------------|
| `CF-A1` | Phase 1 scaffold + BM25 contract-compatible stub for ForgePipe integration | **Done** | — |
| `CF-A2` | Hybrid retrieval + REST API for ForgePipe worker integration | **Done** | `FP-A2` contract schema freeze |
| `CF-A3` | Code dependency graph + PageRank + repo map for structural context | **Done** | — |
| `CF-A4` | MCP server + daemon mode + BGE-Base embeddings + 10 tools + live watcher + 2.6× faster init | **Done** | — |

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

### Phase 4 (Met)
- [x] Claude Code can call all 10 CodeForge tools via MCP with a single `claude mcp add` command
- [x] Daemon mode delivers 4–5× faster per-call latency; file watcher keeps index fresh within 100ms
- [x] Init time: 0.87s on 246K LoC (2.6× faster than Phase 3 baseline via import cache elimination of double-parse)
- [x] Token budget enforcement: high-frequency grep patterns return 99% fewer tokens than native `grep`

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

## Phase 4: Agent Integration — COMPLETE

**Delivered:** March 2026
**Tests:** 222 total (57 new — MCP smoke tests + watcher integration + engine optimization tests)

### What shipped

**MCP server (`codeforge-mcp` binary)**
- [x] JSON-RPC 2.0 message loop over stdin/stdout (`initialize`, `tools/list`, `tools/call`)
- [x] 10 MCP tools: `code_search`, `grep_code`, `find_symbol`, `read_symbol`, `read_file`, `get_repo_map`, `get_references`, `get_transitive_deps`, `search_usages`, `index_status`
- [x] `explore` strategy: BM25 first-pass + graph neighbor expansion
- [x] Auto-init: if no `.codeforge/` index exists, MCP server builds one automatically (BM25-only, no embeddings)
- [x] Daemon mode (`--daemon`): loads engine once, serves all clients over a Unix domain socket, 4–5× faster per-call latency vs cold-start
- [x] Proxy mode: normal `codeforge-mcp` invocations detect a live daemon socket and forward traffic through it transparently
- [x] Live file watcher in daemon: `FileWatcher` runs in a background thread; file changes apply within ~100ms, no daemon restart needed
- [x] Batched PageRank: `apply_changes()` runs a single PageRank pass for any N-file batch (N× faster than N individual reindexes)
- [x] `search_usages` API + CLI `codeforge usages` subcommand

**Better embeddings**
- [x] Upgraded to BGE-Base-EN-v1.5 (768 dims, ~79% code MRR vs ~70% for BGE-Small at 384 dims)
- [x] `EmbeddingModel::BgeBaseEn` variant, new default in `EmbeddingConfig`

**2.6× faster init (the key performance win)**
- [x] Eliminated double-parse: `process_file()` now caches `(Vec<RawImport>, Language)` in a `DashMap`
- [x] `build_graph()` reads from cache — zero extra I/O, zero extra tree-sitter parses
- [x] Phase 1 of graph build (import resolution) parallelized with rayon
- [x] Result: 2.3s → 0.87s on OpenClaw (246K LoC, 770 files)

**Benchmark vs native tools (OpenClaw, daemon mode, ripgrep 15.1.0)**

| Operation | Native | CodeForge | Speed | Tokens |
|-----------|-------:|----------:|------:|-------:|
| Literal search | 23ms | 24ms | ≈ same | **−61%** |
| Regex + file filter (4,102 hits) | 18ms | 10ms | **1.8×** | **−99%** |
| High-freq pattern (2,240 hits) | 20ms | 7ms | **2.9×** | **−99%** |
| Find class definition | 16ms | 8ms | **1.9×** | structured |
| Read large file | 3ms | 6ms | −1.8× | **−91%** |
| Reverse dependency lookup | 13ms | 7ms | **1.8×** | **−99%** |
| Transitive dep chain (depth 2) | n/a | 7ms | structural | −66% |
| Architecture overview | n/a | 109ms | PageRank-sorted | structural |
| Semantic / conceptual search | n/a | 38ms | **natural language** | structured |

> The b2Vec2 case: raw `rg b2Vec2` returns 225,343 bytes (2,240 lines). CodeForge returns top 20 in 1,332 bytes — **99% less waste**, same signal.

---

## Phase 5: Production Hardening — Planned

**Goal:** Reliability, broader language coverage, and real-world agent integrations.

### High-impact next steps (prioritized)

**P0 — Indexing reliability**
- [x] **Batched Tantivy commits in `apply_changes()`** — single fsync for N-file batches; N fsyncs → 1
- [x] **Hash-based incremental sync** — `Engine::sync()` + `codeforge sync` diff stored xxh3 hashes; re-indexes only changed files
- [x] **`.gitignore`-aware file walker** — replaced manual `walk_dir_recursive` with `ignore::WalkBuilder`; respects `.gitignore`, `.ignore`, global gitignore (same as ripgrep)
- [ ] **Git-aware incremental init** — `git diff --name-only <last-indexed-commit>` to skip unchanged files on re-open; enables sub-100ms "re-open after pull" instead of full re-index

**P1 — Retrieval quality**
- [x] **Cross-encoder reranking** — BGE-Reranker-Base ONNX reranker (`Strategy::Deep`); opt-in via `--reranker` at init time; graceful fallback to `thorough` if not loaded (Phase 4A)
- [x] **Contextual embeddings** — file path + scope chain + signature prepended before embedding; `EmbeddingConfig.contextual_embeddings = true` by default (+35% recall) (Phase 4A)
- [x] **int8 quantization for HNSW** — `usearch` int8 (`ScalarKind::I8`); `EmbeddingConfig.quantize = true` by default; 8× memory reduction (Phase 4A)
- [x] **Tantivy field boosting** — `signature` field 3×, `entity_names` 2×; symbol lookups rank above raw content hits in BM25
- [x] **Asymmetric RRF query routing** — `is_identifier_query()` classifier; BM25-dominant k=(20,90) for identifiers, vector-dominant k=(90,20) for natural language
- [x] **Context band merging (LDAR-style)** — adjacent same-file chunks within 3 lines merged before rendering; 25–63% fewer tokens
- [x] **JinaEmbedCode model** — `EmbeddingModel::JinaEmbedCode` variant (768 dims); optimised for code/text retrieval
- [x] **Personalized PageRank** — `compute_personalized_pagerank(seeds)` + `Engine::personalized_pagerank(seed_files)`; files closer to seeds score higher
- [x] **Symbol-level call graph edges** — `CallExtractor` for Rust/Python/TS/JS/Go; `EdgeKind::Calls`; conservative resolution (exact-name, single-defining-file)
- [x] **`codeforge embed`** — `Engine::embed_remaining()` embeds un-embedded chunks without full re-index
- [x] **Retrieval quality test suite** — `tests/retrieval_quality_test.rs`; 10 Recall@k + precision assertions on synthetic codebase

**P2 — Scope expansion**
- [ ] **Tier 2 language support** — Ruby, Swift, Kotlin, Scala, Zig (tree-sitter grammars exist; need language trait + import resolver implementations)
- [ ] **Multi-repo support** — index multiple roots, query across them; needed for monorepos and cross-service agent workflows
- [ ] **`read_symbol` tool** — return full source of a named function/class (already partially implemented via symbol table + file reader; needs MCP wiring)

**P3 — Production ops**
- [ ] **Cross-platform CI** — Linux, macOS, Windows GitHub Actions matrix
- [ ] **Comprehensive benchmarks** — automated comparison vs Aider, Cline, Cursor retrieval layers with reproducible harness
- [ ] **Tier 2 language support** — Ruby, PHP, Swift, Kotlin, Scala, Zig
- [ ] **Optional Qdrant backend** — for distributed deployments where the index must live outside the binary
- [ ] **Telemetry** — OpenTelemetry spans for indexing + retrieval latency in production environments

# Codixing Roadmap

## Organization Goal

Ship Codixing as the retrieval backbone for ForgePipe AI workflows. Prioritize retrieval correctness, indexing stability, and predictable latency before advanced features.

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
- [x] ForgePipe can execute a code-aware workflow template using Codixing as a worker
- [x] Hybrid retrieval (BM25 + vector) measurably improves recall over BM25-only
- [x] REST API serves <50ms p99 on 1M+ LoC codebases

### Phase 3 (Met)
- [x] Code dependency graph built from tree-sitter import extraction across all 10 languages
- [x] PageRank scores transparently boost `fast`/`thorough` search ranking
- [x] Repo map generation respects token budget for AI agent context windows
- [x] Graph persists across index open/close and updates incrementally on file change

### Phase 4 (Met)
- [x] Claude Code can call all 10 Codixing tools via MCP with a single `claude mcp add` command
- [x] Daemon mode delivers 4–5× faster per-call latency; file watcher keeps index fresh within 100ms
- [x] Init time: 0.87s on 246K LoC (2.6× faster than Phase 3 baseline via import cache elimination of double-parse)
- [x] Token budget enforcement: high-frequency grep patterns return 99% fewer tokens than native `grep`

---

## Phase 1: Foundation — COMPLETE

**Delivered:** February 2026
**Tests:** 97 unit + 14 integration = 111 total

Core indexing and BM25 retrieval end-to-end.

- [x] Cargo workspace scaffold (core, cli, server crates)
- [x] Error types (`CodixingError` with thiserror) and configuration (`IndexConfig`, `ChunkConfig`)
- [x] Language trait + Rust implementation (reference pattern)
- [x] Tier 1 languages: Python, TypeScript/TSX/JS, Go, Java, C, C++, C#
- [x] Tree-sitter parser with DashMap-based incremental tree cache
- [x] cAST recursive split-then-merge chunker (AST-aware, never splits functions)
- [x] Tantivy BM25 index with custom CodeTokenizer (camelCase/snake_case/dot.path splitting)
- [x] DashMap-based symbol table with bitcode persistence
- [x] Index persistence to `.codixing/` directory (JSON config/meta, bitcode symbols/hashes, tantivy native)
- [x] Engine facade: `init`, `open`, `search`, `symbols`, `reindex_file`, `remove_file`, `watch`, `save`
- [x] BM25Retriever implementing the Retriever trait
- [x] CLI commands: `codixing init`, `codixing search`, `codixing symbols`
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
- [x] Vector + chunk_meta persistence to `.codixing/vectors/`

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
- [x] Graph persistence to `.codixing/graph/graph.bin`; incremental updates on reindex/remove
- [x] CLI: `codixing graph`, `codixing callers`, `codixing callees`, `codixing dependencies`
- [x] REST API: `POST /graph/repo-map`, `GET /graph/callers`, `GET /graph/callees`, `GET /graph/stats`

---

## Phase 4: Agent Integration — COMPLETE

**Delivered:** March 2026
**Tests:** 222 total (57 new — MCP smoke tests + watcher integration + engine optimization tests)

### What shipped

**MCP server (`codixing-mcp` binary)**
- [x] JSON-RPC 2.0 message loop over stdin/stdout (`initialize`, `tools/list`, `tools/call`)
- [x] 10 MCP tools: `code_search`, `grep_code`, `find_symbol`, `read_symbol`, `read_file`, `get_repo_map`, `get_references`, `get_transitive_deps`, `search_usages`, `index_status`
- [x] `explore` strategy: BM25 first-pass + graph neighbor expansion
- [x] Auto-init: if no `.codixing/` index exists, MCP server builds one automatically (BM25-only, no embeddings)
- [x] Daemon mode (`--daemon`): loads engine once, serves all clients over a Unix domain socket, 4–5× faster per-call latency vs cold-start
- [x] Proxy mode: normal `codixing-mcp` invocations detect a live daemon socket and forward traffic through it transparently
- [x] Live file watcher in daemon: `FileWatcher` runs in a background thread; file changes apply within ~100ms, no daemon restart needed
- [x] Batched PageRank: `apply_changes()` runs a single PageRank pass for any N-file batch (N× faster than N individual reindexes)
- [x] `search_usages` API + CLI `codixing usages` subcommand

**Better embeddings**
- [x] Upgraded to BGE-Base-EN-v1.5 (768 dims, ~79% code MRR vs ~70% for BGE-Small at 384 dims)
- [x] `EmbeddingModel::BgeBaseEn` variant, new default in `EmbeddingConfig`

**2.6× faster init (the key performance win)**
- [x] Eliminated double-parse: `process_file()` now caches `(Vec<RawImport>, Language)` in a `DashMap`
- [x] `build_graph()` reads from cache — zero extra I/O, zero extra tree-sitter parses
- [x] Phase 1 of graph build (import resolution) parallelized with rayon
- [x] Result: 2.3s → 0.87s on OpenClaw (246K LoC, 770 files)

**Benchmark vs native tools (OpenClaw, daemon mode, ripgrep 15.1.0)**

| Operation | Native | Codixing | Speed | Tokens |
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

> The b2Vec2 case: raw `rg b2Vec2` returns 225,343 bytes (2,240 lines). Codixing returns top 20 in 1,332 bytes — **99% less waste**, same signal.

---

## Phase 5: Production Hardening — COMPLETE

**Delivered:** March 2026
**Tests:** 232 total (was 222 — +10 new retrieval quality tests)

**P0 — Indexing reliability**
- [x] **Batched Tantivy commits in `apply_changes()`** — single fsync for N-file batches; N fsyncs → 1
- [x] **Hash-based incremental sync** — `Engine::sync()` + `codixing sync` diff stored xxh3 hashes; re-indexes only changed files
- [x] **`.gitignore`-aware file walker** — replaced manual `walk_dir_recursive` with `ignore::WalkBuilder`; respects `.gitignore`, `.ignore`, global gitignore (same as ripgrep)

**P1 — Retrieval quality**
- [x] **Cross-encoder reranking** — BGE-Reranker-Base ONNX reranker (`Strategy::Deep`); opt-in via `--reranker` at init time; graceful fallback to `thorough` if not loaded
- [x] **Contextual embeddings** — file path + scope chain + signature prepended before embedding; `EmbeddingConfig.contextual_embeddings = true` by default (+35% recall)
- [x] **int8 quantization for HNSW** — `usearch` int8 (`ScalarKind::I8`); 8× memory reduction
- [x] **Tantivy field boosting** — `signature` ×3, `entity_names` ×2; definitions rank above mentions
- [x] **Asymmetric RRF query routing** — `is_identifier_query()` → BM25-dominant or vector-dominant fusion
- [x] **Context band merging (LDAR-style)** — adjacent same-file chunks within 3 lines merged; 25–63% fewer tokens
- [x] **JinaEmbedCode model** — `EmbeddingModel::JinaEmbedCode` (768 dims)
- [x] **Personalized PageRank** — `compute_personalized_pagerank(seeds)` + `Engine::personalized_pagerank(seed_files)`
- [x] **Symbol-level call graph edges** — `CallExtractor` for Rust/Python/TS/JS/Go; `EdgeKind::Calls`
- [x] **`codixing embed`** — embed BM25-only indexes without full re-init
- [x] **Retrieval quality test suite** — 10 Recall@k + 2 precision assertions on synthetic codebase

---

## Phase 6: Ecosystem Expansion — COMPLETE

**Delivered:** March 2026
**Tests:** 244 total (was 232 — +12 new)

- [x] **Tier 2 language support** — Ruby, Swift, Kotlin, Scala; full AST entity extraction + import graph integration
- [x] **Multi-repo support** — `IndexConfig.extra_roots: Vec<PathBuf>`; CLI: `codixing init --also <DIR>`
- [x] **VS Code / Cursor extension** — 6 commands; auto-registers in `~/.claude.json` + `~/.cursor/mcp.json`; status bar indicator
- [x] **Cross-platform CI** — GitHub Actions matrix: ubuntu / macos / windows + release workflow on `v*` tags (4 architectures)
- [x] **Optional Qdrant vector backend** — `VectorBackend` trait + `QdrantVectorIndex`; `--features qdrant`

---

## Phase 7: Git Sync + Qwen3 + Eval Harness — COMPLETE

**Delivered:** March 2026
**Tests:** 249 total (was 244 — +5 git-sync integration tests)

- [x] **Git-aware incremental sync** — `IndexMeta.git_commit` + `git_head_commit()` / `git_diff_since()`; `Engine::git_sync()` → `GitSyncStats`; CLI: `codixing git-sync`; handles changed / deleted / renamed / no-op / non-git-repo cases
- [x] **Qwen3 candle backend** — `EmbedBackend::Qwen3`; `EmbeddingModel::Qwen3SmallEmbedding` (1024d); fastembed 5.12 + candle-core 0.9.1; opt-in via `--features qwen3`
- [x] **Embedding eval harness** — `tests/embedding_eval_test.rs`; 12-query suite (4 identifier + 8 NL); `compare_bm25_vs_hybrid_recall` + `compare_embedding_models`; auto-verdict on fine-tuning need; all `#[ignore]` (require model download)
- [x] **Server depth fix** — REST graph routes (`/graph/callers`, `/graph/callees`) now route `depth > 1` through `transitive_callers/callees`
- [x] **`read_symbol` MCP tool** — returns full source of first matching symbol + lists additional matches

---

## Phase 8: Launch Readiness — IN PROGRESS

**Target:** Pre-launch (GTM Phase 0)

**Repository hygiene**
- [x] CONTRIBUTING.md, SECURITY.md, CODE_OF_CONDUCT.md, CLA.md
- [x] GitHub issue templates (bug, feature request) + PR template
- [x] Cargo.lock committed (binary workspace — reproducible builds)
- [x] .gitignore cleaned (no marketing assets, no internal docs, no generated data)
- [x] README updated (249 tests, Phase 7 roadmap entry)

**Website**
- [ ] Deploy `docs/index.html` to `codixing.com`
- [ ] Add email capture form (Teams waitlist)
- [ ] Wire "Request Enterprise Access" CTA to a form or email
- [ ] Record 60-second demo video (index OpenClaw → `code_search` via MCP → show token delta)

**Enterprise collateral** *(build quietly before public launch)*
- [ ] 2-page PDF: "Codixing for Air-Gapped Environments"
- [ ] 1-page ROI calculator: tokens saved × LLM API cost × queries/day
- [ ] 50-company target list (financial services, defense, large tech with internal AI coding programs)

**Pre-seeding** *(identify before publishing)*
- [ ] 3–5 Aider contributors
- [ ] 3–5 active Claude Code power users
- [ ] Continue.dev and MCP directory maintainers

---

## Next Steps (Phase 9+)

- **Zig / PHP** — Tier 2 language extensions (tree-sitter grammars exist)
- **BGE fine-tuning** — if eval harness shows >15% NL recall gap vs identifier queries
- **Telemetry** — OpenTelemetry spans for indexing + retrieval latency
- **Comprehensive benchmarks** — automated comparison vs Aider, Cline, Cursor retrieval layers

# Product Requirements Document: **Codixing** — Ultra-Fast Code Retrieval Engine for AI Agents

**Version:** 0.1.0
**Date:** February 7, 2026
**Status:** Phase 1 Complete — Foundation delivered

---

## 1. Executive Summary

Codixing is a Rust-native code retrieval engine purpose-built for feeding AI coding agents with precisely the right context from massive codebases. It combines structure-aware parsing, hybrid search (lexical + semantic + graph-based), and an AI-optimized output layer into a single, blazing-fast binary. The goal is to solve the fundamental bottleneck in AI-assisted coding: context quality. The best LLM in the world produces garbage if fed the wrong code snippets.

The project draws on cutting-edge research (cAST chunking, graph-based retrieval à la Aider, hybrid BM25+vector fusion with reranking) and proven Rust infrastructure (tree-sitter, tantivy, qdrant) to build something that doesn't exist today: an open-source, single-binary code retrieval engine that understands code as *structure*, not text.

---

## 2. Problem Statement

### 2.1 The Context Bottleneck

AI coding agents (Claude Code, Cursor, Aider, Cline, etc.) live or die by the quality of context they feed to the LLM. Current approaches have critical limitations:

- **Naive text search (ripgrep/grep):** Fast but semantically blind. Searching for "authentication" won't find the `verify_jwt_token` function unless it contains that exact word.
- **Pure vector search:** Captures semantic meaning but misses exact identifiers, API names, and variable references that code depends on. Also expensive to keep fresh.
- **File-level context stuffing:** Wastes tokens. A 2,000-line file where only 40 lines matter still consumes the full context window.
- **No structural awareness:** Existing tools chunk code by character count or line breaks, routinely splitting functions in half, separating a method from its class, or divorcing an implementation from its interface.

### 2.2 Scale Challenge

Enterprise and large open-source codebases present unique difficulties:

- Millions of lines across thousands of files and hundreds of repositories
- Polyglot environments (Rust + Python + TypeScript + SQL + Protobuf in one project)
- Rapid change: the index must stay current with every branch switch and file save
- AI agents need results in <100ms to feel interactive, and ideally <20ms for inline completions

### 2.3 Opportunity

Research from 2024–2025 shows that the retrieval layer — not the LLM itself — is the primary bottleneck to AI coding quality. Tools like Aider achieve 4.3–6.5% context utilization through graph-based retrieval (vs. 14–17% for Cursor/Cline). The cAST paper demonstrates 4–5 point gains on code benchmarks from AST-aware chunking alone. Hybrid search with reranking consistently outperforms any single retrieval method. No existing tool combines all of these techniques in a single, fast, Rust-native package.

---

## 3. Product Vision

### 3.1 One-Liner

*The fastest way to give an AI agent exactly the code it needs from any codebase, regardless of size.*

### 3.2 Design Principles

1. **Structure over text.** Code is a tree, not a string. Every operation — parsing, chunking, indexing, retrieval — respects syntactic and semantic boundaries.
2. **Hybrid by default.** No single retrieval method wins. Lexical, semantic, and structural signals are fused and reranked.
3. **Incremental everything.** Full re-indexing is a failure mode. Every mutation (file save, branch switch, git pull) triggers minimal, targeted updates.
4. **AI-native output.** Results are formatted to maximize LLM comprehension: contextualized chunks with scope chains, signatures, dependency annotations, and token budgets.
5. **Zero-config start, infinite tunability.** Works out of the box on `codixing init .` but exposes every knob for power users.
6. **Single binary, no runtime dependencies.** No JVM, no Docker, no external databases required for core functionality.

---

## 4. Target Users

| User Persona | Need | How Codixing Helps |
|---|---|---|
| **AI Agent Builders** | Feed LLMs with precise, token-efficient context from large repos | Hybrid retrieval + AI-optimized output with token budgets |
| **IDE Plugin Developers** | Provide code intelligence at interactive speeds | Sub-20ms retrieval, incremental indexing on keystroke |
| **Platform/DevTool Teams** | Build code search, code review, migration tools at enterprise scale | Multi-repo indexing, graph queries, MCP/LSP integration |
| **Individual Developers** | Understand and navigate unfamiliar codebases | Natural language code search, dependency exploration |

---

## 5. Architecture

### 5.1 High-Level Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        Codixing Engine                         │
│                                                                 │
│  ┌──────────────┐  ┌──────────────┐  ┌────────────────────────┐ │
│  │   Ingestion   │  │   Indexing    │  │      Retrieval         │ │
│  │   Pipeline    │──▶   Engine     │──▶      Engine            │ │
│  └──────┬───────┘  └──────────────┘  └────────────────────────┘ │
│         │                                        │              │
│  ┌──────▼───────┐  ┌──────────────┐  ┌──────────▼─────────────┐ │
│  │  Tree-sitter  │  │  Code Graph   │  │   Fusion & Reranking   │ │
│  │  AST Parser   │  │  (petgraph)   │  │   (RRF / Cross-enc.)  │ │
│  └──────────────┘  └──────────────┘  └────────────────────────┘ │
│                                                                 │
│  ┌──────────────┐  ┌──────────────┐  ┌────────────────────────┐ │
│  │   Tantivy     │  │   Vector      │  │   Output Formatter     │ │
│  │   (BM25/FTS)  │  │   Index       │  │   (AI-optimized)       │ │
│  └──────────────┘  │  (HNSW/IVF)  │  └────────────────────────┘ │
│                    └──────────────┘                              │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────────┐│
│  │               API Layer (gRPC / REST / MCP / CLI)            ││
│  └──────────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────────┘
```

### 5.2 Core Subsystems

#### 5.2.1 Ingestion Pipeline

**Purpose:** Transform raw source files into structured, indexed, searchable representations.

**Key components:**

- **File watcher (notify crate):** Monitors filesystem for changes. On modification, triggers re-parse of only the affected file. On branch switch (detected via `.git/HEAD` polling or libgit2), triggers differential re-index.
- **Language detection:** File extension + shebang line + content heuristics. Maps to one of 30+ supported tree-sitter grammars.
- **Tree-sitter parser:** Parses each file into a Concrete Syntax Tree (CST). Tree-sitter is battle-tested (powers Neovim, Helix, Zed), supports incremental parsing (only re-parses changed byte ranges), and covers 40+ languages.
- **AST chunker (cAST algorithm):** Traverses the CST to extract semantic entities: functions, methods, classes, structs, enums, interfaces, traits, type aliases, constants, imports. Uses recursive split-then-merge: packs maximum complete syntactic units into each chunk without exceeding token budget. Based on the cAST paper (2025) which shows 4–5 point gains over naive line-based chunking on SWE-bench and RepoEval.
- **Context enrichment:** Each chunk is annotated with:
  - **Scope chain:** e.g., `module::ClassName::method_name`
  - **Signature:** Full function/method signature with types
  - **Docstring/comments:** Extracted and associated
  - **Imports:** Which external symbols does this chunk reference?
  - **Siblings:** Signatures of adjacent methods in the same class/module
  - **File metadata:** Path, language, last modified, git blame info

#### 5.2.2 Indexing Engine

**Purpose:** Maintain multiple complementary index structures that stay fresh with incremental updates.

**Index types:**

1. **Full-text index (Tantivy):**
   - BM25 scoring with code-aware tokenization (camelCase splitting, snake_case splitting, identifier tokenization)
   - Fields: raw code, signatures, docstrings, file paths, scope chains
   - Supports phrase queries, wildcard queries, regex queries
   - Tantivy is ~2x faster than Lucene, pure Rust, supports incremental updates

2. **Vector index (embedded HNSW):**
   - Stores dense embeddings for each chunk (from code embedding models like `jina-embeddings-v2-base-code`, `voyage-code-3`, or `nomic-embed-code`)
   - Built-in HNSW implementation using the `hnsw_rs` or `instant-distance` crate to avoid external DB dependency
   - Optional: connect to external Qdrant instance for distributed deployments
   - Supports both code-to-code similarity and NL-to-code semantic search via dual embedding (NLP model + code model)

3. **Code graph index (petgraph + custom storage):**
   - Nodes: files, modules, classes, functions, types
   - Edges: `calls`, `imports`, `inherits`, `implements`, `references`, `contains`
   - Built from tree-sitter AST analysis (definition/reference extraction) + import resolution
   - Supports PageRank-style relevance ranking (à la Aider's repo-map approach)
   - Stored as adjacency lists with fast lookup by symbol name
   - Enables: "find all callers of X", "what depends on this module?", "trace the call chain from A to B"

4. **Symbol table (DashMap):**
   - Concurrent hash map of all symbols → definitions (file, line, byte range)
   - Provides instant go-to-definition without LSP overhead
   - Incrementally updated on file change

**Incremental update strategy:**

- File change detected → re-parse single file with tree-sitter (50–200ms) → diff old vs. new chunks → update only changed chunks in tantivy, vector index, and graph → transactional commit
- Target: <500ms from file save to fully updated index (vs. 60–120s for full SCIP re-index)
- Merkle-tree based change detection for efficient "has this chunk actually changed?" checks

#### 5.2.3 Retrieval Engine

**Purpose:** Given a query (natural language question, code snippet, symbol name, or agent task description), return the most relevant code chunks within a token budget.

**Retrieval pipeline (multi-stage):**

```
Query
  │
  ├──▶ [Stage 1: Parallel Candidate Fetch]
  │       ├── BM25 search (tantivy) → top-K₁ candidates
  │       ├── Vector search (HNSW) → top-K₂ candidates  
  │       ├── Graph walk (if query contains identifiers) → related nodes
  │       └── Symbol lookup (exact match on identifiers) → direct hits
  │
  ├──▶ [Stage 2: Fusion]
  │       ├── Reciprocal Rank Fusion (RRF) across BM25 + vector results
  │       ├── Graph boost: candidates connected to direct hits get score bonus
  │       └── Deduplication + Maximal Marginal Relevance (MMR, λ≈0.7)
  │
  ├──▶ [Stage 3: Reranking] (optional, for high-stakes queries)
  │       ├── Cross-encoder reranking (via ONNX runtime, e.g., ms-marco-MiniLM)
  │       └── Or: LLM-based reranking via API call (configurable)
  │
  └──▶ [Stage 4: Context Assembly]
          ├── Token budget allocation across selected chunks
          ├── Dependency expansion: pull in signatures of referenced symbols
          ├── Format as AI-optimized context block
          └── Return with metadata (file paths, line ranges, confidence scores)
```

**Retrieval strategies (configurable presets):**

| Strategy | Use Case | Methods Used | Typical Latency |
|---|---|---|---|
| `instant` | Autocomplete, inline suggestions | Symbol table + BM25 only | <10ms |
| `fast` | Chat Q&A, quick lookups | BM25 + vector, RRF fusion | <50ms |
| `thorough` | Complex code understanding tasks | Full pipeline + graph walk + reranking | <200ms |
| `deep` | Architecture analysis, cross-repo | Multi-hop graph traversal + deep search | <2s |

#### 5.2.4 Output Formatter (AI-Native)

**Purpose:** Format retrieved chunks to maximize LLM comprehension and minimize token waste.

**Output format per chunk:**

```
# {file_path}:{start_line}-{end_line}
# Language: {language}
# Scope: {module} > {class} > {method}
# Defines: {signature}
# Uses: {imported_symbols}
# Called by: {caller_signatures} (from graph)
# Relevance: {score} | Source: {retrieval_method}

{raw_code}
```

**Token budget management:**

- Caller specifies a total token budget (e.g., 8192 tokens)
- Engine uses tiktoken-rs to count tokens per chunk
- Greedy allocation: highest-relevance chunks first, then dependency-expand until budget exhausted
- Chunks that exceed individual size limits are trimmed to signature + key logic (using AST-aware truncation, never mid-expression)

**Context strategies:**

- **Focused:** Only the directly matching code. Best for narrow questions.
- **Contextual:** Matching code + caller/callee signatures + class outline. Best for understanding.
- **Architectural:** Module-level summaries + dependency graph excerpt. Best for planning.

---

## 6. Technical Specifications

### 6.1 Language & Runtime

- **Language:** Rust (stable toolchain, no `unsafe` in application code; vendored C deps for tree-sitter only)
- **Async runtime:** Tokio
- **Minimum Rust version:** 1.75+ (for async trait stability)

### 6.2 Key Dependencies (Crate Ecosystem)

| Component | Crate | Purpose |
|---|---|---|
| AST Parsing | `tree-sitter` + language grammars | Incremental, multi-language parsing |
| Full-text search | `tantivy` | BM25 scoring, inverted index, phrase/regex queries |
| Vector search | `hnsw_rs` or `instant-distance` | In-process approximate nearest neighbor |
| Graph storage | `petgraph` | Code dependency graph with traversal algorithms |
| Tokenization | `tiktoken-rs` | Token counting for LLM context budgets |
| File watching | `notify` | Cross-platform filesystem event monitoring |
| Git integration | `gix` (gitoxide) | Pure-Rust git operations (branch detection, blame, diff) |
| Concurrency | `dashmap`, `rayon` | Lock-free concurrent maps, parallel iteration |
| Serialization | `serde` + `bincode` | Fast binary serialization for index persistence |
| HTTP/gRPC | `axum` + `tonic` | API server |
| Embedding inference | `ort` (ONNX Runtime) | Local embedding model execution |
| CLI | `clap` | Command-line interface |
| Logging | `tracing` | Structured, async-compatible logging |

### 6.3 Supported Languages (Initial)

**Tier 1 (full AST chunking + graph + symbol resolution):**
Rust, Python, TypeScript/JavaScript, Go, Java, C/C++, C#

**Tier 2 (AST chunking + basic indexing):**
Ruby, PHP, Swift, Kotlin, Scala, Zig, Elixir, Lua, Bash, SQL

**Tier 3 (text-mode fallback):**
Any language with a tree-sitter grammar (40+ total), with graceful degradation

### 6.4 Performance Targets

| Metric | Target | Measurement |
|---|---|---|
| Index speed (initial) | >50K files/min | Cold index of Linux kernel (~28M LoC) |
| Incremental update | <500ms | Single file change → index updated |
| Branch switch | <5s | Full differential re-index on `git checkout` |
| Retrieval latency (fast) | <50ms p99 | Query → ranked results on 1M+ LoC codebase |
| Retrieval latency (instant) | <10ms p99 | Symbol lookup + BM25 on 1M+ LoC codebase |
| Memory usage | <2GB | Resident memory for 1M LoC indexed codebase |
| Binary size | <50MB | Statically linked, no external deps |
| Startup time | <1s | Cold start with persisted index |

### 6.5 Storage

- **Index persistence:** All indices (tantivy, vector, graph, symbol table) serialized to disk using `bincode` / tantivy's native format
- **Index location:** `.codixing/` directory in project root (gitignored by default)
- **Estimated disk usage:** ~500MB per 1M lines of code (all indices combined)
- **Cache invalidation:** File content hash (xxHash) compared on load; stale entries re-indexed

---

## 7. API & Integration Surface

### 7.1 CLI Interface

```bash
# Initialize and index a codebase
codixing init .
codixing init ./my-project --languages rust,python,typescript

# Search
codixing search "how does authentication work?"
codixing search "fn verify_token" --strategy instant
codixing search "payment processing flow" --strategy deep --budget 4096

# Inspect
codixing symbols --filter "pub fn" --file src/main.rs
codixing graph --node "AuthService" --depth 2
codixing stats

# Serve
codixing serve --port 8080
codixing serve --mcp  # MCP server mode for AI agents
```

### 7.2 MCP (Model Context Protocol) Server

First-class MCP integration for direct use by Claude Code, Cursor, and other MCP-compatible agents:

**Tools exposed:**

| Tool | Description |
|---|---|
| `code_search` | Hybrid search with configurable strategy and token budget |
| `find_symbol` | Exact symbol lookup with go-to-definition |
| `get_references` | Find all references to a symbol (from graph) |
| `get_callers` / `get_callees` | Call graph traversal |
| `get_dependencies` | Module/file dependency graph |
| `get_file_outline` | AST-based outline (signatures only) of a file |
| `get_context` | Retrieve contextual code around a specific location |
| `get_repo_map` | Aider-style repo map (symbol-level overview within token budget) |

### 7.3 REST / gRPC API

For integration into web services, IDE backends, CI pipelines:

```
POST /v1/search          { query, strategy, budget, filters }
GET  /v1/symbols/:name   Symbol lookup
GET  /v1/graph/:node     Graph traversal
POST /v1/index/refresh   Trigger re-index
GET  /v1/health          Health check + index stats
WS   /v1/stream          WebSocket for real-time index updates
```

### 7.4 Rust Library (crate)

```rust
use codixing::{Engine, SearchQuery, Strategy};

let engine = Engine::open("./my-project").await?;

let results = engine.search(SearchQuery {
    query: "how does payment processing work?",
    strategy: Strategy::Thorough,
    token_budget: 8192,
    languages: Some(vec!["rust", "python"]),
    file_filter: Some("src/**"),
    ..Default::default()
}).await?;

for chunk in results.chunks {
    println!("{}:{}-{} (score: {:.2})", 
        chunk.file_path, chunk.start_line, chunk.end_line, chunk.score);
    println!("{}", chunk.contextualized_text);
}
```

---

## 8. Embedding Strategy

### 8.1 Local-First Approach

Codixing runs embedding inference locally by default using ONNX Runtime:

- **Default model:** `nomic-embed-code` or `jina-embeddings-v2-base-code` (open weights, 8192 token context)
- **Quantized:** INT8 quantization for ~4x speedup with <1% quality loss
- **Batch processing:** Chunks are batched and processed in parallel during indexing
- **Lazy embedding:** Embeddings are computed asynchronously; BM25 + symbol search work immediately while embeddings are still being generated

### 8.2 Remote Embedding Support

For users who prefer API-based embeddings:

- Configurable endpoint for OpenAI, Voyage, Cohere, or custom embedding APIs
- Built-in request batching and rate limiting
- Cached: each chunk's embedding is stored alongside its content hash; re-computed only on content change

### 8.3 Dual-Embedding Architecture

Following the bloop/Qdrant approach:

- **NLP embedding:** Encodes the chunk's natural-language representation (docstrings, enriched description) for natural language queries
- **Code embedding:** Encodes the raw code for code-to-code similarity
- Query routing: NL queries hit the NLP index; code queries hit the code index; ambiguous queries hit both with RRF fusion

---

## 9. Code Graph Details

### 9.1 Graph Construction

The code graph is built from tree-sitter AST analysis without requiring a full compiler or LSP:

1. **Definition extraction:** Identify all symbol definitions (functions, classes, types, constants) with their fully-qualified names
2. **Reference extraction:** Identify all symbol references within each definition's body
3. **Import resolution:** Parse import statements to resolve cross-file references (language-specific resolvers for Tier 1 languages)
4. **Edge creation:** Create edges from reference → definition (calls, uses, imports, inherits, implements)

### 9.2 Graph Queries

- **PageRank relevance:** Given a set of "seed" files (e.g., currently open files), compute personalized PageRank to find the most architecturally relevant files
- **Neighborhood expansion:** Given a symbol, expand to N-hop neighbors in the call/dependency graph
- **Path finding:** Shortest path between two symbols (useful for "how does A reach B?")
- **Impact analysis:** Given a changed symbol, find all transitive dependents

### 9.3 Repo Map Generation

Inspired by Aider's approach — the most token-efficient retrieval method in the literature (4.3–6.5% utilization):

- Generate a compact "map" of the entire repo showing only symbol signatures and dependency edges
- Use binary search to fit the map within a configurable token budget
- Prioritize symbols by PageRank score and recency of modification
- Cache the map and update incrementally

---

## 10. Development Roadmap

### Phase 1: Foundation — COMPLETE (February 2026)

**Goal:** Core indexing and basic retrieval working end-to-end.
**Result:** 111 tests (97 unit + 14 integration), 0 clippy warnings, all 10 language variants working.

- [x] Project scaffold with Cargo workspace (core, cli, server crates)
- [x] Tree-sitter integration with Tier 1 language grammars (10 variants: Rust, Python, TS, TSX, JS, Go, Java, C, C++, C#)
- [x] AST chunker implementing cAST algorithm (recursive split-then-merge)
- [x] Tantivy integration with code-aware tokenizer (camelCase/snake_case/dot.path splitting)
- [x] Symbol table (DashMap-based, with bitcode persistence)
- [x] CLI: `init`, `search` (BM25 only), `symbols`
- [x] File watcher with incremental re-parse (notify, 100ms debounce)
- [x] Index persistence to `.codixing/` directory (JSON config/meta, bitcode symbols/hashes, tantivy native)
- [x] Integration test suite: multi-language indexing, BM25 search accuracy, chunker boundary verification, watcher lifecycle

### Phase 2: Semantic Search (Months 3–5)

**Goal:** Vector search and hybrid retrieval operational.

- [ ] ONNX Runtime integration for local embeddings
- [ ] HNSW vector index with incremental updates
- [ ] Dual-embedding pipeline (NLP + code)
- [ ] Hybrid retrieval: BM25 + vector with RRF fusion
- [ ] MMR deduplication
- [ ] Token budget management with tiktoken-rs
- [ ] Context enrichment (scope chains, signatures, imports)
- [ ] AI-optimized output formatter
- [ ] REST API server (axum)

### Phase 3: Graph Intelligence (Months 5–7)

**Goal:** Code graph unlocks structural understanding.

- [ ] Definition/reference extraction from tree-sitter ASTs
- [ ] Import resolvers for Tier 1 languages
- [ ] petgraph-based code graph with persistence
- [ ] PageRank scoring for file/symbol relevance
- [ ] Repo map generation (Aider-style)
- [ ] Graph-boosted retrieval (graph signal fused into ranking)
- [ ] CLI: `graph`, `callers`, `callees`, `dependencies`
- [ ] Incremental graph updates on file change

### Phase 4: Agent Integration (Months 7–9)

**Goal:** First-class AI agent support.

- [ ] MCP server implementation (full tool suite)
- [ ] gRPC API for high-performance integrations
- [ ] Retrieval strategy presets (instant, fast, thorough, deep)
- [ ] Cross-encoder reranking (optional, via ONNX)
- [ ] Multi-repo support (index and query across multiple repositories)
- [ ] Git-aware features (branch-relative search, blame integration, diff-aware re-indexing)
- [ ] WebSocket streaming for real-time updates

### Phase 5: Production Hardening (Months 9–12)

**Goal:** Enterprise-ready reliability and performance.

- [ ] Comprehensive benchmarks against Aider, Cline, Cursor retrieval layers
- [ ] Fuzzing and property-based testing
- [ ] Memory profiling and optimization
- [ ] Cross-platform CI (Linux, macOS, Windows)
- [ ] Documentation: architecture guide, API reference, integration tutorials
- [ ] Plugin/extension system for custom retrieval strategies
- [ ] Tier 2 language support
- [ ] Optional Qdrant backend for distributed deployments
- [ ] Telemetry and observability (OpenTelemetry)

---

## 11. Competitive Landscape

| Tool | Approach | Strengths | Codixing Advantage |
|---|---|---|---|
| **Aider repo-map** | Graph-based (tree-sitter + PageRank) | Most token-efficient (4.3–6.5% utilization), no GPU needed | Codixing adds semantic search + hybrid fusion on top of the same graph approach |
| **Sourcegraph Cody** | Multi-repo semantic search | Enterprise-grade, cross-repo context | Codixing is open-source, local-first, single binary |
| **Cursor** | Hybrid semantic-lexical indexing | IDE-integrated, good UX | Codixing is standalone, embeddable, not locked to an IDE |
| **Cline** | ripgrep + fzf + tree-sitter AST | Transparent, lightweight | Codixing adds vector search, graph intelligence, reranking |
| **Augment Code** | Deep context engine | Strong cross-repo reasoning | Codixing is open-source and self-hosted |
| **Qodo** | RAG pipeline with NL-enriched chunks | Enterprise scale with continuous indexing | Codixing offers more retrieval strategies and graph-based understanding |
| **Narsil-MCP** | Rust MCP server, 90 tools, tree-sitter | Comprehensive code intelligence | Codixing focuses deeper on retrieval quality with hybrid search + embeddings |
| **code-splitter** | Rust crate, tree-sitter chunking | Simple, reusable | Codixing is a full engine, not just a chunker |

---

## 12. Key Research References

These papers and projects directly inform Codixing's design:

1. **cAST (2025)** — AST-based chunking for code RAG. Shows 4.3pt gain on RepoEval, 2.7pt on SWE-bench over line-based chunking. Directly informs our chunking algorithm.

2. **Aider's repo-map architecture** — Graph-based retrieval using tree-sitter + PageRank achieves 4.3–6.5% context utilization (best in class). No embeddings, no GPU. Informs our graph layer.

3. **"An Exploratory Study of Code Retrieval Techniques in Coding Agents" (Preprints.org, 2025)** — Comparative analysis of Aider, Cline, Cursor retrieval approaches. Key finding: graph-based > hybrid semantic-lexical > pure lexical for context efficiency.

4. **Code-Craft: Hierarchical Graph-Based Code Summarization (2025)** — Uses LSP + hierarchical graph + context-aware embeddings. Shows that contextual embeddings (with scope/caller info) significantly outperform isolated code embeddings.

5. **LDAR (2025)** — Demonstrates that selecting continuous "bands" of passages (vs. independent top-k) uses 25–63% fewer tokens while maintaining quality. Informs our context assembly strategy.

6. **MEM1 (2025)** — RL framework for long multi-turn agents with constant memory. Shows that learned internal state handling can replace expanding context windows. Relevant for agent-side integration.

7. **Bloop + Qdrant case study** — Rust-native semantic code search using tantivy (lexical) + Qdrant (vector). Demonstrates sub-second search on 2.8M LoC Rust codebase. Validates our tech stack choices.

8. **Hybrid retrieval + RRF + reranking (multiple sources, 2024–2025)** — Consistent evidence that BM25 + vector + RRF + cross-encoder reranking outperforms any single method across domains.

---

## 13. Success Metrics

| Metric | Definition | Target |
|---|---|---|
| **Retrieval Recall@10** | % of relevant chunks in top 10 results | >85% on RepoEval benchmark |
| **Context Utilization** | Useful tokens / total tokens sent to LLM | <10% (lower is better) |
| **SWE-bench Impact** | Improvement in SWE-bench resolve rate when Codixing feeds the agent vs. baseline | >3pt improvement |
| **Indexing Throughput** | Files indexed per second (cold start) | >800 files/s |
| **Query Latency p99** | 99th percentile retrieval time | <50ms (fast strategy) |
| **Incremental Update** | Time from file save to index consistency | <500ms |
| **Adoption** | GitHub stars / downloads / MCP integrations | 1K stars in first 6 months |

---

## 14. Risks & Mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Embedding model quality varies across languages | High | Medium | Default to well-benchmarked models; support model swapping; lean on BM25+graph for underserved languages |
| Tree-sitter grammar gaps for niche languages | Medium | Low | Graceful degradation to text-mode; community grammar contributions |
| HNSW in-process scalability limit (~10M vectors) | Medium | Medium | Offer Qdrant backend for large deployments; shard by repo |
| Import resolution complexity (dynamic imports, monkeypatching) | High | Medium | Best-effort resolution; flag unresolved references; support LSP fallback |
| Cross-encoder reranking adds latency | Low | Low | Optional; only in `thorough`/`deep` strategies; quantized models |
| Context window sizes keep growing (10M+ tokens) | Medium | Medium | Codixing still valuable for quality over quantity; even with 10M tokens, 1M LoC doesn't fit; precision always matters |

---

## 15. Open Questions

1. **Embedding model selection:** Should we ship a default model bundled in the binary (increases size to ~200MB) or require a one-time download on first run?

2. **Graph persistence format:** petgraph's native serialization vs. a custom adjacency-list format optimized for incremental updates and memory-mapped access?

3. **Multi-repo federation:** Single unified index vs. per-repo indices with federated search? The former is simpler; the latter scales better.

4. **LSP integration:** Should Codixing optionally spin up LSP servers for Tier 1 languages to get compiler-accurate symbol resolution? Research shows mixed results (Claude Code's LSP experiment showed 8.5% higher token consumption without quality improvement), but it could benefit complex dependency analysis.

5. **Chunk summarization:** Should each chunk have an LLM-generated natural language summary stored alongside it (like Qodo's approach)? Improves NL search quality but adds indexing cost and API dependency.

---

## Appendix A: Glossary

- **AST** — Abstract Syntax Tree. Hierarchical representation of code structure.
- **BM25** — Best Match 25. Probabilistic ranking function for keyword search.
- **cAST** — Chunking via Abstract Syntax Trees. Research technique from CMU (2025).
- **CST** — Concrete Syntax Tree. Full parse tree including whitespace and punctuation (what tree-sitter produces).
- **HNSW** — Hierarchical Navigable Small World. Algorithm for approximate nearest neighbor search.
- **MCP** — Model Context Protocol. Anthropic's protocol for AI agent ↔ tool communication.
- **MMR** — Maximal Marginal Relevance. Technique to balance relevance and diversity in search results.
- **RRF** — Reciprocal Rank Fusion. Method for combining ranked lists from multiple retrieval systems.
- **SWE-bench** — Software Engineering benchmark. Standard evaluation for AI coding agents.

---

*This document is a living artifact. It will be updated as research progresses and implementation begins.*

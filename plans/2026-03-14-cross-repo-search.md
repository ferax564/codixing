# Phase 15: Cross-Repo Search -- FederatedEngine Design

**Date:** 2026-03-14
**Status:** Design (not yet implemented)

---

## Motivation

Today a single `Engine` instance is bound to exactly one project root (plus
optional `extra_roots` that get merged into the same index). This works well
for monorepos, but many real-world setups involve several sibling
repositories that depend on each other:

- A backend API server + a shared library + a frontend SPA
- A Rust workspace spread across multiple git repos linked via `path = "../"`
- A Python microservice cluster where services `pip install -e ../common`
- A Cargo workspace + a companion `codegen` repo producing generated code

When an LLM agent needs to trace a call from the API server into the shared
library, it currently has to open a separate MCP server for each project and
mentally fuse the results. Phase 15 eliminates that friction by introducing a
`FederatedEngine` that wraps multiple `Engine` instances and presents a
single, unified search surface.

---

## Codebase Architecture Summary

The design below is grounded in a detailed reading of the current codebase.
Key observations:

### Engine lifecycle

`Engine` is created via two paths (both in `crates/core/src/engine/mod.rs`):

1. **`Engine::init(root, config)`** -- full indexing from scratch. Walks
   source files, parses in parallel with rayon, chunks with cAST, indexes
   into Tantivy, optionally batch-embeds into an HNSW vector index, builds a
   `CodeGraph` with import + call edges, computes PageRank, and persists
   everything to `.codixing/`.

2. **`Engine::open(root)`** -- hot-loads a pre-built index from `.codixing/`.
   Restores Tantivy, symbols, chunk metadata, vector index, and graph from
   disk. Much faster than `init` (milliseconds vs. seconds).

Both paths produce the same struct:

```text
Engine {
    config:           IndexConfig,          // root, extra_roots, embedding, graph, ...
    store:            IndexStore,           // .codixing/ directory layout
    parser:           Parser,
    tantivy:          TantivyIndex,         // BM25 full-text index
    symbols:          SymbolTable,          // name -> definitions
    file_chunk_counts: HashMap<String, usize>,
    embedder:         Option<Arc<Embedder>>,
    vector:           Option<VectorIndex>,  // HNSW ANN index
    chunk_meta:       DashMap<u64, ChunkMeta>,
    graph:            Option<CodeGraph>,    // petgraph DiGraph + path lookup
    reranker:         Option<Arc<Reranker>>,
    session:          Arc<SessionState>,
}
```

### Search pipeline

`Engine::search(query)` in `crates/core/src/engine/search.rs` selects a
strategy based on `SearchQuery.strategy`:

| Strategy  | Pipeline                                         |
|-----------|--------------------------------------------------|
| Instant   | BM25 only                                        |
| Fast      | BM25 + vector + asymmetric RRF fusion             |
| Thorough  | hybrid + MMR deduplication                       |
| Explore   | BM25 broad-fetch + graph-expansion (callers/callees) |
| Deep      | hybrid + BGE-Reranker-Base cross-encoder          |

Post-search, every strategy applies `apply_graph_boost` (PageRank),
`apply_definition_boost` (symbol-table lookup), `apply_test_demotion`, and
`dedup_overlapping`.

The `HybridRetriever` in `crates/core/src/retriever/hybrid.rs` uses
`rrf_fuse_asymmetric()` to merge BM25 and vector ranked lists, with the
asymmetry direction chosen by `is_identifier_query()`.

### MCP server

`crates/mcp/src/main.rs` wraps the engine in `Arc<RwLock<Engine>>` and
exposes it over JSON-RPC (stdin/stdout or Unix socket daemon).
`dispatch_tool()` in `crates/mcp/src/tools/mod.rs` routes `tools/call`
requests to handler functions that take `&mut Engine`.

### Dependency graph

`CodeGraph` in `crates/core/src/graph/mod.rs` is a `petgraph::DiGraph` with:
- **Nodes**: `CodeNode { file_path, language, pagerank, in_degree, out_degree }`
- **Edges**: `CodeEdge { raw_import, kind: Resolved | External | Calls }`

Import resolution lives in `ImportResolver` (`graph/resolver.rs`), which
resolves raw import strings against the set of indexed file paths. It has
per-language strategies for Rust, Python, JS/TS, Go, Java, C/C++, C#, Ruby,
Kotlin, and Scala.

### Persistence

`IndexStore` (`crates/core/src/persistence/mod.rs`) manages the `.codixing/`
directory with:
- `config.json` -- `IndexConfig`
- `meta.json` -- `IndexMeta` (version, counts, git commit)
- `tantivy/` -- BM25 index files
- `symbols.bin` -- bitcode-serialized symbol table
- `chunk_meta.bin` -- bitcode-serialized chunk metadata
- `vectors/` -- `index.usearch` + `file_chunks.bin`
- `graph/graph.bin` -- bitcode-serialized `GraphData`
- `tree_hashes.bin` -- content hashes for change detection

### Existing multi-root support

`IndexConfig.extra_roots` already allows indexing files from additional
directories into the same engine. Files from extra roots are prefixed with
the directory's base name (e.g., `shared-lib/src/types.rs`). This is useful
for co-indexing a local dependency, but all data lives in a single Tantivy
index + single vector index + single graph -- which means:

1. The extra root must be re-indexed whenever the main project is, and
2. Two projects cannot share an already-built index for the extra root.

The `FederatedEngine` takes a different approach: each repo keeps its own
independent `.codixing/` index, and federation happens at query time.

---

## FederatedEngine Architecture

### Core Concept

```text
FederatedEngine
  |
  +-- Engine[0]  (project-a)  -- .codixing/ in /home/user/project-a
  +-- Engine[1]  (project-b)  -- .codixing/ in /home/user/project-b
  +-- Engine[2]  (project-c)  -- .codixing/ in /home/user/project-c
  |
  +-- BridgeGraph             -- cross-repo import edges
  +-- FederationConfig        -- discovery, boost weights, routing
```

Each engine maintains its own independent index, graph, and session. The
`FederatedEngine` fans out queries to all (or a filtered subset of) engines,
collects results, and fuses them into a single ranked list.

### API Surface

```rust
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use codixing_core::{Engine, IndexConfig, SearchQuery, SearchResult};

/// Configuration for the federated search layer.
pub struct FederationConfig {
    /// Root directories of the projects to federate.
    pub roots: Vec<PathBuf>,

    /// Per-project boost weight multiplier applied before RRF fusion.
    /// Key is the project root's base name (e.g., "shared-lib").
    /// Projects not listed default to 1.0.
    pub project_weights: HashMap<String, f32>,

    /// RRF constant for cross-project result fusion.
    pub rrf_k: f32,

    /// Whether to lazily load engines on first query rather than at startup.
    pub lazy_load: bool,

    /// Maximum number of engines to keep resident in memory.
    /// When exceeded, the least-recently-queried engine is evicted.
    /// 0 = no limit (keep all loaded).
    pub max_resident: usize,
}

impl Default for FederationConfig {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            project_weights: HashMap::new(),
            rrf_k: 60.0,
            lazy_load: true,
            max_resident: 5,
        }
    }
}

/// A search result annotated with its source project.
pub struct FederatedResult {
    /// The underlying search result (chunk content, file path, score, etc.).
    pub result: SearchResult,

    /// Base name of the project this result came from.
    /// E.g., "project-a" for root "/home/user/project-a".
    pub project: String,

    /// Absolute path to the project root.
    pub project_root: PathBuf,
}

/// Wraps multiple Engine instances and provides unified cross-repo search.
pub struct FederatedEngine {
    /// Individual project engines. RwLock allows concurrent reads with
    /// exclusive access for sync/reindex. Option enables lazy loading:
    /// None = known root but engine not yet loaded.
    engines: Vec<(PathBuf, Arc<RwLock<Option<Engine>>>)>,

    /// Cross-project dependency edges (detected bridge imports).
    bridge_graph: BridgeGraph,

    /// Federation-level configuration.
    config: FederationConfig,

    /// LRU tracking for engine eviction (index into `engines` vec).
    lru_order: Mutex<VecDeque<usize>>,
}

impl FederatedEngine {
    /// Create a federated engine from a list of project roots.
    ///
    /// If `config.lazy_load` is true, engines are not opened until the
    /// first query touches them. Otherwise, all engines are opened
    /// immediately (fail-fast on corrupt indices).
    pub fn new(config: FederationConfig) -> Result<Self>;

    /// Add a new project root to the federation at runtime.
    ///
    /// The engine is opened (or lazily registered) and its bridge edges
    /// are detected against all existing projects.
    pub fn add_project(&mut self, root: PathBuf) -> Result<()>;

    /// Remove a project from the federation.
    pub fn remove_project(&self, root: &Path) -> Result<()>;

    /// Search across all federated projects.
    ///
    /// Fans out the query to each engine, collects results, applies
    /// per-project boost weights, and fuses via RRF.
    pub fn search(&self, query: SearchQuery) -> Result<Vec<FederatedResult>>;

    /// Search a specific project only (bypass federation).
    pub fn search_project(
        &self,
        project: &str,
        query: SearchQuery,
    ) -> Result<Vec<SearchResult>>;

    /// Find symbols across all projects.
    pub fn find_symbol(
        &self,
        name: &str,
        file: Option<&str>,
    ) -> Result<Vec<(String, Symbol)>>;

    /// Return callers/callees that span project boundaries (bridge edges).
    pub fn cross_repo_references(&self, file: &str) -> Vec<BridgeEdge>;

    /// Return aggregate statistics across all loaded engines.
    pub fn stats(&self) -> FederatedStats;

    /// List all registered projects with their load status.
    pub fn projects(&self) -> Vec<ProjectInfo>;
}
```

---

## Key Design Decisions

### 1. Discovery: How does the federated engine discover repos?

Three discovery modes, from most explicit to most automatic:

**a) Explicit config file** (Phase 15a -- primary mode)

A `codixing-federation.json` file in a user-chosen directory:

```json
{
    "projects": [
        { "root": "/home/user/api-server" },
        { "root": "/home/user/shared-lib", "weight": 1.2 },
        { "root": "/home/user/frontend" }
    ],
    "rrf_k": 60.0,
    "lazy_load": true,
    "max_resident": 5
}
```

The MCP server accepts a `--federation` flag pointing to this file. This is
deterministic, requires no heuristics, and works for any project layout.

**b) Parent-directory auto-discovery** (Phase 15c)

Walk the parent of the primary root and discover sibling directories
containing `.codixing/`. For example, if the MCP server is started with
`--root /home/user/api-server`, scan `/home/user/*/` for `.codixing/`
directories and auto-register them.

Opt-in via `--discover-siblings` flag. Risky because it might find unrelated
projects; mitigated by language overlap filtering (see routing below).

**c) Dependency-file parsing** (Phase 15b)

Parse `Cargo.toml`, `pyproject.toml`, `package.json`, etc. for local `path`
dependencies pointing to sibling repos. This is the most accurate auto-
discovery method but requires per-ecosystem parsing.

### 2. Query routing: Should all repos be searched or only relevant ones?

**Default: broadcast to all loaded engines.** With lazy loading and LRU
eviction, only `max_resident` engines are warm at any time. Querying a cold
engine triggers a load (first-query latency hit, ~30 ms for BM25-only).

**Smart routing** (Phase 15c) filters the broadcast set:

- **Language overlap**: If a Python query hits only Python results on the
  primary engine, skip engines that index no Python files. Detectable from
  `IndexMeta.languages` or the file extension distribution.

- **Session affinity**: Projects the agent has interacted with in the
  current session get priority. The `SessionState` already tracks file reads
  and searches; extend this to track project-level activity.

- **Bridge-edge affinity**: If the primary project's graph has bridge edges
  into project B but not project C, route the query to B only.

### 3. Result fusion: Standard RRF with per-project boost weights

The same `rrf_fuse_asymmetric` machinery already in `retriever/hybrid.rs`
is reused at the federation level, with one ranked list per project:

```text
score(chunk) = SUM over projects P where chunk appears:
    project_weight[P] * (1 / (k + rank_in_P))
```

Per-project weights allow boosting the "primary" project (where the user
is actively coding) above auxiliary reference repos.

Additionally:

- **Session boost propagation**: The session's file-activity boost is applied
  per-engine before results leave each engine, so recently-touched files in
  any project get a natural lift.

- **Definition priority**: When searching for a symbol, the project that
  *defines* it should rank above projects that merely *use* it. This is
  already handled per-engine by `apply_definition_boost`; at the federation
  level, we apply an additional 1.5x boost to results from the engine whose
  symbol table contains a defining occurrence.

### 4. Bridge edges: Cross-repo import detection

A `BridgeGraph` stores edges between files in different projects:

```rust
pub struct BridgeEdge {
    /// Source file (project-relative path).
    pub from_file: String,
    /// Source project name.
    pub from_project: String,
    /// Target file (project-relative path).
    pub to_file: String,
    /// Target project name.
    pub to_project: String,
    /// How the edge was detected.
    pub kind: BridgeKind,
}

pub enum BridgeKind {
    /// Rust: `path = "../sibling-crate"` in Cargo.toml
    CargoPath,
    /// Python: `pip install -e ../common` or sys.path manipulation
    PythonEditable,
    /// JS/TS: `"shared-lib": "file:../shared-lib"` in package.json
    NpmLink,
    /// Go: `replace` directive in go.mod
    GoReplace,
    /// Generic: user-declared in federation config
    Manual,
}
```

**Detection strategy (Phase 15b):**

1. **Manifest parsing**: Read `Cargo.toml` for `path = "../..."` deps,
   `package.json` for `file:../...` deps, `go.mod` for `replace` directives,
   `pyproject.toml` for editable installs.

2. **External edge matching**: Each engine's `CodeGraph` already stores
   `EdgeKind::External` edges for unresolved imports. In federation mode,
   check if an external import from engine A matches an indexed file in
   engine B. E.g., engine A has `import shared_lib.utils` as External;
   engine B's root is named `shared_lib` and has `utils.py` indexed.

3. **Symbol cross-reference**: After loading all engines, iterate symbols
   exported by each project and check if other projects' external import
   paths reference those symbol names.

Bridge edges enable:
- `cross_repo_references(file)` returning callers/callees in other projects
- Graph-expanded `Explore` strategy that follows bridge edges across repos
- Impact analysis (`predict_impact`) spanning multiple projects

### 5. MCP integration

**Approach: extend existing tools rather than adding a separate tool.**

The MCP server already has a single `Engine` behind `Arc<RwLock<Engine>>`.
In federated mode, the server creates a `FederatedEngine` instead and adapts
the tool dispatch.

**Option A (recommended): transparent federation**

Modify `dispatch_tool` to detect whether the engine is federated. For
`code_search`, `find_symbol`, `search_usages`, and `grep_code`, the handler
calls `FederatedEngine::search()` instead of `Engine::search()`. Results
include a `[project-name]` prefix in the file path for disambiguation.

This means the LLM does not need to learn new tool names -- existing prompts
that use `code_search` automatically gain cross-repo coverage.

**Option B: opt-in `project` parameter**

Add an optional `project` parameter to `code_search` and `find_symbol`:

```json
{
    "name": "code_search",
    "arguments": {
        "query": "compute_pagerank",
        "project": "shared-lib"
    }
}
```

When `project` is omitted, search all. When specified, search only that
project. This gives the LLM explicit control over scope.

**Recommended: implement both.** Default is broadcast (Option A); explicit
`project` parameter (Option B) provides override when the agent knows which
repo to target.

New tools added for federation management:

```json
{
    "name": "list_projects",
    "description": "List all projects in the federation with index stats."
},
{
    "name": "cross_repo_refs",
    "description": "Show cross-project callers/callees for a file.",
    "inputSchema": {
        "properties": {
            "file": { "type": "string" },
            "project": { "type": "string" }
        }
    }
}
```

### 6. Memory/performance: Lazy loading with LRU eviction

Opening N engines means:
- N Tantivy mmap'd indices (lightweight, OS manages pages)
- N HNSW vector indices in memory (~4 bytes per dim per vector, or ~1 byte with int8 quantization)
- N `CodeGraph` petgraph instances
- N `SymbolTable` DashMaps
- N embedder models -- **shared**: all engines sharing the same model config
  can use a single `Arc<Embedder>` (the embedding model is stateless)

**Strategy:**

1. **Lazy loading**: Engines are opened on first query, not at startup.
   The `engines` vec stores `Option<Engine>` and loads on demand.

2. **LRU eviction**: When `engines.len() > max_resident`, drop the
   least-recently-queried engine. Since all state is persisted to `.codixing/`,
   re-opening is fast (~30 ms for BM25, ~2 s with embedder).

3. **Shared embedder**: The `Embedder` is `Send + Sync` behind an `Arc`.
   The federated engine loads one embedder instance and injects it into each
   child engine on open. This avoids loading the ONNX model N times.

4. **Parallel fan-out**: Queries are dispatched to each engine in parallel
   using rayon or tokio `spawn_blocking` (depending on whether we're in the
   MCP async context). Results are collected and fused.

**Memory estimate for 5 projects (each ~100 files, 700 chunks):**

| Component     | Per-engine | 5 engines | Shared?   |
|---------------|------------|-----------|-----------|
| Tantivy mmap  | ~2 MB RSS  | ~10 MB    | No        |
| Vector (int8) | ~0.3 MB    | ~1.5 MB   | No        |
| Symbols       | ~1 MB      | ~5 MB     | No        |
| Graph         | ~0.5 MB    | ~2.5 MB   | No        |
| Chunk meta    | ~2 MB      | ~10 MB    | No        |
| Embedder      | ~120 MB    | ~120 MB   | **Yes**   |
| **Total**     |            | **~149 MB** |         |

This is well within acceptable limits. The embedder dominates and is shared.

---

## Implementation Phases

### Phase 15a: Basic federation (fan-out + RRF)

**Goal:** A `FederatedEngine` that opens multiple `Engine` instances and
returns merged search results.

**Files to create/modify:**
- `crates/core/src/federation/mod.rs` -- `FederatedEngine`, `FederationConfig`, `FederatedResult`
- `crates/core/src/federation/config.rs` -- config parsing, `codixing-federation.json`
- `crates/core/src/federation/fusion.rs` -- multi-list RRF fusion
- `crates/core/src/lib.rs` -- add `pub mod federation;` and re-exports
- `crates/mcp/src/main.rs` -- `--federation` flag, `FederatedEngine` creation
- `crates/mcp/src/tools/mod.rs` -- adapt `dispatch_tool` for federation

**Key implementation details:**

1. `FederatedEngine::new()` reads `codixing-federation.json`, opens each
   root with `Engine::open()` (or defers if `lazy_load`), validates that
   each root has a `.codixing/` directory.

2. `FederatedEngine::search()`:
   - Fan out query to each loaded engine (sequential in 15a, parallel in 15c).
   - Prepend `project_name/` to each result's `file_path` for disambiguation.
   - Apply per-project weight to each result's score.
   - Fuse all per-project ranked lists using multi-list RRF:
     ```
     score(d) = SUM_p weight[p] / (k + rank_in_p + 1)
     ```
   - Sort by fused score, truncate to `query.limit`.

3. MCP server detects `--federation` flag and wraps the primary engine plus
   all federation targets in a `FederatedEngine`. The tool dispatch layer
   transparently delegates to `FederatedEngine::search()`.

**Exit criteria:** `code_search` with `--federation config.json` returns
results from multiple projects, correctly prefixed and ranked.

### Phase 15b: Bridge edge detection

**Goal:** Detect cross-repo imports by parsing build manifests and matching
external edges.

**Files to create/modify:**
- `crates/core/src/federation/bridge.rs` -- `BridgeGraph`, `BridgeEdge`, manifest parsers
- `crates/core/src/federation/mod.rs` -- wire bridge detection into `FederatedEngine`

**Key implementation details:**

1. After all engines are loaded, iterate each engine's `CodeGraph` external
   edges. For each `EdgeKind::External` edge with import path P:
   - Check if P matches any project's root name or known package name.
   - If yes, resolve P against that project's indexed files.
   - Record a `BridgeEdge`.

2. Parse `Cargo.toml` for `path = "../..."` dependencies:
   ```toml
   [dependencies]
   shared-lib = { path = "../shared-lib" }
   ```
   Map the crate name to the target project root.

3. Parse `package.json` for `file:../...` dependencies.

4. Parse `go.mod` for `replace` directives with local paths.

5. `cross_repo_references(file)` returns bridge edges touching that file,
   enabling the LLM to trace calls across projects.

**Exit criteria:** `cross_repo_refs` tool returns bridge edges for a file
that imports from a sibling project.

### Phase 15c: Smart query routing

**Goal:** Reduce fan-out cost by routing queries only to relevant projects.

**Files to modify:**
- `crates/core/src/federation/mod.rs` -- add routing logic
- `crates/core/src/federation/router.rs` -- `QueryRouter` trait + implementations

**Key implementation details:**

1. **Language filter**: If the query is identified as language-specific (e.g.,
   file filter contains `.py`), skip engines that have no Python files.

2. **Bridge affinity**: If the primary project has bridge edges into project
   B, include B; exclude unconnected projects for graph-expanding strategies.

3. **Session affinity**: Projects accessed in the last 5 minutes are always
   included. Dormant projects are excluded unless the query is very broad.

4. **Parallel fan-out**: Switch from sequential to parallel query dispatch
   using `rayon::scope` (the engines' `search()` methods are CPU-bound and
   use rayon internally, but each engine's rayon work is independent).

**Exit criteria:** Queries that clearly target one language skip irrelevant
engines; latency is no worse than single-engine search + 10% overhead.

### Phase 15d: MCP tool integration polish

**Goal:** Full MCP tool coverage for federation, including all graph tools.

**Files to modify:**
- `crates/mcp/src/tools/search.rs` -- federated `code_search`, `find_symbol`
- `crates/mcp/src/tools/graph.rs` -- federated `get_references`, `predict_impact`
- `crates/mcp/src/tools/mod.rs` -- `list_projects`, `cross_repo_refs` tools
- `crates/mcp/src/tools/files.rs` -- `read_file` with `project:` prefix

**Key implementation details:**

1. `read_file` with project prefix: `read_file { file: "shared-lib/src/types.rs" }`
   routes to the `shared-lib` engine and reads from its root.

2. `predict_impact` across projects: compute impact within each project,
   then extend via bridge edges to find impacted files in sibling projects.

3. `get_repo_map` in federated mode: return a combined repo map with project
   sections, each budgeted proportionally to project weight.

4. `list_projects` tool: return project name, root path, load status, file
   count, language breakdown for each registered project.

**Exit criteria:** All existing MCP tools work transparently in federated
mode; LLM agents can trace calls and impact across project boundaries.

---

## Risk Assessment

### Memory usage with N projects open

**Risk:** Medium.
**Mitigation:** LRU eviction with configurable `max_resident` (default 5).
The embedder model (~120 MB) is shared across all engines. With int8
quantized vectors, 5 engines at ~100 files each add only ~30 MB beyond the
shared model. For very large projects (10K+ files), vector indices grow to
tens of MB; LRU eviction keeps total RSS bounded.

### Cold start time

**Risk:** Low with lazy loading; Medium without.
**Mitigation:** `lazy_load: true` (default) defers engine open until first
query. Opening a BM25-only engine is ~30 ms. Opening with embedder is ~2 s
(model load), but the embedder is loaded once and shared. Worst case: user
queries 5 new projects in rapid succession, each taking ~30 ms to open =
150 ms total. Acceptable for an MCP tool call.

### Result quality degradation from mixing unrelated repos

**Risk:** Medium. Fusing results from a TypeScript frontend and a Rust
backend for a query like "handle authentication" would surface irrelevant
chunks from the wrong ecosystem.

**Mitigation:**
1. Per-project weights let the user boost the primary project.
2. Language-aware routing (Phase 15c) skips engines with no language overlap.
3. Definition boost at the federation level: if the query matches a symbol
   defined in project A but only used in project B, project A results rank
   higher.
4. The `project` parameter on `code_search` lets the LLM explicitly scope
   to one project when it knows the target.

### Index staleness across projects

**Risk:** Low. Each engine has its own `.codixing/` index and can be synced
independently. The daemon's file watcher only watches the primary project.

**Mitigation:** In daemon mode with federation, start a file watcher per
loaded engine (using `FileWatcher::new` for each project root). Budget
watcher threads proportionally (e.g., max 3 concurrent watchers; rotate
watchers for LRU-evicted engines).

### Graph coherence across projects

**Risk:** Medium. PageRank is computed per-project. A file that is
architecturally central in the *federated* graph (hub between projects) will
not have its importance reflected in any single project's PageRank.

**Mitigation:** In Phase 15b, compute a lightweight "federated PageRank"
over the bridge graph. Files with many incoming bridge edges get an
additional cross-project importance boost applied during fusion. This does
not require recomputing the full per-project PageRank; it is a separate,
small graph (one node per file that participates in a bridge edge).

---

## Future Extensions (out of scope for Phase 15)

- **Federated vector search**: Shared embedding space across projects enables
  true cross-project semantic search (requires same embedding model and
  dimension). The current design does per-project vector search + RRF fusion,
  which is a reasonable approximation.

- **Cross-project rename**: `rename_symbol` that propagates across projects
  via bridge edges. Requires write access to all projects' engines.

- **Federated onboarding**: `generate_onboarding` that produces a combined
  architectural overview spanning all federated projects.

- **Cloud federation**: Replace `Engine::open` with a remote RPC call to a
  codixing server hosting the index. This would enable federation across
  machines (e.g., a central index server for a large organization).

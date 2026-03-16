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

## MCP Integration

Codixing exposes all its tools via the [Model Context Protocol](https://modelcontextprotocol.io) — any MCP-compatible client picks them up automatically.

### Claude Code (one command)

```bash
claude mcp add codixing -- npx -y codixing-mcp --root .
```

### Continue.dev

Add to your `~/.continue/config.json` under `mcpServers`:

```json
{
  "mcpServers": [
    {
      "name": "codixing",
      "command": "npx",
      "args": ["-y", "codixing-mcp", "--root", "."]
    }
  ]
}
```

### Cursor / Windsurf / other MCP clients

Add to your MCP configuration (`.mcp.json` or settings):

```json
{
  "mcpServers": {
    "codixing": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "codixing-mcp", "--root", "."]
    }
  }
}
```

### From source (development)

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

See `mcp.json.example` for a template.

### Daemon mode (recommended)

Normal mode spawns a new process per call (~30ms cold start). Daemon mode loads the engine once and serves all calls over a Unix socket (~6ms IPC overhead) — **4–5× faster for cheap operations**.

```bash
# Start daemon (keeps running, auto-updates index on file saves)
codixing-mcp --root /path/to/project --daemon &

# All subsequent codixing-mcp calls auto-proxy through the daemon
```

The daemon runs a background file watcher. When you save a file, the index updates within ~100ms. Claude Code always queries a fresh index.

### Available MCP tools (44)

#### Search & Navigation
| Tool | What it does |
|------|-------------|
| `code_search` | BM25 + graph-boosted search; `instant`/`fast`/`thorough`/`explore`/`deep` strategies |
| `grep_code` | Regex or literal search across indexed files; bounded output, glob filter, context lines |
| `find_symbol` | Structured symbol lookup — returns definition location + signature |
| `read_symbol` | Full source of a named symbol |
| `read_file` | Token-budgeted file reader with line range |
| `outline_file` | Per-file symbol tree sorted by line number — token-efficient alternative to `read_file` |
| `search_usages` | All usage sites of a symbol across the codebase |
| `list_files` | List all indexed files with optional glob filter and chunk counts |
| `find_similar` | Find code chunks semantically similar to a given snippet or description |

#### Graph & Architecture
| Tool | What it does |
|------|-------------|
| `get_repo_map` | PageRank-ranked architecture overview within a token budget |
| `focus_map` | **NEW** — Context-aware repo map using Personalized PageRank seeded by recently edited files |
| `get_references` | Who imports a file (callers) + what it imports (callees) |
| `get_transitive_deps` | Multi-hop dependency chain to arbitrary depth |
| `symbol_callers` | Symbol-level call graph: which functions directly call a given symbol |
| `symbol_callees` | Symbol-level call graph: which functions a given symbol directly calls |
| `explain` | Assembled context package: definition + usage sites (callers) + callees for any symbol |
| `get_context_for_task` | Given a task description, assembles relevant context with dependency-aware ordering |

#### Code Modification
| Tool | What it does |
|------|-------------|
| `write_file` | Write a file and immediately reindex it |
| `edit_file` | Exact find-and-replace in a file with immediate reindex |
| `delete_file` | Delete a file and remove it from the index |
| `apply_patch` | Apply a unified git diff across one or more files with auto-reindex |
| `rename_symbol` | Project-wide identifier rename with conflict validation and auto-reindex |
| `run_tests` | Execute a test command in the project root and return stdout + exit code |

#### Analysis & Quality
| Tool | What it does |
|------|-------------|
| `predict_impact` | Given a unified diff, rank files most likely to need changes (call graph + import graph) |
| `stitch_context` | Search + automatically attach callee definitions for cross-file context in one call |
| `review_context` | Assemble context for reviewing a diff: changed symbols, callers, related tests |
| `find_tests` | Find test files and test functions related to a given source file or symbol |
| `find_source_for_test` | **NEW** — Given a test file, find the source it tests (naming, imports, co-location) |
| `get_complexity` | Compute cyclomatic complexity for functions in a file |
| `find_orphans` | Detect dead code — files with zero in-degree in the dependency graph |
| `check_staleness` | **NEW** — Fast stat()-based check if the index is out of date |

#### Git & Temporal
| Tool | What it does |
|------|-------------|
| `git_diff` | Show `git diff` output for the working tree or between commits |
| `get_hotspots` | Rank files by change frequency and author diversity from git history |
| `search_changes` | Search git log by commit message or file path |
| `get_blame` | Show git blame with grouped output by commit |

#### Session & Memory
| Tool | What it does |
|------|-------------|
| `get_session_summary` | Inspect current session state: files read/edited, symbols explored |
| `session_status` | **NEW** — Multi-agent shared session: active agents, hot files, event count |
| `session_reset_focus` | Clear progressive directory focus narrowing |
| `remember` | Store a key-value note in persistent project memory (`.codixing/memory.json`) |
| `recall` | Retrieve notes from project memory by key or keyword search |
| `forget` | Remove a note from project memory |

#### Other
| Tool | What it does |
|------|-------------|
| `index_status` | Current index statistics (files, chunks, symbols, graph) |
| `enrich_docs` | LLM-generated doc summaries per symbol (Anthropic or Ollama) |
| `generate_onboarding` | Generate a structured onboarding document for the indexed project |

---

## LSP Server

`codixing-lsp` implements the Language Server Protocol, bringing Codixing's code intelligence to **any LSP-capable editor** — VS Code, Neovim, Emacs, Sublime Text, JetBrains, and more.

**Capabilities:**

| Feature | Description |
|---------|-------------|
| **Hover** | Symbol signature + kind + file location; prefers same-file matches |
| **Go-to-definition** | Jump to any symbol's definition across the codebase |
| **References** | Find all usage sites of the symbol under cursor |
| **Workspace symbol** | Global fuzzy symbol search (`Ctrl+T` / `Cmd+T`) |
| **Document symbol** | Per-file outline sorted by line number |
| **Document sync** | Tracks open documents; live reindex on save |
| **Complexity diagnostics** | Cyclomatic complexity warnings on functions (configurable threshold) |

```bash
# Start the LSP server
codixing-lsp --root /path/to/project

# With custom complexity threshold (default: 6 = moderate+)
codixing-lsp --root /path/to/project --complexity-threshold 11
```

**Editor configuration:**

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
| **Test suite** | 452 tests (including retrieval quality regression suite) |

### Init speed breakdown (0.87s on 246K LoC)

| Stage | Time | Notes |
|-------|------|-------|
| File discovery | ~5ms | Directory walk, 770 files |
| Parse + chunk + BM25 index | ~600ms | rayon parallel, all CPU cores |
| Graph build (imports + PageRank) | ~200ms | Parallel resolution, single sequential insert pass |
| Persist to `.codixing/` | ~50ms | bitcode + Tantivy flush |

> **Why it's fast:** `build_graph()` reuses the import lists extracted during the parallel parse phase — no second file read, no second tree-sitter parse. Files are parsed exactly once.

### Claude Session Benchmark: grep vs Codixing

Simulates a 5-task coding investigation (find a struct, trace callers, architecture overview, semantic search, impact analysis) on the Codixing codebase (86 Rust files).

| Task | grep/cat/find | Codixing | grep output | cdx output | Savings |
|------|-------------|----------|------------|------------|---------|
| Find Engine struct | 7ms | 12ms | 85.6 KB | 470 B | **99%** |
| Find callers of reindex_file | 3ms | 10ms | 2.0 KB | 799 B | 60% |
| Architecture overview | 12ms | 295ms | 11.9 KB | 11.5 KB | 3% |
| Find BM25 scoring code | 13ms | 11ms | 38.0 KB | 3.0 KB | **92%** |
| Impact analysis (chunker) | 7ms | 22ms | 5.7 KB | 327 B | **94%** |
| **TOTAL** | **42ms** | **350ms** | **143 KB** | **16 KB** | **88%** |

**Token impact**: ~36,655 tokens (grep) → ~4,103 tokens (codixing) = **~32,500 fewer tokens per 5-task session (88%)**.

**Tool calls**: grep needs 12 calls → codixing needs 6 calls (50% fewer round-trips).

> The decisive case: `grep + cat` returns the **entire 85KB engine.rs** to find one struct definition. Codixing returns the struct name + signature in 470 bytes. Fewer wasted tokens = more room for reasoning.

Run `python3 benchmark_claude_session.py` to reproduce on your machine.

### Real-World Benchmark: 6 Open-Source Projects

Tested on tokio (765 Rust files), ripgrep (100 Rust files), axum (291 Rust files), django (2,894 Python files), fastapi (1,118 Python files), react (4,325 JS files) — total 9,493 files, 55,869 symbols.

All repos indexed with BM25-only in under 9 seconds total.

26 tasks across 6 categories:

| Metric | grep/cat/find | Codixing | Improvement |
|--------|---------------|----------|-------------|
| Tool calls | 58 | 26 | **55% fewer** |
| Output bytes | 338KB | 92KB | **73% fewer** |
| Est. tokens | ~84,600 | ~22,900 | **73% fewer** |

By category:
| Category | Byte Savings |
|----------|-------------|
| Symbol lookup (6 tasks) | **93%** |
| Impact analysis (2 tasks) | **90%** |
| Code understanding (6 tasks) | **84%** |
| Bug localization (2 tasks) | **83%** |
| Call graph (6 tasks) | **72%** |

### SWE-bench Lite Localization (300 tasks, 12 repos)

| Metric | grep | Codixing | Improvement |
|--------|------|----------|-------------|
| Recall@1 | 14.7% | **48.7%** | **+231%** |
| Recall@5 | 41.3% | **74.3%** | **+80%** |
| Recall@10 | 54.7% | **77.3%** | **+41%** |
| Contains GT | 64.7% | **78.7%** | **+22%** |

Multi-strategy BM25 search with SweRankEmbed-Small outline reranking, automatic CamelCase↔snake_case query expansion, score-weighted ranking, and usage-based file coverage. No LLM needed — pure retrieval + lightweight embedding.

Run `python3 benchmarks/swe_bench_eval.py --skip-clone` to reproduce (requires `datasets` package).

### Multi-Language Search Quality

Symbol localization across 5 languages (BM25-only, no GPU needed):

| Language | Repo | Tasks | Hit@1 | Hit@5 | Hit@10 |
|----------|------|-------|-------|-------|--------|
| Rust | tokio | 10 | 50% | 90% | 100% |
| Python | django | 10 | 80% | 100% | 100% |
| Go | gin | 10 | 50% | 90% | 90% |
| C++ | leveldb | 10 | 40% | 70% | 100% |
| JavaScript | react | 10 | 60% | 90% | 100% |
| **Overall** | **5 repos** | **50** | **56%** | **88%** | **98%** |

16 languages supported with full AST parsing via tree-sitter, plus 4 config formats (YAML, TOML, Dockerfile, Makefile). Run `python3 benchmarks/multilang_eval.py` to reproduce.

### MCP Server Benchmark (Self-Hosting)

Codixing MCP server running on its own codebase — 127 Rust files, 1054 chunks, 2030 symbols, 1054 vectors (BgeSmallEn 384d), 375 graph nodes. Measured on Apple M4 (macOS ARM64).

**Cold start:** 107ms (process launch + ONNX model load + index open)

**Warm tool latency** (persistent MCP connection, 44 tools available):

| Tool | Latency | Output |
|------|---------|--------|
| `index_status` | 0.2ms | 405 chars |
| `find_symbol` | 0.2ms | 479 chars |
| `symbol_callees` | 1.4ms | 104 chars |
| `list_files` | 0.8ms | 4.1 KB |
| `find_tests` | 1.0ms | 15.1 KB |
| `get_complexity` | 0.5ms | 1.0 KB |
| `check_staleness` | 2.2ms | 178 chars |
| `symbol_callers` | 8.5ms | 190 chars |
| `search instant` | 35ms | 1.6–5.2 KB |
| `search fast` (hybrid) | 35ms | 4.6–7.4 KB |
| `explain` | 37ms | 1.3 KB |
| `search thorough` | 3.1s | 6.3 KB |

**Hybrid vs BM25 search quality** (warm, same server):

| Query Type | BM25 (`instant`) | Hybrid (`fast`) | Winner |
|------------|-------------------|-----------------|--------|
| Exact identifier | Top-1 correct (3 files) | Top-1 correct (2 files) | BM25 (more context) |
| Concept (NL) | Relevant but scattered | Focused on core engine | **Hybrid** |
| Semantic ("convergence iterative") | Finds pagerank + noise | All 5 hits = pagerank | **Hybrid** |
| Cross-domain ("dead code") | Misses orphan module | Still misses (vocab gap) | Tie |
| Implementation detail | Correct (simd_distance) | Correct (simd_distance) | Tie |
| Architecture question | Correct (tools/mod.rs) | Correct + VS Code ext | **Hybrid** |

> **Takeaway:** Hybrid search (BgeSmallEn + asymmetric RRF) outperforms BM25-only for natural language and conceptual queries while matching BM25 for exact identifiers. The asymmetric RRF automatically routes identifier queries BM25-dominant and NL queries vector-dominant.

---

## Embedding Model Selection

BM25-only is the default and works well for most codebases. To enable semantic search, pass `--model` at init time:

| Situation | Recommendation |
|-----------|----------------|
| Good identifiers and docstrings | **BM25-only** (default) — fast, no GPU/ONNX needed |
| Natural-language queries matter | **BgeLarge** or **Snowflake-Arctic-L** — best quality; ~7 min one-time init |
| Fast init + some semantic search | **BgeSmall** — 73–110s init (CPU-dependent), run as daemon to eliminate cold-start |

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

- **AST-aware chunking** — Tree-sitter parsing across 16 language families + 4 config formats; never splits a function in half
- **BM25 full-text search** — Tantivy-backed with a custom code tokenizer; `signature` field ×3.0 and `entity_names` ×2.0 field boosts; 3.5× definition boost; automatic CamelCase↔snake_case query expansion for cross-convention matching
- **Hybrid retrieval** — BM25 + vector (fastembed BGE-Base-EN-v1.5, 768 dims) fused with asymmetric Reciprocal Rank Fusion (O(N+M) HashMap-based); identifier queries route BM25-dominant, natural language routes vector-dominant
- **Code dependency graph** — Import + call extraction for all 16 languages, petgraph `DiGraph`, PageRank scoring; transparently boosts search result ranking
- **Personalized PageRank** — Focus-aware repo maps seeded by recently edited files; surfaces contextually relevant code for AI agents
- **Test-to-code mapping** — Automatically links test files to source via naming conventions, imports, and co-location analysis
- **Memory-mapped vectors** — Optional mmap backend for the vector index; reduces RSS for large repositories
- **Multi-agent sessions** — Shared session context across concurrent MCP clients; time-decayed file boost from cross-agent activity
- **Signature-aware truncation** — Smart snippet formatting that preserves function signatures while eliding bodies
- **Stale index detection** — Fast stat()-based freshness check without content hashing
- **Rename validation** — Detects name collisions and shadowing before applying project-wide renames
- **Band merging** — Adjacent same-file result chunks within 3 lines are merged before rendering; reduces token output by 25–91% on typical codebases
- **Repo map generation** — Aider-style, token-budgeted output sorted by PageRank (importance) not file size
- **Live index freshness** — Daemon file watcher updates the in-memory engine within 100ms of any file save; no restart needed
- **`.gitignore`-aware indexing** — File walker respects `.gitignore`, `.ignore`, and global gitignore (same as ripgrep); no manual exclude lists needed
- **Hash-based incremental sync** — `codixing sync` uses mtime+size pre-filtering then xxh3 content hashes; re-indexes only changed files
- **Cross-repo federation** — `FederatedEngine` wraps multiple `Engine` instances for unified multi-project search via RRF fusion; lazy loading with LRU eviction; per-project boost weights; `--federation config.json` flag
- **MCP server** — 47 tools exposed via JSON-RPC 2.0; Claude Code registers with one command
- **Dynamic tool discovery** — `--compact` mode reduces tools/list from ~6,600 to ~220 tokens (96.7% reduction); meta-tools `search_tools` and `get_tool_schema` let LLMs discover tools on demand
- **Contextual chunk embedding** — Prepends file path, scope chain, and entity names to chunks before embedding; improves semantic retrieval by giving the embedding model positional context
- **Adaptive result truncation** — Detects score cliffs in search results and truncates where confidence drops, returning fewer but higher-quality results; saves ~23% output tokens
- **Query-to-code reformulation** — Lightweight HyDE: maps natural language concepts to hypothetical code patterns (18 mappings) for improved retrieval in Deep strategy
- **Type-filtered search** — `kind` parameter on `code_search` filters by symbol type (function, struct, enum, trait, class, method, interface, type, const, impl)
- **BGE query prefix** — Instruction-tuned query embedding (`"Represent this sentence: "`) for BGE models, improving cosine similarity for hybrid search
- **Concurrent symbol table** — DashMap-backed with exact, prefix, and substring matching
- **Single binary, zero runtime deps** — No JVM, no Docker, no external databases

---

## Supported Languages

| Tier | Languages |
|------|-----------|
| **Tier 1** (full AST + graph) | Rust, Python, TypeScript, TSX, JavaScript, Go, Java, C, C++, C# |
| **Tier 2** (full AST + graph) | Ruby, Swift, Kotlin, Scala |
| **Tier 3** (full AST + graph) | Zig, PHP |
| **Config** (symbol extraction) | YAML, TOML, Dockerfile, Makefile |

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                              Codixing Engine                                │
│                                                                             │
│  ┌─────────────┐  ┌──────────┐  ┌──────────────┐  ┌──────────────────────┐ │
│  │ Tree-sitter  │  │ Tantivy  │  │    Symbol    │  │   Code Graph         │ │
│  │ AST Parser   │  │  (BM25)  │  │    Table     │  │ (petgraph + PPR)     │ │
│  └──────┬───────┘  └────┬─────┘  └──────┬───────┘  └──────────┬───────────┘ │
│         │               │               │                      │            │
│  ┌──────▼───────┐  ┌────▼──────┐  ┌─────▼──────┐  ┌───────────▼──────────┐ │
│  │     cAST     │  │   Code    │  │  DashMap   │  │ ImportExtractor      │ │
│  │   Chunker    │  │ Tokenizer │  │  (conc.)   │  │ + PageRank + PPR     │ │
│  └──────────────┘  └────┬──────┘  └────────────┘  └───────────┬──────────┘ │
│                          │                                     │            │
│  ┌───────────────────────▼─────────────────────────────────────▼──────────┐ │
│  │  Retriever: BM25 · Hybrid (RRF) · Thorough (MMR) · Explore · Deep     │ │
│  │  + Graph PageRank boost · Definition 3.5× · Popularity boost          │ │
│  └────────────────────────────────────────────────────────────────────────┘ │
│                                                                             │
│  ┌──────────────────┐  ┌───────────────┐  ┌────────────────────────────┐   │
│  │   Test Mapping    │  │ Shared Session │  │   Vector Index             │   │
│  │ (naming+imports)  │  │ (multi-agent)  │  │ (brute-force / mmap)       │   │
│  └──────────────────┘  └───────────────┘  └────────────────────────────┘   │
│                                                                             │
│  ┌─────────────────────────────────────────────────────────────────────────┐ │
│  │ API Layer: CLI (clap) · REST (axum) · MCP 44 tools (JSON-RPC 2.0)      │ │
│  │            + Daemon (Unix socket) · File Watcher · LSP Server           │ │
│  └─────────────────────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## Retrieval Strategies

| Strategy | Method | Graph boost | Warm Latency |
|----------|--------|-------------|--------------|
| `instant` | BM25 only (exact match, no query expansion) | No | ~35ms |
| `fast` | BM25 + vector (asymmetric RRF) | Yes | ~35ms |
| `thorough` | Hybrid + MMR dedup | Yes | ~3s |
| `explore` | BM25 + graph neighbor expansion | Yes | <100ms |
| `deep` | Multi-query RRF fusion + code reformulation + popularity boost | Yes | ~1.5s |

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
| Memory mapping | `memmap2` 0.9 | Optional mmap vector index backend |
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
| **Phase 4: Agent Integration** | ✅ Complete | MCP (38 tools), daemon mode, 2.6× faster init, live watcher — 222 tests |
| **Phase 5: Production Hardening** | ✅ Complete | Field boosts, band merging, asymmetric RRF, call graph edges, sync, .gitignore walker — 232 tests |
| **Phase 6: Ecosystem Expansion** | ✅ Complete | Tier 2 languages (Ruby/Swift/Kotlin/Scala), multi-repo, VS Code extension, CI matrix, Qdrant backend — 244 tests |
| **Phase 7: Git Sync + Qwen3 + Eval** | ✅ Complete | Git-aware incremental sync, Qwen3 candle backend, embedding eval harness — 260 tests |
| **Phase 8: Productivity + Ecosystem** | ✅ Complete | 33 MCP tools (apply_patch, run_tests, outline_file, rename_symbol, explain, symbol_callers, symbol_callees, predict_impact, stitch_context, enrich_docs), LSP server, Zig+PHP, Docker, Homebrew, 60s search cache — 260 tests |
| **Phase 10: Developer Intelligence** | ✅ Complete | 33 MCP tools (remember, recall, forget, find_tests, find_similar, get_complexity, review_context, generate_onboarding), persistent memory store, cyclomatic complexity, onboarding doc generation — 210 tests |
| **Phase 11: IDE Integration** | ✅ Complete | LSP server (`codixing-lsp`) with hover, go-to-def, references, symbols, document sync, live reindex, cyclomatic complexity diagnostics; VS Code LSP client; BM25-only default; Tier 2 retrieval quality regression suite — 368 tests |
| **Phase 12: Launch Prep** | ✅ Complete | Multi-language benchmarks, code cleanup, binary optimization (thin LTO + strip), website update — 368 tests |
| **Phase 13a: Session-Aware Retrieval** | ✅ Complete | Track agent interactions, graph-propagated session boost (1-hop 0.3×, 2-hop 0.1×), progressive focus, linear decay, session persistence — 377 tests |
| **Phase 13b: Temporal Code Context** | ✅ Complete | `get_hotspots`, `search_changes`, `get_blame`, blame-aware `explain`, diff-aware `predict_impact` — 383 tests |
| **Phase 14: Dead Code Detection** | ✅ Complete | `find_orphans` — zero in-degree graph analysis with confidence scoring (Certain/High/Moderate/Low) |
| **Phase 15: Cross-Repo Search** | ✅ Complete | FederatedEngine (multi-repo RRF fusion), `--federation` flag, `list_projects` tool, lazy loading with LRU eviction, per-project boost weights, `get_context_for_task`, asymmetric RRF, query expansion, path-match reranking — 426 tests |
| **Phase 16: Intelligence & Scale** | ✅ Complete | Focus-aware repo map (PPR), test-to-code mapping, config languages (YAML/TOML/Dockerfile/Makefile), mmap vector index, multi-agent shared sessions, signature-aware truncation, stale index detection, rename validation — **452 tests** |
| **Phase 17: Research-Backed Retrieval** | ✅ Complete | Dynamic tool discovery (`--compact`, 96.7% token reduction), contextual chunk embedding, adaptive result truncation (score cliff detection), query-to-code reformulation (lightweight HyDE), type-filtered search, BGE instruction prefix, synonym expansion, late chunking — **628 tests** |

---

## Development

```bash
cargo build --workspace
cargo test --workspace        # 628 tests
cargo clippy --workspace -- -D warnings
cargo fmt --all
```

---

## License

Codixing Business Source License 1.0. Free for:
- Open-source projects
- Personal and educational use
- Teams of 5 or fewer developers

Commercial license required for larger teams. Contact [hello@codixing.com](mailto:hello@codixing.com).

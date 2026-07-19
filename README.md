# Codixing

[![CI](https://github.com/ferax564/codixing/actions/workflows/ci.yml/badge.svg)](https://github.com/ferax564/codixing/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/ferax564/codixing/graph/badge.svg)](https://codecov.io/gh/ferax564/codixing)

**Website: [codixing.com](https://codixing.com)** · **[Docs](https://codixing.com/docs)**

Code retrieval engine that saves your AI agent 73% of its token budget. Replaces grep with ranked, AST-aware search — so models spend tokens reasoning, not reading.

## Install

```sh
curl --proto '=https' --proto-redir '=https' -fsSLo /tmp/codixing-install.sh https://codixing.com/install.sh
sh /tmp/codixing-install.sh
```

Installs the `codixing`, `codixing-mcp`, `codixing-lsp`, and
`codixing-server` suite on Linux x86_64 or Apple Silicon macOS. It uses
`/usr/local/bin` when writable and otherwise falls back to
`$HOME/.local/bin`. Set `CODIXING_INSTALL_DIR` for another destination or
`CODIXING_VERSION=X.Y.Z` to pin a release. Windows x86_64 binaries are on the
[releases page](https://github.com/ferax564/codixing/releases), and the MCP
server is also available through `npx -y codixing-mcp`.

### Claude Code plugin (optional)

```bash
claude plugin marketplace add ferax564/codixing
claude plugin install codixing@codixing
```

Adds 5 slash commands: `/codixing-setup`, `/codixing-explore`, `/codixing-review`, `/codixing-preflight`, `/codixing-release`.

### MCP server (optional — for Cursor, Windsurf, Continue.dev, Codex)

Add to your project's `.mcp.json`:

```json
{
  "mcpServers": {
    "codixing": {
      "command": "npx",
      "args": ["-y", "codixing-mcp", "--root", ".", "--profile", "minimal", "--no-daemon-fork"]
    }
  }
}
```

Or for OpenAI Codex CLI: `codex mcp add codixing -- npx -y codixing-mcp --root . --profile minimal --no-daemon-fork`

For large repositories, run `codixing init .` before starting the MCP client. When
using Codex configuration, set `startup_timeout_sec = 120` so the first `npx`
download or index load is not cut off by the default startup timeout.
Codixing skips individual source files over 2 MiB by default so generated or
minified bundles cannot dominate parsing memory; override this with
`--max-file-bytes N` (or `0` for no limit).
Auxiliary
concept and learned-vocabulary artifacts are evidence-ranked and bounded: at
most 32 vocabulary terms are paired per file, 12 expansions are retained per
term, and concept clusters retain at most 32 symbols / 16 files. Their v2 files
intern repeated paths and names, and every sync invalidates them before a bounded
rebuild so large-repo searches never use stale semantic mappings.
Once initialized, MCP watcher updates stay proportional to the edited files:
changes accumulate in an unpublished copy-on-write generation and publish after
2 seconds idle, 30 seconds maximum, or 256 paths. Existing readers keep their
complete old snapshot, interrupted batches replay automatically, and a true
no-op sync does not create another generation.

---

## Why Not Just Grep?

AI coding agents use `grep`, `find`, and `cat` for code navigation. These tools return **everything, always** — a single `rg b2Vec2` on a real codebase returns 2,240 hits (225 KB), burning context before any reasoning happens.

Codixing returns the top 20 results in 1.3 KB — same signal, **99% less waste**.

### The cost of noise

Tested on 6 real-world repos (tokio, ripgrep, axum, django, fastapi, react — 9,493 files):

| Metric | grep/cat/find | Codixing | Savings |
|--------|---------------|----------|---------|
| Tool calls per session | 58 | 26 | **55% fewer** |
| Output tokens | ~84,600 | ~22,900 | **73% fewer** |
| Est. cost (Opus @ $15/M) | $1.27 | $0.34 | **$0.93/session** |

At 50 agent sessions/day, that's **$1,400/month** back in your pocket — and the agent finds the right code more often.

### What you get

| Capability | grep/rg | Codixing |
|-----------|---------|----------|
| Bounded, ranked output | No | Yes (BM25 + PageRank) |
| Symbol definitions (not just mentions) | No | Yes (AST-parsed symbol table) |
| Dependency graph queries | No | Yes (transitive imports, call graph) |
| Natural language search | No | Yes (BM25 + optional embeddings) |
| Token budget management | No | Yes (auto-truncation) |

### Agent golden path

If you are wiring Codixing into an AI agent, start with these tools instead of
exposing the whole surface at once:

| Task | Use this first | MCP profile | Why |
|------|----------------|-------------|-----|
| Find relevant code from a concept | `code_search` / `codixing search` | Minimal | Ranked, token-bounded retrieval for natural language and code terms |
| Jump to a known definition | `find_symbol` / `codixing symbols` | Minimal | Definitions only, not every textual mention |
| Get a compact architecture map | `get_repo_map` / `codixing graph --map` | Minimal | A bounded orientation pass before deeper traversal |
| Check blast radius before editing | `search_usages --complete` or `predict_impact` / `codixing impact` | Reviewer | Deterministic bounded scan instead of top-K guesses; follow `next_offset` pages |
| Understand a feature | `feature_hub` or `get_context_for_task` | Reviewer | One call combines search, dependencies, dependents, and tests |
| Inspect exact text | `grep_code` / `codixing grep` | Reviewer | Literal/regex scan for strings, errors, TODOs, and generated names |
| Focus on current work | `focus_map` / `codixing graph --map` | Reviewer | Graph-ranked context biased toward changed or seed files |

Minimal is the startup default. Call `set_mcp_profile` with `reviewer` before
using the read-only specialist rows above. Use `search_tools` and
`get_tool_schema` when an agent needs to discover a narrower capability. A
minimal/reviewer server cannot upgrade itself into a write-capable profile
unless it was started explicitly with `--allow-profile-escalation`.

---

## Getting Started

### 60-second setup

```bash
# 1. Install
curl --proto '=https' --proto-redir '=https' -fsSLo /tmp/codixing-install.sh https://codixing.com/install.sh
sh /tmp/codixing-install.sh

# 2. Index your project
codixing init .
# ✓ Indexed 2,847 files, 14,203 chunks, 8,891 symbols in 1.2s

# 3. Search
codixing search "authentication handler"
# ► src/auth/handler.rs:42  [score: 0.94]
#   pub fn handle_auth_request(req: Request) -> Result<Token>
```

That's it. Your agent now uses ranked search instead of grep.

`codixing init` is safe to rerun. It builds a complete index generation beside
the active one, validates it, and atomically switches readers only when the new
generation is ready. An interrupted, failed, or out-of-space rebuild leaves the
previous index searchable; a successful switch removes the superseded data.
Long-lived read-only engines detect the generation switch and reopen the whole
new snapshot without observing a mixture of old and new artifacts.
Plan for temporary free space approximately equal to one additional index while
a rebuild is running. `codixing doctor` reports the active generation and any
abandoned staging generations that could not yet be reclaimed.

### CLI commands

```bash
# Search (natural language or symbol names)
codixing search "error handling middleware"

# Symbol lookup (definitions only, not mentions)
codixing symbols Engine

# Dependency graph
codixing callers src/engine.rs    # who imports this file?
codixing callees src/engine.rs    # what does this file import?

# Keep index fresh (re-indexes only changed files)
codixing sync

# Architecture map
codixing graph --map --token-budget 4000
```

### Hybrid search (optional)

`codixing init` builds BM25 + symbol graph by default. For natural-language queries
("how does the auth flow work?"), opt into semantic embeddings with `--embed`:

```bash
codixing init . --embed --model bge-small-en    # one-time, ~2 min on a medium repo
codixing search "how does auth work" --strategy fast
```

ONNX-based embedding models (`bge-small-en`, `bge-base-en`, etc.) require ONNX
Runtime (`pip install onnxruntime`, or download from the
[onnxruntime releases](https://github.com/microsoft/onnxruntime/releases)). Set
`ORT_DYLIB_PATH` to the exact absolute path of `libonnxruntime.so`,
`libonnxruntime.dylib`, or `onnxruntime.dll` before running Codixing. Run
`codixing doctor` to verify the path. The static `model2vec` model and BM25-only
installs do not need ONNX Runtime.

---

## CLI Commands

The most common commands (run `codixing --help` for the full list):

```bash
codixing search "query"          # Ranked code search
codixing grep "pattern"          # Literal/regex text scan (path:line:col:text)
codixing symbols Widget          # Find symbol definitions
codixing usages add_chunk        # Find call sites and imports
codixing callers src/engine.rs   # Who imports this file
codixing callees src/engine.rs   # What this file imports
codixing graph --map             # Architecture overview
codixing graph --communities     # Louvain community detection
codixing graph --surprises 10    # Top N surprising edges
codixing graph --html graph.html # Interactive HTML dashboard
codixing graph --html g.html --diff-base main  # Dashboard + diff-impact overlay
codixing path src/a.rs src/b.rs  # Shortest import chain
codixing impact src/engine.rs    # Blast radius analysis
codixing api src/engine.rs       # Public API surface
codixing types Engine            # Type relationships
codixing examples add_chunk      # Usage examples from tests + callers
codixing context src/engine.rs   # Cross-file context assembly
codixing agent-context-pack "task" # Stable JSON context pack for agents
codixing init .                  # Index a project
codixing sync                    # Incremental re-index
codixing import github issues.json  # Import GitHub issues/PRs as searchable context
codixing import adr docs/adr/    # Import architecture decision records
codixing import jira export.csv  # Import Jira issues (CSV or JSON)
codixing import linear issues.json  # Import Linear issues (CSV or JSON)
codixing search "auth bug" --source jira    # Search only imported context
codixing audit                   # Find stale files
```

Full reference: [codixing.com/docs](https://codixing.com/docs)

### MCP server (optional)

For editors with MCP support, the `codixing-mcp` binary exposes a generated,
profile-gated JSON-RPC 2.0 catalog.
It starts in the narrow read-only `minimal` profile by default; use
`--profile reviewer` for the broader read-only analysis surface, `--profile editor` or
`--allow-write-tools` for non-destructive write helpers, and `--profile dangerous`
only when destructive file and shell tools are intentional. Agents can call
`get_mcp_profile` and `set_mcp_profile` to inspect or switch within the server's
startup safety ceiling. Minimal/reviewer startup remains read-only by default;
`--allow-profile-escalation` is required to permit runtime write-profile upgrades.
Successful switches emit `notifications/tools/list_changed` so clients can
refresh `tools/list`.

| Category | Representative tools |
|----------|-------|
| **Search** | code_search, find_symbol, grep_code, search_usages, read_symbol, find_similar, stitch_context |
| **Graph** | get_repo_map, focus_map, get_references, get_transitive_deps, symbol_callers, symbol_callees, predict_impact, find_orphans, explain |
| **Files** | read_file, write_file, edit_file, delete_file, apply_patch, list_files, outline_file |
| **Analysis** | agent_context_pack, find_tests, find_source_for_test, get_complexity, review_context, rename_symbol, run_tests, get_context_for_task, check_staleness, generate_onboarding, audit_freshness |
| **Git** | git_diff, get_hotspots, search_changes, get_blame |
| **Session** | remember, recall, forget, get_session_summary, session_status, session_reset_focus |
| **Meta** | index_status, search_tools, get_tool_schema, get_mcp_profile, set_mcp_profile, enrich_docs |

### Daemon mode

Daemon mode loads the engine once and serves calls over a Unix socket (or named pipe on Windows) — **4-5x faster**.
The daemon auto-starts on first connection and self-terminates after 30 minutes idle:

```bash
codixing-mcp --root /path/to/project          # auto-starts daemon
codixing-mcp --root /path/to/project --daemon  # explicit daemon start
codixing-mcp --root /path/to/project --no-daemon-fork  # disable auto-start
```

The daemon auto-updates the index after a short debounce on file saves, then
persists the refreshed index before serving the new state.

---

## LSP Server

`codixing-lsp` brings code intelligence to any LSP-capable editor — VS Code, Neovim, Emacs, Sublime Text, JetBrains.

**Capabilities:** Hover, Go-to-definition, References, Call hierarchy (incoming/outgoing), Workspace symbols, Document symbols, Live reindex on save, Cyclomatic complexity diagnostics, Code actions, Inlay hints, Completions, Signature help, Rename refactoring, Semantic tokens.

```bash
codixing-lsp --root /path/to/project
```

**Neovim:**
```lua
{ cmd = { "codixing-lsp", "--root", vim.fn.getcwd() } }
```

**Emacs (eglot):**
```elisp
(add-to-list 'eglot-server-programs
  '((rust-mode python-mode) . ("codixing-lsp" "--root" "/your/project")))
```

---

## VS Code / Cursor Extension

The `editors/vscode/` directory contains a TypeScript extension with: Index Workspace, Sync Index, Search, Show Repo Map, Start Daemon, Register MCP Server.

```bash
cd editors/vscode && npm install && npm run compile
# Then F5 in VS Code to launch the Extension Development Host
```

**Pre-built VSIX:** Download `codixing.vsix` from the [releases page](https://github.com/ferax564/codixing/releases) and install:

```bash
code --install-extension codixing.vsix
```

---

## Performance

| Metric | BM25-only | Hybrid (BgeSmallEn) |
|--------|-----------|---------------------|
| Init (138 files) | **0.21s** | 120s (one-time) |
| MCP cold start | **24ms** | 107ms |
| Search latency | 30-42ms | 36-40ms |
| Top-1 accuracy | 7/10 | **10/10** |

**Retrieval accuracy** (OpenClaw, 20 curated file-localization queries, 2026-04-28):

| Tool | Recall@10 | MRR | Notes |
|------|----------:|----:|-------|
| Codixing | **0.802** | **0.827** | `symbols`, `usages`, `search`, and `cross-imports` routed by query type |
| codebase-memory-mcp v0.6.0 | 0.374 | 0.243 | Local CLI benchmark; semantic tool was not exposed by the downloaded build |
| grep | 0.191 | 0.168 | Baseline recursive text scan |

Raw results: [external_competitor_benchmark.md](benchmarks/results/external_competitor_benchmark.md). To reproduce the full table, set `CODEBASE_MEMORY_MCP=/path/to/codebase-memory-mcp` for a local v0.6.0 binary, then run [run_external_competitors.sh](benchmarks/run_external_competitors.sh).

**Large codebase** (368K LoC, 7,607 files): Init 7.9s, search 94ms, 99% token reduction vs grep.

**Linux kernel** (63K C/H files, 30M+ lines, 84K-node dependency graph): 1.57s cold-start search, 0.79s warm via the MCP daemon path. Zero-deserialization mmap for instant startup. Note: fresh-process CLI invocations on a 2GB+ hybrid index pay startup cost on every call — prefer the MCP daemon or a BM25-only index (`codixing init .` without `--embed`) for the CLI path.

**SWE-bench Lite** (300 tasks, 12 repos): Recall@5 = 74.3% (vs grep 41.3%).

See [benchmarks/](benchmarks/) for detailed methodology and reproduction scripts.

---

## Key Features

- **Broad language and document support** — Tree-sitter AST for Rust, Python, JavaScript/TypeScript/TSX, Go, Java, C, C++, C#, Ruby, Swift, Kotlin, Scala, Zig, PHP, Bash, and Matlab; line-based parsing for config/diagram formats (YAML, TOML, Dockerfile, Makefile, Mermaid, XML); structured doc parsers for Markdown, HTML, reStructuredText, AsciiDoc, and plain text
- **Documentation indexing** — indexes Markdown, HTML, reStructuredText (`.rst`), AsciiDoc (`.adoc`), and plain text (`.txt` + bare `README`/`LICENSE`/`AUTHORS`/`CHANGELOG`) alongside code with section-aware chunking, CHANGELOG-aware version-section splitting, breadcrumb metadata, and doc-to-code graph linking; use `--docs-only` to restrict results to docs or `--code-only` to exclude them
- **Hybrid search** — BM25 + optional vector embeddings, fused with Reciprocal Rank Fusion
- **Symbol-level call graph** — Function-to-function call edges extracted from AST, including Rust trait dispatch, Python class inheritance, and TypeScript interface implementations
- **Dependency graph** — Import + call extraction, PageRank scoring, Personalized PageRank for focus-aware maps, Louvain community detection, shortest path queries, surprise/anomaly edge scoring
- **Interactive graph dashboard** — `codixing graph --html` generates a self-contained HTML dashboard (no CDN, no framework): force-directed layout, color-by layer/language/directory, a node detail panel (PageRank, language, callers/callees), named architectural layers with show/hide, a deterministic guided tour of the codebase, a client-side path finder, fuzzy search-to-focus, surprise/anomaly edges, and a `--diff-base <ref>` diff-impact overlay that highlights changed files and their blast radius
- **Graph exports for external tools** — `codixing graph --graphml` (Gephi/yEd), `--cypher` (Neo4j), `--obsidian` (markdown vault with one note per community) for downstream analysis and knowledge-base integration
- **Git hooks** — `codixing hook install` wires post-commit hooks for automatic index sync after every commit; `codixing hook status` / `uninstall` manage the lifecycle
- **Caller cascade** — `codixing callers <file> --depth N` walks the import graph N hops to surface the full transitive caller cascade
- **TOML output filter pipeline** — Project-local `.codixing/filter_rules.toml` compresses MCP tool output for token-tight agent loops, with tee recovery to disk for full output when agents need it
- **Edge confidence** — Every dependency edge tagged Verified/High/Medium/Low based on extraction method (AST-resolved, call extraction, doc reference, external)
- **Ranked cross-imports** — PageRank + git recency scoring for relevance-ranked graph queries across directory boundaries
- **Memory relations** — `memory_relate` tool creates typed edges between agent memory entries, enabling associative recall across sessions
- **Feature hub** — One-call feature exploration combining search + callers + callees + tests for unified understanding
- **Change impact analysis** — `codixing impact` computes blast radius: direct dependents, transitive dependents, and affected tests for any file
- **Semantic concept graph** — Vocabulary gap bridging via behavioral signatures; embedding-free `--semantic` strategy matches code by what it does, not just what it's named
- **API surface analysis** — `codixing api` lists public symbols with visibility tracking (pub, pub(crate), export, etc.)
- **Type-aware search** — `codixing types` shows type relationships: implements, extends, returns, contains
- **Usage example mining** — `codixing examples` finds real usage from tests, callers, and doc blocks
- **Cross-file context assembly** — `codixing context` follows import chains and callees to assemble understanding context
- **Agent context pack** — `codixing agent-context-pack` and MCP `agent_context_pack` compile a versioned JSON pack with repo orientation, must-read evidence handles, related symbols, likely tests, docs, risks, and recommended next tools
- **External-context import** — `codixing import <github|adr|jira|linear> <path>` and the MCP `import_external` tool ingest GitHub issues/PRs (from `gh issue list --json …` or the REST API), architecture decision records, and Jira/Linear issue exports (CSV or JSON, auto-detected) as first-class searchable documents. Imported context is chunked like docs, linked to the code symbols it mentions (doc→code graph edges, so `callers`/`impact` surface the tickets discussing a file), and tagged so `codixing search --source github` (or `--source jira` / `linear` / `adr` / `external`) scopes results. Fully local — no SaaS connector or API key. Re-importing a source replaces it; imports survive `sync` (a full `init` rebuilds from disk, so re-run imports after)
- **Query-personalized PageRank** — Query-time graph boost seeds PageRank from query-relevant nodes for context-aware ranking
- **Learned query reformulation** — Project-specific vocabulary expansion learns from codebase patterns with deterministic evidence-ranked caps (32 terms/file, 12 expansions/term), compact string-interned persistence, and sync-safe freshness
- **CLI + MCP** — Full CLI surface for direct use (run `codixing --help`) plus a profile-gated MCP catalog for editor integration (search, graph traversal, file operations, code review, git analysis, session memory, federation discovery)
- **File freshness audit** — `audit_freshness` tool identifies stale and orphaned files across releases
- **Preflight gates** — Plugin enforces existence scanning before proposing new features
- **TypeScript import resolution** — Resolve `.js` → `.ts` imports with node16/bundler moduleResolution support, enabling 0.8+ R@10 on cross-package code discovery
- **BM25-first embedding workflow** — Plain `codixing init .` creates the fast lexical/graph index. `init --embed` builds vectors and waits for a durable checkpoint; `init --embed --defer-embeddings` intentionally returns BM25-only and `codixing embed` adds vectors later without re-indexing source
- **Model2Vec with code-aware preprocessing** — Static embeddings via `potion-base-8M` (no ONNX needed, instant init). CamelCase/snake_case splitting before tokenization reduces subword fragments by 50-70%, achieving MRR 1.000 on concept queries
- **Jina Code Int8** — `jina-embeddings-v2-base-code` int8-quantized for ARM64 (768 dims, 8ms/query, nDCG@10 0.949). Set `JINA_CODE_INT8_ONNX` env var to the model path
- **Embedding speed measurement** — New `bench-embed` CLI subcommand for profiling embedding performance across custom models
- **Health diagnostics** — `codixing doctor` reports binary/version, index metadata health, git staleness, daemon endpoint status, ONNX runtime configuration, and index disk usage in human or JSON form
- **Daemon mode** — Engine stays in memory, auto-starts on first connection, Unix socket (macOS/Linux) or named pipe (Windows) IPC, file watcher for live index updates, 30-min idle timeout
- **Field-weighted BM25** — Configurable per-field boosting (entity_names 3×, signature 2×, scope_chain 1.5×, content 1×)
- **Search pipeline** — Composable search stages (definition boost, test demotion, path match, graph boost, recency boost, graph semantic propagation via GraphPropagationStage, file-level dedup via FileDedupStage, truncation) with seven strategies, including file-trigram exact-match and embedding-free semantic matching
- **Multi-query RRF fusion** — Auto-generates query reformulations for natural-language queries (3+ words) and fuses results via Reciprocal Rank Fusion; also available via explicit `queries` parameter on `code_search`
- **Git recency signal** — Mildly boosts recently modified files (+10% linear decay over 180 days) via lazy-loaded git log timestamps
- **Overlapping chunks** — Bridge chunks at AST-aware chunk boundaries capture cross-function context; configurable `overlap_ratio` (default 0.0)
- **File path boosting** — Detects explicit file paths and backtick code references in queries and boosts matching results (2.5×)
- **Kernel-scale performance** — Tested on the Linux kernel (63K C/H files, 30M+ lines, 84K-node graph): 1.57s cold-start search, 0.79s warm via the MCP daemon. Mmap symbols, compact chunk metadata (11× smaller), and one lazy file-level trigram artifact serve both grep and exact search. Exact lookup streams candidate paths in bounded batches and hydrates only selected Tantivy chunks; fresh indexes no longer persist a duplicate chunk-level trigram corpus
- **Trigram pre-filtering** — File-level trigram inverted index (Russ Cox/trigrep technique) skips files before disk I/O; **110× faster** literal grep at 1K files, **52× faster** at 10K files; persistent bitcode storage, regex HIR walking with OR-branch support, parallel rayon verification
- **LSP rename + semantic tokens** — Cross-file rename refactoring with conflict detection; semantic highlighting for Rust, Python, TypeScript, Go
- **Optional RustQueue embedding primitives** — Feature-gated, file-grouped job and bounded-channel worker implementation for embedding experiments; the supported CLI durability path is `codixing embed` with generation checkpoints
- **Streaming embeddings** — Fixed-window batch processing (256 chunks) with progress reporting; incremental vector reuse via content hashing
- **Federation auto-discovery** — Auto-detects Cargo, npm, pnpm, Go workspaces, git submodules, and nested projects; lazy federation keeps a bounded stable resident set and searches overflow projects through short-lived read-only engines instead of churning the whole cache
- **Read-only concurrent access** — CLI analysis/search commands and federated project members open the index without probing or owning the Tantivy writer lock, so reads start immediately alongside sync/indexing; periodic reload detects writer updates automatically
- **Changed-file checkpoints** — Incremental updates hard-link immutable artifacts into an unpublished generation, retain a mmap-backed symbol overlay and tombstoned file-trigram updates while edits arrive, then atomically publish once per 2 s idle / 30 s maximum / 256-path batch. A durable path journal recovers interrupted work; unsupported hard-link filesystems fail before copying more than 64 MiB instead of silently duplicating a multi-GB index
- **Incremental embedding** — `sync` skips re-embedding unchanged chunks (content hash comparison)
- **Cosmetic-edit embedding reuse** — `sync` computes a deterministic per-file *signature fingerprint* (symbol signatures, imports, exports) from the AST; when a file's content changed but its fingerprint did not (a comment/whitespace/internal-logic edit), it refreshes BM25/symbols but reuses the cached embedding vectors instead of recomputing them. Conservative: any file without a stable fingerprint re-embeds
- **Progress notifications** — Long-running MCP tools emit `notifications/progress` with streaming partial results so agents see live status
- **Windows support** — Named pipe daemon, brute-force vector fallback when usearch (POSIX-only) is unavailable
- **GitHub Action** — Bounded dependency-impact analysis on PRs, with changed-file and comment-size ceilings for very large diffs
- **Token budgets** — All output respects token limits; adaptive truncation at score cliffs
- **Cross-repo federation** — Unified search across multiple indexed projects with CLI management and workspace auto-discovery (`codixing federation init/add/remove/list/search/discover`)
- **Cross-package import graph** — `cross-imports` command finds files in one directory that import from another via single O(E) graph walk
- **HTTP API server** — REST endpoints (search, symbols, grep, hotspots, complexity, outline, graph) with SSE streaming (`crates/server/`)
- **Self-contained binaries** — No JVM, Docker, external database, or hosted API key. CLI, MCP, LSP, and HTTP server binaries are released for Linux, Apple Silicon macOS, and Windows x86_64

---

## Supported Languages

| Tier | Languages |
|------|-----------|
| **Tier 1** (full AST + graph) | Rust, Python, TypeScript, TSX, JavaScript, Go, Java, C, C++, C# |
| **Tier 2** (full AST + graph) | Ruby, Swift, Kotlin, Scala |
| **Tier 3** (full AST + graph) | Zig, PHP, Bash, Matlab |
| **Config** (symbol extraction) | YAML, TOML, Dockerfile, Makefile |
| **Diagram / Markup** (symbol extraction) | Mermaid, XML/Draw.io |
| **Docs** (section-aware chunking) | Markdown, HTML, reStructuredText (`.rst`), AsciiDoc (`.adoc`, `.asciidoc`), plain text (`.txt` + bare `README`/`LICENSE`/`AUTHORS`/`CHANGELOG`) |

---

## Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                        Codixing Engine                            │
│                                                                   │
│  Tree-sitter  →  cAST Chunker  →  Tantivy (BM25)                │
│  AST Parser      (18 langs)       + Code Tokenizer               │
│                                                                   │
│  Symbol Table (DashMap)    Code Graph (petgraph + PageRank)      │
│                                                                   │
│  Retriever: BM25 · Hybrid (RRF) · Thorough (MMR) · Explore      │
│  + Exact (trigram) · Graph boost · Definition 3.5× · Session     │
│  SearchPipeline: composable stages, 7 strategies                  │
│                                                                   │
│  API: CLI · profile-gated MCP · LSP · HTTP                       │
│       Daemon (Unix socket / Windows named pipe) · File Watcher   │
└──────────────────────────────────────────────────────────────────┘
```

---

## Development

```bash
cargo build --workspace
cargo test --workspace        # run the workspace test suite
cargo clippy --workspace -- -D warnings
cargo fmt --check
```

---

## License

Licensed under the [Apache License, Version 2.0](LICENSE). See [LICENSE](LICENSE) for the full text.

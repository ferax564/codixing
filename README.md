# CodeForge

Ultra-fast code retrieval engine for AI agents.

CodeForge is a Rust-native engine that gives AI coding agents precisely the right context from any codebase, regardless of size. It combines structure-aware parsing (tree-sitter), hybrid search (BM25 + vector + graph), and AI-optimized output into a single, blazing-fast binary.

## Why

AI coding agents live or die by context quality. Current approaches — naive text search, pure vector search, file-level stuffing — waste tokens and miss relevant code. CodeForge solves this by understanding code as *structure*, not text.

## Key Features

- **AST-aware chunking** — Tree-sitter parsing across 30+ languages; never splits a function in half
- **Hybrid retrieval** — BM25 lexical search (Tantivy) + vector similarity (HNSW) + code graph (petgraph), fused with Reciprocal Rank Fusion
- **Code graph intelligence** — Call graphs, dependency analysis, PageRank-scored repo maps (inspired by Aider's approach)
- **AI-native output** — Token-budgeted context with scope chains, signatures, and dependency annotations
- **Incremental indexing** — Sub-500ms updates on file save; no full re-indexing
- **Single binary, zero runtime deps** — No JVM, no Docker, no external databases
- **Multiple interfaces** — CLI, REST API, gRPC, MCP server, Rust library

## Performance Targets

| Metric | Target |
|---|---|
| Index speed | >50K files/min |
| Incremental update | <500ms |
| Retrieval (fast) | <50ms p99 |
| Retrieval (instant) | <10ms p99 |
| Memory (1M LoC) | <2GB |
| Binary size | <50MB |

## Retrieval Strategies

| Strategy | Use Case | Latency |
|---|---|---|
| `instant` | Autocomplete, inline suggestions | <10ms |
| `fast` | Chat Q&A, quick lookups | <50ms |
| `thorough` | Complex code understanding | <200ms |
| `deep` | Architecture analysis, cross-repo | <2s |

## Quick Start

```bash
# Index a codebase
codeforge init .

# Search with natural language
codeforge search "how does authentication work?"

# Exact symbol lookup
codeforge search "fn verify_token" --strategy instant

# Deep architectural analysis
codeforge search "payment processing flow" --strategy deep --budget 4096

# Start as MCP server for AI agents
codeforge serve --mcp
```

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                      CodeForge Engine                        │
│                                                              │
│  Ingestion ──▶ Indexing ──▶ Retrieval ──▶ Output Formatter  │
│                                                              │
│  ┌────────────┐  ┌──────────┐  ┌──────────┐  ┌───────────┐ │
│  │ Tree-sitter │  │ Tantivy  │  │  Vector  │  │ Code Graph│ │
│  │ AST Parser  │  │ (BM25)   │  │  (HNSW)  │  │ (petgraph)│ │
│  └────────────┘  └──────────┘  └──────────┘  └───────────┘ │
│                                                              │
│  API Layer: CLI / REST / gRPC / MCP / WebSocket              │
└─────────────────────────────────────────────────────────────┘
```

## Supported Languages

**Tier 1** (full AST + graph + symbol resolution): Rust, Python, TypeScript/JavaScript, Go, Java, C/C++, C#

**Tier 2** (AST + basic indexing): Ruby, PHP, Swift, Kotlin, Scala, Zig, Elixir, Lua, Bash, SQL

**Tier 3** (text-mode fallback): Any language with a tree-sitter grammar (40+)

## Tech Stack

| Component | Crate |
|---|---|
| AST Parsing | `tree-sitter` |
| Full-text search | `tantivy` |
| Vector search | `hnsw_rs` / `instant-distance` |
| Code graph | `petgraph` |
| Token counting | `tiktoken-rs` |
| File watching | `notify` |
| Git integration | `gix` (gitoxide) |
| HTTP/gRPC | `axum` + `tonic` |
| Embeddings | `ort` (ONNX Runtime) |
| CLI | `clap` |

## Status

Pre-development — research and design phase. See [PRD.md](PRD.md) for the full product requirements document.

## 2026 Priority Alignment

- **P0:** ship Phase 1 MVP (workspace scaffold, tree-sitter parsing, AST chunking, BM25 index, CLI `init/search/symbols`, incremental updates).
- **P1:** semantic + hybrid retrieval and REST integration for ForgePipe workflows.
- **P2:** graph intelligence, MCP/gRPC depth, and production benchmark hardening.

See `ROADMAP.md` for the synchronized project roadmap.

## License

TBD

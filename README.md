# CodeForge

Ultra-fast code retrieval engine for AI agents.

CodeForge is a Rust-native engine that gives AI coding agents precisely the right context from any codebase, regardless of size. It combines structure-aware parsing (tree-sitter), hybrid search (BM25 + vector + graph), and AI-optimized output into a single, blazing-fast binary.

## Why

AI coding agents live or die by context quality. Current approaches — naive text search, pure vector search, file-level stuffing — waste tokens and miss relevant code. CodeForge solves this by understanding code as *structure*, not text.

## Quick Start

```bash
# Install (from source)
cargo install --path crates/cli

# Index a codebase
codeforge init .

# Search with natural language
codeforge search "authentication handler"

# Search with file filter
codeforge search "parse config" --file "src/" --limit 5

# List symbols
codeforge symbols Engine
codeforge symbols --file src/main.rs
```

## Example Output

```
$ codeforge init .
Indexing /home/user/my-project...
Indexed 28 files, 136 chunks, 370 symbols in 0.27s

$ codeforge search "fn main"
1. crates/cli/src/main.rs [L55-L101] (Rust) score=11.528
   fn main() -> Result<()>
   | fn main() -> Result<()> {
   |     tracing_subscriber::fmt()
   |     ...

$ codeforge symbols Config
KIND         NAME                 FILE                    LINES
--------------------------------------------------------------
Struct       Config               src/engine.rs           L35-L44
Import       Config               src/lib.rs              L13-L14
```

## Key Features

- **AST-aware chunking** — Tree-sitter parsing across 10 language families; never splits a function in half
- **BM25 full-text search** — Tantivy-backed with a custom code tokenizer (camelCase, snake_case, dot.path splitting)
- **Concurrent symbol table** — DashMap-backed with exact, prefix, and pattern matching
- **Incremental indexing** — Sub-500ms updates via file watcher with debouncing
- **Single binary, zero runtime deps** — No JVM, no Docker, no external databases

## Supported Languages

| Tier | Languages | Capabilities |
|------|-----------|-------------|
| **Tier 1** | Rust, Python, TypeScript, TSX, JavaScript, Go, Java, C, C++, C# | Full AST parsing + entity extraction + symbol resolution |
| **Tier 2** | *(Phase 2)* Ruby, PHP, Swift, Kotlin, Scala, Zig, Elixir, Lua, Bash, SQL | AST chunking + basic indexing |
| **Tier 3** | Any tree-sitter grammar (40+) | Text-mode fallback |

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                       CodeForge Engine                        │
│                                                               │
│  ┌────────────┐  ┌──────────┐  ┌──────────┐  ┌────────────┐ │
│  │ Tree-sitter │  │ Tantivy  │  │  Symbol  │  │   File     │ │
│  │ AST Parser  │  │  (BM25)  │  │  Table   │  │  Watcher   │ │
│  └─────┬──────┘  └────┬─────┘  └────┬─────┘  └─────┬──────┘ │
│        │              │              │               │        │
│  ┌─────▼──────┐  ┌────▼─────┐  ┌────▼─────┐  ┌─────▼──────┐ │
│  │   cAST     │  │ Code     │  │ DashMap  │  │  notify    │ │
│  │  Chunker   │  │ Tokenizer│  │ (conc.)  │  │ (debounce) │ │
│  └────────────┘  └──────────┘  └──────────┘  └────────────┘ │
│                                                               │
│  ┌──────────────────────────────────────────────────────────┐ │
│  │            API Layer: CLI (clap) / REST (Phase 2)         │ │
│  └──────────────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────────────┘
```

## Rust Library API

```rust
use codeforge_core::{Engine, IndexConfig, SearchQuery};

// Index a project
let config = IndexConfig::new("./my-project");
let engine = Engine::init("./my-project", config)?;

// Search
let results = engine.search(
    SearchQuery::new("authentication handler")
        .with_limit(10)
        .with_file_filter("src/")
)?;

for result in &results {
    println!("{}:{}-{} score={:.2}",
        result.file_path, result.line_start, result.line_end, result.score);
}

// Symbol lookup
let symbols = engine.symbols("Config", None)?;

// Incremental re-indexing
let mut engine = engine;
engine.reindex_file(Path::new("src/main.rs"))?;
```

## Performance

| Metric | Measured | Target |
|--------|----------|--------|
| Index speed | 28 files in 0.27s (~100 files/s) | >800 files/s |
| Incremental update | <500ms | <500ms |
| Test suite | 111 tests in ~3s | <30s |
| Binary size | TBD | <50MB |

## Tech Stack

| Component | Crate | Purpose |
|-----------|-------|---------|
| AST Parsing | `tree-sitter` 0.26 | Incremental, multi-language parsing |
| Full-text search | `tantivy` 0.22 | BM25 scoring, inverted index |
| Symbol table | `dashmap` 6 | Lock-free concurrent hash map |
| Parallelism | `rayon` 1 | Parallel file processing |
| File watching | `notify` 8 | Cross-platform fs event monitoring |
| Serialization | `bitcode` 0.6 | Fast binary serialization |
| Content hashing | `xxhash-rust` 0.8 | Change detection (xxh3) |
| CLI | `clap` 4 | Command-line interface |
| Logging | `tracing` 0.1 | Structured logging |

## Roadmap

See [ROADMAP.md](ROADMAP.md) for the full phased delivery plan.

| Phase | Status | Description |
|-------|--------|-------------|
| **Phase 1: Foundation** | **Complete** | AST parsing, BM25 search, CLI, file watcher, 111 tests |
| Phase 2: Semantic Search | Planned | Vector search, hybrid retrieval, REST API |
| Phase 3: Graph Intelligence | Planned | Code graph, PageRank, repo maps |
| Phase 4: Agent Integration | Planned | MCP server, gRPC, retrieval strategies |
| Phase 5: Production Hardening | Planned | Benchmarks, fuzzing, Tier 2 languages |

## Development

```bash
# Build
cargo build --workspace

# Test
cargo test --workspace

# Lint
cargo clippy --workspace -- -D warnings

# Format
cargo fmt --all
```

## 2026 Priority Alignment

- **P0:** ship Phase 1 MVP (workspace scaffold, tree-sitter parsing, AST chunking, BM25 index, CLI `init/search/symbols`, incremental updates).
- **P1:** semantic + hybrid retrieval and REST integration for ForgePipe workflows.
- **P2:** graph intelligence, MCP/gRPC depth, and production benchmark hardening.

See `ROADMAP.md` for the synchronized project roadmap.

## License

MIT

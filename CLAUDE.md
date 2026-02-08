# CodeForge — Claude Code Instructions

## Project Overview

CodeForge is a Rust-native code retrieval engine for AI agents. It combines tree-sitter AST parsing, hybrid search (BM25 + vector + graph), and AI-optimized output into a single binary.

**PRD**: `PRD.md` at repo root — the authoritative specification.
**Roadmap**: `ROADMAP.md` — phased delivery plan with status.

## Current Status

**Phase 1 (Foundation): COMPLETE** — 111 tests, 28 source files indexed in 0.27s

Delivered:
- Tree-sitter AST parsing for 10 language variants (Rust, Python, TS, TSX, JS, Go, Java, C, C++, C#)
- cAST recursive split-then-merge chunker
- Tantivy BM25 full-text index with custom CodeTokenizer
- DashMap-based symbol table with bitcode persistence
- Engine facade: `init`, `open`, `search`, `symbols`, `reindex_file`, `remove_file`, `watch`
- CLI: `codeforge init`, `codeforge search`, `codeforge symbols`
- File watcher with debounced incremental re-indexing
- Index persistence to `.codeforge/` directory
- 97 unit tests + 14 integration tests

## Language & Toolchain

- **Language**: Rust (stable toolchain, 2024 edition)
- **Build**: `cargo build --workspace`
- **Test**: `cargo test --workspace`
- **Linting**: `cargo clippy --workspace -- -D warnings`
- **Formatting**: `cargo fmt --check` / `cargo fmt --all`

## Architecture

```
codeforge/
├── Cargo.toml             # Workspace root (virtual manifest)
├── crates/
│   ├── core/              # Engine library
│   │   ├── src/
│   │   │   ├── lib.rs          # Re-exports: Engine, SearchQuery, SearchResult, etc.
│   │   │   ├── engine.rs       # Engine facade (init, open, search, symbols, watch)
│   │   │   ├── error.rs        # CodeforgeError (thiserror)
│   │   │   ├── config.rs       # IndexConfig, ChunkConfig
│   │   │   ├── language/       # 10 languages, LanguageSupport trait, registry
│   │   │   ├── parser/         # tree-sitter parser + DashMap tree cache
│   │   │   ├── chunker/        # cAST + line-based fallback
│   │   │   ├── index/          # Tantivy schema + CodeTokenizer + BM25
│   │   │   ├── retriever/      # Retriever trait + BM25Retriever
│   │   │   ├── symbols/        # DashMap symbol table + bitcode persistence
│   │   │   ├── persistence/    # .codeforge/ directory management
│   │   │   └── watcher/        # notify-based debounced file watcher
│   │   └── tests/              # Integration tests
│   ├── cli/               # CLI binary (clap)
│   └── server/            # API server (Phase 2 — placeholder)
├── PRD.md                 # Product requirements
├── ROADMAP.md             # Delivery roadmap
├── CLAUDE.md              # This file
└── README.md
```

## Key Dependencies (Phase 1)

| Crate | Version | Purpose |
|-------|---------|---------|
| `tree-sitter` | 0.26 | AST parsing (10 language grammars) |
| `tantivy` | 0.22 | BM25 full-text search |
| `notify` | 8 | File watching |
| `clap` | 4 | CLI (derive macros) |
| `dashmap` | 6 | Concurrent symbol table |
| `rayon` | 1 | Parallel file processing |
| `serde` + `serde_json` | 1 | JSON serialization |
| `bitcode` | 0.6 | Binary serialization (serde feature) |
| `xxhash-rust` | 0.8 | Content hashing (xxh3) |
| `thiserror` | 2 | Library error types |
| `anyhow` | 1 | Binary error handling |
| `tracing` | 0.1 | Structured logging |

**Note**: `bitcode` uses `bitcode::serialize`/`bitcode::deserialize` (serde feature), NOT `bitcode::encode`/`bitcode::decode`.

## Coding Standards

- No `unsafe` in application code (vendored C deps for tree-sitter are acceptable)
- Use `thiserror` for library errors, `anyhow` for binary crates
- All public APIs must have doc comments
- Prefer `impl Trait` over dynamic dispatch where possible
- Use `tracing` for all logging (not `println!` or `eprintln!`)
- Test with both unit tests and integration tests
- `tree_sitter::Parser` is `!Send` — create fresh per call, never store in structs

## Performance Requirements

- Retrieval: <50ms p99 for `fast` strategy on 1M+ LoC
- Incremental index update: <500ms from file save
- Memory: <2GB for 1M LoC indexed
- Binary: <50MB statically linked

## Design Principles

1. **Structure over text** — code is a tree, not a string
2. **Hybrid by default** — no single retrieval method wins
3. **Incremental everything** — full re-indexing is a failure mode
4. **AI-native output** — results formatted for LLM comprehension
5. **Zero-config start** — `codeforge init .` just works
6. **Single binary** — no runtime dependencies

## Workflow

- Run `cargo test --workspace` before committing
- Run `cargo clippy --workspace -- -D warnings` to catch lint issues
- Format with `cargo fmt --all`
- Commit messages: imperative mood, concise (<72 chars first line)

## Priority Alignment (2026)

- **P0:** implement Phase 1 MVP (workspace scaffold, tree-sitter parsing, AST chunking, BM25 index, CLI `init/search/symbols`, incremental updates, persistence).
- **P1:** semantic + hybrid retrieval and REST server path for ForgePipe workflow integration.
- **P2:** graph intelligence, MCP/gRPC depth, and production benchmark hardening.

Roadmap reference: `ROADMAP.md`.

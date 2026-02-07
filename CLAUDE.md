# CodeForge — Claude Code Instructions

## Project Overview

CodeForge is a Rust-native code retrieval engine for AI agents. It combines tree-sitter AST parsing, hybrid search (BM25 + vector + graph), and AI-optimized output into a single binary.

**PRD**: `PRD.md` at repo root — the authoritative specification.

## Language & Toolchain

- **Language**: Rust (stable toolchain, 2024 edition)
- **Build**: `cargo build`, `cargo test`
- **Linting**: `cargo clippy -- -D warnings`
- **Formatting**: `cargo fmt --check`

## Architecture

```
codeforge/
├── crates/
│   ├── core/          # Engine: parsing, indexing, retrieval, graph
│   ├── cli/           # CLI binary (clap)
│   └── server/        # API server (axum + tonic)
├── PRD.md             # Product requirements
├── CLAUDE.md          # This file
└── README.md
```

Cargo workspace with three crates: `core` (library), `cli` (binary), `server` (binary).

## Key Dependencies

- `tree-sitter` — AST parsing (30+ languages)
- `tantivy` — BM25 full-text search
- `hnsw_rs` or `instant-distance` — vector similarity (HNSW)
- `petgraph` — code dependency graph
- `tiktoken-rs` — LLM token counting
- `notify` — file watching
- `gix` — pure-Rust git operations
- `axum` + `tonic` — HTTP + gRPC server
- `ort` — ONNX Runtime for local embeddings
- `clap` — CLI
- `serde` + `bincode` — serialization
- `dashmap` — concurrent symbol table
- `rayon` — parallel iteration
- `tracing` — structured logging

## Coding Standards

- No `unsafe` in application code (vendored C deps for tree-sitter are acceptable)
- Use `thiserror` for library errors, `anyhow` for binary crates
- All public APIs must have doc comments
- Prefer `impl Trait` over dynamic dispatch where possible
- Use `tracing` for all logging (not `println!` or `eprintln!`)
- Test with both unit tests and integration tests against real-world repos

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

- Run `cargo test` before committing
- Run `cargo clippy -- -D warnings` to catch lint issues
- Format with `cargo fmt`
- Commit messages: imperative mood, concise (<72 chars first line)

## Priority Alignment (2026)

- **P0:** implement Phase 1 MVP (workspace scaffold, tree-sitter parsing, AST chunking, BM25 index, CLI `init/search/symbols`, incremental updates, persistence).
- **P1:** semantic + hybrid retrieval and REST server path for ForgePipe workflow integration.
- **P2:** graph intelligence, MCP/gRPC depth, and production benchmark hardening.

Roadmap reference: `ROADMAP.md`.

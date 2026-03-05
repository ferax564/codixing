# Contributing to Codixing

Thank you for your interest in contributing. This document covers how to get started, the development workflow, and what we expect from contributors.

## Contributor License Agreement (CLA)

**Before your first pull request can be merged, you must sign the Contributor License Agreement** (`CLA.md`).

The CLA grants the project maintainers a license to use your contributions under both the MIT license and any future commercial license. This is required to support the dual-license model that keeps Codixing free for individuals while funding enterprise development.

For individuals: add a comment to your PR with the text:

> I have read and agree to the Contributor License Agreement in CLA.md.

For corporate contributors, contact the maintainers directly.

---

## Development Setup

### Prerequisites

- Rust stable toolchain (`rustup update stable`)
- On Ubuntu/Debian: `sudo apt-get install -y libclang-dev clang`
- On macOS: Xcode command-line tools (`xcode-select --install`)

### Build

```bash
git clone https://github.com/your-org/codixing
cd codixing
cargo build --workspace
```

### Test

```bash
cargo test --workspace
```

The full suite runs in about 30 seconds on a modern machine. A subset of tests marked `#[ignore]` require downloading embedding models (~400MB) and are opt-in:

```bash
cargo test --workspace -- --ignored   # requires model download
```

### Lint and format

```bash
cargo clippy --workspace -- -D warnings
cargo fmt --all
```

CI enforces both. Fix clippy warnings before submitting a PR — the `-D warnings` flag makes warnings into errors.

---

## Project Structure

```
crates/
  core/       — Engine library (AST parsing, search, graph, embeddings)
  cli/        — codixing binary (clap)
  server/     — REST API server (axum)
  mcp/        — MCP server binary (JSON-RPC 2.0 over stdio/Unix socket)
editors/
  vscode/     — VS Code / Cursor extension (TypeScript)
docs/         — Landing page (HTML/CSS, no framework)
```

Start with `crates/core/src/engine.rs` — the `Engine` struct is the central facade; all public functionality flows through it.

---

## Coding Standards

- **No `unsafe`** in application code. Tree-sitter C bindings are the only exception.
- Use `thiserror` for library errors (`crates/core`), `anyhow` for binary crates.
- Use `tracing` for all logging — never `println!` or `eprintln!`.
- `tree_sitter::Parser` is `!Send` — create a fresh parser per call; never store it in a struct.
- When modifying `CodeGraph`, keep `path_to_node: HashMap<String, NodeIndex>` in sync. petgraph uses swap-remove on node deletion, which invalidates untracked `NodeIndex` values.
- `bitcode` uses `bitcode::serialize` / `bitcode::deserialize` (serde feature), not `bitcode::encode` / `bitcode::decode`.
- All public API items must have doc comments.

---

## Submitting a Pull Request

1. Fork the repo and create a branch from `main`.
2. Write or update tests. New functionality without tests will not be merged.
3. Run the full check: `cargo test --workspace && cargo clippy --workspace -- -D warnings && cargo fmt --check`.
4. Open a PR against `main`. Fill in the PR template.
5. Sign the CLA (see above) if this is your first contribution.

**Keep PRs focused.** One feature or bug fix per PR. Large refactors should be discussed in a GitHub Discussion or issue first.

---

## Reporting Issues

Use the GitHub issue templates. For security vulnerabilities, follow the process in `SECURITY.md` — do not open a public issue.

---

## Language Grammar Contributions

Codixing uses tree-sitter grammars. To add a new language:

1. Add the grammar crate to `crates/core/Cargo.toml`.
2. Implement `LanguageSupport` in `crates/core/src/language/`.
3. Register it in `crates/core/src/language/registry.rs`.
4. Add at least one integration test in `crates/core/tests/`.

See `crates/core/src/language/rust.rs` as the reference implementation.

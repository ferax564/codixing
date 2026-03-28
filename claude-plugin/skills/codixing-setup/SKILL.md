---
name: codixing-setup
description: Set up Codixing code retrieval engine for the current project. Indexes the codebase and registers the MCP server with Claude Code. Use when starting work on a new project or when the user asks to set up Codixing.
user-invocable: true
disable-model-invocation: false
argument-hint: "[--embeddings] [--model bge-small-en]"
allowed-tools: Bash, Read, Glob, Grep
---

# Codixing Setup

Set up the Codixing code retrieval engine for the current project. This gives you AST-aware search, dependency graph intelligence, and 54 MCP tools.

## Steps

### 1. Check if codixing-mcp is available

```bash
npx -y codixing-mcp -V
```

If this fails, tell the user to install via:
```bash
curl -fsSL https://codixing.com/install.sh | sh
```

### 2. Check for existing index

Look for a `.codixing/` directory in the project root. If it exists, skip to step 4 (just register the MCP server).

### 3. Index the project

Run the indexer. By default, use BM25-only (fast, no dependencies):

```bash
npx -y codixing-mcp --root . </dev/null
```

Wait — that starts the MCP server. Instead, if `codixing` CLI is available:

```bash
codixing init . --no-embeddings
```

If `codixing` CLI is not available but `codixing-mcp` is, the MCP server will auto-create a BM25 index on first connection. Just proceed to step 4.

If the user passed `--embeddings` or `--model`, use:
```bash
codixing init . --model $1
```

This requires ONNX Runtime at `~/.local/lib/libonnxruntime.dylib` (macOS) or `~/.local/lib/libonnxruntime.so` (Linux).

### 4. Register MCP server with Claude Code

```bash
claude mcp add codixing -- npx -y codixing-mcp --root .
```

### 5. Verify

Tell the user to restart Claude Code to pick up the new MCP server. After restart, all 48 Codixing tools will be available.

Suggest they try:
- `code_search` for finding code
- `find_symbol` for symbol lookup
- `get_repo_map` for architecture overview
- `explain` for understanding any symbol

## Arguments

- No arguments: BM25-only index (fastest, recommended)
- `--embeddings`: Enable semantic search with default model (bge-small-en)
- `--model <name>`: Choose embedding model (bge-small-en, bge-base-en, bge-large-en, snowflake-arctic-l)

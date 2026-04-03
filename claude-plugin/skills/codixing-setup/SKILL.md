---
name: codixing-setup
description: Set up Codixing code retrieval engine for the current project. Installs the binary and indexes the codebase.
user-invocable: true
disable-model-invocation: false
argument-hint: "[--embeddings] [--model bge-small-en]"
allowed-tools: Bash, Read, Glob
---

# Codixing Setup

Set up the Codixing code retrieval engine for the current project.

## Steps

### 1. Check if codixing is available

```bash
codixing --version
```

If this fails, install:
```bash
curl -fsSL https://codixing.com/install.sh | sh
```

Or via npm:
```bash
npx -y codixing-mcp --version
```

### 2. Check for existing index

Look for a `.codixing/` directory in the project root. If it exists, skip to step 4.

### 3. Index the project

```bash
codixing init .
```

If the user passed `--embeddings` or `--model`:
```bash
codixing init . --model $1
```

This requires ONNX Runtime at `~/.local/lib/libonnxruntime.dylib` (macOS) or `~/.local/lib/libonnxruntime.so` (Linux).

### 4. Verify

Test the index with a search:
```bash
codixing search "main entry point" --limit 3
```

Suggest the user try:
- `codixing search "query"` for finding code
- `codixing symbols Name` for symbol lookup
- `codixing graph --map` for architecture overview

## Arguments

- No arguments: BM25-only index (fastest, recommended)
- `--embeddings`: Enable semantic search with default model (bge-small-en)
- `--model <name>`: Choose embedding model (bge-small-en, bge-base-en, all-minilm-l6-v2)

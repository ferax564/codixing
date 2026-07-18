---
name: codixing-setup
description: Set up Codixing code retrieval engine for the current project. Installs the binary and indexes the codebase.
user-invocable: true
disable-model-invocation: false
argument-hint: "[--embed] [--model bge-small-en]"
allowed-tools: Bash, Read, Glob
---

# Codixing Setup

Set up the Codixing code retrieval engine for the current project.

## Steps

### 1. Check whether the CLI is available

```bash
codixing --version
```

If this fails, install:
```bash
curl --proto '=https' --proto-redir '=https' -fsSLo /tmp/codixing-install.sh https://codixing.com/install.sh
sh /tmp/codixing-install.sh
```

The setup workflow needs the `codixing` CLI, so the MCP-only
`npx -y codixing-mcp` package is not a substitute for this step.

### 2. Check for existing index

Look for a `.codixing/` directory in the project root. If it exists, skip to step 4.

### 3. Index the project

```bash
codixing init .
```

If the user passed `--embed`:
```bash
codixing init . --embed
```

If the user selected a model, enable embeddings explicitly:
```bash
codixing init . --embed --model <name>
```

ONNX-based models require `ORT_DYLIB_PATH` to point to the exact shared-library
file (`libonnxruntime.dylib`, `libonnxruntime.so*`, or `onnxruntime.dll`). Do not
assume a fixed `~/.local/lib` location: Python wheels and release archives use
version- and platform-specific paths. If the variable is missing, show the
portable discovery recipe in the plugin README and verify it with
`codixing doctor` before indexing. BM25-only and `model2vec` do not require ONNX
Runtime.

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
- `--embed`: Enable semantic search with the default model
- `--model <name>`: Choose an embedding model; requires `--embed` (for example, bge-small-en, bge-base-en, or jina-embed-code)

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

Inspect the layout rather than treating the `.codixing/` directory itself as
proof that indexing succeeded:

```bash
codixing doctor . --json
```

Only skip to step 4 when `index.status` is `ok`. A `missing` status continues
to step 3. For `partial`, run `codixing doctor .` for recovery guidance, then
either preserve recoverable artifacts with `codixing repair .` (which syncs by
default), or run `codixing init .` to rebuild a clean index. Re-run the JSON
check and do not continue until it reports `ok`.

### 3. Index the project

```bash
codixing init .
```

Indexing uses a bounded worker default: `min(available CPUs, 8)` on non-Windows
hosts and `min(available CPUs, 4)` on Windows. Keep that default initially;
pass `--threads <N>` only after measuring the repository and storage. Files over
2 MiB are skipped by default to bound indexing work and memory; use
`--max-file-bytes 0` only when unlimited file size is intentional.

If the user passed `--embed`:
```bash
codixing init . --embed
```

If the user selected a model, enable embeddings explicitly:
```bash
codixing init . --embed --model <name>
```

For a one-shot validation query on a very large repository, explicitly choose
`--strategy instant`, or `--strategy exact` for a known identifier or literal.
Those CLI strategies use the lean lexical read profile and skip graph, vector,
and reranker loading. `auto` and the other strategies open the full index.

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

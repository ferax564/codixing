#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO="${REPO:-$ROOT/benchmarks/repos/openclaw}"
OUTPUT_PREFIX="${OUTPUT_PREFIX:-external_competitor_benchmark}"
CBM_CACHE_DIR="${CBM_CACHE_DIR:-/tmp/cbm-benchmark/cache}"
CODEBASE_MEMORY_MCP="${CODEBASE_MEMORY_MCP:-}"
CODEBASE_MEMORY_PROJECT="${CODEBASE_MEMORY_PROJECT:-$(echo "${REPO#/}" | tr '/' '-')}"
SETUP_LOG="$ROOT/benchmarks/results/${OUTPUT_PREFIX}_setup.md"

mkdir -p "$ROOT/benchmarks/results"

cargo build --release -p codixing

{
  echo "# External Competitor Benchmark Setup"
  echo
  echo "**Date:** $(date '+%Y-%m-%d %H:%M:%S %Z')"
  echo "**Repo:** \`$REPO\`"
  echo "**Output prefix:** \`$OUTPUT_PREFIX\`"
  echo
  echo "## Indexing"
  echo
} > "$SETUP_LOG"

echo "[setup] indexing with codixing"
(
  cd "$REPO"
  { /usr/bin/time -p "$ROOT/target/release/codixing" init .; } \
    2> "$ROOT/benchmarks/results/${OUTPUT_PREFIX}_codixing_index.log"
)
{
  echo "- Codixing index log: \`benchmarks/results/${OUTPUT_PREFIX}_codixing_index.log\`"
} >> "$SETUP_LOG"

TOOLS=(--tool codixing --tool grep)
if [[ -n "$CODEBASE_MEMORY_MCP" && -x "$CODEBASE_MEMORY_MCP" ]]; then
  echo "[setup] indexing with codebase-memory-mcp"
  mkdir -p "$CBM_CACHE_DIR"
  { /usr/bin/time -p env CBM_CACHE_DIR="$CBM_CACHE_DIR" "$CODEBASE_MEMORY_MCP" \
      cli index_repository "{\"repo_path\":\"$REPO\"}"; } \
    > "$ROOT/benchmarks/results/${OUTPUT_PREFIX}_codebase_memory_index.out" \
    2> "$ROOT/benchmarks/results/${OUTPUT_PREFIX}_codebase_memory_index.log"
  {
    echo "- codebase-memory-mcp binary: \`$CODEBASE_MEMORY_MCP\`"
    echo "- codebase-memory-mcp project: \`$CODEBASE_MEMORY_PROJECT\`"
    echo "- codebase-memory-mcp cache: \`$CBM_CACHE_DIR\`"
    echo "- codebase-memory-mcp index log: \`benchmarks/results/${OUTPUT_PREFIX}_codebase_memory_index.log\`"
    echo "- codebase-memory-mcp index output: \`benchmarks/results/${OUTPUT_PREFIX}_codebase_memory_index.out\`"
  } >> "$SETUP_LOG"
  TOOLS+=(--tool codebase-memory-mcp)
else
  {
    echo "- codebase-memory-mcp skipped: set \`CODEBASE_MEMORY_MCP=/path/to/codebase-memory-mcp\` to include it."
  } >> "$SETUP_LOG"
fi

echo "[run] benchmark"
python3 "$ROOT/benchmarks/competitor_benchmark.py" \
  --repo "$REPO" \
  --validate-only

CODEBASE_MEMORY_MCP="$CODEBASE_MEMORY_MCP" \
CODEBASE_MEMORY_PROJECT="$CODEBASE_MEMORY_PROJECT" \
CBM_CACHE_DIR="$CBM_CACHE_DIR" \
python3 "$ROOT/benchmarks/competitor_benchmark.py" \
  --repo "$REPO" \
  --include-disabled \
  "${TOOLS[@]}" \
  --output-prefix "$OUTPUT_PREFIX"

{
  echo
  echo "## Results"
  echo
  echo "- Markdown: \`benchmarks/results/${OUTPUT_PREFIX}.md\`"
  echo "- JSON: \`benchmarks/results/${OUTPUT_PREFIX}.json\`"
} >> "$SETUP_LOG"

echo "[done] $SETUP_LOG"

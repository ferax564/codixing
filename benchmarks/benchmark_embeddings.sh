#!/usr/bin/env bash
# Benchmark embedding models: BM25-only, BgeSmallEn, BgeBaseEn,
#   NomicEmbedCode, SnowflakeArcticEmbedL, Qwen3
# Measures: init time, search latency, retrieval quality
set -euo pipefail

CODIXING=./target/release/codixing
export ORT_DYLIB_PATH="$HOME/.local/lib/libonnxruntime.so"
export LD_LIBRARY_PATH="$HOME/.local/lib"
RESULTS_DIR=".benchmark_results"
mkdir -p "$RESULTS_DIR"
rm -f "$RESULTS_DIR/timings.tsv" "$RESULTS_DIR"/*.txt

QUERIES=(
  "how does BM25 search work"
  "vector embedding model initialization"
  "parse AST tree-sitter nodes"
  "graph pagerank dependency scoring"
  "MCP tool dispatch JSON-RPC"
  "cyclomatic complexity calculation"
  "file watcher incremental reindex"
  "symbol rename across codebase"
  "chunk metadata serialization"
  "test discover functions by annotation"
)

ms_cmd() { local s; s=$(date +%s%N); eval "$@" >/dev/null 2>&1; echo "$(( ($(date +%s%N) - s) / 1000000 ))"; }

search_query() {
  local q="$1" strategy="$2"
  "$CODIXING" search "$q" --limit 5 --strategy "$strategy" 2>/dev/null || true
}

# --------------------------------------------------------------------------
# BASELINE: BM25-only
# --------------------------------------------------------------------------
echo "================================================================"
echo "BASELINE: BM25-only (no embeddings)"
echo "================================================================"
rm -rf .codixing
BM25_INIT_MS=$(ms_cmd '"$CODIXING" init . --no-embeddings')
echo "Init time: ${BM25_INIT_MS}ms"

echo "Search latency (instant/BM25):"
TOTAL_BM25=0
for q in "${QUERIES[@]}"; do
  ms=$(ms_cmd '"$CODIXING" search "$q" --limit 5 --strategy instant')
  printf "  [%3dms] %s\n" "$ms" "$q"
  TOTAL_BM25=$(( TOTAL_BM25 + ms ))
  slug=$(echo "$q" | tr ' ' '_' | tr -cd 'a-zA-Z0-9_')
  search_query "$q" "instant" > "$RESULTS_DIR/bm25_${slug}.txt"
done
AVG_BM25=$(( TOTAL_BM25 / ${#QUERIES[@]} ))
echo "  Avg: ${AVG_BM25}ms"
echo "bm25_only|${BM25_INIT_MS}|${AVG_BM25}|${BM25_INIT_MS}" >> "$RESULTS_DIR/timings.tsv"

# --------------------------------------------------------------------------
# Model benchmark
# --------------------------------------------------------------------------
benchmark_model() {
  local model_name="$1" model_flag="$2" dims="$3" strategy="$4"

  echo ""
  echo "================================================================"
  echo "MODEL: $model_name  ($dims dims, strategy=$strategy)"
  echo "================================================================"

  rm -rf .codixing

  echo -n "Initialising with $model_name..."
  local start_ms init_ms
  start_ms=$(date +%s%N)
  "$CODIXING" init . --model "$model_flag" 2>&1 | grep -oP 'Indexed.*' || true
  init_ms=$(( ($(date +%s%N) - start_ms) / 1000000 ))
  echo "Init total: ${init_ms}ms ($(( init_ms / 1000 ))s)"

  echo "Search latency (instant vs $strategy, cold process per call):"
  local total_i=0 total_v=0
  for q in "${QUERIES[@]}"; do
    local t_i t_v
    t_i=$(ms_cmd '"$CODIXING" search "$q" --limit 5 --strategy instant')
    t_v=$(ms_cmd '"$CODIXING" search "$q" --limit 5 --strategy '"$strategy"'')
    printf "  [%3dms / %4dms]  %s\n" "$t_i" "$t_v" "$q"
    total_i=$(( total_i + t_i ))
    total_v=$(( total_v + t_v ))

    slug=$(echo "$q" | tr ' ' '_' | tr -cd 'a-zA-Z0-9_')
    slug_model=$(echo "$model_name" | tr ' ' '_' | tr -cd 'a-zA-Z0-9_')
    search_query "$q" "$strategy" > "$RESULTS_DIR/${slug_model}_${slug}.txt"
  done
  local avg_i=$(( total_i / ${#QUERIES[@]} ))
  local avg_v=$(( total_v / ${#QUERIES[@]} ))
  echo "  Avg instant: ${avg_i}ms | Avg ${strategy}: ${avg_v}ms"

  echo "${model_name}|${init_ms}|${avg_v}|${avg_i}" >> "$RESULTS_DIR/timings.tsv"
}

benchmark_model "BgeSmallEn"          "bge-small-en"       384  "fast"
benchmark_model "BgeBaseEn"           "bge-base-en"        768  "fast"
benchmark_model "BgeLargeEn"          "bge-large-en"       1024 "fast"
benchmark_model "SnowflakeArcticL"    "snowflake-arctic-l" 1024 "fast"
benchmark_model "Qwen3SmallEmbedding" "qwen3"              1024 "thorough"

# --------------------------------------------------------------------------
# Retrieval quality
# --------------------------------------------------------------------------
echo ""
echo "================================================================"
echo "RETRIEVAL QUALITY — top-4 matching files per query"
echo "================================================================"

extract_files() {
  grep -oP '(crates|src)/[^\s:]+\.(rs|py|ts|js)' "$1" 2>/dev/null \
    | sed 's|crates/[a-z-]*/src/||' | head -4 | paste -sd '  ' || echo "(no results)"
}

for q in "${QUERIES[@]}"; do
  slug=$(echo "$q" | tr ' ' '_' | tr -cd 'a-zA-Z0-9_')
  echo ""
  echo "  Q: \"$q\""
  printf "    %-14s: %s\n" "BM25"        "$(extract_files "$RESULTS_DIR/bm25_${slug}.txt")"
  printf "    %-14s: %s\n" "BgeSmall"    "$(extract_files "$RESULTS_DIR/BgeSmallEn_${slug}.txt")"
  printf "    %-14s: %s\n" "BgeBase"     "$(extract_files "$RESULTS_DIR/BgeBaseEn_${slug}.txt")"
  printf "    %-14s: %s\n" "BgeLarge"    "$(extract_files "$RESULTS_DIR/BgeLargeEn_${slug}.txt")"
  printf "    %-14s: %s\n" "ArcticL"     "$(extract_files "$RESULTS_DIR/SnowflakeArcticL_${slug}.txt")"
  printf "    %-14s: %s\n" "Qwen3"       "$(extract_files "$RESULTS_DIR/Qwen3SmallEmbedding_${slug}.txt")"
done

# --------------------------------------------------------------------------
# Summary
# --------------------------------------------------------------------------
echo ""
echo "================================================================"
echo "SUMMARY"
echo "================================================================"
printf "%-24s | %12s | %10s | %10s | %s\n" "Model" "Init(ms)" "AvgVec(ms)" "AvgBM25(ms)" "Notes"
printf '%0.s-' {1..90}; echo
while IFS='|' read -r name init_ms avg_v avg_i; do
  case "$name" in
    bm25_only)          notes="no vectors; instant BM25 only" ;;
    BgeSmallEn*)        notes="384d ONNX, fastembed, quantized" ;;
    BgeBaseEn*)         notes="768d ONNX, fastembed, quantized" ;;
    BgeLargeEn*)        notes="1024d ONNX, fastembed, quantized" ;;
    SnowflakeArcticL*)  notes="1024d ONNX, fastembed, SOTA MTEB at 335M" ;;
    Qwen3*)             notes="1024d ONNX, last-token pool, thorough strategy" ;;
    *)                  notes="" ;;
  esac
  printf "%-24s | %12s | %10s | %10s | %s\n" "$name" "$init_ms" "$avg_v" "$avg_i" "$notes"
done < "$RESULTS_DIR/timings.tsv"

echo ""
echo "Results saved in $RESULTS_DIR/"

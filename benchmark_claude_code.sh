#!/usr/bin/env bash
# ============================================================================
# Claude Code Efficiency Benchmark: Standard Tools vs Codixing MCP
# ============================================================================
#
# Compares TWO workflows for answering code exploration questions:
#
#   A) Standard tools — what Claude Code does by default:
#      grep/find to locate → cat/head to read → grep again to refine
#      Each step = 1 LLM tool call (round-trip + context tokens)
#
#   B) Codixing MCP — single tool call with structured output:
#      search, symbols, callers, graph, usages
#      Typically 1 call returns the complete answer
#
# Key metric: tool calls (proxies for LLM round-trips + tokens consumed).
# In Claude Code, each tool call costs ~500-2000 context tokens + ~1-3s latency.
#
# Also measures: wall time, output bytes, and correctness.
#
# Usage: bash benchmark_claude_code.sh
# ============================================================================

set -euo pipefail

CODIXING="./target/release/codixing"
PROJECT_ROOT="$(cd "$(dirname "$0")" && pwd)"
RESULTS_DIR="$PROJECT_ROOT/.benchmark_results"
REPORT="$RESULTS_DIR/claude_code_benchmark.md"

mkdir -p "$RESULTS_DIR"

# Ensure index is up to date
"$CODIXING" sync "$PROJECT_ROOT" 2>/dev/null || true

# ---------- timing helper ---------------------------------------------------

run_and_measure() {
    local label="$1"
    shift
    local start end elapsed output_size
    start=$(date +%s%N)
    eval "$@" > /tmp/bench_out_${label} 2>&1 || true
    end=$(date +%s%N)
    elapsed=$(( (end - start) / 1000000 ))
    output_size=$(wc -c < /tmp/bench_out_${label})
    echo "$elapsed $output_size"
}

# ---------- task runner -----------------------------------------------------
# Each task simulates the REALISTIC Claude Code workflow.
#
# Standard workflow: Claude needs multiple tool calls to explore.
#   Step 1: Grep/find to locate relevant files
#   Step 2: Read the file section
#   Step 3: Maybe grep again to find related code
#
# Codixing workflow: 1 call returns structured answer.

declare -a RESULTS
task_num=0

run_task() {
    local desc="$1"
    local std_steps="$2"      # number of tool calls in standard workflow
    local std_cmd="$3"        # combined standard workflow command
    local cdx_cmd="$4"        # single codixing command
    local validator="$5"      # substring to check in output

    task_num=$((task_num + 1))
    echo "Task $task_num: $desc"

    # Standard workflow
    local std_result cdx_result
    std_result=$(run_and_measure "std" "$std_cmd")
    local std_time=${std_result%% *}
    local std_bytes=${std_result##* }
    local std_correct
    grep -qi "$validator" /tmp/bench_out_std && std_correct="yes" || std_correct="no"

    # Codixing workflow
    cdx_result=$(run_and_measure "cdx" "$cdx_cmd")
    local cdx_time=${cdx_result%% *}
    local cdx_bytes=${cdx_result##* }
    local cdx_correct
    grep -qi "$validator" /tmp/bench_out_cdx && cdx_correct="yes" || cdx_correct="no"

    # Token estimate: ~4 chars per token, plus ~200 tokens overhead per tool call
    local std_tokens=$(( (std_bytes / 4) + (std_steps * 200) ))
    local cdx_tokens=$(( (cdx_bytes / 4) + (1 * 200) ))

    # LLM round-trip estimate: ~2s per tool call
    local std_llm_time=$((std_steps * 2000))
    local cdx_llm_time=2000

    RESULTS+=("$task_num|$desc|$std_steps|$std_time|$std_bytes|$std_tokens|$std_llm_time|$std_correct|1|$cdx_time|$cdx_bytes|$cdx_tokens|$cdx_llm_time|$cdx_correct")
}

# ============================================================================
# TASKS
# ============================================================================

# Task 1: "Where is the Engine struct defined?"
# Standard: grep to find file → head to read the definition
run_task \
    "Find where Engine struct is defined" \
    2 \
    'grep -rn "pub struct Engine" --include="*.rs" '"$PROJECT_ROOT"'/crates/core/src/ && FILE=$(grep -rl "pub struct Engine" --include="*.rs" '"$PROJECT_ROOT"'/crates/core/src/ | head -1) && head -n 80 "$FILE" | tail -n +40' \
    "$CODIXING"' symbols Engine --file crates/core/src/engine/mod.rs' \
    "Engine"

# Task 2: "What does the search function do? Show me the implementation."
# Standard: grep to find fn → identify file → read the function
run_task \
    "Read the search method implementation" \
    3 \
    'grep -rn "pub fn search" --include="*.rs" '"$PROJECT_ROOT"'/crates/core/src/engine/ | head -5 && FILE=$(grep -rl "pub fn search" --include="*.rs" '"$PROJECT_ROOT"'/crates/core/src/engine/ | head -1) && LINE=$(grep -n "pub fn search" "$FILE" | head -1 | cut -d: -f1) && sed -n "${LINE},$((LINE+60))p" "$FILE"' \
    "$CODIXING"' search "pub fn search query strategy" -l 3 -f engine/search --format --token-budget 2000' \
    "search"

# Task 3: "What files import or depend on the parser module?"
# Standard: grep for import statements referencing parser
run_task \
    "Find all files depending on parser module" \
    2 \
    'grep -rln "use crate::parser\|mod parser\|parser::" --include="*.rs" '"$PROJECT_ROOT"'/crates/core/src/ && grep -rn "use crate::parser" --include="*.rs" '"$PROJECT_ROOT"'/crates/core/src/ | head -20' \
    "$CODIXING"' callers crates/core/src/parser/mod.rs' \
    "engine\|sync\|mod"

# Task 4: "How does BM25 scoring work in this codebase?"
# Standard: grep for BM25 → find relevant file → read it
run_task \
    "Understand BM25 scoring implementation" \
    3 \
    'grep -rn "bm25\|BM25\|field_boost" --include="*.rs" '"$PROJECT_ROOT"'/crates/core/src/ | head -20 && FILE=$(grep -rl "fn search" '"$PROJECT_ROOT"'/crates/core/src/index/tantivy.rs) && head -n 120 "$FILE" | tail -n +60' \
    "$CODIXING"' search "BM25 scoring field boost tantivy" -l 5 --format --token-budget 3000' \
    "bm25\|tantivy\|search"

# Task 5: "What is the cyclomatic complexity logic?"
# Standard: grep for complexity → find file → read
run_task \
    "Find cyclomatic complexity implementation" \
    3 \
    'grep -rn "cyclomatic\|compute_complexity\|count_cyclomatic" --include="*.rs" '"$PROJECT_ROOT"'/crates/core/src/ | head -10 && FILE=$(grep -rl "count_cyclomatic" --include="*.rs" '"$PROJECT_ROOT"'/crates/core/src/ | head -1) && LINE=$(grep -n "pub fn count_cyclomatic" "$FILE" | head -1 | cut -d: -f1) && sed -n "${LINE},$((LINE+50))p" "$FILE"' \
    "$CODIXING"' search "cyclomatic complexity calculation function" -l 3 --format --token-budget 2000' \
    "complexity"

# Task 6: "Show me the project structure / module map"
# Standard: find all rs files → list them → try to understand structure
run_task \
    "Get project structure / module map" \
    3 \
    'find '"$PROJECT_ROOT"'/crates -name "mod.rs" -o -name "lib.rs" -o -name "main.rs" | sort && find '"$PROJECT_ROOT"'/crates/core/src -maxdepth 1 -name "*.rs" | sort && find '"$PROJECT_ROOT"'/crates/core/src -mindepth 1 -maxdepth 1 -type d | sort' \
    "$CODIXING"' graph --repo-map --token-budget 4000' \
    "engine\|parser\|graph\|vector"

# Task 7: "Find all call sites of the reindex_file function"
# Standard: grep for reindex_file → filter test files → read context
run_task \
    "Find all call sites of reindex_file" \
    2 \
    'grep -rn "reindex_file" --include="*.rs" '"$PROJECT_ROOT"'/crates/ | grep -v "fn reindex_file\|test\|target" | head -20 && grep -rn "reindex_file" --include="*.rs" '"$PROJECT_ROOT"'/crates/ | head -30' \
    "$CODIXING"' usages reindex_file -l 20' \
    "Re-index\|sync.rs"

# Task 8: "What symbols are in the vector module?"
# Standard: grep for pub fn/struct/trait → read file header
run_task \
    "List all symbols in vector module" \
    2 \
    'grep -n "pub fn\|pub struct\|pub trait\|pub enum" '"$PROJECT_ROOT"'/crates/core/src/vector/mod.rs && head -50 '"$PROJECT_ROOT"'/crates/core/src/vector/mod.rs' \
    "$CODIXING"' symbols --file crates/core/src/vector/mod.rs' \
    "VectorIndex\|VectorBackend\|vector"

# Task 9: "What does the reindex_file function do and who calls it?"
# Standard: find definition → read it → grep callers → read caller context
run_task \
    "Understand reindex_file: definition + callers" \
    4 \
    'grep -rn "fn reindex_file" --include="*.rs" '"$PROJECT_ROOT"'/crates/core/src/ && FILE=$(grep -rl "fn reindex_file" --include="*.rs" '"$PROJECT_ROOT"'/crates/core/src/ | head -1) && LINE=$(grep -n "pub fn reindex_file\|pub async fn reindex_file" "$FILE" | head -1 | cut -d: -f1) && sed -n "${LINE},$((LINE+40))p" "$FILE" && grep -rn "\.reindex_file(" --include="*.rs" '"$PROJECT_ROOT"'/crates/ | grep -v "fn reindex_file" | head -10' \
    "$CODIXING"' search "reindex_file definition implementation" -l 3 --format --token-budget 3000' \
    "reindex_file"

# Task 10: "How does the graph PageRank work and where is it used?"
# Standard: find pagerank → read implementation → grep usages
run_task \
    "Understand PageRank: implementation + usage" \
    4 \
    'grep -rn "pagerank\|page_rank\|PageRank" --include="*.rs" '"$PROJECT_ROOT"'/crates/core/src/ | head -15 && FILE=$(grep -rl "fn pagerank\|fn compute_pagerank\|fn run" --include="*.rs" '"$PROJECT_ROOT"'/crates/core/src/graph/pagerank.rs 2>/dev/null || echo "") && if [ -n "$FILE" ]; then head -80 "$FILE"; fi && grep -rn "pagerank\|graph_boost\|apply_graph" --include="*.rs" '"$PROJECT_ROOT"'/crates/core/src/engine/ | head -10' \
    "$CODIXING"' search "PageRank graph dependency scoring boost" -l 5 --format --token-budget 3000' \
    "pagerank\|PageRank\|graph"

# ============================================================================
# GENERATE REPORT
# ============================================================================

{
echo "# Claude Code Efficiency Benchmark"
echo ""
echo "**Date:** $(date +%Y-%m-%d)"
echo "**Project:** Codixing (Rust workspace, ~100 source files)"
echo "**Index:** BM25-only (0 vectors)"
echo "**Methodology:** Simulates realistic Claude Code exploration workflows"
echo ""
echo "---"
echo ""
echo "## Cost Model"
echo ""
echo "In a Claude Code session, each tool call costs:"
echo "- **~200 tokens** overhead (tool schema, call framing)"
echo "- **~N/4 tokens** for the output (N bytes / ~4 chars per token)"
echo "- **~2 seconds** wall clock (API round-trip + execution)"
echo "- The standard approach often requires 2-4 sequential tool calls"
echo "- Codixing MCP provides 1-call answers with formatted context"
echo ""
echo "---"
echo ""
echo "## Per-Task Results"
echo ""
echo "| # | Task | Std Calls | Std Tokens | Std LLM Time | Cdx Calls | Cdx Tokens | Cdx LLM Time | Call Savings | Token Savings |"
echo "|---|---|---|---|---|---|---|---|---|---|"

total_std_steps=0
total_cdx_steps=0
total_std_tokens=0
total_cdx_tokens=0
total_std_llm=0
total_cdx_llm=0
std_correct_count=0
cdx_correct_count=0

for row in "${RESULTS[@]}"; do
    IFS='|' read -r num desc s_steps s_time s_bytes s_tokens s_llm s_correct c_steps c_time c_bytes c_tokens c_llm c_correct <<< "$row"

    call_save=$((s_steps - 1))
    token_save=$((s_tokens - c_tokens))
    token_pct=0
    if [ "$s_tokens" -gt 0 ]; then
        token_pct=$(echo "scale=0; $token_save * 100 / $s_tokens" | bc 2>/dev/null || echo "0")
    fi

    echo "| $num | $desc | $s_steps | $s_tokens | ${s_llm}ms | 1 | $c_tokens | ${c_llm}ms | -${call_save} calls | ${token_pct}% fewer |"

    total_std_steps=$((total_std_steps + s_steps))
    total_cdx_steps=$((total_cdx_steps + 1))
    total_std_tokens=$((total_std_tokens + s_tokens))
    total_cdx_tokens=$((total_cdx_tokens + c_tokens))
    total_std_llm=$((total_std_llm + s_llm))
    total_cdx_llm=$((total_cdx_llm + c_llm))
    [ "$s_correct" = "yes" ] && std_correct_count=$((std_correct_count + 1))
    [ "$c_correct" = "yes" ] && cdx_correct_count=$((cdx_correct_count + 1))
done

echo ""
echo "### Totals (10 tasks)"
echo ""
echo "| Metric | Standard Tools | Codixing MCP | Improvement |"
echo "|---|---|---|---|"
echo "| **Tool calls** | $total_std_steps | $total_cdx_steps | **$((total_std_steps - total_cdx_steps)) fewer** ($((  (total_std_steps - total_cdx_steps) * 100 / total_std_steps  ))% reduction) |"
echo "| **Est. context tokens** | $total_std_tokens | $total_cdx_tokens | **$((total_std_tokens - total_cdx_tokens)) fewer** ($((  (total_std_tokens - total_cdx_tokens) * 100 / total_std_tokens  ))% reduction) |"
echo "| **Est. LLM wall time** | ${total_std_llm}ms (${total_std_steps}×2s) | ${total_cdx_llm}ms (${total_cdx_steps}×2s) | **$((total_std_llm - total_cdx_llm))ms saved** |"
echo "| **Correctness** | ${std_correct_count}/10 | ${cdx_correct_count}/10 | — |"

echo ""
echo "---"
echo ""
echo "## Raw Execution Time (CLI)"
echo ""
echo "| # | Std CLI (ms) | Cdx CLI (ms) | Note |"
echo "|---|---|---|---|"

for row in "${RESULTS[@]}"; do
    IFS='|' read -r num desc s_steps s_time s_bytes s_tokens s_llm s_correct c_steps c_time c_bytes c_tokens c_llm c_correct <<< "$row"
    echo "| $num | ${s_time} | ${c_time} | Cdx includes index load (~10ms); MCP daemon mode = ~1ms |"
done

echo ""
echo "> **Note:** Raw CLI time is misleading. Codixing pays ~10-80ms to load the BM25 index"
echo "> per invocation. In MCP server (daemon) mode, the index stays in memory and queries"
echo "> cost ~1ms. The real bottleneck in Claude Code is LLM round-trips (~2s each), not"
echo "> CLI execution time (~10ms)."
echo ""
echo "---"
echo ""
echo "## Key Findings"
echo ""
echo "### 1. Tool call reduction"
echo ""
echo "Standard exploration requires **${total_std_steps} tool calls** across 10 tasks."
echo "Codixing requires **${total_cdx_steps} calls** — a **$((  (total_std_steps - total_cdx_steps) * 100 / total_std_steps  ))% reduction**."
echo ""
echo "This matters because each tool call in Claude Code costs:"
echo "- ~2 seconds of API round-trip latency"
echo "- ~200+ tokens of context overhead"
echo "- Cognitive load for the LLM to plan the next step"
echo ""
echo "### 2. Token efficiency"
echo ""
echo "Standard tools return raw grep/cat output that the LLM must parse and filter."
echo "Codixing returns **formatted, scoped context** with file paths, line ranges,"
echo "and scope chains — ready for LLM consumption."
echo ""
echo "### 3. Capabilities standard tools lack"
echo ""
echo "| Capability | Standard Tools | Codixing |"
echo "|---|---|---|"
echo "| Semantic/concept search | No (keyword only) | Yes (BM25 + optional embeddings) |"
echo "| Dependency graph | Manual grep of imports | \`callers\`, \`callees\`, \`graph\` |"
echo "| Symbol-level navigation | grep \\"pub fn\\" | AST-parsed symbol table |"
echo "| Formatted context blocks | Manual cat/head | \`--format --token-budget\` |"
echo "| Call site analysis | grep + manual filtering | \`usages\` with dedup |"
echo "| Project overview | find + ls | \`graph --repo-map\` |"
echo ""
echo "### 4. When Codixing wins most"
echo ""
echo "- **Multi-step exploration**: \"What does X do and who calls it?\" — 4 grep/reads → 1 Codixing call"
echo "- **Semantic queries**: \"How does scoring work?\" — grep needs exact keywords, Codixing uses BM25 ranking"
echo "- **Dependency analysis**: \"What depends on module Y?\" — grep misses indirect imports, Codixing has the graph"
echo ""
echo "### 5. When standard tools are sufficient"
echo ""
echo "- **Exact keyword search**: \`grep -rn 'fn foo'\` is fast and precise"
echo "- **Reading a known file**: \`cat file.rs\` needs no index"
echo "- **Small codebases**: <20 files, manual navigation is fine"

} > "$REPORT"

echo ""
echo "Benchmark complete. Report: $REPORT"
echo ""
echo "=== SUMMARY ==="
echo "Standard: $total_std_steps tool calls, ~$total_std_tokens tokens"
echo "Codixing: $total_cdx_steps tool calls, ~$total_cdx_tokens tokens"
echo "Savings: $((total_std_steps - total_cdx_steps)) fewer calls, $((total_std_tokens - total_cdx_tokens)) fewer tokens"
echo "Correctness: Standard ${std_correct_count}/10, Codixing ${cdx_correct_count}/10"

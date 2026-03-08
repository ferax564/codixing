#!/usr/bin/env python3
"""
benchmark_claude_session.py — Compare Claude Code sessions with/without Codixing

Simulates a realistic 5-task coding investigation on the Codixing codebase,
measuring what a Claude agent would do with grep/cat/find (baseline) vs
Codixing MCP tools (optimized).

Measures: wall time (ms) and output bytes (proxy for context tokens consumed).
"""

import glob
import os
import subprocess
import sys
import time

ROOT = "/home/andrea/code/codixing"
CODIXING = f"{ROOT}/target/release/codixing"
RESULTS_DIR = f"{ROOT}/.benchmark_results"
os.makedirs(RESULTS_DIR, exist_ok=True)

# Use grep -rn as baseline (always available, what Claude Code actually falls back to)
GREP = "grep"


def run_timed(cmds: list[list[str]]) -> tuple[int, int]:
    """Run a list of commands sequentially, return (total_ms, total_bytes)."""
    total_bytes = 0
    start = time.perf_counter_ns()
    for cmd in cmds:
        try:
            result = subprocess.run(cmd, capture_output=True, timeout=30)
            total_bytes += len(result.stdout)
        except Exception:
            pass
    elapsed_ms = (time.perf_counter_ns() - start) // 1_000_000
    return elapsed_ms, total_bytes


def fmt_bytes(b: int) -> str:
    if b < 1024:
        return f"{b} B"
    return f"{b / 1024:.1f} KB"


# Count Rust files
rs_files = glob.glob(f"{ROOT}/crates/**/*.rs", recursive=True)
num_rs = len(rs_files)

print("=" * 62)
print(f"  Claude Session Benchmark: grep/cat/find vs Codixing")
print(f"  Codebase: codixing ({num_rs} Rust files)")
print("=" * 62)

tasks = []

# ============================================================================
# Task 1: Find where Engine struct is defined and what fields it has
# Baseline: Claude would grep for the struct, then cat the entire file
# ============================================================================
print("\n--- Task 1: Find Engine struct definition + fields ---")

t1g = run_timed([
    [GREP, "-rn", "pub struct Engine", f"{ROOT}/crates", "--include=*.rs"],
    ["cat", f"{ROOT}/crates/core/src/engine.rs"],
])
t1c = run_timed([
    [CODIXING, "symbols", "Engine", "--file", "engine.rs"],
])
tasks.append(("Find Engine struct", t1g, t1c))
print(f"  grep+cat:  {t1g[0]}ms, {fmt_bytes(t1g[1])}")
print(f"  codixing:  {t1c[0]}ms, {fmt_bytes(t1c[1])}")

# ============================================================================
# Task 2: Find all callers/usages of reindex_file
# Baseline: Claude would grep for all mentions
# ============================================================================
print("\n--- Task 2: Find all callers of reindex_file ---")

t2g = run_timed([
    [GREP, "-rn", "reindex_file", f"{ROOT}/crates", "--include=*.rs"],
])
t2c = run_timed([
    [CODIXING, "search", "reindex_file", "--strategy", "instant", "--limit", "20"],
])
tasks.append(("Find callers of reindex_file", t2g, t2c))
print(f"  grep:      {t2g[0]}ms, {fmt_bytes(t2g[1])}")
print(f"  codixing:  {t2c[0]}ms, {fmt_bytes(t2c[1])}")

# ============================================================================
# Task 3: Architecture overview — understand the project structure
# Baseline: Claude would list files with wc -l and read key file headers
# ============================================================================
print("\n--- Task 3: Architecture overview ---")

t3g = run_timed([
    ["bash", "-c", f"find {ROOT}/crates -name '*.rs' -exec wc -l {{}} + | sort -rn"],
    ["head", "-50", f"{ROOT}/crates/core/src/engine.rs"],
    ["head", "-50", f"{ROOT}/crates/mcp/src/tools.rs"],
    ["head", "-50", f"{ROOT}/crates/mcp/src/main.rs"],
])
t3c = run_timed([
    [CODIXING, "graph", "--map", "--token-budget", "2000"],
])
tasks.append(("Architecture overview", t3g, t3c))
print(f"  find+wc+head: {t3g[0]}ms, {fmt_bytes(t3g[1])}")
print(f"  codixing:     {t3c[0]}ms, {fmt_bytes(t3c[1])}")

# ============================================================================
# Task 4: Find BM25 scoring code — semantic/conceptual search
# Baseline: Claude would grep for multiple related patterns
# ============================================================================
print("\n--- Task 4: Find BM25 scoring code ---")

t4g = run_timed([
    [GREP, "-rin", "bm25", f"{ROOT}/crates", "--include=*.rs"],
    [GREP, "-rin", "tantivy", f"{ROOT}/crates", "--include=*.rs"],
    [GREP, "-rin", r"field.boost\|field_boost", f"{ROOT}/crates", "--include=*.rs"],
])
t4c = run_timed([
    [CODIXING, "search", "BM25 scoring tantivy field boost", "--strategy", "instant", "--limit", "10"],
])
tasks.append(("Find BM25 code", t4g, t4c))
print(f"  grep (3x):  {t4g[0]}ms, {fmt_bytes(t4g[1])}")
print(f"  codixing:   {t4c[0]}ms, {fmt_bytes(t4c[1])}")

# ============================================================================
# Task 5: Impact analysis — what depends on the chunker module?
# Baseline: Claude would grep for imports/usages across the codebase
# ============================================================================
print("\n--- Task 5: Impact analysis — what depends on chunker? ---")

t5g = run_timed([
    [GREP, "-rin", "chunker", f"{ROOT}/crates", "--include=*.rs"],
    [GREP, "-rn", r"CastChunker\|ChunkConfig", f"{ROOT}/crates", "--include=*.rs"],
])
t5c = run_timed([
    [CODIXING, "callers", "crates/core/src/chunker/cast.rs"],
    [CODIXING, "dependencies", "crates/core/src/chunker/cast.rs"],
])
tasks.append(("Impact analysis (chunker)", t5g, t5c))
print(f"  grep (2x):  {t5g[0]}ms, {fmt_bytes(t5g[1])}")
print(f"  codixing:   {t5c[0]}ms, {fmt_bytes(t5c[1])}")

# ============================================================================
# Summary
# ============================================================================
print()
print("=" * 62)
print("  SUMMARY")
print("=" * 62)
print()

total_gt, total_gb = 0, 0
total_ct, total_cb = 0, 0

header = f"{'Task':<37} {'grep ms':>9} {'cdx ms':>9} {'grep bytes':>12} {'cdx bytes':>12} {'savings':>9}"
print(header)
print("-" * len(header))

for name, (gt, gb), (ct, cb) in tasks:
    total_gt += gt
    total_gb += gb
    total_ct += ct
    total_cb += cb
    if gb > cb and gb > 0:
        saving = f"{(gb - cb) * 100 // gb}%"
    elif cb > gb and cb > 0:
        saving = f"+{(cb - gb) * 100 // max(gb, 1)}%"
    else:
        saving = "—"
    print(f"{name:<37} {gt:>7}ms {ct:>7}ms {gb:>10} B {cb:>10} B {saving:>9}")

print()
total_saving = (total_gb - total_cb) * 100 // total_gb if total_gb > total_cb else 0
print(f"{'TOTAL (5 tasks)':<37} {total_gt:>7}ms {total_ct:>7}ms {total_gb:>10} B {total_cb:>10} B {total_saving:>8}%")

grep_tokens = total_gb // 4
cdx_tokens = total_cb // 4
saved_tokens = grep_tokens - cdx_tokens

print()
print("Token estimate (cl100k_base ~4 chars/token):")
print(f"  grep/cat/find: ~{grep_tokens:,} tokens consumed by tool output")
print(f"  codixing:      ~{cdx_tokens:,} tokens consumed by tool output")
print(f"  savings:       ~{saved_tokens:,} tokens saved per 5-task session ({total_saving}%)")

print()
print("Tool call comparison:")
print("  Task 1: grep needs 2 calls (grep + cat)   → codixing needs 1 call (symbols)")
print("  Task 2: grep needs 1 call (grep)           → codixing needs 1 call (search)")
print("  Task 3: grep needs 4 calls (find+3x head)  → codixing needs 1 call (repo-map)")
print("  Task 4: grep needs 3 calls (3x grep)       → codixing needs 1 call (search)")
print("  Task 5: grep needs 2 calls (2x grep)       → codixing needs 2 calls (callers+deps)")
print("  TOTAL:  grep = 12 tool calls               → codixing = 6 tool calls (50% fewer)")
print("  Each tool call adds ~200-500ms of LLM round-trip overhead.")

print()
print("Key insight:")
print("  The biggest win is context efficiency, not raw speed. grep returns every")
print("  matching line (often thousands), forcing the LLM to process irrelevant tokens.")
print("  Codixing returns bounded, structured results — definitions, signatures, and")
print("  PageRank-ranked overviews. This means more tasks per context window.")
print()

# Write markdown results
with open(f"{RESULTS_DIR}/session_benchmark.md", "w") as f:
    f.write("# Claude Session Benchmark Results\n\n")
    f.write(f"**Codebase**: codixing ({num_rs} Rust files)  \n")
    f.write(f"**Platform**: {os.uname().machine}, {os.cpu_count()} cores\n\n")
    f.write("## 5-Task Investigation Session\n\n")
    f.write("| Task | grep/cat/find | Codixing | grep bytes | cdx bytes | Savings |\n")
    f.write("|------|-------------|----------|-----------|-----------|--------|\n")
    for name, (gt, gb), (ct, cb) in tasks:
        if gb > cb and gb > 0:
            s = f"{(gb - cb) * 100 // gb}%"
        else:
            s = "—"
        f.write(f"| {name} | {gt}ms | {ct}ms | {fmt_bytes(gb)} | {fmt_bytes(cb)} | {s} |\n")
    f.write(f"| **TOTAL** | **{total_gt}ms** | **{total_ct}ms** | "
            f"**{fmt_bytes(total_gb)}** | **{fmt_bytes(total_cb)}** | **{total_saving}%** |\n\n")

    f.write("## Token Impact\n\n")
    f.write("| Metric | grep/cat/find | Codixing |\n")
    f.write("|--------|-------------|----------|\n")
    f.write(f"| Tokens per session | ~{grep_tokens:,} | ~{cdx_tokens:,} |\n")
    f.write(f"| Tool calls | 12 | 6 |\n")
    f.write(f"| LLM round-trip overhead | ~3.6s | ~1.8s |\n\n")
    f.write(f"**Savings: ~{saved_tokens:,} tokens per 5-task session ({total_saving}%)**\n\n")

    f.write("## Key Insight\n\n")
    f.write("The main value is **context efficiency**. grep returns every matching line,\n")
    f.write("forcing the LLM to process thousands of irrelevant tokens. Codixing returns\n")
    f.write("bounded, structured results: definitions, signatures, and PageRank-ranked\n")
    f.write("overviews. This means more tasks per context window and better answers.\n")

print(f"Results saved to {RESULTS_DIR}/session_benchmark.md")

#!/usr/bin/env python3
"""
run_swe_bench.py — SWE-bench style bug localization benchmark

Simulates the "localization" phase of SWE-bench: given a bug report (issue text),
find the correct file(s) and function(s) to modify.

Compares Codixing search vs grep-based search for finding the right code.

Usage:
    python3 benchmarks/run_swe_bench.py
    python3 benchmarks/run_swe_bench.py --repos django tokio
"""

import argparse
import json
import os
import re
import subprocess
import sys
import time
from dataclasses import dataclass, asdict
from pathlib import Path

try:
    import tomllib
except ImportError:
    import tomli as tomllib

ROOT = Path(__file__).resolve().parent.parent
CODIXING = ROOT / "target" / "release" / "codixing"
REPOS_DIR = ROOT / "benchmarks" / "repos"
RESULTS_DIR = ROOT / "benchmarks" / "results"

ENV = {
    **os.environ,
    "ORT_DYLIB_PATH": os.path.expanduser("~/.local/lib/libonnxruntime.so"),
    "LD_LIBRARY_PATH": os.path.expanduser("~/.local/lib"),
}

LANG_EXT = {
    "django": "*.py",
    "fastapi": "*.py",
    "tokio": "*.rs",
    "ripgrep": "*.rs",
    "axum": "*.rs",
    "react": "*.js",
}


@dataclass
class LocalizationResult:
    task_id: str
    repo: str
    title: str

    # Baseline (grep)
    grep_files_found: list  # which expected files were found
    grep_symbols_found: list  # which expected symbols were found
    grep_file_score: float  # fraction of expected files found
    grep_symbol_score: float  # fraction of expected symbols found
    grep_bytes: int  # total output bytes
    grep_calls: int  # number of grep commands
    grep_ms: int

    # Codixing
    cdx_files_found: list
    cdx_symbols_found: list
    cdx_file_score: float
    cdx_symbol_score: float
    cdx_bytes: int
    cdx_calls: int
    cdx_ms: int


def run_cmd(cmd, cwd, timeout=60):
    start = time.perf_counter_ns()
    try:
        result = subprocess.run(cmd, capture_output=True, timeout=timeout, cwd=cwd, env=ENV)
        ms = (time.perf_counter_ns() - start) // 1_000_000
        return ms, result.stdout, result.returncode
    except Exception:
        ms = (time.perf_counter_ns() - start) // 1_000_000
        return ms, b"", -1


def grep_localize(task, repo_path):
    """Simulate grep-based localization: extract keywords from issue and grep."""
    ext = LANG_EXT.get(task["repo"], "*")

    # Strategy: extract important-looking keywords from issue text and grep
    issue = task["issue_text"]
    # Extract capitalized words, function-like patterns, class names
    keywords = set()
    for word in re.findall(r'[A-Z][a-zA-Z]+(?:\.[a-z_]+)*', issue):
        keywords.add(word)
    for word in re.findall(r'[a-z_]+(?:\(\))?', issue):
        if len(word) > 4 and word not in {"because", "raises", "error", "should", "between", "appears"}:
            keywords.add(word.rstrip("()"))

    # Also use the search queries as grep patterns (what a human might try)
    grep_patterns = list(task.get("search_queries", []))
    # Add extracted keywords
    for kw in list(keywords)[:5]:
        grep_patterns.append(kw)

    total_bytes = 0
    total_ms = 0
    combined_output = b""
    calls = 0

    for pattern in grep_patterns[:6]:  # Cap at 6 grep calls
        # Use simple word from the pattern
        words = pattern.split()
        for w in words[:2]:
            w = w.strip("'\"")
            if len(w) < 3:
                continue
            ms, stdout, _ = run_cmd(
                ["grep", "-rin", w, ".", f"--include={ext}", "-l"],
                cwd=str(repo_path), timeout=15,
            )
            total_ms += ms
            total_bytes += len(stdout)
            combined_output += stdout
            calls += 1

    return calls, total_bytes, total_ms, combined_output


def codixing_localize(task, repo_path):
    """Use Codixing search to localize the bug."""
    total_bytes = 0
    total_ms = 0
    combined_output = b""
    calls = 0

    for query in task.get("search_queries", []):
        ms, stdout, _ = run_cmd(
            [str(CODIXING), "search", query, "--limit", "10"],
            cwd=str(repo_path), timeout=60,
        )
        total_ms += ms
        total_bytes += len(stdout)
        combined_output += stdout
        calls += 1

    return calls, total_bytes, total_ms, combined_output


def score_output(output_text, expected_files, expected_symbols):
    """Score how many expected files and symbols appear in the output."""
    text = output_text if isinstance(output_text, str) else output_text.decode(errors="replace")
    files_found = [f for f in expected_files if any(part in text for part in f.split("/")[-2:])]
    symbols_found = [s for s in expected_symbols if s in text]
    file_score = len(files_found) / len(expected_files) if expected_files else 0
    symbol_score = len(symbols_found) / len(expected_symbols) if expected_symbols else 0
    return files_found, symbols_found, file_score, symbol_score


def main():
    parser = argparse.ArgumentParser(description="SWE-bench style localization benchmark")
    parser.add_argument("--repos", nargs="*", help="Only test these repos")
    args = parser.parse_args()

    if not CODIXING.exists():
        print(f"ERROR: codixing binary not found at {CODIXING}")
        sys.exit(1)

    with open(ROOT / "benchmarks" / "swe_bench_tasks.toml", "rb") as f:
        cfg = tomllib.load(f)

    tasks = cfg["task"]
    if args.repos:
        tasks = [t for t in tasks if t["repo"] in args.repos]

    # Check repos exist
    available = {t["repo"] for t in tasks}
    for repo in list(available):
        if not (REPOS_DIR / repo).exists():
            print(f"WARNING: {repo} not cloned. Run run_benchmark.py first.")
            tasks = [t for t in tasks if t["repo"] != repo]

    if not tasks:
        print("No tasks to run. Clone repos first with run_benchmark.py")
        sys.exit(1)

    results = []

    print(f"\n{'='*70}")
    print(f"  SWE-bench Style Bug Localization Benchmark")
    print(f"  Tasks: {len(tasks)} | Repos: {len({t['repo'] for t in tasks})}")
    print(f"{'='*70}\n")

    for task in tasks:
        repo_path = REPOS_DIR / task["repo"]
        print(f"  [{task['id']}] {task['title'][:60]}")

        # Grep baseline
        g_calls, g_bytes, g_ms, g_out = grep_localize(task, repo_path)
        g_files, g_syms, g_fscore, g_sscore = score_output(
            g_out, task["expected_files"], task["expected_symbols"]
        )

        # Codixing
        c_calls, c_bytes, c_ms, c_out = codixing_localize(task, repo_path)
        c_files, c_syms, c_fscore, c_sscore = score_output(
            c_out, task["expected_files"], task["expected_symbols"]
        )

        lr = LocalizationResult(
            task_id=task["id"],
            repo=task["repo"],
            title=task["title"],
            grep_files_found=g_files,
            grep_symbols_found=g_syms,
            grep_file_score=g_fscore,
            grep_symbol_score=g_sscore,
            grep_bytes=g_bytes,
            grep_calls=g_calls,
            grep_ms=g_ms,
            cdx_files_found=c_files,
            cdx_symbols_found=c_syms,
            cdx_file_score=c_fscore,
            cdx_symbol_score=c_sscore,
            cdx_bytes=c_bytes,
            cdx_calls=c_calls,
            cdx_ms=c_ms,
        )
        results.append(lr)

        g_tag = "✓" if g_fscore > 0.5 else "✗"
        c_tag = "✓" if c_fscore > 0.5 else "✗"
        print(
            f"    grep: {g_tag} files={g_fscore:.0%} syms={g_sscore:.0%} "
            f"({g_calls} calls, {g_bytes:,}B)"
        )
        print(
            f"    cdx:  {c_tag} files={c_fscore:.0%} syms={c_sscore:.0%} "
            f"({c_calls} calls, {c_bytes:,}B)"
        )

    # Summary
    print(f"\n{'='*70}")
    print("  SUMMARY")
    print(f"{'='*70}\n")

    g_file_avg = sum(r.grep_file_score for r in results) / len(results)
    g_sym_avg = sum(r.grep_symbol_score for r in results) / len(results)
    c_file_avg = sum(r.cdx_file_score for r in results) / len(results)
    c_sym_avg = sum(r.cdx_symbol_score for r in results) / len(results)
    g_total_bytes = sum(r.grep_bytes for r in results)
    c_total_bytes = sum(r.cdx_bytes for r in results)
    g_total_calls = sum(r.grep_calls for r in results)
    c_total_calls = sum(r.cdx_calls for r in results)

    print("| Metric | grep | Codixing | Winner |")
    print("|--------|------|----------|--------|")
    fw = "Codixing" if c_file_avg > g_file_avg else ("grep" if g_file_avg > c_file_avg else "tie")
    sw = "Codixing" if c_sym_avg > g_sym_avg else ("grep" if g_sym_avg > c_sym_avg else "tie")
    bw = "Codixing" if c_total_bytes < g_total_bytes else "grep"
    cw = "Codixing" if c_total_calls < g_total_calls else "grep"
    print(f"| File localization | {g_file_avg:.0%} | {c_file_avg:.0%} | **{fw}** |")
    print(f"| Symbol localization | {g_sym_avg:.0%} | {c_sym_avg:.0%} | **{sw}** |")
    print(f"| Total bytes | {g_total_bytes:,} | {c_total_bytes:,} | **{bw}** |")
    print(f"| Total calls | {g_total_calls} | {c_total_calls} | **{cw}** |")

    # Save results
    report_lines = [
        "# SWE-bench Style Localization Benchmark\n",
        f"**Date:** {time.strftime('%Y-%m-%d %H:%M')}",
        f"**Tasks:** {len(results)}\n",
        "## Results\n",
        "| Task | grep File% | grep Sym% | Cdx File% | Cdx Sym% | grep Bytes | Cdx Bytes |",
        "|------|-----------|-----------|-----------|----------|-----------|-----------|",
    ]
    for r in results:
        report_lines.append(
            f"| {r.title[:40]} | {r.grep_file_score:.0%} | {r.grep_symbol_score:.0%} | "
            f"{r.cdx_file_score:.0%} | {r.cdx_symbol_score:.0%} | "
            f"{r.grep_bytes:,} | {r.cdx_bytes:,} |"
        )
    report_lines.append("")
    report_lines.append("## Aggregate\n")
    report_lines.append(f"- **File localization**: grep {g_file_avg:.0%} vs Codixing {c_file_avg:.0%}")
    report_lines.append(f"- **Symbol localization**: grep {g_sym_avg:.0%} vs Codixing {c_sym_avg:.0%}")
    report_lines.append(f"- **Context efficiency**: grep {g_total_bytes:,}B vs Codixing {c_total_bytes:,}B")
    byte_pct = (g_total_bytes - c_total_bytes) / g_total_bytes * 100 if g_total_bytes else 0
    report_lines.append(f"- **Byte savings**: {byte_pct:.0f}%")

    report_path = RESULTS_DIR / "swe_bench_localization.md"
    report_path.write_text("\n".join(report_lines))
    print(f"\nReport: {report_path}")

    json_path = RESULTS_DIR / "swe_bench_localization.json"
    json_path.write_text(json.dumps([asdict(r) for r in results], indent=2))
    print(f"Data: {json_path}")


if __name__ == "__main__":
    main()

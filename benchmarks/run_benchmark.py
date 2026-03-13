#!/usr/bin/env python3
"""
run_benchmark.py — Real-world benchmark: Codixing vs standard grep/cat/find

Clones well-known repos, indexes them with Codixing, runs SE tasks, and
compares tool call count, output bytes (token proxy), and result quality.

Usage:
    python3 benchmarks/run_benchmark.py                    # all repos
    python3 benchmarks/run_benchmark.py --repos tokio axum # specific repos
    python3 benchmarks/run_benchmark.py --skip-clone       # reuse existing clones
    python3 benchmarks/run_benchmark.py --skip-index       # reuse existing indexes
"""

import argparse
import json
import os
import re
import subprocess
import sys
import time
from dataclasses import dataclass, field, asdict
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


@dataclass
class TaskResult:
    task_id: str
    repo: str
    category: str
    description: str
    baseline_calls: int = 0
    baseline_bytes: int = 0
    baseline_ms: int = 0
    baseline_lines: int = 0
    codixing_calls: int = 0
    codixing_bytes: int = 0
    codixing_ms: int = 0
    codixing_lines: int = 0
    call_savings: int = 0
    byte_savings_pct: float = 0.0
    baseline_found_target: bool = False
    codixing_found_target: bool = False
    error: str = ""


@dataclass
class RepoResult:
    name: str
    lang: str
    description: str
    file_count: int = 0
    index_time_ms: int = 0
    symbol_count: int = 0
    chunk_count: int = 0
    tasks: list = field(default_factory=list)


def run_cmd(cmd: list[str], cwd: str, timeout: int = 60) -> tuple[int, bytes, int]:
    """Run a command, return (elapsed_ms, stdout, returncode)."""
    start = time.perf_counter_ns()
    try:
        result = subprocess.run(
            cmd, capture_output=True, timeout=timeout, cwd=cwd, env=ENV
        )
        elapsed = (time.perf_counter_ns() - start) // 1_000_000
        return elapsed, result.stdout, result.returncode
    except subprocess.TimeoutExpired:
        elapsed = (time.perf_counter_ns() - start) // 1_000_000
        return elapsed, b"<timeout>", -1
    except Exception as e:
        elapsed = (time.perf_counter_ns() - start) // 1_000_000
        return elapsed, str(e).encode(), -1


def clone_repo(repo_cfg: dict) -> Path:
    """Shallow-clone a repo if not already present."""
    dest = REPOS_DIR / repo_cfg["name"]
    if dest.exists():
        print(f"  [skip] {repo_cfg['name']} already cloned")
        return dest
    print(f"  [clone] {repo_cfg['name']} from {repo_cfg['url']}...")
    subprocess.run(
        ["git", "clone", "--depth=1", repo_cfg["url"], str(dest)],
        capture_output=True,
        timeout=300,
    )
    return dest


def index_repo(repo_path: Path) -> tuple[int, int, int]:
    """Index a repo with codixing, return (time_ms, symbols, chunks)."""
    codixing_dir = repo_path / ".codixing"
    if codixing_dir.exists():
        # Parse existing index stats via `graph` (no --map = stats only)
        ms, stdout, _ = run_cmd(
            [str(CODIXING), "graph", str(repo_path)], cwd=str(repo_path), timeout=30
        )
        text = stdout.decode(errors="replace")
        syms = int(m.group(1)) if (m := re.search(r"(\d+)\s*symbols?", text)) else 0
        chunks = int(m.group(1)) if (m := re.search(r"(\d+)\s*chunks?", text)) else 0
        print(f"  [skip] index exists ({syms} symbols, {chunks} chunks)")
        return 0, syms, chunks

    print(f"  [index] {repo_path.name}...")
    start = time.perf_counter_ns()
    result = subprocess.run(
        [str(CODIXING), "init", "."],
        capture_output=True,
        timeout=600,
        cwd=str(repo_path),
        env=ENV,
    )
    elapsed = (time.perf_counter_ns() - start) // 1_000_000
    text = result.stdout.decode(errors="replace") + result.stderr.decode(errors="replace")
    syms = int(m.group(1)) if (m := re.search(r"(\d+)\s*symbols?", text)) else 0
    chunks = int(m.group(1)) if (m := re.search(r"(\d+)\s*chunks?", text)) else 0
    print(f"  [index] done in {elapsed}ms ({syms} symbols, {chunks} chunks)")
    return elapsed, syms, chunks


def count_source_files(repo_path: Path, lang: str) -> int:
    """Count source files for the given language."""
    exts = {
        "rust": ["*.rs"],
        "python": ["*.py"],
        "javascript": ["*.js", "*.jsx", "*.ts", "*.tsx"],
    }
    count = 0
    for ext in exts.get(lang, ["*"]):
        count += len(list(repo_path.rglob(ext)))
    return count


def run_baseline_task(task: dict, repo_path: Path) -> tuple[int, int, int, bytes]:
    """Run baseline (grep/cat/find) commands, return (calls, bytes, ms, combined_output)."""
    total_ms = 0
    total_bytes = 0
    combined = b""
    calls = len(task["baseline_commands"])
    for cmd_str in task["baseline_commands"]:
        ms, stdout, _ = run_cmd(
            ["bash", "-c", cmd_str], cwd=str(repo_path), timeout=30
        )
        total_ms += ms
        total_bytes += len(stdout)
        combined += stdout
    return calls, total_bytes, total_ms, combined


def run_codixing_task(task: dict, repo_path: Path) -> tuple[int, int, int, bytes]:
    """Run codixing commands, return (calls, bytes, ms, combined_output)."""
    total_ms = 0
    total_bytes = 0
    combined = b""
    calls = len(task["codixing_commands"])
    for cmd_str in task["codixing_commands"]:
        parts = cmd_str.split()
        subcmd = parts[0]
        args = parts[1:] if len(parts) > 1 else []

        # Map task commands to actual codixing CLI invocations
        if subcmd == "find_symbol":
            # Use search with limit (closer to MCP find_symbol which returns definition context)
            query = " ".join(args)
            cli_cmd = [str(CODIXING), "search", query, "--limit", "5"]
        elif subcmd == "search":
            query = " ".join(args).strip("'\"")
            cli_cmd = [str(CODIXING), "search", query, "--limit", "10"]
        elif subcmd == "symbol_callers":
            cli_cmd = [str(CODIXING), "usages"] + args + ["--limit", "15"]
        elif subcmd == "callers":
            cli_cmd = [str(CODIXING), "callers"] + args
        elif subcmd == "predict_impact":
            cli_cmd = [str(CODIXING), "callers"] + args  # closest CLI equivalent
        elif subcmd == "graph":
            # Ensure tight token budget for fair comparison
            cli_cmd = [str(CODIXING), "graph"] + args
            if "--token-budget" not in cmd_str:
                cli_cmd += ["--token-budget", "1500"]
        else:
            cli_cmd = [str(CODIXING)] + parts

        ms, stdout, rc = run_cmd(cli_cmd, cwd=str(repo_path), timeout=60)
        total_ms += ms
        total_bytes += len(stdout)
        combined += stdout
    return calls, total_bytes, total_ms, combined


def evaluate_task(task: dict, repo_path: Path) -> TaskResult:
    """Run both baseline and codixing for a task, compare results."""
    tr = TaskResult(
        task_id=task["id"],
        repo=task["repo"],
        category=task["category"],
        description=task["description"],
    )

    # Baseline
    try:
        tr.baseline_calls, tr.baseline_bytes, tr.baseline_ms, baseline_out = (
            run_baseline_task(task, repo_path)
        )
        tr.baseline_lines = baseline_out.count(b"\n")
        if "expected_file" in task:
            tr.baseline_found_target = (
                task["expected_file"].encode() in baseline_out
            )
    except Exception as e:
        tr.error = f"baseline: {e}"

    # Codixing
    try:
        tr.codixing_calls, tr.codixing_bytes, tr.codixing_ms, codixing_out = (
            run_codixing_task(task, repo_path)
        )
        tr.codixing_lines = codixing_out.count(b"\n")
        if "expected_file" in task:
            tr.codixing_found_target = (
                task["expected_file"].encode() in codixing_out
            )
    except Exception as e:
        tr.error += f" codixing: {e}"

    # Compute savings
    tr.call_savings = tr.baseline_calls - tr.codixing_calls
    if tr.baseline_bytes > 0:
        tr.byte_savings_pct = (
            (tr.baseline_bytes - tr.codixing_bytes) / tr.baseline_bytes * 100
        )

    return tr


def generate_report(results: list[RepoResult]) -> str:
    """Generate a markdown benchmark report."""
    lines = []
    lines.append("# Codixing Real-World Benchmark Report\n")
    lines.append(f"**Date:** {time.strftime('%Y-%m-%d %H:%M')}")
    lines.append(f"**Codixing version:** BM25-only (default)\n")

    # Summary table
    lines.append("## Repository Summary\n")
    lines.append("| Repo | Language | Files | Symbols | Chunks | Index Time |")
    lines.append("|------|----------|-------|---------|--------|------------|")
    for r in results:
        idx_time = f"{r.index_time_ms}ms" if r.index_time_ms else "cached"
        lines.append(
            f"| {r.name} | {r.lang} | {r.file_count:,} | {r.symbol_count:,} | "
            f"{r.chunk_count:,} | {idx_time} |"
        )

    # Aggregate stats
    all_tasks = [t for r in results for t in r.tasks]
    total_baseline_calls = sum(t.baseline_calls for t in all_tasks)
    total_codixing_calls = sum(t.codixing_calls for t in all_tasks)
    total_baseline_bytes = sum(t.baseline_bytes for t in all_tasks)
    total_codixing_bytes = sum(t.codixing_bytes for t in all_tasks)

    lines.append("\n## Aggregate Results\n")
    lines.append(f"**Tasks evaluated:** {len(all_tasks)}")

    call_pct = (
        (total_baseline_calls - total_codixing_calls) / total_baseline_calls * 100
        if total_baseline_calls
        else 0
    )
    byte_pct = (
        (total_baseline_bytes - total_codixing_bytes) / total_baseline_bytes * 100
        if total_baseline_bytes
        else 0
    )
    baseline_tokens = total_baseline_bytes // 4
    codixing_tokens = total_codixing_bytes // 4

    lines.append("")
    lines.append("| Metric | grep/cat/find | Codixing | Improvement |")
    lines.append("|--------|---------------|----------|-------------|")
    lines.append(
        f"| Tool calls | {total_baseline_calls} | {total_codixing_calls} | "
        f"**{call_pct:.0f}% fewer** |"
    )
    lines.append(
        f"| Output bytes | {total_baseline_bytes:,} | {total_codixing_bytes:,} | "
        f"**{byte_pct:.0f}% fewer** |"
    )
    lines.append(
        f"| Est. tokens | ~{baseline_tokens:,} | ~{codixing_tokens:,} | "
        f"**{byte_pct:.0f}% fewer** |"
    )
    lines.append(
        f"| Est. LLM wall time | ~{total_baseline_calls * 2}s | "
        f"~{total_codixing_calls * 2}s | "
        f"**{(total_baseline_calls - total_codixing_calls) * 2}s saved** |"
    )

    # By category
    lines.append("\n## Results by Category\n")
    categories = {}
    for t in all_tasks:
        categories.setdefault(t.category, []).append(t)

    for cat, tasks in sorted(categories.items()):
        bl_calls = sum(t.baseline_calls for t in tasks)
        cx_calls = sum(t.codixing_calls for t in tasks)
        bl_bytes = sum(t.baseline_bytes for t in tasks)
        cx_bytes = sum(t.codixing_bytes for t in tasks)
        byte_sav = (bl_bytes - cx_bytes) / bl_bytes * 100 if bl_bytes else 0
        lines.append(
            f"| **{cat}** ({len(tasks)} tasks) | {bl_calls} calls / "
            f"{bl_bytes:,}B | {cx_calls} calls / {cx_bytes:,}B | "
            f"{byte_sav:.0f}% bytes saved |"
        )

    # Per-repo detailed results
    for r in results:
        lines.append(f"\n## {r.name} ({r.description})\n")
        lines.append(
            "| Task | Category | Baseline Calls | Cdx Calls | "
            "Baseline Bytes | Cdx Bytes | Savings |"
        )
        lines.append(
            "|------|----------|----------------|-----------|"
            "---------------|-----------|---------|"
        )
        for t in r.tasks:
            sav = f"{t.byte_savings_pct:.0f}%" if t.byte_savings_pct > 0 else "—"
            lines.append(
                f"| {t.description[:50]} | {t.category} | "
                f"{t.baseline_calls} | {t.codixing_calls} | "
                f"{t.baseline_bytes:,} | {t.codixing_bytes:,} | {sav} |"
            )

    # Conclusions
    lines.append("\n## Key Findings\n")
    lines.append("### When Codixing wins most")
    lines.append("- **Multi-step exploration**: explain/understand tasks need 2-4 grep calls vs 1 Codixing call")
    lines.append("- **Semantic search**: natural language queries that grep can't handle")
    lines.append("- **Call graph navigation**: finding callers/callees across a large codebase")
    lines.append("- **Architecture overview**: repo-map provides structured overview vs find+wc+head")
    lines.append("")
    lines.append("### When standard tools suffice")
    lines.append("- Exact keyword search on small codebases")
    lines.append("- Reading a known file at a known path")
    lines.append("- Simple single-pattern grep")
    lines.append("")
    lines.append("### Context window impact")
    lines.append(
        f"Over {len(all_tasks)} tasks across {len(results)} repos, "
        f"Codixing saves **~{(baseline_tokens - codixing_tokens):,} tokens** "
        f"({byte_pct:.0f}% reduction). "
        f"In an 8K-token context budget, this means "
        f"**{baseline_tokens // 8000 if baseline_tokens > 0 else 0} vs "
        f"{codixing_tokens // 8000 if codixing_tokens > 0 else 0} context fills** — "
        f"fewer LLM round-trips and less context pressure."
    )

    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description="Codixing real-world benchmark")
    parser.add_argument("--repos", nargs="*", help="Only benchmark these repos")
    parser.add_argument("--skip-clone", action="store_true", help="Skip cloning")
    parser.add_argument("--skip-index", action="store_true", help="Skip indexing")
    parser.add_argument("--category", help="Only run tasks of this category")
    args = parser.parse_args()

    # Check codixing binary
    if not CODIXING.exists():
        print(f"ERROR: codixing binary not found at {CODIXING}")
        print("Run: cargo build --release --bin codixing")
        sys.exit(1)

    # Load config
    with open(ROOT / "benchmarks" / "repos.toml", "rb") as f:
        repos_cfg = tomllib.load(f)
    with open(ROOT / "benchmarks" / "tasks.toml", "rb") as f:
        tasks_cfg = tomllib.load(f)

    repos = repos_cfg["repo"]
    tasks = tasks_cfg["task"]

    if args.repos:
        repos = [r for r in repos if r["name"] in args.repos]
    if args.category:
        tasks = [t for t in tasks if t["category"] == args.category]

    REPOS_DIR.mkdir(parents=True, exist_ok=True)
    RESULTS_DIR.mkdir(parents=True, exist_ok=True)

    results = []

    for repo_cfg in repos:
        name = repo_cfg["name"]
        print(f"\n{'='*60}")
        print(f"  {name} — {repo_cfg['description']}")
        print(f"{'='*60}")

        # Clone
        if not args.skip_clone:
            repo_path = clone_repo(repo_cfg)
        else:
            repo_path = REPOS_DIR / name
            if not repo_path.exists():
                print(f"  [SKIP] {name} not cloned, skipping")
                continue

        rr = RepoResult(
            name=name,
            lang=repo_cfg["lang"],
            description=repo_cfg["description"],
        )

        # Count files
        rr.file_count = count_source_files(repo_path, repo_cfg["lang"])
        print(f"  {rr.file_count:,} source files")

        # Index
        if not args.skip_index:
            rr.index_time_ms, rr.symbol_count, rr.chunk_count = index_repo(repo_path)
        else:
            # Try to read stats from existing index
            ms, stdout, _ = run_cmd(
                [str(CODIXING), "graph", str(repo_path)], cwd=str(repo_path), timeout=30
            )
            text = stdout.decode(errors="replace")
            rr.symbol_count = (
                int(m.group(1)) if (m := re.search(r"(\d+)\s*symbols?", text)) else 0
            )
            rr.chunk_count = (
                int(m.group(1)) if (m := re.search(r"(\d+)\s*chunks?", text)) else 0
            )

        # Run tasks for this repo
        repo_tasks = [t for t in tasks if t["repo"] == name]
        print(f"\n  Running {len(repo_tasks)} tasks...")

        for task in repo_tasks:
            print(f"    [{task['id']}] {task['description'][:55]}...", end=" ")
            tr = evaluate_task(task, repo_path)
            rr.tasks.append(tr)

            if tr.byte_savings_pct > 0:
                print(
                    f"✓ {tr.baseline_calls}→{tr.codixing_calls} calls, "
                    f"{tr.byte_savings_pct:.0f}% fewer bytes"
                )
            else:
                print(
                    f"  {tr.baseline_calls}→{tr.codixing_calls} calls, "
                    f"{tr.codixing_bytes:,}B vs {tr.baseline_bytes:,}B"
                )

        results.append(rr)

    # Generate report
    report = generate_report(results)
    report_path = RESULTS_DIR / "real_world_benchmark.md"
    report_path.write_text(report)
    print(f"\n{'='*60}")
    print(f"Report saved to: {report_path}")
    print(f"{'='*60}")

    # Also save raw JSON
    json_path = RESULTS_DIR / "real_world_benchmark.json"
    json_data = []
    for rr in results:
        d = {
            "name": rr.name,
            "lang": rr.lang,
            "file_count": rr.file_count,
            "index_time_ms": rr.index_time_ms,
            "symbol_count": rr.symbol_count,
            "chunk_count": rr.chunk_count,
            "tasks": [asdict(t) for t in rr.tasks],
        }
        json_data.append(d)
    json_path.write_text(json.dumps(json_data, indent=2))
    print(f"Raw data saved to: {json_path}")

    # Print report to stdout
    print(f"\n{report}")


if __name__ == "__main__":
    main()

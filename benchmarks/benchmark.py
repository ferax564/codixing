#!/usr/bin/env python3
"""
benchmark.py — Unified benchmark runner for Codixing.

Runs CLI benchmarks (existing run_benchmark.py) and/or agent benchmarks
(agent_benchmark.py) with optional multi-run statistical reporting.

Usage:
    python3 benchmark.py --mode all --runs 5
    python3 benchmark.py --mode agent --repos codixing --runs 1
    python3 benchmark.py --mode cli --repos tokio,axum
"""

import argparse
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PYTHON = sys.executable


def run_cli_benchmark(repos: str | None, runs: int, category: str | None):
    """Run the CLI benchmark (run_benchmark.py)."""
    cmd = [PYTHON, str(ROOT / "benchmarks" / "run_benchmark.py")]
    if repos:
        cmd += ["--repos"] + repos.split(",")
    if category:
        cmd += ["--category", category]
    print(f"\n{'='*60}")
    print("  CLI Benchmark")
    print(f"{'='*60}")
    subprocess.run(cmd, cwd=str(ROOT))


def run_agent_benchmark(
    repos: str | None, runs: int, tasks: str | None, model: str
):
    """Run the agent benchmark (agent_benchmark.py)."""
    cmd = [PYTHON, str(ROOT / "benchmarks" / "agent_benchmark.py")]
    if repos:
        cmd += ["--repos", repos]
    cmd += ["--runs", str(runs)]
    if tasks:
        cmd += ["--tasks", tasks]
    cmd += ["--model", model]
    print(f"\n{'='*60}")
    print("  Agent Benchmark")
    print(f"{'='*60}")
    subprocess.run(cmd, cwd=str(ROOT))


def main():
    parser = argparse.ArgumentParser(
        description="Codixing unified benchmark runner"
    )
    parser.add_argument(
        "--mode",
        choices=["cli", "agent", "all"],
        default="all",
        help="Which benchmarks to run",
    )
    parser.add_argument("--repos", help="Comma-separated repo names")
    parser.add_argument(
        "--runs",
        type=int,
        default=1,
        help="Runs per task per condition (agent benchmark)",
    )
    parser.add_argument(
        "--tasks", help="Comma-separated task IDs (agent benchmark)"
    )
    parser.add_argument(
        "--model",
        default="claude-sonnet-4-6",
        help="Model ID (agent benchmark)",
    )
    parser.add_argument(
        "--category", help="Task category filter (CLI benchmark)"
    )
    args = parser.parse_args()

    if args.mode in ("cli", "all"):
        run_cli_benchmark(args.repos, args.runs, args.category)

    if args.mode in ("agent", "all"):
        run_agent_benchmark(args.repos, args.runs, args.tasks, args.model)

    print("\nBenchmark complete. Results in benchmarks/results/")


if __name__ == "__main__":
    main()

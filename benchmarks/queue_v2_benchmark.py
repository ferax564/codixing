#!/usr/bin/env python3
"""Queue Embedding v2 — Benchmark: grep vs codixing (sync) vs codixing (queue).

Usage:
    python3 benchmarks/queue_v2_benchmark.py [--repo openclaw|linux|both] [--skip-accuracy]

Outputs:
    benchmarks/results/queue_v2_benchmark.json
    benchmarks/results/queue_v2_benchmark.md
"""

import json
import os
import subprocess
import sys
import time
import tomllib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
CODIXING = REPO_ROOT / "target" / "release" / "codixing"
RESULTS_DIR = REPO_ROOT / "benchmarks" / "results"

REPOS = {
    "openclaw": REPO_ROOT / "benchmarks" / "repos" / "openclaw",
    "linux": Path.home() / "code" / "linux",
}


def run(cmd: list[str], cwd: str | None = None, timeout: int = 600) -> tuple[str, float]:
    """Run command, return (stdout, elapsed_seconds)."""
    start = time.monotonic()
    result = subprocess.run(
        cmd, capture_output=True, text=True, cwd=cwd, timeout=timeout
    )
    elapsed = time.monotonic() - start
    if result.returncode != 0:
        print(f"  WARN: {' '.join(cmd[:3])}... exited {result.returncode}", file=sys.stderr)
        if result.stderr:
            print(f"  stderr: {result.stderr[:200]}", file=sys.stderr)
    return result.stdout, elapsed


def build_codixing():
    """Build codixing release binary."""
    print("Building codixing (release)...")
    subprocess.run(
        ["cargo", "build", "--release", "-p", "codixing"],
        cwd=REPO_ROOT, check=True, capture_output=True,
    )


# ── Axis 1: Search Accuracy ────────────────────────────────────────────────

def load_queries() -> list[dict]:
    """Load benchmark queries from TOML."""
    path = REPO_ROOT / "benchmarks" / "queue_v2_queries.toml"
    with open(path, "rb") as f:
        data = tomllib.load(f)
    return data.get("query", [])


def grep_search(repo_path: Path, pattern: str, top_k: int = 10) -> list[str]:
    """Run grep and return top files ranked by match count."""
    out, _ = run(
        ["grep", "-rn", "--include=*.ts", "--include=*.tsx",
         "--include=*.js", "--include=*.jsx", pattern, "."],
        cwd=str(repo_path),
    )
    counts: dict[str, int] = {}
    for line in out.splitlines():
        parts = line.split(":", 2)
        if len(parts) >= 2:
            fp = parts[0].lstrip("./")
            counts[fp] = counts.get(fp, 0) + 1
    ranked = sorted(counts.keys(), key=lambda f: counts[f], reverse=True)
    return ranked[:top_k]


def codixing_search(repo_path: Path, query: str, strategy: str, top_k: int = 10) -> list[str]:
    """Run codixing search and return DEDUPLICATED file paths (rank-ordered).

    Codixing returns chunks, not files. We request extra results (3x) and
    deduplicate to unique file paths, preserving the rank of first appearance.
    """
    out, _ = run(
        [str(CODIXING), "search", query, "--strategy", strategy,
         "--limit", str(top_k * 3), "--json"],
        cwd=str(repo_path),
    )
    try:
        results = json.loads(out)
        if not isinstance(results, list):
            return []
        # Deduplicate: keep first occurrence of each file path.
        seen: set[str] = set()
        files: list[str] = []
        for r in results:
            fp = r.get("file_path", r.get("file", ""))
            if fp and fp not in seen:
                seen.add(fp)
                files.append(fp)
                if len(files) >= top_k:
                    break
        return files
    except (json.JSONDecodeError, TypeError):
        return []


def score_results(returned: list[str], ground_truth: list[str]) -> dict:
    """Compute file-level precision@10, recall@10, MRR.

    Both `returned` and `ground_truth` are file paths. Matching uses suffix
    comparison to handle relative vs absolute paths.
    """
    gt_set = set(ground_truth)

    def is_match(returned_file: str) -> bool:
        for gt in gt_set:
            if returned_file == gt or returned_file.endswith(gt) or gt.endswith(returned_file):
                return True
        return False

    top = returned[:10]
    hits = [1 if is_match(f) else 0 for f in top]
    precision = sum(hits) / len(top) if top else 0.0
    # Recall: count DISTINCT ground truth files found (not total hits).
    found_gt: set[str] = set()
    for f in top:
        for gt in gt_set:
            if f == gt or f.endswith(gt) or gt.endswith(f):
                found_gt.add(gt)
    recall = len(found_gt) / len(gt_set) if gt_set else 0.0
    mrr = 0.0
    for i, h in enumerate(hits):
        if h:
            mrr = 1.0 / (i + 1)
            break
    return {"precision_at_10": round(precision, 3), "recall_at_10": round(recall, 3), "mrr": round(mrr, 3)}


def run_accuracy_benchmark(repo_path: Path) -> dict:
    """Run search accuracy benchmark on OpenClaw."""
    queries = load_queries()
    if not queries:
        print("  No queries found in queue_v2_queries.toml", file=sys.stderr)
        return {"queries": {}, "summary": {}}

    results = {"grep": [], "bm25": [], "hybrid": []}

    for q in queries:
        gt = q.get("ground_truth", [])
        if not gt:
            continue
        name = q.get("name", "unknown")
        print(f"  Query: {name}...")

        grep_files = grep_search(repo_path, q["grep_pattern"])
        bm25_files = codixing_search(repo_path, q["text"], "instant")
        hybrid_files = codixing_search(repo_path, q["text"], "fast")

        results["grep"].append({"query": name, **score_results(grep_files, gt)})
        results["bm25"].append({"query": name, **score_results(bm25_files, gt)})
        results["hybrid"].append({"query": name, **score_results(hybrid_files, gt)})

    summary = {}
    for method in results:
        if results[method]:
            n = len(results[method])
            summary[method] = {
                "avg_precision": round(sum(r["precision_at_10"] for r in results[method]) / n, 3),
                "avg_recall": round(sum(r["recall_at_10"] for r in results[method]) / n, 3),
                "avg_mrr": round(sum(r["mrr"] for r in results[method]) / n, 3),
            }
    return {"queries": results, "summary": summary}


# ── Axis 2: Indexing Speed ──────────────────────────────────────────────────

def clean_index(repo_path: Path):
    """Remove .codixing index directory."""
    index_dir = repo_path / ".codixing"
    if index_dir.exists():
        subprocess.run(["rm", "-rf", str(index_dir)], check=True)


def run_indexing_benchmark(repo_path: Path, repo_name: str) -> dict:
    """Measure init time for sync path (BM25 only, no embeddings)."""
    clean_index(repo_path)

    print(f"  Indexing {repo_name} (BM25 only)...")
    _, init_time = run(
        [str(CODIXING), "init", ".", "--no-embeddings"],
        cwd=str(repo_path), timeout=300,
    )

    return {
        "repo": repo_name,
        "bm25_init_seconds": round(init_time, 2),
    }


# ── Axis 3: Time to First Search ────────────────────────────────────────────

def run_ttfs_benchmark(repo_path: Path, repo_name: str) -> dict:
    """Measure time from init start to first successful search."""
    # Standard init (blocks until done)
    clean_index(repo_path)
    print(f"  TTFS {repo_name} (standard init)...")
    start = time.monotonic()
    run([str(CODIXING), "init", ".", "--no-embeddings"], cwd=str(repo_path), timeout=300)
    run([str(CODIXING), "search", "function", "--limit", "1"], cwd=str(repo_path))
    standard_ttfs = time.monotonic() - start

    # Deferred init (--defer-embeddings)
    clean_index(repo_path)
    print(f"  TTFS {repo_name} (deferred embeddings)...")
    start = time.monotonic()
    run([str(CODIXING), "init", ".", "--defer-embeddings"], cwd=str(repo_path), timeout=300)
    run([str(CODIXING), "search", "function", "--limit", "1"], cwd=str(repo_path))
    deferred_ttfs = time.monotonic() - start

    return {
        "repo": repo_name,
        "standard_ttfs_seconds": round(standard_ttfs, 2),
        "deferred_ttfs_seconds": round(deferred_ttfs, 2),
        "speedup": round(standard_ttfs / deferred_ttfs, 1) if deferred_ttfs > 0 else 0,
    }


# ── Report ──────────────────────────────────────────────────────────────────

def generate_report(data: dict) -> str:
    """Generate markdown report."""
    lines = ["# Queue Embedding v2 — Benchmark Results\n"]
    lines.append(f"**Date:** {time.strftime('%Y-%m-%d %H:%M')}\n")

    if "accuracy" in data and data["accuracy"].get("summary"):
        lines.append("## Search Accuracy (OpenClaw)\n")
        lines.append("| Method | Precision@10 | Recall@10 | MRR |")
        lines.append("|--------|-------------|----------|-----|")
        for method, scores in data["accuracy"]["summary"].items():
            lines.append(
                f"| {method} | {scores['avg_precision']:.3f} | "
                f"{scores['avg_recall']:.3f} | {scores['avg_mrr']:.3f} |"
            )
        lines.append("")

        # Per-query breakdown
        lines.append("### Per-Query Breakdown\n")
        for method in ["grep", "bm25", "hybrid"]:
            lines.append(f"**{method}:**\n")
            lines.append("| Query | P@10 | R@10 | MRR |")
            lines.append("|-------|------|------|-----|")
            for r in data["accuracy"]["queries"].get(method, []):
                lines.append(
                    f"| {r['query']} | {r['precision_at_10']:.3f} | "
                    f"{r['recall_at_10']:.3f} | {r['mrr']:.3f} |"
                )
            lines.append("")

    if "indexing" in data and data["indexing"]:
        lines.append("## Indexing Speed\n")
        lines.append("| Repo | BM25 Init (s) |")
        lines.append("|------|--------------|")
        for r in data["indexing"]:
            lines.append(f"| {r['repo']} | {r['bm25_init_seconds']} |")
        lines.append("")

    if "ttfs" in data and data["ttfs"]:
        lines.append("## Time to First Search\n")
        lines.append("| Repo | Standard (s) | Deferred (s) | Speedup |")
        lines.append("|------|-------------|--------------|---------|")
        for r in data["ttfs"]:
            lines.append(
                f"| {r['repo']} | {r['standard_ttfs_seconds']} | "
                f"{r['deferred_ttfs_seconds']} | {r['speedup']}x |"
            )
        lines.append("")

    return "\n".join(lines)


def main():
    import argparse
    parser = argparse.ArgumentParser(description="Queue v2 benchmark")
    parser.add_argument("--repo", choices=["openclaw", "linux", "both"], default="both")
    parser.add_argument("--skip-accuracy", action="store_true",
                        help="Skip search accuracy benchmark (useful for linux)")
    parser.add_argument("--skip-build", action="store_true")
    args = parser.parse_args()

    if not args.skip_build:
        build_codixing()

    repos = []
    if args.repo in ("openclaw", "both"):
        repos.append("openclaw")
    if args.repo in ("linux", "both"):
        repos.append("linux")

    # Check repos exist
    for repo_name in repos:
        if not REPOS[repo_name].exists():
            print(f"WARN: {repo_name} repo not found at {REPOS[repo_name]}", file=sys.stderr)
            repos.remove(repo_name)

    if not repos:
        print("No repos available. Exiting.", file=sys.stderr)
        sys.exit(1)

    data: dict = {}

    # Axis 1: Accuracy (OpenClaw only)
    if not args.skip_accuracy and "openclaw" in repos:
        print("\n=== Axis 1: Search Accuracy (OpenClaw) ===")
        data["accuracy"] = run_accuracy_benchmark(REPOS["openclaw"])

    # Axis 2: Indexing Speed
    print("\n=== Axis 2: Indexing Speed ===")
    data["indexing"] = []
    for repo_name in repos:
        data["indexing"].append(run_indexing_benchmark(REPOS[repo_name], repo_name))

    # Axis 3: Time to First Search
    print("\n=== Axis 3: Time to First Search ===")
    data["ttfs"] = []
    for repo_name in repos:
        data["ttfs"].append(run_ttfs_benchmark(REPOS[repo_name], repo_name))

    # Save results
    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    with open(RESULTS_DIR / "queue_v2_benchmark.json", "w") as f:
        json.dump(data, f, indent=2)

    report = generate_report(data)
    with open(RESULTS_DIR / "queue_v2_benchmark.md", "w") as f:
        f.write(report)

    print(f"\n{report}")
    print(f"\nResults saved to {RESULTS_DIR}/queue_v2_benchmark.*")


if __name__ == "__main__":
    main()

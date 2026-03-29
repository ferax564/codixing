#!/usr/bin/env python3
"""Queue Embedding v2 — Benchmark: grep vs codixing (strategy-aware).

Usage:
    python3 benchmarks/queue_v2_benchmark.py [--repo openclaw|linux|both] [--skip-accuracy]

Outputs:
    benchmarks/results/queue_v2_benchmark.json
    benchmarks/results/queue_v2_benchmark.md
"""

import json
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


def grep_search(repo_path: Path, pattern: str, top_k: int = 10) -> tuple[list[str], float]:
    """Run grep and return (top files ranked by match count, elapsed_ms)."""
    out, elapsed = run(
        ["grep", "-rn", "--include=*.ts", "--include=*.tsx",
         "--include=*.js", "--include=*.jsx", pattern, "."],
        cwd=str(repo_path),
    )
    elapsed_ms = round(elapsed * 1000, 1)
    counts: dict[str, int] = {}
    for line in out.splitlines():
        parts = line.split(":", 2)
        if len(parts) >= 2:
            fp = parts[0].lstrip("./")
            counts[fp] = counts.get(fp, 0) + 1
    ranked = sorted(counts.keys(), key=lambda f: counts[f], reverse=True)
    return ranked[:top_k], elapsed_ms


def codixing_search(repo_path: Path, query: str, strategy: str, top_k: int = 10) -> tuple[list[str], float]:
    """Run codixing search and return (DEDUPLICATED file paths, elapsed_ms).

    Codixing returns chunks, not files. We request extra results (3x) and
    deduplicate to unique file paths, preserving the rank of first appearance.
    """
    out, elapsed = run(
        [str(CODIXING), "search", query, "--strategy", strategy,
         "--limit", str(top_k * 3), "--json"],
        cwd=str(repo_path),
    )
    elapsed_ms = round(elapsed * 1000, 1)
    try:
        results = json.loads(out)
        if not isinstance(results, list):
            return [], elapsed_ms
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
        return files, elapsed_ms
    except (json.JSONDecodeError, TypeError):
        return [], elapsed_ms


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


# Strategy selection based on query category.
# - symbol/usage: exact (trigram grep, like grep but faster)
# - concept/cross-package: fast (BM25 + vectors, semantic search)
CATEGORY_STRATEGY = {
    "symbol": "exact",
    "usage": "exact",
    "concept": "fast",
    "cross-package": "fast",
}


def run_accuracy_benchmark(repo_path: Path) -> dict:
    """Run search accuracy benchmark on OpenClaw."""
    queries = load_queries()
    if not queries:
        print("  No queries found in queue_v2_queries.toml", file=sys.stderr)
        return {"queries": {}, "summary": {}, "category_summary": {}, "timing": {}}

    results = {"grep": [], "codixing": []}
    # Per-query timing: list of {query, category, grep_ms, codixing_ms, codixing_strategy}
    timing_records: list[dict] = []

    for q in queries:
        gt = q.get("ground_truth", [])
        if not gt:
            continue
        name = q.get("name", "unknown")
        category = q.get("category", "unknown")
        strategy = CATEGORY_STRATEGY.get(category, "fast")
        print(f"  Query: {name} (category={category}, strategy={strategy})...")

        grep_files, grep_ms = grep_search(repo_path, q["grep_pattern"])
        codixing_files, codixing_ms = codixing_search(repo_path, q["text"], strategy)

        results["grep"].append({"query": name, "category": category, **score_results(grep_files, gt)})
        results["codixing"].append({"query": name, "category": category, "strategy": strategy, **score_results(codixing_files, gt)})

        timing_records.append({
            "query": name,
            "category": category,
            "grep_ms": grep_ms,
            "codixing_ms": codixing_ms,
            "codixing_strategy": strategy,
        })

    # Overall summary
    summary = {}
    for method in results:
        if results[method]:
            n = len(results[method])
            summary[method] = {
                "avg_precision": round(sum(r["precision_at_10"] for r in results[method]) / n, 3),
                "avg_recall": round(sum(r["recall_at_10"] for r in results[method]) / n, 3),
                "avg_mrr": round(sum(r["mrr"] for r in results[method]) / n, 3),
            }

    # Per-category summary
    categories = sorted(set(q.get("category", "unknown") for q in queries if q.get("ground_truth")))
    category_summary: dict[str, dict] = {}
    for cat in categories:
        cat_grep = [r for r in results["grep"] if r["category"] == cat]
        cat_codixing = [r for r in results["codixing"] if r["category"] == cat]
        if cat_grep and cat_codixing:
            n = len(cat_grep)
            grep_p = round(sum(r["precision_at_10"] for r in cat_grep) / n, 3)
            grep_r = round(sum(r["recall_at_10"] for r in cat_grep) / n, 3)
            codixing_p = round(sum(r["precision_at_10"] for r in cat_codixing) / n, 3)
            codixing_r = round(sum(r["recall_at_10"] for r in cat_codixing) / n, 3)
            # Determine winner by recall, then precision
            if grep_r > codixing_r or (grep_r == codixing_r and grep_p > codixing_p):
                best = "grep"
            elif codixing_r > grep_r or (codixing_r == grep_r and codixing_p > grep_p):
                best = "codixing"
            else:
                best = "tie"
            strategy = CATEGORY_STRATEGY.get(cat, "fast")
            category_summary[cat] = {
                "grep_precision": grep_p, "grep_recall": grep_r,
                "codixing_precision": codixing_p, "codixing_recall": codixing_r,
                "codixing_strategy": strategy, "best": best,
            }

    # Timing summary by strategy
    timing_summary: dict[str, dict] = {}
    # grep timing
    grep_times = [t["grep_ms"] for t in timing_records]
    if grep_times:
        timing_summary["grep"] = {
            "avg_ms": round(sum(grep_times) / len(grep_times), 1),
            "min_ms": round(min(grep_times), 1),
            "max_ms": round(max(grep_times), 1),
        }
    # codixing timing grouped by strategy
    for strat in sorted(set(t["codixing_strategy"] for t in timing_records)):
        strat_times = [t["codixing_ms"] for t in timing_records if t["codixing_strategy"] == strat]
        if strat_times:
            timing_summary[f"codixing {strat}"] = {
                "avg_ms": round(sum(strat_times) / len(strat_times), 1),
                "min_ms": round(min(strat_times), 1),
                "max_ms": round(max(strat_times), 1),
            }

    return {
        "queries": results,
        "summary": summary,
        "category_summary": category_summary,
        "timing": {"per_query": timing_records, "summary": timing_summary},
    }


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

    accuracy = data.get("accuracy", {})

    if accuracy.get("summary"):
        lines.append("## Search Accuracy (OpenClaw)\n")
        lines.append("| Method | Precision@10 | Recall@10 | MRR |")
        lines.append("|--------|-------------|----------|-----|")
        for method, scores in accuracy["summary"].items():
            lines.append(
                f"| {method} | {scores['avg_precision']:.3f} | "
                f"{scores['avg_recall']:.3f} | {scores['avg_mrr']:.3f} |"
            )
        lines.append("")

        # Per-category breakdown
        if accuracy.get("category_summary"):
            lines.append("### By Category\n")
            lines.append("| Category | Strategy | grep P@10 | grep R@10 | codixing P@10 | codixing R@10 | Best |")
            lines.append("|----------|----------|----------|----------|--------------|--------------|------|")
            for cat, cs in accuracy["category_summary"].items():
                lines.append(
                    f"| {cat} | {cs['codixing_strategy']} | {cs['grep_precision']:.3f} | "
                    f"{cs['grep_recall']:.3f} | {cs['codixing_precision']:.3f} | "
                    f"{cs['codixing_recall']:.3f} | {cs['best']} |"
                )
            lines.append("")

        # Per-query breakdown
        lines.append("### Per-Query Breakdown\n")
        for method in ["grep", "codixing"]:
            lines.append(f"**{method}:**\n")
            header_extra = " Strategy |" if method == "codixing" else ""
            lines.append(f"| Query | Category |{header_extra} P@10 | R@10 | MRR |")
            sep_extra = "----------|" if method == "codixing" else ""
            lines.append(f"|-------|----------|{sep_extra}------|------|-----|")
            for r in accuracy["queries"].get(method, []):
                strat_col = f" {r.get('strategy', '')} |" if method == "codixing" else ""
                lines.append(
                    f"| {r['query']} | {r.get('category', '')} |{strat_col} "
                    f"{r['precision_at_10']:.3f} | {r['recall_at_10']:.3f} | {r['mrr']:.3f} |"
                )
            lines.append("")

    # Search speed section
    timing = accuracy.get("timing", {})
    if timing.get("summary"):
        lines.append("## Search Speed\n")
        lines.append("| Method | Avg query time (ms) | Min | Max |")
        lines.append("|--------|-------------------|-----|-----|")
        for method, ts in timing["summary"].items():
            lines.append(f"| {method} | {ts['avg_ms']} | {ts['min_ms']} | {ts['max_ms']} |")
        lines.append("")

        # Per-query timing
        if timing.get("per_query"):
            lines.append("### Per-Query Timing\n")
            lines.append("| Query | Category | grep (ms) | codixing (ms) | Strategy |")
            lines.append("|-------|----------|----------|--------------|----------|")
            for t in timing["per_query"]:
                lines.append(
                    f"| {t['query']} | {t['category']} | {t['grep_ms']} | "
                    f"{t['codixing_ms']} | {t['codixing_strategy']} |"
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

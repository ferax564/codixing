#!/usr/bin/env python3
"""Codixing retrieval and durable-embedding workflow benchmark.

Usage:
    python3 benchmarks/queue_v2_benchmark.py [--repo openclaw|linux|both] [--skip-accuracy]

Outputs:
    benchmarks/results/queue_v2_benchmark.json
    benchmarks/results/queue_v2_benchmark.md
"""

import json
import re
import shutil
import subprocess
import sys
import threading
import time
import tomllib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
CODIXING = REPO_ROOT / "target" / "release" / "codixing"
RESULTS_DIR = REPO_ROOT / "benchmarks" / "results"
ACCURACY_MODEL = "bge-small-en"
MAX_STDOUT_BYTES = 8 * 1024 * 1024
MAX_STDERR_BYTES = 2 * 1024 * 1024

REPOS = {
    "openclaw": REPO_ROOT / "benchmarks" / "repos" / "openclaw",
    "linux": Path.home() / "code" / "linux",
}


def run(
    cmd: list[str],
    cwd: str | None = None,
    timeout: int = 600,
    allowed_returncodes: tuple[int, ...] = (0,),
    max_stdout_bytes: int = MAX_STDOUT_BYTES,
    max_stderr_bytes: int = MAX_STDERR_BYTES,
) -> tuple[str, float]:
    """Run a command with bounded output, returning stdout and elapsed time."""
    start = time.monotonic()
    process = subprocess.Popen(
        cmd,
        cwd=cwd,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )

    streams = {
        "stdout": (process.stdout, max_stdout_bytes),
        "stderr": (process.stderr, max_stderr_bytes),
    }
    captured = {"stdout": bytearray(), "stderr": bytearray()}
    exceeded: list[str] = []
    lock = threading.Lock()

    def drain(name: str, stream, limit: int) -> None:
        assert stream is not None
        while chunk := stream.read(64 * 1024):
            with lock:
                remaining = max(0, limit - len(captured[name]))
                captured[name].extend(chunk[:remaining])
                over_limit = len(chunk) > remaining
                if over_limit and name not in exceeded:
                    exceeded.append(name)
            if over_limit:
                try:
                    process.kill()
                except OSError:
                    pass
                break

    readers = [
        threading.Thread(target=drain, args=(name, stream, limit), daemon=True)
        for name, (stream, limit) in streams.items()
    ]
    for reader in readers:
        reader.start()

    try:
        process.wait(timeout=timeout)
    except subprocess.TimeoutExpired as exc:
        process.kill()
        process.wait()
        for reader in readers:
            reader.join()
        raise subprocess.TimeoutExpired(
            cmd,
            timeout,
            output=bytes(captured["stdout"]).decode("utf-8", errors="replace"),
            stderr=bytes(captured["stderr"]).decode("utf-8", errors="replace"),
        ) from exc

    for reader in readers:
        reader.join()

    elapsed = time.monotonic() - start
    stdout = bytes(captured["stdout"]).decode("utf-8", errors="replace")
    stderr = bytes(captured["stderr"]).decode("utf-8", errors="replace")
    if exceeded:
        limits = ", ".join(
            f"{name}={streams[name][1]} bytes" for name in sorted(exceeded)
        )
        raise RuntimeError(f"command output exceeded benchmark limit ({limits}): {cmd!r}")
    if process.returncode not in allowed_returncodes:
        raise subprocess.CalledProcessError(
            process.returncode,
            cmd,
            output=stdout,
            stderr=stderr,
        )
    return stdout, elapsed


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
        [
            "grep",
            "--recursive",
            "--with-filename",
            "--count",
            "--binary-files=without-match",
            "--include=*.ts",
            "--include=*.tsx",
            "--include=*.js",
            "--include=*.jsx",
            "--exclude=*.min.js",
            "--exclude-dir=.codixing",
            "--exclude-dir=.git",
            "--exclude-dir=.next",
            "--exclude-dir=build",
            "--exclude-dir=coverage",
            "--exclude-dir=dist",
            "--exclude-dir=node_modules",
            "--exclude-dir=target",
            "--",
            pattern,
            ".",
        ],
        cwd=str(repo_path),
        allowed_returncodes=(0, 1),
    )
    elapsed_ms = round(elapsed * 1000, 1)
    counts: dict[str, int] = {}
    for line in out.splitlines():
        path_and_count = line.rsplit(":", 1)
        if len(path_and_count) != 2:
            continue
        fp, raw_count = path_and_count
        try:
            match_count = int(raw_count)
        except ValueError:
            continue
        if match_count > 0:
            counts[fp.removeprefix("./")] = match_count
    ranked = sorted(counts, key=lambda f: (-counts[f], f))
    return ranked[:top_k], elapsed_ms


def codixing_symbols(repo_path: Path, symbol: str, top_k: int = 10) -> tuple[list[str], float]:
    """Run codixing symbols and return (file paths of definitions, elapsed_ms).

    Parses the table output format:
        KIND         NAME                FILE              LINES
        -------------------------------------------------------
        TypeAlias    ChannelPlugin       src/foo.ts        L76-L117
        Import       import { ... }      src/bar.ts        L1-L2
        ...

    Prioritises definition kinds (TypeAlias, Interface, Class, Struct, Enum,
    Function) over Import lines, so the file that *defines* a symbol ranks first.
    Among definitions, larger line spans rank higher (canonical definitions tend
    to be larger than local re-aliases).
    """
    out, elapsed = run(
        [str(CODIXING), "symbols", symbol],
        cwd=str(repo_path),
    )
    elapsed_ms = round(elapsed * 1000, 1)

    DEFINITION_KINDS = {"TypeAlias", "Interface", "Class", "Struct", "Enum", "Function"}

    # Collect ALL entries first, then deduplicate with priority to definitions.
    # A file may appear multiple times (e.g., once as Import, once as TypeAlias);
    # we want the definition entry to win.
    file_best: dict[str, tuple[str, int]] = {}  # file -> (kind_group, line_span)

    for line in out.splitlines():
        # Skip header, separator, and empty lines
        stripped = line.strip()
        if not stripped or stripped.startswith("KIND") or stripped.startswith("---"):
            continue

        # Parse: KIND  NAME  FILE  LINES  (whitespace-separated columns)
        # The NAME column may contain spaces (e.g. multi-line import blocks),
        # but the FILE column always ends with a file extension + space + "L"
        # We look for the pattern: <file.ext> L<start>-L<end> or L<single>
        m = re.search(r'(\S+\.\w+)\s+L(\d+)(?:-L?(\d+))?', line)
        if not m:
            continue
        fp = m.group(1)
        line_start = int(m.group(2))
        line_end = int(m.group(3)) if m.group(3) else line_start
        line_span = line_end - line_start + 1

        kind = stripped.split()[0] if stripped else ""
        is_def = kind in DEFINITION_KINDS
        kind_group = "definition" if is_def else "import"

        # Keep the best entry per file: definition > import, then largest span
        if fp not in file_best:
            file_best[fp] = (kind_group, line_span)
        else:
            prev_group, prev_span = file_best[fp]
            if kind_group == "definition" and prev_group != "definition":
                file_best[fp] = (kind_group, line_span)
            elif kind_group == prev_group and line_span > prev_span:
                file_best[fp] = (kind_group, line_span)

    # Split into definitions and imports, sort definitions by span descending
    definition_entries = [(fp, span) for fp, (grp, span) in file_best.items() if grp == "definition"]
    import_entries = [fp for fp, (grp, _) in file_best.items() if grp != "definition"]

    definition_entries.sort(key=lambda x: x[1], reverse=True)
    definition_files = [fp for fp, _ in definition_entries]

    # Definitions first (largest first), then imports as fallback
    files = definition_files + import_entries
    return files[:top_k], elapsed_ms


def codixing_usages(repo_path: Path, symbol: str, top_k: int = 10) -> tuple[list[str], float]:
    """Run codixing usages and return (deduplicated file paths, elapsed_ms).

    Parses the table output format:
        FILE [LINES]    SCORE    PREVIEW
        ----------------------------------------------------------
        src/foo.ts [L0-L38]    51.326    import { ... } from ...
        ...
        N usage location(s) found.
    """
    out, elapsed = run(
        [str(CODIXING), "usages", symbol, "--limit", str(top_k * 3)],
        cwd=str(repo_path),
    )
    elapsed_ms = round(elapsed * 1000, 1)
    seen: set[str] = set()
    files: list[str] = []
    for line in out.splitlines():
        # Skip header, separator, summary, and empty lines
        if not line.strip() or line.startswith("FILE ") or line.startswith("---") or "usage location(s) found" in line:
            continue
        # Extract file path: everything before " [L"
        bracket_idx = line.find(" [L")
        if bracket_idx < 0:
            continue
        fp = line[:bracket_idx].strip()
        if fp and fp not in seen:
            seen.add(fp)
            files.append(fp)
            if len(files) >= top_k:
                break
    return files, elapsed_ms


def codixing_cross_imports(repo_path: Path, from_dir: str, to_dir: str, top_k: int = 10) -> tuple[list[str], float]:
    """Run codixing cross-imports and return (file paths, elapsed_ms).

    Uses the import graph to find files in from_dir that import any file
    in to_dir. This is the correct tool for module-level cross-package queries.
    """
    out, elapsed = run(
        [str(CODIXING), "cross-imports", "--from", from_dir, "--to", to_dir],
        cwd=str(repo_path),
    )
    elapsed_ms = round(elapsed * 1000, 1)
    files: list[str] = []
    for line in out.splitlines():
        stripped = line.strip()
        # Skip summary line and empty lines
        if not stripped or "file(s) in" in stripped or "import from" in stripped:
            continue
        if "/" in stripped or stripped.endswith(".ts") or stripped.endswith(".tsx"):
            # Strip optional score suffix: "file.ts (score: 0.85)" → "file.ts"
            path = stripped.split(" (score:")[0].strip()
            files.append(path)
            if len(files) >= top_k:
                break
    return files, elapsed_ms


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
# - symbol: symbol_lookup (codixing symbols — finds definitions, not just references)
# - usage: usages (dedicated codixing usages subcommand)
# - concept: fast (BM25 + vectors, semantic search)
# - cross-package: cross_imports (codixing cross-imports — graph edge walk)
CATEGORY_STRATEGY = {
    "symbol": "symbol_lookup",
    "usage": "usages",
    "concept": "fast",
    "cross-package": "cross_imports",
}


def run_accuracy_benchmark(repo_path: Path) -> dict:
    """Run search accuracy benchmark on a fresh hybrid OpenClaw index."""
    clean_index(repo_path)
    print(f"  Building deterministic hybrid index ({ACCURACY_MODEL})...")
    run(
        [
            str(CODIXING),
            "init",
            ".",
            "--embed",
            "--model",
            ACCURACY_MODEL,
        ],
        cwd=str(repo_path),
        timeout=1800,
    )

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

        # Route to the right codixing tool based on strategy:
        # - symbol_lookup: codixing symbols (finds definitions, prioritises over imports)
        # - usages: dedicated codixing usages subcommand with symbol name
        # - callers: codixing callers (structural graph query for cross-package imports)
        #   Falls back to usages (if symbol field) or explore (text search)
        # - fast/other: use NL text (semantic search)
        if strategy == "symbol_lookup":
            codixing_files, codixing_ms = codixing_symbols(repo_path, q["grep_pattern"])
        elif strategy == "usages":
            usages_symbol = q.get("symbol", q["grep_pattern"])
            codixing_files, codixing_ms = codixing_usages(repo_path, usages_symbol)
        elif strategy == "cross_imports":
            from_dir = q.get("from_dir", "")
            to_dir = q.get("to_dir", "")
            if from_dir and to_dir:
                codixing_files, codixing_ms = codixing_cross_imports(repo_path, from_dir, to_dir)
            else:
                # Fallback to explore strategy if no from/to dirs
                codixing_files, codixing_ms = codixing_search(repo_path, q["text"], "explore")
        else:
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
        "index": {"type": "hybrid", "embedding_model": ACCURACY_MODEL},
        "queries": results,
        "summary": summary,
        "category_summary": category_summary,
        "timing": {"per_query": timing_records, "summary": timing_summary},
    }


# ── Axis 2: Indexing Speed ──────────────────────────────────────────────────

def clean_index(repo_path: Path):
    """Remove .codixing index directory."""
    index_dir = repo_path / ".codixing"
    if index_dir.is_symlink():
        index_dir.unlink()
    elif index_dir.exists():
        shutil.rmtree(index_dir)


def run_embedding_speed_benchmark(repo_path: Path, repo_name: str) -> dict:
    """Measure the supported durable hybrid-initialization path.

    Older versions of this script varied CODIXING_EMBED_WORKERS, but the CLI
    never consumed that environment variable, so the claimed worker comparison
    measured identical code paths. Record only the real, validated command.
    """
    results: dict = {"repo": repo_name}

    clean_index(repo_path)
    print(f"  Embedding {repo_name} (durable hybrid init)...")
    _, elapsed = run(
        [str(CODIXING), "init", ".", "--embed", "--model", "bge-small-en"],
        cwd=str(repo_path),
        timeout=1800,
    )
    results["hybrid_init_seconds"] = round(elapsed, 2)

    return results


def run_indexing_benchmark(repo_path: Path, repo_name: str) -> dict:
    """Measure init time for sync path (BM25 only, no embeddings)."""
    clean_index(repo_path)

    print(f"  Indexing {repo_name} (BM25 only)...")
    _, init_time = run(
        [str(CODIXING), "init", "."],
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
    run(
        [str(CODIXING), "init", ".", "--embed"],
        cwd=str(repo_path),
        timeout=1800,
    )
    run([str(CODIXING), "search", "function", "--limit", "1"], cwd=str(repo_path))
    standard_ttfs = time.monotonic() - start

    # BM25-first init: measure first lexical search, then continue to durable
    # vector readiness with the documented post-hoc command.
    clean_index(repo_path)
    print(f"  TTFS {repo_name} (BM25 first, vectors added afterward)...")
    start = time.monotonic()
    run(
        [str(CODIXING), "init", ".", "--embed", "--defer-embeddings"],
        cwd=str(repo_path),
        timeout=300,
    )
    run([str(CODIXING), "search", "function", "--limit", "1"], cwd=str(repo_path))
    bm25_first_ttfs = time.monotonic() - start
    run([str(CODIXING), "embed", "."], cwd=str(repo_path), timeout=1800)
    posthoc_vector_ready = time.monotonic() - start

    return {
        "repo": repo_name,
        "hybrid_ready_ttfs_seconds": round(standard_ttfs, 2),
        "bm25_first_ttfs_seconds": round(bm25_first_ttfs, 2),
        "posthoc_vector_ready_seconds": round(posthoc_vector_ready, 2),
        "first_search_speedup": (
            round(standard_ttfs / bm25_first_ttfs, 1)
            if bm25_first_ttfs > 0
            else 0
        ),
    }


# ── Report ──────────────────────────────────────────────────────────────────

def generate_report(data: dict) -> str:
    """Generate markdown report."""
    lines = ["# Codixing Retrieval and Embedding Benchmark Results\n"]
    lines.append(f"**Date:** {time.strftime('%Y-%m-%d %H:%M')}\n")

    accuracy = data.get("accuracy", {})

    if accuracy.get("summary"):
        lines.append("## Search Accuracy (OpenClaw)\n")
        index = accuracy.get("index", {})
        if index:
            lines.append(
                f"**Index:** {index.get('type', 'unknown')} "
                f"({index.get('embedding_model', 'unknown')})\n"
            )
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
        lines.append("| Repo | Hybrid ready (s) | BM25 first search (s) | Post-hoc vectors ready (s) | First-search speedup |")
        lines.append("|------|------------------|-----------------------|----------------------------|----------------------|")
        for r in data["ttfs"]:
            lines.append(
                f"| {r['repo']} | {r['hybrid_ready_ttfs_seconds']} | "
                f"{r['bm25_first_ttfs_seconds']} | "
                f"{r['posthoc_vector_ready_seconds']} | "
                f"{r['first_search_speedup']}x |"
            )
        lines.append("")

    if "embedding_speed" in data and data["embedding_speed"]:
        es = data["embedding_speed"]
        lines.append("## Durable Hybrid Initialization\n")
        lines.append(f"**Repo:** {es.get('repo', 'unknown')}\n")
        lines.append("| Supported CLI path | Time (s) |")
        lines.append("|--------------------|----------|")
        lines.append(f"| `init --embed --model bge-small-en` | {es.get('hybrid_init_seconds', 'N/A')} |")
        lines.append("")

    return "\n".join(lines)


def main():
    import argparse
    parser = argparse.ArgumentParser(
        description="Codixing retrieval and durable-embedding benchmark"
    )
    parser.add_argument("--repo", choices=["openclaw", "linux", "both"], default="both")
    parser.add_argument("--skip-accuracy", action="store_true",
                        help="Skip search accuracy benchmark (useful for linux)")
    parser.add_argument("--skip-embedding", action="store_true",
                        help="Skip embedding speed benchmark")
    parser.add_argument("--skip-build", action="store_true")
    args = parser.parse_args()

    if not args.skip_build:
        build_codixing()

    repos = []
    if args.repo in ("openclaw", "both"):
        repos.append("openclaw")
    if args.repo in ("linux", "both"):
        repos.append("linux")

    # Preserve iteration order while filtering unavailable repositories.
    available_repos = []
    for repo_name in repos:
        if not REPOS[repo_name].exists():
            print(f"WARN: {repo_name} repo not found at {REPOS[repo_name]}", file=sys.stderr)
        else:
            available_repos.append(repo_name)
    repos = available_repos

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

    # Axis 4: supported durable embedding path
    if not args.skip_embedding and "openclaw" in repos:
        print("\n=== Axis 4: Durable Hybrid Initialization ===")
        data["embedding_speed"] = run_embedding_speed_benchmark(REPOS["openclaw"], "openclaw")

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

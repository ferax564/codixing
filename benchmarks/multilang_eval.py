#!/usr/bin/env python3
"""
multilang_eval.py — Multi-language search quality evaluation

Tests Codixing's ability to locate known symbols and files across
Rust, Python, Go, C++, and JavaScript codebases.

Usage:
    python3 benchmarks/multilang_eval.py
    python3 benchmarks/multilang_eval.py --repos gin leveldb
    python3 benchmarks/multilang_eval.py --skip-clone
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CODIXING = ROOT / "target" / "release" / "codixing"
REPOS_DIR = ROOT / "benchmarks" / "repos"

ENV = {
    **os.environ,
    "ORT_DYLIB_PATH": os.path.expanduser("~/.local/lib/libonnxruntime.so"),
    "LD_LIBRARY_PATH": os.path.expanduser("~/.local/lib"),
}

TASKS = {
    "tokio": {
        "lang": "Rust",
        "url": "https://github.com/tokio-rs/tokio",
        "queries": [
            ("Runtime struct definition", "runtime/runtime.rs"),
            ("TcpListener bind accept", "net/tcp/listener.rs"),
            ("spawn_blocking thread pool", "runtime/blocking"),
            ("JoinHandle abort cancel", "task/join.rs"),
            ("io copy async utility", "io/util/copy.rs"),
            ("sleep timer future", "time/sleep.rs"),
            ("channel mpsc sender", "sync/mpsc"),
            ("select macro implementation", "macros/select.rs"),
            ("signal ctrl_c handler", "signal"),
            ("read_buf buffer trait", "io/read_buf.rs"),
        ],
    },
    "django": {
        "lang": "Python",
        "url": "https://github.com/django/django",
        "queries": [
            ("QuerySet filter implementation", "db/models/query.py"),
            ("URL resolver match", "urls/resolvers.py"),
            ("Model save method", "db/models/base.py"),
            ("Template render context", "template/base.py"),
            ("Form validation clean", "forms/forms.py"),
            ("middleware process request", "middleware"),
            ("HttpResponse class", "http/response.py"),
            ("migration autodetector", "migrations/autodetector.py"),
            ("admin site register", "contrib/admin/sites.py"),
            ("cache backend set get", "cache/backends"),
        ],
    },
    "gin": {
        "lang": "Go",
        "url": "https://github.com/gin-gonic/gin",
        "queries": [
            ("Engine struct definition", "gin.go"),
            ("Context JSON response", "context.go"),
            ("RouterGroup middleware Use", "routergroup.go"),
            ("recovery middleware panic", "recovery.go"),
            ("binding validation", "binding"),
            ("render HTML template", "render"),
            ("logger middleware format", "logger.go"),
            ("tree node route matching", "tree.go"),
            ("BasicAuth middleware", "auth.go"),
            ("multipart form file upload", "context.go"),
        ],
    },
    "leveldb": {
        "lang": "C++",
        "url": "https://github.com/google/leveldb",
        "queries": [
            ("DB Open implementation", "db/db_impl.cc"),
            ("MemTable insert add", "db/memtable.cc"),
            ("SSTable block reader", "table/block.cc"),
            ("WriteBatch implementation", "db/write_batch.cc"),
            ("LRU cache implementation", "util/cache.cc"),
            ("compaction level merge", "db/db_impl.cc"),
            ("bloom filter policy", "util/bloom.cc"),
            ("log writer format", "db/log_writer.cc"),
            ("iterator merge implementation", "table/merger.cc"),
            ("snapshot sequence number", "db/snapshot.h"),
        ],
    },
    "react": {
        "lang": "JavaScript",
        "url": "https://github.com/facebook/react",
        "queries": [
            ("useState hook implementation", "ReactHooks"),
            ("reconciler beginWork fiber", "ReactFiberBeginWork"),
            ("createElement function", "ReactElement"),
            ("useEffect cleanup hook", "ReactFiberHooks"),
            ("commit work fiber", "ReactFiberCommitWork"),
            ("scheduler priority queue", "Scheduler"),
            ("context provider consumer", "ReactFiberNewContext"),
            ("suspense boundary fallback", "ReactFiberSuspense"),
            ("memo shallow comparison", "ReactMemo"),
            ("event delegation handler", "ReactDOMEventListener"),
        ],
    },
}


def clone_repo(name, url):
    """Shallow clone repo if not already present."""
    repo_dir = REPOS_DIR / name
    if repo_dir.exists():
        return repo_dir
    REPOS_DIR.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["git", "clone", "--depth=1", url, str(repo_dir)],
        check=True,
    )
    return repo_dir


def index_repo(repo_dir):
    """Index with codixing (BM25 only)."""
    codixing_dir = repo_dir / ".codixing"
    if codixing_dir.exists():
        shutil.rmtree(codixing_dir)
    start = time.perf_counter()
    r = subprocess.run(
        [str(CODIXING), "init", str(repo_dir), "--no-embeddings"],
        capture_output=True, timeout=120, env=ENV,
    )
    elapsed = time.perf_counter() - start
    success = r.returncode == 0
    if not success:
        sys.stderr.write(f"  Index error: {r.stderr.decode()[:200]}\n")
    return elapsed, success


def search(repo_dir, query, limit=20):
    """Search and return list of file paths."""
    r = subprocess.run(
        [str(CODIXING), "search", query, "--json", "--limit", str(limit)],
        capture_output=True, timeout=30, env=ENV, cwd=str(repo_dir),
    )
    if r.returncode != 0:
        return []
    try:
        data = json.loads(r.stdout)
        # Extract unique file paths from results
        files = []
        items = data if isinstance(data, list) else data.get("results", [])
        for item in items:
            fp = item.get("file") or item.get("file_path") or item.get("path", "")
            if fp and fp not in files:
                files.append(fp)
        return files
    except (json.JSONDecodeError, KeyError):
        return []


def evaluate(repos_filter=None, skip_clone=False):
    """Run evaluation across all repos."""
    results = {}

    for repo_name, config in TASKS.items():
        if repos_filter and repo_name not in repos_filter:
            continue

        lang = config["lang"]
        url = config["url"]
        queries = config["queries"]

        print(f"\n{'='*60}")
        print(f"{repo_name} ({lang})")
        print(f"{'='*60}")

        # Clone
        repo_dir = REPOS_DIR / repo_name
        if not skip_clone and not repo_dir.exists():
            print(f"  Cloning {url}...")
            clone_repo(repo_name, url)

        if not repo_dir.exists():
            print(f"  SKIP — repo not found at {repo_dir}")
            continue

        # Index
        print(f"  Indexing...", end=" ", flush=True)
        idx_time, success = index_repo(repo_dir)
        if not success:
            print(f"FAILED")
            continue
        print(f"{idx_time:.1f}s")

        # Search
        hit1, hit5, hit10 = 0, 0, 0
        for query, expected in queries:
            files = search(repo_dir, query)
            # Check if expected substring is in any of top-k file paths
            found_at = None
            for i, fp in enumerate(files[:20]):
                if expected.lower() in fp.lower():
                    found_at = i
                    break

            status = "miss"
            if found_at is not None:
                if found_at < 1:
                    hit1 += 1; hit5 += 1; hit10 += 1; status = "hit@1"
                elif found_at < 5:
                    hit5 += 1; hit10 += 1; status = "hit@5"
                elif found_at < 10:
                    hit10 += 1; status = "hit@10"

            top1 = files[0] if files else "—"
            print(f"  [{status:>6s}] {query[:50]:<50s} -> {top1}")

        n = len(queries)
        results[repo_name] = {
            "lang": lang,
            "tasks": n,
            "hit1": hit1, "hit5": hit5, "hit10": hit10,
            "hit1_pct": hit1 / n * 100, "hit5_pct": hit5 / n * 100, "hit10_pct": hit10 / n * 100,
            "index_time": idx_time,
        }
        print(f"  Hit@1={hit1/n:.0%}  Hit@5={hit5/n:.0%}  Hit@10={hit10/n:.0%}")

    # Summary
    print(f"\n{'='*60}")
    print("SUMMARY — Multi-Language Search Quality")
    print(f"{'='*60}")
    print(f"  {'Language':<12s} {'Repo':<15s} {'Tasks':>5s} {'Hit@1':>7s} {'Hit@5':>7s} {'Hit@10':>7s} {'Index':>7s}")
    print(f"  {'-'*70}")
    total_tasks, total_h1, total_h5, total_h10 = 0, 0, 0, 0
    for repo_name, r in results.items():
        print(f"  {r['lang']:<12s} {repo_name:<15s} {r['tasks']:>5d} {r['hit1_pct']:>6.0f}% {r['hit5_pct']:>6.0f}% {r['hit10_pct']:>6.0f}% {r['index_time']:>6.1f}s")
        total_tasks += r["tasks"]
        total_h1 += r["hit1"]
        total_h5 += r["hit5"]
        total_h10 += r["hit10"]
    if total_tasks:
        print(f"  {'-'*70}")
        print(f"  {'OVERALL':<28s} {total_tasks:>5d} {total_h1/total_tasks*100:>6.0f}% {total_h5/total_tasks*100:>6.0f}% {total_h10/total_tasks*100:>6.0f}%")

    # Save results
    results_file = ROOT / "benchmarks" / "results" / "multilang_eval.json"
    results_file.parent.mkdir(parents=True, exist_ok=True)
    with open(results_file, "w") as f:
        json.dump(results, f, indent=2)
    print(f"\nResults saved to: {results_file}")

    return results


def main():
    parser = argparse.ArgumentParser(description="Multi-language search quality evaluation")
    parser.add_argument("--repos", nargs="+", help="Only evaluate these repos")
    parser.add_argument("--skip-clone", action="store_true", help="Skip cloning (use existing)")
    args = parser.parse_args()

    if not CODIXING.exists():
        print(f"ERROR: codixing binary not found at {CODIXING}")
        print("Run: cargo build --release --bin codixing")
        sys.exit(1)

    evaluate(repos_filter=args.repos, skip_clone=args.skip_clone)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""
swe_bench_eval.py — SWE-bench Lite localization evaluation for Codixing

Measures how well Codixing retrieves the correct files given a bug report.
No LLM API keys, no Docker, no cost. Pure retrieval quality measurement.

For each of the 300 SWE-bench Lite tasks:
  1. Check out the repo at the correct base_commit
  2. Index with Codixing (BM25-only)
  3. Search with the problem_statement
  4. Compare returned files against gold files (from the patch)
  5. Compute recall@k and hit@k

Usage:
    python3 benchmarks/swe_bench_eval.py
    python3 benchmarks/swe_bench_eval.py --limit 50      # first 50 tasks
    python3 benchmarks/swe_bench_eval.py --repo django    # only django tasks
"""

import argparse
import json
import os
import re
import subprocess
import sys
import time
from collections import defaultdict
from pathlib import Path

from datasets import load_dataset

ROOT = Path(__file__).resolve().parent.parent
CODIXING = ROOT / "target" / "release" / "codixing"
REPOS_DIR = ROOT / "benchmarks" / "swe_bench_repos"
RESULTS_DIR = ROOT / "benchmarks" / "results"

ENV = {
    **os.environ,
    "ORT_DYLIB_PATH": os.path.expanduser("~/.local/lib/libonnxruntime.so"),
    "LD_LIBRARY_PATH": os.path.expanduser("~/.local/lib"),
}


def extract_gold_files(patch: str) -> set[str]:
    """Extract modified file paths from a unified diff patch."""
    files = set()
    for m in re.finditer(r"^--- a/(.+)$", patch, re.MULTILINE):
        f = m.group(1)
        if f != "/dev/null":
            files.add(f)
    for m in re.finditer(r"^\+\+\+ b/(.+)$", patch, re.MULTILINE):
        f = m.group(1)
        if f != "/dev/null":
            files.add(f)
    return files


def clone_repo(repo: str) -> Path:
    """Clone a repo (full, not shallow — we need to checkout old commits)."""
    org, name = repo.split("/")
    dest = REPOS_DIR / name
    if dest.exists():
        return dest
    print(f"  [clone] {repo}...")
    subprocess.run(
        ["git", "clone", f"https://github.com/{repo}.git", str(dest)],
        capture_output=True,
        timeout=600,
    )
    return dest


def checkout_commit(repo_path: Path, commit: str) -> bool:
    """Checkout a specific commit. Returns True on success."""
    result = subprocess.run(
        ["git", "checkout", "-f", commit],
        capture_output=True,
        timeout=60,
        cwd=str(repo_path),
    )
    return result.returncode == 0


def _is_test_file(path: str) -> bool:
    """Check if a file path looks like a test file."""
    return "/test" in path or path.startswith("tests/") or path.startswith("test/") or path.split("/")[-1].startswith("test_")


def _list_py_files(repo_path: Path) -> list[str]:
    """List all non-test .py files in a repo, relative paths."""
    import subprocess as sp
    result = sp.run(
        ["find", ".", "-name", "*.py",
         "-not", "-path", "*/.git/*",
         "-not", "-path", "*/.codixing/*",
         "-not", "-path", "*/__pycache__/*",
         "-not", "-path", "*/node_modules/*"],
        capture_output=True, timeout=30, cwd=str(repo_path),
    )
    files = []
    for line in result.stdout.decode(errors="replace").strip().split("\n"):
        f = line.strip().lstrip("./")
        if f and f.endswith(".py") and not _is_test_file(f):
            files.append(f)
    return sorted(files)


SEARCH_STRATEGY = None  # Set via --strategy CLI flag (None = default/auto)
EMBED_MODEL = None  # SweRankEmbed or other embedding model for reranking
EMBED_MODEL_NAME = None  # Model name string
CE_RERANKER = None  # Cross-encoder reranker model
CE_RERANKER_NAME = None  # Cross-encoder model name
EMBED_RETRIEVE_NAME = None  # Model name for full file retrieval
EMBED_RETRIEVE_MODEL = None  # Loaded model instance
EMBED_CACHE_DIR = ROOT / "benchmarks" / "embed_cache"
EMBED_ALPHA = 2.0  # Weight for embed retrieval in RRF fusion


def index_repo(repo_path: Path) -> tuple[int, bool]:
    """Index with codixing. Returns (time_ms, success)."""
    # Remove old index
    codixing_dir = repo_path / ".codixing"
    if codixing_dir.exists():
        subprocess.run(["rm", "-rf", str(codixing_dir)], capture_output=True)

    cmd = [str(CODIXING), "init", "."]

    start = time.perf_counter_ns()
    result = subprocess.run(
        cmd,
        capture_output=True,
        timeout=120,
        cwd=str(repo_path),
        env=ENV,
    )
    elapsed = (time.perf_counter_ns() - start) // 1_000_000
    return elapsed, result.returncode == 0


STOPWORDS = {
    "that", "this", "with", "from", "have", "been", "which", "when",
    "would", "should", "could", "there", "their", "them", "they",
    "will", "does", "done", "were", "what", "some", "more", "into",
    "than", "then", "also", "just", "like", "only", "very", "each",
    "other", "about", "after", "before", "because", "while", "being",
    "using", "used", "need", "want", "make", "case", "file", "code",
    "error", "following", "example", "expected", "actual", "issue",
    "problem", "currently", "instead", "however", "still", "model",
    "output", "input", "return", "value", "result", "class", "function",
    "method", "https", "description", "consider", "hello", "think",
    "work", "works", "working", "seems", "seem", "things", "thing",
    "something", "anything", "nothing", "everything", "every", "another",
    "first", "second", "third", "given", "possible", "possible",
    "report", "tried", "trying", "patch", "commit", "version", "test",
    "tests", "testing", "python", "django", "import", "print", "line",
    "lines", "added", "removed", "changed", "changes", "change",
}


def extract_queries(problem_statement: str) -> list[str]:
    """Extract multiple focused search queries from a problem statement.

    Returns a list of queries in priority order:
    1. Code identifiers from backtick spans (most precise)
    2. Dotted module paths (e.g., django.db.models.query)
    3. Error class names and traceback symbols
    4. Title line keywords
    """
    text = problem_statement
    queries = []

    # ── Query 1: Code identifiers from backtick spans ──
    code_spans = re.findall(r"`([^`]{2,80})`", text)
    code_parts = []
    seen = set()
    for span in code_spans:
        span = span.strip("()[]{}.,;: '\"")
        # Skip full expressions / long code, keep identifiers
        if " " not in span and len(span) < 60:
            if span and span.lower() not in STOPWORDS and span not in seen:
                code_parts.append(span)
                seen.add(span)
        elif "." in span and " " not in span:
            # Dotted path like django.db.models.QuerySet
            if span not in seen:
                code_parts.append(span)
                seen.add(span)
    if code_parts:
        queries.append(" ".join(code_parts[:6]))

    # ── Query 2: Dotted module/class paths from prose ──
    dotted = re.findall(r"\b[a-zA-Z_]\w+(?:\.\w+){1,6}\b", text[:2000])
    dotted_parts = []
    for d in dotted:
        # Filter out URLs and common patterns
        if d.startswith("http") or d.startswith("e.g"):
            continue
        if d not in seen:
            dotted_parts.append(d)
            seen.add(d)
    if dotted_parts:
        queries.append(" ".join(dotted_parts[:5]))

    # ── Query 3: Error/exception names + traceback symbols ──
    # Look for "XyzError", "XyzException", "raise Xyz"
    errors = re.findall(r"\b\w+(?:Error|Exception|Warning|Fault)\b", text)
    # Traceback file references
    tb_files = re.findall(r'File "([^"]+)"', text)
    tb_funcs = re.findall(r"in (\w+)\n", text)
    error_parts = []
    for e in errors[:3]:
        if e not in seen:
            error_parts.append(e)
            seen.add(e)
    for f in tb_funcs[:3]:
        if f not in seen and f.lower() not in STOPWORDS:
            error_parts.append(f)
            seen.add(f)
    if error_parts:
        queries.append(" ".join(error_parts))

    # ── Query 4: CamelCase + snake_case identifiers ──
    camel = re.findall(r"\b[A-Z][a-zA-Z0-9]{2,}(?:\.[A-Za-z_]\w*)*\b", text[:2000])
    snake = re.findall(r"\b[a-z_][a-z0-9_]{4,}\b", text[:1000])
    ident_parts = []
    for ident in camel + snake:
        if ident.lower() not in STOPWORDS and ident not in seen:
            ident_parts.append(ident)
            seen.add(ident)
            if len(ident_parts) >= 8:
                break
    if ident_parts:
        queries.append(" ".join(ident_parts[:8]))

    # ── Query 5: Title (first line) ──
    title = text.split("\n")[0].strip()
    if title and len(title) > 10:
        queries.append(title[:200])

    # Deduplicate queries that are too similar
    final = []
    for q in queries:
        if q and not any(q == existing for existing in final):
            final.append(q)

    # Fallback
    if not final:
        final.append(text[:300])

    return final[:5]


def search_codixing_single(repo_path: Path, query: str, limit: int = 15) -> list[tuple[str, float, str]]:
    """Run one codixing search, return list of (file_path, score, content)."""
    cmd = [str(CODIXING), "search", query, "--limit", str(limit), "--json"]
    if SEARCH_STRATEGY:
        cmd.extend(["--strategy", SEARCH_STRATEGY])
    result = subprocess.run(
        cmd,
        capture_output=True,
        timeout=30,
        cwd=str(repo_path),
        env=ENV,
    )
    if result.returncode != 0:
        return []

    stdout = result.stdout.decode(errors="replace").strip()
    results = []

    try:
        data = json.loads(stdout)
        if isinstance(data, list):
            for obj in data:
                fp = obj.get("file_path") or obj.get("file") or ""
                score = obj.get("score", 0.0)
                content = obj.get("content", "")
                if fp:
                    fp = fp.lstrip("./")
                    results.append((fp, score, content))
            return results
    except json.JSONDecodeError:
        pass

    # Fallback: parse non-JSON
    for line in stdout.split("\n"):
        m = re.match(r"\d+\.\s+(.+?)\s+\[", line.strip())
        if m:
            results.append((m.group(1).lstrip("./"), 0.0, ""))
    return results


def search_codixing_find_symbol(repo_path: Path, symbol: str) -> str | None:
    """Use `codixing find-symbol` to locate where a symbol is defined. Returns file path or None."""
    result = subprocess.run(
        [str(CODIXING), "find-symbol", symbol, "--json"],
        capture_output=True,
        timeout=10,
        cwd=str(repo_path),
        env=ENV,
    )
    if result.returncode != 0:
        return None
    try:
        data = json.loads(result.stdout.decode(errors="replace").strip())
        if isinstance(data, list) and data:
            return data[0].get("file", "").lstrip("./")
        if isinstance(data, dict) and data.get("file"):
            return data["file"].lstrip("./")
    except (json.JSONDecodeError, KeyError):
        pass
    # Fallback: parse text output
    for line in result.stdout.decode(errors="replace").split("\n"):
        m = re.match(r"\s*(\S+?\.\w+)\s+\[L\d+", line.strip())
        if m:
            return m.group(1).lstrip("./")
    return None


def search_codixing_usages(repo_path: Path, symbol: str, limit: int = 10) -> list[str]:
    """Use `codixing usages` for file-level coverage of a symbol."""
    result = subprocess.run(
        [str(CODIXING), "usages", symbol, "--limit", str(limit)],
        capture_output=True,
        timeout=15,
        cwd=str(repo_path),
        env=ENV,
    )
    if result.returncode != 0:
        return []

    files = []
    seen = set()
    for line in result.stdout.decode(errors="replace").split("\n"):
        # CLI format: "django/db/models/query.py [L42-L45]   0.850  preview..."
        m = re.match(r"\s*(\S+?\.\w+)\s+\[L\d+", line.strip())
        if m:
            fp = m.group(1).lstrip("./")
            if fp and fp not in seen:
                files.append(fp)
                seen.add(fp)
    return files


def get_embed_model():
    """Lazily initialize the embedding model for reranking."""
    global EMBED_MODEL
    if EMBED_MODEL is None and EMBED_MODEL_NAME:
        from sentence_transformers import SentenceTransformer
        print(f"  [embed] Loading {EMBED_MODEL_NAME}...", flush=True)
        EMBED_MODEL = SentenceTransformer(EMBED_MODEL_NAME, trust_remote_code=True)
        print(f"  [embed] Ready ({EMBED_MODEL.get_sentence_embedding_dimension()}d).", flush=True)
    return EMBED_MODEL



def get_ce_reranker():
    """Lazily initialize the cross-encoder reranker model."""
    global CE_RERANKER
    if CE_RERANKER is None and CE_RERANKER_NAME:
        from sentence_transformers import CrossEncoder
        print(f"  [ce-rerank] Loading {CE_RERANKER_NAME}...", flush=True)
        CE_RERANKER = CrossEncoder(CE_RERANKER_NAME, trust_remote_code=True)
        print(f"  [ce-rerank] Ready.", flush=True)
    return CE_RERANKER


def get_embed_retrieve_model():
    """Lazily initialize the embedding retrieval model."""
    global EMBED_RETRIEVE_MODEL
    if EMBED_RETRIEVE_MODEL is None and EMBED_RETRIEVE_NAME:
        from sentence_transformers import SentenceTransformer
        print(f"  [embed-retrieve] Loading {EMBED_RETRIEVE_NAME}...", flush=True)
        EMBED_RETRIEVE_MODEL = SentenceTransformer(EMBED_RETRIEVE_NAME, trust_remote_code=True)
        EMBED_RETRIEVE_MODEL.max_seq_length = 512  # Truncate for CPU speed
        print(f"  [embed-retrieve] Ready ({EMBED_RETRIEVE_MODEL.get_sentence_embedding_dimension()}d, max_seq=512).", flush=True)
    return EMBED_RETRIEVE_MODEL


def _file_content_hash(content: bytes) -> str:
    """Short hash of file content for cache key."""
    import hashlib
    return hashlib.sha256(content).hexdigest()[:16]


def _get_cached_embeddings(
    repo_path: Path, repo_name: str, files: list[str]
) -> tuple[list[str], "np.ndarray"]:
    """Load or compute embeddings for all files. Returns (file_list, embeddings_matrix).

    Cache is stored per-repo in EMBED_CACHE_DIR/<repo_name>/<content_hash>.npy.
    Files whose content hash is already cached are loaded from disk; the rest
    are batch-encoded with SweRankEmbed and cached.
    """
    import numpy as np

    model = get_embed_retrieve_model()
    if model is None:
        return [], np.array([])

    cache_dir = EMBED_CACHE_DIR / repo_name
    cache_dir.mkdir(parents=True, exist_ok=True)

    valid_files = []
    embeddings = []
    to_encode = []  # (index_in_valid, content_text)

    for fp in files:
        full_path = repo_path / fp
        try:
            content_bytes = full_path.read_bytes()
        except OSError:
            continue

        chash = _file_content_hash(content_bytes)
        cache_path = cache_dir / f"{chash}.npy"
        valid_files.append(fp)

        if cache_path.exists():
            emb = np.load(cache_path)
            embeddings.append(emb)
        else:
            embeddings.append(None)
            content_text = content_bytes.decode(errors="replace")[:8000]  # ~2K tokens
            to_encode.append((len(valid_files) - 1, content_text, chash))

    # Batch encode uncached files
    if to_encode:
        texts = [text for _, text, _ in to_encode]
        encoded = model.encode(texts, batch_size=8, normalize_embeddings=True,
                               show_progress_bar=len(texts) > 100)
        for j, (idx, _, chash) in enumerate(to_encode):
            emb = encoded[j]
            embeddings[idx] = emb
            np.save(cache_dir / f"{chash}.npy", emb)

    if not embeddings or all(e is None for e in embeddings):
        return [], np.array([])

    return valid_files, np.stack(embeddings)


def embed_retrieve_files(
    repo_path: Path, repo_name: str, query: str, files: list[str], top_k: int = 20
) -> list[tuple[str, float]]:
    """Full-file embedding retrieval: encode all files, rank by cosine similarity.

    Unlike embed_rerank_files() which only scores outlines of BM25's top-20,
    this scores ALL non-test .py files in the repo against the query.
    """
    import numpy as np

    model = get_embed_retrieve_model()
    if model is None or not files:
        return []

    valid_files, file_embs = _get_cached_embeddings(repo_path, repo_name, files)
    if len(valid_files) == 0:
        return []

    q_emb = model.encode([query], prompt_name="query", normalize_embeddings=True)
    scores = (q_emb @ file_embs.T)[0]

    scored = sorted(zip(valid_files, scores.tolist()), key=lambda x: -x[1])
    return scored[:top_k]


def ce_rerank_files(
    repo_path: Path, query: str, files: list[str], top_k: int = 10
) -> list[tuple[str, float]]:
    """Cross-encoder reranking: score (query, file_outline) pairs jointly.

    Uses a code-aware cross-encoder (e.g. GTE-Reranker-ModernBERT-Base) that
    has been evaluated on code retrieval benchmarks (COIR).
    """
    model = get_ce_reranker()
    if model is None or not files:
        return [(f, 0.0) for f in files]

    # Build (query, outline) pairs
    pairs = []
    valid_files = []
    for fp in files[:top_k]:
        full_path = repo_path / fp
        if full_path.exists():
            outline = extract_file_outline(full_path, fp)
            pairs.append((query[:1500], outline))
            valid_files.append(fp)
        else:
            valid_files.append(fp)
            pairs.append((query[:1500], fp))

    # Score all pairs at once
    scores = model.predict(pairs)

    scored = list(zip(valid_files, scores.tolist()))
    scored.sort(key=lambda x: -x[1])
    return scored


def extract_functions_from_file(file_path: Path, rel_path: str) -> list[tuple[str, str]]:
    """Extract functions/methods from a Python file using AST.

    Returns list of (doc_id, source_code) tuples where doc_id is like
    'path/to/file.py/ClassName/method_name'.
    """
    import ast
    try:
        source = file_path.read_text(errors="replace")
    except (OSError, UnicodeDecodeError):
        return []

    try:
        tree = ast.parse(source)
    except SyntaxError:
        return []

    lines = source.splitlines(keepends=True)
    results = []
    seen = set()

    def get_source(node):
        start = node.lineno - 1
        end = node.end_lineno if hasattr(node, "end_lineno") and node.end_lineno else start + 1
        return "".join(lines[start:end])[:2000]

    # Top-level functions
    for node in ast.iter_child_nodes(tree):
        if isinstance(node, ast.FunctionDef | ast.AsyncFunctionDef):
            doc_id = f"{rel_path}/{node.name}"
            if doc_id not in seen:
                seen.add(doc_id)
                src = get_source(node)
                if src.strip():
                    results.append((doc_id, src))
        elif isinstance(node, ast.ClassDef):
            # Class methods
            for child in ast.iter_child_nodes(node):
                if isinstance(child, ast.FunctionDef | ast.AsyncFunctionDef):
                    doc_id = f"{rel_path}/{node.name}/{child.name}"
                    if doc_id not in seen:
                        seen.add(doc_id)
                        src = get_source(child)
                        if src.strip():
                            results.append((doc_id, src))

    # Fallback: if no functions found, use the whole file (truncated)
    if not results:
        src = "".join(lines[:100])[:3000]
        if src.strip():
            results.append((rel_path, src))

    return results


def extract_file_outline(file_path: Path, rel_path: str) -> str:
    """Extract a compact outline of a Python file: path + class/function signatures.

    Returns a single string like:
    'django/db/models/lookups.py
    class Lookup: ...
    def get_lookup(name): ...
    class Exact(Lookup): ...'

    This is ~200 chars and encodes 5x faster than full function bodies.
    """
    import ast
    try:
        source = file_path.read_text(errors="replace")
    except (OSError, UnicodeDecodeError):
        return rel_path

    try:
        tree = ast.parse(source)
    except SyntaxError:
        return rel_path

    lines = source.splitlines()
    parts = [rel_path]

    for node in ast.iter_child_nodes(tree):
        if isinstance(node, ast.FunctionDef | ast.AsyncFunctionDef):
            sig = lines[node.lineno - 1].strip() if node.lineno <= len(lines) else ""
            parts.append(sig[:120])
        elif isinstance(node, ast.ClassDef):
            sig = lines[node.lineno - 1].strip() if node.lineno <= len(lines) else ""
            parts.append(sig[:120])
            # Include method names
            methods = []
            for child in ast.iter_child_nodes(node):
                if isinstance(child, ast.FunctionDef | ast.AsyncFunctionDef):
                    methods.append(child.name)
            if methods:
                parts.append(f"  methods: {', '.join(methods[:15])}")

    return "\n".join(parts)[:800]



def embed_rerank_files(
    repo_path: Path, query: str, files: list[str], top_k: int = 20
) -> list[tuple[str, float]]:
    """Outline-only embedding rerank: fast (~0.7s for 20 files on CPU).

    Embeds file outlines (path + class/function signatures) and scores against
    the query. At 31 outlines/s this adds <1s overhead per task.
    """
    model = get_embed_model()
    if not model or not files:
        return [(f, 0.0) for f in files]

    import numpy as np
    q_emb = model.encode([query], prompt_name="query", normalize_embeddings=True)

    outlines = []
    for fp in files[:top_k]:
        full_path = repo_path / fp
        if full_path.suffix == ".py":
            outline = extract_file_outline(full_path, fp)
        else:
            outline = fp
        outlines.append((fp, outline))

    if not outlines:
        return [(f, 0.0) for f in files]

    outline_texts = [text for _, text in outlines]
    o_embs = model.encode(outline_texts, batch_size=64, normalize_embeddings=True)
    scores = (q_emb @ o_embs.T)[0]

    scored = [(fp, float(scores[i])) for i, (fp, _) in enumerate(outlines)]
    scored.sort(key=lambda x: -x[1])
    seen = {f for f, _ in scored}
    for f in files:
        if f not in seen:
            scored.append((f, 0.0))
    return scored



def search_codixing_multi(repo_path: Path, problem: str, repo_name: str = "") -> list[str]:
    """Multi-strategy Codixing search combining chunk-level + file-level results.

    Runs multiple queries and merges results using score-weighted ranking.
    Also uses `usages` for key symbols to add file-level coverage.
    """
    queries = extract_queries(problem)

    # Accumulate file scores across all queries
    file_scores: dict[str, float] = defaultdict(float)

    # Run each query with decreasing weight
    weights = [1.0, 0.7, 0.5, 0.4, 0.3]
    for i, query in enumerate(queries):
        weight = weights[i] if i < len(weights) else 0.2
        results = search_codixing_single(repo_path, query, limit=30)

        for rank, (fp, score, _content) in enumerate(results):
            # Score: BM25 score * weight * rank decay
            rank_factor = 1.0 / (rank + 1)
            file_scores[fp] += score * weight * rank_factor

    # ── Traceback file extraction ──
    # Extract source file paths from tracebacks (File "...") and map to repo paths
    tb_files = re.findall(r'File "([^"]+)"', problem)
    seen_tb = set()
    for tf in tb_files:
        # Skip non-source files
        if tf.startswith("<") or not tf.endswith(".py"):
            continue
        # Extract the package-relative path from absolute paths
        # e.g., "/path/to/site-packages/django/utils/autoreload.py" -> "django/utils/autoreload.py"
        # e.g., "…/src/django/django/contrib/staticfiles/handlers.py" -> "django/contrib/staticfiles/handlers.py"
        basename = tf.split("/")[-1]
        if basename in seen_tb or basename in ("manage.py", "conftest.py", "tests.py"):
            continue
        # Try to find the repo-relative path by matching path components
        parts = tf.replace("\\", "/").split("/")
        # Find the repo root component (e.g., "django", "astropy")
        repo_name = str(repo_path.name)  # e.g., "django"
        for i, p in enumerate(parts):
            if p == repo_name and i + 1 < len(parts):
                candidate = "/".join(parts[i:])
                if (repo_path / candidate).exists():
                    seen_tb.add(basename)
                    boost = 20.0 if _is_test_file(candidate) else 70.0
                    file_scores[candidate] += boost
                    break
        else:
            # Fallback: find by basename
            if basename not in seen_tb:
                seen_tb.add(basename)
                find_result = subprocess.run(
                    ["find", ".", "-name", basename, "-not", "-path", "*/.codixing/*", "-not", "-path", "*/test*"],
                    capture_output=True, timeout=5, cwd=str(repo_path),
                )
                matches = [
                    l.strip().lstrip("./")
                    for l in find_result.stdout.decode(errors="replace").strip().split("\n")
                    if l.strip() and l.strip().lstrip("./").endswith(".py")
                ]
                if len(matches) == 1:
                    # Only boost if unambiguous
                    file_scores[matches[0]] += 50.0

    # ── Direct repo-relative file path mentions ──
    # Match paths like "django/contrib/sitemaps/__init__.py" directly
    repo_name = repo_path.name  # e.g., "django"
    repo_paths_in_text = re.findall(
        rf"\b({re.escape(repo_name)}/[\w/]+\.py)\b", problem
    )
    for rp in repo_paths_in_text[:5]:
        if (repo_path / rp).exists():
            file_scores[rp] += 90.0

    # ── File name hints from text ──
    # Extract file names mentioned in the problem statement
    file_mentions = re.findall(
        r"\b(\w[\w/]*\.(?:py|rs|js|ts|go|java|rb|cpp|c|h))\b", problem
    )
    seen_fm = set()
    # Common names that appear in hundreds of files — skip them
    common_basenames = {"models.py", "__init__.py", "views.py", "forms.py", "admin.py", "urls.py", "apps.py", "utils.py"}
    for fm in file_mentions[:10]:
        basename = fm.split("/")[-1]
        if basename in seen_fm or basename in ("setup.py", "manage.py", "conftest.py"):
            continue
        if basename in common_basenames:
            continue  # Too ambiguous — hundreds of matches
        seen_fm.add(basename)
        # Find actual files matching this basename
        find_result = subprocess.run(
            ["find", ".", "-name", basename, "-not", "-path", "*/.codixing/*"],
            capture_output=True, timeout=5, cwd=str(repo_path),
        )
        matches = [
            line.strip().lstrip("./")
            for line in find_result.stdout.decode(errors="replace").strip().split("\n")
            if line.strip() and line.strip().lstrip("./").endswith(".py")
        ]
        if len(matches) > 10:
            continue  # Too many matches — not discriminative
        for fp in matches:
            # Strong boost, but reduce for test files
            boost = 20.0 if _is_test_file(fp) else 80.0
            file_scores[fp] += boost

    # ── Dotted module path → file path mapping ──
    # e.g., "django.db.models.fields" → "django/db/models/fields.py" or "__init__.py"
    # Also handles "django.contrib.admin.utils.display_for_field" → try progressively shorter paths
    dotted_paths = re.findall(r"\b([a-zA-Z_]\w+(?:\.\w+){2,6})\b", problem[:3000])
    seen_dp = set()
    for dp in dotted_paths[:10]:
        if dp.startswith("http") or dp.startswith("e.g") or dp.startswith("self."):
            continue
        if dp in seen_dp:
            continue
        seen_dp.add(dp)
        # Try progressively shorter suffixes: a.b.c.d → a/b/c/d.py, a/b/c.py, a/b.py
        parts = dp.split(".")
        for end in range(len(parts), max(1, len(parts) - 3), -1):
            prefix = "/".join(parts[:end])
            for suffix in (prefix + ".py", prefix + "/__init__.py"):
                if (repo_path / suffix).exists():
                    file_scores[suffix] += 60.0
                    break
            else:
                continue
            break  # Found a match, stop trying shorter paths

    # ── Code block symbol definitions ──
    # Extract class/function names from code blocks and find their definitions
    code_blocks = re.findall(r"```[^\n]*\n(.*?)```", problem, re.DOTALL)
    cb_symbols = set()
    for cb in code_blocks[:3]:
        for m in re.finditer(r"^\s*(?:class|def)\s+(\w+)", cb, re.MULTILINE):
            name = m.group(1)
            if name and len(name) > 2 and name.lower() not in STOPWORDS:
                cb_symbols.add(name)
    for sym in list(cb_symbols)[:5]:
        fp = search_codixing_find_symbol(repo_path, sym)
        if fp and fp.endswith(".py"):
            # Symbol definition is a strong signal
            file_scores[fp] += 40.0

    # NOTE: Several approaches were tested here but all HURT R@1:
    # - Import-chain expansion, __init__.py resolution, migration detection:
    #   R@1 dropped from 48% to ~35% (floods file_scores with noise)
    # - Cross-encoder reranking (ms-marco-MiniLM-L-6-v2, BAAI/bge-reranker):
    #   R@1 dropped from 48% to 27-41% (web-search models pick wrong code files)
    # See git history for the implementations.

    # ── File-level coverage via usages ──
    # Extract top 3 most distinctive code identifiers
    code_spans = re.findall(r"`([A-Za-z_]\w{2,40})`", problem)
    # Also try CamelCase from first query
    camel = re.findall(r"\b[A-Z][a-zA-Z]{3,}\b", queries[0] if queries else "")
    key_symbols = []
    seen_sym = set()
    for sym in code_spans + camel:
        sym = sym.strip("().,;: ")
        if (
            sym
            and sym.lower() not in STOPWORDS
            and sym not in seen_sym
            and not sym[0].islower()  # prefer CamelCase symbols for usages
        ):
            key_symbols.append(sym)
            seen_sym.add(sym)
            if len(key_symbols) >= 3:
                break

    for sym in key_symbols:
        usage_files = search_codixing_usages(repo_path, sym, limit=8)
        for rank, fp in enumerate(usage_files):
            # Lower weight for usage results but they provide file-level coverage
            file_scores[fp] += 0.3 / (rank + 1)

    # ── Rank and filter ──
    # Remove test files from top positions (they're noise for localization)
    ranked = sorted(file_scores.items(), key=lambda x: -x[1])

    # Separate non-test and test files
    non_test = [(f, s) for f, s in ranked if not _is_test_file(f)]
    test_files = [(f, s) for f, s in ranked if _is_test_file(f)]

    # Interleave: non-test files first, then test files
    final = [f for f, _ in non_test] + [f for f, _ in test_files]

    # ── Optional: embedding-based reranking ──
    if EMBED_MODEL_NAME and final:
        # Use full problem statement (model was trained on full issue text)
        rerank_query = problem
        # Only embed non-test files
        non_test_final = [f for f in final[:20] if not _is_test_file(f)]
        embed_ranked = embed_rerank_files(repo_path, rerank_query, non_test_final, top_k=20)

        # Weighted RRF fusion: embed gets 2x weight over BM25
        bm25_rank = {f: i for i, f in enumerate(final[:20])}
        embed_rank = {f: i for i, (f, _s) in enumerate(embed_ranked[:20])}
        k = 60  # RRF constant
        rrf_scores = {}
        for f in set(list(bm25_rank.keys()) + list(embed_rank.keys())):
            bm25_rrf = 1.0 / (k + bm25_rank.get(f, 50))
            embed_rrf = 1.0 / (k + embed_rank.get(f, 50))
            rrf_scores[f] = bm25_rrf + 2.0 * embed_rrf  # Embed 2x weight

        final = sorted(rrf_scores.keys(), key=lambda f: -rrf_scores[f])
        # Add back remaining files
        seen = set(final)
        for f, _ in non_test + test_files:
            if f not in seen:
                final.append(f)
                seen.add(f)

    # ── Optional: full-file embedding retrieval ──
    if EMBED_RETRIEVE_NAME:
        all_py_files = _list_py_files(repo_path)
        if all_py_files:
            embed_results = embed_retrieve_files(
                repo_path, repo_name, problem, all_py_files, top_k=20
            )
            if embed_results:
                bm25_rank = {f: i for i, f in enumerate(final[:20])}
                embed_rank = {f: i for i, (f, _) in enumerate(embed_results)}
                k = 60
                rrf_scores = {}
                all_candidates = set(list(bm25_rank.keys()) + list(embed_rank.keys()))
                for f in all_candidates:
                    bm25_rrf = 1.0 / (k + bm25_rank.get(f, 100))
                    embed_rrf = 1.0 / (k + embed_rank.get(f, 100))
                    rrf_scores[f] = bm25_rrf + EMBED_ALPHA * embed_rrf
                fused = sorted(rrf_scores.keys(), key=lambda f: -rrf_scores[f])
                if EMBED_ALPHA >= 100:
                    # Standalone mode: only embed results, no BM25
                    final = [f for f, _ in embed_results]
                else:
                    seen = set(fused)
                    for f in final:
                        if f not in seen:
                            fused.append(f)
                            seen.add(f)
                    final = fused

    # ── Optional: cross-encoder reranking (currently disabled — hurts R@1) ──
    # GTE-Reranker-ModernBERT-Base and ms-marco-MiniLM both degrade performance.
    # Cross-encoders trained on text relevance don't transfer to code localization.
    if CE_RERANKER_NAME and final:
        rerank_query = problem
        top_non_test = [f for f in final[:5] if not _is_test_file(f)]
        if len(top_non_test) >= 2:
            ce_ranked = ce_rerank_files(repo_path, rerank_query, top_non_test, top_k=5)
            ce_order = [f for f, _ in ce_ranked]
            rest = [f for f in final if f not in set(ce_order)]
            final = ce_order + rest

    return final[:20]


def search_grep(repo_path: Path, query: str, limit: int = 20) -> list[str]:
    """Baseline: extract keywords from query and grep for them."""
    # Extract important-looking terms
    words = re.findall(r"[A-Za-z_][A-Za-z0-9_]{3,}", query[:1000])
    # Filter out common English words
    stopwords = {
        "that", "this", "with", "from", "have", "been", "which", "when",
        "would", "should", "could", "there", "their", "them", "they",
        "will", "does", "done", "were", "what", "some", "more", "into",
        "than", "then", "also", "just", "like", "only", "very", "each",
        "other", "about", "after", "before", "because", "while", "being",
        "using", "used", "need", "want", "make", "case", "file", "code",
        "error", "following", "example", "expected", "actual", "issue",
        "problem", "currently", "instead", "however", "still",
    }
    keywords = [w for w in words if w.lower() not in stopwords][:8]

    file_hits = defaultdict(int)
    for kw in keywords:
        result = subprocess.run(
            ["grep", "-ril", kw, ".", "--include=*.py"],
            capture_output=True,
            timeout=15,
            cwd=str(repo_path),
        )
        for f in result.stdout.decode(errors="replace").strip().split("\n"):
            f = f.strip().lstrip("./")
            if f:
                file_hits[f] += 1

    # Sort by number of keyword hits (descending)
    ranked = sorted(file_hits.items(), key=lambda x: -x[1])
    return [f for f, _ in ranked[:limit]]


def recall_at_k(predicted: list[str], gold: set[str], k: int) -> float:
    """Fraction of gold files in top-k predictions."""
    if not gold:
        return 1.0
    top_k = set(predicted[:k])
    return len(top_k & gold) / len(gold)


def hit_at_k(predicted: list[str], gold: set[str], k: int) -> float:
    """Binary: is any gold file in top-k?"""
    if not gold:
        return 1.0
    top_k = set(predicted[:k])
    return 1.0 if (top_k & gold) else 0.0


def contains_gt(predicted: list[str], gold: set[str]) -> float:
    """Agentless metric: does predicted set contain ALL gold files?"""
    if not gold:
        return 1.0
    return 1.0 if gold.issubset(set(predicted)) else 0.0


def main():
    parser = argparse.ArgumentParser(description="SWE-bench Lite localization eval")
    parser.add_argument("--limit", type=int, help="Max tasks to evaluate")
    parser.add_argument("--repo", help="Only evaluate tasks from this repo")
    parser.add_argument("--skip-clone", action="store_true")
    parser.add_argument("--skip-grep", action="store_true", help="Skip grep baseline")
    parser.add_argument("--strategy", help="Search strategy (e.g. 'deep' for reranker)")
    parser.add_argument(
        "--embed-rerank",
        metavar="MODEL",
        help="Embedding model for file reranking (e.g. Salesforce/SweRankEmbed-Small)",
    )
    parser.add_argument(
        "--ce-rerank",
        metavar="MODEL",
        help="Cross-encoder model for reranking (e.g. Alibaba-NLP/gte-reranker-modernbert-base)",
    )
    parser.add_argument(
        "--embed-retrieve",
        metavar="MODEL",
        help="Embedding model for full-file retrieval (e.g. Salesforce/SweRankEmbed-Small)",
    )
    parser.add_argument("--embed-alpha", type=float, default=2.0,
                        help="Weight for embed retrieval in RRF fusion (default: 2.0)")
    args = parser.parse_args()

    global SEARCH_STRATEGY, EMBED_MODEL_NAME, CE_RERANKER_NAME, EMBED_RETRIEVE_NAME, EMBED_ALPHA
    SEARCH_STRATEGY = args.strategy
    EMBED_MODEL_NAME = args.embed_rerank
    CE_RERANKER_NAME = args.ce_rerank
    EMBED_RETRIEVE_NAME = args.embed_retrieve
    EMBED_ALPHA = args.embed_alpha

    if not CODIXING.exists():
        print(f"ERROR: codixing binary not found at {CODIXING}")
        sys.exit(1)

    REPOS_DIR.mkdir(parents=True, exist_ok=True)
    RESULTS_DIR.mkdir(parents=True, exist_ok=True)

    # Load dataset
    print("Loading SWE-bench Lite dataset...")
    ds = load_dataset("princeton-nlp/SWE-bench_Lite", split="test")
    tasks = list(ds)

    if args.repo:
        tasks = [t for t in tasks if args.repo.lower() in t["repo"].lower()]
    if args.limit:
        tasks = tasks[: args.limit]

    print(f"Tasks: {len(tasks)}")
    repos = sorted(set(t["repo"] for t in tasks))
    print(f"Repos: {', '.join(repos)}")

    # Clone repos (we need full history for checkout)
    if not args.skip_clone:
        for repo in repos:
            clone_repo(repo)

    # Track metrics
    cdx_results = []
    grep_results = []
    repo_cache = {}  # track last indexed commit per repo

    total_start = time.perf_counter_ns()

    for i, task in enumerate(tasks):
        instance_id = task["instance_id"]
        repo = task["repo"]
        commit = task["base_commit"]
        problem = task["problem_statement"]
        gold = extract_gold_files(task["patch"])

        org, name = repo.split("/")
        repo_path = REPOS_DIR / name

        if not repo_path.exists():
            print(f"  [{i+1}/{len(tasks)}] SKIP {instance_id} — repo not cloned")
            continue

        print(f"  [{i+1}/{len(tasks)}] {instance_id} ({len(gold)} gold files)...", end=" ", flush=True)

        # Checkout
        if not checkout_commit(repo_path, commit):
            print("FAIL (checkout)")
            continue

        # Index (re-index if commit changed for this repo)
        cache_key = (name, commit)
        if cache_key not in repo_cache:
            idx_ms, success = index_repo(repo_path)
            if not success:
                print(f"FAIL (index)")
                continue
            repo_cache[cache_key] = idx_ms

        # Multi-strategy Codixing search: multiple queries + usages
        cdx_files = search_codixing_multi(repo_path, problem, repo_name=name)

        # Compute codixing metrics
        cdx_r1 = recall_at_k(cdx_files, gold, 1)
        cdx_r3 = recall_at_k(cdx_files, gold, 3)
        cdx_r5 = recall_at_k(cdx_files, gold, 5)
        cdx_r10 = recall_at_k(cdx_files, gold, 10)
        cdx_h1 = hit_at_k(cdx_files, gold, 1)
        cdx_h3 = hit_at_k(cdx_files, gold, 3)
        cdx_h5 = hit_at_k(cdx_files, gold, 5)
        cdx_cgt = contains_gt(cdx_files, gold)

        cdx_results.append({
            "instance_id": instance_id,
            "repo": repo,
            "gold_files": sorted(gold),
            "predicted_files": cdx_files[:10],
            "recall@1": cdx_r1,
            "recall@3": cdx_r3,
            "recall@5": cdx_r5,
            "recall@10": cdx_r10,
            "hit@1": cdx_h1,
            "hit@3": cdx_h3,
            "hit@5": cdx_h5,
            "contains_gt": cdx_cgt,
        })

        tag = "hit" if cdx_h1 > 0 else ("top5" if cdx_h5 > 0 else "miss")

        # Grep baseline
        if not args.skip_grep:
            grep_files = search_grep(repo_path, problem, limit=20)
            grep_r1 = recall_at_k(grep_files, gold, 1)
            grep_r5 = recall_at_k(grep_files, gold, 5)
            grep_r10 = recall_at_k(grep_files, gold, 10)
            grep_h1 = hit_at_k(grep_files, gold, 1)
            grep_h5 = hit_at_k(grep_files, gold, 5)
            grep_cgt = contains_gt(grep_files, gold)
            grep_results.append({
                "instance_id": instance_id,
                "recall@1": grep_r1,
                "recall@5": grep_r5,
                "recall@10": grep_r10,
                "hit@1": grep_h1,
                "hit@5": grep_h5,
                "contains_gt": grep_cgt,
            })
            print(f"cdx={tag} r@5={cdx_r5:.0%} | grep r@5={grep_r5:.0%}")
        else:
            print(f"cdx={tag} r@1={cdx_r1:.0%} r@5={cdx_r5:.0%} r@10={cdx_r10:.0%}")

    total_ms = (time.perf_counter_ns() - total_start) // 1_000_000

    # Aggregate
    n = len(cdx_results)
    if n == 0:
        print("No results. Check that repos are cloned.")
        return

    print(f"\n{'='*70}")
    print(f"  SWE-bench Lite Localization Results ({n} tasks)")
    print(f"{'='*70}\n")

    def avg(key, data):
        return sum(d[key] for d in data) / len(data)

    print("CODIXING (BM25-only)")
    print(f"  Recall@1:     {avg('recall@1', cdx_results):.1%}")
    print(f"  Recall@3:     {avg('recall@3', cdx_results):.1%}")
    print(f"  Recall@5:     {avg('recall@5', cdx_results):.1%}")
    print(f"  Recall@10:    {avg('recall@10', cdx_results):.1%}")
    print(f"  Hit@1:        {avg('hit@1', cdx_results):.1%}")
    print(f"  Hit@3:        {avg('hit@3', cdx_results):.1%}")
    print(f"  Hit@5:        {avg('hit@5', cdx_results):.1%}")
    print(f"  Contains GT:  {avg('contains_gt', cdx_results):.1%}")

    if grep_results:
        print(f"\nGREP BASELINE (keyword extraction)")
        print(f"  Recall@1:     {avg('recall@1', grep_results):.1%}")
        print(f"  Recall@5:     {avg('recall@5', grep_results):.1%}")
        print(f"  Recall@10:    {avg('recall@10', grep_results):.1%}")
        print(f"  Hit@1:        {avg('hit@1', grep_results):.1%}")
        print(f"  Hit@5:        {avg('hit@5', grep_results):.1%}")
        print(f"  Contains GT:  {avg('contains_gt', grep_results):.1%}")

        print(f"\nCOMPARISON")
        for metric in ["recall@1", "recall@5", "recall@10", "hit@1", "hit@5", "contains_gt"]:
            c = avg(metric, cdx_results)
            g = avg(metric, grep_results)
            diff = c - g
            winner = "Codixing" if diff > 0 else ("grep" if diff < 0 else "tie")
            print(f"  {metric:<15} cdx={c:.1%} grep={g:.1%} {'+'if diff>0 else ''}{diff:.1%} → {winner}")

    # Per-repo breakdown
    print(f"\nPER-REPO RECALL@5")
    by_repo = defaultdict(list)
    for r in cdx_results:
        by_repo[r["repo"]].append(r["recall@5"])
    for repo in sorted(by_repo):
        vals = by_repo[repo]
        print(f"  {repo:<35} {sum(vals)/len(vals):.1%} ({len(vals)} tasks)")

    # Save results
    report = {
        "benchmark": "SWE-bench Lite",
        "tasks_evaluated": n,
        "total_time_ms": total_ms,
        "codixing": {
            "recall@1": avg("recall@1", cdx_results),
            "recall@3": avg("recall@3", cdx_results),
            "recall@5": avg("recall@5", cdx_results),
            "recall@10": avg("recall@10", cdx_results),
            "hit@1": avg("hit@1", cdx_results),
            "hit@3": avg("hit@3", cdx_results),
            "hit@5": avg("hit@5", cdx_results),
            "contains_gt": avg("contains_gt", cdx_results),
        },
        "per_task": cdx_results,
    }
    if grep_results:
        report["grep_baseline"] = {
            "recall@1": avg("recall@1", grep_results),
            "recall@5": avg("recall@5", grep_results),
            "recall@10": avg("recall@10", grep_results),
            "hit@1": avg("hit@1", grep_results),
            "hit@5": avg("hit@5", grep_results),
            "contains_gt": avg("contains_gt", grep_results),
        }

    json_path = RESULTS_DIR / "swe_bench_lite_eval.json"
    json_path.write_text(json.dumps(report, indent=2))
    print(f"\nResults saved to: {json_path}")

    # Markdown report
    md_lines = [
        "# SWE-bench Lite Localization Evaluation\n",
        f"**Date:** {time.strftime('%Y-%m-%d %H:%M')}",
        f"**Tasks:** {n} / 300",
        f"**Retriever:** Codixing BM25-only (no embeddings)",
        f"**Total time:** {total_ms // 1000}s\n",
        "## Results\n",
        "| Metric | Codixing |" + (" grep |" if grep_results else ""),
        "|--------|----------|" + ("------|" if grep_results else ""),
    ]
    for metric in ["recall@1", "recall@3", "recall@5", "recall@10", "hit@1", "hit@3", "hit@5", "contains_gt"]:
        c = avg(metric, cdx_results)
        line = f"| {metric} | {c:.1%} |"
        if grep_results and metric in grep_results[0]:
            g = avg(metric, grep_results)
            line += f" {g:.1%} |"
        md_lines.append(line)

    md_lines.append("\n## Per-Repo Recall@5\n")
    md_lines.append("| Repo | Recall@5 | Tasks |")
    md_lines.append("|------|----------|-------|")
    for repo in sorted(by_repo):
        vals = by_repo[repo]
        md_lines.append(f"| {repo} | {sum(vals)/len(vals):.1%} | {len(vals)} |")

    md_path = RESULTS_DIR / "swe_bench_lite_eval.md"
    md_path.write_text("\n".join(md_lines))
    print(f"Report saved to: {md_path}")


if __name__ == "__main__":
    main()

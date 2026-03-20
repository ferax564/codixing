# SweRankEmbed Full File Retriever Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Use SweRankEmbed as a full file-level retriever (not just outline reranker) in the SWE-bench eval, potentially closing the gap from 48.7% to ~66% R@1.

**Architecture:** Add `--embed-retrieve` flag to `swe_bench_eval.py` that pre-encodes all non-test .py files per repo with SweRankEmbed, caches embeddings by content hash, and fuses the resulting file ranking with BM25 via RRF. Runs alongside the existing BM25 pipeline — no Rust changes needed.

**Tech Stack:** Python, sentence-transformers, numpy. Existing `swe_bench_eval.py` script.

---

### Task 1: Add embed cache infrastructure and `--embed-retrieve` CLI flag

**Files:**
- Modify: `benchmarks/swe_bench_eval.py`

**Step 1: Add globals and CLI flag**

After the existing `CE_RERANKER_NAME` global (line 94), add:

```python
EMBED_RETRIEVE_NAME = None  # Model name for full file retrieval
EMBED_RETRIEVE_MODEL = None  # Loaded model instance
EMBED_CACHE_DIR = ROOT / "benchmarks" / "embed_cache"
```

In `main()` argparse section (after `--ce-rerank` arg, ~line 823), add:

```python
parser.add_argument(
    "--embed-retrieve",
    metavar="MODEL",
    help="Embedding model for full-file retrieval (e.g. Salesforce/SweRankEmbed-Small)",
)
```

In the globals assignment block (~line 826), add:

```python
global SEARCH_STRATEGY, EMBED_MODEL_NAME, CE_RERANKER_NAME, EMBED_RETRIEVE_NAME
# ... existing assignments ...
EMBED_RETRIEVE_NAME = args.embed_retrieve
```

**Step 2: Add lazy model loader**

After `get_ce_reranker()` (~line 343), add:

```python
def get_embed_retrieve_model():
    """Lazily initialize the embedding retrieval model."""
    global EMBED_RETRIEVE_MODEL
    if EMBED_RETRIEVE_MODEL is None and EMBED_RETRIEVE_NAME:
        from sentence_transformers import SentenceTransformer
        print(f"  [embed-retrieve] Loading {EMBED_RETRIEVE_NAME}...", flush=True)
        EMBED_RETRIEVE_MODEL = SentenceTransformer(EMBED_RETRIEVE_NAME, trust_remote_code=True)
        print(f"  [embed-retrieve] Ready ({EMBED_RETRIEVE_MODEL.get_sentence_embedding_dimension()}d).", flush=True)
    return EMBED_RETRIEVE_MODEL
```

**Step 3: Verify flag is parsed**

Run: `python3 benchmarks/swe_bench_eval.py --help | grep embed-retrieve`
Expected: Shows `--embed-retrieve MODEL` in help output.

**Step 4: Commit**

```bash
git add benchmarks/swe_bench_eval.py
git commit -m "feat(bench): add --embed-retrieve CLI flag and cache globals"
```

---

### Task 2: Implement file listing and content-hash caching

**Files:**
- Modify: `benchmarks/swe_bench_eval.py`

**Step 1: Add helper to list non-test .py files**

After the `_is_test_file()` function (~line 87), add:

```python
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
```

**Step 2: Add content-hash embedding cache**

After `get_embed_retrieve_model()`, add:

```python
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

    dims = model.get_sentence_embedding_dimension()
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
            # Placeholder — will be filled after batch encoding
            embeddings.append(None)
            content_text = content_bytes.decode(errors="replace")
            to_encode.append((len(valid_files) - 1, content_text, chash))

    # Batch encode uncached files
    if to_encode:
        texts = [text for _, text, _ in to_encode]
        encoded = model.encode(texts, batch_size=32, normalize_embeddings=True,
                               show_progress_bar=len(texts) > 100)
        for j, (idx, _, chash) in enumerate(to_encode):
            emb = encoded[j]
            embeddings[idx] = emb
            # Cache to disk
            np.save(cache_dir / f"{chash}.npy", emb)

    if not embeddings or all(e is None for e in embeddings):
        return [], np.array([])

    return valid_files, np.stack(embeddings)
```

**Step 3: Smoke test caching logic**

Run a quick manual test:

```bash
python3 -c "
import sys; sys.path.insert(0, 'benchmarks')
# Just verify the imports and functions exist
exec(open('benchmarks/swe_bench_eval.py').read().split('def main')[0])
print('_list_py_files exists:', callable(_list_py_files))
print('_get_cached_embeddings exists:', callable(_get_cached_embeddings))
print('_file_content_hash exists:', callable(_file_content_hash))
"
```
Expected: All three print True.

**Step 4: Commit**

```bash
git add benchmarks/swe_bench_eval.py
git commit -m "feat(bench): add file listing and content-hash embedding cache"
```

---

### Task 3: Implement `embed_retrieve_files()` — the core retriever

**Files:**
- Modify: `benchmarks/swe_bench_eval.py`

**Step 1: Add the retrieval function**

After `_get_cached_embeddings()`, add:

```python
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

    # Get/compute embeddings for all files
    valid_files, file_embs = _get_cached_embeddings(repo_path, repo_name, files)
    if len(valid_files) == 0:
        return []

    # Encode query
    q_emb = model.encode([query], prompt_name="query", normalize_embeddings=True)

    # Cosine similarity (embeddings are already normalized)
    scores = (q_emb @ file_embs.T)[0]

    # Sort by score descending
    scored = sorted(zip(valid_files, scores.tolist()), key=lambda x: -x[1])
    return scored[:top_k]
```

**Step 2: Commit**

```bash
git add benchmarks/swe_bench_eval.py
git commit -m "feat(bench): add embed_retrieve_files() full-file retriever"
```

---

### Task 4: Integrate retriever into the eval loop with RRF fusion

**Files:**
- Modify: `benchmarks/swe_bench_eval.py`

**Step 1: Add embed retrieval call in `search_codixing_multi()`**

This is the critical integration. We need to add a new parameter for the file list and fuse embed retrieval results with BM25. Modify the function signature and add the fusion logic.

Change `search_codixing_multi` signature (line ~519) from:

```python
def search_codixing_multi(repo_path: Path, problem: str) -> list[str]:
```

to:

```python
def search_codixing_multi(repo_path: Path, problem: str, repo_name: str = "") -> list[str]:
```

Then, **after** the existing embed reranking block (after line 732, before the CE reranking block at line 734), add the embed retrieval fusion:

```python
    # ── Optional: full-file embedding retrieval ──
    if EMBED_RETRIEVE_NAME:
        all_py_files = _list_py_files(repo_path)
        if all_py_files:
            embed_results = embed_retrieve_files(
                repo_path, repo_name, problem, all_py_files, top_k=20
            )
            if embed_results:
                # RRF fusion: combine BM25 ranking with embed ranking
                bm25_rank = {f: i for i, f in enumerate(final[:20])}
                embed_rank = {f: i for i, (f, _) in enumerate(embed_results)}
                k = 60
                rrf_scores = {}
                all_candidates = set(list(bm25_rank.keys()) + list(embed_rank.keys()))
                for f in all_candidates:
                    bm25_rrf = 1.0 / (k + bm25_rank.get(f, 100))
                    embed_rrf = 1.0 / (k + embed_rank.get(f, 100))
                    rrf_scores[f] = bm25_rrf + 2.0 * embed_rrf
                fused = sorted(rrf_scores.keys(), key=lambda f: -rrf_scores[f])
                # Preserve remaining files beyond top-20
                seen = set(fused)
                for f in final:
                    if f not in seen:
                        fused.append(f)
                        seen.add(f)
                final = fused
```

**Step 2: Update the call site in `main()` (~line 895)**

Change:

```python
cdx_files = search_codixing_multi(repo_path, problem)
```

to:

```python
cdx_files = search_codixing_multi(repo_path, problem, repo_name=name)
```

**Step 3: Commit**

```bash
git add benchmarks/swe_bench_eval.py
git commit -m "feat(bench): integrate full-file embed retrieval with RRF fusion"
```

---

### Task 5: Run 30-task eval to validate

**Step 1: Run 30-task test with embed retrieval only (no outline rerank)**

```bash
python3 benchmarks/swe_bench_eval.py \
    --limit 30 --skip-clone --skip-grep \
    --embed-retrieve "Salesforce/SweRankEmbed-Small"
```

Expected: First run encodes all files (~3 min for astropy+django), subsequent runs use cache. R@1 should be meaningfully different from baseline 53.3%.

**Step 2: Run 30-task test with both embed retrieval AND outline rerank**

```bash
python3 benchmarks/swe_bench_eval.py \
    --limit 30 --skip-clone --skip-grep \
    --embed-rerank "Salesforce/SweRankEmbed-Small" \
    --embed-retrieve "Salesforce/SweRankEmbed-Small"
```

**Step 3: Run 30-task baseline for comparison (no embed)**

```bash
python3 benchmarks/swe_bench_eval.py \
    --limit 30 --skip-clone --skip-grep
```

**Step 4: Compare results**

| Config | R@1 | R@5 | R@10 |
|---|---|---|---|
| BM25 only | ? | ? | ? |
| BM25 + outline rerank | ? | ? | ? |
| BM25 + full retrieval | ? | ? | ? |
| BM25 + outline + full | ? | ? | ? |

**Step 5: If R@1 improves, run full 300 tasks**

```bash
python3 benchmarks/swe_bench_eval.py \
    --skip-clone --skip-grep \
    --embed-retrieve "Salesforce/SweRankEmbed-Small"
```

This will take ~1 hour (cache building for 12 repos + 300 task evals).

---

### Task 6: Tune RRF alpha and measure standalone embed retrieval

Only do this if Task 5 results are promising.

**Step 1: Add `--embed-alpha` flag**

In argparse:

```python
parser.add_argument("--embed-alpha", type=float, default=2.0,
                    help="Weight for embed retrieval in RRF fusion (default: 2.0)")
```

Use it in the fusion block instead of hardcoded `2.0`.

**Step 2: Test alpha sweep on 30 tasks**

```bash
for alpha in 1.0 2.0 3.0 5.0; do
    echo "=== alpha=$alpha ==="
    python3 benchmarks/swe_bench_eval.py \
        --limit 30 --skip-clone --skip-grep \
        --embed-retrieve "Salesforce/SweRankEmbed-Small" \
        --embed-alpha $alpha 2>&1 | tail -6
done
```

**Step 3: Test standalone SweRankEmbed (no BM25 fusion)**

Add a quick hack: if `--embed-alpha 999`, skip BM25 entirely and return only embed results. This tells us the pure SweRankEmbed ceiling — should be close to SweRank paper's 66.4%.

**Step 4: Commit with best alpha**

```bash
git add benchmarks/swe_bench_eval.py
git commit -m "feat(bench): add --embed-alpha flag for RRF weight tuning"
```

---

### Task 7: Update memory and documentation with results

**Step 1: Update MEMORY.md with findings**

Add embed retrieval results (R@1, R@5, best alpha, standalone vs fused, cold/warm cache times).

**Step 2: If results justify it, update README.md and docs/index.html benchmarks**

**Step 3: Final commit**

```bash
git add -A
git commit -m "docs: update benchmarks with SweRankEmbed full retrieval results"
```

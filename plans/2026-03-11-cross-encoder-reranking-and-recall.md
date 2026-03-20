# Cross-Encoder Reranking + Recall Improvements

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Push SWE-bench Lite R@1 from 48.0% toward 55-60% via cross-encoder reranking of top-5 candidates, and improve recall (75.3% CGT) by fixing the 74 complete misses with better import-chain tracing and `__init__.py` resolution.

**Architecture:** Two-stage reranking pipeline: (1) existing BM25 + outline-embed produces top-20, (2) cross-encoder scores (query, file_content) pairs for top-5 to pick #1. Recall improvements via static analysis heuristics in the multi-query search function.

**Tech Stack:** Python, sentence-transformers CrossEncoder, AST import tracing, SWE-bench eval script

---

## Current State

| Metric | Value | Note |
|--------|-------|------|
| R@1 | 48.0% | 144/300 correct at rank 1 |
| R@5 | 71.0% | 213/300 correct in top 5 |
| R@10 | 74.3% | 223/300 correct in top 10 |
| CGT | 75.3% | 226/300 gold file anywhere in output |
| Reranking opportunities | 69 | Gold in top-5 but not #1 |
| Complete misses | 74 | Gold not in top-20 at all |
| Speed | 8.3s/task | BM25 + outline embed |

### Gap Analysis

**Reranking (69 tasks):** If we pick the right file from top-5, R@1 would reach 71.0%. Cross-encoder should capture much of this since it sees full file content.

**Recall misses (74 tasks) breakdown:**
- django/django: 32 misses (28% miss rate) — many are `__init__.py`, migrations, backends
- sympy/sympy: 21 misses (27%) — many are `sympy/core/` files
- sphinx-doc/sphinx: 6 misses (38%)
- matplotlib/matplotlib: 5 misses (22%)
- All 74 misses have gold file NOT in top-20 — true retrieval failures

**Gold file patterns in misses:**
- 8 migration files (`django/contrib/auth/migrations/...`)
- 7 `sympy/core/` files
- 3 `__init__.py` files
- 3 backend files (`django/db/backends/...`)

---

## Task 1: Add Cross-Encoder Reranking

Add a cross-encoder that scores (problem_statement, file_content) pairs for the top-5 BM25+embed candidates. Use `cross-encoder/ms-marco-MiniLM-L-6-v2` (22M params, ~150ms for 5 pairs on CPU).

**Files:**
- Modify: `benchmarks/swe_bench_eval.py`

**Step 1: Add cross-encoder model loading**

Add after the existing `get_embed_model()` function (~line 430):

```python
RERANKER_MODEL = None
RERANKER_MODEL_NAME = None

def get_reranker_model():
    """Load cross-encoder reranker model (lazy singleton)."""
    global RERANKER_MODEL
    if RERANKER_MODEL is not None:
        return RERANKER_MODEL
    model_name = RERANKER_MODEL_NAME
    if not model_name:
        return None
    from sentence_transformers import CrossEncoder
    print(f"  Loading reranker: {model_name}...")
    RERANKER_MODEL = CrossEncoder(model_name)
    return RERANKER_MODEL
```

**Step 2: Add cross-encoder reranking function**

Add after `embed_rerank_files()`:

```python
def cross_encoder_rerank(
    repo_path: Path, query: str, files: list[str], top_k: int = 5
) -> list[str]:
    """Rerank top-k files using cross-encoder on (query, file_content) pairs.

    Reads actual file content (first ~2000 chars) and scores with cross-encoder.
    Returns files sorted by cross-encoder score.
    """
    model = get_reranker_model()
    if not model or not files:
        return files

    candidates = files[:top_k]
    pairs = []
    valid_files = []
    for fp in candidates:
        full_path = repo_path / fp
        try:
            content = full_path.read_text(errors="replace")[:2000]
        except OSError:
            continue
        # Prepend file path for context
        doc = f"{fp}\n{content}"
        pairs.append((query[:1000], doc))
        valid_files.append(fp)

    if not pairs:
        return files

    scores = model.predict(pairs)
    # Sort by score descending
    scored = sorted(zip(valid_files, scores), key=lambda x: -x[1])
    reranked = [f for f, _ in scored]

    # Append remaining files that weren't reranked
    seen = set(reranked)
    for f in files:
        if f not in seen:
            reranked.append(f)
            seen.add(f)
    return reranked
```

**Step 3: Add `--reranker` CLI argument**

In the `argparse` section (~line 750), add:

```python
parser.add_argument("--reranker", type=str, default=None,
                    help="Cross-encoder model for reranking top-5 (e.g. cross-encoder/ms-marco-MiniLM-L-6-v2)")
```

And in the main function, after arg parsing:

```python
global RERANKER_MODEL_NAME
RERANKER_MODEL_NAME = args.reranker
```

**Step 4: Integrate cross-encoder into search_codixing_multi()**

In `search_codixing_multi()`, after the existing embed reranking block (~line 711, after `final = sorted(rrf_scores.keys(), ...)`), add:

```python
    # ── Optional: cross-encoder reranking of top-5 ──
    if RERANKER_MODEL_NAME and final:
        non_test_top5 = [f for f in final[:10]
                         if "/test" not in f and not f.split("/")[-1].startswith("test_")][:5]
        if non_test_top5:
            reranked_top = cross_encoder_rerank(repo_path, problem, non_test_top5, top_k=5)
            # Merge: cross-encoder top-5 first, then remaining in original order
            seen = set(reranked_top)
            rest = [f for f in final if f not in seen]
            final = reranked_top + rest
```

**Step 5: Run 30-task test with cross-encoder**

```bash
python3 benchmarks/swe_bench_eval.py --limit 30 \
  --embed-rerank "Salesforce/SweRankEmbed-Small" \
  --reranker "cross-encoder/ms-marco-MiniLM-L-6-v2"
```

Expected: R@1 should improve by 3-8pp over embed-only (from ~53% to ~56-61% on 30 tasks).
Speed: should add <1s per task (5 pairs × ~30ms each).

**Step 6: Test with larger cross-encoder if MiniLM is too weak**

```bash
python3 benchmarks/swe_bench_eval.py --limit 30 \
  --embed-rerank "Salesforce/SweRankEmbed-Small" \
  --reranker "BAAI/bge-reranker-base"
```

This is 278M params, ~500ms for 5 pairs. If accuracy is meaningfully better, use it.

**Step 7: Commit**

```bash
git add benchmarks/swe_bench_eval.py
git commit -m "feat(bench): add cross-encoder reranking for top-5 candidates"
```

---

## Task 2: Improve Import-Chain Recall

Many misses happen because the gold file is not directly mentioned in the issue — it's reached via import chains. For example, issue mentions `django.db.models.lookups` but gold file is `django/db/models/lookups.py` which isn't found because the issue only describes the *behavior*, not the file.

Strategy: For top-5 BM25 files, trace their imports to find sibling/parent modules that might contain the actual fix location.

**Files:**
- Modify: `benchmarks/swe_bench_eval.py`

**Step 1: Add import extraction function**

Add after `cross_encoder_rerank()`:

```python
def extract_imports(repo_path: Path, filepath: str) -> list[str]:
    """Extract imported module file paths from a Python file."""
    import ast
    full_path = repo_path / filepath
    try:
        source = full_path.read_text(errors="replace")
        tree = ast.parse(source)
    except (OSError, SyntaxError, UnicodeDecodeError):
        return []

    imported_files = []
    for node in ast.walk(tree):
        if isinstance(node, ast.ImportFrom) and node.module:
            # Convert dotted module to file path candidates
            parts = node.module.split(".")
            for end in range(len(parts), 0, -1):
                prefix = "/".join(parts[:end])
                for suffix in (prefix + ".py", prefix + "/__init__.py"):
                    if (repo_path / suffix).exists():
                        imported_files.append(suffix)
                        break
                else:
                    continue
                break
    return imported_files
```

**Step 2: Add import-chain expansion to search_codixing_multi()**

After the dotted path block and before the RRF section, add:

```python
    # ── Import-chain expansion ──
    # For top-10 scored files, find what they import and boost those modules
    top_scored = sorted(file_scores.items(), key=lambda x: -x[1])[:10]
    import_boost_seen = set()
    for fp, score in top_scored:
        if not fp.endswith(".py") or "/test" in fp:
            continue
        imported = extract_imports(repo_path, fp)
        for imp_fp in imported[:5]:  # limit per file
            if imp_fp not in import_boost_seen and imp_fp not in file_scores:
                import_boost_seen.add(imp_fp)
                # Moderate boost — these are one hop away from a high-scoring file
                file_scores[imp_fp] += score * 0.3
```

**Step 3: Run 30-task test (BM25 + import chains, no embed)**

```bash
python3 benchmarks/swe_bench_eval.py --limit 30
```

Compare CGT (contains_gt) vs baseline. Import chains should help for files like `django/db/models/lookups.py` that are imported by `django/db/models/sql/query.py` (which IS found).

**Step 4: Commit**

```bash
git add benchmarks/swe_bench_eval.py
git commit -m "feat(bench): add import-chain expansion for recall improvement"
```

---

## Task 3: Better `__init__.py` and Package Resolution

8 of the 74 misses involve migration files and 3 involve `__init__.py`. Improve resolution of package paths.

**Files:**
- Modify: `benchmarks/swe_bench_eval.py`

**Step 1: Enhance dotted path resolution for `__init__.py`**

In the existing dotted path block, the code tries `prefix.py` and `prefix/__init__.py` but only for exact dotted paths. Add fallback: when a top BM25 result is a directory (e.g., `django/db/models/`), also check `__init__.py` inside it.

After the existing dotted path block, add:

```python
    # ── __init__.py resolution for scored directories ──
    # If we scored "django/db/models/fields/related.py", also check
    # "django/db/models/fields/__init__.py" since that's often the fix location
    init_candidates = set()
    for fp in list(file_scores.keys()):
        if not fp.endswith(".py"):
            continue
        # Check parent directory's __init__.py
        parent_dir = "/".join(fp.split("/")[:-1])
        init_path = parent_dir + "/__init__.py"
        if init_path not in file_scores and (repo_path / init_path).exists():
            init_candidates.add(init_path)
        # Check grandparent too (for deeply nested packages)
        gparent_dir = "/".join(fp.split("/")[:-2])
        if gparent_dir:
            ginit_path = gparent_dir + "/__init__.py"
            if ginit_path not in file_scores and (repo_path / ginit_path).exists():
                init_candidates.add(ginit_path)

    for init_fp in init_candidates:
        # Moderate boost — parent package often contains the actual definitions
        file_scores[init_fp] += 25.0
```

**Step 2: Add migration file detection**

Some issues explicitly mention migrations. Add heuristic:

```python
    # ── Migration file mentions ──
    # If issue mentions "migration" and a specific app, find its migrations
    if re.search(r'\bmigrat', problem, re.IGNORECASE):
        # Look for app names mentioned alongside "migration"
        migration_context = re.findall(
            r'(\w+)/migrations/|migrations.*?(\w+)|(\w+).*?migration',
            problem[:2000]
        )
        for groups in migration_context[:3]:
            app_name = next((g for g in groups if g), None)
            if not app_name or len(app_name) < 3:
                continue
            # Find migration files for this app
            mig_dir = None
            find_r = subprocess.run(
                ["find", ".", "-path", f"*/{app_name}/migrations/*.py",
                 "-not", "-name", "__init__.py"],
                capture_output=True, timeout=5, cwd=str(repo_path),
            )
            for line in find_r.stdout.decode(errors="replace").strip().split("\n"):
                fp = line.strip().lstrip("./")
                if fp and fp.endswith(".py"):
                    file_scores[fp] += 15.0
```

**Step 3: Run 30-task test**

```bash
python3 benchmarks/swe_bench_eval.py --limit 30
```

Check if CGT improved.

**Step 4: Commit**

```bash
git add benchmarks/swe_bench_eval.py
git commit -m "feat(bench): add __init__.py resolution and migration file detection"
```

---

## Task 4: Full 300-Task Evaluation

Run the combined improvements (cross-encoder + import chains + __init__.py + migration) on all 300 tasks.

**Step 1: Run full eval with all improvements**

```bash
nohup python3 -u benchmarks/swe_bench_eval.py \
  --embed-rerank "Salesforce/SweRankEmbed-Small" \
  --reranker "cross-encoder/ms-marco-MiniLM-L-6-v2" \
  > /tmp/swe_v10_300.log 2>&1 &
```

Monitor with: `tail -f /tmp/swe_v10_300.log`

Expected targets:
- R@1: 48.0% → 53-58% (cross-encoder reranking + heuristics)
- R@5: 71.0% → 72-74% (import chains + __init__.py)
- CGT: 75.3% → 77-79% (recall improvements)
- Speed: <12s/task (8.3s BM25/embed + ~1s cross-encoder + ~2s import tracing)

**Step 2: Compare and save results**

```bash
python3 -c "
# Parse results from log and compare with baseline
"
```

**Step 3: If results are positive, update README.md and docs/index.html**

Update the SWE-bench numbers in:
- `README.md` (~line 339-348)
- `docs/index.html` (~line 1322-1334)

**Step 4: Update MEMORY.md with findings**

---

## Risk Assessment

| Risk | Mitigation |
|------|-----------|
| Cross-encoder hurts R@1 on some tasks | Test on 30 tasks first; rerank only top-5 non-test files |
| Import chains add too many noise files | Limit to top-10 scored files, max 5 imports each, 0.3× score |
| `__init__.py` boost is too aggressive | Use moderate boost (25.0) — lower than traceback (70.0) or file mentions (80.0) |
| Speed regression | Cross-encoder ~150ms for 5 pairs; import tracing ~100ms; total <1s extra |
| Migration heuristic false positives | Only trigger when "migrat" is in issue text |

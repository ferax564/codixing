# SWE-bench v9: Fast Embed Reranking + BM25 Recall Improvements

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Close the R@1 gap from 47.7% to ~55-60% by making embed reranking fast enough for 300-task runs and improving BM25 recall for the 74 complete misses.

**Architecture:** Outline-only embed reranking (0.65s per task, not 40s) with weighted RRF, plus BM25 heuristic improvements for error-message grep, better __init__.py resolution, and import-chain tracing.

**Tech Stack:** Python, SweRankEmbed-Small (sentence-transformers), AST parsing, BM25 (codixing CLI)

---

## Current State

| Version | R@1 | R@5 | R@10 | CGT | Speed |
|---------|-----|-----|------|-----|-------|
| BM25-only (latest, saved) | 47.7% | 71.7% | 74.3% | 75.3% | ~53s/task |
| v6 (func embed + equal RRF) | 44.7% | 66.7% | 69.0% | 73.0% | ~240s/task |
| v8 (two-tier + 2x RRF, 30-task) | 53.3% | 66.7% | 66.7% | 66.7% | ~75s/task |
| SweRank SOTA | 66.4% | — | — | — | GPU |

Key findings:
- **71.7% R@5 → 47.7% R@1** = 72 tasks where gold is in top-5 but not #1 (reranking opportunity)
- **24.7% complete misses** = 74 tasks where gold is not even in top-10
- Outline embedding (200ch) runs at 31/s = **0.65s for 20 files**
- Full function embedding (1500ch) runs at 3/s = **40s for 120 functions** (too slow)
- Two-tier helped accuracy but tier-2 killed speed

## Plan

### Task 1: Outline-Only Embed Reranking (fast v9)

Drop tier-2 (function-level) entirely. Use outline-only embedding with weighted RRF.
Speed target: <2s overhead per task (vs 40s for tier-2).

**Files:**
- Modify: `benchmarks/swe_bench_eval.py` — `embed_rerank_files()` function

**Step 1: Simplify embed_rerank_files to outline-only**

Replace the two-tier function with outline-only:
```python
def embed_rerank_files(repo_path, query, files, top_k=20):
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
```

**Step 2: Run 30-task test**

```bash
python3 benchmarks/swe_bench_eval.py --limit 30 --embed-rerank "Salesforce/SweRankEmbed-Small"
```
Expected: ~55s/task (53s BM25 + 2s embed), R@1 should be >= BM25-only baseline.

**Step 3: If 30-task looks good, run full 300-task eval**

```bash
nohup python3 -u benchmarks/swe_bench_eval.py --embed-rerank "Salesforce/SweRankEmbed-Small" > /tmp/swe_v9_300.log 2>&1 &
```
Expected: ~5h for 300 tasks.

---

### Task 2: Error Message Grep Heuristic

Many missed tasks include error messages like `"TypeError: foo() got an unexpected keyword argument"`. Grep for distinctive error strings to find where they're raised.

**Files:**
- Modify: `benchmarks/swe_bench_eval.py` — inside `search_codixing_multi()`

**Step 1: Add error message extraction and grep**

After the traceback file extraction block, add:
```python
# ── Error message grep ──
# Extract distinctive error messages and grep for them in source
error_patterns = re.findall(
    r'(?:raise\s+\w+|Error|Exception|Warning)\s*\(\s*["\']([^"\']{15,80})["\']',
    problem
)
# Also extract quoted error message strings
error_patterns += re.findall(r'"([^"]{15,80})"', problem[:2000])
error_patterns += re.findall(r"'([^']{15,80})'", problem[:2000])
# Filter to likely error messages (contain format specifiers or look error-like)
seen_err = set()
for ep in error_patterns[:8]:
    ep_clean = ep.strip()
    if len(ep_clean) < 15 or ep_clean in seen_err:
        continue
    # Skip URLs, paths, code snippets
    if ep_clean.startswith("http") or "/" in ep_clean[:5]:
        continue
    seen_err.add(ep_clean)
    # Grep for this string in the repo
    grep_result = subprocess.run(
        ["grep", "-rl", ep_clean[:60], "--include=*.py", "."],
        capture_output=True, timeout=5, cwd=str(repo_path),
    )
    for line in grep_result.stdout.decode(errors="replace").strip().split("\n"):
        fp = line.strip().lstrip("./")
        if fp and fp.endswith(".py") and "/test" not in fp:
            file_scores[fp] += 30.0
```

**Step 2: Run 30-task test (BM25-only, no embed)**

```bash
python3 benchmarks/swe_bench_eval.py --limit 30
```
Compare R@5 and CGT vs baseline.

---

### Task 3: Better __init__.py and Module Resolution

Gold files like `django/db/models/fields/__init__.py` get missed because dotted paths resolve to non-existent `.py` files. Improve the fallback.

**Files:**
- Modify: `benchmarks/swe_bench_eval.py` — dotted path resolution block

**Step 1: Add package __init__.py resolution for mentioned classes**

After the dotted path block, add:
```python
# ── Class/symbol → module __init__.py resolution ──
# When issue mentions "Field" or "QuerySet", find where they're defined
# Many are in __init__.py files of their package
backtick_symbols = re.findall(r"`([A-Z][a-zA-Z]{2,30})`", problem)
for sym in backtick_symbols[:5]:
    if sym.lower() in STOPWORDS:
        continue
    fp = search_codixing_find_symbol(repo_path, sym)
    if fp and fp.endswith(".py") and "/test" not in fp:
        file_scores[fp] += 35.0
```

This extends the existing code block symbol lookup (which only looks at `def/class` in code blocks) to also look at backtick-quoted CamelCase symbols anywhere in the issue text.

---

### Task 4: Full 300-Task Eval with All Improvements

Run the combined improvements (outline embed + error grep + symbol resolution) on all 300 tasks.

**Step 1: Run full eval**

```bash
nohup python3 -u benchmarks/swe_bench_eval.py --embed-rerank "Salesforce/SweRankEmbed-Small" > /tmp/swe_v9_300.log 2>&1 &
```

**Step 2: Parse results and compare**

Expected improvement targets:
- R@1: 47.7% → 52-55% (outline reranking + heuristics)
- R@5: 71.7% → 73-75% (heuristic recall improvement)
- Speed: <60s/task (vs 75s for v8, 240s for v6)

**Step 3: Update README.md and docs/index.html with new numbers**

**Step 4: Update MEMORY.md with findings**

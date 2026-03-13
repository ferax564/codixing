# SWE-bench Lite Localization Evaluation

**Date:** 2026-03-13
**Tasks:** 300 / 300
**Retriever:** Codixing BM25 + SweRankEmbed outline reranking
**Total time:** ~2550s (8.5s/task avg)

## Results

| Metric | BM25-only | +Outline Rerank | grep baseline |
|--------|-----------|-----------------|---------------|
| recall@1 | 44.3% | **48.7%** | 14.7% |
| recall@3 | 59.0% | — | — |
| recall@5 | 66.7% | **74.3%** | 41.3% |
| recall@10 | 74.7% | **77.3%** | 54.7% |
| contains_gt | 78.7% | **78.7%** | 64.7% |

## Outline Reranking (SweRankEmbed-Small)

Uses `extract_file_outline()` to build compact signatures (path + class/def headers, ~800ch),
then reranks BM25 top-20 via cosine similarity with RRF fusion (embed weight 2.0x).

- Model: Salesforce/SweRankEmbed-Small (137M params, 768d)
- +4.4pp R@1, +7.6pp R@5 over BM25-only

## Per-Repo Recall@5 (BM25-only)

| Repo | Recall@5 | Tasks |
|------|----------|-------|
| astropy/astropy | 83.3% | 6 |
| django/django | 65.8% | 114 |
| matplotlib/matplotlib | 69.6% | 23 |
| mwaskom/seaborn | 100.0% | 4 |
| pallets/flask | 100.0% | 3 |
| psf/requests | 50.0% | 6 |
| pydata/xarray | 80.0% | 5 |
| pylint-dev/pylint | 33.3% | 6 |
| pytest-dev/pytest | 52.9% | 17 |
| scikit-learn/scikit-learn | 91.3% | 23 |
| sphinx-doc/sphinx | 62.5% | 16 |
| sympy/sympy | 62.3% | 77 |

## Negative Results (2026-03-13)

### Full-file SweRankEmbed Retrieval (CPU, max_seq_length=512)

Encodes all non-test .py files per repo with SweRankEmbed-Small, fuses with BM25 via RRF.

| Config | R@1 | R@5 | R@10 | Tasks |
|--------|-----|-----|------|-------|
| BM25-only (baseline) | 44.3% | 66.7% | 74.7% | 300 |
| +full retrieve α=3.0 | 44.1% | 64.4% | 71.2% | 295 |
| +outline + retrieve α=1.0 | ~43.6% | ~76.1% | ~85.3% | 163 (partial) |

**Conclusion:** With max_seq_length=512 (CPU constraint), files are truncated to ~100 lines
(imports + class headers) — essentially the same information as outline reranking. The SweRank
paper achieves 66.4% R@1 using the Large model with 8192 token context on GPU.
Infrastructure kept (`--embed-retrieve`, `--embed-alpha`) for future GPU testing.

### Cross-Encoder Reranking (4 models tested)

All cross-encoder models hurt R@1. Text relevance ≠ code localization.
See MEMORY.md for details. Do not revisit.

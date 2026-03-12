# SWE-bench Lite Localization Evaluation

**Date:** 2026-03-12 16:40
**Tasks:** 300 / 300
**Retriever:** Codixing BM25-only (no embeddings)
**Total time:** 2549s

## Results

| Metric | Codixing | grep |
|--------|----------|------|
| recall@1 | 48.0% | 14.7% |
| recall@3 | 66.3% |
| recall@5 | 71.3% | 41.3% |
| recall@10 | 74.7% | 54.7% |
| hit@1 | 48.0% | 14.7% |
| hit@3 | 66.3% |
| hit@5 | 71.3% | 41.3% |
| contains_gt | 75.7% | 64.7% |

## Per-Repo Recall@5

| Repo | Recall@5 | Tasks |
|------|----------|-------|
| astropy/astropy | 83.3% | 6 |
| django/django | 71.1% | 114 |
| matplotlib/matplotlib | 73.9% | 23 |
| mwaskom/seaborn | 100.0% | 4 |
| pallets/flask | 100.0% | 3 |
| psf/requests | 66.7% | 6 |
| pydata/xarray | 80.0% | 5 |
| pylint-dev/pylint | 33.3% | 6 |
| pytest-dev/pytest | 58.8% | 17 |
| scikit-learn/scikit-learn | 91.3% | 23 |
| sphinx-doc/sphinx | 56.2% | 16 |
| sympy/sympy | 70.1% | 77 |
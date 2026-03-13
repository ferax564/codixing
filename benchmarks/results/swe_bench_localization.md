# SWE-bench Style Localization Benchmark

**Date:** 2026-03-08 15:50
**Tasks:** 7

## Results

| Task | grep File% | grep Sym% | Cdx File% | Cdx Sym% | grep Bytes | Cdx Bytes |
|------|-----------|-----------|-----------|----------|-----------|-----------|
| QuerySet.count() issues with annotations | 100% | 0% | 100% | 50% | 51,743 | 7,119 |
| CSRF middleware breaks with async views | 100% | 0% | 100% | 33% | 21,427 | 7,135 |
| Migration squashing loses RunPython oper | 100% | 33% | 100% | 100% | 77,250 | 5,648 |
| Task budget unfairness in multi-threaded | 100% | 33% | 100% | 100% | 31,263 | 4,647 |
| select! macro doesn't properly drop futu | 100% | 100% | 100% | 100% | 38,341 | 5,374 |
| useEffect cleanup runs after component u | 100% | 0% | 100% | 50% | 346,629 | 6,120 |
| Hydration mismatch with useId in streami | 100% | 0% | 100% | 33% | 102,135 | 6,222 |

## Aggregate

- **File localization**: grep 100% vs Codixing 100%
- **Symbol localization**: grep 24% vs Codixing 67%
- **Context efficiency**: grep 668,788B vs Codixing 42,265B
- **Byte savings**: 94%
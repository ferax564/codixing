# Codixing Agent Benchmark Report

**Date:** 2026-03-28 16:15
**Model:** claude-sonnet-4-6
**Runs per task per condition:** 1

## Summary

| Metric | Vanilla (mean) | Codixing (mean) | Reduction |
|--------|----------------|-----------------|-----------|
| Tool calls | 21.0 | 12.0 | **43% fewer** |
| Tokens | 2,382 | 3,596 | **-51% fewer** |
| Wall time | 81.1s | 62.8s | **23% faster** |
| Pass rate | 100% | 100% | **+0%** |

## Per-Task Results

| Task | Repo | Category | V Calls (mean+/-std) | C Calls (mean+/-std) | Call Reduction | Significant? |
|------|------|----------|----------------------|----------------------|----------------|--------------|
| agent-codixing-1 | codixing | symbol_lookup | 21.0 +/- 0.0 | 12.0 +/- 0.0 | 43% | N/A |

## Cost

**Total:** $0.48
**Per session:** $0.238
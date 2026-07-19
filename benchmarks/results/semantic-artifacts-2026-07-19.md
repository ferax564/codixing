# Bounded semantic artifact benchmark — 2026-07-19

## Method

- Platform: macOS arm64.
- Corpus: two clean, identical archives of Codixing commit `a52dd96` (349
  indexed files, 3,468 chunks, 5,156 symbols).
- Configuration: BM25-only `codixing init .`; no vectors or model startup.
- Baseline: optimized pre-change CLI (`/tmp/codixing-root-cli`).
- Candidate: optimized CLI from `codex/bounded-semantic-artifacts`.
- Peak RSS: `/usr/bin/time -l`; artifact bytes: `stat`; total allocated index
  blocks: `du -sk`.
- Repetitions: three fresh archive extractions per binary; the table reports
  medians. Candidate artifact bytes were identical in all three runs.

## Self-repository result

| Metric | Baseline | Bounded v2 | Change |
|---|---:|---:|---:|
| Engine-reported init time | 1.80 s | 0.91 s | **1.98× faster** |
| Maximum resident set size | 399,212,544 B | 187,449,344 B | **-53.0%** |
| `concepts.bin` | 13,751,213 B | 397,267 B | **-97.1%** |
| `reformulations.bin` | 6,502,333 B | 159,264 B | **-97.6%** |
| Both semantic artifacts | 20,253,546 B | 556,531 B | **-97.3%** |
| Complete `.codixing` allocated size | 45,768 KiB | 26,560 KiB | **-42.0%** |

The semantic files exceed the requested 50% reduction independently and in
aggregate. The complete index improves by 42.0% on this small corpus because the
unchanged Tantivy, graph, symbol, trigram, and metadata files form a 26 MiB
floor; those stores are measured by the separate dense-storage benchmark.

## Pathological synthetic persistence fixtures

The unit fixtures deliberately repeat the same 32 symbols / 16 paths across
256 concept clusters and the same 12 expansions across thousands of learned
terms. They compare the old direct-bitcode representation with the exact v2
bytes written by the new persistence path.

| Fixture | Legacy bytes | Bounded v2 bytes | Change |
|---|---:|---:|---:|
| Concept clusters | 228,611 | 15,889 | **-93.1%** |
| Learned reformulations | 467,876 | 66,858 | **-85.7%** |

The same tests assert deterministic bytes across reversed input order, legacy
decode compatibility, per-cluster/per-term caps, and retention of repeated
high-evidence vocabulary. An integration test pins init → sync modification →
sync removal freshness in memory and after reopen. Semantic construction makes
two streaming symbol-table passes instead of materializing a full-table clone;
concept and reformulation intermediate maps are built and released sequentially.

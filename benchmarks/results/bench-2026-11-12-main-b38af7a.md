# Benchmark run — main @ b38af7a (2026-11-12)

- **Commit:** `b38af7a` — "Fix retrieval, output size, and cold-start cost on large repos (#164)"
- **Workspace version:** 0.48.0. The latest release is v0.48.0 (published 2026-07-23); #164 is merged but not released yet.
- **Host:** cloud Linux container, 4 vCPU, BM25 only (no ONNX). This is a noisy shared runner, so compare these numbers with each other, not with the Apple M4 tables in `CLAUDE.md`.
- **Build:** `cargo build --release --workspace` passed (7m 44s); `cargo fmt --check` passed.
- **CI:** [run #525](https://github.com/ferax564/codixing/actions/runs/43168936748) on this commit passed. All 13 jobs succeeded, including the three release builds.

## 1. Retrieval gate (`benchmarks/retrieval/tasks_self.json`): passed

Token counts are estimates (bytes / 3.2), because `tiktoken` is not installed here.

| Task set | Surface | hit@1 | hit@3 | hit@10 | MRR | Median tokens to answer | Median time |
|---|---|---|---|---|---|---|---|
| Identifier (8) | search | 8/8 | 8/8 | 8/8 | 1.00 | — | — |
| Identifier (8) | ask | 8/8 | 8/8 | 8/8 | 1.00 | 107 | 13 ms |
| Identifier (8) | `rg -nw` baseline | | | | | 5,685 (max 30,123) | 9 ms |
| Natural language (12) | search | 8/12 | 9/12 | 12/12 | 0.73 | 105 | 23 ms |
| Natural language (12) | ask | 8/12 | 9/12 | 12/12 | 0.73 | 177 | 32 ms |

The weakest natural-language queries were "rename a symbol across the codebase" (rank 10 in search, 9 in ask), "add a --json flag to the callers command" (rank 6) and "single writer lease for the index" (rank 6).

## 2. `codixing bench-tokens` (exact cl100k counts)

| Workflow | Codixing | Naive grep + read | Saved |
|---|---|---|---|
| Locate the definition + blast radius of `IndexStore` | 1,896 tokens, 142 ms, answer found in 4/4 steps | 5,092 tokens, 19 ms, answer found in 2/2 steps | **3,196 tokens (62.8%)** |

Gates: `graph --map --token-budget 1500` returned 1,492 tokens. Compact `impact` returned 448 tokens and `impact --full` returned 2,745. `search IndexStore` returned 49 tokens with the definition as the top hit.

## 3. Large-repository gate (`--profile pr --evidence-mode smoke`, 250-file fixture): passed

The checks for correctness, sync correctness, generation layout, provenance, cleanup and doctor all passed. Performance figures are recorded but not gated in this profile.

| Metric | Value |
|---|---|
| init | 99 ms, peak RSS 47 MB |
| Cold query (p50 / p95) | 14.2 / 14.6 ms, RSS 31 MB |
| Warm query round trip (p50 / p95) | 1.2 / 2.1 ms |
| Warm query with 32 concurrent readers (p50 / p95) | 10.1 / 18.5 ms, 0 errors |
| No-op sync | 14.8 ms, 0 bytes rewritten |
| One-file sync | 65 ms |
| Quality | MRR 1.00, Recall@10 1.00 |

## 4. Criterion (`search_bench`, `large_repo_bench`, `vector_bench`): all passed

| Benchmark | Median |
|---|---|
| bm25_search_identifier | 116.2 µs |
| bm25_search_natural_language | 690.7 µs |
| bm25_search_with_file_filter | 61.6 µs |
| detect_strategy_identifier / natural_language / two_words | 6.58 µs / 124.6 ns / 54.0 ns |
| rrf_fuse 10×10 / 50×50 / 100×100 | 4.36 / 24.9 / 49.8 µs |
| sync_noop_20_files | 121.7 µs |
| grep_literal trigram / full scan | 225.7 / 215.0 µs |
| grep_regex trigram / full scan | 447.6 / 394.7 µs |
| grep_or_pattern trigram / full scan | 312.1 / 286.7 µs |
| large_repo_init 1K / 10K files | 1.23 s / 105.0 s |
| large_repo_search 1K / 10K files | 622.8 µs / 10.77 ms |
| large_repo_grep trigram_literal 1K / 10K | 109.3 µs / 3.17 ms |
| large_repo_grep full_scan_literal 1K / 10K | 2.66 ms / 21.99 ms |
| large_repo_grep trigram_regex 1K / 10K | 1.13 ms / 8.54 ms |
| large_repo_grep full_scan_regex 1K / 10K | 599.4 µs / 2.91 ms |
| vector brute force 1K / 5K / 10K / 50K | 279 µs / 1.61 ms / 3.29 ms / 16.2 ms |

On a selective literal, the trigram prefilter is **24× faster** than a full scan at 1K files and **6.9× faster** at 10K files. With regexes that match every file it is about 2–3× slower than a full scan on this host, as the trigram notes in `CLAUDE.md` predict.

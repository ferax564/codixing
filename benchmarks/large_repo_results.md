# Large Repo Benchmark Results

Two complementary harnesses cover large-repository performance:

```bash
# Algorithm-level Criterion measurements.
cargo bench -p codixing-core --bench large_repo_bench

# Fast end-to-end smoke run (not valid for strict 2x/50% claims).
cargo build --release -p codixing -p codixing-server
python3 benchmarks/large_repo_gate.py --profile pr

# Local 10K scale capture. CI adds the pinned same-run baseline and policy.
python3 benchmarks/large_repo_gate.py --profile 10k --threads 8
```

## Results

No hardware-independent result is checked in. CI uploads `criterion-results.txt`
and `large-repo-gate.json` as the `benchmark-results` artifact. Pull requests and
main pushes use 10K files as a regression-only scale surrogate. Weekly and
manually selected 100K runs enter strict-claim mode; they support the
huge-repository claim only when commit-bound external-quality evidence is
provisioned. Manual workflow runs can select the short smoke profile, which
records evidence without evaluating a baseline.

Performance ratios are evaluated only when a same-machine baseline is supplied
to `large_repo_gate.py`; baseline and candidate must also use the same canonical
`--work-dir` parent. Every result must identify a valid Git revision and source
tree digest from a clean checkout. Baseline comparisons currently require
`worker_mode=fixed`, the same positive `requested_threads` and
`effective_worker_threads` for init and sync, and use `--threads 8` in CI.
Explicit worker-pool setup failures abort rather than being recorded as an
effective count. Search and server thread counts are outside this telemetry.
Shipping-default captures retain null effective telemetry for legacy
compatibility and cannot support a strict or regression comparison. An
unbaselined run records evidence without claiming a performance pass. This
avoids turning unattributed source, runner-to-runner variance, or
filesystem-path variance into a fabricated baseline.

See [README.md](README.md#large-repository-performance-gate) for the JSON
contents, quality hooks, and the exact 2x/50% comparison command.

## Targets

- Speed suite: geometric mean of BM25-only initialization, cold-query p95,
  no-op sync, one-file sync, and one-percent sync ratios. The 10K surrogate
  limit is 0.80; only the 100K limit of 0.50 supports the narrower **2x
  geometric-mean on this named BM25-only five-operation suite** claim. Hybrid
  and vector initialization, model memory, and vector-index disk are outside
  this gate and require separate evidence. Every component must also
  remain at or below 1.05 except one-file sync, which has its own relative limit
  of 0.80 at 10K and 0.50 at 100K. A separate absolute non-regression allowance
  is 100ms at 10K and 500ms at 100K; the harness hard-caps it at 500ms.
- Sync sampling: no-op and one-file sync each run five deterministic,
  interleaved repetitions. Latency gates use the median, rewrite gates use the
  maximum, and every raw sample remains in the JSON. Evidence is rejected when
  either timing median absolute deviation exceeds `max(50ms, 10% of median)` or
  its interquartile spread exceeds `max(100ms, 20% of median)`.
- Warm query p95: apply the requested 0.50 ratio until it reaches a 10ms
  absolute floor, while allowing no more than 2ms regression from a faster
  baseline.
- Peak initialization and cold-query RSS: at most 0.75 of baseline at 10K and
  0.50 at 100K; strict one-shot evidence uses direct-child `wait4` peak RSS,
  excludes descendants, and leaves PSS unset. The warm resident server is
  sampled separately by direct PID after requests and may record PSS.
- Peak one-file and one-percent sync RSS: at most 0.75 of baseline at 10K and
  0.50 at 100K.
- Fresh and post-sync deduplicated allocated index bytes: at most 0.50 of
  baseline at both strict scales. The candidate must finish with exactly one
  active generation and no abandoned generations; a clean pinned legacy-flat
  baseline remains comparable.
- Incremental writes: no-op effective writes must remain within the configured
  ceiling; one-file and one-percent effective writes must each be at most 0.50
  of baseline. Effective writes are the maximum of surviving-artifact churn and
  final direct-child `/proc/<pid>/io` writes when Linux exposes them.
- Retrieval: validate every timed cold/warm exact result, retain at least 99% of
  baseline MRR and Recall@10, and meet the absolute MRR/Recall floors. Strict
  mode requires a positive-task-count, dataset-digested, commit-bound external
  result for both baseline and candidate.

All comparisons require the same profile, canonical work parent, hardware
class, OS, explicit fixed worker count, sampling policy, actual fixture manifest, filesystem
allocation geometry, Rust toolchain, build profile, and binary feature label. A
10K pull-request run cannot prove the 100K-file target. Initialization,
one-percent sync, and the server lifecycle are still single-run measurements;
that limitation is recorded rather than hidden behind a universal speed claim.

## Post-0.47.1 hardening notes (2026-07-22)

Branch `feat/ten-out-of-ten-completion` ships incremental correctness/perf fixes that
directly target the failing one-file gate:

1. **CLI sync hash path** now uses `update_tree_hash_delta` + compact threshold
   (same as git/daemon), instead of folding the full `tree_hashes_v2` snapshot on
   every one-file edit.
2. **PageRank** is skipped for tiny structural batches on large repositories
   (edges still persist).
3. **Multi-reader matrix** (concurrency 1/8/32) is recorded under
   `metrics.queries.warm.concurrent_readers` during warm-query measurement.

Local microbench (200-file synthetic, release CLI, one structural append):

| Metric | Value |
|---|---|
| one-file `sync --no-embed` wall | ~146 ms |
| Token visible after sync | yes (`gate_token_xyz`) |

Full 10K same-run baseline comparison should be re-run on CI hardware before
claiming the 50% outcome gates closed. Query p95 was already far under target on
the 2026-07-19 capture; rewrite/latency should improve materially from (1)–(2).


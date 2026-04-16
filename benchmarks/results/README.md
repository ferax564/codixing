# Benchmark Results Freshness Index

This directory holds the checked-in benchmark artifacts used to back claims in
the README, blog posts, and release notes. Each row below pairs a result file
with the date of its last commit (via `git log -1 --format=%ci`, *not*
filesystem mtime, which resets on worktree checkout).

**Freshness policy:** results are considered stale after **14 days**. As of
**2026-04-16**, anything last committed before **2026-04-02** is flagged
`STALE — candidate for re-run`. Stale results are kept for provenance; none of
them are deleted by this index.

## Results

| File | Last measured | Status |
|---|---|---|
| `embed_benchmark_results.md` | 2026-03-13 | STALE — candidate for re-run |
| `multilang_eval.json` | 2026-03-13 | STALE — candidate for re-run |
| `real_world_benchmark.json` | 2026-03-13 | STALE — candidate for re-run |
| `real_world_benchmark.md` | 2026-03-13 | STALE — candidate for re-run |
| `swe_bench_localization.json` | 2026-03-13 | STALE — candidate for re-run |
| `swe_bench_localization.md` | 2026-03-13 | STALE — candidate for re-run |
| `swe_bench_lite_eval.json` | 2026-03-14 | STALE — candidate for re-run |
| `swe_bench_lite_eval.md` | 2026-03-14 | STALE — candidate for re-run |
| `agent_benchmark.json` | 2026-03-29 | STALE — candidate for re-run |
| `agent_benchmark.md` | 2026-03-29 | STALE — candidate for re-run |
| `queue_v2_benchmark.json` | 2026-04-06 | fresh |
| `queue_v2_benchmark.md` | 2026-04-06 | fresh |
| `agent_benchmark_large_hard.json` | 2026-04-14 | fresh |
| `agent_benchmark_large_hard.md` | 2026-04-14 | fresh |
| `agent_benchmark_large_hard_full.json` | 2026-04-14 | fresh |
| `agent_benchmark_large_hard_full.md` | 2026-04-14 | fresh |
| `agent_benchmark_large_march_replay_full.json` | 2026-04-14 | fresh |
| `agent_benchmark_large_march_replay_full.md` | 2026-04-14 | fresh |
| `agent_benchmark_large_march_replay_medium.json` | 2026-04-14 | fresh |
| `agent_benchmark_large_march_replay_medium.md` | 2026-04-14 | fresh |
| `agent_benchmark_large.json` | 2026-04-14 | fresh |
| `agent_benchmark_large.md` | 2026-04-14 | fresh |

## Re-running

See the per-benchmark harness docs under `benchmarks/` (e.g. `queue_v2`,
`agent_benchmark_large`). Reproducible benchmark harnesses are documented at
`reference_benchmarks_reproducible.md` in the auto-memory.

## Refreshing this index

Regenerate the dates with:

```bash
for f in benchmarks/results/*.md benchmarks/results/*.json; do
  echo "$(git log -1 --format='%ci' -- "$f" | cut -d' ' -f1) $f"
done | sort
```

Then recompute the 14-day staleness cutoff against today's date.

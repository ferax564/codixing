# Benchmark Results Freshness Index

This directory holds the checked-in benchmark artifacts used to back claims in
the README, blog posts, and release notes. Each row below pairs a result file
with the date of its last commit (via `git log -1 --format=%ci`, *not*
filesystem mtime, which resets on worktree checkout).

**Freshness policy:** results are considered stale after **14 days**. As of
**2026-04-19**, anything last committed before **2026-04-05** is flagged
`STALE — candidate for re-run`. Stale results are kept for provenance; none of
them are deleted by this index.

## Release-to-release performance comparison

The criterion `base/` + `change/` directories under `target/criterion/<bench>/`
track **consecutive runs on whatever commit is checked out** — they are
useless for "did vX.Y regress vs vX.(Y-1)" questions because the base gets
overwritten every run.

The right tool is a named baseline:

```bash
# At release time, capture numbers for the released tag.
git checkout v0.41.0
cargo bench --bench search_bench -- --save-baseline v0.41

# Later (on main, before cutting v0.42), diff against that baseline.
git checkout main
cargo bench --bench search_bench -- --baseline v0.41
```

Criterion will emit the delta inline per benchmark:

```
bm25_search_identifier  time:   [72.4 µs 72.5 µs 72.6 µs]
                        change: [-1.2% +0.1% +1.5%] (p = 0.87 > 0.05)
                        No change in performance detected.
```

Named baselines live under `target/criterion/<bench>/<name>/` and survive
`cargo clean` (criterion owns `target/criterion/`, not cargo). They are **not**
committed — each developer captures their own locally against release tags.

**Recommended capture points:**

- On every release tag — release scripts should add
  `-- --save-baseline $TAG` to their `cargo bench` invocation.
- Before a large refactor — name the baseline `pre-<slug>` so you can diff
  the refactor against its own starting point.

Without this, comparing two releases requires `git worktree add /tmp/oldver
<oldtag>` + a second full rebuild + a manual diff of the terminal output,
which is what happened during the v0.41 shipped-verification.

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

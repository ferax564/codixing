# Large Repo Benchmark Results

Two complementary harnesses cover large-repository performance:

```bash
# Algorithm-level Criterion measurements.
cargo bench -p codixing-core --bench large_repo_bench

# End-to-end, machine-readable indexing/query/sync/quality measurements.
cargo build --release -p codixing -p codixing-server
python3 benchmarks/large_repo_gate.py --profile pr
```

## Results

No hardware-independent result is checked in. CI uploads `criterion-results.txt`
and `large-repo-gate.json` as the `benchmark-results` artifact. Pull requests use
the short profile, main/release-gating pushes use 10K files, and the weekly run
uses 100K files. Manual workflow runs can select any profile.

Performance ratios are evaluated only when a same-machine baseline is supplied
to `large_repo_gate.py`; an unbaselined run records evidence without claiming a
pass. This avoids turning runner-to-runner variance into a fabricated baseline.

See [README.md](README.md#large-repository-performance-gate) for the JSON
contents, quality hooks, and the exact 2x/50% comparison command.

## Targets

- Fresh BM25-ready initialization wall time: at most 50% of current main.
- Peak RSS: at most 50% of current main; record Linux PSS when available.
- Steady on-disk index bytes: at most 50% of current main.
- Cold and warm query p95: at most 50% of current main.
- One-file incremental sync: at most 50% of current main, with rewrite bytes
  scaling with the change rather than the corpus.
- Retrieval: preserve exact results and retain at least 99% of baseline MRR.

All comparisons require the same profile, hardware class, OS, worker count, and
binary feature set. A narrow PR run cannot prove the 100K-file target.

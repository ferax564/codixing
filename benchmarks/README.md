# Benchmarks

This directory contains reproducible harnesses for Codixing retrieval,
agent-loop, and indexing claims.

## Direct Competitor Benchmark

Use `competitor_benchmark.py` to compare Codixing against external local tools
on the curated OpenClaw query set in `queue_v2_queries.toml`.

```bash
python3 benchmarks/competitor_benchmark.py --list-tools
python3 benchmarks/competitor_benchmark.py --repo benchmarks/repos/openclaw --validate-only
python3 benchmarks/competitor_benchmark.py --repo benchmarks/repos/openclaw --dry-run
python3 benchmarks/competitor_benchmark.py --repo benchmarks/repos/openclaw
```

External tools are configured in `competitor_tools.toml`. Keep tools disabled
until they are installed locally, then update their command templates. Results
are written to:

```text
benchmarks/results/competitor_benchmark.json
benchmarks/results/competitor_benchmark.md
```

The benchmark scores file-level `Precision@10`, `Recall@10`, and `MRR`, plus
latency and output bytes. Matching uses suffix comparison so absolute and
relative paths can be compared fairly.

Methodology:

- The query set is `queue_v2_queries.toml`, with symbol, usage, concept, and cross-package file-localization tasks.
- Each command is rendered from `competitor_tools.toml` and executed from the target repo root.
- Query validation checks that ground-truth files exist and that opt-in `cross_pattern` fixtures match at least one expected file.
- Codixing uses the most specific local command for each category: `symbols`, `usages`, `search --json`, and `cross-imports`.
- Cross-package queries may opt into `cross_pattern=true` when the query asks for a specific import shape inside a broad package boundary.
- Indexing time is not mixed into per-query latency; use `run_external_competitors.sh` to record setup/index logs before query timing.

Current local baseline from 2026-04-28:

```text
codixing: 20 queries, Recall@10 0.802, MRR 0.827
grep:     20 queries, Recall@10 0.191, MRR 0.168
```

The `claude-context` and `codebase-memory-mcp` entries are intentionally
disabled until their CLIs or wrappers are installed locally.

To run the checked external codebase-memory-mcp comparison and record setup
logs:

```bash
CODEBASE_MEMORY_MCP=/path/to/codebase-memory-mcp \
CODEBASE_MEMORY_PROJECT=Users-andreaferrarelli-code-codixing-benchmarks-repos-openclaw \
CBM_CACHE_DIR=/tmp/cbm-benchmark/cache \
benchmarks/run_external_competitors.sh
```

Latest external local result from 2026-04-28:

```text
codixing:            20 queries, Recall@10 0.802, MRR 0.827
codebase-memory-mcp: 20 queries, Recall@10 0.374, MRR 0.243
grep:                20 queries, Recall@10 0.191, MRR 0.168
```

## Large-repository performance gate

`large_repo_gate.py` measures the end-to-end costs that Criterion microbenches
cannot cover. It generates an owned synthetic Rust repository and records:

- fresh BM25-ready initialization wall time, Linux direct-child peak RSS from
  `wait4`, and final direct-child `/proc/<pid>/io` counters; descendant
  processes are excluded from the measurement scope;
- fresh and post-sync logical/allocated index bytes with hardlink
  de-duplication, plus an audit requiring one active generation and no
  abandoned generations;
- cold-process and warm resident-server query p50/p95, plus gated cold and
  resident RSS/PSS;
- five interleaved no-op and one-file sync samples, plus one-percent sync;
  every edit gets a unique searchable Rust identifier, one-file visibility is
  checked after each timed sync, and first/middle/last one-percent edits are
  checked after timing; latency gates use the median while rewrite gates use
  the worst sample;
- every timed cold/warm exact-query result, plus a normalized external-quality
  hook that is mandatory for strict claims.

The versioned JSON retains every raw sync sample and an explicit
`measurement_scope` declaring that claim metrics cover the measured direct
child. It also records the canonical `--work-dir` parent, binary hashes,
Rust toolchain/build settings, and explicit worker provenance. Fixed-worker
runs record `worker_mode`, `requested_threads`, and
`effective_worker_threads` for init and sync. An explicit `--threads` request
aborts if the Rayon pool cannot be installed, so a successful current binary
run has the recorded effective count. Baseline comparison fails unless both
sides report the same positive fixed count. Search and server thread counts are
outside this telemetry. Shipping-default runs retain null effective telemetry
for legacy compatibility and cannot support a strict or regression comparison.
The result also records a source-tree digest that covers the Git
revision, tracked diff, and untracked file contents. The CLI and server expose
compile-time revision/tree/dirty attestations; claim-bearing and cached evidence
is rejected unless both binaries match the clean measured checkout. Their build
scripts track every Git-tracked workspace file, not only each binary crate: this
prevents a binary linked from dirty core code from retaining a clean attestation
after the checkout is reverted. Missing Git metadata or unrepresentable tracked
paths fail closed. The generated fixture is identified by a sorted path+content
manifest hash, not only a schema label.

The pinned baseline may predate the hidden attestation command. That case has
one narrow bootstrap path: the current harness requires a clean historical
worktree, a fresh target directory and receipt outside that worktree, records
sanitized pre/post Git and Rust toolchain identity plus artifact-affecting build
environment, builds the two release binaries with `cargo build --locked`, and
binds their exact paths and SHA-256 hashes in an
`owned-clean-legacy-build-v1` receipt. Measurement and cached comparison
revalidate the receipt. It cannot replace either native attestation, mixed
native/legacy binaries fail, and strict candidate evidence never accepts this
fallback. The same-run CI baseline capture is the trust boundary; a standalone
JSON receipt is not a general exemption for arbitrary binaries.

The built-in profiles are `pr` (250 files), `10k`, and `100k`; `--files` and
`--query-runs` support targeted experiments.

```bash
cargo build --release -p codixing -p codixing-server
python3 benchmarks/large_repo_gate.py \
  --profile pr \
  --output /tmp/codixing-large-repo.json
```

The script does not invent a universal baseline. Evidence is explicitly labeled
`smoke`, `regression`, `claim-baseline`, or `strict-claim`. The `pr` profile is
an unbaselined smoke run only. CI uses 10K files as a regression/scale
surrogate: its speed-suite limit is 0.80 and its RSS limit is 0.75, so a green
PR/main job is not a published 2x/50% result. Only a `strict-claim` 100K run
whose source revision exactly matches the candidate and whose trusted baseline
revision exactly matches `origin/main` can support the huge-repository goal.
Strict 100K evidence caps both the fresh allocated index and the final
post-sync deduplicated allocation at 0.50 of baseline. The final snapshot is
accepted for the treatment only when the doctor/filesystem audit reports
exactly one active generation and no abandoned generations. A pinned pre-
generation baseline is normalized as clean `legacy_flat` state so the migration
itself remains comparable; unverified or accumulating layouts fail.

The speed suite is the geometric mean of current/baseline ratios for fresh
initialization, cold-query p95, no-op sync, one-file sync, and one-percent sync.
No-op and one-file timings are each measured five times after one discarded
warmup pair (no-op + one-file); their existing `process.wall_time_ms` JSON paths
hold the median. The run is rejected as unstable when either median absolute
deviation exceeds the larger of 75ms and 10% of its median, or when the
interquartile spread exceeds the larger of 200ms and 20% of its median. Absolute
floors absorb residual shared-runner jitter on sub-second CLI process samples;
the relative caps still catch true bimodal / high-dispersion failures. Their surviving-churn and effective-write paths each hold the maximum
across all five samples.
A 1.05 component limit protects initialization, cold-query p95, no-op sync, and
one-percent sync. CI separately requires one-file sync to meet the profile's
relative speed limit: 0.80 at 10K and 0.50 at 100K. It also applies a bounded
absolute non-regression allowance of 100ms at 10K and 500ms at 100K, with a hard
500ms ceiling in the harness.
Warm HTTP p95 is handled outside the suite: the ratio target stops at a 10ms
absolute floor and may regress by at most 2ms from a faster baseline. This
avoids pretending that a noisy 4ms operation must become 2ms to establish a 2x
large-repository improvement. Accordingly, the defensible wording is **2x
geometric mean on the named BM25-only five-operation 100K suite**, not “every
operation is 2x faster.” The harness runs with embeddings disabled. Hybrid and
vector initialization, model memory, and vector-index disk are outside this
gate and require separate evidence. Initialization, one-percent sync, and the
resident-server lifecycle remain single-run measurements; the JSON records that
limitation.

To evaluate the 100K claim, capture baseline and treatment on the same machine
and OS, with the same explicit worker count and the same canonical work parent.
The current campaign uses `--threads 8` for both revisions; a comparison that
omits fixed-worker telemetry fails closed. Then run:

```bash
python3 benchmarks/large_repo_gate.py \
  --profile 100k \
  --evidence-mode strict-claim \
  --threads 8 \
  --baseline /path/to/current-main-100k.json \
  --external-quality-result /path/to/candidate-quality.json \
  --expected-current-revision "$CANDIDATE_SHA" \
  --expected-baseline-revision "$TRUSTED_BASELINE_SHA" \
  --work-dir /path/to/shared-benchmark-parent \
  --max-init-ratio 1.05 \
  --max-rss-ratio 0.50 \
  --max-resident-rss-ratio 0.50 \
  --max-disk-ratio 0.50 \
  --max-post-sync-disk-ratio 0.50 \
  --max-cold-query-p95-ratio 1.05 \
  --max-cold-query-rss-ratio 0.50 \
  --max-warm-query-p95-ratio 0.50 \
  --warm-query-absolute-floor-ms 10 \
  --max-warm-query-regression-ms 2 \
  --max-speed-suite-ratio 0.50 \
  --max-speed-component-ratio 1.05 \
  --max-one-file-sync-ratio 0.50 \
  --max-one-file-sync-regression-ms 500 \
  --max-one-file-sync-rss-ratio 0.50 \
  --max-one-file-rewrite-ratio 0.50 \
  --max-one-percent-sync-ratio 1.05 \
  --max-one-percent-sync-rss-ratio 0.50 \
  --max-one-percent-rewrite-ratio 0.50 \
  --max-no-op-rewrite-bytes 0 \
  --min-quality-ratio 0.99 \
  --min-recall-at-10-ratio 0.99 \
  --min-quality-mrr 0.80 \
  --min-quality-recall-at-10 0.90 \
  --output /tmp/codixing-treatment-100k.json
```

Without `--baseline`, the JSON says `performance_gate.status = "recorded"`;
that is evidence capture, not a performance claim. Synthetic exact probes still
gate correctness. Supply `--quality-file` with a JSON array of `query`,
`expected_file`, and optional `strategy` fields to use a custom query set that
matches the generated fixture. For a representative repository
evaluation run by another harness, pass normalized
`--external-quality-result quality.json` containing `mrr`, `recall_at_10`,
`source`, positive `task_count`, `dataset_sha256`, and `source_revision`. In
strict mode all attribution fields are mandatory and `source_revision` must
equal the source checkout being measured. Baseline and treatment must use the
same source/task-count/dataset identity; both MRR and Recall@10 have relative
and absolute floors. Generated exact probes still gate every timed cold and
warm result.

CI performs this comparison on one runner and records the CPU, kernel,
filesystem, actual fixture identity, sampling policy, toolchain/build settings,
fixed effective worker count, and canonical work parent in both results. The trusted pin is read from
`origin/main:benchmarks/large_repo_baseline_ref.txt`, not from candidate-controlled
contents. Pull requests and main pushes enforce regression-only 10K limits.
Scheduled or manually selected 100K runs enter strict mode and require separate
commit-bound baseline/candidate quality JSON. Manual runs accept the raw JSON
values through `baseline_quality_json` and `candidate_quality_json`; they are
written to runner-owned temporary files, so paths on the caller's machine are
never interpreted. For example, a branch can be evaluated without changing
repository variables:

```bash
gh workflow run ci.yml --ref my-branch \
  -f large_repo_profile=100k \
  -f baseline_quality_json="$(<baseline-quality.json)" \
  -f candidate_quality_json="$(<candidate-quality.json)"
```

Scheduled runs read the JSON from the
`LARGE_REPO_BASELINE_QUALITY_JSON` and `LARGE_REPO_CANDIDATE_QUALITY_JSON`
repository variables. Missing or stale evidence fails closed. Linux is the
claim platform because it supplies exact direct-child peak RSS after exit and
final direct-child I/O before reaping. Descendant processes are excluded:
short-lived helpers cannot be proven complete without perturbing the timed
operation. macOS direct-PID polling remains useful for non-claim diagnostics.
The baseline and treatment JSON files are uploaded together so the claim is
auditable.

Incremental write gates use the larger of surviving changed-inode allocation
and Linux direct-child `io_write_bytes`. The surviving-artifact churn remains
a separate JSON field for diagnosis; temporary write/delete traffic in the
measured process can no longer masquerade as zero rewrite cost. Work delegated
to external helper processes is outside the claim scope and must be benchmarked
separately.

`claude-context` was not benchmarked locally because it requires Node.js
20-23 plus OpenAI and Zilliz/Milvus credentials; this machine currently has
Node.js 25.6.1 and no benchmark credentials configured.

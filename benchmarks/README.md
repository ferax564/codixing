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

- fresh BM25-ready initialization wall time, peak RSS, and Linux PSS/I/O;
- steady logical and allocated index bytes with hardlink de-duplication and a
  generation-aware artifact breakdown;
- cold-process and warm resident-server query p50/p95;
- no-op, one-file, and one-percent sync wall time and bytes rewritten;
- synthetic exact-query Recall@10/MRR, with a normalized external-quality hook.

The result is versioned JSON. The built-in profiles are `pr` (250 files),
`10k`, and `100k`; `--files` and `--query-runs` support targeted experiments.

```bash
cargo build --release -p codixing -p codixing-server
python3 benchmarks/large_repo_gate.py \
  --profile pr \
  --output /tmp/codixing-large-repo.json
```

The script does not invent a universal baseline. To prove the current 2x
speed/50% memory/50% disk goals, capture the baseline and treatment on the same
machine and OS, with the same profile and worker count, then run:

```bash
python3 benchmarks/large_repo_gate.py \
  --profile 100k \
  --baseline /path/to/current-main-100k.json \
  --max-init-ratio 0.50 \
  --max-rss-ratio 0.50 \
  --max-disk-ratio 0.50 \
  --max-cold-query-p95-ratio 0.50 \
  --max-warm-query-p95-ratio 0.50 \
  --max-one-file-sync-ratio 0.50 \
  --max-one-file-rewrite-ratio 0.50 \
  --min-quality-ratio 0.99 \
  --output /tmp/codixing-treatment-100k.json
```

Without `--baseline`, the JSON says `performance_gate.status = "recorded"`;
that is evidence capture, not a performance claim. Synthetic exact probes still
gate correctness. Supply `--quality-file` with a JSON array of `query`,
`expected_file`, and optional `strategy` fields to use a custom query set that
matches the generated fixture. For a representative repository
evaluation run by another harness, pass normalized
`--external-quality-result quality.json` containing `mrr`, `recall_at_10`, and
an optional `source`/`task_count`. That external MRR becomes the baseline
comparison metric while the generated exact probes continue to gate basic
correctness.

CI performs this comparison on one runner and records the CPU, kernel,
filesystem, and fixture-schema identity in both results. It builds the commit pinned in
`large_repo_baseline_ref.txt`, measures that binary first, then measures the
candidate with the same profile and enforces every 2x/50% threshold above.
The baseline and treatment JSON files are uploaded together so the claim is
auditable. Move the pin only when intentionally starting a new performance
campaign, and preserve the prior evidence in `benchmarks/results/`.

`claude-context` was not benchmarked locally because it requires Node.js
20-23 plus OpenAI and Zilliz/Milvus credentials; this machine currently has
Node.js 25.6.1 and no benchmark credentials configured.

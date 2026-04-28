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

`claude-context` was not benchmarked locally because it requires Node.js
20-23 plus OpenAI and Zilliz/Milvus credentials; this machine currently has
Node.js 25.6.1 and no benchmark credentials configured.

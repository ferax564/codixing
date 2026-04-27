# Direct Competitor Benchmark

**Date:** 2026-04-27 21:21
**Repo:** `/Users/andreaferrarelli/code/codixing/benchmarks/repos/openclaw`
**Queries:** `/Users/andreaferrarelli/code/codixing/benchmarks/queue_v2_queries.toml`
**Tools:** `/Users/andreaferrarelli/code/codixing/benchmarks/competitor_tools.toml`

## Skipped Tools

- `claude-context` — Install and expose a CLI/MCP wrapper locally, then update these command templates to match your invocation.
- `codebase-memory-mcp` — Install/index locally, then run with --include-disabled --tool codebase-memory-mcp. Set CODEBASE_MEMORY_MCP, CODEBASE_MEMORY_PROJECT, and optionally CBM_CACHE_DIR.

## Validation

- validated 20 query fixture(s)

## Summary

| Tool | Queries | Precision@10 | Recall@10 | MRR | Avg ms | Avg output bytes |
|---|---:|---:|---:|---:|---:|---:|
| codixing | 20 | 0.261 | 0.783 | 0.827 | 378.3 | 24799.8 |
| grep | 20 | 0.125 | 0.191 | 0.168 | 555.4 | 958299.0 |

## Category Summary

| Category | Tool | Queries | Recall@10 | MRR |
|---|---|---:|---:|---:|
| concept | codixing | 5 | 0.563 | 0.900 |
| concept | grep | 5 | 0.000 | 0.000 |
| cross-package | codixing | 5 | 1.000 | 0.640 |
| cross-package | grep | 5 | 0.000 | 0.000 |
| symbol | codixing | 5 | 1.000 | 1.000 |
| symbol | grep | 5 | 0.200 | 0.200 |
| usage | codixing | 5 | 0.570 | 0.767 |
| usage | grep | 5 | 0.565 | 0.472 |

## Methodology

- Query set: curated OpenClaw file-localization queries from `/Users/andreaferrarelli/code/codixing/benchmarks/queue_v2_queries.toml`.
- Scoring: file-level Precision@10, Recall@10, and MRR. A returned path counts as a hit when it matches a ground-truth path exactly or by suffix.
- Command execution: each tool command is rendered from `benchmarks/competitor_tools.toml` and run from the target repository root.
- Codixing routing: symbols use `codixing symbols`; usage uses `codixing usages`; concepts use `codixing search --json`; cross-package queries use `codixing cross-imports`, with an optional regex pattern only when the query opts into `cross_pattern=true`.
- External tools: disabled tools are excluded unless `--include-disabled --tool <name>` is passed and their local CLI/cache environment variables are configured.
- Limitations: this is a retrieval/localization benchmark, not an end-to-end agent task benchmark. Indexing time is recorded separately by `benchmarks/run_external_competitors.sh`.

## Per Query

| Tool | Query | Category | Recall@10 | MRR | ms | bytes | Error |
|---|---|---|---:|---:|---:|---:|---|
| codixing | symbol-channel-plugin | symbol | 1.000 | 1.000 | 101 | 50421 |  |
| codixing | symbol-gateway-server | symbol | 1.000 | 1.000 | 100 | 9173 |  |
| codixing | symbol-context-engine-interface | symbol | 1.000 | 1.000 | 100 | 10174 |  |
| codixing | symbol-openclaw-config | symbol | 1.000 | 1.000 | 101 | 199091 |  |
| codixing | symbol-tool-policy-like | symbol | 1.000 | 1.000 | 100 | 514 |  |
| codixing | usage-redact-sensitive-text | usage | 0.875 | 1.000 | 97 | 1913 |  |
| codixing | usage-create-auth-rate-limiter | usage | 1.000 | 0.333 | 96 | 896 |  |
| codixing | usage-channel-plugin-imports | usage | 0.500 | 0.500 | 97 | 3171 |  |
| codixing | usage-context-engine-imports | usage | 0.375 | 1.000 | 95 | 2744 |  |
| codixing | usage-load-config | usage | 0.100 | 1.000 | 98 | 3298 |  |
| codixing | concept-security-audit | concept | 0.750 | 1.000 | 1063 | 41620 |  |
| codixing | concept-rate-limiting | concept | 0.333 | 1.000 | 734 | 33954 |  |
| codixing | concept-secret-redaction | concept | 0.333 | 1.000 | 1011 | 36781 |  |
| codixing | concept-cron-scheduling | concept | 0.400 | 1.000 | 997 | 45790 |  |
| codixing | concept-system-prompt-composition | concept | 1.000 | 0.500 | 1081 | 38017 |  |
| codixing | cross-pkg-plugin-sdk-entry | cross-package | 1.000 | 1.000 | 1237 | 1306 |  |
| codixing | cross-pkg-bundled-channel-entries | cross-package | 1.000 | 0.500 | 113 | 336 |  |
| codixing | cross-pkg-security-from-gateway | cross-package | 1.000 | 0.200 | 116 | 313 |  |
| codixing | cross-pkg-config-types-from-agents | cross-package | 1.000 | 1.000 | 112 | 16148 |  |
| codixing | cross-pkg-plugin-contracts-registry | cross-package | 1.000 | 0.500 | 117 | 336 |  |
| grep | symbol-channel-plugin | symbol | 0.000 | 0.000 | 348 | 19482 |  |
| grep | symbol-gateway-server | symbol | 0.000 | 0.000 | 543 | 19346 |  |
| grep | symbol-context-engine-interface | symbol | 0.000 | 0.000 | 554 | 23134 |  |
| grep | symbol-openclaw-config | symbol | 0.000 | 0.000 | 60 | 18558 |  |
| grep | symbol-tool-policy-like | symbol | 1.000 | 1.000 | 549 | 1237 |  |
| grep | usage-redact-sensitive-text | usage | 0.875 | 0.500 | 1027 | 1238 |  |
| grep | usage-create-auth-rate-limiter | usage | 1.000 | 1.000 | 1018 | 221 |  |
| grep | usage-channel-plugin-imports | usage | 0.250 | 0.111 | 991 | 23268 |  |
| grep | usage-context-engine-imports | usage | 0.500 | 0.500 | 988 | 2367 |  |
| grep | usage-load-config | usage | 0.200 | 0.250 | 1006 | 21481 |  |
| grep | concept-security-audit | concept | 0.000 | 0.000 | 507 | 9488541 |  |
| grep | concept-rate-limiting | concept | 0.000 | 0.000 | 1315 | 9488542 |  |
| grep | concept-secret-redaction | concept | 0.000 | 0.000 | 519 | 21150 |  |
| grep | concept-cron-scheduling | concept | 0.000 | 0.000 | 14 | 16069 |  |
| grep | concept-system-prompt-composition | concept | 0.000 | 0.000 | 1245 | 19139 |  |
| grep | cross-pkg-plugin-sdk-entry | cross-package | 0.000 | 0.000 | 280 | 449 |  |
| grep | cross-pkg-bundled-channel-entries | cross-package | 0.000 | 0.000 | 7 | 879 |  |
| grep | cross-pkg-security-from-gateway | cross-package | 0.000 | 0.000 | 44 | 0 |  |
| grep | cross-pkg-config-types-from-agents | cross-package | 0.000 | 0.000 | 86 | 0 |  |
| grep | cross-pkg-plugin-contracts-registry | cross-package | 0.000 | 0.000 | 6 | 879 |  |

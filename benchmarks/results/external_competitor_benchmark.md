# Direct Competitor Benchmark

**Date:** 2026-04-27 20:44
**Repo:** `/Users/andreaferrarelli/code/codixing/benchmarks/repos/openclaw`
**Queries:** `/Users/andreaferrarelli/code/codixing/benchmarks/queue_v2_queries.toml`
**Tools:** `/Users/andreaferrarelli/code/codixing/benchmarks/competitor_tools.toml`

## Validation

- validated 20 query fixture(s)

## Summary

| Tool | Queries | Precision@10 | Recall@10 | MRR | Avg ms | Avg output bytes |
|---|---:|---:|---:|---:|---:|---:|
| codebase-memory-mcp | 20 | 0.147 | 0.374 | 0.243 | 612.0 | 6353.7 |
| codixing | 20 | 0.291 | 0.807 | 0.789 | 391.6 | 24638.2 |
| grep | 20 | 0.125 | 0.191 | 0.168 | 559.7 | 958299.0 |

## Category Summary

| Category | Tool | Queries | Recall@10 | MRR |
|---|---|---:|---:|---:|
| concept | codebase-memory-mcp | 5 | 0.000 | 0.000 |
| concept | codixing | 5 | 0.563 | 0.900 |
| concept | grep | 5 | 0.000 | 0.000 |
| cross-package | codebase-memory-mcp | 5 | 0.000 | 0.000 |
| cross-package | codixing | 5 | 1.000 | 0.640 |
| cross-package | grep | 5 | 0.000 | 0.000 |
| symbol | codebase-memory-mcp | 5 | 1.000 | 0.650 |
| symbol | codixing | 5 | 1.000 | 1.000 |
| symbol | grep | 5 | 0.200 | 0.200 |
| usage | codebase-memory-mcp | 5 | 0.495 | 0.322 |
| usage | codixing | 5 | 0.665 | 0.617 |
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
| codixing | symbol-channel-plugin | symbol | 1.000 | 1.000 | 120 | 50421 |  |
| codixing | symbol-gateway-server | symbol | 1.000 | 1.000 | 107 | 9173 |  |
| codixing | symbol-context-engine-interface | symbol | 1.000 | 1.000 | 123 | 10174 |  |
| codixing | symbol-openclaw-config | symbol | 1.000 | 1.000 | 112 | 199091 |  |
| codixing | symbol-tool-policy-like | symbol | 1.000 | 1.000 | 171 | 514 |  |
| codixing | usage-redact-sensitive-text | usage | 0.625 | 0.250 | 98 | 1747 |  |
| codixing | usage-create-auth-rate-limiter | usage | 1.000 | 0.500 | 98 | 1734 |  |
| codixing | usage-channel-plugin-imports | usage | 0.750 | 0.333 | 98 | 1689 |  |
| codixing | usage-context-engine-imports | usage | 0.250 | 1.000 | 98 | 1816 |  |
| codixing | usage-load-config | usage | 0.700 | 1.000 | 111 | 1805 |  |
| codixing | concept-security-audit | concept | 0.750 | 1.000 | 1040 | 41620 |  |
| codixing | concept-rate-limiting | concept | 0.333 | 1.000 | 757 | 33954 |  |
| codixing | concept-secret-redaction | concept | 0.333 | 1.000 | 1004 | 36781 |  |
| codixing | concept-cron-scheduling | concept | 0.400 | 1.000 | 1052 | 45790 |  |
| codixing | concept-system-prompt-composition | concept | 1.000 | 0.500 | 1115 | 38017 |  |
| codixing | cross-pkg-plugin-sdk-entry | cross-package | 1.000 | 1.000 | 1274 | 1306 |  |
| codixing | cross-pkg-bundled-channel-entries | cross-package | 1.000 | 0.500 | 113 | 336 |  |
| codixing | cross-pkg-security-from-gateway | cross-package | 1.000 | 0.200 | 112 | 313 |  |
| codixing | cross-pkg-config-types-from-agents | cross-package | 1.000 | 1.000 | 114 | 16148 |  |
| codixing | cross-pkg-plugin-contracts-registry | cross-package | 1.000 | 0.500 | 116 | 336 |  |
| grep | symbol-channel-plugin | symbol | 0.000 | 0.000 | 338 | 19482 |  |
| grep | symbol-gateway-server | symbol | 0.000 | 0.000 | 555 | 19346 |  |
| grep | symbol-context-engine-interface | symbol | 0.000 | 0.000 | 604 | 23134 |  |
| grep | symbol-openclaw-config | symbol | 0.000 | 0.000 | 61 | 18558 |  |
| grep | symbol-tool-policy-like | symbol | 1.000 | 1.000 | 542 | 1237 |  |
| grep | usage-redact-sensitive-text | usage | 0.875 | 0.500 | 1063 | 1238 |  |
| grep | usage-create-auth-rate-limiter | usage | 1.000 | 1.000 | 1017 | 221 |  |
| grep | usage-channel-plugin-imports | usage | 0.250 | 0.111 | 980 | 23268 |  |
| grep | usage-context-engine-imports | usage | 0.500 | 0.500 | 1046 | 2367 |  |
| grep | usage-load-config | usage | 0.200 | 0.250 | 1001 | 21481 |  |
| grep | concept-security-audit | concept | 0.000 | 0.000 | 513 | 9488541 |  |
| grep | concept-rate-limiting | concept | 0.000 | 0.000 | 1320 | 9488542 |  |
| grep | concept-secret-redaction | concept | 0.000 | 0.000 | 509 | 21150 |  |
| grep | concept-cron-scheduling | concept | 0.000 | 0.000 | 14 | 16069 |  |
| grep | concept-system-prompt-composition | concept | 0.000 | 0.000 | 1224 | 19139 |  |
| grep | cross-pkg-plugin-sdk-entry | cross-package | 0.000 | 0.000 | 266 | 449 |  |
| grep | cross-pkg-bundled-channel-entries | cross-package | 0.000 | 0.000 | 6 | 879 |  |
| grep | cross-pkg-security-from-gateway | cross-package | 0.000 | 0.000 | 42 | 0 |  |
| grep | cross-pkg-config-types-from-agents | cross-package | 0.000 | 0.000 | 87 | 0 |  |
| grep | cross-pkg-plugin-contracts-registry | cross-package | 0.000 | 0.000 | 6 | 879 |  |
| codebase-memory-mcp | symbol-channel-plugin | symbol | 1.000 | 0.250 | 446 | 2960 |  |
| codebase-memory-mcp | symbol-gateway-server | symbol | 1.000 | 0.500 | 805 | 2949 |  |
| codebase-memory-mcp | symbol-context-engine-interface | symbol | 1.000 | 0.500 | 450 | 2979 |  |
| codebase-memory-mcp | symbol-openclaw-config | symbol | 1.000 | 1.000 | 839 | 2909 |  |
| codebase-memory-mcp | symbol-tool-policy-like | symbol | 1.000 | 1.000 | 841 | 394 |  |
| codebase-memory-mcp | usage-redact-sensitive-text | usage | 0.875 | 1.000 | 1078 | 10126 |  |
| codebase-memory-mcp | usage-create-auth-rate-limiter | usage | 1.000 | 0.250 | 902 | 4968 |  |
| codebase-memory-mcp | usage-channel-plugin-imports | usage | 0.000 | 0.000 | 591 | 10628 |  |
| codebase-memory-mcp | usage-context-engine-imports | usage | 0.500 | 0.250 | 672 | 11413 |  |
| codebase-memory-mcp | usage-load-config | usage | 0.100 | 0.111 | 468 | 10868 |  |
| codebase-memory-mcp | concept-security-audit | concept | 0.000 | 0.000 | 228 | 42944 |  |
| codebase-memory-mcp | concept-rate-limiting | concept | 0.000 | 0.000 | 7 | 132 |  |
| codebase-memory-mcp | concept-secret-redaction | concept | 0.000 | 0.000 | 828 | 10967 |  |
| codebase-memory-mcp | concept-cron-scheduling | concept | 0.000 | 0.000 | 158 | 11697 |  |
| codebase-memory-mcp | concept-system-prompt-composition | concept | 0.000 | 0.000 | 6 | 132 |  |
| codebase-memory-mcp | cross-pkg-plugin-sdk-entry | cross-package | 0.000 | 0.000 | 1004 | 219 |  |
| codebase-memory-mcp | cross-pkg-bundled-channel-entries | cross-package | 0.000 | 0.000 | 7 | 132 |  |
| codebase-memory-mcp | cross-pkg-security-from-gateway | cross-package | 0.000 | 0.000 | 956 | 219 |  |
| codebase-memory-mcp | cross-pkg-config-types-from-agents | cross-package | 0.000 | 0.000 | 985 | 219 |  |
| codebase-memory-mcp | cross-pkg-plugin-contracts-registry | cross-package | 0.000 | 0.000 | 968 | 219 |  |

# Direct Competitor Benchmark

**Date:** 2026-04-28 20:57
**Repo:** `/Users/andreaferrarelli/code/codixing/benchmarks/repos/openclaw`
**Queries:** `/Users/andreaferrarelli/code/codixing/benchmarks/queue_v2_queries.toml`
**Tools:** `/Users/andreaferrarelli/code/codixing/benchmarks/competitor_tools.toml`

## Validation

- validated 20 query fixture(s)

## Summary

| Tool | Queries | Precision@10 | Recall@10 | MRR | Avg ms | Avg output bytes |
|---|---:|---:|---:|---:|---:|---:|
| claude-context | 20 | 0.000 | 0.000 | 0.000 | 3.1 | 43.0 |
| codebase-memory-mcp | 20 | 0.147 | 0.374 | 0.243 | 763.9 | 6366.8 |
| codixing | 20 | 0.281 | 0.802 | 0.827 | 409.9 | 24827.8 |
| grep | 20 | 0.125 | 0.191 | 0.168 | 584.4 | 958669.0 |

## Category Summary

| Category | Tool | Queries | Recall@10 | MRR |
|---|---|---:|---:|---:|
| concept | claude-context | 5 | 0.000 | 0.000 |
| concept | codebase-memory-mcp | 5 | 0.000 | 0.000 |
| concept | codixing | 5 | 0.563 | 0.900 |
| concept | grep | 5 | 0.000 | 0.000 |
| cross-package | claude-context | 5 | 0.000 | 0.000 |
| cross-package | codebase-memory-mcp | 5 | 0.000 | 0.000 |
| cross-package | codixing | 5 | 1.000 | 0.640 |
| cross-package | grep | 5 | 0.000 | 0.000 |
| symbol | claude-context | 5 | 0.000 | 0.000 |
| symbol | codebase-memory-mcp | 5 | 1.000 | 0.650 |
| symbol | codixing | 5 | 1.000 | 1.000 |
| symbol | grep | 5 | 0.200 | 0.200 |
| usage | claude-context | 5 | 0.000 | 0.000 |
| usage | codebase-memory-mcp | 5 | 0.495 | 0.322 |
| usage | codixing | 5 | 0.645 | 0.767 |
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
| codixing | symbol-channel-plugin | symbol | 1.000 | 1.000 | 113 | 50421 |  |
| codixing | symbol-gateway-server | symbol | 1.000 | 1.000 | 114 | 9173 |  |
| codixing | symbol-context-engine-interface | symbol | 1.000 | 1.000 | 112 | 10174 |  |
| codixing | symbol-openclaw-config | symbol | 1.000 | 1.000 | 115 | 199091 |  |
| codixing | symbol-tool-policy-like | symbol | 1.000 | 1.000 | 112 | 514 |  |
| codixing | usage-redact-sensitive-text | usage | 0.875 | 1.000 | 109 | 1941 |  |
| codixing | usage-create-auth-rate-limiter | usage | 1.000 | 0.500 | 109 | 918 |  |
| codixing | usage-channel-plugin-imports | usage | 0.250 | 1.000 | 112 | 3263 |  |
| codixing | usage-context-engine-imports | usage | 0.500 | 1.000 | 119 | 3166 |  |
| codixing | usage-load-config | usage | 0.600 | 0.333 | 118 | 3295 |  |
| codixing | concept-security-audit | concept | 0.750 | 1.000 | 1138 | 41613 |  |
| codixing | concept-rate-limiting | concept | 0.333 | 1.000 | 786 | 33954 |  |
| codixing | concept-secret-redaction | concept | 0.333 | 1.000 | 1077 | 36785 |  |
| codixing | concept-cron-scheduling | concept | 0.400 | 1.000 | 1062 | 45791 |  |
| codixing | concept-system-prompt-composition | concept | 1.000 | 0.500 | 1160 | 38017 |  |
| codixing | cross-pkg-plugin-sdk-entry | cross-package | 1.000 | 1.000 | 1323 | 1306 |  |
| codixing | cross-pkg-bundled-channel-entries | cross-package | 1.000 | 0.500 | 128 | 336 |  |
| codixing | cross-pkg-security-from-gateway | cross-package | 1.000 | 0.200 | 128 | 313 |  |
| codixing | cross-pkg-config-types-from-agents | cross-package | 1.000 | 1.000 | 130 | 16148 |  |
| codixing | cross-pkg-plugin-contracts-registry | cross-package | 1.000 | 0.500 | 134 | 336 |  |
| grep | symbol-channel-plugin | symbol | 0.000 | 0.000 | 335 | 19482 |  |
| grep | symbol-gateway-server | symbol | 0.000 | 0.000 | 528 | 19346 |  |
| grep | symbol-context-engine-interface | symbol | 0.000 | 0.000 | 573 | 23134 |  |
| grep | symbol-openclaw-config | symbol | 0.000 | 0.000 | 62 | 18558 |  |
| grep | symbol-tool-policy-like | symbol | 1.000 | 1.000 | 560 | 1237 |  |
| grep | usage-redact-sensitive-text | usage | 0.875 | 0.500 | 1037 | 1238 |  |
| grep | usage-create-auth-rate-limiter | usage | 1.000 | 1.000 | 1055 | 221 |  |
| grep | usage-channel-plugin-imports | usage | 0.250 | 0.111 | 1028 | 23268 |  |
| grep | usage-context-engine-imports | usage | 0.500 | 0.500 | 1045 | 2367 |  |
| grep | usage-load-config | usage | 0.200 | 0.250 | 1045 | 21481 |  |
| grep | concept-security-audit | concept | 0.000 | 0.000 | 543 | 9488541 |  |
| grep | concept-rate-limiting | concept | 0.000 | 0.000 | 1360 | 9488542 |  |
| grep | concept-secret-redaction | concept | 0.000 | 0.000 | 533 | 21150 |  |
| grep | concept-cron-scheduling | concept | 0.000 | 0.000 | 16 | 16069 |  |
| grep | concept-system-prompt-composition | concept | 0.000 | 0.000 | 1270 | 19139 |  |
| grep | cross-pkg-plugin-sdk-entry | cross-package | 0.000 | 0.000 | 524 | 2730 |  |
| grep | cross-pkg-bundled-channel-entries | cross-package | 0.000 | 0.000 | 9 | 0 |  |
| grep | cross-pkg-security-from-gateway | cross-package | 0.000 | 0.000 | 50 | 793 |  |
| grep | cross-pkg-config-types-from-agents | cross-package | 0.000 | 0.000 | 107 | 5790 |  |
| grep | cross-pkg-plugin-contracts-registry | cross-package | 0.000 | 0.000 | 7 | 294 |  |
| claude-context | symbol-channel-plugin | symbol | 0.000 | 0.000 | 4 | 43 | command exited 127 |
| claude-context | symbol-gateway-server | symbol | 0.000 | 0.000 | 3 | 43 | command exited 127 |
| claude-context | symbol-context-engine-interface | symbol | 0.000 | 0.000 | 3 | 43 | command exited 127 |
| claude-context | symbol-openclaw-config | symbol | 0.000 | 0.000 | 3 | 43 | command exited 127 |
| claude-context | symbol-tool-policy-like | symbol | 0.000 | 0.000 | 3 | 43 | command exited 127 |
| claude-context | usage-redact-sensitive-text | usage | 0.000 | 0.000 | 3 | 43 | command exited 127 |
| claude-context | usage-create-auth-rate-limiter | usage | 0.000 | 0.000 | 3 | 43 | command exited 127 |
| claude-context | usage-channel-plugin-imports | usage | 0.000 | 0.000 | 3 | 43 | command exited 127 |
| claude-context | usage-context-engine-imports | usage | 0.000 | 0.000 | 3 | 43 | command exited 127 |
| claude-context | usage-load-config | usage | 0.000 | 0.000 | 3 | 43 | command exited 127 |
| claude-context | concept-security-audit | concept | 0.000 | 0.000 | 3 | 43 | command exited 127 |
| claude-context | concept-rate-limiting | concept | 0.000 | 0.000 | 3 | 43 | command exited 127 |
| claude-context | concept-secret-redaction | concept | 0.000 | 0.000 | 3 | 43 | command exited 127 |
| claude-context | concept-cron-scheduling | concept | 0.000 | 0.000 | 3 | 43 | command exited 127 |
| claude-context | concept-system-prompt-composition | concept | 0.000 | 0.000 | 3 | 43 | command exited 127 |
| claude-context | cross-pkg-plugin-sdk-entry | cross-package | 0.000 | 0.000 | 3 | 43 | command exited 127 |
| claude-context | cross-pkg-bundled-channel-entries | cross-package | 0.000 | 0.000 | 3 | 43 | command exited 127 |
| claude-context | cross-pkg-security-from-gateway | cross-package | 0.000 | 0.000 | 3 | 43 | command exited 127 |
| claude-context | cross-pkg-config-types-from-agents | cross-package | 0.000 | 0.000 | 4 | 43 | command exited 127 |
| claude-context | cross-pkg-plugin-contracts-registry | cross-package | 0.000 | 0.000 | 3 | 43 | command exited 127 |
| codebase-memory-mcp | symbol-channel-plugin | symbol | 1.000 | 0.250 | 463 | 2960 |  |
| codebase-memory-mcp | symbol-gateway-server | symbol | 1.000 | 0.500 | 777 | 2949 |  |
| codebase-memory-mcp | symbol-context-engine-interface | symbol | 1.000 | 0.500 | 475 | 2979 |  |
| codebase-memory-mcp | symbol-openclaw-config | symbol | 1.000 | 1.000 | 873 | 2909 |  |
| codebase-memory-mcp | symbol-tool-policy-like | symbol | 1.000 | 1.000 | 861 | 394 |  |
| codebase-memory-mcp | usage-redact-sensitive-text | usage | 0.875 | 1.000 | 1140 | 10126 |  |
| codebase-memory-mcp | usage-create-auth-rate-limiter | usage | 1.000 | 0.250 | 968 | 4968 |  |
| codebase-memory-mcp | usage-channel-plugin-imports | usage | 0.000 | 0.000 | 658 | 10628 |  |
| codebase-memory-mcp | usage-context-engine-imports | usage | 0.500 | 0.250 | 700 | 11413 |  |
| codebase-memory-mcp | usage-load-config | usage | 0.100 | 0.111 | 490 | 10868 |  |
| codebase-memory-mcp | concept-security-audit | concept | 0.000 | 0.000 | 235 | 42944 |  |
| codebase-memory-mcp | concept-rate-limiting | concept | 0.000 | 0.000 | 951 | 219 |  |
| codebase-memory-mcp | concept-secret-redaction | concept | 0.000 | 0.000 | 868 | 10967 |  |
| codebase-memory-mcp | concept-cron-scheduling | concept | 0.000 | 0.000 | 150 | 11697 |  |
| codebase-memory-mcp | concept-system-prompt-composition | concept | 0.000 | 0.000 | 909 | 219 |  |
| codebase-memory-mcp | cross-pkg-plugin-sdk-entry | cross-package | 0.000 | 0.000 | 1022 | 219 |  |
| codebase-memory-mcp | cross-pkg-bundled-channel-entries | cross-package | 0.000 | 0.000 | 737 | 219 |  |
| codebase-memory-mcp | cross-pkg-security-from-gateway | cross-package | 0.000 | 0.000 | 1006 | 219 |  |
| codebase-memory-mcp | cross-pkg-config-types-from-agents | cross-package | 0.000 | 0.000 | 990 | 219 |  |
| codebase-memory-mcp | cross-pkg-plugin-contracts-registry | cross-package | 0.000 | 0.000 | 1005 | 219 |  |

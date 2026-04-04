# Queue Embedding v2 — Benchmark Results

**Date:** 2026-04-04 15:25

## Search Accuracy (OpenClaw)

| Method | Precision@10 | Recall@10 | MRR |
|--------|-------------|----------|-----|
| grep | 0.150 | 0.345 | 0.176 |
| codixing | 0.256 | 0.720 | 0.556 |

### By Category

| Category | Strategy | grep P@10 | grep R@10 | codixing P@10 | codixing R@10 | Best |
|----------|----------|----------|----------|--------------|--------------|------|
| concept | fast | 0.060 | 0.183 | 0.120 | 0.340 | codixing |
| cross-package | cross_imports | 0.040 | 0.080 | 0.233 | 0.800 | codixing |
| symbol | symbol_lookup | 0.140 | 0.600 | 0.180 | 1.000 | codixing |
| usage | usages | 0.360 | 0.515 | 0.489 | 0.740 | codixing |

### Per-Query Breakdown

**grep:**

| Query | Category | P@10 | R@10 | MRR |
|-------|----------|------|------|-----|
| symbol-channel-plugin | symbol | 0.000 | 0.000 | 0.000 |
| symbol-gateway-server | symbol | 0.100 | 1.000 | 0.111 |
| symbol-context-engine-interface | symbol | 0.100 | 1.000 | 0.100 |
| symbol-openclaw-config | symbol | 0.000 | 0.000 | 0.000 |
| symbol-tool-policy-like | symbol | 0.500 | 1.000 | 0.500 |
| usage-redact-sensitive-text | usage | 0.700 | 0.875 | 0.500 |
| usage-create-auth-rate-limiter | usage | 0.500 | 1.000 | 1.000 |
| usage-channel-plugin-imports | usage | 0.000 | 0.000 | 0.000 |
| usage-context-engine-imports | usage | 0.400 | 0.500 | 0.200 |
| usage-load-config | usage | 0.200 | 0.200 | 0.125 |
| concept-security-audit | concept | 0.100 | 0.250 | 0.500 |
| concept-rate-limiting | concept | 0.000 | 0.000 | 0.000 |
| concept-secret-redaction | concept | 0.100 | 0.333 | 0.111 |
| concept-cron-scheduling | concept | 0.000 | 0.000 | 0.000 |
| concept-system-prompt-composition | concept | 0.100 | 0.333 | 0.200 |
| cross-pkg-plugin-sdk-entry | cross-package | 0.000 | 0.000 | 0.000 |
| cross-pkg-bundled-channel-entries | cross-package | 0.000 | 0.000 | 0.000 |
| cross-pkg-security-from-gateway | cross-package | 0.000 | 0.000 | 0.000 |
| cross-pkg-config-types-from-agents | cross-package | 0.200 | 0.400 | 0.167 |
| cross-pkg-plugin-contracts-registry | cross-package | 0.000 | 0.000 | 0.000 |

**codixing:**

| Query | Category | Strategy | P@10 | R@10 | MRR |
|-------|----------|----------|------|------|-----|
| symbol-channel-plugin | symbol | symbol_lookup | 0.100 | 1.000 | 0.167 |
| symbol-gateway-server | symbol | symbol_lookup | 0.100 | 1.000 | 1.000 |
| symbol-context-engine-interface | symbol | symbol_lookup | 0.100 | 1.000 | 0.500 |
| symbol-openclaw-config | symbol | symbol_lookup | 0.100 | 1.000 | 0.500 |
| symbol-tool-policy-like | symbol | symbol_lookup | 0.500 | 1.000 | 1.000 |
| usage-redact-sensitive-text | usage | usages | 0.500 | 0.625 | 0.250 |
| usage-create-auth-rate-limiter | usage | usages | 0.143 | 1.000 | 0.500 |
| usage-channel-plugin-imports | usage | usages | 0.600 | 0.750 | 1.000 |
| usage-context-engine-imports | usage | usages | 0.500 | 0.625 | 1.000 |
| usage-load-config | usage | usages | 0.700 | 0.700 | 1.000 |
| concept-security-audit | concept | fast | 0.200 | 0.500 | 0.500 |
| concept-rate-limiting | concept | fast | 0.100 | 0.333 | 0.500 |
| concept-secret-redaction | concept | fast | 0.100 | 0.333 | 0.500 |
| concept-cron-scheduling | concept | fast | 0.100 | 0.200 | 0.167 |
| concept-system-prompt-composition | concept | fast | 0.100 | 0.333 | 0.333 |
| cross-pkg-plugin-sdk-entry | cross-package | cross_imports | 0.000 | 0.000 | 0.000 |
| cross-pkg-bundled-channel-entries | cross-package | cross_imports | 0.250 | 1.000 | 0.500 |
| cross-pkg-security-from-gateway | cross-package | cross_imports | 0.167 | 1.000 | 0.200 |
| cross-pkg-config-types-from-agents | cross-package | cross_imports | 0.500 | 1.000 | 1.000 |
| cross-pkg-plugin-contracts-registry | cross-package | cross_imports | 0.250 | 1.000 | 0.500 |

## Search Speed

| Method | Avg query time (ms) | Min | Max |
|--------|-------------------|-----|-----|
| grep | 1142.2 | 546.5 | 2093.1 |
| codixing cross_imports | 131.5 | 118.5 | 143.0 |
| codixing fast | 72445.5 | 59811.7 | 99987.9 |
| codixing symbol_lookup | 90.4 | 82.7 | 107.4 |
| codixing usages | 83.4 | 79.8 | 90.5 |

### Per-Query Timing

| Query | Category | grep (ms) | codixing (ms) | Strategy |
|-------|----------|----------|--------------|----------|
| symbol-channel-plugin | symbol | 1247.0 | 107.4 | symbol_lookup |
| symbol-gateway-server | symbol | 558.5 | 86.5 | symbol_lookup |
| symbol-context-engine-interface | symbol | 597.6 | 82.7 | symbol_lookup |
| symbol-openclaw-config | symbol | 546.5 | 90.5 | symbol_lookup |
| symbol-tool-policy-like | symbol | 554.2 | 85.0 | symbol_lookup |
| usage-redact-sensitive-text | usage | 1009.9 | 90.5 | usages |
| usage-create-auth-rate-limiter | usage | 985.4 | 85.4 | usages |
| usage-channel-plugin-imports | usage | 990.3 | 81.0 | usages |
| usage-context-engine-imports | usage | 956.2 | 79.8 | usages |
| usage-load-config | usage | 997.1 | 80.2 | usages |
| concept-security-audit | concept | 698.7 | 59849.3 | fast |
| concept-rate-limiting | concept | 1512.7 | 62386.9 | fast |
| concept-secret-redaction | concept | 1562.1 | 99987.9 | fast |
| concept-cron-scheduling | concept | 1414.0 | 59811.7 | fast |
| concept-system-prompt-composition | concept | 2093.1 | 80191.7 | fast |
| cross-pkg-plugin-sdk-entry | cross-package | 1933.3 | 143.0 | cross_imports |
| cross-pkg-bundled-channel-entries | cross-package | 1204.1 | 118.5 | cross_imports |
| cross-pkg-security-from-gateway | cross-package | 1304.3 | 142.6 | cross_imports |
| cross-pkg-config-types-from-agents | cross-package | 1359.6 | 120.8 | cross_imports |
| cross-pkg-plugin-contracts-registry | cross-package | 1319.0 | 132.7 | cross_imports |

## Indexing Speed

| Repo | BM25 Init (s) |
|------|--------------|
| openclaw | 27.14 |

## Time to First Search

| Repo | Standard (s) | Deferred (s) | Speedup |
|------|-------------|--------------|---------|
| openclaw | 25.74 | 27.59 | 0.9x |

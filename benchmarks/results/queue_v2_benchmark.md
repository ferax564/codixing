# Queue Embedding v2 — Benchmark Results

**Date:** 2026-04-06 06:54

## Search Accuracy (OpenClaw)

| Method | Precision@10 | Recall@10 | MRR |
|--------|-------------|----------|-----|
| grep | 0.150 | 0.345 | 0.176 |
| codixing | 0.265 | 0.757 | 0.647 |

### By Category

| Category | Strategy | grep P@10 | grep R@10 | codixing P@10 | codixing R@10 | Best |
|----------|----------|----------|----------|--------------|--------------|------|
| concept | fast | 0.060 | 0.183 | 0.180 | 0.513 | codixing |
| cross-package | cross_imports | 0.040 | 0.080 | 0.233 | 0.800 | codixing |
| symbol | symbol_lookup | 0.140 | 0.600 | 0.180 | 1.000 | codixing |
| usage | usages | 0.360 | 0.515 | 0.469 | 0.715 | codixing |

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
| usage-channel-plugin-imports | usage | usages | 0.600 | 0.750 | 0.333 |
| usage-context-engine-imports | usage | usages | 0.400 | 0.500 | 1.000 |
| usage-load-config | usage | usages | 0.700 | 0.700 | 1.000 |
| concept-security-audit | concept | fast | 0.200 | 0.500 | 1.000 |
| concept-rate-limiting | concept | fast | 0.100 | 0.333 | 1.000 |
| concept-secret-redaction | concept | fast | 0.100 | 0.333 | 1.000 |
| concept-cron-scheduling | concept | fast | 0.200 | 0.400 | 0.500 |
| concept-system-prompt-composition | concept | fast | 0.300 | 1.000 | 1.000 |
| cross-pkg-plugin-sdk-entry | cross-package | cross_imports | 0.000 | 0.000 | 0.000 |
| cross-pkg-bundled-channel-entries | cross-package | cross_imports | 0.250 | 1.000 | 0.500 |
| cross-pkg-security-from-gateway | cross-package | cross_imports | 0.167 | 1.000 | 0.200 |
| cross-pkg-config-types-from-agents | cross-package | cross_imports | 0.500 | 1.000 | 1.000 |
| cross-pkg-plugin-contracts-registry | cross-package | cross_imports | 0.250 | 1.000 | 0.500 |

## Search Speed

| Method | Avg query time (ms) | Min | Max |
|--------|-------------------|-----|-----|
| grep | 946.0 | 519.0 | 1316.1 |
| codixing cross_imports | 76.4 | 73.8 | 80.4 |
| codixing fast | 354.5 | 332.4 | 389.4 |
| codixing symbol_lookup | 232.8 | 63.3 | 898.1 |
| codixing usages | 66.4 | 58.2 | 77.3 |

### Per-Query Timing

| Query | Category | grep (ms) | codixing (ms) | Strategy |
|-------|----------|----------|--------------|----------|
| symbol-channel-plugin | symbol | 1308.0 | 898.1 | symbol_lookup |
| symbol-gateway-server | symbol | 519.0 | 65.1 | symbol_lookup |
| symbol-context-engine-interface | symbol | 553.5 | 63.3 | symbol_lookup |
| symbol-openclaw-config | symbol | 586.2 | 72.0 | symbol_lookup |
| symbol-tool-policy-like | symbol | 554.9 | 65.3 | symbol_lookup |
| usage-redact-sensitive-text | usage | 1003.9 | 77.3 | usages |
| usage-create-auth-rate-limiter | usage | 1016.9 | 71.3 | usages |
| usage-channel-plugin-imports | usage | 992.0 | 63.3 | usages |
| usage-context-engine-imports | usage | 997.6 | 62.1 | usages |
| usage-load-config | usage | 1002.7 | 58.2 | usages |
| concept-security-audit | concept | 669.0 | 346.9 | fast |
| concept-rate-limiting | concept | 1316.1 | 389.4 | fast |
| concept-secret-redaction | concept | 744.2 | 332.4 | fast |
| concept-cron-scheduling | concept | 637.3 | 349.7 | fast |
| concept-system-prompt-composition | concept | 1229.2 | 353.9 | fast |
| cross-pkg-plugin-sdk-entry | cross-package | 1042.5 | 76.2 | cross_imports |
| cross-pkg-bundled-channel-entries | cross-package | 1125.6 | 76.4 | cross_imports |
| cross-pkg-security-from-gateway | cross-package | 1195.1 | 80.4 | cross_imports |
| cross-pkg-config-types-from-agents | cross-package | 1218.3 | 73.8 | cross_imports |
| cross-pkg-plugin-contracts-registry | cross-package | 1209.0 | 75.3 | cross_imports |

## Indexing Speed

| Repo | BM25 Init (s) |
|------|--------------|
| openclaw | 24.16 |

## Time to First Search

| Repo | Standard (s) | Deferred (s) | Speedup |
|------|-------------|--------------|---------|
| openclaw | 23.75 | 23.9 | 1.0x |

## Embedding Speed (sync vs parallel)

**Repo:** openclaw

| Workers | Time (s) | Speedup |
|---------|---------|---------|
| 1 (sync) | 0.03 | 1.0x |
| 4 (parallel) | 0.02 | 1.39x |

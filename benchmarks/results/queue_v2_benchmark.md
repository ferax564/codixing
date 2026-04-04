# Queue Embedding v2 — Benchmark Results

**Date:** 2026-04-03 23:20

## Search Accuracy (OpenClaw)

| Method | Precision@10 | Recall@10 | MRR |
|--------|-------------|----------|-----|
| grep | 0.150 | 0.345 | 0.176 |
| codixing | 0.251 | 0.710 | 0.631 |

### By Category

| Category | Strategy | grep P@10 | grep R@10 | codixing P@10 | codixing R@10 | Best |
|----------|----------|----------|----------|--------------|--------------|------|
| concept | fast | 0.060 | 0.183 | 0.100 | 0.300 | codixing |
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
| concept-rate-limiting | concept | fast | 0.100 | 0.333 | 1.000 |
| concept-secret-redaction | concept | fast | 0.100 | 0.333 | 1.000 |
| concept-cron-scheduling | concept | fast | 0.000 | 0.000 | 0.000 |
| concept-system-prompt-composition | concept | fast | 0.100 | 0.333 | 1.000 |
| cross-pkg-plugin-sdk-entry | cross-package | cross_imports | 0.000 | 0.000 | 0.000 |
| cross-pkg-bundled-channel-entries | cross-package | cross_imports | 0.250 | 1.000 | 0.500 |
| cross-pkg-security-from-gateway | cross-package | cross_imports | 0.167 | 1.000 | 0.200 |
| cross-pkg-config-types-from-agents | cross-package | cross_imports | 0.500 | 1.000 | 1.000 |
| cross-pkg-plugin-contracts-registry | cross-package | cross_imports | 0.250 | 1.000 | 0.500 |

## Search Speed

| Method | Avg query time (ms) | Min | Max |
|--------|-------------------|-----|-----|
| grep | 938.5 | 511.7 | 1368.3 |
| codixing cross_imports | 149.5 | 148.9 | 149.9 |
| codixing fast | 66903.8 | 55556.4 | 93273.5 |
| codixing symbol_lookup | 140.6 | 138.8 | 142.2 |
| codixing usages | 133.8 | 132.0 | 138.5 |

### Per-Query Timing

| Query | Category | grep (ms) | codixing (ms) | Strategy |
|-------|----------|----------|--------------|----------|
| symbol-channel-plugin | symbol | 1205.6 | 140.9 | symbol_lookup |
| symbol-gateway-server | symbol | 511.7 | 140.5 | symbol_lookup |
| symbol-context-engine-interface | symbol | 542.3 | 138.8 | symbol_lookup |
| symbol-openclaw-config | symbol | 520.2 | 140.5 | symbol_lookup |
| symbol-tool-policy-like | symbol | 533.8 | 142.2 | symbol_lookup |
| usage-redact-sensitive-text | usage | 993.2 | 132.9 | usages |
| usage-create-auth-rate-limiter | usage | 1007.4 | 132.6 | usages |
| usage-channel-plugin-imports | usage | 979.6 | 132.0 | usages |
| usage-context-engine-imports | usage | 974.7 | 138.5 | usages |
| usage-load-config | usage | 985.4 | 133.1 | usages |
| concept-security-audit | concept | 653.7 | 55995.8 | fast |
| concept-rate-limiting | concept | 1368.3 | 55556.4 | fast |
| concept-secret-redaction | concept | 761.1 | 93273.5 | fast |
| concept-cron-scheduling | concept | 684.0 | 55575.2 | fast |
| concept-system-prompt-composition | concept | 1271.9 | 74117.9 | fast |
| cross-pkg-plugin-sdk-entry | cross-package | 1081.3 | 149.8 | cross_imports |
| cross-pkg-bundled-channel-entries | cross-package | 1106.1 | 149.6 | cross_imports |
| cross-pkg-security-from-gateway | cross-package | 1180.9 | 148.9 | cross_imports |
| cross-pkg-config-types-from-agents | cross-package | 1199.8 | 149.2 | cross_imports |
| cross-pkg-plugin-contracts-registry | cross-package | 1209.9 | 149.9 | cross_imports |

## Indexing Speed

| Repo | BM25 Init (s) |
|------|--------------|
| openclaw | 22.83 |

## Time to First Search

| Repo | Standard (s) | Deferred (s) | Speedup |
|------|-------------|--------------|---------|
| openclaw | 23.17 | 23.32 | 1.0x |

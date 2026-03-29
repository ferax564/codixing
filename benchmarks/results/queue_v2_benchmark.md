# Queue Embedding v2 — Benchmark Results

**Date:** 2026-03-29 18:47

## Search Accuracy (OpenClaw)

| Method | Precision@10 | Recall@10 | MRR |
|--------|-------------|----------|-----|
| grep | 0.145 | 0.406 | 0.271 |
| codixing | 0.139 | 0.354 | 0.438 |

### By Category

| Category | Strategy | grep P@10 | grep R@10 | codixing P@10 | codixing R@10 | Best |
|----------|----------|----------|----------|--------------|--------------|------|
| concept | fast | 0.060 | 0.183 | 0.100 | 0.300 | codixing |
| cross-package | fast | 0.060 | 0.367 | 0.040 | 0.067 | grep |
| symbol | exact | 0.140 | 0.600 | 0.187 | 0.600 | codixing |
| usage | usages | 0.320 | 0.475 | 0.229 | 0.450 | grep |

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
| usage-load-config | usage | 0.000 | 0.000 | 0.000 |
| concept-security-audit | concept | 0.100 | 0.250 | 0.500 |
| concept-rate-limiting | concept | 0.000 | 0.000 | 0.000 |
| concept-secret-redaction | concept | 0.100 | 0.333 | 0.111 |
| concept-cron-scheduling | concept | 0.000 | 0.000 | 0.000 |
| concept-system-prompt-composition | concept | 0.100 | 0.333 | 0.200 |
| cross-pkg-plugin-sdk-entry | cross-package | 0.000 | 0.000 | 0.000 |
| cross-pkg-bundled-channel-entries | cross-package | 0.100 | 1.000 | 1.000 |
| cross-pkg-security-from-gateway | cross-package | 0.100 | 0.333 | 0.200 |
| cross-pkg-config-types-from-agents | cross-package | 0.000 | 0.000 | 0.000 |
| cross-pkg-plugin-contracts-registry | cross-package | 0.100 | 0.500 | 1.000 |

**codixing:**

| Query | Category | Strategy | P@10 | R@10 | MRR |
|-------|----------|----------|------|------|-----|
| symbol-channel-plugin | symbol | exact | 0.000 | 0.000 | 0.000 |
| symbol-gateway-server | symbol | exact | 0.333 | 1.000 | 1.000 |
| symbol-context-engine-interface | symbol | exact | 0.100 | 1.000 | 0.500 |
| symbol-openclaw-config | symbol | exact | 0.000 | 0.000 | 0.000 |
| symbol-tool-policy-like | symbol | exact | 0.500 | 1.000 | 0.500 |
| usage-redact-sensitive-text | usage | usages | 0.500 | 0.625 | 0.250 |
| usage-create-auth-rate-limiter | usage | usages | 0.143 | 1.000 | 0.500 |
| usage-channel-plugin-imports | usage | usages | 0.000 | 0.000 | 0.000 |
| usage-context-engine-imports | usage | usages | 0.500 | 0.625 | 1.000 |
| usage-load-config | usage | usages | 0.000 | 0.000 | 0.000 |
| concept-security-audit | concept | fast | 0.200 | 0.500 | 1.000 |
| concept-rate-limiting | concept | fast | 0.100 | 0.333 | 1.000 |
| concept-secret-redaction | concept | fast | 0.100 | 0.333 | 1.000 |
| concept-cron-scheduling | concept | fast | 0.000 | 0.000 | 0.000 |
| concept-system-prompt-composition | concept | fast | 0.100 | 0.333 | 1.000 |
| cross-pkg-plugin-sdk-entry | cross-package | fast | 0.200 | 0.333 | 1.000 |
| cross-pkg-bundled-channel-entries | cross-package | fast | 0.000 | 0.000 | 0.000 |
| cross-pkg-security-from-gateway | cross-package | fast | 0.000 | 0.000 | 0.000 |
| cross-pkg-config-types-from-agents | cross-package | fast | 0.000 | 0.000 | 0.000 |
| cross-pkg-plugin-contracts-registry | cross-package | fast | 0.000 | 0.000 | 0.000 |

## Search Speed

| Method | Avg query time (ms) | Min | Max |
|--------|-------------------|-----|-----|
| grep | 1011.6 | 541.6 | 1607.9 |
| codixing exact | 1904.1 | 1847.8 | 2038.5 |
| codixing fast | 46.1 | 38.7 | 53.7 |
| codixing usages | 11.6 | 8.6 | 15.1 |

### Per-Query Timing

| Query | Category | grep (ms) | codixing (ms) | Strategy |
|-------|----------|----------|--------------|----------|
| symbol-channel-plugin | symbol | 1607.9 | 2038.5 | exact |
| symbol-gateway-server | symbol | 579.2 | 1874.7 | exact |
| symbol-context-engine-interface | symbol | 556.6 | 1889.6 | exact |
| symbol-openclaw-config | symbol | 541.6 | 1869.9 | exact |
| symbol-tool-policy-like | symbol | 545.7 | 1847.8 | exact |
| usage-redact-sensitive-text | usage | 1013.5 | 13.4 | usages |
| usage-create-auth-rate-limiter | usage | 1038.8 | 15.1 | usages |
| usage-channel-plugin-imports | usage | 1006.6 | 10.5 | usages |
| usage-context-engine-imports | usage | 994.4 | 8.6 | usages |
| usage-load-config | usage | 1030.6 | 10.3 | usages |
| concept-security-audit | concept | 691.0 | 51.9 | fast |
| concept-rate-limiting | concept | 1370.2 | 46.1 | fast |
| concept-secret-redaction | concept | 789.1 | 46.0 | fast |
| concept-cron-scheduling | concept | 724.7 | 44.3 | fast |
| concept-system-prompt-composition | concept | 1316.0 | 38.7 | fast |
| cross-pkg-plugin-sdk-entry | cross-package | 1124.3 | 50.5 | fast |
| cross-pkg-bundled-channel-entries | cross-package | 1279.2 | 41.7 | fast |
| cross-pkg-security-from-gateway | cross-package | 1261.4 | 53.7 | fast |
| cross-pkg-config-types-from-agents | cross-package | 1488.7 | 47.9 | fast |
| cross-pkg-plugin-contracts-registry | cross-package | 1273.4 | 40.2 | fast |

## Indexing Speed

| Repo | BM25 Init (s) |
|------|--------------|
| openclaw | 30.86 |

## Time to First Search

| Repo | Standard (s) | Deferred (s) | Speedup |
|------|-------------|--------------|---------|
| openclaw | 32.01 | 32.05 | 1.0x |

# Queue Embedding v2 — Benchmark Results

**Date:** 2026-03-29 21:47

## Search Accuracy (OpenClaw)

| Method | Precision@10 | Recall@10 | MRR |
|--------|-------------|----------|-----|
| grep | 0.145 | 0.406 | 0.271 |
| codixing | 0.137 | 0.456 | 0.475 |

### By Category

| Category | Strategy | grep P@10 | grep R@10 | codixing P@10 | codixing R@10 | Best |
|----------|----------|----------|----------|--------------|--------------|------|
| concept | fast | 0.060 | 0.183 | 0.120 | 0.340 | codixing |
| cross-package | callers | 0.060 | 0.367 | 0.020 | 0.033 | grep |
| symbol | symbol_lookup | 0.140 | 0.600 | 0.180 | 1.000 | codixing |
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
| symbol-channel-plugin | symbol | symbol_lookup | 0.100 | 1.000 | 0.167 |
| symbol-gateway-server | symbol | symbol_lookup | 0.100 | 1.000 | 1.000 |
| symbol-context-engine-interface | symbol | symbol_lookup | 0.100 | 1.000 | 0.500 |
| symbol-openclaw-config | symbol | symbol_lookup | 0.100 | 1.000 | 0.500 |
| symbol-tool-policy-like | symbol | symbol_lookup | 0.500 | 1.000 | 1.000 |
| usage-redact-sensitive-text | usage | usages | 0.500 | 0.625 | 0.250 |
| usage-create-auth-rate-limiter | usage | usages | 0.143 | 1.000 | 0.500 |
| usage-channel-plugin-imports | usage | usages | 0.000 | 0.000 | 0.000 |
| usage-context-engine-imports | usage | usages | 0.500 | 0.625 | 1.000 |
| usage-load-config | usage | usages | 0.000 | 0.000 | 0.000 |
| concept-security-audit | concept | fast | 0.200 | 0.500 | 1.000 |
| concept-rate-limiting | concept | fast | 0.100 | 0.333 | 1.000 |
| concept-secret-redaction | concept | fast | 0.100 | 0.333 | 1.000 |
| concept-cron-scheduling | concept | fast | 0.100 | 0.200 | 0.250 |
| concept-system-prompt-composition | concept | fast | 0.100 | 0.333 | 1.000 |
| cross-pkg-plugin-sdk-entry | cross-package | callers | 0.100 | 0.167 | 0.333 |
| cross-pkg-bundled-channel-entries | cross-package | callers | 0.000 | 0.000 | 0.000 |
| cross-pkg-security-from-gateway | cross-package | callers | 0.000 | 0.000 | 0.000 |
| cross-pkg-config-types-from-agents | cross-package | callers | 0.000 | 0.000 | 0.000 |
| cross-pkg-plugin-contracts-registry | cross-package | callers | 0.000 | 0.000 | 0.000 |

## Search Speed

| Method | Avg query time (ms) | Min | Max |
|--------|-------------------|-----|-----|
| grep | 909.5 | 509.8 | 1323.0 |
| codixing callers | 51.3 | 41.2 | 66.6 |
| codixing fast | 217.9 | 207.2 | 235.3 |
| codixing symbol_lookup | 51.8 | 50.2 | 54.8 |
| codixing usages | 42.7 | 41.1 | 45.6 |

### Per-Query Timing

| Query | Category | grep (ms) | codixing (ms) | Strategy |
|-------|----------|----------|--------------|----------|
| symbol-channel-plugin | symbol | 617.1 | 50.8 | symbol_lookup |
| symbol-gateway-server | symbol | 509.8 | 50.2 | symbol_lookup |
| symbol-context-engine-interface | symbol | 542.1 | 50.7 | symbol_lookup |
| symbol-openclaw-config | symbol | 528.3 | 54.8 | symbol_lookup |
| symbol-tool-policy-like | symbol | 556.7 | 52.7 | symbol_lookup |
| usage-redact-sensitive-text | usage | 1059.4 | 42.8 | usages |
| usage-create-auth-rate-limiter | usage | 1030.2 | 42.6 | usages |
| usage-channel-plugin-imports | usage | 986.5 | 41.6 | usages |
| usage-context-engine-imports | usage | 991.7 | 45.6 | usages |
| usage-load-config | usage | 1008.1 | 41.1 | usages |
| concept-security-audit | concept | 663.8 | 211.3 | fast |
| concept-rate-limiting | concept | 1323.0 | 212.1 | fast |
| concept-secret-redaction | concept | 715.0 | 223.8 | fast |
| concept-cron-scheduling | concept | 638.8 | 207.2 | fast |
| concept-system-prompt-composition | concept | 1243.0 | 235.3 | fast |
| cross-pkg-plugin-sdk-entry | cross-package | 1040.3 | 41.2 | callers |
| cross-pkg-bundled-channel-entries | cross-package | 1117.8 | 41.3 | callers |
| cross-pkg-security-from-gateway | cross-package | 1204.0 | 66.6 | callers |
| cross-pkg-config-types-from-agents | cross-package | 1208.1 | 66.0 | callers |
| cross-pkg-plugin-contracts-registry | cross-package | 1207.0 | 41.2 | callers |

## Indexing Speed

| Repo | BM25 Init (s) |
|------|--------------|
| openclaw | 23.49 |

## Time to First Search

| Repo | Standard (s) | Deferred (s) | Speedup |
|------|-------------|--------------|---------|
| openclaw | 24.1 | 23.38 | 1.0x |

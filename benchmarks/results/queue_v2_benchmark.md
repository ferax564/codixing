# Queue Embedding v2 — Benchmark Results

**Date:** 2026-03-29 22:00

## Search Accuracy (OpenClaw)

| Method | Precision@10 | Recall@10 | MRR |
|--------|-------------|----------|-----|
| grep | 0.130 | 0.315 | 0.161 |
| codixing | 0.212 | 0.607 | 0.554 |

### By Category

| Category | Strategy | grep P@10 | grep R@10 | codixing P@10 | codixing R@10 | Best |
|----------|----------|----------|----------|--------------|--------------|------|
| concept | fast | 0.060 | 0.183 | 0.120 | 0.340 | codixing |
| cross-package | cross_imports | 0.000 | 0.000 | 0.320 | 0.640 | codixing |
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
| cross-pkg-bundled-channel-entries | cross-package | 0.000 | 0.000 | 0.000 |
| cross-pkg-security-from-gateway | cross-package | 0.000 | 0.000 | 0.000 |
| cross-pkg-config-types-from-agents | cross-package | 0.000 | 0.000 | 0.000 |
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
| usage-channel-plugin-imports | usage | usages | 0.000 | 0.000 | 0.000 |
| usage-context-engine-imports | usage | usages | 0.500 | 0.625 | 1.000 |
| usage-load-config | usage | usages | 0.000 | 0.000 | 0.000 |
| concept-security-audit | concept | fast | 0.200 | 0.500 | 1.000 |
| concept-rate-limiting | concept | fast | 0.100 | 0.333 | 1.000 |
| concept-secret-redaction | concept | fast | 0.100 | 0.333 | 1.000 |
| concept-cron-scheduling | concept | fast | 0.100 | 0.200 | 0.250 |
| concept-system-prompt-composition | concept | fast | 0.100 | 0.333 | 1.000 |
| cross-pkg-plugin-sdk-entry | cross-package | cross_imports | 0.100 | 0.200 | 0.250 |
| cross-pkg-bundled-channel-entries | cross-package | cross_imports | 0.250 | 1.000 | 0.333 |
| cross-pkg-security-from-gateway | cross-package | cross_imports | 1.000 | 1.000 | 1.000 |
| cross-pkg-config-types-from-agents | cross-package | cross_imports | 0.000 | 0.000 | 0.000 |
| cross-pkg-plugin-contracts-registry | cross-package | cross_imports | 0.250 | 1.000 | 0.333 |

## Search Speed

| Method | Avg query time (ms) | Min | Max |
|--------|-------------------|-----|-----|
| grep | 962.4 | 528.0 | 1347.0 |
| codixing cross_imports | 41.8 | 40.3 | 44.1 |
| codixing fast | 247.6 | 240.7 | 255.4 |
| codixing symbol_lookup | 55.5 | 53.5 | 59.0 |
| codixing usages | 54.3 | 51.3 | 58.8 |

### Per-Query Timing

| Query | Category | grep (ms) | codixing (ms) | Strategy |
|-------|----------|----------|--------------|----------|
| symbol-channel-plugin | symbol | 1311.8 | 56.0 | symbol_lookup |
| symbol-gateway-server | symbol | 528.0 | 55.5 | symbol_lookup |
| symbol-context-engine-interface | symbol | 559.3 | 53.5 | symbol_lookup |
| symbol-openclaw-config | symbol | 552.2 | 59.0 | symbol_lookup |
| symbol-tool-policy-like | symbol | 558.9 | 53.7 | symbol_lookup |
| usage-redact-sensitive-text | usage | 1060.1 | 54.9 | usages |
| usage-create-auth-rate-limiter | usage | 1053.9 | 58.8 | usages |
| usage-channel-plugin-imports | usage | 1011.3 | 51.3 | usages |
| usage-context-engine-imports | usage | 1035.5 | 51.8 | usages |
| usage-load-config | usage | 1029.0 | 54.7 | usages |
| concept-security-audit | concept | 678.8 | 242.1 | fast |
| concept-rate-limiting | concept | 1347.0 | 247.0 | fast |
| concept-secret-redaction | concept | 742.5 | 252.7 | fast |
| concept-cron-scheduling | concept | 658.5 | 240.7 | fast |
| concept-system-prompt-composition | concept | 1262.9 | 255.4 | fast |
| cross-pkg-plugin-sdk-entry | cross-package | 1043.8 | 41.2 | cross_imports |
| cross-pkg-bundled-channel-entries | cross-package | 1126.7 | 42.2 | cross_imports |
| cross-pkg-security-from-gateway | cross-package | 1199.1 | 40.3 | cross_imports |
| cross-pkg-config-types-from-agents | cross-package | 1209.2 | 44.1 | cross_imports |
| cross-pkg-plugin-contracts-registry | cross-package | 1280.2 | 41.1 | cross_imports |

## Indexing Speed

| Repo | BM25 Init (s) |
|------|--------------|
| openclaw | 23.79 |

## Time to First Search

| Repo | Standard (s) | Deferred (s) | Speedup |
|------|-------------|--------------|---------|
| openclaw | 24.15 | 23.82 | 1.0x |

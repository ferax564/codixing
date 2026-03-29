# Queue Embedding v2 — Benchmark Results

**Date:** 2026-03-29 13:56

## Search Accuracy (OpenClaw)

| Method | Precision@10 | Recall@10 | MRR |
|--------|-------------|----------|-----|
| grep | 0.302 | 0.506 | 0.461 |
| codixing | 0.070 | 0.310 | 0.425 |

### By Category

| Category | Strategy | grep P@10 | grep R@10 | codixing P@10 | codixing R@10 | Best |
|----------|----------|----------|----------|--------------|--------------|------|
| concept | fast | 0.060 | 0.183 | 0.100 | 0.300 | codixing |
| cross-package | fast | 0.060 | 0.367 | 0.040 | 0.067 | grep |
| symbol | exact | 0.767 | 1.000 | 0.060 | 0.600 | grep |
| usage | exact | 0.320 | 0.475 | 0.080 | 0.275 | grep |

### Per-Query Breakdown

**grep:**

| Query | Category | P@10 | R@10 | MRR |
|-------|----------|------|------|-----|
| symbol-channel-plugin | symbol | 0.333 | 1.000 | 0.500 |
| symbol-gateway-server | symbol | 0.500 | 1.000 | 1.000 |
| symbol-context-engine-interface | symbol | 1.000 | 1.000 | 1.000 |
| symbol-openclaw-config | symbol | 1.000 | 1.000 | 1.000 |
| symbol-tool-policy-like | symbol | 1.000 | 1.000 | 1.000 |
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
| symbol-gateway-server | symbol | exact | 0.100 | 1.000 | 1.000 |
| symbol-context-engine-interface | symbol | exact | 0.100 | 1.000 | 1.000 |
| symbol-openclaw-config | symbol | exact | 0.000 | 0.000 | 0.000 |
| symbol-tool-policy-like | symbol | exact | 0.100 | 1.000 | 0.500 |
| usage-redact-sensitive-text | usage | exact | 0.000 | 0.000 | 0.000 |
| usage-create-auth-rate-limiter | usage | exact | 0.100 | 1.000 | 0.500 |
| usage-channel-plugin-imports | usage | exact | 0.000 | 0.000 | 0.000 |
| usage-context-engine-imports | usage | exact | 0.300 | 0.375 | 1.000 |
| usage-load-config | usage | exact | 0.000 | 0.000 | 0.000 |
| concept-security-audit | concept | fast | 0.200 | 0.500 | 1.000 |
| concept-rate-limiting | concept | fast | 0.100 | 0.333 | 1.000 |
| concept-secret-redaction | concept | fast | 0.100 | 0.333 | 1.000 |
| concept-cron-scheduling | concept | fast | 0.000 | 0.000 | 0.000 |
| concept-system-prompt-composition | concept | fast | 0.100 | 0.333 | 1.000 |
| cross-pkg-plugin-sdk-entry | cross-package | fast | 0.200 | 0.333 | 0.500 |
| cross-pkg-bundled-channel-entries | cross-package | fast | 0.000 | 0.000 | 0.000 |
| cross-pkg-security-from-gateway | cross-package | fast | 0.000 | 0.000 | 0.000 |
| cross-pkg-config-types-from-agents | cross-package | fast | 0.000 | 0.000 | 0.000 |
| cross-pkg-plugin-contracts-registry | cross-package | fast | 0.000 | 0.000 | 0.000 |

## Search Speed

| Method | Avg query time (ms) | Min | Max |
|--------|-------------------|-----|-----|
| grep | 1047.7 | 638.4 | 1815.1 |
| codixing exact | 583.5 | 437.1 | 1569.2 |
| codixing fast | 245.6 | 208.8 | 333.3 |

### Per-Query Timing

| Query | Category | grep (ms) | codixing (ms) | Strategy |
|-------|----------|----------|--------------|----------|
| symbol-channel-plugin | symbol | 1815.1 | 1569.2 | exact |
| symbol-gateway-server | symbol | 937.5 | 464.8 | exact |
| symbol-context-engine-interface | symbol | 880.9 | 459.0 | exact |
| symbol-openclaw-config | symbol | 974.7 | 653.7 | exact |
| symbol-tool-policy-like | symbol | 900.8 | 444.2 | exact |
| usage-redact-sensitive-text | usage | 1056.1 | 449.6 | exact |
| usage-create-auth-rate-limiter | usage | 1011.5 | 444.6 | exact |
| usage-channel-plugin-imports | usage | 987.4 | 450.0 | exact |
| usage-context-engine-imports | usage | 992.5 | 462.9 | exact |
| usage-load-config | usage | 1041.1 | 437.1 | exact |
| concept-security-audit | concept | 654.9 | 210.8 | fast |
| concept-rate-limiting | concept | 1302.4 | 209.8 | fast |
| concept-secret-redaction | concept | 715.3 | 228.3 | fast |
| concept-cron-scheduling | concept | 638.4 | 208.8 | fast |
| concept-system-prompt-composition | concept | 1254.3 | 241.2 | fast |
| cross-pkg-plugin-sdk-entry | cross-package | 1035.8 | 229.4 | fast |
| cross-pkg-bundled-channel-entries | cross-package | 1130.2 | 333.3 | fast |
| cross-pkg-security-from-gateway | cross-package | 1209.5 | 267.2 | fast |
| cross-pkg-config-types-from-agents | cross-package | 1207.9 | 225.3 | fast |
| cross-pkg-plugin-contracts-registry | cross-package | 1207.9 | 302.0 | fast |

## Indexing Speed

| Repo | BM25 Init (s) |
|------|--------------|
| openclaw | 27.61 |

## Time to First Search

| Repo | Standard (s) | Deferred (s) | Speedup |
|------|-------------|--------------|---------|
| openclaw | 27.64 | 27.43 | 1.0x |

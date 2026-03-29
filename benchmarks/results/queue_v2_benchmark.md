# Queue Embedding v2 — Benchmark Results

**Date:** 2026-03-29 19:03

## Search Accuracy (OpenClaw)

| Method | Precision@10 | Recall@10 | MRR |
|--------|-------------|----------|-----|
| grep | 0.145 | 0.406 | 0.271 |
| codixing | 0.137 | 0.454 | 0.496 |

### By Category

| Category | Strategy | grep P@10 | grep R@10 | codixing P@10 | codixing R@10 | Best |
|----------|----------|----------|----------|--------------|--------------|------|
| concept | fast | 0.060 | 0.183 | 0.100 | 0.300 | codixing |
| cross-package | explore | 0.060 | 0.367 | 0.040 | 0.067 | grep |
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
| concept-cron-scheduling | concept | fast | 0.000 | 0.000 | 0.000 |
| concept-system-prompt-composition | concept | fast | 0.100 | 0.333 | 1.000 |
| cross-pkg-plugin-sdk-entry | cross-package | explore | 0.200 | 0.333 | 1.000 |
| cross-pkg-bundled-channel-entries | cross-package | explore | 0.000 | 0.000 | 0.000 |
| cross-pkg-security-from-gateway | cross-package | explore | 0.000 | 0.000 | 0.000 |
| cross-pkg-config-types-from-agents | cross-package | explore | 0.000 | 0.000 | 0.000 |
| cross-pkg-plugin-contracts-registry | cross-package | explore | 0.000 | 0.000 | 0.000 |

## Search Speed

| Method | Avg query time (ms) | Min | Max |
|--------|-------------------|-----|-----|
| grep | 962.4 | 520.2 | 1323.6 |
| codixing explore | 72.9 | 70.5 | 75.3 |
| codixing fast | 242.1 | 224.2 | 262.2 |
| codixing symbol_lookup | 60.1 | 51.4 | 90.8 |
| codixing usages | 52.6 | 47.0 | 60.8 |

### Per-Query Timing

| Query | Category | grep (ms) | codixing (ms) | Strategy |
|-------|----------|----------|--------------|----------|
| symbol-channel-plugin | symbol | 1321.5 | 90.8 | symbol_lookup |
| symbol-gateway-server | symbol | 520.2 | 53.0 | symbol_lookup |
| symbol-context-engine-interface | symbol | 551.4 | 51.4 | symbol_lookup |
| symbol-openclaw-config | symbol | 529.6 | 53.2 | symbol_lookup |
| symbol-tool-policy-like | symbol | 540.0 | 52.3 | symbol_lookup |
| usage-redact-sensitive-text | usage | 1010.0 | 60.8 | usages |
| usage-create-auth-rate-limiter | usage | 1022.6 | 55.5 | usages |
| usage-channel-plugin-imports | usage | 1043.4 | 50.8 | usages |
| usage-context-engine-imports | usage | 1084.1 | 49.0 | usages |
| usage-load-config | usage | 1036.5 | 47.0 | usages |
| concept-security-audit | concept | 672.9 | 262.2 | fast |
| concept-rate-limiting | concept | 1323.6 | 246.0 | fast |
| concept-secret-redaction | concept | 723.5 | 235.6 | fast |
| concept-cron-scheduling | concept | 663.2 | 224.2 | fast |
| concept-system-prompt-composition | concept | 1230.1 | 242.5 | fast |
| cross-pkg-plugin-sdk-entry | cross-package | 1048.5 | 75.3 | explore |
| cross-pkg-bundled-channel-entries | cross-package | 1150.7 | 74.8 | explore |
| cross-pkg-security-from-gateway | cross-package | 1304.2 | 72.7 | explore |
| cross-pkg-config-types-from-agents | cross-package | 1250.9 | 70.5 | explore |
| cross-pkg-plugin-contracts-registry | cross-package | 1220.7 | 71.2 | explore |

## Indexing Speed

| Repo | BM25 Init (s) |
|------|--------------|
| openclaw | 26.5 |

## Time to First Search

| Repo | Standard (s) | Deferred (s) | Speedup |
|------|-------------|--------------|---------|
| openclaw | 24.81 | 25.27 | 1.0x |

# Queue Embedding v2 — Benchmark Results

**Date:** 2026-03-29 13:43

## Search Accuracy (OpenClaw)

| Method | Precision@10 | Recall@10 | MRR |
|--------|-------------|----------|-----|
| grep | 0.282 | 0.410 | 0.370 |
| bm25 | 0.070 | 0.310 | 0.425 |
| hybrid | 0.095 | 0.342 | 0.383 |

### Per-Query Breakdown

**grep:**

| Query | P@10 | R@10 | MRR |
|-------|------|------|-----|
| symbol-channel-plugin | 0.333 | 1.000 | 0.500 |
| symbol-gateway-server | 0.500 | 1.000 | 1.000 |
| symbol-context-engine-interface | 1.000 | 1.000 | 1.000 |
| symbol-openclaw-config | 1.000 | 1.000 | 1.000 |
| symbol-tool-policy-like | 1.000 | 1.000 | 1.000 |
| usage-redact-sensitive-text | 0.700 | 0.875 | 0.500 |
| usage-create-auth-rate-limiter | 0.500 | 1.000 | 1.000 |
| usage-channel-plugin-imports | 0.000 | 0.000 | 0.000 |
| usage-context-engine-imports | 0.400 | 0.500 | 0.200 |
| usage-load-config | 0.000 | 0.000 | 0.000 |
| concept-security-audit | 0.000 | 0.000 | 0.000 |
| concept-rate-limiting | 0.000 | 0.000 | 0.000 |
| concept-secret-redaction | 0.000 | 0.000 | 0.000 |
| concept-cron-scheduling | 0.000 | 0.000 | 0.000 |
| concept-system-prompt-composition | 0.000 | 0.000 | 0.000 |
| cross-pkg-plugin-sdk-entry | 0.000 | 0.000 | 0.000 |
| cross-pkg-bundled-channel-entries | 0.000 | 0.000 | 0.000 |
| cross-pkg-security-from-gateway | 0.100 | 0.333 | 0.200 |
| cross-pkg-config-types-from-agents | 0.000 | 0.000 | 0.000 |
| cross-pkg-plugin-contracts-registry | 0.100 | 0.500 | 1.000 |

**bm25:**

| Query | P@10 | R@10 | MRR |
|-------|------|------|-----|
| symbol-channel-plugin | 0.000 | 0.000 | 0.000 |
| symbol-gateway-server | 0.100 | 1.000 | 1.000 |
| symbol-context-engine-interface | 0.100 | 1.000 | 1.000 |
| symbol-openclaw-config | 0.000 | 0.000 | 0.000 |
| symbol-tool-policy-like | 0.100 | 1.000 | 0.500 |
| usage-redact-sensitive-text | 0.000 | 0.000 | 0.000 |
| usage-create-auth-rate-limiter | 0.100 | 1.000 | 0.500 |
| usage-channel-plugin-imports | 0.000 | 0.000 | 0.000 |
| usage-context-engine-imports | 0.300 | 0.375 | 1.000 |
| usage-load-config | 0.000 | 0.000 | 0.000 |
| concept-security-audit | 0.200 | 0.500 | 1.000 |
| concept-rate-limiting | 0.100 | 0.333 | 1.000 |
| concept-secret-redaction | 0.100 | 0.333 | 0.500 |
| concept-cron-scheduling | 0.000 | 0.000 | 0.000 |
| concept-system-prompt-composition | 0.100 | 0.333 | 1.000 |
| cross-pkg-plugin-sdk-entry | 0.200 | 0.333 | 1.000 |
| cross-pkg-bundled-channel-entries | 0.000 | 0.000 | 0.000 |
| cross-pkg-security-from-gateway | 0.000 | 0.000 | 0.000 |
| cross-pkg-config-types-from-agents | 0.000 | 0.000 | 0.000 |
| cross-pkg-plugin-contracts-registry | 0.000 | 0.000 | 0.000 |

**hybrid:**

| Query | P@10 | R@10 | MRR |
|-------|------|------|-----|
| symbol-channel-plugin | 0.000 | 0.000 | 0.000 |
| symbol-gateway-server | 0.100 | 1.000 | 0.167 |
| symbol-context-engine-interface | 0.100 | 1.000 | 0.333 |
| symbol-openclaw-config | 0.000 | 0.000 | 0.000 |
| symbol-tool-policy-like | 0.100 | 1.000 | 0.500 |
| usage-redact-sensitive-text | 0.400 | 0.500 | 0.333 |
| usage-create-auth-rate-limiter | 0.100 | 1.000 | 0.333 |
| usage-channel-plugin-imports | 0.100 | 0.143 | 0.500 |
| usage-context-engine-imports | 0.300 | 0.375 | 1.000 |
| usage-load-config | 0.000 | 0.000 | 0.000 |
| concept-security-audit | 0.200 | 0.500 | 1.000 |
| concept-rate-limiting | 0.100 | 0.333 | 1.000 |
| concept-secret-redaction | 0.100 | 0.333 | 1.000 |
| concept-cron-scheduling | 0.000 | 0.000 | 0.000 |
| concept-system-prompt-composition | 0.100 | 0.333 | 1.000 |
| cross-pkg-plugin-sdk-entry | 0.200 | 0.333 | 0.500 |
| cross-pkg-bundled-channel-entries | 0.000 | 0.000 | 0.000 |
| cross-pkg-security-from-gateway | 0.000 | 0.000 | 0.000 |
| cross-pkg-config-types-from-agents | 0.000 | 0.000 | 0.000 |
| cross-pkg-plugin-contracts-registry | 0.000 | 0.000 | 0.000 |

## Indexing Speed

| Repo | BM25 Init (s) |
|------|--------------|
| openclaw | 27.62 |

## Time to First Search

| Repo | Standard (s) | Deferred (s) | Speedup |
|------|-------------|--------------|---------|
| openclaw | 27.47 | 27.69 | 1.0x |

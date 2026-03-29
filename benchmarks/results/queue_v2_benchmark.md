# Queue Embedding v2 — Benchmark Results

**Date:** 2026-03-29 12:19

## Search Accuracy (OpenClaw)

| Method | Precision@10 | Recall@10 | MRR |
|--------|-------------|----------|-----|
| grep | 0.282 | 0.410 | 0.370 |
| bm25 | 0.070 | 0.325 | 0.380 |
| hybrid | 0.112 | 0.488 | 0.360 |

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
| symbol-gateway-server | 0.000 | 0.000 | 0.000 |
| symbol-context-engine-interface | 0.100 | 1.000 | 1.000 |
| symbol-openclaw-config | 0.000 | 0.000 | 0.000 |
| symbol-tool-policy-like | 0.200 | 2.000 | 0.500 |
| usage-redact-sensitive-text | 0.100 | 0.125 | 0.100 |
| usage-create-auth-rate-limiter | 0.100 | 1.000 | 0.500 |
| usage-channel-plugin-imports | 0.000 | 0.000 | 0.000 |
| usage-context-engine-imports | 0.100 | 0.125 | 1.000 |
| usage-load-config | 0.000 | 0.000 | 0.000 |
| concept-security-audit | 0.100 | 0.250 | 1.000 |
| concept-rate-limiting | 0.100 | 0.333 | 1.000 |
| concept-secret-redaction | 0.100 | 0.333 | 0.500 |
| concept-cron-scheduling | 0.000 | 0.000 | 0.000 |
| concept-system-prompt-composition | 0.300 | 1.000 | 1.000 |
| cross-pkg-plugin-sdk-entry | 0.200 | 0.333 | 1.000 |
| cross-pkg-bundled-channel-entries | 0.000 | 0.000 | 0.000 |
| cross-pkg-security-from-gateway | 0.000 | 0.000 | 0.000 |
| cross-pkg-config-types-from-agents | 0.000 | 0.000 | 0.000 |
| cross-pkg-plugin-contracts-registry | 0.000 | 0.000 | 0.000 |

**hybrid:**

| Query | P@10 | R@10 | MRR |
|-------|------|------|-----|
| symbol-channel-plugin | 0.000 | 0.000 | 0.000 |
| symbol-gateway-server | 0.000 | 0.000 | 0.000 |
| symbol-context-engine-interface | 0.429 | 3.000 | 0.200 |
| symbol-openclaw-config | 0.000 | 0.000 | 0.000 |
| symbol-tool-policy-like | 0.333 | 3.000 | 0.500 |
| usage-redact-sensitive-text | 0.000 | 0.000 | 0.000 |
| usage-create-auth-rate-limiter | 0.100 | 1.000 | 0.500 |
| usage-channel-plugin-imports | 0.111 | 0.143 | 0.500 |
| usage-context-engine-imports | 0.429 | 0.375 | 1.000 |
| usage-load-config | 0.000 | 0.000 | 0.000 |
| concept-security-audit | 0.100 | 0.250 | 1.000 |
| concept-rate-limiting | 0.143 | 0.333 | 1.000 |
| concept-secret-redaction | 0.100 | 0.333 | 1.000 |
| concept-cron-scheduling | 0.000 | 0.000 | 0.000 |
| concept-system-prompt-composition | 0.300 | 1.000 | 1.000 |
| cross-pkg-plugin-sdk-entry | 0.200 | 0.333 | 0.500 |
| cross-pkg-bundled-channel-entries | 0.000 | 0.000 | 0.000 |
| cross-pkg-security-from-gateway | 0.000 | 0.000 | 0.000 |
| cross-pkg-config-types-from-agents | 0.000 | 0.000 | 0.000 |
| cross-pkg-plugin-contracts-registry | 0.000 | 0.000 | 0.000 |

## Indexing Speed

| Repo | Files | Chunks | Symbols | BM25 Init (s) |
|------|-------|--------|---------|--------------|
| openclaw | 8,600 | ~45,000 | ~48,000 | 29.3 |
| linux | 73,376 | 881,711 | 935,342 | 534.0 |

## Time to First Search

| Repo | Standard (s) | Deferred (s) | Speedup |
|------|-------------|--------------|---------|
| openclaw | 30.27 | 27.78 | 1.1x |

Note: Linux kernel TTFS with deferred embeddings requires ONNX runtime
for a meaningful comparison (both paths skip embedding without it).
With embeddings enabled, standard init would block for ~60+ minutes
while deferred returns BM25-only in ~534s.

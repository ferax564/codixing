# Embedding Model Benchmark Results

**Date:** 2026-03-10
**Hardware:** AMD Ryzen 7 6800H (16 threads) + Radeon 680M iGPU (RDNA 2, Vulkan)
**Frameworks:** llama.cpp (Vulkan + CPU), sentence-transformers (PyTorch CPU)
**Corpus:** 200 chunks from 79 Rust source files (codixing codebase)

## Speed Ranking (medium-length texts, 20 texts batch)

| # | Model | Backend | Med/s | Long/s | Dims |
|---|-------|---------|------:|-------:|-----:|
| 1 | bge-small-en-v1.5 | st (PyTorch CPU) | **157** | 57 | 384 |
| 2 | bge-small-en-v1.5 Q8 | llama.cpp Vulkan | **84** | **56** | 384 |
| 3 | bge-base-en-v1.5 | st (PyTorch CPU) | 69 | 19 | 768 |
| 4 | bge-small-en-v1.5 Q8 | llama.cpp CPU | 67 | 30 | 384 |
| 5 | bge-base-en-v1.5 Q8 | llama.cpp Vulkan | 55 | 32 | 768 |
| 6 | nomic-embed-text-v1.5 Q8 | llama.cpp Vulkan | 51 | 27 | 768 |
| 7 | nomic-embed-text-v1.5 | st (PyTorch CPU) | 47 | 13 | 768 |
| 8 | SweRankEmbed-Small | st (PyTorch CPU) | 38 | 12 | 768 |
| 9 | bge-large-en-v1.5 Q8 | llama.cpp Vulkan | 27 | 12 | 1024 |
| 10 | bge-large-en-v1.5 | st (PyTorch CPU) | 20 | 6 | 1024 |
| 11 | Qwen3-Embedding-0.6B Q8 | llama.cpp Vulkan | 11 | 7 | 1024 |
| 12 | nomic-embed-text-v2-moe Q8 | llama.cpp CPU | 11 | 8 | 768 |

## Accuracy Ranking (MRR@10 on codixing code search)

| # | Model | Backend | MRR@10 | Hit@1 | Hit@5 | Enc Rate |
|---|-------|---------|-------:|------:|------:|---------:|
| 1 | bge-large-en-v1.5 | st (PyTorch) | **0.750** | 60% | **90%** | 2/s |
| 2 | bge-small-en-v1.5 | st (PyTorch) | **0.683** | 50% | **90%** | 21/s |
| 3 | SweRankEmbed-Small | st (PyTorch) | **0.683** | **60%** | 80% | 4/s |
| 4 | nomic-embed-text-v1.5 | st (PyTorch) | 0.661 | **60%** | 70% | 4/s |
| 5 | bge-base-en-v1.5 | st (PyTorch) | 0.578 | 50% | 80% | 7/s |
| 6 | bge-large-en-v1.5 Q8 | llama.cpp | 0.542 | 40% | 80% | 5/s |
| 7 | nomic-embed-text-v2-moe Q8 | llama.cpp | 0.517 | 40% | 60% | 9/s |
| 8 | bge-small-en-v1.5 Q8 | llama.cpp | 0.478 | 30% | 80% | 17/s |
| 9 | bge-base-en-v1.5 Q8 | llama.cpp | 0.368 | 20% | 70% | 13/s |
| 10 | Qwen3-Embedding-0.6B Q8 | llama.cpp | 0.316 | 20% | 50% | 3/s |
| 11 | nomic-embed-text-v1.5 Q8 | llama.cpp | 0.270 | 10% | 50% | 12/s |

## Vulkan GPU Speedup (llama.cpp CPU → Vulkan)

| Model | CPU Med/s | Vulkan Med/s | Speedup | CPU Long/s | Vulkan Long/s | Speedup |
|-------|----------:|-------------:|--------:|-----------:|--------------:|--------:|
| bge-small Q8 | 67 | 84 | **1.25×** | 30 | 56 | **1.87×** |
| bge-base Q8 | 47 | 55 | **1.17×** | 21 | 32 | **1.52×** |
| bge-large Q8 | 24 | 27 | **1.13×** | 8 | 12 | **1.50×** |
| nomic-v1.5 Q8 | 45 | 51 | **1.13×** | 20 | 27 | **1.35×** |
| nomic-v2-moe Q8 | 11 | 11 | 1.00× | 8 | 8 | 1.00× |
| qwen3-0.6B Q8 | 8 | 11 | **1.38×** | 4 | 7 | **1.75×** |

## Key Findings

1. **sentence-transformers (PyTorch CPU) beats llama.cpp on accuracy across the board** — same model (bge-small) gets MRR 0.683 via st vs 0.478 via llama.cpp GGUF. The GGUF quantization + different tokenization hurts retrieval quality significantly.

2. **sentence-transformers is also faster for short/medium texts** — st/bge-small at 157/s vs llama-vulkan/bge-small at 84/s. PyTorch's optimized ONNX runtime for BERT models is very efficient on CPU.

3. **Vulkan GPU helps most on long texts** — up to 1.87× speedup for bge-small on long (code-length) texts. Negligible benefit on short texts.

4. **Best speed/accuracy tradeoff: st/bge-small-en-v1.5** — MRR 0.683, 157 texts/s (medium), 21 texts/s (corpus encoding). Fast enough for real-time use.

5. **Best accuracy: st/bge-large-en-v1.5** — MRR 0.750, but 8× slower than bge-small. Worth it for offline batch processing.

6. **SweRankEmbed-Small matches bge-small on accuracy** (MRR 0.683) but is 4× slower. Its advantage is specifically for code search reranking (trained on issue→function pairs).

7. **Nomic models underperform on code** — despite strong MTEB scores, nomic-embed-text-v1.5 (MRR 0.661) and v2-moe (0.517 via llama.cpp) are worse than BGE models on Rust code retrieval.

8. **Qwen3-Embedding-0.6B is disappointing** — slow (8-11/s) and low accuracy (MRR 0.316) despite being the largest model tested. May need different prompt formatting or longer context.

## Recommendation for Codixing

- **Default (BM25-only)**: Keep as-is. BM25 MRR=0.750 on this codebase already matches bge-large.
- **Reranking (SWE-bench)**: Continue using SweRankEmbed-Small via sentence-transformers — it's specifically trained for code search and our outline-only approach (31 outlines/s) is fast enough.
- **If GPU-accelerated embedding is needed**: Use llama.cpp + Vulkan with bge-small Q8 for 1.87× speedup on long texts. But the accuracy loss vs sentence-transformers makes this not worthwhile for our use case.

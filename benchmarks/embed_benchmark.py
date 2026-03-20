#!/usr/bin/env python3
"""
embed_benchmark.py — Benchmark embedding models: speed (CPU vs Vulkan) and accuracy.

Tests llama.cpp GGUF models (CPU + Vulkan GPU) and sentence-transformers (CPU).
Measures embeddings/second and retrieval quality (MRR@10) on codixing source files.

Usage:
    python3 benchmarks/embed_benchmark.py
    python3 benchmarks/embed_benchmark.py --speed-only
    python3 benchmarks/embed_benchmark.py --accuracy-only
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import numpy as np

ROOT = Path(__file__).resolve().parent.parent
MODELS_DIR = ROOT / "benchmarks" / "models"
LLAMA_EMB = Path(os.environ.get("LLAMA_EMB_PATH", "llama-embedding"))

# ── Retrieval test: (query, expected file substring in top-10) ──
QUERIES = [
    ("Engine struct definition and initialization", "engine"),
    ("BM25 scoring ranking search", "bm25"),
    ("reindex_file function implementation", "engine"),
    ("MCP tool dispatch handler", "tools/mod"),
    ("AST chunker parsing Python tree-sitter", "chunk"),
    ("symbol table lookup exact prefix", "symbol"),
    ("code dependency graph import extraction", "graph"),
    ("LSP hover goto definition handler", "lsp"),
    ("search strategy auto detection", "search"),
    ("file watcher daemon notification", "daemon"),
]


def _llama_embed_batch(model_path: str, texts: list[str], ngl: int = 0) -> np.ndarray | None:
    """Embed a small batch of texts (<=64) via llama-embedding."""
    # llama-embedding uses \n as prompt separator, so we must sanitize
    # Also truncate to ~400 chars to stay well within 512 token context per prompt
    with tempfile.NamedTemporaryFile(mode="w", suffix=".txt", delete=False) as f:
        for t in texts:
            clean = " ".join(t.split())[:400]
            f.write(clean + "\n")
        tmp = f.name

    try:
        env = {**os.environ}
        if ngl > 0:
            env["GGML_VK_VISIBLE_DEVICES"] = "0"

        ctx = max(512, len(texts) * 128)
        r = subprocess.run(
            [str(LLAMA_EMB), "-m", model_path, "-f", tmp,
             "--embd-normalize", "2", "--embd-output-format", "json",
             "-ngl", str(ngl), "-b", "2048", "-c", str(ctx)],
            capture_output=True, timeout=300, env=env,
        )
        if r.returncode != 0:
            return None

        data = json.loads(r.stdout.decode(errors="replace"))
        if isinstance(data, dict) and "data" in data:
            embs = [item["embedding"] for item in data["data"]]
        elif isinstance(data, list):
            embs = [item["embedding"] if isinstance(item, dict) else item for item in data]
        else:
            return None

        result = np.array(embs, dtype=np.float32)
        # llama-embedding may produce extra embeddings if a prompt exceeds the
        # model's context window (it splits long prompts). Take first len(texts).
        if result.shape[0] > len(texts):
            result = result[:len(texts)]
        elif result.shape[0] < len(texts):
            return None  # got fewer than expected
        return result
    except Exception:
        return None
    finally:
        os.unlink(tmp)


def llama_embed(model_path: str, texts: list[str], ngl: int = 0, batch_size: int = 50) -> tuple[np.ndarray | None, float]:
    """Embed texts via llama-embedding in batches."""
    if not LLAMA_EMB.exists():
        return None, 0.0

    all_embs = []
    start = time.perf_counter()
    for i in range(0, len(texts), batch_size):
        batch = texts[i:i + batch_size]
        embs = _llama_embed_batch(model_path, batch, ngl)
        if embs is None:
            return None, time.perf_counter() - start
        all_embs.append(embs)
    elapsed = time.perf_counter() - start

    if not all_embs:
        return None, elapsed
    return np.vstack(all_embs), elapsed


def st_embed(model_name: str, texts: list[str], prompt_name: str = None) -> tuple[np.ndarray | None, float]:
    """Embed texts via sentence-transformers."""
    from sentence_transformers import SentenceTransformer
    model = SentenceTransformer(model_name, trust_remote_code=True)
    kwargs = {"normalize_embeddings": True, "batch_size": 64}
    if prompt_name:
        kwargs["prompt_name"] = prompt_name
    start = time.perf_counter()
    embs = model.encode(texts, **kwargs)
    elapsed = time.perf_counter() - start
    return embs, elapsed


def build_corpus(max_chunks: int = 200) -> tuple[list[str], list[str]]:
    """Build a representative corpus from codixing Rust source files."""
    texts, labels = [], []
    for src_dir in [ROOT / "crates" / d / "src" for d in ("core", "mcp", "lsp", "cli")]:
        if not src_dir.exists():
            continue
        for f in sorted(src_dir.rglob("*.rs")):
            rel = str(f.relative_to(ROOT))
            try:
                content = f.read_text(errors="replace")
            except OSError:
                continue
            # Take first ~3 chunks per file (function-level)
            buf, chunks_for_file = [], 0
            for line in content.splitlines():
                buf.append(line)
                if len("\n".join(buf)) > 600:
                    texts.append(f"{rel}\n{chr(10).join(buf)[:800]}")
                    labels.append(rel)
                    buf = []
                    chunks_for_file += 1
                    if chunks_for_file >= 3:
                        break
            if buf and chunks_for_file < 3:
                text = "\n".join(buf)
                if text.strip():
                    texts.append(f"{rel}\n{text[:800]}")
                    labels.append(rel)
    # Subsample if too many
    if len(texts) > max_chunks:
        idx = np.linspace(0, len(texts) - 1, max_chunks, dtype=int)
        idx = list(set(idx))  # deduplicate
        idx.sort()
        texts = [texts[i] for i in idx]
        labels = [labels[i] for i in idx]
    assert len(texts) == len(labels), f"Mismatch: {len(texts)} texts, {len(labels)} labels"
    return texts, labels


def mrr_at_k(query_embs, corpus_embs, queries, labels, k=10):
    """Compute MRR@k and Hit@k."""
    qn = query_embs / (np.linalg.norm(query_embs, axis=1, keepdims=True) + 1e-9)
    cn = corpus_embs / (np.linalg.norm(corpus_embs, axis=1, keepdims=True) + 1e-9)
    scores = qn @ cn.T
    mrr, h1, h5 = 0.0, 0, 0
    for i, (_, expected) in enumerate(queries):
        ranked = np.argsort(-scores[i])
        for rank, idx in enumerate(ranked[:k]):
            if expected.lower() in labels[idx].lower():
                mrr += 1.0 / (rank + 1)
                if rank < 1: h1 += 1
                if rank < 5: h5 += 1
                break
    n = len(queries)
    return {"mrr@10": mrr / n, "hit@1": h1 / n, "hit@5": h5 / n}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--speed-only", action="store_true")
    parser.add_argument("--accuracy-only", action="store_true")
    args = parser.parse_args()

    print("=" * 75)
    print("EMBEDDING MODEL BENCHMARK — AMD Ryzen 7 6800H + Radeon 680M (Vulkan)")
    print("=" * 75)

    # ── Discover GGUF models ──
    gguf_models = []
    if MODELS_DIR.exists():
        for f in sorted(MODELS_DIR.glob("*.gguf")):
            if f.stat().st_size > 1000:
                gguf_models.append((f.stem, str(f)))
    print(f"\nGGUF models: {[n for n, _ in gguf_models]}")

    # ── sentence-transformers models ──
    st_models = [
        ("BAAI/bge-small-en-v1.5", "query"),
        ("BAAI/bge-base-en-v1.5", "query"),
        ("BAAI/bge-large-en-v1.5", "query"),
        ("Salesforce/SweRankEmbed-Small", "query"),
        ("nomic-ai/nomic-embed-text-v1.5", "query"),
    ]

    # ── Generate test texts ──
    short = [f"search query about code function {i}" for i in range(20)]
    medium = [
        f"This is a medium-length text about software engineering concepts "
        f"including design patterns, refactoring strategies, and test-driven "
        f"development approaches for building maintainable systems. Query {i}."
        for i in range(20)
    ]
    long = [
        f"def example_function_{i}(self, arg1, arg2):\n"
        f"    '''Process the input data and return results.'''\n"
        f"    result = []\n    for item in arg1:\n"
        f"        if item.is_valid():\n"
        f"            processed = self._transform(item, arg2)\n"
        f"            result.append(processed)\n"
        f"    return result\n\n"
        f"class ExampleClass_{i}:\n"
        f"    def __init__(self): self.data = {{}}\n"
        f"    def process(self): pass\n"
        f"    def validate(self): return True\n"
        f"    def transform(self, x): return x * 2"
        for i in range(20)
    ]

    # ════════════════════════════════════════════════════════
    # SPEED BENCHMARKS
    # ════════════════════════════════════════════════════════
    if not args.accuracy_only:
        print(f"\n{'='*75}")
        print("SPEED BENCHMARKS (20 texts × 3 lengths)")
        print(f"{'='*75}")

        results_table = []  # (name, short_rate, medium_rate, long_rate, dims)

        for name, path in gguf_models:
            for backend, ngl in [("CPU", 0), ("Vulkan", 99)]:
                label = f"llama-{backend.lower()}/{name}"
                rates = {}
                for length, texts in [("short", short), ("medium", medium), ("long", long)]:
                    embs, elapsed = llama_embed(path, texts, ngl=ngl)
                    if embs is not None:
                        rates[length] = len(texts) / elapsed
                        dims = embs.shape[1]
                    else:
                        rates[length] = None
                        dims = 0
                if any(v is not None for v in rates.values()):
                    results_table.append((label, rates.get("short"), rates.get("medium"), rates.get("long"), dims))
                    s = f"{rates.get('short', 0) or 0:.0f}"
                    m = f"{rates.get('medium', 0) or 0:.0f}"
                    l = f"{rates.get('long', 0) or 0:.0f}"
                    print(f"  {label:<50s}  S:{s:>4s}  M:{m:>4s}  L:{l:>4s}/s  {dims}d")

        for st_name, prompt in st_models:
            short_name = st_name.split("/")[-1]
            label = f"st/{short_name}"
            try:
                rates = {}
                dims = 0
                for length, texts in [("short", short), ("medium", medium), ("long", long)]:
                    embs, elapsed = st_embed(st_name, texts, prompt)
                    if embs is not None:
                        rates[length] = len(texts) / elapsed
                        dims = embs.shape[1]
                results_table.append((label, rates.get("short"), rates.get("medium"), rates.get("long"), dims))
                s = f"{rates.get('short', 0) or 0:.0f}"
                m = f"{rates.get('medium', 0) or 0:.0f}"
                l = f"{rates.get('long', 0) or 0:.0f}"
                print(f"  {label:<50s}  S:{s:>4s}  M:{m:>4s}  L:{l:>4s}/s  {dims}d")
            except Exception as e:
                print(f"  {label:<50s}  ERROR: {e}")

        # Summary sorted by medium rate
        print(f"\n{'='*75}")
        print("SPEED RANKING (medium-length texts, texts/second)")
        print(f"{'='*75}")
        print(f"  {'#':<3s} {'Model':<50s} {'Med':>6s} {'Short':>6s} {'Long':>6s} {'Dims':>5s}")
        print(f"  {'-'*73}")
        sorted_results = sorted(results_table, key=lambda x: -(x[2] or 0))
        for i, (name, s, m, l, d) in enumerate(sorted_results):
            ms = f"{m:.0f}" if m else "FAIL"
            ss = f"{s:.0f}" if s else "FAIL"
            ls = f"{l:.0f}" if l else "FAIL"
            print(f"  {i+1:<3d} {name:<50s} {ms:>6s} {ss:>6s} {ls:>6s} {d:>5d}")

    # ════════════════════════════════════════════════════════
    # ACCURACY BENCHMARKS
    # ════════════════════════════════════════════════════════
    if not args.speed_only:
        print(f"\n{'='*75}")
        print("ACCURACY BENCHMARKS — codixing source retrieval (MRR@10)")
        print(f"{'='*75}")

        corpus_texts, corpus_labels = build_corpus(max_chunks=200)
        print(f"Corpus: {len(corpus_texts)} chunks from {len(set(corpus_labels))} files")
        query_texts = [q for q, _ in QUERIES]

        accuracy_results = []  # (name, mrr, h1, h5, enc_time)

        for name, path in gguf_models:
            label = f"llama/{name}"
            print(f"  {label}...", end=" ", flush=True)
            c_embs, c_time = llama_embed(path, corpus_texts, ngl=0, batch_size=50)
            if c_embs is None:
                print("FAILED (corpus)")
                continue
            q_embs, _ = llama_embed(path, query_texts, ngl=0)
            if q_embs is None:
                print("FAILED (queries)")
                continue
            m = mrr_at_k(q_embs, c_embs, QUERIES, corpus_labels)
            accuracy_results.append((label, m["mrr@10"], m["hit@1"], m["hit@5"], c_time))
            print(f"MRR={m['mrr@10']:.3f}  H@1={m['hit@1']:.0%}  H@5={m['hit@5']:.0%}  ({c_time:.1f}s)")

        for st_name, prompt in st_models:
            short_name = st_name.split("/")[-1]
            label = f"st/{short_name}"
            print(f"  {label}...", end=" ", flush=True)
            try:
                c_embs, c_time = st_embed(st_name, corpus_texts, prompt)
                q_embs, _ = st_embed(st_name, query_texts, prompt)
                m = mrr_at_k(q_embs, c_embs, QUERIES, corpus_labels)
                accuracy_results.append((label, m["mrr@10"], m["hit@1"], m["hit@5"], c_time))
                print(f"MRR={m['mrr@10']:.3f}  H@1={m['hit@1']:.0%}  H@5={m['hit@5']:.0%}  ({c_time:.1f}s)")
            except Exception as e:
                print(f"ERROR: {e}")

        # Summary sorted by MRR
        print(f"\n{'='*75}")
        print("ACCURACY RANKING")
        print(f"{'='*75}")
        print(f"  {'#':<3s} {'Model':<50s} {'MRR@10':>7s} {'Hit@1':>6s} {'Hit@5':>6s} {'Enc':>7s}")
        print(f"  {'-'*75}")
        for i, (name, mrr, h1, h5, ct) in enumerate(sorted(accuracy_results, key=lambda x: -x[1])):
            rate = f"{len(corpus_texts)/ct:.0f}/s" if ct > 0 else "N/A"
            print(f"  {i+1:<3d} {name:<50s} {mrr:>7.3f} {h1:>5.0%} {h5:>5.0%} {rate:>7s}")


if __name__ == "__main__":
    main()

# Code Understanding Infrastructure — Research Notes

> Research conducted April 4-6 2026 via FastCodeEmbed (25+ embedding experiments across 9 paths).
> Key finding: embedding quality has diminishing returns. Code structure signals > bigger models.

## Background: What FastCodeEmbed Proved

25+ experiments across 9 paths tested every angle of improving code embedding quality for a 33M parameter model (BGE-small-en-v1.5). Results:

| Approach | JS nDCG@10 | Verdict |
|---|---|---|
| BGE-small baseline | 0.747 | No code training |
| Fine-tuned on CSN | 0.768 | +1.2% avg |
| + camelCase preprocessing | **0.792** | Best free win |
| + Hard negatives + MRL | 0.778 | Marginal over FT |
| + LoRA JS adapter (607K pairs) | 0.784 | +1.6% but doesn't stack with preprocess |
| + Knowledge distillation | 0.771 | Teacher can't fix tokenizer |
| + Contextual fine-tuning | 0.783 | Marginal |
| Jina v2 code int8 (161M, 5x larger) | **0.843** | Best quality, 6x slower |

**Ceiling**: JS nDCG ~0.79 is a hard limit of BERT's 30K WordPiece tokenizer fragmenting camelCase/JS syntax. No training recipe breaks through. Closing the gap requires a code-native tokenizer + full pretraining (~50B tokens, ~$1000+ GPU).

**Dead ends** (don't revisit): BM25 hybrid RRF (hurts code retrieval), tokenizer swap without pretraining, cross-encoder reranking (overfits), zero-shot contextual prefixes, Code HyDE query expansion.

**What works**: camelCase splitting (+2.4% JS), contrastive fine-tuning on CSN, 6-layer distillation for 2x speed.

**Key insight**: Codixing achieves 100% top-1 accuracy with BM25 + code structure signals, validating that code understanding infrastructure matters more than embedding model quality.

## Current Codixing Infrastructure (Mature)

### What exists and works well

1. **AST parsing** — 26 languages via tree-sitter, custom entity extraction (SemanticEntity with kind, name, signature, doc_comment, scope)
2. **Symbol tables** — DashMap + mmap persistence, lookup/prefix/filter queries
3. **AST-aware chunking (cAST)** — recursive split-merge with context preservation (scope chains, signatures, entity names, doc comments)
4. **BM25 search** — Tantivy backend, sub-ms latency, camelCase-aware tokenization
5. **Vector search** — HNSW via Usearch, FastEmbed BGE-small, optional
6. **Hybrid RRF** — Asymmetric weighting (identifier queries favor BM25, NL queries favor vector)
7. **Dependency graph** — Import resolution (18 languages), PageRank, file-level + symbol-level call graph
8. **Test mapping** — Naming conventions + import analysis, confidence levels
9. **Query expansion** — CamelCase split, synonym maps, code HyDE patterns, multi-query RRF
10. **Post-retrieval pipeline** — Truncation, file dedup, graph boost, recency boost, symbol boost
11. **Query routing** — Code vs docs intent classification
12. **Doc indexing** — Section-aware Markdown/HTML chunking, doc-to-code edges
13. **Trigram index** — Sub-ms exact identifier search
14. **Federated search** — Cross-repo with LRU lazy loading

### What's partially implemented

- **Concept-to-path boosting**: Symbol boost exists but no semantic concept clustering
- **Query personalization**: PageRank is static, not query-personalized
- **Reranker**: BGE-Reranker-Base available for Deep strategy but general rerankers hurt code quality
- **HyDE**: Hardcoded code patterns only, no LLM-generated hypothetical documents

## Investment Opportunities

### Tier 1: High-impact, builds on existing infrastructure

#### 1. Semantic Concept Graph

**Problem**: The vocabulary gap between NL queries and code identifiers is the root cause of both BM25 and embedding failures. "Authentication" doesn't match `verify_jwt_token()` in either system.

**Solution**: Build a concept graph that maps domain concepts to symbol clusters:
```
"authentication" → {login(), verify_token(), Session, jwt_middleware(), AuthGuard}
"rate limiting"  → {RateLimiter, throttle(), burst_capacity, LeakyBucket}
"caching"        → {LruCache, cache_get(), invalidate(), CacheConfig}
```

**How to build it**:
- Mine co-occurrence from import graphs + call graphs (files that import each other share concepts)
- Use doc comments / JSDoc as concept labels (functions already describe themselves)
- Cluster symbols by embedding similarity (use existing BGE-small vectors)
- Build a ConceptBoostStage for the post-retrieval pipeline

**Expected impact**: Bridges the exact gap that killed BM25 hybrid in our experiments. Instead of matching tokens, match concepts. Should improve NL queries by 10-20% on concept-heavy searches.

**Effort**: 2-3 days. Extends existing graph infrastructure.

#### 2. Query-Personalized PageRank

**Problem**: Static PageRank boosts important files regardless of query context. The file `main.rs` always has high PageRank but is rarely the search target.

**Solution**: Personalized PageRank seeded from query-matching files:
```
Query: "authentication"
  1. Find files containing "auth" symbols → {auth.rs, middleware.rs}
  2. Run personalized PageRank seeded from these files
  3. Session.rs, jwt.rs propagate high scores (they import auth modules)
  4. Unrelated files (logging.rs, config.rs) get low scores
  5. Apply as query-specific boost in pipeline
```

**How to build it**:
- Add `personalized_pagerank(seed_nodes, damping, iterations)` to `graph/pagerank.rs`
- In search pipeline, compute seed nodes from BM25 top-k results
- Apply personalized scores as a `QueryGraphBoostStage`
- Cache results for common query patterns

**Expected impact**: Surfaces structurally related files that BM25 misses. Particularly valuable for architectural queries ("how does the auth system work?").

**Effort**: 1-2 days. PageRank infra already exists, just needs personalization.

#### 3. Usage Example Mining

**Problem**: Finding a symbol definition is only half the answer. Agents need to see how it's used.

**Solution**: For each symbol, extract usage examples from:
- Test files (already mapped via test_mapping)
- Call sites (already tracked in call graph)
- Doc code blocks (already parsed)

**How to build it**:
- Add `find_usage_examples(symbol_name, max_examples)` to Engine
- Rank examples by: (1) test file examples first, (2) closest callers by graph distance, (3) doc examples
- Include in search results as `examples: Vec<UsageExample>` field
- Context assembly already exists; extend it to pull examples automatically

**Expected impact**: Increases utility per search result. Agent gets definition + usage in one query instead of two.

**Effort**: 2-3 days. Builds on test_mapping + call graph.

### Tier 2: Medium effort, fills gaps

#### 4. Type-Aware Search

**Problem**: Symbol table stores EntityKind but not type relationships. Can't query "all implementations of trait X" or "functions returning Result<T>".

**Solution**: Extract and index type relationships:
- Inheritance chains (class A extends B)
- Interface/trait implementations
- Generic type parameters
- Return types (where inferrable from signatures)

Add to symbol table as `type_relations: Vec<TypeRelation>`. Enable queries like `kind=impl trait=Handler`.

**Effort**: 3-5 days. Requires per-language type extraction logic.

#### 5. API Surface Analysis

**Problem**: No distinction between public exports and internal helpers. Search results mix implementation details with public API.

**Solution**: Track visibility per symbol:
- Rust: `pub`, `pub(crate)`, private
- JS/TS: `export`, `export default`, module-scoped
- Python: `__all__`, underscore convention
- Go: uppercase = exported

Boost public API symbols in search. Demote internal helpers unless query is specifically about internals.

**Effort**: 1-2 days per language. Visibility is available in AST.

#### 6. Change Impact Analysis

**Problem**: "If I change this file, what breaks?" — common question during refactoring.

**Solution**: Given the call graph + dependency graph:
```
change_impact("auth.rs") → {
  direct_dependents: ["middleware.rs", "routes/login.rs"],
  transitive_dependents: ["app.rs", "server.rs"],
  affected_tests: ["tests/auth_test.rs", "tests/integration.rs"],
  blast_radius: 12 files
}
```

**Effort**: 1-2 days. Graph traversal on existing dependency graph.

### Tier 3: Research-level, high potential

#### 7. Learned Query Reformulation

Replace hardcoded synonym maps with project-specific learned mappings. Approaches:
- **Term frequency analysis**: Mine the most common terms in the indexed codebase, build project-specific synonyms automatically
- **Doc-to-code vocabulary bridge**: For each documented symbol, map the docstring vocabulary to the code vocabulary
- **User feedback loop**: Track which results users select, learn which reformulations helped

#### 8. Cross-File Context Assembly

When returning search results, automatically assemble the minimal context an agent needs:
- The matched chunk
- Import chain (what this file depends on — type definitions, constants)
- Key callees (functions this code calls that define behavior)
- Usage examples from callers

Codixing's context assembly partially does this. Opportunity: make it more aggressive and smarter about what context matters for each query type.

#### 9. Embedding-Free Semantic Matching

The ultimate research question: can we match Jina's JS=0.84 quality without any embedding model?

Path:
1. Deep AST analysis — understand what a function *does* (reads input, transforms, writes output)
2. Symbol-to-concept mapping — learned from docstrings
3. Graph propagation for concept search
4. Structural similarity — functions with similar call patterns are semantically similar

This would eliminate the ONNX dependency entirely while potentially exceeding embedding quality for code-specific queries.

## Recommended Investment Priority

```
Impact vs Effort:

HIGH IMPACT
  │
  │  ◆ Personalized PageRank (1-2d)
  │  ◆ Concept Graph (2-3d)
  │  ◆ Usage Examples (2-3d)
  │
  ├──────────────────────────
  │  ◇ API Surface (1-2d)        ◇ Change Impact (1-2d)
  │  ◇ Type-Aware Search (3-5d)
  │
LOW IMPACT
  │  ○ Learned Reformulation (5d+)
  │  ○ Embedding-Free Semantic (research)
  │
  └──────── LOW EFFORT ──────── HIGH EFFORT ────────
```

**Start with**: Personalized PageRank (1-2 days, extends existing graph). Then Concept Graph + Usage Examples in parallel (2-3 days each).

## FastCodeEmbed Production Models for Codixing

| Model | Size | Speed | When to use |
|---|---|---|---|
| **BGE-small-12L FT int8** | 34MB | 2.5ms ARM | Default. Py=0.98, JS=0.79 (w/preprocess), Go=0.95 |
| **BGE-small-6L Distilled int8** | 23MB | 4.8ms x86 | Large codebases (47K chunks in ~11 min vs ~23 min) |
| **Jina v2 Code int8** | 162MB | 15ms | JS-critical workloads only (JS=0.84 vs 0.79) |
| **Model2Vec-Jina-Code** | ~5MB | 0.1ms | Ultra-fast recall stage, low quality (R@10=0.81) |

All models available at: `/Users/andreaferrarelli/code/fastembed/models/`
Server: `python -m src.server --model-dir <path> --port 8080`
Preprocessing: pass `"language": "javascript"` in embedding request for auto camelCase split.

## References

- FastCodeEmbed: `/Users/andreaferrarelli/code/fastembed/CLAUDE.md`
- LoRACode (ICLR 2025): Per-language LoRA adapters for code embeddings
- ContraCode (EMNLP 2021): Contrastive code representation via JS transforms
- CoRNStack (ICLR 2025): Hard negative mining for code retrieval
- C2LLM (Dec 2025): Decoder-based code embeddings with cross-attention pooling
- Codixing blog benchmarks: `/Users/andreaferrarelli/code/codixing/docs/blog-benchmarks-2026-03.html`

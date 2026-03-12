use std::sync::Arc;

use tracing::debug;

use crate::error::Result;
use crate::retriever::bm25::BM25Retriever;
use crate::retriever::hybrid::HybridRetriever;
use crate::retriever::mmr::mmr_select;
use crate::retriever::{Retriever, SearchQuery, SearchResult, Strategy};

use super::Engine;

impl Engine {
    /// Search the index using the strategy specified in `query`.
    ///
    /// - `Instant` → BM25 only
    /// - `Fast`    → BM25 + vector + RRF fusion (falls back to BM25 if no embedder)
    /// - `Thorough` → hybrid + MMR deduplication
    pub fn search(&self, query: SearchQuery) -> Result<Vec<SearchResult>> {
        let mut results = match query.strategy {
            Strategy::Instant => {
                let retriever = BM25Retriever::new(&self.tantivy);
                let mut results = retriever.search(&query)?;
                // Apply definition boost even on the BM25-only path: it's pure
                // HashMap lookups and fixes the definition-vs-usage ranking issue.
                self.apply_definition_boost(&mut results, &query.query);
                self.apply_test_demotion(&mut results);
                results
            }
            Strategy::Fast => {
                let mut results = if let (Some(emb), Some(vec_idx)) = (&self.embedder, &self.vector)
                {
                    let retriever = HybridRetriever::new(
                        &self.tantivy,
                        Arc::clone(emb),
                        vec_idx,
                        &self.chunk_meta,
                        self.config.embedding.rrf_k,
                    );
                    retriever.search(&query)?
                } else {
                    debug!("no embedder available; falling back to BM25 for Fast strategy");
                    BM25Retriever::new(&self.tantivy).search(&query)?
                };
                self.apply_graph_boost(&mut results, self.config.graph.boost_weight);
                self.apply_definition_boost(&mut results, &query.query);
                self.apply_test_demotion(&mut results);
                results
            }
            Strategy::Explore => self.search_explore(query)?,
            Strategy::Thorough => {
                let mut results = if let (Some(emb), Some(vec_idx)) = (&self.embedder, &self.vector)
                {
                    let hybrid = HybridRetriever::new(
                        &self.tantivy,
                        Arc::clone(emb),
                        vec_idx,
                        &self.chunk_meta,
                        self.config.embedding.rrf_k,
                    );
                    let fetch_query = SearchQuery {
                        limit: query.limit * 3,
                        ..query.clone()
                    };
                    let candidates = hybrid.search(&fetch_query)?;

                    if candidates.is_empty() {
                        return Ok(Vec::new());
                    }

                    let (results_with_meta, embeddings): (Vec<SearchResult>, Vec<Vec<f32>>) =
                        candidates
                            .into_iter()
                            .filter_map(|r| {
                                let emb_vec = emb.embed_one(&r.content).ok()?;
                                Some((r, emb_vec))
                            })
                            .unzip();

                    let query_vec = emb.embed_one(&query.query)?;
                    mmr_select(
                        results_with_meta,
                        &query_vec,
                        &embeddings,
                        self.config.embedding.mmr_lambda,
                        query.limit,
                    )
                } else {
                    debug!("no embedder available; falling back to BM25 for Thorough strategy");
                    BM25Retriever::new(&self.tantivy).search(&query)?
                };
                self.apply_graph_boost(&mut results, self.config.graph.boost_weight);
                self.apply_definition_boost(&mut results, &query.query);
                self.apply_test_demotion(&mut results);
                results
            }
            Strategy::Deep => self.search_deep(query)?,
        };
        dedup_overlapping(&mut results);
        Ok(results)
    }

    /// Graph-expanded search (RepoHyper "Search-then-Expand" pattern).
    ///
    /// Phase 1: broad BM25 retrieval identifies anchor files.
    /// Phase 2: import graph expands anchor set to direct callers/callees.
    /// Phase 3: each newly-discovered neighbour file contributes its best
    ///          BM25 chunk, scored by PageRank to penalise low-importance files.
    ///
    /// This surfaces transitively-relevant code that a single BM25 pass misses
    /// — especially useful on 3 M+ LoC codebases where related logic is spread
    /// across many files connected only via import chains.
    fn search_explore(&self, query: SearchQuery) -> Result<Vec<SearchResult>> {
        use std::collections::HashSet;

        let bm25 = BM25Retriever::new(&self.tantivy);

        // Phase 1 — broad BM25 over-fetch.
        let wide_q = SearchQuery {
            limit: query.limit * 3,
            strategy: Strategy::Instant,
            ..query.clone()
        };
        let mut results = bm25.search(&wide_q)?;
        self.apply_graph_boost(&mut results, self.config.graph.boost_weight);

        // Phase 2 — expand via import graph.
        if let Some(ref graph) = self.graph {
            // Anchor = files in the top-limit initial results.
            let anchor_files: HashSet<String> = results
                .iter()
                .take(query.limit)
                .map(|r| r.file_path.clone())
                .collect();

            // Already-covered = all files in the full result set.
            let covered_files: HashSet<String> =
                results.iter().map(|r| r.file_path.clone()).collect();

            // Collect graph neighbours not already in the anchor set.
            let mut neighbour_files: HashSet<String> = HashSet::new();
            for file in &anchor_files {
                for n in graph.callers(file) {
                    if !anchor_files.contains(&n) {
                        neighbour_files.insert(n);
                    }
                }
                for n in graph.callees(file) {
                    if !anchor_files.contains(&n) {
                        neighbour_files.insert(n);
                    }
                }
            }

            // Phase 3 — for each uncovered neighbour, fetch its best BM25 chunk.
            // Cap at 8 neighbours to keep latency predictable.
            let mut expansion: Vec<SearchResult> = Vec::new();
            for neighbour in neighbour_files.iter().take(8) {
                if covered_files.contains(neighbour) {
                    continue;
                }
                let nq = SearchQuery {
                    query: query.query.clone(),
                    limit: 1,
                    file_filter: Some(neighbour.clone()),
                    strategy: Strategy::Instant,
                    token_budget: None,
                };
                if let Ok(mut exp) = bm25.search(&nq) {
                    for r in exp.iter_mut() {
                        // Scale by PageRank: neighbour files must be architecturally
                        // important to surface above the direct BM25 hits.
                        let pr = graph.node(&r.file_path).map(|n| n.pagerank).unwrap_or(0.0);
                        r.score *= 0.6 + 0.6 * pr;
                    }
                    expansion.extend(exp);
                }
            }
            results.extend(expansion);
            results.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }

        self.apply_test_demotion(&mut results);
        results.truncate(query.limit);
        Ok(results)
    }

    /// Multiply each result's score by `1 + weight * pagerank` then re-sort descending.
    pub(super) fn apply_graph_boost(&self, results: &mut [SearchResult], weight: f32) {
        if let Some(ref graph) = self.graph {
            for r in results.iter_mut() {
                let pr = graph.node(&r.file_path).map(|n| n.pagerank).unwrap_or(0.0);
                r.score *= 1.0 + weight * pr;
            }
            results.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
    }

    /// Boost results that *define* a symbol matching any identifier token in `query`.
    ///
    /// BM25 TF-IDF over-ranks files that *heavily use* a symbol (e.g. `engine.rs`
    /// mentions `IndexConfig` 40+ times) above the file that *defines* it
    /// (e.g. `config.rs`).  This method corrects that by applying a 1.5× score
    /// multiplier to any result whose `file_path` appears in the symbol table as
    /// a defining location for a query term.
    ///
    /// Works for all strategies — even `Instant` — since it is pure in-memory
    /// DashMap lookups with no I/O.
    pub(super) fn apply_definition_boost(&self, results: &mut [SearchResult], query: &str) {
        use std::collections::HashSet;

        // Collect files that define any identifier-like token in the query.
        let mut defining_files: HashSet<String> = HashSet::new();
        for term in query.split_whitespace() {
            // Skip short or punctuation-heavy tokens.
            if term.len() < 3 || !term.chars().all(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            // Exact-name lookup (covers CamelCase identifiers like `IndexConfig`).
            let exact = self.symbols.lookup(term);
            if !exact.is_empty() {
                for sym in exact {
                    defining_files.insert(sym.file_path);
                }
            } else {
                // Case-insensitive substring fallback (e.g. "indexconfig" → IndexConfig).
                for sym in self.symbols.filter(term, None) {
                    defining_files.insert(sym.file_path);
                }
            }
        }

        if defining_files.is_empty() {
            return;
        }

        const DEFINITION_BOOST: f32 = 2.0;
        let mut boosted = false;
        for r in results.iter_mut() {
            if defining_files.contains(&r.file_path) {
                r.score *= DEFINITION_BOOST;
                boosted = true;
            }
        }
        if boosted {
            results.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
    }

    /// Demote results from test files so implementation code ranks higher.
    ///
    /// Test files naturally have high BM25 keyword density for implementation
    /// terms (they mention all the keywords in assertions and comments), which
    /// causes BM25 to over-rank them above the actual implementation.
    ///
    /// A 0.7× score multiplier pushes tests below equally-relevant impl code.
    /// Applied to concept/search queries, **not** to `search_usages` (where
    /// test call-sites are legitimate results).
    pub(super) fn apply_test_demotion(&self, results: &mut [SearchResult]) {
        const TEST_DEMOTION: f32 = 0.5;
        let mut demoted = false;
        for r in results.iter_mut() {
            if is_test_file(&r.file_path) || is_test_chunk(r) {
                r.score *= TEST_DEMOTION;
                demoted = true;
            }
        }
        if demoted {
            results.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
    }

    /// Auto-detect the best search strategy based on query characteristics
    /// and available engine capabilities (embedder, reranker).
    ///
    /// - Single identifier → `Instant` (BM25 is fastest for exact matches)
    /// - Two identifiers → `Fast` (if embedder available) or `Instant`
    /// - Natural language (3+ words) → `Thorough`/`Deep`/`Instant` depending on availability
    pub fn detect_strategy(&self, query: &str) -> Strategy {
        let trimmed = query.trim();
        let words: Vec<&str> = trimmed.split_whitespace().collect();
        let word_count = words.len();

        // Single identifier → Instant (BM25)
        if word_count == 1 && is_identifier_like(trimmed) {
            return Strategy::Instant;
        }

        // Two identifiers (e.g. "IndexConfig new") → Fast or Instant
        if word_count == 2 && words.iter().all(|w| is_identifier_like(w)) {
            return if self.embedder.is_some() {
                Strategy::Fast
            } else {
                Strategy::Instant
            };
        }

        // Longer natural language queries → Thorough or Deep
        if word_count >= 3 {
            if self.reranker.is_some() {
                return Strategy::Deep;
            }
            if self.embedder.is_some() {
                return Strategy::Thorough;
            }
            return Strategy::Instant;
        }

        // Default fallback
        if self.embedder.is_some() {
            Strategy::Fast
        } else {
            Strategy::Instant
        }
    }

    /// Format search results as an LLM-friendly context block.
    pub fn format_results(&self, results: &[SearchResult], token_budget: Option<usize>) -> String {
        crate::formatter::format_context(results, token_budget)
    }

    /// Find all code chunks that reference `symbol` (BM25 full-text search).
    ///
    /// This is the "find usages" operation: given an identifier name, it returns
    /// ranked chunks where that identifier appears — including call sites,
    /// imports, and variable usages, not just the definition.
    pub fn search_usages(&self, symbol: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let query = SearchQuery::new(symbol).with_limit(limit);
        let mut results = BM25Retriever::new(&self.tantivy).search(&query)?;
        // Apply PageRank boost so architecturally central files rank first.
        self.apply_graph_boost(&mut results, self.config.graph.boost_weight);
        Ok(results)
    }

    /// Two-stage reranked search: hybrid first-pass then cross-encoder scoring.
    ///
    /// Phase 1: collect up to `max(limit × 3, 30)` candidates via the `Fast`
    ///          hybrid pipeline (BM25 + vector + graph boost).
    /// Phase 2: BGE-Reranker-Base scores each `(query, chunk)` pair jointly.
    ///          Results are re-sorted by reranker score and truncated.
    ///
    /// Falls back to `Thorough` if the reranker is not loaded.
    fn search_deep(&self, query: SearchQuery) -> Result<Vec<SearchResult>> {
        let reranker = match self.reranker.as_ref() {
            Some(r) => Arc::clone(r),
            None => {
                tracing::warn!(
                    "deep strategy requested but reranker not loaded \
                     (set reranker_enabled = true in config and re-open the engine)"
                );
                // Graceful degradation: run Thorough instead.
                return self.search(SearchQuery {
                    strategy: Strategy::Thorough,
                    ..query
                });
            }
        };

        // Phase 1: over-fetch candidates.
        let candidate_limit = (query.limit * 3).max(30);
        let candidate_query = SearchQuery {
            limit: candidate_limit,
            strategy: Strategy::Fast,
            ..query.clone()
        };

        let mut candidates = if let (Some(emb), Some(vec_idx)) = (&self.embedder, &self.vector) {
            let retriever = HybridRetriever::new(
                &self.tantivy,
                Arc::clone(emb),
                vec_idx,
                &self.chunk_meta,
                self.config.embedding.rrf_k,
            );
            retriever.search(&candidate_query)?
        } else {
            BM25Retriever::new(&self.tantivy).search(&candidate_query)?
        };
        self.apply_graph_boost(&mut candidates, self.config.graph.boost_weight);

        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        // Phase 2: rerank with cross-encoder.
        let docs: Vec<String> = candidates.iter().map(|r| r.content.clone()).collect();
        let ranked = reranker.rerank(&query.query, &docs)?;

        // Apply reranker scores — map (original_index, score) back onto candidates.
        for (orig_idx, score) in &ranked {
            candidates[*orig_idx].score = *score;
        }

        // Re-sort descending by the new scores.
        candidates.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Apply file filter, test demotion, and truncate to requested limit.
        if let Some(ref filter) = query.file_filter {
            candidates.retain(|r| r.file_path.contains(filter.as_str()));
        }
        self.apply_test_demotion(&mut candidates);
        candidates.truncate(query.limit);

        Ok(candidates)
    }
}

/// Remove results whose line ranges overlap with a higher-scored result from
/// the same file.  Results arrive sorted by score descending, so the first
/// result from an overlapping group is always the best.
fn dedup_overlapping(results: &mut Vec<SearchResult>) {
    if results.len() <= 1 {
        return;
    }
    let mut deduped: Vec<SearchResult> = Vec::with_capacity(results.len());
    for r in results.drain(..) {
        let dominated = deduped.iter().any(|existing| {
            existing.file_path == r.file_path
                && existing.line_start < r.line_end
                && r.line_start < existing.line_end
        });
        if !dominated {
            deduped.push(r);
        }
    }
    *results = deduped;
}

/// Detect inline test chunks by scope chain (e.g. Rust `#[cfg(test)] mod tests { }`).
fn is_test_chunk(result: &SearchResult) -> bool {
    result
        .scope_chain
        .iter()
        .any(|s| s == "tests" || s == "test")
}

/// Detect test files by common path patterns across languages.
fn is_test_file(path: &str) -> bool {
    // Directory patterns: tests/, test/, __tests__/, fixtures/, __fixtures__/
    if path.contains("/tests/")
        || path.contains("/test/")
        || path.contains("/__tests__/")
        || path.contains("/fixtures/")
        || path.contains("/__fixtures__/")
    {
        return true;
    }
    // Rust: *_test.rs
    if path.ends_with("_test.rs") {
        return true;
    }
    // File-name patterns (case-insensitive basename check)
    let basename = path.rsplit('/').next().unwrap_or(path);
    let lower = basename.to_ascii_lowercase();
    // Python: test_*.py, *_test.py
    if (lower.starts_with("test_") || lower.ends_with("_test.py")) && lower.ends_with(".py") {
        return true;
    }
    // JS/TS: *.test.ts, *.spec.ts, *.test.js, *.spec.js
    if lower.ends_with(".test.ts")
        || lower.ends_with(".test.js")
        || lower.ends_with(".test.tsx")
        || lower.ends_with(".test.jsx")
        || lower.ends_with(".spec.ts")
        || lower.ends_with(".spec.js")
    {
        return true;
    }
    false
}

/// Check if a string looks like a code identifier (alphanumeric + underscores/colons/dots).
fn is_identifier_like(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 80
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == ':' || c == '.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_rust_test_files() {
        assert!(is_test_file("crates/core/tests/retrieval_quality_test.rs"));
        assert!(is_test_file("crates/core/tests/graph_test.rs"));
        assert!(is_test_file("src/engine_test.rs"));
    }

    #[test]
    fn detects_test_directories() {
        assert!(is_test_file("crates/core/tests/common/mod.rs"));
        assert!(is_test_file("src/test/helpers.py"));
        assert!(is_test_file("src/__tests__/App.test.tsx"));
    }

    #[test]
    fn detects_python_test_files() {
        assert!(is_test_file("tests/test_engine.py"));
        assert!(is_test_file("src/engine_test.py"));
    }

    #[test]
    fn detects_js_ts_test_files() {
        assert!(is_test_file("src/App.test.tsx"));
        assert!(is_test_file("src/utils.spec.ts"));
        assert!(is_test_file("src/App.test.js"));
        assert!(is_test_file("src/utils.spec.js"));
    }

    #[test]
    fn non_test_files_not_detected() {
        assert!(!is_test_file("src/main.rs"));
        assert!(!is_test_file("crates/core/src/engine/mod.rs"));
        assert!(!is_test_file("src/retriever/bm25.rs"));
        assert!(!is_test_file("src/config.rs"));
        assert!(!is_test_file("src/App.tsx"));
    }

    #[test]
    fn detects_inline_test_chunks() {
        let test_result = SearchResult {
            chunk_id: "1".into(),
            file_path: "src/engine/mod.rs".into(),
            language: "Rust".into(),
            score: 100.0,
            line_start: 500,
            line_end: 550,
            signature: "fn my_test()".into(),
            scope_chain: vec!["Engine".into(), "tests".into()],
            content: "fn my_test() {}".into(),
        };
        let non_test_result = SearchResult {
            scope_chain: vec!["Engine".into(), "search".into()],
            ..test_result.clone()
        };
        assert!(is_test_chunk(&test_result));
        assert!(!is_test_chunk(&non_test_result));
    }

    #[test]
    fn dedup_overlapping_removes_lower_scored_overlap() {
        let mut results = vec![
            SearchResult {
                chunk_id: "1".into(),
                file_path: "src/main.rs".into(),
                language: "Rust".into(),
                score: 10.0,
                line_start: 0,
                line_end: 20,
                signature: "fn main()".into(),
                scope_chain: vec![],
                content: "fn main() {}".into(),
            },
            SearchResult {
                chunk_id: "2".into(),
                file_path: "src/main.rs".into(),
                language: "Rust".into(),
                score: 5.0,
                line_start: 15,
                line_end: 30,
                signature: "fn helper()".into(),
                scope_chain: vec![],
                content: "fn helper() {}".into(),
            },
            SearchResult {
                chunk_id: "3".into(),
                file_path: "src/lib.rs".into(),
                language: "Rust".into(),
                score: 3.0,
                line_start: 0,
                line_end: 10,
                signature: "fn lib_fn()".into(),
                scope_chain: vec![],
                content: "fn lib_fn() {}".into(),
            },
        ];
        dedup_overlapping(&mut results);
        // chunk 2 overlaps chunk 1 (same file, lines 15-30 overlaps 0-20)
        // chunk 1 has higher score, so chunk 2 is removed
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].chunk_id, "1");
        assert_eq!(results[1].chunk_id, "3");
    }

    #[test]
    fn dedup_overlapping_keeps_non_overlapping() {
        let mut results = vec![
            SearchResult {
                chunk_id: "1".into(),
                file_path: "src/main.rs".into(),
                language: "Rust".into(),
                score: 10.0,
                line_start: 0,
                line_end: 20,
                signature: String::new(),
                scope_chain: vec![],
                content: String::new(),
            },
            SearchResult {
                chunk_id: "2".into(),
                file_path: "src/main.rs".into(),
                language: "Rust".into(),
                score: 5.0,
                line_start: 25,
                line_end: 40,
                signature: String::new(),
                scope_chain: vec![],
                content: String::new(),
            },
        ];
        dedup_overlapping(&mut results);
        assert_eq!(results.len(), 2, "non-overlapping results should be kept");
    }

    #[test]
    fn is_identifier_like_detects_identifiers() {
        assert!(is_identifier_like("Engine"));
        assert!(is_identifier_like("compute_pagerank"));
        assert!(is_identifier_like("std::io::Read"));
        assert!(is_identifier_like("BM25Retriever"));
        assert!(!is_identifier_like("how does search work"));
        assert!(!is_identifier_like("find all callers"));
        assert!(!is_identifier_like(""));
    }
}

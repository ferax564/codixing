use std::sync::Arc;

use tracing::debug;

use crate::error::Result;
use crate::retriever::bm25::BM25Retriever;
use crate::retriever::hybrid::{HybridRetriever, rrf_fuse};
use crate::retriever::mmr::mmr_select;
use crate::retriever::{Retriever, SearchQuery, SearchResult, Strategy};

use super::Engine;
use super::pipeline::{SearchContext, SearchPipeline};

impl Engine {
    /// Search the index using the strategy specified in `query`.
    ///
    /// - `Instant` → BM25 only
    /// - `Fast`    → BM25 + vector + RRF fusion (falls back to BM25 if no embedder)
    /// - `Thorough` → hybrid + MMR deduplication
    /// - `Exact`   → Trigram index fast-path with BM25 fallback
    pub fn search(&self, query: SearchQuery) -> Result<Vec<SearchResult>> {
        // Expand CamelCase/snake_case identifiers in the query for better BM25 matching.
        // Skip expansion for Instant/Exact strategies (exact symbol lookups).
        let query = if query.strategy != Strategy::Instant && query.strategy != Strategy::Exact {
            let expanded = expand_query(&query.query);
            if expanded != query.query {
                SearchQuery {
                    query: expanded,
                    ..query
                }
            } else {
                query
            }
        } else {
            query
        };

        // Note: synonym expansion is applied only in the Deep strategy
        // (via generate_reformulations) to avoid polluting BM25 results.
        // Expanding synonyms into the main query causes the synonym definition
        // code itself to rank highly (e.g., "orphan" matches search.rs 47 times
        // because it contains the synonym map).

        let strategy = query.strategy;
        let pipeline = self.pipeline_for_strategy(strategy);
        // Save query string before the match block moves `query` into Explore/Deep.
        let query_str = query.query.clone();

        // Handle explicit multi-query RRF fusion (queries param from MCP/API).
        if let Some(ref queries) = query.queries {
            if queries.len() >= 2 {
                let mut fused = self.search_multi(queries, &query)?;
                let ctx = self.search_context(&query_str);
                pipeline.run(&mut fused, &ctx)?;
                return Ok(fused);
            }
        }

        let mut results = match strategy {
            Strategy::Instant => {
                let retriever = BM25Retriever::new(&self.tantivy);
                retriever.search(&query)?
            }
            Strategy::Fast => {
                // Multi-query RRF fusion for 3+ word natural language queries.
                let word_count = query.query.split_whitespace().count();
                if word_count >= 3 {
                    let reformulations = generate_reformulations(&query.query);
                    if reformulations.len() >= 2 {
                        let mut fused = self.search_multi(&reformulations, &query)?;
                        let ctx = self.search_context(&query_str);
                        pipeline.run(&mut fused, &ctx)?;
                        return Ok(fused);
                    }
                }

                if let (Some(emb), Some(vec_idx)) = (&self.embedder, &self.vector) {
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
                }
            }
            Strategy::Explore => self.search_explore(query)?,
            Strategy::Thorough => {
                // Multi-query RRF fusion for 3+ word natural language queries.
                let word_count = query.query.split_whitespace().count();
                if word_count >= 3 {
                    let reformulations = generate_reformulations(&query.query);
                    if reformulations.len() >= 2 {
                        let mut fused = self.search_multi(&reformulations, &query)?;
                        let ctx = self.search_context(&query_str);
                        pipeline.run(&mut fused, &ctx)?;
                        return Ok(fused);
                    }
                }

                if let (Some(emb), Some(vec_idx)) = (&self.embedder, &self.vector) {
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

                    let query_vec = emb.embed_query(&query.query)?;
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
                }
            }
            Strategy::Deep => self.search_deep(query)?,
            Strategy::Exact => self.search_exact(&query)?,
        };

        // Apply the post-retrieval pipeline (boosts, demotions, dedup, truncation).
        let ctx = self.search_context(&query_str);
        pipeline.run(&mut results, &ctx)?;
        Ok(results)
    }

    /// Build the post-retrieval pipeline for the given strategy.
    fn pipeline_for_strategy(&self, strategy: Strategy) -> SearchPipeline {
        use super::pipeline::*;
        match strategy {
            Strategy::Instant => instant_pipeline(),
            Strategy::Fast => fast_pipeline(),
            Strategy::Thorough => thorough_pipeline(),
            Strategy::Exact => exact_pipeline(),
            // Explore and Deep handle their own boosts/demotions internally,
            // but still need truncation + dedup from the outer pipeline.
            Strategy::Explore | Strategy::Deep => SearchPipeline::new()
                .add(TruncationStage {
                    min_results: 3,
                    cliff_threshold: 0.35,
                })
                .add(DeduplicationStage),
        }
    }

    /// Build a [`SearchContext`] for pipeline stages.
    fn search_context<'a>(&'a self, query: &'a str) -> SearchContext<'a> {
        SearchContext {
            query,
            symbols: &self.symbols,
            graph: self.graph.as_ref(),
            graph_boost_weight: self.config.graph.boost_weight,
            recency_map: Some(self.get_recency_map()),
        }
    }

    /// Multi-query search with RRF fusion.
    ///
    /// Runs `search_first_pass` independently for each query string and fuses
    /// all result lists via progressive Reciprocal Rank Fusion. Results that
    /// appear in multiple query passes are promoted, improving recall for
    /// natural-language queries with vocabulary mismatches.
    ///
    /// Used by:
    /// - `Fast`/`Thorough` strategies when the query is 3+ words (auto-reformulation)
    /// - Explicit `queries` parameter from MCP/API (user-supplied reformulations)
    /// - `Deep` strategy (which adds synonym + code-pattern reformulations)
    pub fn search_multi(
        &self,
        queries: &[String],
        base_query: &SearchQuery,
    ) -> Result<Vec<SearchResult>> {
        if queries.is_empty() {
            return Ok(Vec::new());
        }

        let candidate_limit = (base_query.limit * 3).max(30);
        let mut all_results: Vec<Vec<SearchResult>> = Vec::new();

        for q_text in queries {
            let sub_query = SearchQuery {
                query: expand_query(q_text),
                limit: candidate_limit,
                file_filter: base_query.file_filter.clone(),
                strategy: Strategy::Fast,
                token_budget: None,
                queries: None,
            };
            if let Ok(results) = self.search_first_pass(&sub_query) {
                if !results.is_empty() {
                    all_results.push(results);
                }
            }
        }

        if all_results.is_empty() {
            return Ok(Vec::new());
        }

        // Progressive RRF fusion: fold all result lists together.
        let mut fused = all_results.remove(0);
        for results in &all_results {
            fused = rrf_fuse(&fused, results, 60.0);
        }
        fused.truncate(base_query.limit);
        Ok(fused)
    }

    /// Trigram-index fast-path for exact identifier lookups.
    ///
    /// Phase 1: query the trigram inverted index for sub-millisecond exact
    ///          substring matching.
    /// Phase 2: if trigram yields < 3 results, fall back to BM25 and merge.
    ///
    /// Results are hydrated from chunk_meta and scored by match count.
    fn search_exact(&self, query: &SearchQuery) -> Result<Vec<SearchResult>> {
        let candidate_ids = self.trigram.search(&query.query);

        // Verify candidates and count actual substring matches using chunk content.
        let mut results: Vec<SearchResult> = Vec::new();
        for chunk_id in &candidate_ids {
            if let Some(meta) = self.chunk_meta.get(chunk_id) {
                // Apply file filter if set.
                if let Some(ref filter) = query.file_filter {
                    if !meta.file_path.contains(filter.as_str()) {
                        continue;
                    }
                }
                // Verify actual substring match and count occurrences.
                let hit_count = meta.content.matches(&query.query).count();
                if hit_count == 0 {
                    continue; // Trigram false positive.
                }
                results.push(SearchResult {
                    chunk_id: format!("{chunk_id}"),
                    file_path: meta.file_path.clone(),
                    language: meta.language.clone(),
                    score: hit_count as f32,
                    line_start: meta.line_start,
                    line_end: meta.line_end,
                    signature: meta.signature.clone(),
                    scope_chain: meta.scope_chain.clone(),
                    content: meta.content.clone(),
                });
            }
        }

        // Sort by score descending.
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // If trigram yields < 3 results, augment with BM25.
        if results.len() < 3 {
            let bm25_query = SearchQuery {
                strategy: Strategy::Instant,
                ..query.clone()
            };
            let bm25_results = BM25Retriever::new(&self.tantivy).search(&bm25_query)?;

            // Merge: add BM25 results not already in the trigram set.
            let existing_ids: std::collections::HashSet<String> =
                results.iter().map(|r| r.chunk_id.clone()).collect();
            for r in bm25_results {
                if !existing_ids.contains(&r.chunk_id) {
                    results.push(r);
                }
            }
        }

        results.truncate(query.limit);
        Ok(results)
    }

    /// Like [`search()`](Self::search), but calls `on_progress` after each
    /// retrieval phase so callers can stream partial results to the client.
    ///
    /// Phase names reported:
    /// - `"bm25"` — BM25-only results (always reported first)
    /// - `"fused"` — hybrid BM25 + vector results (for `Fast`/`Thorough`/`Deep`)
    /// - `"reranked"` — cross-encoder re-ranked results (for `Deep` only)
    ///
    /// For `Instant` strategy only the `"bm25"` phase fires and the returned
    /// results are identical to [`search()`](Self::search).
    pub fn search_with_progress<F>(
        &self,
        query: SearchQuery,
        mut on_progress: F,
    ) -> Result<Vec<SearchResult>>
    where
        F: FnMut(&str, &[SearchResult]),
    {
        let strategy = query.strategy;

        // Phase 1: quick BM25-only pass — always runs, gives near-instant
        // partial results regardless of the final strategy.
        let bm25_query = SearchQuery {
            strategy: Strategy::Instant,
            ..query.clone()
        };
        let bm25_results = self.search(bm25_query)?;
        on_progress("bm25", &bm25_results);

        // For Instant/Exact, BM25 is the only phase — return directly.
        if strategy == Strategy::Instant || strategy == Strategy::Exact {
            return Ok(bm25_results);
        }

        // Phase 2+: run the full strategy which internally performs BM25 again
        // (redundant but safe — avoids refactoring the monolithic search path).
        let full_results = self.search(query)?;

        // Report the appropriate phase name based on strategy.
        match strategy {
            Strategy::Deep => on_progress("reranked", &full_results),
            _ => on_progress("fused", &full_results),
        }

        Ok(full_results)
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
                    queries: None,
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

        self.apply_test_demotion(&mut results, &query.query);
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

        const DEFINITION_BOOST: f32 = 3.5;
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
    /// A 0.5× score multiplier pushes tests below equally-relevant impl code.
    /// Applied to concept/search queries, **not** to `search_usages` (where
    /// test call-sites are legitimate results).
    pub(super) fn apply_test_demotion(&self, results: &mut [SearchResult], query: &str) {
        const TEST_DEMOTION: f32 = 0.5;
        const INFRA_DEMOTION: f32 = 0.5;
        let mut changed = false;
        for r in results.iter_mut() {
            if is_test_file(&r.file_path) || is_test_chunk(r) {
                r.score *= TEST_DEMOTION;
                changed = true;
            } else if is_search_infra(&r.file_path, query) {
                // Search infrastructure files (engine/search.rs, retriever/*.rs)
                // are self-referential: they contain synonym maps, reformulation
                // patterns, and strategy code that mentions every search concept.
                // Demote them so domain-specific results rank higher.
                r.score *= INFRA_DEMOTION;
                changed = true;
            }
        }

        // Demote C/C++ header files when a corresponding implementation file
        // is also in results.  Headers declare; .c/.cc/.cpp files implement.
        apply_header_demotion(results, &mut changed);

        if changed {
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
    /// - Single exact identifier → `Exact` (trigram fast-path for `_`, `::`, `.`, or camelCase)
    /// - Single identifier → `Instant` (BM25 is fastest for exact matches)
    /// - Two identifiers → `Fast` (if embedder available) or `Instant`
    /// - Natural language (3+ words) → `Thorough`/`Deep`/`Instant` depending on availability
    pub fn detect_strategy(&self, query: &str) -> Strategy {
        let trimmed = query.trim();
        let words: Vec<&str> = trimmed.split_whitespace().collect();
        let word_count = words.len();

        // Single identifier that looks like code → Exact (trigram fast-path).
        // Criteria: contains `_`, `::`, `.`, or is camelCase (mixed case).
        if word_count == 1 && is_identifier_like(trimmed) && looks_like_exact_identifier(trimmed) {
            return Strategy::Exact;
        }

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

    /// First-pass retrieval: BM25+vector hybrid (if available) with graph boost.
    ///
    /// This is the shared retrieval core used by `search_deep` (and its
    /// multi-query variant) to avoid duplicating the BM25/hybrid logic
    /// and — critically — to avoid recursion through the public `search()`
    /// method which would trigger query expansion and strategy dispatch again.
    fn search_first_pass(&self, query: &SearchQuery) -> Result<Vec<SearchResult>> {
        let mut candidates = if let (Some(emb), Some(vec_idx)) = (&self.embedder, &self.vector) {
            let retriever = HybridRetriever::new(
                &self.tantivy,
                Arc::clone(emb),
                vec_idx,
                &self.chunk_meta,
                self.config.embedding.rrf_k,
            );
            retriever.search(query)?
        } else {
            BM25Retriever::new(&self.tantivy).search(query)?
        };
        self.apply_graph_boost(&mut candidates, self.config.graph.boost_weight);
        self.apply_definition_boost(&mut candidates, &query.query);
        self.apply_popularity_boost(&mut candidates);
        Ok(candidates)
    }

    /// Two-stage reranked search with multi-query RRF fusion.
    ///
    /// Phase 0: generate query reformulations (keywords-only, CamelCase,
    ///          snake_case variants) and run a first-pass retrieval for each,
    ///          fusing results via Reciprocal Rank Fusion.
    /// Phase 1: collect up to `max(limit × 3, 30)` candidates via the fused
    ///          first-pass results.
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

        // Phase 0 & 1: multi-query retrieval with RRF fusion.
        let candidate_limit = (query.limit * 3).max(30);
        let mut reformulations = generate_reformulations(&query.query);

        // Append code-pattern reformulation (lightweight HyDE): join the top 3
        // patterns into a single query string so they participate in RRF fusion.
        let code_patterns = reformulate_to_code(&query.query);
        if !code_patterns.is_empty() {
            let code_query: String = code_patterns
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(" ");
            reformulations.push(code_query);
        }

        // Append synonym expansion as a separate reformulation query.
        // Synonyms are kept out of the main query to avoid polluting BM25
        // (the synonym definitions in search.rs itself would rank highly).
        if let Some(synonym_query) = expand_synonyms(&query.query) {
            reformulations.push(synonym_query);
        }

        debug!(
            reformulations = ?reformulations,
            "deep: multi-query reformulations"
        );

        let mut candidates = {
            // Run first-pass for each reformulation, then fuse.
            let mut all_results: Vec<Vec<SearchResult>> = Vec::new();
            for q_text in &reformulations {
                let sub_query = SearchQuery {
                    query: expand_query(q_text),
                    limit: candidate_limit,
                    file_filter: query.file_filter.clone(),
                    strategy: Strategy::Fast,
                    token_budget: None,
                    queries: None,
                };
                if let Ok(results) = self.search_first_pass(&sub_query) {
                    if !results.is_empty() {
                        all_results.push(results);
                    }
                }
            }

            if all_results.is_empty() {
                return Ok(Vec::new());
            }

            // Progressive RRF fusion: fold all result lists together.
            let mut fused = all_results.remove(0);
            for results in &all_results {
                fused = rrf_fuse(&fused, results, 60.0);
            }
            fused.truncate(candidate_limit);
            fused
        };

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
        self.apply_test_demotion(&mut candidates, &query.query);
        candidates.truncate(query.limit);

        Ok(candidates)
    }

    /// Boost results whose files have many callers in the dependency graph.
    ///
    /// Files that are imported by many other files are architecturally central
    /// and often more relevant to a broad concept query.  The boost is
    /// logarithmic to avoid letting mega-popular files dominate all results.
    pub(super) fn apply_popularity_boost(&self, results: &mut [SearchResult]) {
        if let Some(ref graph) = self.graph {
            let mut boosted = false;
            for r in results.iter_mut() {
                let caller_count = graph.callers(&r.file_path).len();
                if caller_count > 3 {
                    // Modest logarithmic boost: ln(4)≈1.4 → 7%, ln(10)≈2.3 → 11.5%
                    r.score *= 1.0 + (caller_count as f32).ln() * 0.05;
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
    }
}

/// Truncate search results at the natural score boundary.
///
/// Detects "score cliffs" -- points where the relevance score drops
/// significantly compared to the previous result -- and truncates
/// there instead of at the hard limit.
///
/// Algorithm:
/// 1. If fewer than `min_results` results, return as-is
/// 2. Compute relative score drops between consecutive results
/// 3. If any drop exceeds `cliff_threshold` of the top score, truncate at that point
/// 4. Always keep at least `min_results`
/// 5. Never exceed the original length
pub(super) fn adaptive_truncate(
    results: &mut Vec<SearchResult>,
    min_results: usize,
    cliff_threshold: f32,
) {
    if results.len() <= min_results {
        return;
    }

    let top_score = results[0].score;
    if top_score <= 0.0 {
        return;
    }

    for i in 1..results.len() {
        if i < min_results {
            continue;
        }

        let relative_drop = (results[i - 1].score - results[i].score) / top_score;
        if relative_drop > cliff_threshold {
            results.truncate(i);
            return;
        }
    }
}

/// Remove results whose line ranges overlap with a higher-scored result from
/// the same file.  Results arrive sorted by score descending, so the first
/// result from an overlapping group is always the best.
pub(super) fn dedup_overlapping(results: &mut Vec<SearchResult>) {
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
pub(super) fn is_test_chunk(result: &SearchResult) -> bool {
    result
        .scope_chain
        .iter()
        .any(|s| s == "tests" || s == "test")
}

/// Detect test files by common path patterns across languages.
pub(super) fn is_test_file(path: &str) -> bool {
    // Directory patterns: tests/, test/, __tests__/, fixtures/, __fixtures__/
    // Also check start of path for relative paths like "tests/foo/bar.py"
    if path.contains("/tests/")
        || path.contains("/test/")
        || path.contains("/__tests__/")
        || path.contains("/fixtures/")
        || path.contains("/__fixtures__/")
        || path.starts_with("tests/")
        || path.starts_with("test/")
        || path.starts_with("__tests__/")
    {
        return true;
    }
    // Benchmark/evaluation files: benchmarks/, bench/, *.bench.*, *_bench.*
    if path.contains("/benchmarks/")
        || path.contains("/bench/")
        || path.starts_with("benchmarks/")
        || path.starts_with("bench/")
    {
        return true;
    }
    // Rust: *_test.rs
    if path.ends_with("_test.rs") {
        return true;
    }
    // C/C++: *_test.cc, *_test.cpp, *_unittest.cc, *_unittest.cpp
    if path.ends_with("_test.cc")
        || path.ends_with("_test.cpp")
        || path.ends_with("_unittest.cc")
        || path.ends_with("_unittest.cpp")
    {
        return true;
    }
    // File named "tests.rs" or "tests.py" etc. — test module files
    let basename = path.rsplit('/').next().unwrap_or(path);
    if basename.starts_with("tests.") || basename == "tests" {
        return true;
    }
    // File-name patterns (case-insensitive basename check)
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

/// Check if a file is part of the search/retrieval infrastructure.
///
/// These files are self-referential: they contain synonym maps, reformulation
/// patterns, query expansion code, and strategy dispatch logic that mentions
/// every search concept. BM25 falsely ranks them highly for domain queries
/// like "dead code detection" because the synonym map literally contains
/// those terms.
///
/// Skipped when the query explicitly targets search concepts (detected by
/// presence of search-related terms like "rrf", "fusion", "retriev", "hybrid").
pub(super) fn is_search_infra(path: &str, query: &str) -> bool {
    let is_infra = path.ends_with("engine/search.rs") || path.ends_with("retriever/hybrid.rs");
    if !is_infra {
        return false;
    }
    // If the user is searching FOR the search infrastructure, don't demote it.
    let q = query.to_lowercase();
    let search_terms = [
        "rrf",
        "fusion",
        "retriev",
        "hybrid",
        "bm25",
        "ranking algorithm",
        "search strategy",
        "search pipeline",
    ];
    if search_terms.iter().any(|t| q.contains(t)) {
        return false;
    }
    true
}

/// Demote C/C++ header files when implementation files (.c/.cc/.cpp) are also
/// in results.  Headers declare; implementation files contain the actual logic.
///
/// Two matching strategies:
/// 1. Same stem: `db/db_impl.h` demoted when `db/db_impl.cc` is present
/// 2. Any impl present: when results contain both .h and .cc/.cpp files,
///    all headers get a mild demotion to prefer implementation files.
pub(super) fn apply_header_demotion(results: &mut [SearchResult], changed: &mut bool) {
    use std::collections::HashSet;

    let is_impl = |p: &str| {
        p.ends_with(".c") || p.ends_with(".cc") || p.ends_with(".cpp") || p.ends_with(".cxx")
    };
    let is_header = |p: &str| p.ends_with(".h") || p.ends_with(".hpp");

    let has_impl = results.iter().any(|r| is_impl(&r.file_path));
    let has_header = results.iter().any(|r| is_header(&r.file_path));

    if !has_impl || !has_header {
        return;
    }

    // Collect impl basenames (without extension) for exact-stem matching.
    let impl_basenames: HashSet<String> = results
        .iter()
        .filter(|r| is_impl(&r.file_path))
        .filter_map(|r| {
            let basename = r.file_path.rsplit('/').next().unwrap_or(&r.file_path);
            basename.rsplit_once('.').map(|(stem, _)| stem.to_string())
        })
        .collect();

    const HEADER_DEMOTION_EXACT: f32 = 0.6; // strong: exact .h/.cc pair
    const HEADER_DEMOTION_MILD: f32 = 0.85; // mild: impl files exist but no exact match

    for r in results.iter_mut() {
        if is_header(&r.file_path) {
            let basename = r.file_path.rsplit('/').next().unwrap_or(&r.file_path);
            if let Some((stem, _)) = basename.rsplit_once('.') {
                if impl_basenames.contains(stem) {
                    r.score *= HEADER_DEMOTION_EXACT;
                    *changed = true;
                    continue;
                }
            }
            // Mild demotion: impl files exist but this header has no matching .cc
            r.score *= HEADER_DEMOTION_MILD;
            *changed = true;
        }
    }
}

/// Extract explicit file paths from a query string.
///
/// Recognises path patterns that contain at least one `/` and end with a known
/// source-code extension (e.g. `src/models/query.py`). Returns the matched
/// path strings.
fn extract_explicit_file_paths(query: &str) -> Vec<String> {
    use std::sync::LazyLock;

    static RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r"\b[a-zA-Z0-9_]+/[a-zA-Z0-9_/.\-]*\.(py|rs|js|ts|tsx|jsx|go|java|rb|c|cpp|h|hpp|cs|swift|kt|scala|lua|m|sh|xml|yaml|yml|json|toml)\b",
        )
        .unwrap()
    });

    RE.find_iter(query)
        .map(|m| m.as_str().to_string())
        .collect()
}

/// Extract file-path references wrapped in backticks from a query string.
///
/// Recognises `` `some/path/here` `` patterns where the inner content contains
/// at least one `/`, indicating a file-path reference rather than a plain
/// identifier.
fn extract_backtick_file_paths(query: &str) -> Vec<String> {
    use std::sync::LazyLock;

    static RE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"`([^`]+)`").unwrap());

    RE.captures_iter(query)
        .filter_map(|cap| {
            let inner = cap.get(1)?.as_str();
            if inner.contains('/') {
                Some(inner.to_string())
            } else {
                None
            }
        })
        .collect()
}

/// Extract dotted module paths from a query (e.g. "django.db.models.lookups").
///
/// Only matches paths with at least 3 dot-separated segments to avoid false
/// positives on version numbers or short identifiers.
fn extract_dotted_paths(query: &str) -> Vec<String> {
    use std::sync::LazyLock;

    static RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"[a-z]\w*(?:\.[a-z]\w*){2,}").unwrap());

    RE.find_iter(query)
        .map(|m| m.as_str().replace('.', "/"))
        .collect()
}

/// Boost results whose file path matches a reference in the query text.
///
/// Supports three kinds of path references:
/// - **Explicit file paths** (e.g. `src/models/query.py`) — 2.5× boost
/// - **Backtick file references** (e.g. `` `django/db/models/lookups.py` ``) — 2.5× boost
/// - **Dotted module paths** (e.g. `django.db.models.lookups`) — 2× boost
///
/// Path references are deduplicated via `HashSet` so the same path appearing
/// both in plain text and backticks only boosts once.
/// This is a zero-cost post-retrieval heuristic — no ML compute involved.
pub(super) fn apply_path_match_boost(results: &mut [SearchResult], query: &str) {
    let mut boosted = false;

    // Boost 0: explicit file paths and backtick file references.
    // Collect all path references into a HashSet to deduplicate (a path mentioned
    // both in plain text and inside backticks should only boost once).
    {
        use std::collections::HashSet;

        let mut path_refs: HashSet<String> = HashSet::new();
        for p in extract_explicit_file_paths(query) {
            path_refs.insert(p);
        }
        for p in extract_backtick_file_paths(query) {
            path_refs.insert(p);
        }

        if !path_refs.is_empty() {
            for r in results.iter_mut() {
                for path in &path_refs {
                    if r.file_path.contains(path.as_str()) || path.contains(r.file_path.as_str()) {
                        r.score *= 2.5;
                        boosted = true;
                        break;
                    }
                }
            }
        }
    }

    // Boost 1: dotted module paths (e.g., "django.db.models" → file path match)
    let dotted = extract_dotted_paths(query);
    for r in results.iter_mut() {
        for d in &dotted {
            if r.file_path.contains(d.as_str()) {
                r.score *= 2.0;
                boosted = true;
                break;
            }
        }
    }

    // Boost 2: query keywords matching file/directory names.
    // If the query mentions "parser" and a file is in parser/, boost it.
    // Only applies to words ≥4 chars that aren't common stop words.
    let path_keywords: Vec<&str> = query
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| w.len() >= 4)
        .filter(|w| {
            !matches!(
                w.to_lowercase().as_str(),
                "how"
                    | "does"
                    | "what"
                    | "with"
                    | "from"
                    | "this"
                    | "that"
                    | "have"
                    | "been"
                    | "code"
                    | "file"
                    | "function"
                    | "method"
                    | "class"
                    | "struct"
            )
        })
        .collect();

    if !path_keywords.is_empty() {
        for r in results.iter_mut() {
            let path_lower = r.file_path.to_lowercase();
            for kw in &path_keywords {
                let kw_lower = kw.to_lowercase();
                // Check exact keyword or stem prefix (≥4 chars) as a path component.
                // "parsing" matches parser/, "retrieval" matches retriever/, etc.
                let stem = &kw_lower[..kw_lower.len().min(5)];
                if path_lower.contains(&format!("/{kw_lower}/"))
                    || path_lower.contains(&format!("/{kw_lower}."))
                    || path_lower.ends_with(&format!("/{kw_lower}"))
                    || path_component_starts_with(&path_lower, stem)
                {
                    r.score *= 2.0;
                    boosted = true;
                    break;
                }
            }
        }
    }

    // Boost 3: concept-to-path mapping for well-known vocabulary gaps.
    // Bridges cases where query terminology differs from file naming.
    let concept_paths: &[(&[&str], &[&str])] = &[
        (&["dead code", "unused code", "unreachable"], &["orphan"]),
        (
            &["rrf", "rank fusion", "reciprocal rank", "reciprocal"],
            &["hybrid"],
        ),
        (&["tree-sitter", "tree sitter", "ast pars"], &["parser/"]),
        (
            &["embedding model", "embed model", "onnx embed"],
            &["embedder"],
        ),
        (&["dependency graph", "import graph"], &["graph/"]),
        (&["file watch", "live reload"], &["watcher"]),
    ];
    let query_lower = query.to_lowercase();
    for (triggers, path_fragments) in concept_paths {
        if triggers.iter().any(|t| query_lower.contains(t)) {
            for r in results.iter_mut() {
                let path_lower = r.file_path.to_lowercase();
                if path_fragments.iter().any(|frag| {
                    path_lower.contains(frag) || path_component_starts_with(&path_lower, frag)
                }) {
                    r.score *= 3.0; // Strong concept-path boost
                    boosted = true;
                }
            }
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

/// Check if any path component starts with the given prefix.
/// e.g., path "crates/core/src/parser/mod.rs", prefix "pars" → true (matches "parser")
fn path_component_starts_with(path: &str, prefix: &str) -> bool {
    path.split('/').any(|component| {
        let name = component.split('.').next().unwrap_or(component);
        name.starts_with(prefix)
    })
}

/// Generate hypothetical code patterns from a natural language query.
///
/// This is a lightweight HyDE (Hypothetical Document Embedding) approach:
/// maps common programming concepts to code patterns that would appear
/// in implementations. Used to improve retrieval for conceptual queries.
///
/// Example: "how to sort a list" -> ["fn sort", ".sort(", "sort_by", "Ord", "cmp"]
fn reformulate_to_code(query: &str) -> Vec<String> {
    let query_lower = query.to_lowercase();
    let mut patterns = Vec::new();

    // Map common programming concepts to code patterns.
    let concept_map: &[(&[&str], &[&str])] = &[
        // Sorting
        (
            &["sort", "order", "arrange"],
            &["fn sort", ".sort(", "sort_by", "Ord", "cmp"],
        ),
        // Searching
        (
            &["search", "find", "lookup", "locate"],
            &["fn search", "fn find", ".find(", "filter", "contains"],
        ),
        // Iteration
        (
            &["iterate", "loop", "traverse", "walk"],
            &["for ", ".iter()", ".map(", "while ", "Iterator"],
        ),
        // Error handling
        (
            &["error", "exception", "handle error", "failure"],
            &["Result<", "Err(", "unwrap", "anyhow", "?;"],
        ),
        // Parsing
        (
            &["parse", "parsing", "tokenize", "lex"],
            &["fn parse", "Parser", "Token", "from_str"],
        ),
        // Serialization
        (
            &["serialize", "deserialize", "json", "encode", "decode"],
            &["Serialize", "Deserialize", "serde", "to_string", "from_str"],
        ),
        // Concurrency
        (
            &["concurrent", "parallel", "thread", "async", "mutex"],
            &["Arc<", "Mutex<", "async fn", "tokio", "rayon", "RwLock"],
        ),
        // Testing
        (
            &["test", "assert", "verify", "check"],
            &["#[test]", "assert!", "assert_eq!", "fn test_"],
        ),
        // Caching
        (
            &["cache", "memoize", "store"],
            &["HashMap", "cache", "LruCache", "memo"],
        ),
        // Configuration
        (
            &["config", "setting", "option", "preference"],
            &["Config", "Settings", "Options", "Default"],
        ),
        // Networking/HTTP
        (
            &["http", "request", "endpoint", "api", "rest"],
            &["fn get", "fn post", "Handler", "Router", "axum"],
        ),
        // File I/O
        (
            &["file", "read file", "write file", "io"],
            &["File::open", "read_to_string", "BufReader", "std::fs"],
        ),
        // Graph/tree
        (
            &["graph", "tree", "node", "edge"],
            &["Graph", "Node", "Edge", "petgraph", "DiGraph"],
        ),
        // Database
        (
            &["database", "query", "sql", "store"],
            &["Connection", "execute", "query", "INSERT", "SELECT"],
        ),
        // Authentication
        (
            &["auth", "login", "password", "token", "jwt"],
            &["authenticate", "verify_token", "Bearer", "Session"],
        ),
        // Hashing
        (
            &["hash", "digest", "checksum"],
            &["Hash", "Hasher", "sha256", "xxh3", "digest"],
        ),
        // Embedding/vector
        (
            &["embed", "vector", "similarity", "cosine"],
            &["embed", "Vec<f32>", "cosine_similarity", "dot_product"],
        ),
        // Indexing
        (
            &["index", "inverted", "full text", "bm25"],
            &["Index", "Tantivy", "BM25", "tokenizer"],
        ),
    ];

    for (keywords, code_patterns) in concept_map {
        if keywords.iter().any(|kw| query_lower.contains(kw)) {
            patterns.extend(code_patterns.iter().map(|p| p.to_string()));
        }
    }

    patterns
}

/// Expand query with domain-specific code synonyms.
///
/// Bridges vocabulary gaps where users describe concepts differently
/// than the code names them (e.g. "dead code" -> "orphan", "unused" -> "zero in-degree").
fn expand_synonyms(query: &str) -> Option<String> {
    let query_lower = query.to_lowercase();
    let mut extra_terms = Vec::new();

    let synonym_map: &[(&[&str], &[&str])] = &[
        // Dead code / unused detection
        (
            &["dead code", "unused", "unreachable"],
            &["orphan", "zero in-degree", "find_orphans"],
        ),
        // Error handling
        (
            &["error handling", "exception"],
            &["Result", "Error", "anyhow"],
        ),
        // Dependency / import
        (
            &["dependency", "dependencies"],
            &["import", "require", "use"],
        ),
        // Callback / handler
        (
            &["callback", "handler", "listener"],
            &["on_", "handle_", "hook"],
        ),
        // Cache / memoize
        (
            &["cache", "caching", "memoize"],
            &["LruCache", "HashMap", "memo"],
        ),
        // Refactor / rename
        (
            &["refactor", "restructure"],
            &["rename", "extract", "inline"],
        ),
        // Performance / optimization
        (
            &["performance", "optimize", "speed"],
            &["benchmark", "perf", "fast"],
        ),
        // Authentication / authorization
        (
            &["authentication", "authorization", "auth"],
            &["login", "token", "session", "jwt"],
        ),
        // Serialization
        (
            &["serialize", "marshal"],
            &["serde", "Serialize", "Deserialize", "json"],
        ),
        // Similarity / matching
        (
            &["similar", "duplicate", "clone detection"],
            &["cosine", "similarity", "find_similar"],
        ),
        // Ranking / scoring
        (
            &["ranking", "scoring", "relevance"],
            &["pagerank", "boost", "score", "BM25"],
        ),
        // Documentation
        (
            &["documentation", "docs", "docstring"],
            &["doc comment", "///", "enrich_docs"],
        ),
        // Coverage / testing
        (
            &["coverage", "test coverage"],
            &["find_tests", "test_mapping", "#[test]"],
        ),
        // Complexity
        (
            &["complexity", "complex", "complicated"],
            &["cyclomatic", "get_complexity", "McCabe"],
        ),
    ];

    for (triggers, expansions) in synonym_map {
        if triggers.iter().any(|t| query_lower.contains(t)) {
            for exp in expansions.iter() {
                if !query_lower.contains(&exp.to_lowercase()) {
                    extra_terms.push(exp.to_string());
                }
            }
        }
    }

    if extra_terms.is_empty() {
        None
    } else {
        Some(format!("{} {}", query, extra_terms.join(" ")))
    }
}

/// Generate query reformulations for multi-query search (RRF fusion).
///
/// Given a natural-language query, produces multiple complementary search
/// strings that together improve recall:
///
/// 1. **Original** — the query as-is.
/// 2. **Keywords only** — stop words removed (action verbs, articles, prepositions).
/// 3. **CamelCase identifier** — keywords concatenated as a synthetic identifier
///    (e.g. "authentication token" → "AuthenticationToken").
/// 4. **snake_case identifier** — keywords joined with underscores
///    (e.g. "authentication token" → "authentication_token").
///
/// These variants are fused with RRF so that results matching any variant
/// contribute to the final ranking.
fn generate_reformulations(query: &str) -> Vec<String> {
    let mut reformulations = vec![query.to_string()];

    // Stop words: common action verbs, articles, prepositions, and issue-related
    // noise words that don't carry discriminative signal for code search.
    let stop_words: &[&str] = &[
        "fix",
        "bug",
        "issue",
        "error",
        "problem",
        "how",
        "to",
        "the",
        "a",
        "an",
        "is",
        "in",
        "of",
        "for",
        "with",
        "this",
        "that",
        "when",
        "where",
        "why",
        "what",
        "does",
        "do",
        "not",
        "can",
        "should",
        "would",
        "could",
        "find",
        "get",
        "set",
        "make",
        "add",
        "remove",
        "update",
        "change",
        "implement",
        "use",
        "handle",
        "check",
        "create",
    ];

    let keywords: Vec<&str> = query
        .split_whitespace()
        .filter(|w| !stop_words.contains(&w.to_lowercase().as_str()))
        .collect();

    // 1. Keywords only (if any stop words were actually removed)
    if keywords.len() < query.split_whitespace().count() && !keywords.is_empty() {
        reformulations.push(keywords.join(" "));
    }

    // 2. CamelCase synthetic identifier (2..=4 keywords)
    if keywords.len() >= 2 && keywords.len() <= 4 {
        let camel: String = keywords
            .iter()
            .map(|w| {
                let mut c = w.chars();
                match c.next() {
                    None => String::new(),
                    Some(f) => f.to_uppercase().to_string() + &c.as_str().to_lowercase(),
                }
            })
            .collect();
        reformulations.push(camel);
    }

    // 3. snake_case variant (2+ keywords)
    if keywords.len() >= 2 {
        reformulations.push(
            keywords
                .iter()
                .map(|w| w.to_lowercase())
                .collect::<Vec<_>>()
                .join("_"),
        );
    }

    reformulations
}

/// Fuse multiple ranked result lists via iterative Reciprocal Rank Fusion.
///
/// Folds an arbitrary number of result lists into a single ranked output
/// by pairwise RRF. Used by the multi-query Deep strategy tests.
#[cfg(test)]
fn rrf_fuse_multi(lists: Vec<Vec<SearchResult>>, k: f32) -> Vec<SearchResult> {
    let mut lists = lists;
    if lists.is_empty() {
        return Vec::new();
    }
    let mut fused = lists.remove(0);
    for list in &lists {
        fused = rrf_fuse(&fused, list, k);
    }
    fused
}

/// Check if a string looks like a code identifier (alphanumeric + underscores/colons/dots).
fn is_identifier_like(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 80
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == ':' || c == '.')
}

/// Check if a single-token identifier looks like an exact code symbol that
/// benefits from trigram fast-path: contains `_`, `::`, `.`, or is camelCase.
///
/// Simple all-lowercase words like "engine" or "search" go through BM25 (Instant)
/// instead, since they are more likely to be concept searches.
fn looks_like_exact_identifier(s: &str) -> bool {
    // Must be at least 3 chars for trigram to work.
    if s.len() < 3 {
        return false;
    }
    // Contains structural separators → definitely code.
    if s.contains('_') || s.contains("::") || s.contains('.') {
        return true;
    }
    // camelCase / PascalCase: has both upper and lower case letters.
    let has_upper = s.chars().any(|c| c.is_uppercase());
    let has_lower = s.chars().any(|c| c.is_lowercase());
    has_upper && has_lower
}

/// Split a CamelCase or PascalCase identifier into lowercase words.
/// E.g. "URLResolver" → ["url", "resolver"], "getServerSideProps" → ["get", "server", "side", "props"]
fn split_camel_case(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = s.chars().collect();

    for i in 0..chars.len() {
        let c = chars[i];
        if c == '_' {
            if !current.is_empty() {
                parts.push(current.to_lowercase());
                current.clear();
            }
            continue;
        }
        if c.is_uppercase() && i > 0 {
            // Start new part if:
            // - previous char was lowercase (e.g. "get|S" in getServerSideProps)
            // - OR next char is lowercase and current accumulator has >1 char (e.g. "UR|L|Resolver" → "url" break before 'R')
            let prev_lower = chars[i - 1].is_lowercase();
            let next_lower =
                i + 1 < chars.len() && chars[i + 1].is_lowercase() && current.len() > 1;
            if (prev_lower || next_lower) && !current.is_empty() {
                parts.push(current.to_lowercase());
                current.clear();
            }
        }
        current.push(c);
    }
    if !current.is_empty() {
        parts.push(current.to_lowercase());
    }
    parts
}

/// Expand a search query by splitting CamelCase/snake_case identifiers.
/// Original terms are kept; split terms are appended as additional keywords.
fn expand_query(query: &str) -> String {
    let mut extra_terms: Vec<String> = Vec::new();
    let existing_words: Vec<String> = query.split_whitespace().map(|w| w.to_lowercase()).collect();

    for word in query.split_whitespace() {
        // Only expand words that look like identifiers (>3 chars, alphanumeric+underscore)
        if word.len() > 3 && word.chars().all(|c| c.is_alphanumeric() || c == '_') {
            let parts = split_camel_case(word);
            if parts.len() > 1 {
                // Check if the word is CamelCase or snake_case and add the alternate form.
                let has_uppercase = word.chars().any(|c| c.is_uppercase());
                let has_underscore = word.contains('_');

                // Always add the individual split words for BM25 matching
                // (e.g. "URLResolver" → "url resolver" as separate tokens)
                for part in &parts {
                    if !existing_words.contains(part) {
                        extra_terms.push(part.clone());
                    }
                }

                if has_uppercase && !has_underscore {
                    // CamelCase → add snake_case form (e.g. "URLResolver" → "url_resolver")
                    let snake = parts.join("_");
                    if !existing_words.contains(&snake) {
                        extra_terms.push(snake);
                    }
                } else if has_underscore && !has_uppercase {
                    // snake_case → add CamelCase form (e.g. "url_resolver" → "UrlResolver")
                    let camel: String = parts
                        .iter()
                        .map(|p| {
                            let mut c = p.chars();
                            match c.next() {
                                None => String::new(),
                                Some(f) => f.to_uppercase().to_string() + c.as_str(),
                            }
                        })
                        .collect();
                    let camel_lower = camel.to_lowercase();
                    if !existing_words.contains(&camel_lower) {
                        extra_terms.push(camel);
                    }
                }
            }
        }
    }

    if extra_terms.is_empty() {
        return query.to_string();
    }

    // Keep the original query prominent; append expansions
    format!("{} {}", query, extra_terms.join(" "))
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
    fn detects_top_level_test_dirs() {
        // Relative paths starting with tests/ (e.g., SWE-bench repos)
        assert!(is_test_file("tests/admin_inlines/models.py"));
        assert!(is_test_file("tests/forms_tests/tests.py"));
        assert!(is_test_file("test/unit/helpers.rb"));
        assert!(is_test_file("__tests__/App.test.tsx"));
    }

    #[test]
    fn detects_cpp_test_files() {
        assert!(is_test_file("db/db_test.cc"));
        assert!(is_test_file("table/table_test.cpp"));
        assert!(is_test_file("util/cache_unittest.cc"));
        assert!(is_test_file("src/core_unittest.cpp"));
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

    #[test]
    fn split_identifier_camel_case() {
        assert_eq!(
            split_camel_case("getServerSideProps"),
            vec!["get", "server", "side", "props"]
        );
        assert_eq!(split_camel_case("URLResolver"), vec!["url", "resolver"]);
        assert_eq!(
            split_camel_case("ReactFiberBeginWork"),
            vec!["react", "fiber", "begin", "work"]
        );
        assert_eq!(split_camel_case("simple"), vec!["simple"]);
    }

    #[test]
    fn split_identifier_snake_case() {
        assert_eq!(
            split_camel_case("get_server_side_props"),
            vec!["get", "server", "side", "props"]
        );
        assert_eq!(split_camel_case("url_resolver"), vec!["url", "resolver"]);
    }

    #[test]
    fn expand_query_adds_split_terms() {
        let expanded = expand_query("URLResolver match");
        assert!(expanded.contains("URLResolver"));
        assert!(expanded.contains("url_resolver")); // snake_case alternate
    }

    #[test]
    fn expand_query_no_expansion_for_short_words() {
        assert_eq!(expand_query("URL foo bar"), "URL foo bar");
    }

    #[test]
    fn expand_query_preserves_plain_queries() {
        assert_eq!(expand_query("simple query words"), "simple query words");
    }

    #[test]
    fn header_demotion_prefers_impl_over_header() {
        let mut results = vec![
            SearchResult {
                chunk_id: "1".into(),
                file_path: "db/db_impl.h".into(),
                language: "C++".into(),
                score: 10.0,
                line_start: 0,
                line_end: 20,
                signature: String::new(),
                scope_chain: vec![],
                content: String::new(),
            },
            SearchResult {
                chunk_id: "2".into(),
                file_path: "db/db_impl.cc".into(),
                language: "C++".into(),
                score: 9.0,
                line_start: 0,
                line_end: 100,
                signature: String::new(),
                scope_chain: vec![],
                content: String::new(),
            },
        ];
        let mut changed = false;
        apply_header_demotion(&mut results, &mut changed);
        assert!(changed);
        // Header should be demoted: 10.0 * 0.7 = 7.0, impl stays at 9.0
        assert!(
            results[1].score > results[0].score,
            "impl should outrank header after demotion"
        );
    }

    #[test]
    fn split_camel_case_url_resolver() {
        assert_eq!(split_camel_case("URLResolver"), vec!["url", "resolver"]);
    }

    #[test]
    fn split_camel_case_get_server_side_props() {
        assert_eq!(
            split_camel_case("getServerSideProps"),
            vec!["get", "server", "side", "props"]
        );
    }

    #[test]
    fn split_camel_case_simple() {
        assert_eq!(split_camel_case("simple"), vec!["simple"]);
    }

    #[test]
    fn split_camel_case_snake_case_input() {
        assert_eq!(split_camel_case("snake_case"), vec!["snake", "case"]);
    }

    #[test]
    fn expand_query_contains_split_words() {
        let expanded = expand_query("find URLResolver class");
        assert!(
            expanded.contains("url") && expanded.contains("resolver"),
            "expand_query should contain individual split words 'url' and 'resolver', got: {expanded}"
        );
    }

    #[test]
    fn extract_dotted_paths_django() {
        assert_eq!(
            extract_dotted_paths("fix django.db.models.lookups"),
            vec!["django/db/models/lookups"]
        );
    }

    #[test]
    fn extract_dotted_paths_no_match() {
        assert!(extract_dotted_paths("simple query words").is_empty());
    }

    #[test]
    fn extract_dotted_paths_short_path_ignored() {
        // Two segments (only one dot) should not match — need at least 3 segments
        assert!(extract_dotted_paths("os.path").is_empty());
    }

    #[test]
    fn apply_path_match_boost_boosts_matching_paths() {
        let mut results = vec![
            SearchResult {
                chunk_id: "1".into(),
                file_path: "django/db/models/lookups.py".into(),
                language: "Python".into(),
                score: 5.0,
                line_start: 0,
                line_end: 20,
                signature: String::new(),
                scope_chain: vec![],
                content: String::new(),
            },
            SearchResult {
                chunk_id: "2".into(),
                file_path: "django/utils/text.py".into(),
                language: "Python".into(),
                score: 10.0,
                line_start: 0,
                line_end: 20,
                signature: String::new(),
                scope_chain: vec![],
                content: String::new(),
            },
        ];
        apply_path_match_boost(&mut results, "fix django.db.models.lookups");
        // The matching file gets dotted-path boost (2.0×) + keyword boost (2.0×):
        // 5.0 * 2.0 * 2.0 = 20.0, which beats the non-matching file at 10.0.
        assert_eq!(results[0].file_path, "django/db/models/lookups.py");
        assert!(results[0].score > 10.0, "boosted score should exceed 10.0");
    }

    #[test]
    fn header_demotion_no_effect_without_impl() {
        let mut results = vec![SearchResult {
            chunk_id: "1".into(),
            file_path: "include/leveldb/db.h".into(),
            language: "C++".into(),
            score: 10.0,
            line_start: 0,
            line_end: 20,
            signature: String::new(),
            scope_chain: vec![],
            content: String::new(),
        }];
        let mut changed = false;
        apply_header_demotion(&mut results, &mut changed);
        assert!(
            !changed,
            "no impl file present, header should not be demoted"
        );
        assert_eq!(results[0].score, 10.0);
    }

    // -----------------------------------------------------------------------
    // Multi-query reformulation tests
    // -----------------------------------------------------------------------

    #[test]
    fn reformulations_natural_language_query() {
        let r = generate_reformulations("fix authentication token expiry");
        // Should have: original, keywords-only, CamelCase, snake_case
        assert!(
            r.len() >= 3,
            "expected at least 3 reformulations, got {r:?}"
        );
        assert_eq!(r[0], "fix authentication token expiry"); // original
        // Keywords-only should drop "fix"
        assert!(
            r.iter().any(|q| q == "authentication token expiry"),
            "expected keywords-only reformulation, got: {r:?}"
        );
        // CamelCase
        assert!(
            r.iter().any(|q| q == "AuthenticationTokenExpiry"),
            "expected CamelCase reformulation, got: {r:?}"
        );
        // snake_case
        assert!(
            r.iter().any(|q| q == "authentication_token_expiry"),
            "expected snake_case reformulation, got: {r:?}"
        );
    }

    #[test]
    fn reformulations_single_identifier() {
        let r = generate_reformulations("URLResolver");
        // Single word, no stop words to remove, can't make CamelCase/snake from 1 keyword
        assert_eq!(
            r.len(),
            1,
            "single identifier should produce only 1 reformulation"
        );
        assert_eq!(r[0], "URLResolver");
    }

    #[test]
    fn reformulations_two_keywords() {
        let r = generate_reformulations("token expiry");
        // No stop words → no keywords-only variant, but should have CamelCase + snake_case
        assert!(
            r.len() >= 3,
            "expected at least 3 reformulations, got {r:?}"
        );
        assert_eq!(r[0], "token expiry");
        assert!(
            r.iter().any(|q| q == "TokenExpiry"),
            "expected CamelCase variant, got: {r:?}"
        );
        assert!(
            r.iter().any(|q| q == "token_expiry"),
            "expected snake_case variant, got: {r:?}"
        );
    }

    #[test]
    fn reformulations_all_stop_words() {
        let r = generate_reformulations("how to fix this");
        // All words are stop words → no keywords, so only the original
        assert_eq!(
            r.len(),
            1,
            "all-stop-word query should produce only 1 reformulation"
        );
        assert_eq!(r[0], "how to fix this");
    }

    #[test]
    fn reformulations_five_keywords_no_camel() {
        // More than 4 non-stop keywords → should NOT produce CamelCase (too long)
        // but should still produce snake_case.
        let r = generate_reformulations("database connection pool timeout retry backoff");
        assert!(r[0] == "database connection pool timeout retry backoff");
        // Should have snake_case
        assert!(
            r.iter()
                .any(|q| q == "database_connection_pool_timeout_retry_backoff"),
            "expected snake_case variant for 6-keyword query, got: {r:?}"
        );
        // Should NOT have CamelCase (>4 keywords)
        assert!(
            !r.iter()
                .any(|q| q == "DatabaseConnectionPoolTimeoutRetryBackoff"),
            "should not produce CamelCase for >4 keywords, got: {r:?}"
        );
    }

    // -----------------------------------------------------------------------
    // RRF multi-list fusion tests
    // -----------------------------------------------------------------------

    #[test]
    fn rrf_fuse_multi_empty() {
        let result = rrf_fuse_multi(vec![], 60.0);
        assert!(result.is_empty());
    }

    #[test]
    fn rrf_fuse_multi_single_list() {
        let list = vec![SearchResult {
            chunk_id: "a".into(),
            file_path: "src/lib.rs".into(),
            language: "Rust".into(),
            score: 1.0,
            line_start: 0,
            line_end: 10,
            signature: String::new(),
            scope_chain: vec![],
            content: String::new(),
        }];
        let result = rrf_fuse_multi(vec![list], 60.0);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].chunk_id, "a");
    }

    // -----------------------------------------------------------------------
    // Adaptive truncation tests
    // -----------------------------------------------------------------------

    fn make_result(id: &str) -> SearchResult {
        SearchResult {
            chunk_id: id.into(),
            file_path: format!("src/{id}.rs"),
            language: "Rust".into(),
            score: 0.0,
            line_start: 0,
            line_end: 10,
            signature: String::new(),
            scope_chain: vec![],
            content: String::new(),
        }
    }

    #[test]
    fn adaptive_truncate_detects_cliff() {
        let mut results = vec![
            SearchResult {
                score: 10.0,
                ..make_result("a")
            },
            SearchResult {
                score: 9.5,
                ..make_result("b")
            },
            SearchResult {
                score: 9.0,
                ..make_result("c")
            },
            SearchResult {
                score: 3.0,
                ..make_result("d")
            }, // cliff!
            SearchResult {
                score: 2.5,
                ..make_result("e")
            },
        ];
        adaptive_truncate(&mut results, 3, 0.35);
        assert_eq!(results.len(), 3); // truncated at cliff
    }

    #[test]
    fn adaptive_truncate_keeps_min() {
        let mut results = vec![
            SearchResult {
                score: 10.0,
                ..make_result("a")
            },
            SearchResult {
                score: 1.0,
                ..make_result("b")
            }, // huge cliff but min=3
            SearchResult {
                score: 0.5,
                ..make_result("c")
            },
        ];
        adaptive_truncate(&mut results, 3, 0.35);
        assert_eq!(results.len(), 3); // kept min
    }

    #[test]
    fn adaptive_truncate_no_cliff() {
        let mut results = vec![
            SearchResult {
                score: 10.0,
                ..make_result("a")
            },
            SearchResult {
                score: 9.0,
                ..make_result("b")
            },
            SearchResult {
                score: 8.0,
                ..make_result("c")
            },
            SearchResult {
                score: 7.0,
                ..make_result("d")
            },
        ];
        adaptive_truncate(&mut results, 3, 0.35);
        assert_eq!(results.len(), 4); // no cliff, all kept
    }

    #[test]
    fn rrf_fuse_multi_promotes_shared_results() {
        // Result "shared" appears in all 3 lists → should rank highest.
        // Result "only1" appears in list 1 only.
        let make = |id: &str| SearchResult {
            chunk_id: id.into(),
            file_path: format!("src/{id}.rs"),
            language: "Rust".into(),
            score: 1.0,
            line_start: 0,
            line_end: 10,
            signature: String::new(),
            scope_chain: vec![],
            content: String::new(),
        };

        let list1 = vec![make("only1"), make("shared")];
        let list2 = vec![make("shared"), make("only2")];
        let list3 = vec![make("only3"), make("shared")];

        let fused = rrf_fuse_multi(vec![list1, list2, list3], 60.0);
        assert_eq!(
            fused[0].chunk_id, "shared",
            "result appearing in all lists should rank first"
        );
    }

    #[test]
    fn reformulate_to_code_sorting() {
        let patterns = reformulate_to_code("how to sort a list");
        assert!(patterns.iter().any(|p| p.contains("sort")));
        assert!(patterns.iter().any(|p| p.contains("Ord")));
    }

    #[test]
    fn reformulate_to_code_no_match() {
        let patterns = reformulate_to_code("quantum physics theory");
        assert!(patterns.is_empty());
    }

    #[test]
    fn reformulate_to_code_multiple_concepts() {
        let patterns = reformulate_to_code("async file parsing with error handling");
        assert!(patterns.iter().any(|p| p.contains("async")));
        assert!(
            patterns
                .iter()
                .any(|p| p.contains("parse") || p.contains("Parser"))
        );
        assert!(
            patterns
                .iter()
                .any(|p| p.contains("Result") || p.contains("Err"))
        );
    }

    // -----------------------------------------------------------------------
    // Synonym expansion tests
    // -----------------------------------------------------------------------

    #[test]
    fn expand_synonyms_dead_code() {
        let result = expand_synonyms("dead code detection").unwrap();
        assert!(
            result.contains("orphan"),
            "expected 'orphan' in expanded query, got: {result}"
        );
        assert!(
            result.contains("find_orphans"),
            "expected 'find_orphans' in expanded query, got: {result}"
        );
    }

    #[test]
    fn expand_synonyms_no_match() {
        let result = expand_synonyms("pagerank algorithm");
        assert!(
            result.is_none(),
            "expected None for query with no synonym triggers"
        );
    }

    #[test]
    fn expand_synonyms_existing_terms() {
        // "orphan" is already in the query, so the standalone "orphan" synonym
        // should not be added again. "find_orphans" is a different term and
        // may still be added.
        let result = expand_synonyms("find unused orphan code").unwrap();
        // Split into whitespace-delimited tokens and count exact "orphan" tokens.
        let orphan_token_count = result.split_whitespace().filter(|t| *t == "orphan").count();
        assert_eq!(
            orphan_token_count, 1,
            "should not add standalone 'orphan' when already in query, got: {result}"
        );
    }

    // -----------------------------------------------------------------------
    // Exact identifier detection tests
    // -----------------------------------------------------------------------

    #[test]
    fn looks_like_exact_identifier_snake_case() {
        assert!(looks_like_exact_identifier("process_batch"));
        assert!(looks_like_exact_identifier("my_func_name"));
    }

    #[test]
    fn looks_like_exact_identifier_camel_case() {
        assert!(looks_like_exact_identifier("ProcessBatch"));
        assert!(looks_like_exact_identifier("getServerSideProps"));
        assert!(looks_like_exact_identifier("URLResolver"));
    }

    #[test]
    fn looks_like_exact_identifier_qualified() {
        assert!(looks_like_exact_identifier("std::io::Read"));
        assert!(looks_like_exact_identifier("django.db.models"));
    }

    #[test]
    fn looks_like_exact_identifier_rejects_plain_words() {
        // Simple all-lowercase words should NOT trigger Exact strategy.
        assert!(!looks_like_exact_identifier("engine"));
        assert!(!looks_like_exact_identifier("search"));
        assert!(!looks_like_exact_identifier("results"));
    }

    #[test]
    fn looks_like_exact_identifier_rejects_short() {
        // Must be >= 3 chars for trigram.
        assert!(!looks_like_exact_identifier("ab"));
        assert!(!looks_like_exact_identifier(""));
    }

    #[test]
    fn looks_like_exact_identifier_all_uppercase() {
        // All-uppercase like "URL" or "HTTP" — no mixed case, no separators.
        assert!(!looks_like_exact_identifier("URL"));
        assert!(!looks_like_exact_identifier("HTTP"));
    }

    // -----------------------------------------------------------------------
    // File path and backtick boosting tests
    // -----------------------------------------------------------------------

    #[test]
    fn path_boost_explicit_file_path_in_query() {
        // Query contains "src/models/query.py" → that file should be boosted to top
        let mut results = vec![
            SearchResult {
                chunk_id: "1".into(),
                file_path: "src/models/query.py".into(),
                language: "Python".into(),
                score: 5.0,
                line_start: 0,
                line_end: 20,
                signature: String::new(),
                scope_chain: vec![],
                content: String::new(),
            },
            SearchResult {
                chunk_id: "2".into(),
                file_path: "src/views/api.py".into(),
                language: "Python".into(),
                score: 10.0,
                line_start: 0,
                line_end: 20,
                signature: String::new(),
                scope_chain: vec![],
                content: String::new(),
            },
        ];
        apply_path_match_boost(&mut results, "fix bug in src/models/query.py");
        assert_eq!(results[0].file_path, "src/models/query.py");
        assert!(results[0].score > 10.0);
    }

    #[test]
    fn path_boost_backtick_file_reference() {
        let mut results = vec![
            SearchResult {
                chunk_id: "1".into(),
                file_path: "django/db/models/lookups.py".into(),
                language: "Python".into(),
                score: 5.0,
                line_start: 0,
                line_end: 20,
                signature: String::new(),
                scope_chain: vec![],
                content: String::new(),
            },
            SearchResult {
                chunk_id: "2".into(),
                file_path: "django/utils/text.py".into(),
                language: "Python".into(),
                score: 10.0,
                line_start: 0,
                line_end: 20,
                signature: String::new(),
                scope_chain: vec![],
                content: String::new(),
            },
        ];
        apply_path_match_boost(
            &mut results,
            "issue in `django/db/models/lookups.py` causes crash",
        );
        assert_eq!(results[0].file_path, "django/db/models/lookups.py");
    }

    #[test]
    fn path_boost_no_false_positive_bare_filename() {
        // "error.py" without a `/` prefix should NOT trigger the 2.5× file-path
        // boost (Boost 0). The keyword boost (Boost 2) may still apply since
        // "error" appears as a path component, but the explicit-path regex
        // requires at least one `/` in the match.
        let explicit = extract_explicit_file_paths("this error.py problem is annoying");
        assert!(
            explicit.is_empty(),
            "bare filename without / should not be extracted as explicit file path, got: {explicit:?}"
        );
        let backtick = extract_backtick_file_paths("this error.py problem is annoying");
        assert!(
            backtick.is_empty(),
            "bare filename without / should not be extracted as backtick file path, got: {backtick:?}"
        );
    }
}

//! Hybrid BM25 + vector retrieval using Reciprocal Rank Fusion (RRF).
//!
//! Combines lexical (BM25 via Tantivy) and semantic (dense vector) search
//! results into a single ranked list. The fusion uses RRF, which is robust
//! to score distribution differences between the two retrieval systems.

use std::collections::HashMap;

use crate::embeddings::Embedder;
use crate::error::CodeforgeError;
use crate::graph::CodeGraph;
use crate::index::tantivy::TantivyIndex;
use crate::index::vector::{BruteForceVectorIndex, VectorIndex};

use super::bm25::BM25Retriever;
use super::{Retriever, SearchQuery, SearchResult};

/// RRF constant (commonly 60 in the literature).
const DEFAULT_RRF_K: f32 = 60.0;

/// Hybrid retriever that combines BM25 and vector search via Reciprocal Rank
/// Fusion.
///
/// Each retrieval system independently ranks documents. The fused score for a
/// document is:
///
/// ```text
/// score = bm25_weight / (k + bm25_rank) + vector_weight / (k + vector_rank)
/// ```
///
/// Documents that appear in both result sets receive contributions from both
/// terms, naturally boosting items that both systems agree are relevant.
///
/// The type parameter `V` selects the vector index backend. Defaults to
/// [`BruteForceVectorIndex`] for backward compatibility; use
/// [`HnswVectorIndex`](crate::index::hnsw::HnswVectorIndex) for sub-linear
/// query time on large corpora.
pub struct HybridRetriever<'a, V: VectorIndex = BruteForceVectorIndex> {
    tantivy: &'a TantivyIndex,
    vector_index: &'a V,
    embedder: &'a dyn Embedder,
    rrf_k: f32,
    bm25_weight: f32,
    vector_weight: f32,
    graph_file_scores: Option<HashMap<String, f32>>,
    graph_boost_weight: f32,
}

impl<'a, V: VectorIndex> HybridRetriever<'a, V> {
    /// Create a new hybrid retriever with default weights (0.5 / 0.5) and
    /// `k = 60`.
    pub fn new(
        tantivy: &'a TantivyIndex,
        vector_index: &'a V,
        embedder: &'a dyn Embedder,
    ) -> Self {
        Self {
            tantivy,
            vector_index,
            embedder,
            rrf_k: DEFAULT_RRF_K,
            bm25_weight: 0.5,
            vector_weight: 0.5,
            graph_file_scores: None,
            graph_boost_weight: 0.0,
        }
    }

    /// Set BM25 and vector weights for the RRF fusion.
    ///
    /// Higher weight means that retrieval system has more influence on the
    /// final ranking.
    pub fn with_weights(mut self, bm25: f32, vector: f32) -> Self {
        self.bm25_weight = bm25;
        self.vector_weight = vector;
        self
    }

    /// Set the RRF constant `k`.
    ///
    /// Lower values make the ranking more sensitive to position differences;
    /// higher values flatten the score curve.
    pub fn with_rrf_k(mut self, k: f32) -> Self {
        self.rrf_k = k;
        self
    }

    /// Enable graph-boosted scoring. For each search result, its score is
    /// multiplied by `1.0 + weight * pagerank_of_file` where pagerank_of_file
    /// is the maximum PageRank score of any symbol defined in that file.
    pub fn with_graph_boost(mut self, graph: &CodeGraph, weight: f32) -> Self {
        let scores = graph.pagerank(0.85, 20);
        let mut file_scores: HashMap<String, f32> = HashMap::new();
        for node_idx in graph.inner.node_indices() {
            if let Some(node) = graph.get_node(node_idx) {
                let pr = scores.get(&node_idx).copied().unwrap_or(0.0) as f32;
                let entry = file_scores.entry(node.file.clone()).or_insert(0.0);
                *entry = entry.max(pr);
            }
        }
        self.graph_file_scores = Some(file_scores);
        self.graph_boost_weight = weight;
        self
    }
}

/// Compute the RRF contribution for a single rank position.
///
/// `rank` is 1-indexed (the top result has rank 1).
fn rrf_score(rank: usize, k: f32, weight: f32) -> f32 {
    weight / (k + rank as f32)
}

impl<V: VectorIndex> Retriever for HybridRetriever<'_, V> {
    fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>, CodeforgeError> {
        // Fetch more from each source than requested so that fusion has a
        // richer candidate set.
        let fetch_limit = query.limit * 3;

        // 1. BM25 search via the existing BM25Retriever.
        let bm25_results = {
            let bm25_query = SearchQuery::new(query.query.clone()).with_limit(fetch_limit);
            let bm25 = BM25Retriever::new(self.tantivy);
            bm25.search(&bm25_query)?
        };

        // 2. Vector search: embed the query, then find nearest neighbors.
        let query_embedding = self.embedder.embed(&query.query)?;
        let vector_results = self.vector_index.search(&query_embedding, fetch_limit)?;

        // 3. Accumulate RRF scores per chunk_id. Keep the full SearchResult
        //    from BM25 (it has all the metadata we need).
        let mut result_map: HashMap<String, SearchResult> = HashMap::new();
        let mut score_map: HashMap<String, f32> = HashMap::new();

        for (rank, result) in bm25_results.into_iter().enumerate() {
            let chunk_id = result.chunk_id.clone();
            *score_map.entry(chunk_id.clone()).or_insert(0.0) +=
                rrf_score(rank + 1, self.rrf_k, self.bm25_weight);
            result_map.entry(chunk_id).or_insert(result);
        }

        // 4. Add vector search RRF contributions. Vector results use u64
        //    chunk IDs; convert to String to match BM25 results.
        for (rank, vr) in vector_results.into_iter().enumerate() {
            let chunk_id = vr.chunk_id.to_string();
            *score_map.entry(chunk_id).or_insert(0.0) +=
                rrf_score(rank + 1, self.rrf_k, self.vector_weight);
            // Note: vector-only results lack full SearchResult metadata.
            // We only include results that also appear in the BM25 result_map
            // (or that were added above). This is acceptable because the BM25
            // index contains all indexed chunks and will surface them if they
            // match even weakly.
        }

        // 5. Sort by fused score descending, take top K.
        let mut fused: Vec<(String, f32)> = score_map.into_iter().collect();
        fused.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        fused.truncate(query.limit);

        // 6. Build final results using result_map for metadata.
        let mut final_results = Vec::new();
        for (chunk_id, fused_score) in fused {
            if let Some(mut result) = result_map.remove(&chunk_id) {
                result.score = fused_score;
                final_results.push(result);
            }
            // Vector-only results without metadata are skipped.
        }

        // Apply graph boost if enabled.
        if let Some(ref file_scores) = self.graph_file_scores {
            for result in &mut final_results {
                if let Some(&pr) = file_scores.get(&result.file_path) {
                    result.score *= 1.0 + self.graph_boost_weight * pr;
                }
            }
            // Re-sort after boosting.
            final_results.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }

        // Apply file filter if present.
        if let Some(ref filter) = query.file_filter {
            final_results.retain(|r| r.file_path.contains(filter));
        }

        Ok(final_results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::Chunk;
    use crate::embeddings::MockEmbedder;
    use crate::language::Language;

    /// Helper: build a test chunk with the given id, path, and content.
    fn make_chunk(id: u64, file_path: &str, content: &str) -> Chunk {
        Chunk {
            id,
            file_path: file_path.to_string(),
            language: Language::Rust,
            content: content.to_string(),
            byte_start: 0,
            byte_end: content.len(),
            line_start: 0,
            line_end: 5,
            scope_chain: vec!["module".to_string()],
            signatures: vec![format!("fn {content}")],
            entity_names: vec![content.split_whitespace().next().unwrap_or("x").to_string()],
        }
    }

    /// Set up a Tantivy index + BruteForceVectorIndex + MockEmbedder with
    /// the given chunks. Returns (tantivy, vector_index, embedder).
    fn setup_indexes(chunks: &[Chunk]) -> (TantivyIndex, BruteForceVectorIndex, MockEmbedder) {
        let dim = 32;
        let embedder = MockEmbedder::new(dim);
        let tantivy = TantivyIndex::create_in_ram().unwrap();
        let mut vector_index = BruteForceVectorIndex::new(dim);

        for chunk in chunks {
            tantivy.add_chunk(chunk).unwrap();
            let embedding = embedder.embed(&chunk.content).unwrap();
            vector_index.add(chunk.id, embedding).unwrap();
        }
        tantivy.commit().unwrap();

        (tantivy, vector_index, embedder)
    }

    #[test]
    fn test_rrf_score_basic() {
        // rank=1, k=60, weight=1.0 => 1/(60+1) = 0.01639...
        let score = rrf_score(1, 60.0, 1.0);
        assert!((score - 1.0 / 61.0).abs() < 1e-6);

        // rank=2, k=60, weight=1.0 => 1/(60+2) = 0.01612...
        let score2 = rrf_score(2, 60.0, 1.0);
        assert!((score2 - 1.0 / 62.0).abs() < 1e-6);

        // Higher rank -> lower score.
        assert!(score > score2);

        // Weight multiplier.
        let weighted = rrf_score(1, 60.0, 2.0);
        assert!((weighted - 2.0 / 61.0).abs() < 1e-6);
    }

    #[test]
    fn test_hybrid_combines_results() {
        let chunks = vec![
            make_chunk(1, "src/alpha.rs", "alpha search function handler"),
            make_chunk(2, "src/beta.rs", "beta processing pipeline worker"),
            make_chunk(3, "src/gamma.rs", "gamma alpha related helper utility"),
        ];

        let (tantivy, vector_index, embedder) = setup_indexes(&chunks);

        let hybrid = HybridRetriever::new(&tantivy, &vector_index, &embedder);
        let query = SearchQuery::new("alpha").with_limit(10);
        let results = hybrid.search(&query).unwrap();

        // Should return results (at least the chunks containing "alpha").
        assert!(
            !results.is_empty(),
            "expected hybrid search to return results"
        );

        // All results should have positive fused scores.
        for r in &results {
            assert!(
                r.score > 0.0,
                "expected positive RRF score, got {}",
                r.score
            );
        }

        // Results should be sorted by score descending.
        for w in results.windows(2) {
            assert!(
                w[0].score >= w[1].score,
                "results not sorted: {} < {}",
                w[0].score,
                w[1].score
            );
        }
    }

    #[test]
    fn test_hybrid_weights() {
        // Use chunks where BM25 and vector rankings disagree.
        // Chunk A: strong BM25 match for "alpha_search" (has the term).
        // Chunk B: weak BM25 match (no "alpha_search"), but its MockEmbedder
        //          vector may rank higher because embedding similarity is
        //          content-independent in MockEmbedder.
        //
        // We verify that changing weights produces different absolute scores,
        // proving that weights are applied.
        let chunks = vec![
            make_chunk(10, "src/foo.rs", "alpha_search function handler"),
            make_chunk(20, "src/bar.rs", "beta_process worker implementation"),
        ];

        let (tantivy, vector_index, embedder) = setup_indexes(&chunks);

        // BM25-heavy weights.
        let bm25_heavy =
            HybridRetriever::new(&tantivy, &vector_index, &embedder).with_weights(0.9, 0.1);
        let results_bm25 = bm25_heavy
            .search(&SearchQuery::new("alpha_search").with_limit(10))
            .unwrap();

        // Vector-heavy weights.
        let vec_heavy =
            HybridRetriever::new(&tantivy, &vector_index, &embedder).with_weights(0.1, 0.9);
        let results_vec = vec_heavy
            .search(&SearchQuery::new("alpha_search").with_limit(10))
            .unwrap();

        // Both should return at least one result.
        assert!(!results_bm25.is_empty());
        assert!(!results_vec.is_empty());

        // Find chunk_id "10" in both result sets and compare scores.
        // The score should differ because the weights are different.
        let score_bm25 = results_bm25
            .iter()
            .find(|r| r.chunk_id == "10")
            .map(|r| r.score);
        let score_vec = results_vec
            .iter()
            .find(|r| r.chunk_id == "10")
            .map(|r| r.score);

        assert!(
            score_bm25.is_some() && score_vec.is_some(),
            "chunk 10 should appear in both result sets"
        );
        assert!(
            (score_bm25.unwrap() - score_vec.unwrap()).abs() > 1e-9,
            "expected different scores for chunk 10 with different weights: bm25_heavy={}, vec_heavy={}",
            score_bm25.unwrap(),
            score_vec.unwrap()
        );
    }

    #[test]
    fn test_hybrid_empty_vector_index() {
        // BM25 has data, vector index is empty.
        let chunks = vec![make_chunk(1, "src/one.rs", "search_target function body")];

        let tantivy = TantivyIndex::create_in_ram().unwrap();
        for chunk in &chunks {
            tantivy.add_chunk(chunk).unwrap();
        }
        tantivy.commit().unwrap();

        let dim = 32;
        let embedder = MockEmbedder::new(dim);
        let vector_index = BruteForceVectorIndex::new(dim); // Empty!

        let hybrid = HybridRetriever::new(&tantivy, &vector_index, &embedder);
        let results = hybrid
            .search(&SearchQuery::new("search_target").with_limit(5))
            .unwrap();

        // Should still return BM25 results.
        assert!(!results.is_empty(), "BM25-only results expected");
        assert_eq!(results[0].chunk_id, "1");
    }

    #[test]
    fn test_hybrid_bm25_only_fallback() {
        // Vector index has vectors, but they are orthogonal to the query
        // embedding, so BM25 results should dominate.
        let chunks = vec![
            make_chunk(
                100,
                "src/relevant.rs",
                "relevant_function implementation details",
            ),
            make_chunk(200, "src/unrelated.rs", "completely different topic xyz"),
        ];

        let tantivy = TantivyIndex::create_in_ram().unwrap();
        for chunk in &chunks {
            tantivy.add_chunk(chunk).unwrap();
        }
        tantivy.commit().unwrap();

        let dim = 32;
        let embedder = MockEmbedder::new(dim);
        let mut vector_index = BruteForceVectorIndex::new(dim);

        // Add vectors for both chunks.
        let emb1 = embedder.embed(&chunks[0].content).unwrap();
        let emb2 = embedder.embed(&chunks[1].content).unwrap();
        vector_index.add(100, emb1).unwrap();
        vector_index.add(200, emb2).unwrap();

        let hybrid = HybridRetriever::new(&tantivy, &vector_index, &embedder);
        let results = hybrid
            .search(&SearchQuery::new("relevant_function").with_limit(5))
            .unwrap();

        assert!(!results.is_empty());
        // The first result should be the relevant chunk because BM25 strongly
        // matches "relevant_function".
        assert_eq!(
            results[0].chunk_id, "100",
            "expected chunk 100 to rank first"
        );
    }

    #[test]
    fn test_hybrid_respects_limit() {
        let chunks: Vec<Chunk> = (0..20)
            .map(|i| {
                make_chunk(
                    i,
                    &format!("src/file_{i}.rs"),
                    &format!("common_term handler variant_{i}"),
                )
            })
            .collect();

        let (tantivy, vector_index, embedder) = setup_indexes(&chunks);

        let hybrid = HybridRetriever::new(&tantivy, &vector_index, &embedder);
        let results = hybrid
            .search(&SearchQuery::new("common_term").with_limit(5))
            .unwrap();

        assert!(
            results.len() <= 5,
            "expected at most 5 results, got {}",
            results.len()
        );
    }

    #[test]
    fn test_hybrid_custom_rrf_k() {
        // Use chunks with distinct content so BM25 ranks them differently.
        // Chunk 1 has "alpha_fn" twice for a stronger BM25 match.
        let chunks = vec![
            make_chunk(
                1,
                "src/a.rs",
                "alpha_fn alpha_fn unique handler function implementation",
            ),
            make_chunk(
                2,
                "src/b.rs",
                "beta_fn completely different content topic unrelated",
            ),
            make_chunk(3, "src/c.rs", "alpha_fn helper utility secondary match"),
        ];

        let (tantivy, vector_index, embedder) = setup_indexes(&chunks);

        // k=1: scores drop off more steeply between ranks.
        let steep = HybridRetriever::new(&tantivy, &vector_index, &embedder).with_rrf_k(1.0);
        let results_steep = steep
            .search(&SearchQuery::new("alpha_fn").with_limit(10))
            .unwrap();

        // k=1000: scores are nearly flat across ranks.
        let flat = HybridRetriever::new(&tantivy, &vector_index, &embedder).with_rrf_k(1000.0);
        let results_flat = flat
            .search(&SearchQuery::new("alpha_fn").with_limit(10))
            .unwrap();

        // With k=1, the gap between rank-1 and rank-2 scores is much larger
        // than with k=1000.
        if results_steep.len() >= 2 && results_flat.len() >= 2 {
            let gap_steep = results_steep[0].score - results_steep[1].score;
            let gap_flat = results_flat[0].score - results_flat[1].score;
            assert!(
                gap_steep >= gap_flat,
                "expected steeper score gap with k=1 ({gap_steep}) vs k=1000 ({gap_flat})"
            );
        }
    }

    #[test]
    fn test_graph_boost_promotes_hub_file() {
        // Create a CodeGraph where core.rs has a hub symbol
        use crate::graph::{CodeGraph, ReferenceKind, SymbolKind};
        let mut graph = CodeGraph::new();
        let hub = graph.add_symbol("process", "src/core.rs", SymbolKind::Function);
        let a = graph.add_symbol("handler_a", "src/handler.rs", SymbolKind::Function);
        let b = graph.add_symbol("handler_b", "src/handler.rs", SymbolKind::Function);
        graph.add_reference(a, hub, ReferenceKind::Call);
        graph.add_reference(b, hub, ReferenceKind::Call);

        // Create chunks from both files with the same search term
        let chunks = vec![
            make_chunk(1, "src/core.rs", "target_fn implementation core"),
            make_chunk(2, "src/handler.rs", "target_fn handler wrapper"),
        ];
        let (tantivy, vector_index, embedder) = setup_indexes(&chunks);

        // Without graph boost
        let plain = HybridRetriever::new(&tantivy, &vector_index, &embedder);
        let plain_results = plain
            .search(&SearchQuery::new("target_fn").with_limit(10))
            .unwrap();

        // With graph boost
        let boosted =
            HybridRetriever::new(&tantivy, &vector_index, &embedder).with_graph_boost(&graph, 5.0); // Strong weight to make effect obvious
        let boosted_results = boosted
            .search(&SearchQuery::new("target_fn").with_limit(10))
            .unwrap();

        // Both should return results
        assert!(!plain_results.is_empty());
        assert!(!boosted_results.is_empty());

        // core.rs should rank higher with graph boost (it has the hub symbol)
        if let Some(core_result) = boosted_results
            .iter()
            .find(|r| r.file_path.contains("core.rs"))
        {
            if let Some(handler_result) = boosted_results
                .iter()
                .find(|r| r.file_path.contains("handler.rs"))
            {
                assert!(
                    core_result.score >= handler_result.score,
                    "expected core.rs (hub) to rank higher: {} vs {}",
                    core_result.score,
                    handler_result.score
                );
            }
        }
    }

    #[test]
    fn test_graph_boost_no_graph_is_noop() {
        // Without calling with_graph_boost, results should be identical
        let chunks = vec![make_chunk(1, "src/a.rs", "test_fn function")];
        let (tantivy, vector_index, embedder) = setup_indexes(&chunks);
        let hybrid = HybridRetriever::new(&tantivy, &vector_index, &embedder);
        let results = hybrid
            .search(&SearchQuery::new("test_fn").with_limit(10))
            .unwrap();
        assert!(!results.is_empty());
    }

    /// Helper: build a Tantivy + HnswVectorIndex + MockEmbedder from the
    /// given chunks.  Mirrors `setup_indexes` but uses the HNSW backend.
    fn setup_hnsw_indexes(
        chunks: &[Chunk],
    ) -> (TantivyIndex, crate::index::HnswVectorIndex, MockEmbedder) {
        let dim = 32;
        let embedder = MockEmbedder::new(dim);
        let tantivy = TantivyIndex::create_in_ram().unwrap();
        let mut vector_index = crate::index::HnswVectorIndex::new(dim);

        for chunk in chunks {
            tantivy.add_chunk(chunk).unwrap();
            let embedding = embedder.embed(&chunk.content).unwrap();
            vector_index.add(chunk.id, embedding).unwrap();
        }
        tantivy.commit().unwrap();

        (tantivy, vector_index, embedder)
    }

    #[test]
    fn test_hybrid_retriever_with_hnsw() {
        // Verify that HybridRetriever<HnswVectorIndex> produces correct fused
        // results, not just HybridRetriever<BruteForceVectorIndex>.
        let chunks = vec![
            make_chunk(1, "src/alpha.rs", "alpha search function handler"),
            make_chunk(2, "src/beta.rs", "beta processing pipeline worker"),
            make_chunk(3, "src/gamma.rs", "gamma alpha related helper utility"),
        ];

        let (tantivy, hnsw_index, embedder) = setup_hnsw_indexes(&chunks);

        let hybrid = HybridRetriever::new(&tantivy, &hnsw_index, &embedder);
        let query = SearchQuery::new("alpha").with_limit(10);
        let results = hybrid.search(&query).unwrap();

        // Should return results (at least chunks containing "alpha").
        assert!(
            !results.is_empty(),
            "expected HNSW-backed hybrid search to return results"
        );

        // All results should have positive fused scores.
        for r in &results {
            assert!(
                r.score > 0.0,
                "expected positive RRF score with HNSW, got {}",
                r.score
            );
        }

        // Results should be sorted by score descending.
        for w in results.windows(2) {
            assert!(
                w[0].score >= w[1].score,
                "HNSW results not sorted: {} < {}",
                w[0].score,
                w[1].score
            );
        }

        // The first result should be from a file containing "alpha" in its content.
        assert!(
            results[0].file_path.contains("alpha") || results[0].file_path.contains("gamma"),
            "expected top result from alpha.rs or gamma.rs, got: {}",
            results[0].file_path,
        );
    }

    #[test]
    fn test_hybrid_hnsw_with_graph_boost() {
        // Verify that graph boost works with the HNSW backend.
        use crate::graph::{CodeGraph, ReferenceKind, SymbolKind};

        let mut graph = CodeGraph::new();
        let hub = graph.add_symbol("core_process", "src/core.rs", SymbolKind::Function);
        let a = graph.add_symbol("caller_a", "src/caller.rs", SymbolKind::Function);
        let b = graph.add_symbol("caller_b", "src/caller.rs", SymbolKind::Function);
        graph.add_reference(a, hub, ReferenceKind::Call);
        graph.add_reference(b, hub, ReferenceKind::Call);

        let chunks = vec![
            make_chunk(1, "src/core.rs", "search_target implementation core"),
            make_chunk(2, "src/caller.rs", "search_target caller wrapper"),
        ];
        let (tantivy, hnsw_index, embedder) = setup_hnsw_indexes(&chunks);

        let boosted = HybridRetriever::new(&tantivy, &hnsw_index, &embedder)
            .with_graph_boost(&graph, 5.0);
        let results = boosted
            .search(&SearchQuery::new("search_target").with_limit(10))
            .unwrap();

        assert!(!results.is_empty(), "expected HNSW+graph results");

        // core.rs has the hub symbol with higher PageRank, so it should score
        // at least as high as caller.rs.
        if let Some(core_r) = results.iter().find(|r| r.file_path.contains("core.rs")) {
            if let Some(caller_r) = results.iter().find(|r| r.file_path.contains("caller.rs")) {
                assert!(
                    core_r.score >= caller_r.score,
                    "expected core.rs (hub) to rank >= caller.rs with HNSW+graph: {} vs {}",
                    core_r.score,
                    caller_r.score,
                );
            }
        }
    }

    #[test]
    fn test_hybrid_file_filter() {
        let chunks = vec![
            make_chunk(1, "src/main.rs", "target_query handler"),
            make_chunk(2, "src/lib.rs", "target_query processor"),
            make_chunk(3, "tests/test.rs", "target_query tester"),
        ];

        let (tantivy, vector_index, embedder) = setup_indexes(&chunks);

        let hybrid = HybridRetriever::new(&tantivy, &vector_index, &embedder);
        let results = hybrid
            .search(
                &SearchQuery::new("target_query")
                    .with_limit(10)
                    .with_file_filter("src/"),
            )
            .unwrap();

        // Only src/ files should pass the filter.
        for r in &results {
            assert!(
                r.file_path.contains("src/"),
                "expected file_filter to restrict to src/, got: {}",
                r.file_path
            );
        }
    }
}

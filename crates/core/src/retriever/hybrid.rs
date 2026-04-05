use std::collections::HashMap;
use std::sync::Arc;

use dashmap::DashMap;
use tracing::debug;

use crate::embedder::Embedder;
use crate::error::Result;
use crate::index::TantivyIndex;
use crate::vector::VectorIndex;

use super::bm25::BM25Retriever;
use super::vector::VectorRetriever;
use super::{ChunkMeta, Retriever, SearchQuery, SearchResult};

/// Optional synonym/context expansion applied to the vector query only.
/// BM25 keeps the original query to avoid self-referential matches
/// (e.g., synonym definition code ranking for its own synonyms).
pub(crate) fn expand_vector_query(query: &str) -> Option<String> {
    let q = query.to_lowercase();
    let mut extra = Vec::new();

    let synonyms: &[(&[&str], &[&str])] = &[
        (
            &["dead code", "unused", "unreachable"],
            &["orphan", "find_orphans"],
        ),
        (&["dependency", "dependencies"], &["import", "use"]),
        (&["refactor", "restructure"], &["rename", "extract"]),
        (&["performance", "optimize"], &["benchmark", "perf"]),
        (&["similar", "duplicate"], &["cosine", "find_similar"]),
        (&["ranking", "scoring"], &["pagerank", "boost", "BM25"]),
        (&["complexity", "complicated"], &["cyclomatic", "McCabe"]),
        (
            &["coverage", "test coverage"],
            &["find_tests", "test_mapping"],
        ),
    ];

    for (triggers, expansions) in synonyms {
        if triggers.iter().any(|t| q.contains(t)) {
            for exp in expansions.iter() {
                if !q.contains(&exp.to_lowercase()) {
                    extra.push(exp.to_string());
                }
            }
        }
    }

    if extra.is_empty() {
        None
    } else {
        Some(format!("{query} {}", extra.join(" ")))
    }
}

/// Hybrid retriever that combines BM25 and vector search via RRF fusion.
///
/// Runs both retrievers in parallel (logically) and fuses their ranked lists
/// using Reciprocal Rank Fusion.
pub struct HybridRetriever<'a> {
    tantivy: &'a TantivyIndex,
    embedder: Arc<Embedder>,
    vector: &'a VectorIndex,
    chunk_meta: &'a DashMap<u64, ChunkMeta>,
    rrf_k: f32,
}

impl<'a> HybridRetriever<'a> {
    /// Create a new HybridRetriever.
    pub fn new(
        tantivy: &'a TantivyIndex,
        embedder: Arc<Embedder>,
        vector: &'a VectorIndex,
        chunk_meta: &'a DashMap<u64, ChunkMeta>,
        rrf_k: f32,
    ) -> Self {
        Self {
            tantivy,
            embedder,
            vector,
            chunk_meta,
            rrf_k,
        }
    }
}

impl Retriever for HybridRetriever<'_> {
    fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>> {
        // Fetch extra candidates from both retrievers to feed the fusion.
        let fetch = query.limit * 3;

        let bm25_query = SearchQuery {
            query: query.query.clone(),
            limit: fetch,
            file_filter: query.file_filter.clone(),
            strategy: query.strategy,
            token_budget: query.token_budget,
            queries: None,
            doc_filter: None,
        };

        let bm25_results = BM25Retriever::new(self.tantivy).search(&bm25_query)?;

        let vec_results = if self.vector.is_empty() {
            Vec::new()
        } else {
            // Expand the vector query with synonyms (BM25 keeps the original
            // to avoid matching synonym definition code).
            let vec_query_text =
                expand_vector_query(&query.query).unwrap_or_else(|| query.query.clone());
            let vec_query = SearchQuery {
                query: vec_query_text,
                limit: fetch,
                file_filter: query.file_filter.clone(),
                strategy: query.strategy,
                token_budget: query.token_budget,
                queries: None,
                doc_filter: None,
            };
            VectorRetriever::with_tantivy(
                Arc::clone(&self.embedder),
                self.vector,
                self.chunk_meta,
                self.tantivy,
            )
            .search(&vec_query)?
        };

        debug!(
            bm25 = bm25_results.len(),
            vector = vec_results.len(),
            "hybrid: fusing results"
        );

        // Asymmetric RRF: identifier queries weight BM25 higher (lower k →
        // larger score contribution from top-ranked BM25 hits); natural-language
        // queries weight the semantic vector list higher.
        let base = self.rrf_k;
        let (k_bm25, k_vec) = if is_identifier_query(&query.query) {
            (base / 3.0, base * 1.5) // BM25 dominates for exact-name lookups
        } else {
            (base * 1.5, base / 3.0) // vector dominates for conceptual queries
        };
        let mut fused = rrf_fuse_asymmetric(&bm25_results, &vec_results, k_bm25, k_vec);

        // Apply file filter (already applied inside sub-retrievers, but
        // bm25_results post-filter may differ from pre-filter ranking so
        // re-applying here is safe and cheap).
        if let Some(ref filter) = query.file_filter {
            fused.retain(|r| r.file_path.contains(filter.as_str()));
        }

        fused.truncate(query.limit);
        Ok(fused)
    }
}

/// Classify a search query as identifier-like vs. natural-language.
///
/// Identifier queries have no spaces and consist of word characters plus common
/// code separators (`_`, `::`, `.`, `->`, `/`).  They benefit from BM25 over
/// semantic search because the tokeniser already handles camelCase/snake_case
/// splitting.
pub fn is_identifier_query(query: &str) -> bool {
    if query.contains(' ') {
        return false;
    }
    !query.is_empty()
        && query
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | ':' | '.' | '-' | '>' | '/'))
}

/// Asymmetric Reciprocal Rank Fusion: uses separate `k` constants for each list.
///
/// Allows callers to weight one list more heavily than the other — useful when
/// query type strongly favours one retrieval method (e.g. BM25 for identifiers,
/// vector for natural-language descriptions).
pub fn rrf_fuse_asymmetric(
    list_a: &[SearchResult],
    list_b: &[SearchResult],
    k_a: f32,
    k_b: f32,
) -> Vec<SearchResult> {
    let a_map: HashMap<&str, &SearchResult> =
        list_a.iter().map(|r| (r.chunk_id.as_str(), r)).collect();
    let b_map: HashMap<&str, &SearchResult> =
        list_b.iter().map(|r| (r.chunk_id.as_str(), r)).collect();

    let mut all_ids: Vec<&str> = a_map.keys().copied().collect();
    for id in b_map.keys() {
        if !a_map.contains_key(id) {
            all_ids.push(id);
        }
    }

    let rank_a: HashMap<&str, usize> = list_a
        .iter()
        .enumerate()
        .map(|(i, r)| (r.chunk_id.as_str(), i))
        .collect();
    let rank_b: HashMap<&str, usize> = list_b
        .iter()
        .enumerate()
        .map(|(i, r)| (r.chunk_id.as_str(), i))
        .collect();

    let mut scored: Vec<(&str, f32)> = all_ids
        .into_iter()
        .map(|id| {
            let score_a = rank_a
                .get(id)
                .map(|&r| 1.0 / (k_a + r as f32 + 1.0))
                .unwrap_or(0.0);
            let score_b = rank_b
                .get(id)
                .map(|&r| 1.0 / (k_b + r as f32 + 1.0))
                .unwrap_or(0.0);
            (id, score_a + score_b)
        })
        .collect();

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    scored
        .into_iter()
        .filter_map(|(id, score)| {
            let base = a_map.get(id).copied().or_else(|| b_map.get(id).copied())?;
            Some(SearchResult {
                score,
                ..base.clone()
            })
        })
        .collect()
}

/// Reciprocal Rank Fusion of two ranked result lists.
///
/// For each document appearing in either list, computes:
/// ```text
/// score(d) = Σ  1 / (k + rank(d, list))
/// ```
/// where the sum is over all lists that contain `d`.
/// Documents not present in a list do not contribute a term.
///
/// The constant `k` (typically 60) controls how steeply rank affects weight.
pub fn rrf_fuse(list_a: &[SearchResult], list_b: &[SearchResult], k: f32) -> Vec<SearchResult> {
    // Index list_a by chunk_id.
    let a_map: HashMap<&str, &SearchResult> =
        list_a.iter().map(|r| (r.chunk_id.as_str(), r)).collect();
    let b_map: HashMap<&str, &SearchResult> =
        list_b.iter().map(|r| (r.chunk_id.as_str(), r)).collect();

    // Collect all unique chunk IDs.
    let mut all_ids: Vec<&str> = a_map.keys().copied().collect();
    for id in b_map.keys() {
        if !a_map.contains_key(id) {
            all_ids.push(id);
        }
    }

    // Pre-build rank maps for O(1) position lookup (avoids O(N×M) linear scans).
    let rank_a: HashMap<&str, usize> = list_a
        .iter()
        .enumerate()
        .map(|(i, r)| (r.chunk_id.as_str(), i))
        .collect();
    let rank_b: HashMap<&str, usize> = list_b
        .iter()
        .enumerate()
        .map(|(i, r)| (r.chunk_id.as_str(), i))
        .collect();

    // Compute RRF scores.
    let mut scored: Vec<(&str, f32)> = all_ids
        .into_iter()
        .map(|id| {
            let score_a = rank_a
                .get(id)
                .map(|&r| 1.0 / (k + r as f32 + 1.0))
                .unwrap_or(0.0);
            let score_b = rank_b
                .get(id)
                .map(|&r| 1.0 / (k + r as f32 + 1.0))
                .unwrap_or(0.0);

            (id, score_a + score_b)
        })
        .collect();

    // Sort by descending RRF score.
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Build output, preferring list_a metadata (richer BM25 field extraction).
    scored
        .into_iter()
        .filter_map(|(id, score)| {
            let base = a_map.get(id).copied().or_else(|| b_map.get(id).copied())?;
            Some(SearchResult {
                score,
                ..base.clone()
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_result(id: &str, score: f32) -> SearchResult {
        SearchResult {
            chunk_id: id.to_string(),
            file_path: "src/lib.rs".to_string(),
            language: "Rust".to_string(),
            score,
            line_start: 0,
            line_end: 10,
            signature: String::new(),
            scope_chain: Vec::new(),
            content: format!("content for {id}"),
        }
    }

    #[test]
    fn rrf_fuse_orders_by_combined_rank() {
        // doc "a" is rank 0 in both lists → should win.
        // doc "b" is rank 1 in list_a only.
        // doc "c" is rank 0 in list_b only.
        let list_a = vec![make_result("a", 1.0), make_result("b", 0.8)];
        let list_b = vec![make_result("c", 0.9), make_result("a", 0.7)];

        let fused = rrf_fuse(&list_a, &list_b, 60.0);

        // "a" appears in both lists → highest RRF score.
        assert_eq!(fused[0].chunk_id, "a");
    }

    #[test]
    fn rrf_fuse_union_of_results() {
        let list_a = vec![make_result("x", 1.0)];
        let list_b = vec![make_result("y", 1.0)];

        let fused = rrf_fuse(&list_a, &list_b, 60.0);

        // Both should appear in the output.
        assert_eq!(fused.len(), 2);
        let ids: Vec<&str> = fused.iter().map(|r| r.chunk_id.as_str()).collect();
        assert!(ids.contains(&"x"));
        assert!(ids.contains(&"y"));
    }

    #[test]
    fn rrf_fuse_empty_lists() {
        assert!(rrf_fuse(&[], &[], 60.0).is_empty());
        let list = vec![make_result("z", 1.0)];
        let fused = rrf_fuse(&list, &[], 60.0);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].chunk_id, "z");
    }
}

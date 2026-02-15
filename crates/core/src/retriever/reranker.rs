//! Reranking stage for refining search results with a cross-encoder model.
//!
//! After initial retrieval (BM25, vector, or hybrid RRF fusion), a reranker
//! scores each (query, document) pair using a cross-encoder and returns the
//! top-k results in refined order. This is a standard two-stage retrieval
//! pattern: cheap first-stage recall followed by expensive but precise
//! reranking.

use crate::error::CodeforgeError;
use crate::retriever::SearchResult;

/// Trait for reranking search results using a cross-encoder or similar model.
///
/// Implementations receive a query, a set of candidate results from the
/// first-stage retriever, and a `top_k` cutoff. They must score each
/// (query, document) pair and return the top-k results sorted by relevance.
pub trait Reranker: Send + Sync {
    /// Score each (query, document) pair and return top-k reranked results.
    ///
    /// The returned results must be sorted by score in descending order and
    /// contain at most `top_k` items.
    fn rerank(
        &self,
        query: &str,
        results: &[SearchResult],
        top_k: usize,
    ) -> Result<Vec<SearchResult>, CodeforgeError>;
}

/// A mock reranker that assigns predetermined scores to results.
///
/// Useful for testing the reranking pipeline without an external model.
/// Scores are cycled if there are more results than scores provided.
pub struct MockReranker {
    scores: Vec<f64>,
}

impl MockReranker {
    /// Create a new mock reranker with the given scores.
    ///
    /// Scores are assigned to results in order, cycling if there are more
    /// results than scores.
    pub fn new(scores: Vec<f64>) -> Self {
        Self { scores }
    }
}

impl Reranker for MockReranker {
    fn rerank(
        &self,
        _query: &str,
        results: &[SearchResult],
        top_k: usize,
    ) -> Result<Vec<SearchResult>, CodeforgeError> {
        let mut scored: Vec<SearchResult> = results
            .iter()
            .zip(self.scores.iter().cycle())
            .map(|(r, &s)| {
                let mut r = r.clone();
                r.score = s as f32;
                r
            })
            .collect();
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(top_k);
        Ok(scored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a SearchResult with the given chunk_id and score.
    fn make_result(chunk_id: &str, score: f32) -> SearchResult {
        SearchResult {
            chunk_id: chunk_id.to_string(),
            file_path: format!("src/{chunk_id}.rs"),
            language: "rust".to_string(),
            score,
            line_start: 0,
            line_end: 10,
            signature: String::new(),
            content: format!("content of {chunk_id}"),
        }
    }

    #[test]
    fn mock_reranker_reorders_results() {
        let reranker = MockReranker::new(vec![0.9, 0.1, 0.5]);
        let results = vec![
            make_result("a", 1.0),
            make_result("b", 1.0),
            make_result("c", 1.0),
        ];

        let reranked = reranker.rerank("query", &results, 10).unwrap();

        assert_eq!(reranked.len(), 3);
        // Score 0.9 -> "a", 0.5 -> "c", 0.1 -> "b"
        assert_eq!(reranked[0].chunk_id, "a");
        assert_eq!(reranked[1].chunk_id, "c");
        assert_eq!(reranked[2].chunk_id, "b");

        // Verify scores were replaced.
        assert!((reranked[0].score - 0.9).abs() < 1e-6);
        assert!((reranked[1].score - 0.5).abs() < 1e-6);
        assert!((reranked[2].score - 0.1).abs() < 1e-6);
    }

    #[test]
    fn mock_reranker_respects_top_k() {
        let reranker = MockReranker::new(vec![0.9, 0.1, 0.5]);
        let results = vec![
            make_result("a", 1.0),
            make_result("b", 1.0),
            make_result("c", 1.0),
        ];

        let reranked = reranker.rerank("query", &results, 2).unwrap();

        assert_eq!(reranked.len(), 2);
        // Only the top 2 by score: "a" (0.9) and "c" (0.5).
        assert_eq!(reranked[0].chunk_id, "a");
        assert_eq!(reranked[1].chunk_id, "c");
    }

    #[test]
    fn mock_reranker_cycles_scores() {
        // Two scores for three results: scores cycle as [0.8, 0.2, 0.8].
        let reranker = MockReranker::new(vec![0.8, 0.2]);
        let results = vec![
            make_result("x", 0.0),
            make_result("y", 0.0),
            make_result("z", 0.0),
        ];

        let reranked = reranker.rerank("query", &results, 10).unwrap();

        assert_eq!(reranked.len(), 3);
        // "x" gets 0.8, "y" gets 0.2, "z" gets 0.8 (cycled).
        // After sort: "x" (0.8), "z" (0.8), "y" (0.2) — stable order for ties.
        assert!((reranked[0].score - 0.8).abs() < 1e-6);
        assert!((reranked[1].score - 0.8).abs() < 1e-6);
        assert!((reranked[2].score - 0.2).abs() < 1e-6);
    }

    #[test]
    fn mock_reranker_empty_results() {
        let reranker = MockReranker::new(vec![0.5]);
        let reranked = reranker.rerank("query", &[], 10).unwrap();
        assert!(reranked.is_empty());
    }
}

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use dashmap::DashMap;

use crate::embedder::Embedder;
use crate::error::Result;
use crate::index::TantivyIndex;
use crate::vector::VectorIndex;
use tracing::warn;

use super::{ChunkMeta, Retriever, SearchQuery, SearchResult};

/// Vector-based retriever using HNSW approximate nearest-neighbour search.
///
/// Embeds the query string, finds the nearest chunk vectors, then hydrates
/// each result from the `chunk_meta` table. When `chunk_meta.content` is
/// empty (compact persistence mode), content is fetched from Tantivy stored
/// fields via the optional `tantivy` reference.
pub struct VectorRetriever<'a> {
    embedder: Arc<Embedder>,
    vector: &'a VectorIndex,
    chunk_meta: &'a DashMap<u64, ChunkMeta>,
    tantivy: Option<&'a TantivyIndex>,
}

impl<'a> VectorRetriever<'a> {
    /// Create a new VectorRetriever.
    pub fn new(
        embedder: Arc<Embedder>,
        vector: &'a VectorIndex,
        chunk_meta: &'a DashMap<u64, ChunkMeta>,
    ) -> Self {
        Self {
            embedder,
            vector,
            chunk_meta,
            tantivy: None,
        }
    }

    /// Create a VectorRetriever with a Tantivy fallback for content retrieval.
    pub fn with_tantivy(
        embedder: Arc<Embedder>,
        vector: &'a VectorIndex,
        chunk_meta: &'a DashMap<u64, ChunkMeta>,
        tantivy: &'a TantivyIndex,
    ) -> Self {
        Self {
            embedder,
            vector,
            chunk_meta,
            tantivy: Some(tantivy),
        }
    }

    /// Hydrate compact metadata content in one indexed Tantivy query.
    fn hydrate_contents(&self, chunk_ids: &HashSet<u64>) -> HashMap<u64, String> {
        if chunk_ids.is_empty() {
            return HashMap::new();
        }
        let Some(tantivy) = self.tantivy else {
            return HashMap::new();
        };
        tantivy
            .lookup_chunk_contents(chunk_ids)
            .unwrap_or_else(|e| {
                warn!(
                    count = chunk_ids.len(),
                    error = %e,
                    "failed to batch-hydrate vector hit content"
                );
                HashMap::new()
            })
    }
}

impl Retriever for VectorRetriever<'_> {
    fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>> {
        if self.vector.is_empty() {
            return Ok(Vec::new());
        }

        // Embed the query (with model-specific instruction prefix for BGE).
        let query_vec = self.embedder.embed_query(&query.query)?;

        // Find nearest neighbours (fetch more than needed to allow file filtering).
        let fetch_limit = if query.file_filter.is_some() {
            query.limit * 4
        } else {
            query.limit
        };

        let matches = self.vector.search(&query_vec, fetch_limit)?;

        let missing_content_ids: HashSet<u64> = matches
            .iter()
            .filter_map(|(chunk_id, _)| {
                let meta = self.chunk_meta.get(chunk_id)?;
                if let Some(ref filter) = query.file_filter
                    && !meta.file_path.contains(filter.as_str())
                {
                    return None;
                }
                meta.content.is_empty().then_some(*chunk_id)
            })
            .collect();
        let hydrated_contents = self.hydrate_contents(&missing_content_ids);

        let mut results = Vec::with_capacity(matches.len());
        for (chunk_id, distance) in matches {
            // Hydrate from the chunk_meta table.
            let Some(meta) = self.chunk_meta.get(&chunk_id) else {
                continue;
            };

            // Convert cosine distance to a similarity score (higher = better).
            let score = 1.0 - distance;

            let content = if meta.content.is_empty() {
                hydrated_contents.get(&chunk_id).cloned()
            } else {
                Some(meta.content.clone())
            };
            let Some(content) = content else {
                warn!(chunk_id, "dropping vector hit with empty content");
                continue;
            };

            results.push(SearchResult {
                chunk_id: chunk_id.to_string(),
                file_path: meta.file_path.clone(),
                language: meta.language.clone(),
                score,
                line_start: meta.line_start,
                line_end: meta.line_end,
                signature: meta.signature.clone(),
                scope_chain: meta.scope_chain.clone(),
                content,
            });
        }

        // Apply file filter.
        if let Some(ref filter) = query.file_filter {
            results.retain(|r| r.file_path.contains(filter.as_str()));
        }

        // Truncate to requested limit.
        results.truncate(query.limit);

        Ok(results)
    }
}

/// Return the query embedding for a string (convenience helper used by hybrid).
pub fn embed_query(embedder: &Embedder, query: &str) -> Result<Vec<f32>> {
    embedder.embed_query(query)
}

/// Compute cosine similarity between two equal-length float vectors.
///
/// Returns a value in `[0.0, 1.0]` where 1.0 means identical direction.
/// Returns 0.0 if either vector is zero-length.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "vector dimension mismatch");
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a < f32::EPSILON || norm_b < f32::EPSILON {
        0.0
    } else {
        (dot / (norm_a * norm_b)).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_similarity_identical() {
        let v = vec![1.0f32, 0.0, 0.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_orthogonal() {
        let a = vec![1.0f32, 0.0, 0.0];
        let b = vec![0.0f32, 1.0, 0.0];
        assert!(cosine_similarity(&a, &b) < 1e-6);
    }

    #[test]
    fn cosine_similarity_zero_vector() {
        let a = vec![0.0f32, 0.0, 0.0];
        let b = vec![1.0f32, 0.0, 0.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }
}

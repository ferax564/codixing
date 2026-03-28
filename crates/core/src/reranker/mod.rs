//! Cross-encoder reranker backed by fastembed's BGE-Reranker-Base ONNX model.
//!
//! The reranker scores `(query, document)` pairs jointly — unlike bi-encoder
//! embeddings which encode each side independently, this reads both together
//! and produces a calibrated relevance score.  It is slower (~50 ms for 30
//! documents) but significantly more precise, making it ideal for a second-pass
//! refinement of BM25 / vector candidates.

use std::sync::Mutex;

use fastembed::{RerankInitOptions, RerankerModel, TextRerank};
use tracing::info;

use crate::error::{CodixingError, Result};
use crate::retriever::SearchResult;
use crate::retriever::reranker::Reranker as RerankerTrait;

/// Wrapper around a fastembed [`TextRerank`] cross-encoder model.
///
/// `TextRerank::rerank` requires `&mut self`, so the model is held behind a
/// `Mutex` to allow sharing via `Arc<Reranker>`.
pub struct Reranker {
    model: Mutex<TextRerank>,
}

impl Reranker {
    /// Load the BGE-Reranker-Base model (~270 MB ONNX download on first use).
    pub fn new() -> Result<Self> {
        info!("loading reranker model (BGERerankerBase)");
        let model = TextRerank::try_new(
            RerankInitOptions::new(RerankerModel::BGERerankerBase)
                .with_show_download_progress(false),
        )
        .map_err(|e| CodixingError::Reranker(format!("failed to load model: {e}")))?;

        Ok(Self {
            model: Mutex::new(model),
        })
    }

    /// Score each `(query, doc)` pair and return `(original_index, score)`
    /// sorted by descending relevance score.
    ///
    /// The caller is responsible for mapping indices back to the original
    /// candidate list.
    pub fn rerank(&self, query: &str, docs: &[String]) -> Result<Vec<(usize, f32)>> {
        if docs.is_empty() {
            return Ok(Vec::new());
        }

        let doc_refs: Vec<&str> = docs.iter().map(String::as_str).collect();

        let mut model = self
            .model
            .lock()
            .map_err(|_| CodixingError::Reranker("model lock poisoned".to_string()))?;

        let results = model
            .rerank(query, doc_refs.as_slice(), false, None)
            .map_err(|e| CodixingError::Reranker(format!("rerank failed: {e}")))?;

        let mut scored: Vec<(usize, f32)> =
            results.into_iter().map(|r| (r.index, r.score)).collect();
        // Sort by score descending — highest relevance first.
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(scored)
    }
}

impl RerankerTrait for Reranker {
    fn rerank(
        &self,
        query: &str,
        results: &[SearchResult],
        top_k: usize,
    ) -> std::result::Result<Vec<SearchResult>, CodixingError> {
        if results.is_empty() {
            return Ok(Vec::new());
        }

        let docs: Vec<String> = results.iter().map(|r| r.content.clone()).collect();
        // Use the inherent method (fully qualified) to avoid infinite recursion.
        let ranked = Reranker::rerank(self, query, &docs)?;

        let mut reranked: Vec<SearchResult> = ranked
            .into_iter()
            .map(|(idx, score)| {
                let mut r = results[idx].clone();
                r.score = score;
                r
            })
            .collect();

        reranked.truncate(top_k);
        Ok(reranked)
    }
}

use std::sync::Mutex;

use fastembed::{EmbeddingModel as FastEmbedModel, InitOptions, TextEmbedding};
use tracing::info;

use crate::config::EmbeddingModel;
use crate::error::{CodeforgeError, Result};

/// Number of dimensions for BGE Small EN v1.5.
pub const BGE_SMALL_EN_DIMS: usize = 384;

/// Number of dimensions for BGE Base EN v1.5.
pub const BGE_BASE_EN_DIMS: usize = 768;

/// Number of dimensions for Jina Embeddings v2 Base Code.
pub const JINA_EMBED_CODE_DIMS: usize = 768;

/// Wrapper around a fastembed [`TextEmbedding`] model.
///
/// `TextEmbedding::embed` requires `&mut self`, so the model is wrapped in a
/// `Mutex` to allow sharing via `Arc<Embedder>` while serialising embed calls.
/// Embedding is CPU-bound so contention is low in practice.
pub struct Embedder {
    model: Mutex<TextEmbedding>,
    /// Embedding vector dimensions (depends on model choice).
    pub dims: usize,
}

impl Embedder {
    /// Load the embedding model specified by `model_cfg`.
    ///
    /// On first use this downloads the ONNX weights (~25 MB for BGE Small EN).
    /// Subsequent calls use the on-disk cache.
    pub fn new(model_cfg: &EmbeddingModel) -> Result<Self> {
        let (fastembed_model, dims) = match model_cfg {
            EmbeddingModel::BgeSmallEn => (FastEmbedModel::BGESmallENV15, BGE_SMALL_EN_DIMS),
            EmbeddingModel::BgeBaseEn => (FastEmbedModel::BGEBaseENV15, BGE_BASE_EN_DIMS),
            EmbeddingModel::JinaEmbedCode => {
                (FastEmbedModel::JinaEmbeddingsV2BaseCode, JINA_EMBED_CODE_DIMS)
            }
        };

        info!(?fastembed_model, dims, "loading embedding model");

        let model = TextEmbedding::try_new(
            InitOptions::new(fastembed_model).with_show_download_progress(false),
        )
        .map_err(|e| CodeforgeError::Embedding(format!("failed to load model: {e}")))?;

        Ok(Self {
            model: Mutex::new(model),
            dims,
        })
    }

    /// Embed a batch of text strings.
    ///
    /// Returns one embedding vector per input string, in the same order.
    pub fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
        let mut model = self
            .model
            .lock()
            .map_err(|e| CodeforgeError::Embedding(format!("model lock poisoned: {e}")))?;
        model
            .embed(refs, None)
            .map_err(|e| CodeforgeError::Embedding(format!("embed failed: {e}")))
    }

    /// Embed a single text string.
    pub fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let mut model = self
            .model
            .lock()
            .map_err(|e| CodeforgeError::Embedding(format!("model lock poisoned: {e}")))?;
        let mut results = model
            .embed(vec![text], None)
            .map_err(|e| CodeforgeError::Embedding(format!("embed failed: {e}")))?;
        results
            .pop()
            .ok_or_else(|| CodeforgeError::Embedding("empty embedding result".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: this test requires network access to download the model on first run.
    // It is marked `#[ignore]` to avoid slowing CI.
    #[test]
    #[ignore]
    fn embed_produces_correct_dims() {
        let embedder = Embedder::new(&EmbeddingModel::BgeSmallEn).unwrap();
        assert_eq!(embedder.dims, BGE_SMALL_EN_DIMS);

        let vecs = embedder
            .embed(vec!["hello world".to_string(), "foo bar".to_string()])
            .unwrap();
        assert_eq!(vecs.len(), 2);
        assert_eq!(vecs[0].len(), BGE_SMALL_EN_DIMS);
    }
}

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

/// Number of dimensions for Qwen3-Embedding-0.6B (hidden_size from model config).
#[cfg(feature = "qwen3")]
pub const QWEN3_SMALL_DIMS: usize = 1024;

/// Hugging Face repo ID for the Qwen3 0.6B embedding checkpoint.
#[cfg(feature = "qwen3")]
const QWEN3_SMALL_REPO: &str = "Qwen/Qwen3-Embedding-0.6B";

/// Maximum token sequence length passed to the Qwen3 tokenizer.
/// The model supports up to 32k but 512 is enough for code chunks and keeps
/// memory + latency manageable on CPU.
#[cfg(feature = "qwen3")]
const QWEN3_MAX_LENGTH: usize = 512;

/// Backing inference engine.
///
/// ONNX Runtime (`TextEmbedding`) and the candle-based `Qwen3TextEmbedding`
/// have incompatible APIs so we abstract the difference here.
enum EmbedBackend {
    /// ONNX Runtime backend (fastembed default — BGE/Jina models).
    Onnx(Mutex<TextEmbedding>),
    /// Candle backend — Qwen3 text embedding models.
    #[cfg(feature = "qwen3")]
    Qwen3(Mutex<fastembed::Qwen3TextEmbedding>),
}

/// Wrapper around a fastembed embedding model.
///
/// Abstracts over the ONNX (BGE / Jina) and candle (Qwen3) backends so that
/// callers always see the same `.embed()` / `.embed_one()` interface.
///
/// The model is wrapped in a `Mutex` because fastembed's `embed` requires
/// `&mut self`.  Embedding is CPU-bound so mutex contention is negligible.
pub struct Embedder {
    backend: EmbedBackend,
    /// Embedding vector dimensions (model-dependent).
    pub dims: usize,
}

impl Embedder {
    /// Load the embedding model specified by `model_cfg`.
    ///
    /// For ONNX-backed models (BGE, Jina) the ONNX weights are downloaded once
    /// from the fastembed CDN and cached on disk.
    ///
    /// For the Qwen3 candle backend the weights (~1.2 GB) are downloaded from
    /// Hugging Face on first use and cached by `hf-hub`.
    pub fn new(model_cfg: &EmbeddingModel) -> Result<Self> {
        match model_cfg {
            // ── ONNX backend (BGE, Jina) ──────────────────────────────────────
            EmbeddingModel::BgeSmallEn
            | EmbeddingModel::BgeBaseEn
            | EmbeddingModel::JinaEmbedCode => Self::new_onnx(model_cfg),

            // ── Candle backend (Qwen3) ────────────────────────────────────────
            #[cfg(feature = "qwen3")]
            EmbeddingModel::Qwen3SmallEmbedding => Self::new_qwen3(),
        }
    }

    /// Construct an ONNX-backed embedder.
    fn new_onnx(model_cfg: &EmbeddingModel) -> Result<Self> {
        let (fastembed_model, dims) = match model_cfg {
            EmbeddingModel::BgeSmallEn => (FastEmbedModel::BGESmallENV15, BGE_SMALL_EN_DIMS),
            EmbeddingModel::BgeBaseEn => (FastEmbedModel::BGEBaseENV15, BGE_BASE_EN_DIMS),
            EmbeddingModel::JinaEmbedCode => (
                FastEmbedModel::JinaEmbeddingsV2BaseCode,
                JINA_EMBED_CODE_DIMS,
            ),
            // This arm is unreachable because `new()` only routes ONNX models
            // here, but the compiler needs exhaustiveness.
            #[cfg(feature = "qwen3")]
            EmbeddingModel::Qwen3SmallEmbedding => unreachable!(),
        };

        info!(?fastembed_model, dims, "loading ONNX embedding model");

        let model = TextEmbedding::try_new(
            InitOptions::new(fastembed_model).with_show_download_progress(false),
        )
        .map_err(|e| CodeforgeError::Embedding(format!("failed to load model: {e}")))?;

        Ok(Self {
            backend: EmbedBackend::Onnx(Mutex::new(model)),
            dims,
        })
    }

    /// Construct a Qwen3 candle-backed embedder.
    ///
    /// Loads `Qwen/Qwen3-Embedding-0.6B` from Hugging Face (~1.2 GB, cached by
    /// hf-hub after the first download).  Runs on CPU with F32 precision.
    #[cfg(feature = "qwen3")]
    fn new_qwen3() -> Result<Self> {
        use candle_core::{DType, Device};

        info!(
            repo = QWEN3_SMALL_REPO,
            dims = QWEN3_SMALL_DIMS,
            "loading Qwen3 candle embedding model"
        );

        let device = Device::Cpu;
        let model =
            fastembed::Qwen3TextEmbedding::from_hf(QWEN3_SMALL_REPO, &device, DType::F32, QWEN3_MAX_LENGTH)
                .map_err(|e| CodeforgeError::Embedding(format!("failed to load Qwen3 model: {e}")))?;

        Ok(Self {
            backend: EmbedBackend::Qwen3(Mutex::new(model)),
            dims: QWEN3_SMALL_DIMS,
        })
    }

    /// Embed a batch of text strings.
    ///
    /// Returns one embedding vector per input string, in the same order.
    pub fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        match &self.backend {
            EmbedBackend::Onnx(m) => {
                let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
                let mut model = m
                    .lock()
                    .map_err(|e| CodeforgeError::Embedding(format!("model lock poisoned: {e}")))?;
                model
                    .embed(refs, None)
                    .map_err(|e| CodeforgeError::Embedding(format!("embed failed: {e}")))
            }

            #[cfg(feature = "qwen3")]
            EmbedBackend::Qwen3(m) => {
                let model = m
                    .lock()
                    .map_err(|e| CodeforgeError::Embedding(format!("model lock poisoned: {e}")))?;
                // Qwen3TextEmbedding::embed takes &[S] where S: AsRef<str>.
                // It does NOT require &mut self (the candle tensors are built
                // fresh on each call).
                model
                    .embed(&texts)
                    .map_err(|e| CodeforgeError::Embedding(format!("embed failed: {e}")))
            }
        }
    }

    /// Embed a single text string.
    pub fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        match &self.backend {
            EmbedBackend::Onnx(m) => {
                let mut model = m
                    .lock()
                    .map_err(|e| CodeforgeError::Embedding(format!("model lock poisoned: {e}")))?;
                let mut results = model
                    .embed(vec![text], None)
                    .map_err(|e| CodeforgeError::Embedding(format!("embed failed: {e}")))?;
                results
                    .pop()
                    .ok_or_else(|| CodeforgeError::Embedding("empty embedding result".to_string()))
            }

            #[cfg(feature = "qwen3")]
            EmbedBackend::Qwen3(m) => {
                let model = m
                    .lock()
                    .map_err(|e| CodeforgeError::Embedding(format!("model lock poisoned: {e}")))?;
                let mut results = model
                    .embed(&[text])
                    .map_err(|e| CodeforgeError::Embedding(format!("embed failed: {e}")))?;
                results
                    .pop()
                    .ok_or_else(|| CodeforgeError::Embedding("empty embedding result".to_string()))
            }
        }
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

    #[test]
    #[cfg(feature = "qwen3")]
    #[ignore]
    fn qwen3_embed_produces_correct_dims() {
        let embedder = Embedder::new(&EmbeddingModel::Qwen3SmallEmbedding).unwrap();
        assert_eq!(embedder.dims, QWEN3_SMALL_DIMS);

        let vecs = embedder
            .embed(vec!["hello world".to_string(), "fn main() {}".to_string()])
            .unwrap();
        assert_eq!(vecs.len(), 2);
        assert_eq!(vecs[0].len(), QWEN3_SMALL_DIMS);
    }
}

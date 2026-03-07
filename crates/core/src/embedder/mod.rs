use std::sync::Mutex;

use fastembed::{EmbeddingModel as FastEmbedModel, InitOptions, TextEmbedding};
use tracing::info;

use crate::config::EmbeddingModel;
use crate::error::{CodixingError, Result};

/// Number of dimensions for BGE Small EN v1.5.
pub const BGE_SMALL_EN_DIMS: usize = 384;

/// Number of dimensions for BGE Base EN v1.5.
pub const BGE_BASE_EN_DIMS: usize = 768;

/// Number of dimensions for Jina Embeddings v2 Base Code.
pub const JINA_EMBED_CODE_DIMS: usize = 768;

/// Number of dimensions for BGE Large EN v1.5.
pub const BGE_LARGE_EN_DIMS: usize = 1024;

/// Number of dimensions for Nomic Embed Code.
pub const NOMIC_EMBED_CODE_DIMS: usize = 768;

/// Number of dimensions for Snowflake Arctic Embed L.
pub const SNOWFLAKE_ARCTIC_L_DIMS: usize = 1024;

/// Number of dimensions for Qwen3-Embedding-0.6B (hidden_size from model config).
#[cfg(feature = "qwen3")]
pub const QWEN3_SMALL_DIMS: usize = 1024;

/// HuggingFace repo that hosts the int8-quantized ONNX export of Qwen3-0.6B.
#[cfg(feature = "qwen3")]
const QWEN3_ONNX_REPO: &str = "onnx-community/Qwen3-Embedding-0.6B-ONNX";

/// Path inside the repo to the single-file int8 quantized model.
#[cfg(feature = "qwen3")]
const QWEN3_ONNX_FILE: &str = "onnx/model_int8.onnx";

/// Maximum token sequence length passed to the Qwen3 tokenizer.
#[cfg(feature = "qwen3")]
const QWEN3_MAX_LENGTH: usize = 512;

/// ORT-based Qwen3 embedder.
///
/// Loads `onnx-community/Qwen3-Embedding-0.6B-ONNX` (int8-quantized, ~380 MB)
/// and runs inference via ONNX Runtime — the same runtime used by the BGE
/// models.
///
/// The model was exported as a generation model (with KV cache), so we must
/// supply empty past KV tensors.  Sub-batching to B=8 keeps peak memory stable
/// (~10 GB) while being the fastest batch size on this architecture.
///
/// Pooling: last-token with left-padding — the real last token is always at
/// position T-1, simplifying pooling regardless of sequence length.
#[cfg(feature = "qwen3")]
struct OrtQwen3Session {
    session: ort_qwen3::session::Session,
    tokenizer: tokenizers_qwen3::Tokenizer,
    /// Number of past_key_values.N.key/value pairs the model expects.
    num_kv_layers: usize,
}

#[cfg(feature = "qwen3")]
impl OrtQwen3Session {
    fn from_hf(repo_id: &str, onnx_file: &str, max_length: usize) -> Result<Self> {
        use hf_hub::api::sync::ApiBuilder;
        use ort_qwen3::session::{Session, builder::GraphOptimizationLevel};
        use tokenizers_qwen3::{
            PaddingDirection, PaddingParams, PaddingStrategy, TruncationParams,
        };

        let api = ApiBuilder::new()
            .build()
            .map_err(|e| CodixingError::Embedding(format!("hf-hub init failed: {e}")))?;
        let repo = api.model(repo_id.to_string());

        let model_path = repo
            .get(onnx_file)
            .map_err(|e| CodixingError::Embedding(format!("model download failed: {e}")))?;
        let tok_path = repo
            .get("tokenizer.json")
            .map_err(|e| CodixingError::Embedding(format!("tokenizer download failed: {e}")))?;

        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);

        let session = Session::builder()
            .map_err(|e| CodixingError::Embedding(format!("ort builder: {e}")))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| CodixingError::Embedding(format!("ort opt level: {e}")))?
            .with_intra_threads(threads)
            .map_err(|e| CodixingError::Embedding(format!("ort threads: {e}")))?
            .commit_from_file(model_path)
            .map_err(|e| CodixingError::Embedding(format!("ort model load: {e}")))?;

        let mut tokenizer = tokenizers_qwen3::Tokenizer::from_file(tok_path)
            .map_err(|e| CodixingError::Embedding(format!("tokenizer load: {e}")))?;

        // Left-padding: ensures the real last token is always at position T-1,
        // simplifying last-token pooling regardless of sequence length.
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            direction: PaddingDirection::Left,
            ..Default::default()
        }));
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length,
                ..Default::default()
            }))
            .map_err(|e| CodixingError::Embedding(format!("tokenizer truncation: {e}")))?;

        // Count how many past KV layers the model expects (one entry per key input).
        let num_kv_layers = session
            .inputs()
            .iter()
            .filter(|inp| {
                inp.name().starts_with("past_key_values.") && inp.name().ends_with(".key")
            })
            .count();

        Ok(Self {
            session,
            tokenizer,
            num_kv_layers,
        })
    }

    fn embed(&mut self, texts: &[impl AsRef<str>]) -> Result<Vec<Vec<f32>>> {
        use ndarray_qwen3::{Array2, Array4};
        use ort_qwen3::{session::SessionInputValue, value::Value};
        use std::borrow::Cow;

        let encodings = self
            .tokenizer
            .encode_batch(texts.iter().map(|s| s.as_ref()).collect::<Vec<_>>(), true)
            .map_err(|e| CodixingError::Embedding(format!("tokenize: {e}")))?;

        if encodings.is_empty() {
            return Ok(vec![]);
        }

        let batch_size = encodings.len();
        let seq_len = encodings[0].len();

        let mut ids: Vec<i64> = Vec::with_capacity(batch_size * seq_len);
        let mut mask: Vec<i64> = Vec::with_capacity(batch_size * seq_len);
        for enc in &encodings {
            ids.extend(enc.get_ids().iter().map(|&x| x as i64));
            mask.extend(enc.get_attention_mask().iter().map(|&x| x as i64));
        }

        // position_ids: cumsum of attention_mask - 1, clamped to 0.
        // For left-padded sequences this gives real token positions 0,1,2,…
        // while padding positions stay at 0 (masked out anyway).
        let mut pos: Vec<i64> = Vec::with_capacity(batch_size * seq_len);
        for b in 0..batch_size {
            let mut cum: i64 = 0;
            for t in 0..seq_len {
                cum += mask[b * seq_len + t];
                pos.push((cum - 1).max(0));
            }
        }

        let ids_arr = Array2::from_shape_vec((batch_size, seq_len), ids)
            .map_err(|e| CodixingError::Embedding(format!("ids shape: {e}")))?;
        let mask_arr = Array2::from_shape_vec((batch_size, seq_len), mask)
            .map_err(|e| CodixingError::Embedding(format!("mask shape: {e}")))?;
        let pos_arr = Array2::from_shape_vec((batch_size, seq_len), pos)
            .map_err(|e| CodixingError::Embedding(format!("pos shape: {e}")))?;

        let to_sv = |arr: Array2<i64>, label: &str| -> Result<SessionInputValue<'_>> {
            Value::from_array(arr)
                .map(SessionInputValue::from)
                .map_err(|e| CodixingError::Embedding(format!("{label} tensor: {e}")))
        };

        // The model was exported with KV-cache support (generation model).
        // For embedding (prefill only) we pass empty past tensors of shape
        // [B, num_kv_heads, 0, head_dim] — zero past_sequence_length.
        let mut session_inputs: Vec<(Cow<'_, str>, SessionInputValue<'_>)> = vec![
            ("input_ids".into(), to_sv(ids_arr, "ids")?),
            ("attention_mask".into(), to_sv(mask_arr, "mask")?),
            ("position_ids".into(), to_sv(pos_arr, "pos")?),
        ];

        for i in 0..self.num_kv_layers {
            let mk_empty = || {
                Value::from_array(Array4::<f32>::zeros((batch_size, 8, 0, 128)))
                    .map(SessionInputValue::from)
                    .map_err(|e| CodixingError::Embedding(format!("kv cache {i}: {e}")))
            };
            session_inputs.push((format!("past_key_values.{i}.key").into(), mk_empty()?));
            session_inputs.push((format!("past_key_values.{i}.value").into(), mk_empty()?));
        }

        let outputs = self
            .session
            .run(session_inputs)
            .map_err(|e| CodixingError::Embedding(format!("ort run: {e}")))?;

        // last_hidden_state: [B, T, H]
        // With left-padding, T-1 is always the real last (non-pad) token.
        let hidden = outputs["last_hidden_state"]
            .try_extract_array::<f32>()
            .map_err(|e| CodixingError::Embedding(format!("extract hidden: {e}")))?;

        let shape = hidden.shape();
        let (b, t, h) = (shape[0], shape[1], shape[2]);

        let mut result = Vec::with_capacity(b);
        for i in 0..b {
            let vec: Vec<f32> = (0..h).map(|j| hidden[[i, t - 1, j]]).collect();
            let norm = vec.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
            result.push(vec.into_iter().map(|x| x / norm).collect());
        }
        Ok(result)
    }
}

/// Backing inference engine.
enum EmbedBackend {
    /// ONNX Runtime backend (fastembed — BGE/Jina/Nomic/Snowflake models).
    Onnx(Mutex<TextEmbedding>),
    /// ONNX Runtime backend for Qwen3 (direct ORT, last-token pooling).
    #[cfg(feature = "qwen3")]
    Qwen3(Mutex<OrtQwen3Session>),
}

/// Wrapper around a fastembed embedding model.
///
/// Abstracts over the ONNX (BGE / Jina / Nomic / Snowflake) and Qwen3
/// backends so that callers always see the same `.embed()` / `.embed_one()`
/// interface.
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
    /// For standard fastembed models (BGE, Jina, Snowflake) the ONNX weights
    /// are downloaded once from the fastembed CDN and cached on disk.
    ///
    /// For `NomicEmbedCode` the model files are downloaded from HuggingFace
    /// on first use via `hf-hub` and loaded via fastembed's UserDefined API.
    pub fn new(model_cfg: &EmbeddingModel) -> Result<Self> {
        match model_cfg {
            // ── Standard fastembed ONNX backend ───────────────────────────────
            EmbeddingModel::BgeSmallEn
            | EmbeddingModel::BgeBaseEn
            | EmbeddingModel::BgeLargeEn
            | EmbeddingModel::JinaEmbedCode
            | EmbeddingModel::SnowflakeArcticEmbedL => Self::new_onnx(model_cfg),

            // ── Nomic Embed Code (UserDefined fastembed + hf-hub) ─────────────
            EmbeddingModel::NomicEmbedCode => Self::new_nomic_embed_code(),

            // ── Qwen3 ONNX backend ────────────────────────────────────────────
            #[cfg(feature = "qwen3")]
            EmbeddingModel::Qwen3SmallEmbedding => Self::new_qwen3(),
        }
    }

    /// Construct an ONNX-backed embedder using a pre-defined fastembed model.
    fn new_onnx(model_cfg: &EmbeddingModel) -> Result<Self> {
        let (fastembed_model, dims) = match model_cfg {
            EmbeddingModel::BgeSmallEn => (FastEmbedModel::BGESmallENV15, BGE_SMALL_EN_DIMS),
            EmbeddingModel::BgeBaseEn => (FastEmbedModel::BGEBaseENV15, BGE_BASE_EN_DIMS),
            EmbeddingModel::BgeLargeEn => (FastEmbedModel::BGELargeENV15, BGE_LARGE_EN_DIMS),
            EmbeddingModel::JinaEmbedCode => (
                FastEmbedModel::JinaEmbeddingsV2BaseCode,
                JINA_EMBED_CODE_DIMS,
            ),
            EmbeddingModel::SnowflakeArcticEmbedL => (
                FastEmbedModel::SnowflakeArcticEmbedL,
                SNOWFLAKE_ARCTIC_L_DIMS,
            ),
            // These are routed elsewhere in `new()`.
            EmbeddingModel::NomicEmbedCode => unreachable!(),
            #[cfg(feature = "qwen3")]
            EmbeddingModel::Qwen3SmallEmbedding => unreachable!(),
        };

        info!(?fastembed_model, dims, "loading ONNX embedding model");

        let model = TextEmbedding::try_new(
            InitOptions::new(fastembed_model).with_show_download_progress(false),
        )
        .map_err(|e| CodixingError::Embedding(format!("failed to load model: {e}")))?;

        Ok(Self {
            backend: EmbedBackend::Onnx(Mutex::new(model)),
            dims,
        })
    }

    /// Construct a Nomic Embed Code embedder.
    ///
    /// Downloads `nomic-ai/nomic-embed-code` (onnx/model.onnx, ~280 MB) from
    /// HuggingFace on first use and caches it via `hf-hub`.  Loaded via
    /// fastembed's `UserDefinedEmbeddingModel` API with mean pooling.
    fn new_nomic_embed_code() -> Result<Self> {
        use fastembed::{
            InitOptionsUserDefined, Pooling, TokenizerFiles, UserDefinedEmbeddingModel,
        };
        use hf_hub::api::sync::ApiBuilder;

        info!(
            repo = "nomic-ai/nomic-embed-code",
            dims = NOMIC_EMBED_CODE_DIMS,
            "loading Nomic Embed Code via UserDefinedEmbeddingModel"
        );

        let api = ApiBuilder::new()
            .build()
            .map_err(|e| CodixingError::Embedding(format!("hf-hub init: {e}")))?;
        let repo = api.model("nomic-ai/nomic-embed-code".to_string());

        let get = |file: &str| -> Result<Vec<u8>> {
            let path = repo
                .get(file)
                .map_err(|e| CodixingError::Embedding(format!("download {file}: {e}")))?;
            std::fs::read(&path).map_err(|e| CodixingError::Embedding(format!("read {file}: {e}")))
        };

        let onnx = get("onnx/model.onnx")?;
        let tokenizer_files = TokenizerFiles {
            tokenizer_file: get("tokenizer.json")?,
            config_file: get("config.json")?,
            special_tokens_map_file: get("special_tokens_map.json")?,
            tokenizer_config_file: get("tokenizer_config.json")?,
        };

        let ud_model =
            UserDefinedEmbeddingModel::new(onnx, tokenizer_files).with_pooling(Pooling::Mean);

        let model =
            TextEmbedding::try_new_from_user_defined(ud_model, InitOptionsUserDefined::default())
                .map_err(|e| CodixingError::Embedding(format!("model init: {e}")))?;

        Ok(Self {
            backend: EmbedBackend::Onnx(Mutex::new(model)),
            dims: NOMIC_EMBED_CODE_DIMS,
        })
    }

    /// Construct a Qwen3 ONNX-backed embedder.
    ///
    /// Downloads `onnx-community/Qwen3-Embedding-0.6B-ONNX` (model_int8.onnx,
    /// ~380 MB) on first use and caches it via hf-hub.  Runs on CPU via the
    /// same ONNX Runtime shared library used by the BGE models — much faster
    /// than the previous candle backend and without the candle memory leak.
    #[cfg(feature = "qwen3")]
    fn new_qwen3() -> Result<Self> {
        info!(
            repo = QWEN3_ONNX_REPO,
            file = QWEN3_ONNX_FILE,
            dims = QWEN3_SMALL_DIMS,
            "loading Qwen3 ONNX embedding model"
        );

        let session = OrtQwen3Session::from_hf(QWEN3_ONNX_REPO, QWEN3_ONNX_FILE, QWEN3_MAX_LENGTH)?;

        Ok(Self {
            backend: EmbedBackend::Qwen3(Mutex::new(session)),
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
                    .map_err(|e| CodixingError::Embedding(format!("model lock poisoned: {e}")))?;
                model
                    .embed(refs, None)
                    .map_err(|e| CodixingError::Embedding(format!("embed failed: {e}")))
            }

            #[cfg(feature = "qwen3")]
            EmbedBackend::Qwen3(m) => {
                let mut session = m
                    .lock()
                    .map_err(|e| CodixingError::Embedding(format!("model lock poisoned: {e}")))?;
                // Sub-batch at B=8: this is the fastest batch size empirically
                // for the int8 KV-cache model on CPU (610s vs 881s at B=32).
                const QWEN3_BATCH: usize = 8;
                let mut results = Vec::with_capacity(texts.len());
                for chunk in texts.chunks(QWEN3_BATCH) {
                    results.extend(session.embed(chunk)?);
                }
                Ok(results)
            }
        }
    }

    /// Embed a single text string.
    pub fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        match &self.backend {
            EmbedBackend::Onnx(m) => {
                let mut model = m
                    .lock()
                    .map_err(|e| CodixingError::Embedding(format!("model lock poisoned: {e}")))?;
                let mut results = model
                    .embed(vec![text], None)
                    .map_err(|e| CodixingError::Embedding(format!("embed failed: {e}")))?;
                results
                    .pop()
                    .ok_or_else(|| CodixingError::Embedding("empty embedding result".to_string()))
            }

            #[cfg(feature = "qwen3")]
            EmbedBackend::Qwen3(m) => {
                let mut session = m
                    .lock()
                    .map_err(|e| CodixingError::Embedding(format!("model lock poisoned: {e}")))?;
                let mut results = session.embed(&[text])?;
                results
                    .pop()
                    .ok_or_else(|| CodixingError::Embedding("empty embedding result".to_string()))
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
    #[ignore]
    fn nomic_embed_code_produces_correct_dims() {
        let embedder = Embedder::new(&EmbeddingModel::NomicEmbedCode).unwrap();
        assert_eq!(embedder.dims, NOMIC_EMBED_CODE_DIMS);

        let vecs = embedder
            .embed(vec![
                "fn main() {}".to_string(),
                "def foo(): pass".to_string(),
            ])
            .unwrap();
        assert_eq!(vecs.len(), 2);
        assert_eq!(vecs[0].len(), NOMIC_EMBED_CODE_DIMS);
    }

    #[test]
    #[ignore]
    fn snowflake_arctic_l_produces_correct_dims() {
        let embedder = Embedder::new(&EmbeddingModel::SnowflakeArcticEmbedL).unwrap();
        assert_eq!(embedder.dims, SNOWFLAKE_ARCTIC_L_DIMS);

        let vecs = embedder
            .embed(vec!["hello world".to_string(), "fn main() {}".to_string()])
            .unwrap();
        assert_eq!(vecs.len(), 2);
        assert_eq!(vecs[0].len(), SNOWFLAKE_ARCTIC_L_DIMS);
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

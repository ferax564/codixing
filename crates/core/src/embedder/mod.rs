use std::sync::Mutex;

use fastembed::{EmbeddingModel as FastEmbedModel, InitOptions, OutputKey, TextEmbedding};
use safetensors::SafeTensors;
use tracing::{debug, info, warn};

use crate::config::EmbeddingModel;
use crate::error::{CodixingError, Result};

/// Maximum token sequence length for the ONNX-based models (BGE / Jina / etc.).
/// Files whose tokenized form exceeds this limit cannot use late chunking and
/// must fall back to independent per-chunk embedding.
const ONNX_MAX_SEQ_LEN: usize = 512;

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

/// Number of dimensions for Snowflake Arctic Embed XS.
pub const SNOWFLAKE_ARCTIC_XS_DIMS: usize = 384;

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

/// Static token embedding matrix for Model2Vec.
///
/// Each row corresponds to a token ID from the tokenizer. Document
/// embeddings are computed as the mean of the token vectors, then
/// L2-normalized. No ONNX runtime required.
struct Model2VecData {
    matrix: Vec<Vec<f32>>,
    tokenizer: tokenizers::Tokenizer,
    /// Whether to preprocess text (CamelCase splitting, structural char removal)
    /// before tokenization. True for BERT WordPiece tokenizers (potion models),
    /// false for BPE tokenizers (Jina Code) that handle code natively.
    needs_preprocessing: bool,
}

/// Pre-process text for Model2Vec's BERT WordPiece tokenizer.
///
/// Raw code produces poor subword splits because the tokenizer was trained
/// on English prose, not code. For example `createRateLimiter` → `create`,
/// `##rate`, `##lim`, `##iter` (3 meaningless subwords) instead of `create`,
/// `rate`, `limiter`.
///
/// This function:
/// 1. Splits CamelCase boundaries (`camelCase` → `camel case`)
/// 2. Splits acronym boundaries (`HTTPClient` → `http client`)
/// 3. Replaces underscores/structural characters with spaces
/// 4. Collapses whitespace and lowercases
///
/// After preprocessing, the BERT tokenizer produces 50-70% fewer subword
/// fragments, dramatically improving embedding quality for code search.
fn preprocess_for_model2vec(text: &str) -> String {
    let mut result = String::with_capacity(text.len() + text.len() / 4);

    let chars: Vec<char> = text.chars().collect();
    for i in 0..chars.len() {
        let c = chars[i];

        // Insert space at CamelCase boundaries:
        // - lowercase followed by uppercase (camelCase → camel Case)
        // - uppercase run ending before uppercase+lowercase (HTTPClient → HTTP Client)
        if i > 0
            && c.is_ascii_uppercase()
            && (chars[i - 1].is_ascii_lowercase()
                || (chars[i - 1].is_ascii_uppercase()
                    && i + 1 < chars.len()
                    && chars[i + 1].is_ascii_lowercase()))
        {
            result.push(' ');
        }

        match c {
            '_' | '(' | ')' | '{' | '}' | '[' | ']' | '<' | '>' | ':' | ';' | ',' | '.' | '-'
            | '+' | '=' | '/' | '*' | '\\' | '|' | '&' | '!' | '?' | '~' | '^' | '%' | '@'
            | '#' | '`' | '\'' | '"' => result.push(' '),
            _ => result.push(c.to_ascii_lowercase()),
        }
    }

    // Collapse whitespace
    let parts: Vec<&str> = result.split_whitespace().collect();
    parts.join(" ")
}

/// Mean-pool token vectors and L2-normalize the result.
///
/// Given the embedding matrix and a list of token IDs, computes the
/// element-wise mean of the corresponding rows, then normalizes to unit
/// length. Token IDs outside the matrix bounds are silently skipped.
/// Returns a zero vector if no valid tokens are provided.
fn mean_pool_and_normalize(matrix: &[Vec<f32>], token_ids: &[u32]) -> Vec<f32> {
    if matrix.is_empty() {
        return Vec::new();
    }
    let dims = matrix[0].len();
    let mut sum = vec![0.0f32; dims];
    let mut count = 0u32;

    for &tid in token_ids {
        let idx = tid as usize;
        if idx < matrix.len() {
            for (s, &v) in sum.iter_mut().zip(&matrix[idx]) {
                *s += v;
            }
            count += 1;
        }
    }

    if count == 0 {
        return vec![0.0; dims];
    }

    let inv_count = 1.0 / count as f32;
    for s in &mut sum {
        *s *= inv_count;
    }

    let norm = sum.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 1e-12 {
        for s in &mut sum {
            *s /= norm;
        }
    }

    sum
}

/// Backing inference engine.
enum EmbedBackend {
    /// ONNX Runtime backend (fastembed — BGE/Jina/Nomic/Snowflake models).
    Onnx(Mutex<TextEmbedding>),
    /// ONNX Runtime backend for Qwen3 (direct ORT, last-token pooling).
    #[cfg(feature = "qwen3")]
    Qwen3(Mutex<OrtQwen3Session>),
    /// Model2Vec static embeddings (lookup table, no ONNX).
    Model2Vec(Mutex<Model2VecData>),
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
    /// Optional prefix prepended to queries at search time.
    ///
    /// BGE models are trained with `"Represent this sentence: "` for queries
    /// but no prefix for passages. Adding this prefix improves cosine similarity
    /// between search queries and indexed code chunks.
    query_prefix: Option<&'static str>,
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
            | EmbeddingModel::BgeSmallEnQ
            | EmbeddingModel::BgeBaseEn
            | EmbeddingModel::BgeLargeEn
            | EmbeddingModel::JinaEmbedCode
            | EmbeddingModel::SnowflakeArcticEmbedXSQ
            | EmbeddingModel::SnowflakeArcticEmbedL => Self::new_onnx(model_cfg),

            // ── Nomic Embed Code (UserDefined fastembed + hf-hub) ─────────────
            EmbeddingModel::NomicEmbedCode => Self::new_nomic_embed_code(),

            // ── Qwen3 ONNX backend ────────────────────────────────────────────
            #[cfg(feature = "qwen3")]
            EmbeddingModel::Qwen3SmallEmbedding => Self::new_qwen3(),

            // ── Model2Vec static embeddings ──────────────────────────────────
            EmbeddingModel::Model2Vec => {
                Self::new_model2vec_variant(Self::MODEL2VEC_REPO, Self::MODEL2VEC_DIMS)
            }
            EmbeddingModel::Model2VecRetrieval => Self::new_model2vec_variant(
                Self::MODEL2VEC_RETRIEVAL_REPO,
                Self::MODEL2VEC_RETRIEVAL_DIMS,
            ),

            // ── Jina Code Int8 (local ONNX) ─────────────────────────────────
            EmbeddingModel::JinaCodeInt8 => Self::new_jina_code_int8(),

            // ── Model2Vec distilled from Jina Code (local dir) ──────────────
            EmbeddingModel::Model2VecJinaCode => Self::new_model2vec_local(),
        }
    }

    /// Construct an ONNX-backed embedder using a pre-defined fastembed model.
    fn new_onnx(model_cfg: &EmbeddingModel) -> Result<Self> {
        let (fastembed_model, dims) = match model_cfg {
            EmbeddingModel::BgeSmallEn => (FastEmbedModel::BGESmallENV15, BGE_SMALL_EN_DIMS),
            EmbeddingModel::BgeSmallEnQ => (FastEmbedModel::BGESmallENV15Q, BGE_SMALL_EN_DIMS),
            EmbeddingModel::BgeBaseEn => (FastEmbedModel::BGEBaseENV15, BGE_BASE_EN_DIMS),
            EmbeddingModel::BgeLargeEn => (FastEmbedModel::BGELargeENV15, BGE_LARGE_EN_DIMS),
            EmbeddingModel::JinaEmbedCode => (
                FastEmbedModel::JinaEmbeddingsV2BaseCode,
                JINA_EMBED_CODE_DIMS,
            ),
            EmbeddingModel::SnowflakeArcticEmbedXSQ => (
                FastEmbedModel::SnowflakeArcticEmbedXSQ,
                SNOWFLAKE_ARCTIC_XS_DIMS,
            ),
            EmbeddingModel::SnowflakeArcticEmbedL => (
                FastEmbedModel::SnowflakeArcticEmbedL,
                SNOWFLAKE_ARCTIC_L_DIMS,
            ),
            // These are routed elsewhere in `new()`.
            EmbeddingModel::NomicEmbedCode => unreachable!(),
            #[cfg(feature = "qwen3")]
            EmbeddingModel::Qwen3SmallEmbedding => unreachable!(),
            EmbeddingModel::Model2Vec
            | EmbeddingModel::Model2VecRetrieval
            | EmbeddingModel::Model2VecJinaCode => unreachable!(),
            EmbeddingModel::JinaCodeInt8 => unreachable!(),
        };

        // BGE models are trained with an instruction prefix for queries.
        let query_prefix = match model_cfg {
            EmbeddingModel::BgeSmallEn
            | EmbeddingModel::BgeSmallEnQ
            | EmbeddingModel::BgeBaseEn
            | EmbeddingModel::BgeLargeEn => Some("Represent this sentence: "),
            _ => None,
        };

        info!(?fastembed_model, dims, "loading ONNX embedding model");

        let model = TextEmbedding::try_new(
            InitOptions::new(fastembed_model).with_show_download_progress(false),
        )
        .map_err(|e| CodixingError::Embedding(format!("failed to load model: {e}")))?;

        Ok(Self {
            backend: EmbedBackend::Onnx(Mutex::new(model)),
            dims,
            query_prefix,
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
            query_prefix: None,
        })
    }

    const JINA_CODE_INT8_DIMS: usize = 768;
    const JINA_CODE_INT8_HF_REPO: &'static str = "jinaai/jina-embeddings-v2-base-code";

    /// Construct a Jina Code Int8 embedder from a local ONNX file.
    ///
    /// Loads `jina-embeddings-v2-base-code` int8-quantized for ARM64.
    /// The ONNX file path is read from `JINA_CODE_INT8_ONNX` env var.
    /// Tokenizer files are downloaded from HuggingFace on first use.
    fn new_jina_code_int8() -> Result<Self> {
        use fastembed::{
            InitOptionsUserDefined, Pooling, TokenizerFiles, UserDefinedEmbeddingModel,
        };
        use hf_hub::api::sync::ApiBuilder;

        let onnx_path = std::env::var("JINA_CODE_INT8_ONNX").map_err(|_| {
            CodixingError::Embedding(
                "JINA_CODE_INT8_ONNX env var not set. \
                 Point it at model_qint8_arm64.onnx"
                    .to_string(),
            )
        })?;

        info!(
            onnx = %onnx_path,
            dims = Self::JINA_CODE_INT8_DIMS,
            "loading Jina Code Int8 embedder"
        );

        let onnx = std::fs::read(&onnx_path)
            .map_err(|e| CodixingError::Embedding(format!("read ONNX file {onnx_path}: {e}")))?;

        // Tokenizer from HuggingFace (same repo as the FP32 model).
        let api = ApiBuilder::new()
            .build()
            .map_err(|e| CodixingError::Embedding(format!("hf-hub init: {e}")))?;
        let repo = api.model(Self::JINA_CODE_INT8_HF_REPO.to_string());

        let get = |file: &str| -> Result<Vec<u8>> {
            let path = repo
                .get(file)
                .map_err(|e| CodixingError::Embedding(format!("download {file}: {e}")))?;
            std::fs::read(&path).map_err(|e| CodixingError::Embedding(format!("read {file}: {e}")))
        };

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
                .map_err(|e| CodixingError::Embedding(format!("Jina Code Int8 init: {e}")))?;

        Ok(Self {
            backend: EmbedBackend::Onnx(Mutex::new(model)),
            dims: Self::JINA_CODE_INT8_DIMS,
            query_prefix: None,
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
            query_prefix: None,
        })
    }

    const MODEL2VEC_DIMS: usize = 256;
    const MODEL2VEC_REPO: &'static str = "minishlab/potion-base-8M";
    const MODEL2VEC_RETRIEVAL_DIMS: usize = 512;
    const MODEL2VEC_RETRIEVAL_REPO: &'static str = "minishlab/potion-retrieval-32M";

    fn new_model2vec_variant(repo_id: &str, _expected_dims: usize) -> Result<Self> {
        use hf_hub::api::sync::ApiBuilder;

        info!(
            repo = repo_id,
            dims = _expected_dims,
            "loading Model2Vec static embeddings"
        );

        let api = ApiBuilder::new()
            .build()
            .map_err(|e| CodixingError::Embedding(format!("hf-hub init: {e}")))?;
        let repo = api.model(repo_id.to_string());

        let safetensors_path = repo
            .get("model.safetensors")
            .map_err(|e| CodixingError::Embedding(format!("download model.safetensors: {e}")))?;
        let safetensors_bytes = std::fs::read(&safetensors_path)
            .map_err(|e| CodixingError::Embedding(format!("read model.safetensors: {e}")))?;

        let tensors = SafeTensors::deserialize(&safetensors_bytes)
            .map_err(|e| CodixingError::Embedding(format!("parse safetensors: {e}")))?;

        let embedding_tensor = tensors
            .tensor("embedding")
            .or_else(|_| tensors.tensor("embeddings"))
            .map_err(|_| {
                let names: Vec<_> = tensors.names().into_iter().collect();
                CodixingError::Embedding(format!(
                    "no 'embedding' tensor found in safetensors. Available: {:?}",
                    names
                ))
            })?;

        let shape = embedding_tensor.shape();
        if shape.len() != 2 {
            return Err(CodixingError::Embedding(format!(
                "expected 2D embedding tensor, got shape {:?}",
                shape
            )));
        }
        let vocab_size = shape[0];
        let dims = shape[1];

        let dtype = embedding_tensor.dtype();
        if !matches!(dtype, safetensors::Dtype::F32 | safetensors::Dtype::F16) {
            return Err(CodixingError::Embedding(format!(
                "unsupported embedding tensor dtype {:?} (need F32 or F16)",
                dtype
            )));
        }

        info!(
            vocab_size,
            dims,
            ?dtype,
            "Model2Vec embedding matrix loaded"
        );

        let raw_data = embedding_tensor.data();
        let bytes_per_elem = match dtype {
            safetensors::Dtype::F32 => 4,
            safetensors::Dtype::F16 => 2,
            _ => unreachable!(),
        };
        if raw_data.len() != vocab_size * dims * bytes_per_elem {
            return Err(CodixingError::Embedding(format!(
                "tensor byte count mismatch: expected {}, got {}",
                vocab_size * dims * bytes_per_elem,
                raw_data.len()
            )));
        }

        let mut matrix = Vec::with_capacity(vocab_size);
        for row_idx in 0..vocab_size {
            let mut row = Vec::with_capacity(dims);
            for col_idx in 0..dims {
                let val = match dtype {
                    safetensors::Dtype::F32 => {
                        let offset = (row_idx * dims + col_idx) * 4;
                        f32::from_le_bytes([
                            raw_data[offset],
                            raw_data[offset + 1],
                            raw_data[offset + 2],
                            raw_data[offset + 3],
                        ])
                    }
                    safetensors::Dtype::F16 => {
                        let offset = (row_idx * dims + col_idx) * 2;
                        let bits = u16::from_le_bytes([raw_data[offset], raw_data[offset + 1]]);
                        half::f16::from_bits(bits).to_f32()
                    }
                    _ => unreachable!(),
                };
                row.push(val);
            }
            matrix.push(row);
        }

        let tokenizer_path = repo
            .get("tokenizer.json")
            .map_err(|e| CodixingError::Embedding(format!("download tokenizer.json: {e}")))?;
        let tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| CodixingError::Embedding(format!("load tokenizer: {e}")))?;

        let data = Model2VecData {
            matrix,
            tokenizer,
            needs_preprocessing: true, // BERT WordPiece needs CamelCase splitting
        };

        Ok(Self {
            backend: EmbedBackend::Model2Vec(Mutex::new(data)),
            dims,
            query_prefix: None,
        })
    }

    /// Load a Model2Vec model from a local directory.
    ///
    /// Reads `model.safetensors` and `tokenizer.json` from the path in
    /// the `MODEL2VEC_JINA_CODE_DIR` env var. Supports F32 and F16 tensors.
    /// The Jina BPE tokenizer handles CamelCase natively, so no preprocessing
    /// is applied (unlike the BERT-based potion models).
    fn new_model2vec_local() -> Result<Self> {
        let dir = std::env::var("MODEL2VEC_JINA_CODE_DIR").map_err(|_| {
            CodixingError::Embedding(
                "MODEL2VEC_JINA_CODE_DIR env var not set. \
                 Point it at the model2vec-jina-code directory"
                    .to_string(),
            )
        })?;

        let dir = std::path::Path::new(&dir);
        info!(dir = %dir.display(), "loading Model2Vec Jina Code from local dir");

        let safetensors_bytes = std::fs::read(dir.join("model.safetensors"))
            .map_err(|e| CodixingError::Embedding(format!("read model.safetensors: {e}")))?;

        let tensors = SafeTensors::deserialize(&safetensors_bytes)
            .map_err(|e| CodixingError::Embedding(format!("parse safetensors: {e}")))?;

        let embedding_tensor = tensors
            .tensor("embeddings")
            .or_else(|_| tensors.tensor("embedding"))
            .map_err(|_| {
                CodixingError::Embedding("no 'embeddings' tensor in safetensors".to_string())
            })?;

        let shape = embedding_tensor.shape();
        if shape.len() != 2 {
            return Err(CodixingError::Embedding(format!(
                "expected 2D tensor, got shape {:?}",
                shape
            )));
        }
        let vocab_size = shape[0];
        let dims = shape[1];

        let dtype = embedding_tensor.dtype();
        let raw_data = embedding_tensor.data();
        let bytes_per_elem: usize = match dtype {
            safetensors::Dtype::F32 => 4,
            safetensors::Dtype::F16 => 2,
            _ => {
                return Err(CodixingError::Embedding(format!(
                    "unsupported dtype {:?}",
                    dtype
                )));
            }
        };

        if raw_data.len() != vocab_size * dims * bytes_per_elem {
            return Err(CodixingError::Embedding(
                "tensor byte count mismatch".to_string(),
            ));
        }

        let mut matrix = Vec::with_capacity(vocab_size);
        for row_idx in 0..vocab_size {
            let mut row = Vec::with_capacity(dims);
            for col_idx in 0..dims {
                let val = match dtype {
                    safetensors::Dtype::F32 => {
                        let off = (row_idx * dims + col_idx) * 4;
                        f32::from_le_bytes([
                            raw_data[off],
                            raw_data[off + 1],
                            raw_data[off + 2],
                            raw_data[off + 3],
                        ])
                    }
                    safetensors::Dtype::F16 => {
                        let off = (row_idx * dims + col_idx) * 2;
                        let bits = u16::from_le_bytes([raw_data[off], raw_data[off + 1]]);
                        half::f16::from_bits(bits).to_f32()
                    }
                    _ => unreachable!(),
                };
                row.push(val);
            }
            matrix.push(row);
        }

        info!(vocab_size, dims, ?dtype, "Model2Vec Jina Code loaded");

        let tokenizer = tokenizers::Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| CodixingError::Embedding(format!("load tokenizer: {e}")))?;

        let data = Model2VecData {
            matrix,
            tokenizer,
            needs_preprocessing: false, // Jina BPE handles code natively
        };

        Ok(Self {
            backend: EmbedBackend::Model2Vec(Mutex::new(data)),
            dims,
            query_prefix: None,
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

            EmbedBackend::Model2Vec(m) => {
                let data = m
                    .lock()
                    .map_err(|e| CodixingError::Embedding(format!("model lock poisoned: {e}")))?;
                let mut results = Vec::with_capacity(texts.len());
                for text in &texts {
                    let to_encode = if data.needs_preprocessing {
                        preprocess_for_model2vec(text)
                    } else {
                        text.to_string()
                    };
                    let encoding = data
                        .tokenizer
                        .encode(to_encode.as_str(), false)
                        .map_err(|e| CodixingError::Embedding(format!("tokenize: {e}")))?;
                    let ids = encoding.get_ids();
                    results.push(mean_pool_and_normalize(&data.matrix, ids));
                }
                Ok(results)
            }
        }
    }

    /// Embed a single query string, prepending the model-specific instruction
    /// prefix if applicable (e.g. `"Represent this sentence: "` for BGE).
    ///
    /// Use this for **search queries**. Use [`embed_one`] for documents/passages.
    pub fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        match self.query_prefix {
            Some(prefix) => {
                let prefixed = format!("{prefix}{query}");
                self.embed_one(&prefixed)
            }
            None => self.embed_one(query),
        }
    }

    /// Embed a single text string (document/passage — no instruction prefix).
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

            EmbedBackend::Model2Vec(m) => {
                let data = m
                    .lock()
                    .map_err(|e| CodixingError::Embedding(format!("model lock poisoned: {e}")))?;
                let to_encode = if data.needs_preprocessing {
                    preprocess_for_model2vec(text)
                } else {
                    text.to_string()
                };
                let encoding = data
                    .tokenizer
                    .encode(to_encode.as_str(), false)
                    .map_err(|e| CodixingError::Embedding(format!("tokenize: {e}")))?;
                Ok(mean_pool_and_normalize(&data.matrix, encoding.get_ids()))
            }
        }
    }

    /// Return a reference to the ONNX model mutex, if this embedder uses ONNX.
    fn onnx_model_ref(&self) -> Option<&Mutex<TextEmbedding>> {
        match &self.backend {
            EmbedBackend::Onnx(m) => Some(m),
            #[cfg(feature = "qwen3")]
            EmbedBackend::Qwen3(_) => None,
            EmbedBackend::Model2Vec(_) => None,
        }
    }

    /// Embed a file using late chunking: pass the entire file through the
    /// transformer once, then mean-pool per-chunk sub-ranges of the
    /// token-level embeddings.
    ///
    /// Late chunking preserves cross-chunk context (e.g. knowing that `self`
    /// refers to a specific struct) because the full-attention pass sees the
    /// entire file, not just the isolated chunk.
    ///
    /// # Arguments
    ///
    /// * `file_text` -- the full file content.
    /// * `chunk_byte_ranges` -- `(start, end)` byte offsets into `file_text`
    ///   for each chunk. The caller is responsible for ensuring these are valid
    ///   byte-offset pairs within `file_text`.
    ///
    /// # Returns
    ///
    /// * `Ok(Some(embeddings))` -- one embedding per chunk (same order).
    /// * `Ok(None)` -- the file exceeds the model's context window or the
    ///   backend does not support late chunking. The caller should fall back
    ///   to independent per-chunk embedding.
    pub fn embed_file_late_chunking(
        &self,
        file_text: &str,
        chunk_byte_ranges: &[(usize, usize)],
    ) -> Result<Option<Vec<Vec<f32>>>> {
        // Late chunking is only supported for ONNX-backed models.
        let mutex = self.onnx_model_ref();
        let Some(mutex) = mutex else {
            return Ok(None);
        };

        let mut model = mutex
            .lock()
            .map_err(|e| CodixingError::Embedding(format!("lock: {e}")))?;

        // ── 1. Tokenize to discover total token count + byte offsets ──────
        //
        // We use the model's own tokenizer (which already has truncation and
        // padding configured) to encode just the file text.  The returned
        // `Encoding` gives us per-token byte offsets via `get_offsets()`.
        //
        // If the tokenized sequence (excluding special tokens) exceeds the
        // context window the tokenizer will have truncated it, so late
        // chunking would lose tail content.  Detect this and bail out.
        let encoding = model
            .tokenizer
            .encode(file_text, true)
            .map_err(|e| CodixingError::Embedding(format!("tokenize: {e}")))?;

        let token_offsets = encoding.get_offsets();
        let special_mask = encoding.get_special_tokens_mask();

        // Count real (non-special) tokens.  If the tokenizer truncated the
        // input, the last real token's end offset will be far from the end
        // of file_text.  A more reliable check: if the encoding hit exactly
        // ONNX_MAX_SEQ_LEN tokens (the truncation limit), the file is too
        // long.
        let real_token_count = special_mask.iter().filter(|&&m| m == 0).count();
        if encoding.len() >= ONNX_MAX_SEQ_LEN {
            debug!(
                tokens = encoding.len(),
                limit = ONNX_MAX_SEQ_LEN,
                "file exceeds context window, skipping late chunking"
            );
            return Ok(None);
        }

        if chunk_byte_ranges.is_empty() || real_token_count == 0 {
            return Ok(Some(Vec::new()));
        }

        // ── 2. Run the transformer to get raw token-level outputs ─────────
        let output = model
            .transform(vec![file_text], None)
            .map_err(|e| CodixingError::Embedding(format!("transform: {e}")))?;

        let raw_batches = output.into_raw();
        if raw_batches.is_empty() {
            return Ok(None);
        }

        let batch = &raw_batches[0];

        // Try to extract the last_hidden_state tensor [1, seq_len, dims].
        let precedence = [OutputKey::ByName("last_hidden_state")];
        let tensor = match batch.select_output(&&precedence[..]) {
            Ok(t) => t,
            Err(e) => {
                warn!("model does not expose last_hidden_state: {e}");
                return Ok(None);
            }
        };

        let shape = tensor.shape();
        if shape.len() != 3 {
            debug!(
                ndim = shape.len(),
                "unexpected tensor rank, skipping late chunking"
            );
            return Ok(None);
        }

        let seq_len = shape[1];
        let dims = shape[2];

        // Flatten the tensor to a contiguous slice for fast indexing.
        // shape is [1, seq_len, dims].
        let flat = tensor.as_slice().ok_or_else(|| {
            CodixingError::Embedding("last_hidden_state tensor is not contiguous".to_string())
        })?;

        // ── 3. Map chunk byte ranges to token index ranges ────────────────
        //
        // `token_offsets[t]` is `(byte_start, byte_end)` for token `t`.
        // Special tokens (CLS, SEP) typically have offset `(0,0)`.
        //
        // For each chunk we find the first token whose byte_start >= chunk_start
        // and the last token whose byte_end <= chunk_end.
        let mut chunk_embeddings = Vec::with_capacity(chunk_byte_ranges.len());

        for &(chunk_start, chunk_end) in chunk_byte_ranges {
            // Find the token range that overlaps this chunk.
            let tok_start = token_offsets
                .iter()
                .zip(special_mask.iter())
                .position(|(&(ts, _te), &sp)| sp == 0 && ts >= chunk_start)
                .unwrap_or(0);

            // Find the last overlapping token (inclusive).
            let tok_end_inclusive = token_offsets
                .iter()
                .zip(special_mask.iter())
                .enumerate()
                .rev()
                .find(|(_, ((_, te), sp))| **sp == 0 && *te <= chunk_end)
                .map(|(i, _)| i);

            let tok_end = match tok_end_inclusive {
                Some(end) if end >= tok_start => end + 1,
                _ => {
                    // No tokens map to this chunk -- produce a zero vector.
                    chunk_embeddings.push(vec![0.0f32; dims]);
                    continue;
                }
            };

            // Ensure we don't exceed the actual sequence length.
            let tok_start = tok_start.min(seq_len);
            let tok_end = tok_end.min(seq_len);
            if tok_start >= tok_end {
                chunk_embeddings.push(vec![0.0f32; dims]);
                continue;
            }

            // Mean pool the token range.
            let count = (tok_end - tok_start) as f32;
            let mut embedding = vec![0.0f32; dims];
            for t in tok_start..tok_end {
                let base = t * dims;
                for (d, emb) in embedding.iter_mut().enumerate() {
                    *emb += flat[base + d];
                }
            }
            for emb in embedding.iter_mut() {
                *emb /= count;
            }

            // L2 normalize.
            let norm = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 1e-12 {
                for v in &mut embedding {
                    *v /= norm;
                }
            }

            chunk_embeddings.push(embedding);
        }

        Ok(Some(chunk_embeddings))
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

    #[test]
    #[cfg(feature = "qwen3")]
    #[ignore]
    fn late_chunking_returns_none_for_qwen3() {
        let embedder = Embedder::new(&EmbeddingModel::Qwen3SmallEmbedding).unwrap();
        let result = embedder
            .embed_file_late_chunking("fn main() {}", &[(0, 12)])
            .unwrap();
        assert!(result.is_none(), "Qwen3 backend should return None");
    }

    #[test]
    #[ignore]
    fn late_chunking_returns_embeddings_for_bge() {
        let embedder = Embedder::new(&EmbeddingModel::BgeSmallEn).unwrap();

        let file_text = concat!(
            "struct Foo {\n",
            "    bar: u32,\n",
            "}\n",
            "\n",
            "impl Foo {\n",
            "    fn baz(&self) -> u32 {\n",
            "        self.bar\n",
            "    }\n",
            "}\n",
        );
        // Two chunks: the struct definition and the impl block.
        let ranges = [(0usize, 28usize), (29usize, file_text.len())];

        let result = embedder
            .embed_file_late_chunking(file_text, &ranges)
            .unwrap();
        let embeddings = result.expect("BGE should support late chunking for short files");
        assert_eq!(embeddings.len(), 2);
        assert_eq!(embeddings[0].len(), BGE_SMALL_EN_DIMS);
        assert_eq!(embeddings[1].len(), BGE_SMALL_EN_DIMS);

        // Verify L2 normalization (norm should be ~1.0).
        let norm0: f32 = embeddings[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm0 - 1.0).abs() < 1e-4,
            "embedding should be L2-normalized, got norm={norm0}"
        );

        // Late-chunked embeddings for different chunks should differ.
        let dot: f32 = embeddings[0]
            .iter()
            .zip(embeddings[1].iter())
            .map(|(a, b)| a * b)
            .sum();
        assert!(
            dot < 0.9999,
            "embeddings for different chunks should differ, cosine={dot}"
        );
    }

    #[test]
    #[ignore]
    fn late_chunking_returns_none_for_long_files() {
        let embedder = Embedder::new(&EmbeddingModel::BgeSmallEn).unwrap();

        // Create a file that exceeds 512 tokens (BGE's context window).
        // ~4 chars per token on average, so 600 functions should exceed 512 tokens.
        let long_file: String = (0..600)
            .map(|i| format!("fn func_{i}() {{ let x_{i} = {i}; }}\n"))
            .collect();
        let ranges = [(0, long_file.len())];

        let result = embedder
            .embed_file_late_chunking(&long_file, &ranges)
            .unwrap();
        assert!(
            result.is_none(),
            "should return None for files exceeding context window"
        );
    }

    #[test]
    #[ignore]
    fn late_chunking_empty_chunks_returns_empty() {
        let embedder = Embedder::new(&EmbeddingModel::BgeSmallEn).unwrap();
        let result = embedder
            .embed_file_late_chunking("fn main() {}", &[])
            .unwrap();
        let embeddings = result.expect("should succeed with empty chunks");
        assert!(embeddings.is_empty());
    }
}

#[cfg(test)]
mod model2vec_tests {
    use super::*;

    #[test]
    fn model2vec_mean_pool_and_normalize() {
        let matrix = vec![
            vec![1.0, 0.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0, 0.0],
            vec![0.0, 0.0, 1.0, 0.0],
        ];
        let result = mean_pool_and_normalize(&matrix, &[0, 1, 2]);
        assert_eq!(result.len(), 4);
        assert!((result[0] - 0.57735).abs() < 0.001);
        assert!((result[1] - 0.57735).abs() < 0.001);
        assert!((result[2] - 0.57735).abs() < 0.001);
        assert!((result[3] - 0.0).abs() < 0.001);
    }

    #[test]
    fn model2vec_mean_pool_empty_tokens() {
        let matrix = vec![vec![1.0, 0.0]];
        let result = mean_pool_and_normalize(&matrix, &[]);
        assert!(result.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn preprocess_splits_camel_case() {
        assert_eq!(
            preprocess_for_model2vec("createRateLimiter"),
            "create rate limiter"
        );
    }

    #[test]
    fn preprocess_splits_snake_case() {
        assert_eq!(
            preprocess_for_model2vec("redact_sensitive_text"),
            "redact sensitive text"
        );
    }

    #[test]
    fn preprocess_splits_acronym_boundary() {
        assert_eq!(preprocess_for_model2vec("HTTPClient"), "http client");
    }

    #[test]
    fn preprocess_removes_structural_chars() {
        assert_eq!(
            preprocess_for_model2vec("fn process_batch(items: Vec<Item>) -> Result<()>"),
            "fn process batch items vec item result"
        );
    }

    #[test]
    fn preprocess_handles_mixed_code() {
        assert_eq!(
            preprocess_for_model2vec(
                "class SecurityAuditLogger { constructor(private store: AuditStore) {} }"
            ),
            "class security audit logger constructor private store audit store"
        );
    }

    #[test]
    fn preprocess_preserves_natural_language() {
        // Queries that are already natural language should pass through cleanly.
        assert_eq!(preprocess_for_model2vec("rate limiting"), "rate limiting");
        assert_eq!(
            preprocess_for_model2vec("security audit logging"),
            "security audit logging"
        );
    }

    #[test]
    fn preprocess_empty_and_structural_only() {
        assert_eq!(preprocess_for_model2vec(""), "");
        assert_eq!(preprocess_for_model2vec("(){}[]"), "");
    }
}

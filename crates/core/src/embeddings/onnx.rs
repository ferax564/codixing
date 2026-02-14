//! ONNX Runtime embedding inference using `all-MiniLM-L6-v2`.
//!
//! Requires the `vector` feature flag.

use std::path::Path;
use std::sync::Mutex;

use ndarray::{Array2, Axis};
use ort::session::Session;
use tokenizers::Tokenizer;

use super::Embedder;
use crate::error::CodeforgeError;

/// Default embedding dimension for `all-MiniLM-L6-v2`.
const DEFAULT_DIMENSION: usize = 384;

/// Embedding inference backed by ONNX Runtime.
///
/// Loads an ONNX model (e.g. `all-MiniLM-L6-v2`) and a HuggingFace
/// `tokenizer.json` to produce dense float vectors from text.
///
/// The inner ONNX session requires `&mut self` for inference, so it is
/// wrapped in a [`Mutex`] to satisfy the `&self` signature of [`Embedder`].
pub struct OnnxEmbedder {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
    dimension: usize,
}

impl OnnxEmbedder {
    /// Load model from a directory containing `model.onnx` and `tokenizer.json`.
    ///
    /// Returns an error if the required files are missing or cannot be loaded.
    pub fn load(model_dir: &Path) -> Result<Self, CodeforgeError> {
        let model_path = model_dir.join("model.onnx");
        let tokenizer_path = model_dir.join("tokenizer.json");

        if !model_path.exists() {
            return Err(CodeforgeError::Embedding(format!(
                "model file not found: {}",
                model_path.display()
            )));
        }
        if !tokenizer_path.exists() {
            return Err(CodeforgeError::Embedding(format!(
                "tokenizer file not found: {}",
                tokenizer_path.display()
            )));
        }

        Self::from_paths(&model_path, &tokenizer_path)
    }

    /// Create with explicit model and tokenizer file paths.
    pub fn from_paths(model_path: &Path, tokenizer_path: &Path) -> Result<Self, CodeforgeError> {
        let session = Session::builder()
            .map_err(|e| {
                CodeforgeError::Embedding(format!("failed to create session builder: {e}"))
            })?
            .commit_from_file(model_path)
            .map_err(|e| {
                CodeforgeError::Embedding(format!(
                    "failed to load ONNX model from {}: {e}",
                    model_path.display()
                ))
            })?;

        let tokenizer = Tokenizer::from_file(tokenizer_path).map_err(|e| {
            CodeforgeError::Embedding(format!(
                "failed to load tokenizer from {}: {e}",
                tokenizer_path.display()
            ))
        })?;

        // Determine dimension from model output metadata if possible,
        // otherwise fall back to the known MiniLM-L6-v2 default.
        let dimension = session
            .outputs()
            .first()
            .and_then(|o| o.dtype().tensor_shape())
            .and_then(|shape| shape.last().copied())
            .map(|d| d as usize)
            .unwrap_or(DEFAULT_DIMENSION);

        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
            dimension,
        })
    }

    /// Mean-pool token embeddings, masking out padding tokens.
    fn mean_pool(
        hidden_state: &ndarray::ArrayView3<f32>,
        attention_mask: &Array2<i64>,
    ) -> Array2<f32> {
        let batch_size = hidden_state.shape()[0];
        let dim = hidden_state.shape()[2];
        let mut result = Array2::<f32>::zeros((batch_size, dim));

        for b in 0..batch_size {
            let mut count = 0.0_f32;
            for t in 0..hidden_state.shape()[1] {
                let mask = attention_mask[[b, t]] as f32;
                if mask > 0.0 {
                    for d in 0..dim {
                        result[[b, d]] += hidden_state[[b, t, d]] * mask;
                    }
                    count += mask;
                }
            }
            if count > 0.0 {
                for d in 0..dim {
                    result[[b, d]] /= count;
                }
            }
        }

        result
    }

    /// L2-normalize each row vector in place.
    fn l2_normalize(vectors: &mut Array2<f32>) {
        for mut row in vectors.rows_mut() {
            let norm: f32 = row.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                row.mapv_inplace(|x| x / norm);
            }
        }
    }

    /// Run inference on pre-tokenized inputs and return normalized embeddings.
    fn infer(
        &self,
        input_ids: Array2<i64>,
        attention_mask: Array2<i64>,
        token_type_ids: Array2<i64>,
    ) -> Result<Array2<f32>, CodeforgeError> {
        use ort::value::Tensor;

        let ids_tensor = Tensor::from_array(input_ids).map_err(|e| {
            CodeforgeError::Embedding(format!("failed to create input_ids tensor: {e}"))
        })?;
        let mask_tensor = Tensor::from_array(attention_mask.clone()).map_err(|e| {
            CodeforgeError::Embedding(format!("failed to create attention_mask tensor: {e}"))
        })?;
        let type_tensor = Tensor::from_array(token_type_ids).map_err(|e| {
            CodeforgeError::Embedding(format!("failed to create token_type_ids tensor: {e}"))
        })?;

        let mut session = self
            .session
            .lock()
            .map_err(|e| CodeforgeError::Embedding(format!("session lock poisoned: {e}")))?;

        let outputs = session
            .run(ort::inputs![
                "input_ids" => ids_tensor,
                "attention_mask" => mask_tensor,
                "token_type_ids" => type_tensor,
            ])
            .map_err(|e| CodeforgeError::Embedding(format!("ONNX inference failed: {e}")))?;

        // The model outputs last_hidden_state as the first output.
        let hidden_state = outputs[0].try_extract_array::<f32>().map_err(|e| {
            CodeforgeError::Embedding(format!("failed to extract output tensor: {e}"))
        })?;

        // Reshape into 3D (batch, seq_len, hidden_dim).
        let view_3d = if hidden_state.ndim() == 3 {
            hidden_state
                .into_dimensionality::<ndarray::Ix3>()
                .map_err(|e| CodeforgeError::Embedding(format!("unexpected output shape: {e}")))?
        } else {
            return Err(CodeforgeError::Embedding(format!(
                "expected 3D output tensor, got {}D",
                hidden_state.ndim()
            )));
        };

        let mut pooled = Self::mean_pool(&view_3d, &attention_mask);
        Self::l2_normalize(&mut pooled);

        Ok(pooled)
    }
}

impl Embedder for OnnxEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>, CodeforgeError> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| CodeforgeError::Embedding(format!("tokenization failed: {e}")))?;

        let ids: Vec<i64> = encoding.get_ids().iter().map(|&id| id as i64).collect();
        let mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|&m| m as i64)
            .collect();
        let type_ids: Vec<i64> = encoding.get_type_ids().iter().map(|&t| t as i64).collect();

        let seq_len = ids.len();
        let input_ids = Array2::from_shape_vec((1, seq_len), ids)
            .map_err(|e| CodeforgeError::Embedding(format!("shape error: {e}")))?;
        let attention_mask = Array2::from_shape_vec((1, seq_len), mask)
            .map_err(|e| CodeforgeError::Embedding(format!("shape error: {e}")))?;
        let token_type_ids = Array2::from_shape_vec((1, seq_len), type_ids)
            .map_err(|e| CodeforgeError::Embedding(format!("shape error: {e}")))?;

        let result = self.infer(input_ids, attention_mask, token_type_ids)?;
        Ok(result.row(0).to_vec())
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, CodeforgeError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| CodeforgeError::Embedding(format!("batch tokenization failed: {e}")))?;

        let batch_size = encodings.len();
        let max_len = encodings.iter().map(|e| e.len()).max().unwrap_or(0);

        // Build padded arrays.
        let mut input_ids = Array2::<i64>::zeros((batch_size, max_len));
        let mut attention_mask = Array2::<i64>::zeros((batch_size, max_len));
        let mut token_type_ids = Array2::<i64>::zeros((batch_size, max_len));

        for (i, enc) in encodings.iter().enumerate() {
            for (j, &id) in enc.get_ids().iter().enumerate() {
                input_ids[[i, j]] = id as i64;
            }
            for (j, &m) in enc.get_attention_mask().iter().enumerate() {
                attention_mask[[i, j]] = m as i64;
            }
            for (j, &t) in enc.get_type_ids().iter().enumerate() {
                token_type_ids[[i, j]] = t as i64;
            }
        }

        let result = self.infer(input_ids, attention_mask, token_type_ids)?;
        Ok(result.axis_iter(Axis(0)).map(|row| row.to_vec()).collect())
    }

    fn dimension(&self) -> usize {
        self.dimension
    }
}

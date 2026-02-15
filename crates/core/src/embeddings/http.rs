//! HTTP-based embedding backend for external APIs.
//!
//! Supports OpenAI-compatible embedding APIs (OpenAI, Voyage Code-3, Jina,
//! Cohere, etc.) by sending `POST` requests with `{"model": "...", "input":
//! [...]}` and parsing `{"data": [{"embedding": [...]}]}` responses.

use serde::{Deserialize, Serialize};
use tracing::debug;

use super::Embedder;
use crate::error::CodeforgeError;

/// An embedding backend that calls an external HTTP API.
///
/// The API must accept OpenAI-compatible request/response format:
///
/// **Request:**
/// ```json
/// {"model": "text-embedding-3-small", "input": ["text1", "text2"]}
/// ```
///
/// **Response:**
/// ```json
/// {"data": [{"embedding": [0.1, 0.2, ...]}, ...]}
/// ```
pub struct HttpEmbedder {
    client: reqwest::blocking::Client,
    url: String,
    model: String,
    dim: usize,
    api_key: Option<String>,
    batch_size: usize,
}

/// Request body sent to the embedding API.
#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: &'a [&'a str],
}

/// Response body from the embedding API.
#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedDataItem>,
}

/// A single embedding result in the response.
#[derive(Deserialize)]
struct EmbedDataItem {
    embedding: Vec<f32>,
}

impl HttpEmbedder {
    /// Create a new HTTP embedder.
    ///
    /// # Arguments
    ///
    /// * `url` - The embedding API endpoint URL.
    /// * `model` - Model identifier to include in the request body.
    /// * `dim` - Expected embedding dimensionality.
    /// * `api_key` - Optional bearer token for authentication. If the value
    ///   starts with `$`, it is treated as an environment variable name and
    ///   resolved at construction time.
    /// * `batch_size` - Maximum number of texts per API call.
    pub fn new(
        url: &str,
        model: &str,
        dim: usize,
        api_key: Option<String>,
        batch_size: usize,
    ) -> Self {
        // Resolve env-var references in api_key.
        let resolved_key = api_key.map(|key| {
            if let Some(var_name) = key.strip_prefix('$') {
                std::env::var(var_name).unwrap_or(key)
            } else {
                key
            }
        });

        Self {
            client: reqwest::blocking::Client::new(),
            url: url.to_string(),
            model: model.to_string(),
            dim,
            api_key: resolved_key,
            batch_size: if batch_size == 0 { 32 } else { batch_size },
        }
    }

    /// Call the embedding API with a batch of texts.
    fn call_api(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, CodeforgeError> {
        let body = EmbedRequest {
            model: &self.model,
            input: texts,
        };

        let mut request = self.client.post(&self.url).json(&body);

        if let Some(ref key) = self.api_key {
            request = request.bearer_auth(key);
        }

        debug!(
            url = %self.url,
            model = %self.model,
            batch_size = texts.len(),
            "calling embedding API"
        );

        let response = request.send().map_err(|e| {
            CodeforgeError::Embedding(format!("HTTP request to {} failed: {e}", self.url))
        })?;

        if !response.status().is_success() {
            let status = response.status();
            let body_text = response.text().unwrap_or_default();
            return Err(CodeforgeError::Embedding(format!(
                "embedding API returned {status}: {body_text}"
            )));
        }

        let embed_response: EmbedResponse = response.json().map_err(|e| {
            CodeforgeError::Embedding(format!("failed to parse embedding response: {e}"))
        })?;

        let embeddings: Vec<Vec<f32>> = embed_response
            .data
            .into_iter()
            .map(|item| item.embedding)
            .collect();

        // Validate dimensions.
        for (i, emb) in embeddings.iter().enumerate() {
            if emb.len() != self.dim {
                return Err(CodeforgeError::Embedding(format!(
                    "embedding[{i}] has dimension {} but expected {}",
                    emb.len(),
                    self.dim
                )));
            }
        }

        Ok(embeddings)
    }
}

impl Embedder for HttpEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>, CodeforgeError> {
        let results = self.call_api(&[text])?;
        results
            .into_iter()
            .next()
            .ok_or_else(|| CodeforgeError::Embedding("empty response from embedding API".into()))
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, CodeforgeError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let mut all_embeddings = Vec::with_capacity(texts.len());

        // Process in chunks of batch_size.
        for chunk in texts.chunks(self.batch_size) {
            let batch_result = self.call_api(chunk)?;
            all_embeddings.extend(batch_result);
        }

        Ok(all_embeddings)
    }

    fn dimension(&self) -> usize {
        self.dim
    }
}

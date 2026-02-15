//! HTTP-based reranker for external cross-encoder APIs.
//!
//! Supports Cohere/Jina-compatible rerank APIs by sending `POST` requests with
//! `{"model": "...", "query": "...", "documents": [...], "top_n": N}` and
//! parsing `{"results": [{"index": 0, "relevance_score": 0.95}, ...]}` responses.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::reranker::Reranker;
use super::SearchResult;
use crate::error::CodeforgeError;

/// A reranker that calls an external cross-encoder API over HTTP.
///
/// The API must accept Cohere/Jina-compatible request/response format:
///
/// **Request:**
/// ```json
/// {"model": "rerank-v3.5", "query": "...", "documents": ["text1", "text2"], "top_n": 10}
/// ```
///
/// **Response:**
/// ```json
/// {"results": [{"index": 0, "relevance_score": 0.95}, {"index": 1, "relevance_score": 0.3}]}
/// ```
pub struct HttpReranker {
    client: reqwest::blocking::Client,
    url: String,
    model: String,
    api_key: Option<String>,
}

/// Request body sent to the reranking API.
#[derive(Serialize)]
struct RerankRequest<'a> {
    model: &'a str,
    query: &'a str,
    documents: &'a [&'a str],
    top_n: usize,
}

/// Response body from the reranking API.
#[derive(Deserialize)]
struct RerankResponse {
    results: Vec<RerankResultItem>,
}

/// A single reranking result in the response.
#[derive(Deserialize)]
struct RerankResultItem {
    index: usize,
    relevance_score: f64,
}

impl HttpReranker {
    /// Create a new HTTP reranker.
    ///
    /// # Arguments
    ///
    /// * `url` - The reranking API endpoint URL.
    /// * `model` - Model identifier to include in the request body.
    /// * `api_key` - Optional bearer token for authentication. If the value
    ///   starts with `$`, it is treated as an environment variable name and
    ///   resolved at construction time.
    pub fn new(url: &str, model: &str, api_key: Option<String>) -> Self {
        // Resolve env-var references in api_key.
        let resolved_key = api_key.map(|key| {
            if let Some(var_name) = key.strip_prefix('$') {
                match std::env::var(var_name) {
                    Ok(val) => val,
                    Err(_) => {
                        warn!(
                            var = var_name,
                            "environment variable not found; using literal string as API key — \
                             this will likely cause authentication failures"
                        );
                        key
                    }
                }
            } else {
                key
            }
        });

        Self {
            client: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10))
                .build()
                .expect("failed to build HTTP client"),
            url: url.to_string(),
            model: model.to_string(),
            api_key: resolved_key,
        }
    }

    /// Return the model identifier.
    pub fn model(&self) -> &str {
        &self.model
    }
}

impl Reranker for HttpReranker {
    fn rerank(
        &self,
        query: &str,
        results: &[SearchResult],
        top_k: usize,
    ) -> Result<Vec<SearchResult>, CodeforgeError> {
        if results.is_empty() {
            return Ok(Vec::new());
        }

        let documents: Vec<&str> = results.iter().map(|r| r.content.as_str()).collect();

        let body = RerankRequest {
            model: &self.model,
            query,
            documents: &documents,
            top_n: top_k,
        };

        let mut request = self.client.post(&self.url).json(&body);

        if let Some(ref key) = self.api_key {
            request = request.bearer_auth(key);
        }

        debug!(
            url = %self.url,
            model = %self.model,
            candidates = results.len(),
            top_k = top_k,
            "calling reranking API"
        );

        let response = request.send().map_err(|e| {
            CodeforgeError::Embedding(format!("HTTP request to {} failed: {e}", self.url))
        })?;

        if !response.status().is_success() {
            let status = response.status();
            let body_text = response.text().unwrap_or_default();
            return Err(CodeforgeError::Embedding(format!(
                "reranking API returned {status}: {body_text}"
            )));
        }

        let rerank_response: RerankResponse = response.json().map_err(|e| {
            CodeforgeError::Embedding(format!("failed to parse reranking response: {e}"))
        })?;

        // Map API results back to SearchResult objects.
        let mut reranked: Vec<SearchResult> = Vec::with_capacity(rerank_response.results.len());
        for item in &rerank_response.results {
            if item.index < results.len() {
                let mut result = results[item.index].clone();
                result.score = item.relevance_score as f32;
                reranked.push(result);
            }
        }

        // Sort by score descending (API may already return sorted, but be safe).
        reranked.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        reranked.truncate(top_k);

        Ok(reranked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_reranker_constructs() {
        let reranker = HttpReranker::new(
            "https://api.cohere.com/v2/rerank",
            "rerank-v3.5",
            Some("test-key".to_string()),
        );

        assert_eq!(reranker.model(), "rerank-v3.5");
    }

    #[test]
    fn http_reranker_constructs_without_api_key() {
        let reranker = HttpReranker::new(
            "https://api.jina.ai/v1/rerank",
            "jina-reranker-v2-base-multilingual",
            None,
        );

        assert_eq!(reranker.model(), "jina-reranker-v2-base-multilingual");
    }
}

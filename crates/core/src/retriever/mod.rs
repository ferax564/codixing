pub mod bm25;
pub mod http_reranker;
pub mod hybrid;
pub mod reranker;

pub use hybrid::HybridRetriever;

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// A search query against the code index.
#[derive(Debug, Clone)]
pub struct SearchQuery {
    /// The search query string (BM25 text query).
    pub query: String,
    /// Maximum number of results to return.
    pub limit: usize,
    /// Optional file path substring filter.
    pub file_filter: Option<String>,
}

impl SearchQuery {
    /// Create a simple query with default limit and no filter.
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            limit: 10,
            file_filter: None,
        }
    }

    /// Set the maximum number of results.
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Set the file path filter.
    pub fn with_file_filter(mut self, filter: impl Into<String>) -> Self {
        self.file_filter = Some(filter.into());
        self
    }
}

/// A single search result with metadata and relevance score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    /// Deterministic chunk identifier.
    pub chunk_id: String,
    /// Path to the source file.
    pub file_path: String,
    /// Programming language name.
    pub language: String,
    /// BM25 relevance score.
    pub score: f32,
    /// Start line (0-indexed).
    pub line_start: u64,
    /// End line (0-indexed, exclusive).
    pub line_end: u64,
    /// Entity signatures found in this chunk.
    pub signature: String,
    /// The source code content of the chunk.
    pub content: String,
}

/// Trait for swappable retrieval strategies.
pub trait Retriever: Send + Sync {
    /// Execute a search query and return ranked results.
    fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>>;
}

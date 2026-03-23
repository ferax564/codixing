pub mod bm25;
pub mod hybrid;
pub mod mmr;
pub mod vector;

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Strategy preset controlling which retrieval pipeline to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    /// BM25 only — sub-millisecond, no vector embedding required.
    Instant,
    /// BM25 + vector + RRF fusion (default).
    #[default]
    Fast,
    /// BM25 + vector + RRF + MMR deduplication.
    Thorough,
    /// BM25 first-pass then graph expansion: surfaces files that are
    /// transitively connected to the initial results via the import graph.
    /// Best for architectural investigation on large codebases.
    Explore,
    /// Two-stage: hybrid BM25+vector first-pass (3× candidates) then
    /// BGE-Reranker-Base cross-encoder re-scores the top-30.
    /// Highest precision available; requires `reranker_enabled = true` in config.
    /// Falls back to `Thorough` if the reranker model is not loaded.
    Deep,
    /// Trigram index fast-path for exact identifier lookups.
    /// Uses the trigram inverted index for sub-millisecond exact substring
    /// matching, with BM25 fallback when trigram yields < 3 results.
    Exact,
}

/// A search query against the code index.
#[derive(Debug, Clone)]
pub struct SearchQuery {
    /// The search query string.
    pub query: String,
    /// Maximum number of results to return.
    pub limit: usize,
    /// Optional file path substring filter.
    pub file_filter: Option<String>,
    /// Retrieval strategy to use.
    pub strategy: Strategy,
    /// Token budget for formatted context output.
    pub token_budget: Option<usize>,
}

impl SearchQuery {
    /// Create a simple query with default limit and no filter.
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            limit: 10,
            file_filter: None,
            strategy: Strategy::default(),
            token_budget: None,
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

    /// Set the retrieval strategy.
    pub fn with_strategy(mut self, strategy: Strategy) -> Self {
        self.strategy = strategy;
        self
    }

    /// Set the token budget for formatted context output.
    pub fn with_token_budget(mut self, budget: usize) -> Self {
        self.token_budget = Some(budget);
        self
    }
}

/// A single search result with metadata and relevance score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    /// Deterministic chunk identifier (xxh3 hash as string).
    pub chunk_id: String,
    /// Path to the source file.
    pub file_path: String,
    /// Programming language name.
    pub language: String,
    /// Relevance score (BM25, cosine similarity, or fused RRF score).
    pub score: f32,
    /// Start line (0-indexed).
    pub line_start: u64,
    /// End line (0-indexed, exclusive).
    pub line_end: u64,
    /// Entity signatures found in this chunk.
    pub signature: String,
    /// AST scope chain (e.g. `["MyModule", "MyClass", "my_method"]`).
    #[serde(default)]
    pub scope_chain: Vec<String>,
    /// The source code content of the chunk.
    pub content: String,
}

/// Rich metadata for a chunk, used to hydrate vector search results.
///
/// Stored in the engine's `chunk_meta` table (keyed by `chunk_id: u64`).
/// Populated during indexing and persisted to `chunk_meta.bin`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkMeta {
    /// Deterministic chunk ID (xxh3).
    pub chunk_id: u64,
    /// Source file path.
    pub file_path: String,
    /// Programming language name.
    pub language: String,
    /// Start line (0-indexed).
    pub line_start: u64,
    /// End line (0-indexed, exclusive).
    pub line_end: u64,
    /// Joined entity signatures.
    pub signature: String,
    /// AST scope chain elements.
    pub scope_chain: Vec<String>,
    /// Names of entities contained in this chunk.
    #[serde(default)]
    pub entity_names: Vec<String>,
    /// Source code content.
    pub content: String,
    /// xxh3 hash of the chunk content, used for incremental vector updates.
    /// During sync, chunks whose content hash has not changed can skip
    /// re-embedding, reusing the existing vector.
    #[serde(default)]
    pub content_hash: u64,
}

/// Trait for swappable retrieval strategies.
pub trait Retriever: Send + Sync {
    /// Execute a search query and return ranked results.
    fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>>;
}

pub mod bm25;
pub mod hybrid;
pub mod mmr;
pub mod reranker;
pub mod routing;
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
    /// File-trigram candidate fast-path with exact substring verification.
    /// Uses the trigram inverted index for sub-millisecond exact substring
    /// matching, with BM25 fallback when trigram yields < 3 results.
    Exact,
    /// Embedding-free semantic matching using behavioral signatures.
    /// Decomposes queries into intent (verbs, nouns, types) and matches
    /// against AST-derived function signatures, call patterns, and concepts.
    Semantic,
}

/// Hard filter for document type in search results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocFilter {
    /// Only code and config chunks.
    CodeOnly,
    /// Only documentation chunks.
    DocsOnly,
}

/// Hard filter for imported external-context documents in search results.
///
/// External documents are indexed under the [`crate::external::EXTERNAL_PATH_PREFIX`]
/// path namespace (`_external/<source>/…`), so this filter matches on the
/// result's `file_path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceFilter {
    /// Only results from external imports (any source).
    ExternalOnly,
    /// Only results from a specific external source namespace (e.g. `"github"`).
    Named(String),
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
    /// Optional list of query reformulations for multi-query RRF fusion.
    /// When provided, each query is searched independently and results are
    /// fused via Reciprocal Rank Fusion. Overrides auto-reformulation.
    pub queries: Option<Vec<String>>,
    /// Filter results by document type.
    pub doc_filter: Option<DocFilter>,
    /// Filter results by external-context source.
    pub source_filter: Option<SourceFilter>,
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
            queries: None,
            doc_filter: None,
            source_filter: None,
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

    /// Filter results by document type.
    pub fn with_doc_filter(mut self, filter: DocFilter) -> Self {
        self.doc_filter = Some(filter);
        self
    }

    /// Filter results by external-context source.
    pub fn with_source_filter(mut self, filter: SourceFilter) -> Self {
        self.source_filter = Some(filter);
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

impl SearchResult {
    /// Whether this result comes from a documentation file
    /// (Markdown, HTML, reStructuredText, AsciiDoc, plain text).
    pub fn is_doc(&self) -> bool {
        matches!(
            self.language.as_str(),
            "Markdown" | "HTML" | "reStructuredText" | "AsciiDoc" | "Plain text"
        )
    }

    /// Whether this result is an imported external-context document
    /// (GitHub issue/PR, ADR, …), identified by its virtual path namespace.
    pub fn is_external(&self) -> bool {
        self.file_path
            .starts_with(crate::external::EXTERNAL_PATH_PREFIX)
    }

    /// The external source namespace (`"github"`, `"adr"`, …) when this result
    /// is an imported document, else `None`.
    pub fn external_source(&self) -> Option<&str> {
        self.file_path
            .strip_prefix(crate::external::EXTERNAL_PATH_PREFIX)
            .and_then(|rest| rest.split('/').next())
    }
}

/// Rich metadata for a chunk, used to hydrate vector search results.
///
/// Stored in the engine's `chunk_meta` table (keyed by `chunk_id: u64`).
/// Populated during indexing and persisted to `chunk_meta.bin`.
///
/// **Compact persistence (v2)**: The `content` field is NOT persisted to disk.
/// On load, `content` is set to an empty string. Code that needs chunk content
/// should fetch it from Tantivy stored fields via [`Engine::get_chunk_content`].
/// During indexing and sync, `content` is populated from the source file and
/// available in memory until the engine is dropped.
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
    ///
    /// **Note**: This field is empty after loading from disk (compact persistence).
    /// Use [`Engine::get_chunk_content`] to retrieve content from Tantivy.
    /// During indexing/sync, content is populated in memory from the source file.
    pub content: String,
    /// xxh3 hash of the chunk content, used for incremental vector updates.
    /// During sync, chunks whose content hash has not changed can skip
    /// re-embedding, reusing the existing vector.
    #[serde(default)]
    pub content_hash: u64,
}

/// Compact serialization format for [`ChunkMeta`] — excludes the `content`
/// field to dramatically reduce `chunk_meta.bin` size.
///
/// On the Linux kernel (881K chunks), this shrinks the file from ~1.5 GB to
/// ~100-150 MB by not duplicating content already stored in Tantivy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkMetaCompact {
    pub chunk_id: u64,
    pub file_path: String,
    pub language: String,
    pub line_start: u64,
    pub line_end: u64,
    pub signature: String,
    pub scope_chain: Vec<String>,
    pub entity_names: Vec<String>,
    pub content_hash: u64,
}

impl From<&ChunkMeta> for ChunkMetaCompact {
    fn from(meta: &ChunkMeta) -> Self {
        Self {
            chunk_id: meta.chunk_id,
            file_path: meta.file_path.clone(),
            language: meta.language.clone(),
            line_start: meta.line_start,
            line_end: meta.line_end,
            signature: meta.signature.clone(),
            scope_chain: meta.scope_chain.clone(),
            entity_names: meta.entity_names.clone(),
            content_hash: meta.content_hash,
        }
    }
}

impl From<ChunkMeta> for ChunkMetaCompact {
    fn from(meta: ChunkMeta) -> Self {
        Self {
            chunk_id: meta.chunk_id,
            file_path: meta.file_path,
            language: meta.language,
            line_start: meta.line_start,
            line_end: meta.line_end,
            signature: meta.signature,
            scope_chain: meta.scope_chain,
            entity_names: meta.entity_names,
            content_hash: meta.content_hash,
        }
    }
}

impl From<ChunkMetaCompact> for ChunkMeta {
    fn from(compact: ChunkMetaCompact) -> Self {
        Self {
            chunk_id: compact.chunk_id,
            file_path: compact.file_path,
            language: compact.language,
            line_start: compact.line_start,
            line_end: compact.line_end,
            signature: compact.signature,
            scope_chain: compact.scope_chain,
            entity_names: compact.entity_names,
            content: String::new(), // Content retrieved from Tantivy on demand
            content_hash: compact.content_hash,
        }
    }
}

/// Trait for swappable retrieval strategies.
pub trait Retriever: Send + Sync {
    /// Execute a search query and return ranked results.
    fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>>;
}

#[cfg(test)]
mod chunk_meta_conversion_tests {
    use super::{ChunkMeta, ChunkMetaCompact};

    #[test]
    fn consuming_compact_conversion_moves_owned_fields() {
        let meta = ChunkMeta {
            chunk_id: 42,
            file_path: "src/lib.rs".to_string(),
            language: "rust".to_string(),
            line_start: 3,
            line_end: 9,
            signature: "pub fn answer() -> u64".to_string(),
            scope_chain: vec!["crate".to_string(), "module".to_string()],
            entity_names: vec!["answer".to_string(), "Result".to_string()],
            content: "pub fn answer() -> u64 { 42 }".to_string(),
            content_hash: 99,
        };
        let file_path_ptr = meta.file_path.as_ptr();
        let language_ptr = meta.language.as_ptr();
        let signature_ptr = meta.signature.as_ptr();
        let scope_ptr = meta.scope_chain.as_ptr();
        let entity_names_ptr = meta.entity_names.as_ptr();
        let first_entity_ptr = meta.entity_names[0].as_ptr();

        let compact = ChunkMetaCompact::from(meta);

        assert_eq!(compact.file_path.as_ptr(), file_path_ptr);
        assert_eq!(compact.language.as_ptr(), language_ptr);
        assert_eq!(compact.signature.as_ptr(), signature_ptr);
        assert_eq!(compact.scope_chain.as_ptr(), scope_ptr);
        assert_eq!(compact.entity_names.as_ptr(), entity_names_ptr);
        assert_eq!(compact.entity_names[0].as_ptr(), first_entity_ptr);

        let restored = ChunkMeta::from(compact);
        assert_eq!(restored.chunk_id, 42);
        assert_eq!(restored.file_path, "src/lib.rs");
        assert_eq!(restored.language, "rust");
        assert_eq!(restored.line_start, 3);
        assert_eq!(restored.line_end, 9);
        assert_eq!(restored.signature, "pub fn answer() -> u64");
        assert_eq!(restored.scope_chain, ["crate", "module"]);
        assert_eq!(restored.entity_names, ["answer", "Result"]);
        assert!(restored.content.is_empty());
        assert_eq!(restored.content_hash, 99);
    }
}

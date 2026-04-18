//! Codixing core — an AST-aware code-search engine for Rust, Python, JavaScript,
//! TypeScript, Go, Java, C, C++, C#, Ruby, Swift, Kotlin, Scala, Zig, PHP,
//! Matlab, and Bash. The top-level handle is [`engine::Engine`]; see its
//! `init` / `open` / `search` / `sync` methods for the primary entry points.
//!
//! The retrieval stack is hybrid: tree-sitter produces AST-aware chunks
//! (see [`chunker`]), Tantivy provides BM25F text ranking (see [`index`]),
//! a file-level trigram inverted index powers literal/regex exact-match
//! (see [`Engine::grep_code`] and [`GrepOptions`]), and an optional 384-dim
//! vector index backed by BgeSmallEn via ONNX Runtime (see [`vector`] and
//! [`embedder`]) rounds out the hybrid ranker. Results from these retrievers
//! are fused with Reciprocal Rank Fusion and passed through composable
//! [`retriever`] pipeline stages.
//!
//! Beyond text search, the crate maintains a typed dependency graph of
//! import and call edges with PageRank scoring (see [`graph`] and
//! [`graph::CodeGraph`]), including doc-to-code edges from the Markdown and
//! HTML doc indexer. Cross-repo queries are served by the
//! [`federation`] module, which exposes [`federation::FederatedEngine`] for
//! unified search over multiple indexed projects.

pub mod chunker;
pub mod complexity;
pub mod config;
pub mod context_assembly;
pub mod embedder;
pub mod engine;
pub mod error;
pub mod federation;
pub mod filter_pipeline;
pub mod formatter;
pub mod graph;
pub mod index;
pub mod language;
pub mod orphans;
pub mod parser;
pub mod persistence;
pub mod reranker;
pub mod retriever;
pub mod session;
pub mod shared_session;
pub mod symbols;
pub mod temporal;
pub mod test_mapping;
pub mod tokenizer;
pub mod vector;
pub mod watcher;

// Re-export primary public API types.
pub use config::{EmbeddingConfig, EmbeddingModel, GraphConfig, IndexConfig};
pub use engine::freshness::{FreshnessEntry, FreshnessOptions, FreshnessReport, FreshnessTier};
pub use engine::sync::SyncOptions;
pub use engine::{
    ChangeImpact, ConflictKind, EmbedTimingStats, Engine, FocusMapEntry, FocusMapOptions,
    GitSyncStats, GrepMatch, GrepOptions, IndexStats, ReferenceOptions, RenameConflict,
    RenameValidation, StaleReport, SymbolReference, SyncStats,
};
pub use error::{CodixingError, Result};
pub use federation::{
    FederatedEngine, FederatedResult, FederatedStats, FederationConfig, ProjectInfo,
    discover::{DiscoveredProject, ProjectType, discover_projects, to_federation_config},
};
pub use filter_pipeline::{FilterPipeline, FilterResult};
pub use graph::{
    CodeEdge, CodeGraph, CodeNode, CommunityResult, CypherExportOptions, EdgeConfidence, EdgeKind,
    GraphStats, GraphmlExportOptions, HtmlExportOptions, ObsidianExportOptions, RepoMapOptions,
    SurprisingEdge,
};
pub use language::{EntityKind, TypeRelation, TypeRelationKind, Visibility};
pub use orphans::{OrphanConfidence, OrphanFile, OrphanOptions};
pub use retriever::{ChunkMeta, DocFilter, SearchQuery, SearchResult, Strategy};
pub use session::{SessionEvent, SessionEventKind, SessionState};
pub use shared_session::{SharedEventType, SharedSession, SharedSessionEvent};
pub use symbols::Symbol;
pub use temporal::{BlameLine, ChangeEntry, Hotspot};
pub use test_mapping::{TestMapping, TestMappingOptions};
pub use vector::VectorBackend;

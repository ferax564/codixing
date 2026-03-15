pub mod chunker;
pub mod complexity;
pub mod config;
pub mod context_assembly;
pub mod embedder;
pub mod engine;
pub mod error;
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
pub mod symbols;
pub mod temporal;
pub mod test_mapping;
pub mod tokenizer;
pub mod vector;
pub mod watcher;

// Re-export primary public API types.
pub use config::{EmbeddingConfig, EmbeddingModel, GraphConfig, IndexConfig};
pub use engine::{
    Engine, FocusMapEntry, FocusMapOptions, GitSyncStats, GrepMatch, IndexStats, SymbolReference,
    SyncStats,
};
pub use error::{CodixingError, Result};
pub use graph::{CodeEdge, CodeGraph, CodeNode, EdgeKind, GraphStats, RepoMapOptions};
pub use language::EntityKind;
pub use orphans::{OrphanConfidence, OrphanFile, OrphanOptions};
pub use retriever::{ChunkMeta, SearchQuery, SearchResult, Strategy};
pub use session::{SessionEvent, SessionEventKind, SessionState};
pub use symbols::Symbol;
pub use temporal::{BlameLine, ChangeEntry, Hotspot};
pub use test_mapping::{TestMapping, TestMappingOptions};
pub use vector::VectorBackend;

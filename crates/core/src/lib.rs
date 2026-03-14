pub mod chunker;
pub mod complexity;
pub mod config;
pub mod embedder;
pub mod engine;
pub mod error;
pub mod formatter;
pub mod graph;
pub mod index;
pub mod language;
pub mod parser;
pub mod persistence;
pub mod reranker;
pub mod retriever;
pub mod session;
pub mod symbols;
pub mod vector;
pub mod watcher;

// Re-export primary public API types.
pub use config::{EmbeddingConfig, EmbeddingModel, GraphConfig, IndexConfig};
pub use engine::{Engine, GitSyncStats, GrepMatch, IndexStats, SyncStats};
pub use error::{CodixingError, Result};
pub use graph::{CodeEdge, CodeGraph, CodeNode, EdgeKind, GraphStats, RepoMapOptions};
pub use language::EntityKind;
pub use retriever::{ChunkMeta, SearchQuery, SearchResult, Strategy};
pub use session::{SessionEvent, SessionEventKind, SessionState};
pub use symbols::Symbol;
pub use vector::VectorBackend;

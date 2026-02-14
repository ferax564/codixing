pub mod chunker;
pub mod config;
pub mod embeddings;
pub mod engine;
pub mod error;
pub mod graph;
pub mod index;
pub mod language;
pub mod parser;
pub mod persistence;
pub mod retriever;
pub mod symbols;
pub mod tokenizer;
pub mod watcher;

// Re-export primary public API types.
pub use config::IndexConfig;
pub use engine::{Engine, IndexStats};
pub use error::{CodeforgeError, Result};
pub use graph::{CodeGraph, ReferenceKind, SymbolKind, SymbolNode};
pub use retriever::{SearchQuery, SearchResult};
pub use symbols::Symbol;
pub use tokenizer::{ApproxTokenCounter, ContextBudget, ContextSnippet, TokenCounter};

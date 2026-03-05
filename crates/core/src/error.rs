use std::path::PathBuf;

/// All errors produced by the Codixing engine.
#[derive(Debug, thiserror::Error)]
pub enum CodixingError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("unsupported language for file: {path}")]
    UnsupportedLanguage { path: PathBuf },

    #[error("parse error for {path}: {message}")]
    Parse { path: PathBuf, message: String },

    #[error("index error: {0}")]
    Index(String),

    #[error("tantivy error: {0}")]
    Tantivy(#[from] tantivy::TantivyError),

    #[error("query parse error: {0}")]
    QueryParse(#[from] tantivy::query::QueryParserError),

    #[error("config error: {0}")]
    Config(String),

    #[error("index not found at {path} — run `codixing init` first")]
    IndexNotFound { path: PathBuf },

    #[error("watcher error: {0}")]
    Watcher(#[from] notify::Error),

    #[error("embedding error: {0}")]
    Embedding(String),

    #[error("vector index error: {0}")]
    VectorIndex(String),

    #[error(
        "embeddings not enabled — run `codixing init` with embedding enabled, or use --strategy instant"
    )]
    EmbeddingNotEnabled,

    #[error("graph error: {0}")]
    Graph(String),

    #[error("reranker error: {0}")]
    Reranker(String),
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, CodixingError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_display() {
        let err: CodixingError = std::io::Error::new(std::io::ErrorKind::NotFound, "gone").into();
        assert!(err.to_string().contains("I/O error"));
    }

    #[test]
    fn unsupported_language_display() {
        let err = CodixingError::UnsupportedLanguage {
            path: PathBuf::from("foo.xyz"),
        };
        assert!(err.to_string().contains("foo.xyz"));
    }

    #[test]
    fn index_not_found_display() {
        let err = CodixingError::IndexNotFound {
            path: PathBuf::from("/project"),
        };
        assert!(err.to_string().contains("codixing init"));
    }
}

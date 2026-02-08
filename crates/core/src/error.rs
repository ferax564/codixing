use std::path::PathBuf;

/// All errors produced by the CodeForge engine.
#[derive(Debug, thiserror::Error)]
pub enum CodeforgeError {
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

    #[error("index not found at {path} — run `codeforge init` first")]
    IndexNotFound { path: PathBuf },

    #[error("watcher error: {0}")]
    Watcher(#[from] notify::Error),
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, CodeforgeError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_display() {
        let err: CodeforgeError = std::io::Error::new(std::io::ErrorKind::NotFound, "gone").into();
        assert!(err.to_string().contains("I/O error"));
    }

    #[test]
    fn unsupported_language_display() {
        let err = CodeforgeError::UnsupportedLanguage {
            path: PathBuf::from("foo.xyz"),
        };
        assert!(err.to_string().contains("foo.xyz"));
    }

    #[test]
    fn index_not_found_display() {
        let err = CodeforgeError::IndexNotFound {
            path: PathBuf::from("/project"),
        };
        assert!(err.to_string().contains("codeforge init"));
    }
}

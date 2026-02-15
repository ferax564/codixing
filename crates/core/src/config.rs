use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Which vector index backend to use for semantic search.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum VectorBackend {
    /// Choose automatically: HNSW for >= 10 000 chunks, brute-force otherwise.
    #[default]
    Auto,
    /// Exact cosine-similarity scan. Simple, correct; suitable up to ~100K chunks.
    BruteForce,
    /// Approximate nearest-neighbor via HNSW. Sub-linear query time for large corpora.
    Hnsw,
}

/// Configuration for the CodeForge index.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IndexConfig {
    /// Root directory of the project being indexed.
    pub root: PathBuf,

    /// Languages to index (empty = auto-detect all supported).
    #[serde(default)]
    pub languages: HashSet<String>,

    /// Glob patterns to exclude from indexing.
    #[serde(default = "default_exclude_patterns")]
    pub exclude_patterns: Vec<String>,

    /// Chunking configuration.
    #[serde(default)]
    pub chunk: ChunkConfig,

    /// Vector index backend for semantic search.
    #[serde(default)]
    pub vector_backend: VectorBackend,
}

/// Controls the cAST chunking algorithm parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChunkConfig {
    /// Maximum non-whitespace characters per chunk.
    #[serde(default = "default_max_chars")]
    pub max_chars: usize,

    /// Minimum non-whitespace characters per chunk (merge threshold).
    #[serde(default = "default_min_chars")]
    pub min_chars: usize,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            max_chars: default_max_chars(),
            min_chars: default_min_chars(),
        }
    }
}

fn default_max_chars() -> usize {
    1500
}

fn default_min_chars() -> usize {
    200
}

fn default_exclude_patterns() -> Vec<String> {
    vec![
        ".git".into(),
        ".codeforge".into(),
        "target".into(),
        "node_modules".into(),
        ".venv".into(),
        "__pycache__".into(),
        "vendor".into(),
        "dist".into(),
        "build".into(),
    ]
}

impl IndexConfig {
    /// Create a new config rooted at `root` with defaults.
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            languages: HashSet::new(),
            exclude_patterns: default_exclude_patterns(),
            chunk: ChunkConfig::default(),
            vector_backend: VectorBackend::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trip() {
        let config = IndexConfig::new("/tmp/project");
        let json = serde_json::to_string_pretty(&config).unwrap();
        let parsed: IndexConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn bitcode_round_trip() {
        let config = IndexConfig::new("/tmp/project");
        let bytes = bitcode::serialize(&config).unwrap();
        let decoded: IndexConfig = bitcode::deserialize(&bytes).unwrap();
        assert_eq!(config, decoded);
    }

    #[test]
    fn defaults_are_sane() {
        let config = IndexConfig::new(".");
        assert_eq!(config.chunk.max_chars, 1500);
        assert_eq!(config.chunk.min_chars, 200);
        assert!(config.exclude_patterns.contains(&".git".to_string()));
        assert!(config.languages.is_empty());
    }
}

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Configuration for the dependency graph module.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GraphConfig {
    /// Whether to build and maintain a dependency graph.
    #[serde(default = "default_graph_enabled")]
    pub enabled: bool,

    /// Multiplicative PageRank boost weight applied to search results.
    #[serde(default = "default_graph_boost_weight")]
    pub boost_weight: f32,

    /// PageRank damping factor (standard value: 0.85).
    #[serde(default = "default_graph_damping")]
    pub damping: f32,

    /// Maximum PageRank power-iteration steps.
    #[serde(default = "default_graph_iterations")]
    pub iterations: usize,
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self {
            enabled: default_graph_enabled(),
            boost_weight: default_graph_boost_weight(),
            damping: default_graph_damping(),
            iterations: default_graph_iterations(),
        }
    }
}

fn default_graph_enabled() -> bool {
    true
}

fn default_graph_boost_weight() -> f32 {
    0.3
}

fn default_graph_damping() -> f32 {
    0.85
}

fn default_graph_iterations() -> usize {
    20
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

    /// Embedding and vector index configuration.
    #[serde(default)]
    pub embedding: EmbeddingConfig,

    /// Dependency graph configuration.
    #[serde(default)]
    pub graph: GraphConfig,
}

/// Which embedding model to use for vector search.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum EmbeddingModel {
    /// BGE Small English v1.5 — 384 dimensions, kept for backwards compat / fast machines.
    BgeSmallEn,
    /// BGE Base English v1.5 — 768 dimensions, higher quality (new default).
    #[default]
    BgeBaseEn,
}

/// Configuration for the vector embedding pipeline.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EmbeddingConfig {
    /// Whether to generate vector embeddings during indexing.
    #[serde(default = "default_embedding_enabled")]
    pub enabled: bool,

    /// Which embedding model to use.
    #[serde(default)]
    pub model: EmbeddingModel,

    /// Reciprocal Rank Fusion constant (higher = more weight on lower ranks).
    #[serde(default = "default_rrf_k")]
    pub rrf_k: f32,

    /// MMR lambda: 1.0 = pure relevance, 0.0 = pure diversity.
    #[serde(default = "default_mmr_lambda")]
    pub mmr_lambda: f32,

    /// Prepend file path, language, and scope chain to each chunk before
    /// embedding.  Mirrors Sourcegraph Cody's "contextual embeddings" technique
    /// which reduces retrieval failure rate by ~35%.  Enabled by default.
    #[serde(default = "default_contextual_embeddings")]
    pub contextual_embeddings: bool,

    /// Store HNSW vectors as int8 instead of float32.  Reduces memory by 8×
    /// (critical for 3 M+ LoC repos) with negligible recall loss.
    #[serde(default = "default_quantize")]
    pub quantize: bool,

    /// Load the BGE-Reranker-Base cross-encoder model to enable the `deep`
    /// retrieval strategy.  Disabled by default because the model is ~270 MB
    /// and takes ~2 s to load.  Enable with `--reranker` on `codeforge init`
    /// or by setting this field in config.
    #[serde(default)]
    pub reranker_enabled: bool,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            enabled: default_embedding_enabled(),
            model: EmbeddingModel::default(),
            rrf_k: default_rrf_k(),
            mmr_lambda: default_mmr_lambda(),
            contextual_embeddings: default_contextual_embeddings(),
            quantize: default_quantize(),
            reranker_enabled: false,
        }
    }
}

fn default_embedding_enabled() -> bool {
    true
}

fn default_rrf_k() -> f32 {
    60.0
}

fn default_mmr_lambda() -> f32 {
    0.7
}

fn default_contextual_embeddings() -> bool {
    true
}

fn default_quantize() -> bool {
    true
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
            embedding: EmbeddingConfig::default(),
            graph: GraphConfig::default(),
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

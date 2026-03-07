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

/// Configuration for the Codixing index.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IndexConfig {
    /// Root directory of the project being indexed.
    pub root: PathBuf,

    /// Additional root directories to index alongside the primary root.
    /// File paths from extra roots are prefixed with the directory's base name.
    /// Example: extra root `/home/user/shared-lib` → prefix `shared-lib/`
    #[serde(default)]
    pub extra_roots: Vec<PathBuf>,

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
    /// Jina Embeddings v2 Base Code — 768 dimensions, optimised for code/text retrieval.
    /// Trained on code-specific corpora; recommended for pure-code repositories.
    JinaEmbedCode,
    /// BGE Large English v1.5 — 1024 dimensions.
    ///
    /// Larger variant of the BGE family; useful for comparing quality at the 1024d tier
    /// against Snowflake Arctic L.
    BgeLargeEn,
    /// Nomic Embed Code — 768 dimensions, code-specific model.
    ///
    /// Uses `nomic-ai/nomic-embed-code` downloaded from Hugging Face on first use.
    /// **Note:** requires an ONNX export of the model to be available on HuggingFace.
    /// Currently the official repo only ships safetensors; falls back to BM25-only if
    /// the ONNX file is not found.
    NomicEmbedCode,
    /// Snowflake Arctic Embed L — 1024 dimensions, SOTA retrieval at ~335M params.
    ///
    /// Top MTEB score at this size class. Recommended when retrieval quality
    /// matters more than init speed (~3–4× slower than BgeSmallEn).
    SnowflakeArcticEmbedL,
    /// Qwen3-Embedding-0.6B — 1024 dimensions, ONNX Runtime backend.
    ///
    /// Uses `onnx-community/Qwen3-Embedding-0.6B-ONNX` (int8-quantized, ~380 MB)
    /// downloaded from Hugging Face on first use and cached by hf-hub.
    ///
    /// **Requires the `qwen3` Cargo feature** (`--features codixing-core/qwen3`).
    /// Runs via the same ONNX Runtime shared library as the BGE models.
    #[cfg(feature = "qwen3")]
    Qwen3SmallEmbedding,
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
    /// and takes ~2 s to load.  Enable with `--reranker` on `codixing init`
    /// or by setting this field in config.
    #[serde(default)]
    pub reranker_enabled: bool,

    /// Use Qdrant as the vector backend instead of the embedded HNSW index.
    ///
    /// Requires the `qdrant` Cargo feature (`--features qdrant`) and a running
    /// Qdrant instance reachable at `QDRANT_URL` (default `http://localhost:6334`).
    /// When disabled (the default) the local usearch HNSW index is used.
    #[serde(default)]
    pub qdrant_enabled: bool,
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
            qdrant_enabled: false,
        }
    }
}

fn default_embedding_enabled() -> bool {
    false
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
        ".codixing".into(),
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
            extra_roots: Vec::new(),
            languages: HashSet::new(),
            exclude_patterns: default_exclude_patterns(),
            chunk: ChunkConfig::default(),
            embedding: EmbeddingConfig::default(),
            graph: GraphConfig::default(),
        }
    }

    /// Returns an iterator over all roots: primary root first, then extra roots.
    pub fn all_roots(&self) -> impl Iterator<Item = &PathBuf> {
        std::iter::once(&self.root).chain(self.extra_roots.iter())
    }

    /// Given an absolute file path, return the normalized relative string path.
    ///
    /// For files under the primary root, no prefix is added (backwards-compatible).
    /// For files under an extra root, the result is prefixed with the extra root's
    /// directory base name, e.g. `shared-lib/src/types.rs`.
    ///
    /// Returns `None` if the path does not fall under any known root.
    pub fn normalize_path(&self, abs_path: &Path) -> Option<String> {
        // Try primary root first (no prefix — backwards-compatible).
        if let Ok(rel) = abs_path.strip_prefix(&self.root) {
            return Some(rel.to_string_lossy().replace('\\', "/"));
        }
        // Try extra roots (prefix = base name of the extra root directory).
        for extra in &self.extra_roots {
            if let Ok(rel) = abs_path.strip_prefix(extra) {
                let prefix = extra
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| extra.to_string_lossy().into_owned());
                return Some(format!(
                    "{}/{}",
                    prefix,
                    rel.to_string_lossy().replace('\\', "/")
                ));
            }
        }
        None
    }

    /// Resolve a normalized relative path back to an absolute filesystem path.
    ///
    /// Tries the primary root first, then each extra root (stripping the prefix).
    /// Returns `None` if the path cannot be mapped to any root.
    pub fn resolve_path(&self, rel_path: &str) -> Option<PathBuf> {
        // Try primary root directly.
        let primary_abs = self.root.join(rel_path);
        if primary_abs.exists() {
            return Some(primary_abs);
        }
        // Try extra roots: strip the prefix (base name) and check.
        for extra in &self.extra_roots {
            let prefix = extra
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| extra.to_string_lossy().into_owned());
            let with_slash = format!("{}/", prefix);
            if let Some(stripped) = rel_path.strip_prefix(&with_slash) {
                let candidate = extra.join(stripped);
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }
        None
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

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

/// BM25 field boost weights for Tantivy queries.
///
/// Controls how much each indexed field contributes to BM25 relevance scoring.
/// Higher values make matches in that field rank higher. The `content` field
/// always has an implicit boost of 1.0 (the Tantivy default).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Bm25Config {
    /// Boost for matches in `entity_names` (symbol names like function/struct/class names).
    #[serde(default = "default_bm25_entity_names_boost")]
    pub entity_names_boost: f32,

    /// Boost for matches in `signature` (full function/method signatures).
    #[serde(default = "default_bm25_signature_boost")]
    pub signature_boost: f32,

    /// Boost for matches in `scope_chain` (module/class/namespace path).
    #[serde(default = "default_bm25_scope_chain_boost")]
    pub scope_chain_boost: f32,

    /// Boost for matches in `content` (raw source code).
    #[serde(default = "default_bm25_content_boost")]
    pub content_boost: f32,

    /// Boost for matches in `doc_comment` (extracted doc comments, stemmed).
    #[serde(default = "default_bm25_doc_comment_boost")]
    pub doc_comment_boost: f32,

    /// Boost for matches in `identifier_words` (split entity names, stemmed).
    #[serde(default = "default_bm25_identifier_words_boost")]
    pub identifier_words_boost: f32,

    /// Boost for matches in `path_segments` (directory/filename tokens, stemmed).
    #[serde(default = "default_bm25_path_segments_boost")]
    pub path_segments_boost: f32,
}

impl Default for Bm25Config {
    fn default() -> Self {
        Self {
            entity_names_boost: default_bm25_entity_names_boost(),
            signature_boost: default_bm25_signature_boost(),
            scope_chain_boost: default_bm25_scope_chain_boost(),
            content_boost: default_bm25_content_boost(),
            doc_comment_boost: default_bm25_doc_comment_boost(),
            identifier_words_boost: default_bm25_identifier_words_boost(),
            path_segments_boost: default_bm25_path_segments_boost(),
        }
    }
}

fn default_bm25_entity_names_boost() -> f32 {
    3.0
}

fn default_bm25_signature_boost() -> f32 {
    2.0
}

fn default_bm25_scope_chain_boost() -> f32 {
    1.5
}

fn default_bm25_content_boost() -> f32 {
    1.0
}

fn default_bm25_doc_comment_boost() -> f32 {
    2.0
}

fn default_bm25_identifier_words_boost() -> f32 {
    2.0
}

fn default_bm25_path_segments_boost() -> f32 {
    1.5
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

    /// BM25 field boost weights.
    #[serde(default)]
    pub bm25: Bm25Config,
}

/// Which embedding model to use for vector search.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum EmbeddingModel {
    /// BGE Small English v1.5 — 384 dimensions, kept for backwards compat / fast machines.
    BgeSmallEn,
    /// BGE Small English v1.5 Quantized — 384 dims, int8 dynamic quantization.
    BgeSmallEnQ,
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
    /// Snowflake Arctic Embed XS Quantized — 384 dims, 22M params, int8.
    SnowflakeArcticEmbedXSQ,
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
    /// Model2Vec static embeddings — 256 dimensions, ~40ms init, no ONNX.
    ///
    /// Uses `minishlab/potion-base-8M` from HuggingFace. Computes document
    /// embeddings as the mean of static token vectors (a lookup table, not
    /// a neural network). Enables hybrid search on any machine without GPU
    /// or ONNX Runtime.
    Model2Vec,
    /// Model2Vec retrieval-optimized — 512 dimensions, ~60ms init, no ONNX.
    ///
    /// Uses `minishlab/potion-retrieval-32M` from HuggingFace. Same static
    /// lookup architecture as `Model2Vec` but with 2× dimensions and training
    /// optimized for retrieval tasks (+13% MTEB retrieval vs potion-base-8M).
    Model2VecRetrieval,
    /// Jina Code Int8 — 768 dimensions, ~8ms on ARM64, ONNX Runtime.
    ///
    /// Uses `jinaai/jina-embeddings-v2-base-code` (int8 quantized for ARM64).
    /// Code-specific transformer with 150 emb/s throughput, nDCG@10 0.949.
    /// Set `JINA_CODE_INT8_ONNX` to the path of `model_qint8_arm64.onnx`.
    JinaCodeInt8,
    /// Model2Vec distilled from Jina Code — 256 dims, static lookup, no ONNX.
    ///
    /// Distilled from `jinaai/jina-embeddings-v2-base-code` via Model2Vec.
    /// Uses BPE tokenizer that handles CamelCase natively (no preprocessing
    /// needed). Set `MODEL2VEC_JINA_CODE_DIR` to the model directory.
    Model2VecJinaCode,
}

/// Which vector storage backend to use for the brute-force / trait-based index.
///
/// This controls whether the [`BruteForceVectorIndex`](crate::index::BruteForceVectorIndex)
/// vectors are kept fully in RAM or backed by a memory-mapped file.
///
/// The default is `InMemory`. Use `Mmap` for large repositories (>1000 chunks)
/// to reduce resident memory usage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum VectorStorageBackend {
    /// All vectors are loaded into process memory (default).
    #[default]
    InMemory,
    /// Vectors are stored in a memory-mapped file (`.codixing/vectors.mmap`).
    /// Only pages accessed during search are loaded into physical RAM.
    Mmap,
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

    /// Which storage backend to use for the brute-force vector index.
    ///
    /// `InMemory` (default) keeps all vectors in RAM. `Mmap` uses a memory-mapped
    /// file to reduce RSS for large repositories.
    #[serde(default)]
    pub vector_storage_backend: VectorStorageBackend,
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
            vector_storage_backend: VectorStorageBackend::default(),
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

    /// Fraction of overlap between adjacent chunks (0.0 = no overlap, 0.3 = 30%).
    /// When > 0, bridge chunks are generated at each chunk boundary.
    #[serde(default)]
    pub overlap_ratio: f32,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            max_chars: default_max_chars(),
            min_chars: default_min_chars(),
            overlap_ratio: 0.0,
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
            bm25: Bm25Config::default(),
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
        if let Some(rel) = self.normalize_path_exact(abs_path) {
            return Some(rel);
        }
        // The roots are canonicalized at Engine construction, but callers
        // (CLI `update --file`, MCP/LSP writes, tests) may pass a
        // non-canonical path — e.g. through a symlinked project dir, or
        // macOS `/var` vs `/private/var`. Retry with the canonical form
        // before giving up. Canonicalization fails for deleted files; those
        // keep the exact-match-only behavior.
        let canonical = abs_path.canonicalize().ok()?;
        if canonical != abs_path {
            self.normalize_path_exact(&canonical)
        } else {
            None
        }
    }

    fn normalize_path_exact(&self, abs_path: &Path) -> Option<String> {
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
    /// Returns `None` if the path is absolute, contains a parent traversal, does
    /// not exist, or resolves through a symlink outside its selected root.
    pub fn resolve_path(&self, rel_path: &str) -> Option<PathBuf> {
        let rel = Path::new(rel_path);
        if !safe_relative_path(rel) {
            return None;
        }

        // Try primary root directly.
        if let Some(primary_abs) = resolve_within(&self.root, rel, false) {
            return Some(primary_abs);
        }

        // Try extra roots: strip the prefix (base name) and check.
        for extra in &self.extra_roots {
            let prefix = extra
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| extra.to_string_lossy().into_owned());
            let with_slash = format!("{}/", prefix);
            if let Some(stripped) = rel_path.strip_prefix(&with_slash)
                && let Some(candidate) = resolve_within(extra, Path::new(stripped), false)
            {
                return Some(candidate);
            }
        }
        None
    }

    /// Resolve a caller-supplied relative path for a filesystem mutation.
    ///
    /// Unlike [`Self::resolve_path`], the final component may not exist yet.
    /// Every existing prefix is canonicalized, including dangling symlinks,
    /// before the missing suffix is appended. This prevents an in-root symlink
    /// from redirecting a create or overwrite outside the configured roots.
    pub fn resolve_path_for_write(&self, rel_path: &str) -> Option<PathBuf> {
        let rel = Path::new(rel_path);
        if !safe_relative_path(rel) {
            return None;
        }

        let primary_candidate = self.root.join(rel);
        if std::fs::symlink_metadata(&primary_candidate).is_ok() {
            return resolve_within(&self.root, rel, true);
        }

        let first_component = rel.components().find_map(|component| match component {
            std::path::Component::Normal(component) => Some(component),
            _ => None,
        });
        let primary_claims_prefix = first_component
            .map(|component| self.root.join(component))
            .is_some_and(|path| std::fs::symlink_metadata(path).is_ok());

        for extra in &self.extra_roots {
            let prefix = extra
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| extra.to_string_lossy().into_owned());
            let stripped = if rel_path == prefix {
                Some("")
            } else {
                rel_path.strip_prefix(&format!("{prefix}/"))
            };
            let Some(stripped) = stripped else {
                continue;
            };
            let extra_rel = Path::new(stripped);
            let extra_candidate = extra.join(extra_rel);
            if std::fs::symlink_metadata(&extra_candidate).is_ok() || !primary_claims_prefix {
                return resolve_within(extra, extra_rel, true);
            }
        }

        resolve_within(&self.root, rel, true)
    }

    /// Resolve an absolute caller-supplied path inside any configured root.
    ///
    /// `allow_missing` is used by deletion/index-maintenance paths whose target
    /// may already be gone. Missing suffixes are accepted only after the nearest
    /// existing ancestor has been canonicalized and proven contained.
    pub fn resolve_absolute_path(&self, path: &Path, allow_missing: bool) -> Option<PathBuf> {
        if !path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return None;
        }
        resolve_absolute_with_roots(self.all_roots().map(PathBuf::as_path), path, allow_missing)
    }
}

fn safe_relative_path(path: &Path) -> bool {
    !path.is_absolute()
        && !path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
}

fn resolve_within(root: &Path, rel: &Path, allow_missing: bool) -> Option<PathBuf> {
    let candidate = root.join(rel);
    resolve_absolute_with_roots(std::iter::once(root), &candidate, allow_missing)
}

fn resolve_absolute_with_roots<'a>(
    roots: impl Iterator<Item = &'a Path>,
    candidate: &Path,
    allow_missing: bool,
) -> Option<PathBuf> {
    let canonical_roots: Vec<PathBuf> = roots.filter_map(|root| root.canonicalize().ok()).collect();
    if canonical_roots.is_empty() {
        return None;
    }

    let mut existing = candidate.to_path_buf();
    let mut missing_suffix = Vec::new();
    loop {
        match std::fs::symlink_metadata(&existing) {
            Ok(_) => break,
            Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
                let name = existing.file_name()?.to_os_string();
                missing_suffix.push(name);
                if !existing.pop() {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }

    // `canonicalize` intentionally follows the existing component. A dangling
    // symlink therefore fails closed instead of being mistaken for a missing
    // ordinary path whose target could be created outside the root.
    let mut resolved = existing.canonicalize().ok()?;
    if !canonical_roots
        .iter()
        .any(|canonical_root| resolved.starts_with(canonical_root))
    {
        return None;
    }
    for component in missing_suffix.iter().rev() {
        resolved.push(component);
    }
    Some(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

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

    #[test]
    fn chunk_config_default_overlap_ratio_is_zero() {
        let config = ChunkConfig::default();
        assert_eq!(config.overlap_ratio, 0.0);
    }

    #[test]
    fn resolve_path_rejects_absolute_and_parent_escape() {
        let parent = tempdir().unwrap();
        let root = parent.path().join("project");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("inside.rs"), "fn inside() {}\n").unwrap();
        let outside = parent.path().join("secret.txt");
        fs::write(&outside, "secret\n").unwrap();

        let config = IndexConfig::new(root.canonicalize().unwrap());
        assert_eq!(
            config.resolve_path("inside.rs"),
            Some(root.join("inside.rs").canonicalize().unwrap())
        );
        assert_eq!(config.resolve_path("../secret.txt"), None);
        assert_eq!(config.resolve_path(outside.to_str().unwrap()), None);
    }

    #[test]
    fn resolve_path_preserves_prefixed_extra_roots() {
        let parent = tempdir().unwrap();
        let root = parent.path().join("project");
        let extra = parent.path().join("shared-lib");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&extra).unwrap();
        fs::write(extra.join("shared.rs"), "fn shared() {}\n").unwrap();

        let mut config = IndexConfig::new(root.canonicalize().unwrap());
        config.extra_roots.push(extra.canonicalize().unwrap());
        assert_eq!(
            config.resolve_path("shared-lib/shared.rs"),
            Some(extra.join("shared.rs").canonicalize().unwrap())
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_path_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let parent = tempdir().unwrap();
        let root = parent.path().join("project");
        fs::create_dir(&root).unwrap();
        let outside = parent.path().join("secret.txt");
        fs::write(&outside, "secret\n").unwrap();
        symlink(&outside, root.join("linked-secret")).unwrap();

        let config = IndexConfig::new(root.canonicalize().unwrap());
        assert_eq!(config.resolve_path("linked-secret"), None);
    }

    #[cfg(unix)]
    #[test]
    fn write_resolution_rejects_existing_and_dangling_symlink_escapes() {
        use std::os::unix::fs::symlink;

        let parent = tempdir().unwrap();
        let root = parent.path().join("project");
        let outside_dir = parent.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside_dir).unwrap();
        fs::write(outside_dir.join("secret.rs"), "secret\n").unwrap();
        symlink(&outside_dir, root.join("outside-link")).unwrap();
        symlink(outside_dir.join("not-created.rs"), root.join("dangling.rs")).unwrap();

        let config = IndexConfig::new(root.canonicalize().unwrap());
        assert_eq!(
            config.resolve_path_for_write("outside-link/secret.rs"),
            None
        );
        assert_eq!(config.resolve_path_for_write("dangling.rs"), None);
        assert_eq!(
            config.resolve_path_for_write("src/new.rs"),
            Some(root.canonicalize().unwrap().join("src/new.rs"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn absolute_resolution_rejects_missing_child_below_symlink_escape() {
        use std::os::unix::fs::symlink;

        let parent = tempdir().unwrap();
        let root = parent.path().join("project");
        let outside = parent.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        symlink(&outside, root.join("escape")).unwrap();

        let config = IndexConfig::new(root.canonicalize().unwrap());
        assert_eq!(
            config.resolve_absolute_path(&root.join("escape/new.rs"), true),
            None
        );
        assert_eq!(
            config.resolve_absolute_path(&root.join("inside/new.rs"), true),
            Some(root.canonicalize().unwrap().join("inside/new.rs"))
        );
    }
}

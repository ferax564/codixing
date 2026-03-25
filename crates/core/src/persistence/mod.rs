use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::config::IndexConfig;
use crate::error::{CodixingError, Result};
use crate::graph::GraphData;

/// Directory name for the Codixing index store.
const CODEFORGE_DIR: &str = ".codixing";
const CONFIG_FILE: &str = "config.json";
const META_FILE: &str = "meta.json";
const TANTIVY_DIR: &str = "tantivy";
const SYMBOLS_FILE: &str = "symbols.bin";
const TREE_HASHES_FILE: &str = "tree_hashes.bin";
const TREE_HASHES_V2_FILE: &str = "tree_hashes_v2.bin";
const VECTORS_DIR: &str = "vectors";
const VECTOR_INDEX_FILE: &str = "index.usearch";
const FILE_CHUNKS_FILE: &str = "file_chunks.bin";
const CHUNK_META_FILE: &str = "chunk_meta.bin";
const GRAPH_DIR: &str = "graph";
const GRAPH_FILE: &str = "graph.bin";
const MMAP_VECTOR_FILE: &str = "vectors.mmap";
const FILE_TRIGRAM_FILE: &str = "file_trigram.bin";
const CHUNK_TRIGRAM_FILE: &str = "chunk_trigram.bin";

/// Extended file hash entry storing content hash alongside filesystem metadata
/// (mtime and size) for fast pre-filtering during sync.
///
/// During `sync()`, if both `mtime` and `size` match the cached values, the
/// file is assumed unchanged and the expensive xxh3 content hash is skipped.
/// This eliminates ~95% of file reads on a typical sync where few files changed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileHashEntry {
    /// xxh3-64 hash of the file contents.
    pub content_hash: u64,
    /// Last modification time (seconds since UNIX epoch + nanos).
    /// Stored as `(secs, nanos)` for bitcode compatibility since `SystemTime`
    /// is not directly serializable.
    pub mtime_secs: u64,
    pub mtime_nanos: u32,
    /// File size in bytes.
    pub size: u64,
}

impl FileHashEntry {
    /// Create a new entry from a content hash and filesystem metadata.
    pub fn new(content_hash: u64, mtime: Option<SystemTime>, size: u64) -> Self {
        let (secs, nanos) = mtime
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| (d.as_secs(), d.subsec_nanos()))
            .unwrap_or((0, 0));
        Self {
            content_hash,
            mtime_secs: secs,
            mtime_nanos: nanos,
            size,
        }
    }

    /// Reconstruct the `SystemTime` from stored seconds/nanos.
    pub fn mtime(&self) -> Option<SystemTime> {
        if self.mtime_secs == 0 && self.mtime_nanos == 0 {
            return None;
        }
        Some(SystemTime::UNIX_EPOCH + std::time::Duration::new(self.mtime_secs, self.mtime_nanos))
    }

    /// Quick check: does the file's current mtime+size match cached values?
    /// Returns `true` if the file might have changed (needs content hash).
    pub fn file_might_have_changed(
        &self,
        current_mtime: Option<SystemTime>,
        current_size: u64,
    ) -> bool {
        if current_size != self.size {
            return true;
        }
        match (self.mtime(), current_mtime) {
            (Some(cached), Some(current)) => cached != current,
            _ => true, // if we can't compare, assume changed
        }
    }
}

/// Index metadata persisted alongside the index.
///
/// Tracks version, counts, and the last indexing timestamp.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IndexMeta {
    /// Version of the index format.
    pub version: String,
    /// Number of files indexed.
    pub file_count: usize,
    /// Number of code chunks produced.
    pub chunk_count: usize,
    /// Number of symbols extracted.
    pub symbol_count: usize,
    /// ISO 8601 timestamp of the last indexing run.
    pub last_indexed: String,
    /// Git commit hash recorded at last index build/sync.
    ///
    /// `None` if git is unavailable or the project is not in a git repo.
    /// Used by [`Engine::git_sync`] to compute the minimal diff since the
    /// last indexed commit, enabling sub-second re-opens after `git pull`.
    #[serde(default)]
    pub git_commit: Option<String>,
}

impl Default for IndexMeta {
    fn default() -> Self {
        Self {
            version: "0.1.0".to_string(),
            file_count: 0,
            chunk_count: 0,
            symbol_count: 0,
            last_indexed: String::new(),
            git_commit: None,
        }
    }
}

/// Manages the `.codixing/` directory layout on disk.
///
/// Provides creation, opening, and persistence of the index configuration,
/// metadata, symbol tables, and tree hashes.
#[derive(Debug)]
pub struct IndexStore {
    root: PathBuf,
}

impl IndexStore {
    /// Initialize a new `.codixing/` directory structure at the given root.
    ///
    /// Creates the directory layout and writes default config and metadata files.
    /// Returns an error if the directory already exists.
    pub fn init(root: &Path, config: &IndexConfig) -> Result<Self> {
        let codixing_dir = root.join(CODEFORGE_DIR);
        fs::create_dir_all(&codixing_dir)?;
        fs::create_dir_all(codixing_dir.join(TANTIVY_DIR))?;
        fs::create_dir_all(codixing_dir.join(VECTORS_DIR))?;
        fs::create_dir_all(codixing_dir.join(GRAPH_DIR))?;

        let store = Self {
            root: root.to_path_buf(),
        };

        store.save_config(config)?;
        store.save_meta(&IndexMeta::default())?;

        Ok(store)
    }

    /// Open an existing `.codixing/` directory.
    ///
    /// Returns [`CodixingError::IndexNotFound`] if the directory does not exist.
    pub fn open(root: &Path) -> Result<Self> {
        let codixing_dir = root.join(CODEFORGE_DIR);
        if !codixing_dir.is_dir() {
            return Err(CodixingError::IndexNotFound {
                path: root.to_path_buf(),
            });
        }
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    /// Check if a `.codixing/` directory exists at root.
    pub fn exists(root: &Path) -> bool {
        root.join(CODEFORGE_DIR).is_dir()
    }

    /// Return the project root path.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path to the `.codixing/` directory.
    pub fn codixing_dir(&self) -> PathBuf {
        self.root.join(CODEFORGE_DIR)
    }

    /// Path to the tantivy index directory.
    pub fn tantivy_dir(&self) -> PathBuf {
        self.codixing_dir().join(TANTIVY_DIR)
    }

    /// Path to the `symbols.bin` file.
    pub fn symbols_path(&self) -> PathBuf {
        self.codixing_dir().join(SYMBOLS_FILE)
    }

    /// Path to the `tree_hashes.bin` file (legacy v1 format).
    pub fn tree_hashes_path(&self) -> PathBuf {
        self.codixing_dir().join(TREE_HASHES_FILE)
    }

    /// Path to the `tree_hashes_v2.bin` file (extended format with mtime+size).
    pub fn tree_hashes_v2_path(&self) -> PathBuf {
        self.codixing_dir().join(TREE_HASHES_V2_FILE)
    }

    /// Path to the `vectors/` sub-directory.
    pub fn vectors_dir(&self) -> PathBuf {
        self.codixing_dir().join(VECTORS_DIR)
    }

    /// Path to the usearch HNSW index binary.
    pub fn vector_index_path(&self) -> PathBuf {
        self.vectors_dir().join(VECTOR_INDEX_FILE)
    }

    /// Path to the file-chunks map binary.
    pub fn file_chunks_path(&self) -> PathBuf {
        self.vectors_dir().join(FILE_CHUNKS_FILE)
    }

    /// Path to the chunk metadata binary.
    pub fn chunk_meta_path(&self) -> PathBuf {
        self.codixing_dir().join(CHUNK_META_FILE)
    }

    /// Path to the memory-mapped vector index file (`vectors.mmap`).
    pub fn mmap_vector_path(&self) -> PathBuf {
        self.codixing_dir().join(MMAP_VECTOR_FILE)
    }

    /// Path to the file-level trigram index.
    pub fn file_trigram_path(&self) -> PathBuf {
        self.codixing_dir().join(FILE_TRIGRAM_FILE)
    }

    /// Path to the chunk-level trigram index (Strategy::Exact fast-path).
    pub fn chunk_trigram_path(&self) -> PathBuf {
        self.codixing_dir().join(CHUNK_TRIGRAM_FILE)
    }

    /// Save the [`IndexConfig`] to `config.json`.
    pub fn save_config(&self, config: &IndexConfig) -> Result<()> {
        let path = self.codixing_dir().join(CONFIG_FILE);
        let json = serde_json::to_string_pretty(config).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize config: {e}"))
        })?;
        fs::write(&path, json)?;
        Ok(())
    }

    /// Load the [`IndexConfig`] from `config.json`.
    pub fn load_config(&self) -> Result<IndexConfig> {
        let path = self.codixing_dir().join(CONFIG_FILE);
        let json = fs::read_to_string(&path)?;
        let config: IndexConfig = serde_json::from_str(&json).map_err(|e| {
            CodixingError::Serialization(format!("failed to deserialize config: {e}"))
        })?;
        Ok(config)
    }

    /// Save the index metadata to `meta.json`.
    pub fn save_meta(&self, meta: &IndexMeta) -> Result<()> {
        let path = self.codixing_dir().join(META_FILE);
        let json = serde_json::to_string_pretty(meta)
            .map_err(|e| CodixingError::Serialization(format!("failed to serialize meta: {e}")))?;
        fs::write(&path, json)?;
        Ok(())
    }

    /// Load the index metadata from `meta.json`.
    pub fn load_meta(&self) -> Result<IndexMeta> {
        let path = self.codixing_dir().join(META_FILE);
        let json = fs::read_to_string(&path)?;
        let meta: IndexMeta = serde_json::from_str(&json).map_err(|e| {
            CodixingError::Serialization(format!("failed to deserialize meta: {e}"))
        })?;
        Ok(meta)
    }

    /// Path to the `graph/` sub-directory.
    pub fn graph_dir(&self) -> PathBuf {
        self.codixing_dir().join(GRAPH_DIR)
    }

    /// Path to the `graph/graph.bin` file.
    pub fn graph_path(&self) -> PathBuf {
        self.graph_dir().join(GRAPH_FILE)
    }

    /// Serialize and persist the dependency graph.
    pub fn save_graph(&self, data: &GraphData) -> Result<()> {
        // Ensure the directory exists (may not on older indexes opened before Phase 3).
        fs::create_dir_all(self.graph_dir())?;
        let bytes = bitcode::serialize(data)
            .map_err(|e| CodixingError::Serialization(format!("failed to serialize graph: {e}")))?;
        fs::write(self.graph_path(), bytes)?;
        Ok(())
    }

    /// Load the dependency graph from disk.  Returns `None` if no graph has been saved yet.
    pub fn load_graph(&self) -> Result<Option<GraphData>> {
        let path = self.graph_path();
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)?;
        let data: GraphData = bitcode::deserialize(&bytes).map_err(|e| {
            CodixingError::Serialization(format!("failed to deserialize graph: {e}"))
        })?;
        Ok(Some(data))
    }

    /// Path to the `graph/symbol_graph.bin` file for the symbol-level graph.
    pub fn symbol_graph_path(&self) -> PathBuf {
        self.graph_dir().join("symbol_graph.bin")
    }

    /// Persist the symbol-level graph to disk via binary serialization.
    pub fn save_symbol_graph(&self, graph: &crate::graph::CodeGraph) -> Result<()> {
        fs::create_dir_all(self.graph_dir())?;
        crate::graph::persistence::save_graph_binary(graph, &self.symbol_graph_path())
    }

    /// Load the symbol-level graph from disk.
    pub fn load_symbol_graph(&self) -> Result<Option<crate::graph::CodeGraph>> {
        let path = self.symbol_graph_path();
        if !path.exists() {
            return Ok(None);
        }
        crate::graph::persistence::load_graph_binary(&path).map(Some)
    }

    /// Save raw bytes to the `symbols.bin` file.
    pub fn save_symbols_bytes(&self, bytes: &[u8]) -> Result<()> {
        fs::write(self.symbols_path(), bytes)?;
        Ok(())
    }

    /// Load raw bytes from the `symbols.bin` file.
    pub fn load_symbols_bytes(&self) -> Result<Vec<u8>> {
        let bytes = fs::read(self.symbols_path())?;
        Ok(bytes)
    }

    /// Save tree hashes (bitcode-serialized `Vec<(PathBuf, u64)>`).
    pub fn save_tree_hashes(&self, hashes: &[(PathBuf, u64)]) -> Result<()> {
        let bytes = bitcode::serialize(hashes).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize tree hashes: {e}"))
        })?;
        fs::write(self.tree_hashes_path(), bytes)?;
        Ok(())
    }

    /// Load tree hashes from `tree_hashes.bin`.
    pub fn load_tree_hashes(&self) -> Result<Vec<(PathBuf, u64)>> {
        let bytes = fs::read(self.tree_hashes_path())?;
        let hashes: Vec<(PathBuf, u64)> = bitcode::deserialize(&bytes).map_err(|e| {
            CodixingError::Serialization(format!("failed to deserialize tree hashes: {e}"))
        })?;
        Ok(hashes)
    }

    /// Save extended tree hashes (v2 format with mtime+size) to `tree_hashes_v2.bin`.
    pub fn save_tree_hashes_v2(&self, hashes: &[(PathBuf, FileHashEntry)]) -> Result<()> {
        let bytes = bitcode::serialize(hashes).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize tree hashes v2: {e}"))
        })?;
        fs::write(self.tree_hashes_v2_path(), bytes)?;
        Ok(())
    }

    /// Load extended tree hashes (v2 format) from `tree_hashes_v2.bin`.
    ///
    /// Falls back to the legacy v1 format if v2 does not exist, converting
    /// entries to `FileHashEntry` with zeroed mtime/size (will trigger a
    /// full content-hash check on the first sync, then v2 is written).
    pub fn load_tree_hashes_v2(&self) -> Result<Vec<(PathBuf, FileHashEntry)>> {
        let v2_path = self.tree_hashes_v2_path();
        if v2_path.exists() {
            let bytes = fs::read(&v2_path)?;
            let hashes: Vec<(PathBuf, FileHashEntry)> =
                bitcode::deserialize(&bytes).map_err(|e| {
                    CodixingError::Serialization(format!(
                        "failed to deserialize tree hashes v2: {e}"
                    ))
                })?;
            return Ok(hashes);
        }

        // Fall back to v1 and upconvert.
        let v1 = self.load_tree_hashes().unwrap_or_default();
        Ok(v1
            .into_iter()
            .map(|(path, hash)| {
                (
                    path,
                    FileHashEntry {
                        content_hash: hash,
                        mtime_secs: 0,
                        mtime_nanos: 0,
                        size: 0,
                    },
                )
            })
            .collect())
    }

    /// Save the chunk metadata map (bitcode-serialized `Vec<(u64, ChunkMeta)>`).
    ///
    /// Accepts a flat list of `(chunk_id, meta)` pairs rather than the DashMap
    /// directly to avoid depending on DashMap in persistence.
    pub fn save_chunk_meta_bytes(&self, bytes: &[u8]) -> Result<()> {
        fs::write(self.chunk_meta_path(), bytes)?;
        Ok(())
    }

    /// Load raw bytes from `chunk_meta.bin`.
    pub fn load_chunk_meta_bytes(&self) -> Result<Vec<u8>> {
        let bytes = fs::read(self.chunk_meta_path())?;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IndexConfig;
    use tempfile::tempdir;

    #[test]
    fn init_creates_directory_structure() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        let store = IndexStore::init(root, &config).unwrap();

        assert!(store.codixing_dir().is_dir());
        assert!(store.tantivy_dir().is_dir());
        assert!(store.codixing_dir().join(CONFIG_FILE).is_file());
        assert!(store.codixing_dir().join(META_FILE).is_file());
        assert!(IndexStore::exists(root));
    }

    #[test]
    fn open_existing_store_succeeds() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        IndexStore::init(root, &config).unwrap();
        let store = IndexStore::open(root).unwrap();

        assert!(store.codixing_dir().is_dir());
    }

    #[test]
    fn open_nonexistent_returns_index_not_found() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let err = IndexStore::open(root).unwrap_err();
        assert!(
            matches!(err, CodixingError::IndexNotFound { .. }),
            "expected IndexNotFound, got: {err}"
        );
    }

    #[test]
    fn config_round_trip() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        let store = IndexStore::init(root, &config).unwrap();

        let loaded = store.load_config().unwrap();
        assert_eq!(config, loaded);

        // Modify and round-trip again
        let mut updated = loaded;
        updated.languages.insert("rust".to_string());
        store.save_config(&updated).unwrap();
        let reloaded = store.load_config().unwrap();
        assert_eq!(updated, reloaded);
    }

    #[test]
    fn meta_round_trip() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        let store = IndexStore::init(root, &config).unwrap();

        // Load the default meta written by init
        let default_meta = store.load_meta().unwrap();
        assert_eq!(default_meta.version, "0.1.0");
        assert_eq!(default_meta.file_count, 0);

        // Update and round-trip
        let meta = IndexMeta {
            version: "0.1.0".to_string(),
            file_count: 42,
            chunk_count: 128,
            symbol_count: 500,
            last_indexed: "2026-02-07T12:00:00Z".to_string(),
            git_commit: Some("abc123def456".to_string()),
        };
        store.save_meta(&meta).unwrap();
        let loaded = store.load_meta().unwrap();
        assert_eq!(meta, loaded);
    }

    #[test]
    fn tree_hashes_round_trip() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        let store = IndexStore::init(root, &config).unwrap();

        let hashes = vec![
            (PathBuf::from("src/main.rs"), 0xDEADBEEF_u64),
            (PathBuf::from("src/lib.rs"), 0xCAFEBABE_u64),
            (PathBuf::from("tests/integration.rs"), 0x12345678_u64),
        ];

        store.save_tree_hashes(&hashes).unwrap();
        let loaded = store.load_tree_hashes().unwrap();
        assert_eq!(hashes, loaded);
    }

    #[test]
    fn symbols_bytes_round_trip() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        let store = IndexStore::init(root, &config).unwrap();

        let data = vec![1_u8, 2, 3, 4, 5, 42, 255, 0];
        store.save_symbols_bytes(&data).unwrap();
        let loaded = store.load_symbols_bytes().unwrap();
        assert_eq!(data, loaded);
    }

    #[test]
    fn exists_returns_false_for_no_index() {
        let dir = tempdir().unwrap();
        assert!(!IndexStore::exists(dir.path()));
    }

    #[test]
    fn path_accessors_are_consistent() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        let store = IndexStore::init(root, &config).unwrap();

        assert_eq!(store.codixing_dir(), root.join(".codixing"));
        assert_eq!(store.tantivy_dir(), root.join(".codixing/tantivy"));
        assert_eq!(store.symbols_path(), root.join(".codixing/symbols.bin"));
        assert_eq!(
            store.tree_hashes_path(),
            root.join(".codixing/tree_hashes.bin")
        );
    }

    #[test]
    fn file_hash_entry_round_trip() {
        let now = SystemTime::now();
        let entry = FileHashEntry::new(0xDEADBEEF, Some(now), 1024);

        // Check that mtime round-trips correctly.
        let recovered = entry.mtime().unwrap();
        let now_duration = now.duration_since(SystemTime::UNIX_EPOCH).unwrap();
        let rec_duration = recovered.duration_since(SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(now_duration.as_secs(), rec_duration.as_secs());
        assert_eq!(now_duration.subsec_nanos(), rec_duration.subsec_nanos());

        assert_eq!(entry.content_hash, 0xDEADBEEF);
        assert_eq!(entry.size, 1024);
    }

    #[test]
    fn file_hash_entry_unchanged_detection() {
        let now = SystemTime::now();
        let entry = FileHashEntry::new(0xCAFE, Some(now), 512);

        // Same mtime+size → not changed.
        assert!(!entry.file_might_have_changed(Some(now), 512));

        // Different size → changed.
        assert!(entry.file_might_have_changed(Some(now), 999));

        // Different mtime → changed.
        let later = now + std::time::Duration::from_secs(1);
        assert!(entry.file_might_have_changed(Some(later), 512));

        // No mtime → changed (conservative).
        assert!(entry.file_might_have_changed(None, 512));
    }

    #[test]
    fn tree_hashes_v2_round_trip() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        let store = IndexStore::init(root, &config).unwrap();

        let now = SystemTime::now();
        let hashes = vec![
            (
                PathBuf::from("src/main.rs"),
                FileHashEntry::new(0xDEADBEEF, Some(now), 1024),
            ),
            (
                PathBuf::from("src/lib.rs"),
                FileHashEntry::new(0xCAFEBABE, Some(now), 2048),
            ),
        ];

        store.save_tree_hashes_v2(&hashes).unwrap();
        let loaded = store.load_tree_hashes_v2().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].1.content_hash, hashes[0].1.content_hash);
        assert_eq!(loaded[1].1.size, 2048);
    }

    #[test]
    fn tree_hashes_v2_fallback_from_v1() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        let store = IndexStore::init(root, &config).unwrap();

        // Write only v1 hashes.
        let v1_hashes = vec![
            (PathBuf::from("src/main.rs"), 0xDEADBEEF_u64),
            (PathBuf::from("src/lib.rs"), 0xCAFEBABE_u64),
        ];
        store.save_tree_hashes(&v1_hashes).unwrap();

        // load_tree_hashes_v2 should fall back to v1 with zeroed mtime/size.
        let loaded = store.load_tree_hashes_v2().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].1.content_hash, 0xDEADBEEF);
        assert_eq!(loaded[0].1.mtime_secs, 0);
        assert_eq!(loaded[0].1.size, 0);
    }
}

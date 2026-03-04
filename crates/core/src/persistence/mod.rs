use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::IndexConfig;
use crate::error::{CodeforgeError, Result};
use crate::graph::GraphData;

/// Directory name for the CodeForge index store.
const CODEFORGE_DIR: &str = ".codeforge";
const CONFIG_FILE: &str = "config.json";
const META_FILE: &str = "meta.json";
const TANTIVY_DIR: &str = "tantivy";
const SYMBOLS_FILE: &str = "symbols.bin";
const TREE_HASHES_FILE: &str = "tree_hashes.bin";
const VECTORS_DIR: &str = "vectors";
const VECTOR_INDEX_FILE: &str = "index.usearch";
const FILE_CHUNKS_FILE: &str = "file_chunks.bin";
const CHUNK_META_FILE: &str = "chunk_meta.bin";
const GRAPH_DIR: &str = "graph";
const GRAPH_FILE: &str = "graph.bin";

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
}

impl Default for IndexMeta {
    fn default() -> Self {
        Self {
            version: "0.1.0".to_string(),
            file_count: 0,
            chunk_count: 0,
            symbol_count: 0,
            last_indexed: String::new(),
        }
    }
}

/// Manages the `.codeforge/` directory layout on disk.
///
/// Provides creation, opening, and persistence of the index configuration,
/// metadata, symbol tables, and tree hashes.
#[derive(Debug)]
pub struct IndexStore {
    root: PathBuf,
}

impl IndexStore {
    /// Initialize a new `.codeforge/` directory structure at the given root.
    ///
    /// Creates the directory layout and writes default config and metadata files.
    /// Returns an error if the directory already exists.
    pub fn init(root: &Path, config: &IndexConfig) -> Result<Self> {
        let codeforge_dir = root.join(CODEFORGE_DIR);
        fs::create_dir_all(&codeforge_dir)?;
        fs::create_dir_all(codeforge_dir.join(TANTIVY_DIR))?;
        fs::create_dir_all(codeforge_dir.join(VECTORS_DIR))?;
        fs::create_dir_all(codeforge_dir.join(GRAPH_DIR))?;

        let store = Self {
            root: root.to_path_buf(),
        };

        store.save_config(config)?;
        store.save_meta(&IndexMeta::default())?;

        Ok(store)
    }

    /// Open an existing `.codeforge/` directory.
    ///
    /// Returns [`CodeforgeError::IndexNotFound`] if the directory does not exist.
    pub fn open(root: &Path) -> Result<Self> {
        let codeforge_dir = root.join(CODEFORGE_DIR);
        if !codeforge_dir.is_dir() {
            return Err(CodeforgeError::IndexNotFound {
                path: root.to_path_buf(),
            });
        }
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    /// Check if a `.codeforge/` directory exists at root.
    pub fn exists(root: &Path) -> bool {
        root.join(CODEFORGE_DIR).is_dir()
    }

    /// Path to the `.codeforge/` directory.
    pub fn codeforge_dir(&self) -> PathBuf {
        self.root.join(CODEFORGE_DIR)
    }

    /// Path to the tantivy index directory.
    pub fn tantivy_dir(&self) -> PathBuf {
        self.codeforge_dir().join(TANTIVY_DIR)
    }

    /// Path to the `symbols.bin` file.
    pub fn symbols_path(&self) -> PathBuf {
        self.codeforge_dir().join(SYMBOLS_FILE)
    }

    /// Path to the `tree_hashes.bin` file.
    pub fn tree_hashes_path(&self) -> PathBuf {
        self.codeforge_dir().join(TREE_HASHES_FILE)
    }

    /// Path to the `vectors/` sub-directory.
    pub fn vectors_dir(&self) -> PathBuf {
        self.codeforge_dir().join(VECTORS_DIR)
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
        self.codeforge_dir().join(CHUNK_META_FILE)
    }

    /// Save the [`IndexConfig`] to `config.json`.
    pub fn save_config(&self, config: &IndexConfig) -> Result<()> {
        let path = self.codeforge_dir().join(CONFIG_FILE);
        let json = serde_json::to_string_pretty(config).map_err(|e| {
            CodeforgeError::Serialization(format!("failed to serialize config: {e}"))
        })?;
        fs::write(&path, json)?;
        Ok(())
    }

    /// Load the [`IndexConfig`] from `config.json`.
    pub fn load_config(&self) -> Result<IndexConfig> {
        let path = self.codeforge_dir().join(CONFIG_FILE);
        let json = fs::read_to_string(&path)?;
        let config: IndexConfig = serde_json::from_str(&json).map_err(|e| {
            CodeforgeError::Serialization(format!("failed to deserialize config: {e}"))
        })?;
        Ok(config)
    }

    /// Save the index metadata to `meta.json`.
    pub fn save_meta(&self, meta: &IndexMeta) -> Result<()> {
        let path = self.codeforge_dir().join(META_FILE);
        let json = serde_json::to_string_pretty(meta)
            .map_err(|e| CodeforgeError::Serialization(format!("failed to serialize meta: {e}")))?;
        fs::write(&path, json)?;
        Ok(())
    }

    /// Load the index metadata from `meta.json`.
    pub fn load_meta(&self) -> Result<IndexMeta> {
        let path = self.codeforge_dir().join(META_FILE);
        let json = fs::read_to_string(&path)?;
        let meta: IndexMeta = serde_json::from_str(&json).map_err(|e| {
            CodeforgeError::Serialization(format!("failed to deserialize meta: {e}"))
        })?;
        Ok(meta)
    }

    /// Path to the `graph/` sub-directory.
    pub fn graph_dir(&self) -> PathBuf {
        self.codeforge_dir().join(GRAPH_DIR)
    }

    /// Path to the `graph/graph.bin` file.
    pub fn graph_path(&self) -> PathBuf {
        self.graph_dir().join(GRAPH_FILE)
    }

    /// Serialize and persist the dependency graph.
    pub fn save_graph(&self, data: &GraphData) -> Result<()> {
        // Ensure the directory exists (may not on older indexes opened before Phase 3).
        fs::create_dir_all(self.graph_dir())?;
        let bytes = bitcode::serialize(data).map_err(|e| {
            CodeforgeError::Serialization(format!("failed to serialize graph: {e}"))
        })?;
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
            CodeforgeError::Serialization(format!("failed to deserialize graph: {e}"))
        })?;
        Ok(Some(data))
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
            CodeforgeError::Serialization(format!("failed to serialize tree hashes: {e}"))
        })?;
        fs::write(self.tree_hashes_path(), bytes)?;
        Ok(())
    }

    /// Load tree hashes from `tree_hashes.bin`.
    pub fn load_tree_hashes(&self) -> Result<Vec<(PathBuf, u64)>> {
        let bytes = fs::read(self.tree_hashes_path())?;
        let hashes: Vec<(PathBuf, u64)> = bitcode::deserialize(&bytes).map_err(|e| {
            CodeforgeError::Serialization(format!("failed to deserialize tree hashes: {e}"))
        })?;
        Ok(hashes)
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

        assert!(store.codeforge_dir().is_dir());
        assert!(store.tantivy_dir().is_dir());
        assert!(store.codeforge_dir().join(CONFIG_FILE).is_file());
        assert!(store.codeforge_dir().join(META_FILE).is_file());
        assert!(IndexStore::exists(root));
    }

    #[test]
    fn open_existing_store_succeeds() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        IndexStore::init(root, &config).unwrap();
        let store = IndexStore::open(root).unwrap();

        assert!(store.codeforge_dir().is_dir());
    }

    #[test]
    fn open_nonexistent_returns_index_not_found() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let err = IndexStore::open(root).unwrap_err();
        assert!(
            matches!(err, CodeforgeError::IndexNotFound { .. }),
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

        assert_eq!(store.codeforge_dir(), root.join(".codeforge"));
        assert_eq!(store.tantivy_dir(), root.join(".codeforge/tantivy"));
        assert_eq!(store.symbols_path(), root.join(".codeforge/symbols.bin"));
        assert_eq!(
            store.tree_hashes_path(),
            root.join(".codeforge/tree_hashes.bin")
        );
    }
}

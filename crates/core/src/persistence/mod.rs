use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::config::IndexConfig;
use crate::error::{CodixingError, Result};
use crate::graph::GraphData;

/// Process-global counter making atomic-write temp filenames unique across threads.
static ATOMIC_WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Atomically write `contents` to `path`: write a sibling temp file, fsync it,
/// then rename it over the destination. A crash, SIGKILL, OOM, or power loss
/// mid-write leaves either the previous file or the complete new file intact —
/// never a truncated or zero-length one. The rename is an atomic same-filesystem
/// replace on both Unix and Windows.
///
/// This replaces the bare `fs::write` (truncate-then-write) previously used by
/// every `save_*` helper, which could corrupt `config.json` / `meta.json` /
/// `graph.bin` / `symbols.bin` into an unloadable state on an ill-timed crash.
fn atomic_write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    use std::io::Write;
    let path = path.as_ref();
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path.file_name().and_then(|n| n.to_str()).unwrap_or("tmp");
    let seq = ATOMIC_WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{stem}.tmp.{}.{seq}", std::process::id()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(contents.as_ref())?;
        f.sync_all()?;
    }
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Atomic write with parent-directory durability for transaction markers.
/// File fsync alone does not guarantee the rename survives a power loss on
/// Unix filesystems; syncing the directory closes that final crash window.
fn atomic_write_durable(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    let path = path.as_ref();
    atomic_write(path, contents)?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// Directory name for the Codixing index store.
const CODEFORGE_DIR: &str = ".codixing";
const CONFIG_FILE: &str = "config.json";
const META_FILE: &str = "meta.json";
const TANTIVY_DIR: &str = "tantivy";
const SYMBOLS_FILE: &str = "symbols.bin";
const TREE_HASHES_FILE: &str = "tree_hashes.bin";
const TREE_HASHES_V2_FILE: &str = "tree_hashes_v2.bin";
/// Tiny write-ahead journal of paths whose index artifacts are mid-mutation.
/// It is intentionally separate from the O(repo-files) hash snapshot so an
/// editor save does not rewrite that snapshot twice just for crash intent.
const DIRTY_PATHS_FILE: &str = "dirty_paths.bin";
/// Small overlay of successfully published incremental mutations. Full sync
/// folds it into `tree_hashes_v2.bin`; watcher edits avoid rewriting the
/// repository-sized baseline on every save.
const TREE_HASH_DELTA_FILE: &str = "tree_hash_delta.bin";
const VECTORS_DIR: &str = "vectors";
const VECTOR_INDEX_FILE: &str = "index.usearch";
const FILE_CHUNKS_FILE: &str = "file_chunks.bin";
const CHUNK_META_FILE: &str = "chunk_meta.bin";
const GRAPH_DIR: &str = "graph";
const GRAPH_FILE: &str = "graph.bin";
const SCHEMA_VERSION_FILE: &str = "schema.version";
const MMAP_VECTOR_FILE: &str = "vectors.mmap";
const FILE_TRIGRAM_FILE: &str = "file_trigram.bin";
const CHUNK_TRIGRAM_FILE: &str = "chunk_trigram.bin";
const SYMBOLS_V2_FILE: &str = "symbols_v2.bin";
const CONCEPTS_FILE: &str = "concepts.bin";
const REFORMULATIONS_FILE: &str = "reformulations.bin";
/// Sidecar file mapping each indexed file to its signature fingerprint (a stable
/// hash over symbol signatures / imports / exports). Stored separately from
/// `tree_hashes_v2.bin` so the existing `FileHashEntry` bitcode layout is never
/// touched — old indexes simply lack this file and are treated as STRUCTURAL on
/// the first sync. See `engine::fingerprint` and `Engine::sync`.
const TREE_SIGNATURES_FILE: &str = "tree_signatures.bin";
const TREE_SIGNATURES_LOCK_FILE: &str = "tree_signatures.lock";

struct TreeSignaturesLock {
    path: PathBuf,
}

impl Drop for TreeSignaturesLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

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
    /// Returns [`CodixingError::IndexNotFound`] if the directory does not exist
    /// at all, or [`CodixingError::PartialIndex`] if the directory exists but
    /// is missing the metadata files needed to bring up the engine
    /// (`config.json` and/or `meta.json`). The latter case happens after a
    /// partial deletion or when an older index format is missing newer
    /// required files, and is the failure mode addressed by `codixing repair`.
    pub fn open(root: &Path) -> Result<Self> {
        let codixing_dir = root.join(CODEFORGE_DIR);
        if !codixing_dir.is_dir() {
            return Err(CodixingError::IndexNotFound {
                path: root.to_path_buf(),
            });
        }

        let audit = Self::audit_layout(root);
        if !audit.essentials_missing.is_empty() {
            return Err(CodixingError::PartialIndex {
                root: root.to_path_buf(),
                missing: audit.essentials_missing,
            });
        }

        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    /// Inspect a `.codixing/` directory layout without instantiating the engine.
    ///
    /// Reports which essential metadata files are present or missing so the
    /// CLI can tell users whether to run `codixing repair` (rebuild the
    /// missing pieces) or `codixing init` (wipe and reindex from scratch).
    pub fn audit_layout(root: &Path) -> LayoutAudit {
        let codixing_dir = root.join(CODEFORGE_DIR);
        let dir_exists = codixing_dir.is_dir();

        let mut essentials_present = Vec::new();
        let mut essentials_missing = Vec::new();
        if dir_exists {
            for file in &[CONFIG_FILE, META_FILE] {
                let path = codixing_dir.join(file);
                // Use `is_file` rather than `exists` so a stray directory
                // named `meta.json` does not fool the audit into thinking
                // the index is healthy.
                if path.is_file() {
                    essentials_present.push(path);
                } else {
                    essentials_missing.push(path);
                }
            }
        }

        // Optional artifacts — useful to mention in the repair report so
        // users know whether tantivy/embeddings/symbols were preserved.
        let mut optional_present = Vec::new();
        if dir_exists {
            for sub in &[TANTIVY_DIR, VECTORS_DIR, GRAPH_DIR] {
                let path = codixing_dir.join(sub);
                if path.exists() {
                    optional_present.push(path);
                }
            }
            for file in &[
                SYMBOLS_FILE,
                SYMBOLS_V2_FILE,
                CHUNK_META_FILE,
                CONCEPTS_FILE,
                REFORMULATIONS_FILE,
            ] {
                let path = codixing_dir.join(file);
                if path.exists() {
                    optional_present.push(path);
                }
            }
        }

        // Best-effort: read the version field from meta.json if it survived
        // so `codixing repair` can mention "indexed by 0.40.x, current 0.41.x".
        let meta_version = codixing_dir
            .join(META_FILE)
            .exists()
            .then(|| fs::read_to_string(codixing_dir.join(META_FILE)).ok())
            .flatten()
            .and_then(|s| serde_json::from_str::<IndexMeta>(&s).ok())
            .map(|m| m.version);

        LayoutAudit {
            dir_exists,
            essentials_present,
            essentials_missing,
            optional_present,
            meta_version,
        }
    }

    /// Rebuild missing metadata files in place using safe defaults.
    ///
    /// Preserves any existing artifacts (tantivy/, vectors/, graph/, symbols
    /// blobs). Writes a default [`IndexConfig`] and [`IndexMeta`] when those
    /// files are absent. Returns the list of files that were created so the
    /// caller can surface a recovery report.
    ///
    /// This does *not* re-index source files — call `codixing sync` (or
    /// `Engine::open` followed by sync) afterwards to repopulate counts.
    pub fn repair(root: &Path) -> Result<RepairReport> {
        let codixing_dir = root.join(CODEFORGE_DIR);
        fs::create_dir_all(&codixing_dir)?;
        fs::create_dir_all(codixing_dir.join(TANTIVY_DIR))?;
        fs::create_dir_all(codixing_dir.join(VECTORS_DIR))?;
        fs::create_dir_all(codixing_dir.join(GRAPH_DIR))?;

        let store = Self {
            root: root.to_path_buf(),
        };

        let mut created = Vec::new();
        let config_path = codixing_dir.join(CONFIG_FILE);
        if !config_path.exists() {
            store.save_config(&IndexConfig::new(root))?;
            created.push(config_path);
        }
        let meta_path = codixing_dir.join(META_FILE);
        if !meta_path.exists() {
            store.save_meta(&IndexMeta::default())?;
            created.push(meta_path);
        }

        Ok(RepairReport { created })
    }
}

/// Snapshot of which `.codixing/` files are present, missing, or salvageable.
#[derive(Debug, Clone)]
pub struct LayoutAudit {
    pub dir_exists: bool,
    pub essentials_present: Vec<PathBuf>,
    pub essentials_missing: Vec<PathBuf>,
    pub optional_present: Vec<PathBuf>,
    pub meta_version: Option<String>,
}

impl LayoutAudit {
    /// True when every file required to open the engine is in place.
    pub fn is_complete(&self) -> bool {
        self.dir_exists && self.essentials_missing.is_empty()
    }
}

/// What `IndexStore::repair` rewrote (or left alone) on disk.
#[derive(Debug, Clone)]
pub struct RepairReport {
    /// Paths of the files this call created. Empty when the layout was already
    /// complete and nothing needed to be recreated.
    pub created: Vec<PathBuf>,
}

impl IndexStore {
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

    /// Path to the `symbols_v2.bin` file (mmap format).
    pub fn symbols_v2_path(&self) -> PathBuf {
        self.codixing_dir().join(SYMBOLS_V2_FILE)
    }

    /// Path to the `tree_hashes.bin` file (legacy v1 format).
    pub fn tree_hashes_path(&self) -> PathBuf {
        self.codixing_dir().join(TREE_HASHES_FILE)
    }

    /// Path to the `tree_hashes_v2.bin` file (extended format with mtime+size).
    pub fn tree_hashes_v2_path(&self) -> PathBuf {
        self.codixing_dir().join(TREE_HASHES_V2_FILE)
    }

    /// Path to the incremental-mutation write-ahead journal.
    pub fn dirty_paths_path(&self) -> PathBuf {
        self.codixing_dir().join(DIRTY_PATHS_FILE)
    }

    /// Path to the incremental hash overlay.
    pub fn tree_hash_delta_path(&self) -> PathBuf {
        self.codixing_dir().join(TREE_HASH_DELTA_FILE)
    }

    /// Path to the `tree_signatures.bin` sidecar (per-file signature fingerprints).
    pub fn tree_signatures_path(&self) -> PathBuf {
        self.codixing_dir().join(TREE_SIGNATURES_FILE)
    }

    fn tree_signatures_lock_path(&self) -> PathBuf {
        self.codixing_dir().join(TREE_SIGNATURES_LOCK_FILE)
    }

    fn acquire_tree_signatures_lock(&self) -> Result<TreeSignaturesLock> {
        use std::io::ErrorKind;
        use std::thread;
        use std::time::{Duration, Instant};

        fs::create_dir_all(self.codixing_dir())?;
        let lock_path = self.tree_signatures_lock_path();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(mut file) => {
                    writeln!(file, "pid={}", std::process::id())?;
                    file.sync_all()?;
                    return Ok(TreeSignaturesLock { path: lock_path });
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    return Err(CodixingError::Io(std::io::Error::new(
                        ErrorKind::TimedOut,
                        format!(
                            "timed out waiting for tree signature sidecar lock at {}",
                            lock_path.display()
                        ),
                    )));
                }
                Err(e) => return Err(CodixingError::Io(e)),
            }
        }
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

    /// Path to the concept index binary (`concepts.bin`).
    pub fn concepts_path(&self) -> PathBuf {
        self.codixing_dir().join(CONCEPTS_FILE)
    }

    /// Path to the learned reformulations binary (`reformulations.bin`).
    pub fn reformulations_path(&self) -> PathBuf {
        self.codixing_dir().join(REFORMULATIONS_FILE)
    }

    /// Save the [`IndexConfig`] to `config.json`.
    pub fn save_config(&self, config: &IndexConfig) -> Result<()> {
        let path = self.codixing_dir().join(CONFIG_FILE);
        let json = serde_json::to_string_pretty(config).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize config: {e}"))
        })?;
        atomic_write(&path, json)?;
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
        atomic_write(&path, json)?;
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

    /// Path to the `graph/schema.version` stamp file.
    pub fn graph_schema_version_path(&self) -> PathBuf {
        self.graph_dir().join(SCHEMA_VERSION_FILE)
    }

    /// Read the graph schema version this index's graph was built with.
    ///
    /// Indexes that predate the stamp (or with an unreadable stamp) report 1,
    /// which is always older than the current [`crate::graph::GRAPH_SCHEMA_VERSION`]
    /// and therefore triggers a one-time rebuild on the next sync.
    pub fn load_graph_schema_version(&self) -> u32 {
        fs::read_to_string(self.graph_schema_version_path())
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(1)
    }

    /// Serialize and persist the dependency graph.
    pub fn save_graph(&self, data: &GraphData) -> Result<()> {
        // Ensure the directory exists (may not on older indexes opened before Phase 3).
        fs::create_dir_all(self.graph_dir())?;
        let bytes = bitcode::serialize(data)
            .map_err(|e| CodixingError::Serialization(format!("failed to serialize graph: {e}")))?;
        atomic_write(self.graph_path(), bytes)?;
        // Stamp the schema version the edges were extracted with, so syncs can
        // detect graphs built by an older extractor/resolver and auto-rebuild.
        atomic_write(
            self.graph_schema_version_path(),
            crate::graph::GRAPH_SCHEMA_VERSION.to_string().into_bytes(),
        )?;
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
        atomic_write(self.symbols_path(), bytes)?;
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
        atomic_write_durable(self.tree_hashes_path(), bytes)?;
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
        atomic_write_durable(self.tree_hashes_v2_path(), bytes)?;
        Ok(())
    }

    /// Load extended tree hashes (v2 format) from `tree_hashes_v2.bin`.
    ///
    /// Falls back to the legacy v1 format if v2 does not exist, converting
    /// entries to `FileHashEntry` with zeroed mtime/size (will trigger a
    /// full content-hash check on the first sync, then v2 is written).
    pub fn load_tree_hashes_v2(&self) -> Result<Vec<(PathBuf, FileHashEntry)>> {
        let v2_path = self.tree_hashes_v2_path();
        let hashes = if v2_path.exists() {
            let bytes = fs::read(&v2_path)?;
            let hashes: Vec<(PathBuf, FileHashEntry)> =
                bitcode::deserialize(&bytes).map_err(|e| {
                    CodixingError::Serialization(format!(
                        "failed to deserialize tree hashes v2: {e}"
                    ))
                })?;
            hashes
        } else {
            // Fall back to v1 and upconvert.
            self.load_tree_hashes()
                .unwrap_or_default()
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
                .collect()
        };

        // Incremental watcher publications live in a small overlay until the
        // next full sync. Applying it here keeps every freshness consumer
        // authoritative without an O(repo-files) rewrite per edit.
        let delta = self.load_tree_hash_delta()?;
        if delta.is_empty() {
            return Ok(hashes);
        }
        let mut merged: std::collections::HashMap<PathBuf, FileHashEntry> =
            hashes.into_iter().collect();
        for (path, entry) in delta {
            match entry {
                Some(entry) => {
                    merged.insert(path, entry);
                }
                None => {
                    merged.remove(&path);
                }
            }
        }
        Ok(merged.into_iter().collect())
    }

    /// Load the successfully-published incremental hash overlay.
    pub fn load_tree_hash_delta(&self) -> Result<Vec<(PathBuf, Option<FileHashEntry>)>> {
        let path = self.tree_hash_delta_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let bytes = fs::read(path)?;
        bitcode::deserialize(&bytes).map_err(|e| {
            CodixingError::Serialization(format!("failed to deserialize tree hash delta: {e}"))
        })
    }

    /// Atomically merge successful watcher publications into the small overlay.
    pub fn update_tree_hash_delta(
        &self,
        updates: &[(PathBuf, Option<FileHashEntry>)],
    ) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        let mut delta: std::collections::HashMap<PathBuf, Option<FileHashEntry>> =
            self.load_tree_hash_delta()?.into_iter().collect();
        delta.extend(updates.iter().cloned());
        let mut delta: Vec<_> = delta.into_iter().collect();
        delta.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        self.replace_tree_hash_delta(&delta)
    }

    /// Atomically replace the overlay. Full sync uses this to align every
    /// existing overlay key with its authoritative snapshot value before the
    /// baseline rename, making replay idempotent in either crash window.
    pub fn replace_tree_hash_delta(
        &self,
        delta: &[(PathBuf, Option<FileHashEntry>)],
    ) -> Result<()> {
        let mut delta = delta.to_vec();
        delta.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let bytes = bitcode::serialize(&delta).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize tree hash delta: {e}"))
        })?;
        atomic_write_durable(self.tree_hash_delta_path(), bytes)?;
        Ok(())
    }

    /// Clear the overlay after it has been folded into a full hash snapshot.
    pub fn clear_tree_hash_delta(&self) -> Result<()> {
        let empty: Vec<(PathBuf, Option<FileHashEntry>)> = Vec::new();
        let bytes = bitcode::serialize(&empty).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize tree hash delta: {e}"))
        })?;
        atomic_write_durable(self.tree_hash_delta_path(), bytes)?;
        Ok(())
    }

    /// Load paths whose index mutation has not yet been fully published.
    /// Missing journals are equivalent to an empty set for old indexes.
    pub fn load_dirty_paths(&self) -> Result<Vec<PathBuf>> {
        let path = self.dirty_paths_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let bytes = fs::read(path)?;
        bitcode::deserialize(&bytes).map_err(|e| {
            CodixingError::Serialization(format!("failed to deserialize dirty paths: {e}"))
        })
    }

    /// Atomically add paths to the incremental-mutation journal.
    pub fn mark_dirty_paths(&self, paths: &[PathBuf]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut dirty: std::collections::HashSet<PathBuf> =
            self.load_dirty_paths()?.into_iter().collect();
        dirty.extend(paths.iter().cloned());
        let mut dirty: Vec<_> = dirty.into_iter().collect();
        dirty.sort_unstable();
        let bytes = bitcode::serialize(&dirty).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize dirty paths: {e}"))
        })?;
        atomic_write_durable(self.dirty_paths_path(), bytes)?;
        Ok(())
    }

    /// Atomically clear paths after every corresponding sidecar is durable.
    pub fn clear_dirty_paths(&self, published: &std::collections::HashSet<PathBuf>) -> Result<()> {
        if published.is_empty() || !self.dirty_paths_path().exists() {
            return Ok(());
        }
        let mut dirty = self.load_dirty_paths()?;
        dirty.retain(|path| !published.contains(path));
        dirty.sort_unstable();
        let bytes = bitcode::serialize(&dirty).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize dirty paths: {e}"))
        })?;
        atomic_write_durable(self.dirty_paths_path(), bytes)?;
        Ok(())
    }

    /// Reset the mutation journal after a complete authoritative rebuild.
    pub fn clear_all_dirty_paths(&self) -> Result<()> {
        let empty: Vec<PathBuf> = Vec::new();
        let bytes = bitcode::serialize(&empty).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize dirty paths: {e}"))
        })?;
        atomic_write_durable(self.dirty_paths_path(), bytes)?;
        Ok(())
    }

    /// Save per-file signature fingerprints to the `tree_signatures.bin` sidecar.
    ///
    /// Stored as a bitcode-serialized `Vec<(PathBuf, u64)>`. This is a *separate*
    /// file from the tree hashes so adding it never alters the existing hash-store
    /// format — an index built before this feature simply has no sidecar.
    pub fn save_tree_signatures(&self, sigs: &[(PathBuf, u64)]) -> Result<()> {
        let _lock = self.acquire_tree_signatures_lock()?;
        self.save_tree_signatures_unlocked(sigs)
    }

    fn save_tree_signatures_unlocked(&self, sigs: &[(PathBuf, u64)]) -> Result<()> {
        let bytes = bitcode::serialize(sigs).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize tree signatures: {e}"))
        })?;
        atomic_write(self.tree_signatures_path(), bytes)?;
        Ok(())
    }

    /// Load per-file signature fingerprints from `tree_signatures.bin`.
    ///
    /// Returns an empty vector if the sidecar does not exist (an index built
    /// before this feature) or fails to deserialize (forward/backward
    /// incompatibility). In both cases the caller treats every changed file as
    /// STRUCTURAL on the first sync — the conservative default — and the sidecar
    /// is rewritten afterwards.
    pub fn load_tree_signatures(&self) -> Result<Vec<(PathBuf, u64)>> {
        self.load_tree_signatures_unlocked()
    }

    fn load_tree_signatures_unlocked(&self) -> Result<Vec<(PathBuf, u64)>> {
        let path = self.tree_signatures_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let bytes = fs::read(&path)?;
        match bitcode::deserialize::<Vec<(PathBuf, u64)>>(&bytes) {
            Ok(sigs) => Ok(sigs),
            // A corrupt or incompatible sidecar must not corrupt the index:
            // fall back to "no fingerprints", i.e. treat all changes as structural.
            Err(_) => Ok(Vec::new()),
        }
    }

    /// Atomically update the signature sidecar under a cross-process lock.
    ///
    /// Mutators that perform read-modify-write must use this instead of doing
    /// load/filter/save themselves; otherwise a concurrent sync can resurrect a
    /// fingerprint that another process deliberately invalidated.
    pub fn update_tree_signatures<F>(&self, update: F) -> Result<()>
    where
        F: FnOnce(Vec<(PathBuf, u64)>) -> Vec<(PathBuf, u64)>,
    {
        let _lock = self.acquire_tree_signatures_lock()?;
        let current = self.load_tree_signatures_unlocked()?;
        let next = update(current);
        self.save_tree_signatures_unlocked(&next)
    }

    /// Save the chunk metadata map (bitcode-serialized `Vec<(u64, ChunkMeta)>`).
    ///
    /// Accepts a flat list of `(chunk_id, meta)` pairs rather than the DashMap
    /// directly to avoid depending on DashMap in persistence.
    pub fn save_chunk_meta_bytes(&self, bytes: &[u8]) -> Result<()> {
        atomic_write(self.chunk_meta_path(), bytes)?;
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
    fn open_partial_layout_returns_partial_index_error() {
        // Regression for #100: when `.codixing/` exists with index artifacts
        // but `config.json` is missing, `IndexStore::open` must return a
        // PartialIndex error naming the missing file instead of letting the
        // failure surface as a generic `I/O error: No such file or directory`.
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);
        IndexStore::init(root, &config).unwrap();

        // Simulate the failure mode reported in the issue.
        fs::remove_file(root.join(CODEFORGE_DIR).join(CONFIG_FILE)).unwrap();

        let err = IndexStore::open(root).unwrap_err();
        match err {
            CodixingError::PartialIndex { ref missing, .. } => {
                assert!(
                    missing.iter().any(|p| p.ends_with(CONFIG_FILE)),
                    "missing list should include config.json: {missing:?}"
                );
                let rendered = format!("{err}");
                assert!(
                    rendered.contains("codixing repair"),
                    "error should suggest repair: {rendered}"
                );
            }
            other => panic!("expected PartialIndex, got: {other:?}"),
        }
    }

    #[test]
    fn repair_recreates_missing_metadata_in_place() {
        // Regression for #100: `IndexStore::repair` must rewrite the missing
        // metadata files using safe defaults while leaving existing
        // artifacts (tantivy/, vectors/, graph/) untouched.
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);
        IndexStore::init(root, &config).unwrap();

        // Drop the metadata files but keep a marker file under tantivy/ so
        // we can prove repair did not nuke the rest of the index.
        let tantivy_marker = root.join(CODEFORGE_DIR).join(TANTIVY_DIR).join("MARKER");
        fs::write(&tantivy_marker, b"keep me").unwrap();
        fs::remove_file(root.join(CODEFORGE_DIR).join(CONFIG_FILE)).unwrap();
        fs::remove_file(root.join(CODEFORGE_DIR).join(META_FILE)).unwrap();

        let pre = IndexStore::audit_layout(root);
        assert!(!pre.is_complete());

        let report = IndexStore::repair(root).unwrap();
        assert_eq!(report.created.len(), 2, "should have rewritten 2 files");

        let post = IndexStore::audit_layout(root);
        assert!(post.is_complete(), "layout should be complete after repair");
        assert!(
            tantivy_marker.exists(),
            "repair must not delete preserved tantivy artifacts"
        );

        // Sanity: store is now openable.
        IndexStore::open(root).unwrap();
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
    fn tree_signatures_update_reloads_and_rewrites_under_lock() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        let store = IndexStore::init(root, &config).unwrap();
        store
            .save_tree_signatures(&[
                (PathBuf::from("src/a.rs"), 1),
                (PathBuf::from("src/b.rs"), 2),
            ])
            .unwrap();

        store
            .update_tree_signatures(|sigs| {
                sigs.into_iter()
                    .filter(|(path, _)| path != Path::new("src/a.rs"))
                    .chain(std::iter::once((PathBuf::from("src/c.rs"), 3)))
                    .collect()
            })
            .unwrap();

        let loaded = store.load_tree_signatures().unwrap();
        assert_eq!(
            loaded,
            vec![
                (PathBuf::from("src/b.rs"), 2),
                (PathBuf::from("src/c.rs"), 3)
            ]
        );
        assert!(!store.tree_signatures_lock_path().exists());
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

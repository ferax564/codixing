#[cfg(feature = "rustqueue")]
pub mod embed_queue;
pub(super) mod embed_state;
pub(super) mod embed_stats;
mod files;
mod focus_map;
pub mod freshness;
mod graph;
pub(crate) mod indexing;
mod init;
mod orphans;
pub(crate) mod pipeline;
pub mod recency;
mod reload;
mod search;
mod symbol_graph;
mod sync;
pub(crate) mod synonyms;
mod temporal;
mod test_mapping;
mod validation;

pub use embed_stats::EmbedTimingStats;
pub use focus_map::{FocusMapEntry, FocusMapOptions};
pub use symbol_graph::SymbolReference;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use dashmap::DashMap;
use serde::Serialize;

use crate::config::IndexConfig;
use crate::embedder::Embedder;
use crate::error::CodixingError;
use crate::graph::CodeGraph;
use crate::index::TantivyIndex;
use crate::index::trigram::FileTrigramIndex;
use crate::parser::Parser;
use crate::persistence::IndexStore;
use crate::reranker::Reranker;
use crate::retriever::ChunkMeta;
use crate::session::SessionState;
use crate::shared_session::SharedSession;
use crate::symbols::SymbolTable;
use crate::vector::VectorIndex;

/// Summary statistics about the index.
#[derive(Debug, Clone)]
pub struct IndexStats {
    /// Number of files indexed.
    pub file_count: usize,
    /// Number of code chunks produced.
    pub chunk_count: usize,
    /// Number of unique symbol names.
    pub symbol_count: usize,
    /// Number of vectors in the HNSW index.
    pub vector_count: usize,
    /// Number of nodes in the dependency graph (0 if graph not built).
    pub graph_node_count: usize,
    /// Number of edges in the dependency graph (0 if graph not built).
    pub graph_edge_count: usize,
    /// Number of nodes in the symbol-level call graph (0 if not built).
    pub symbol_node_count: usize,
    /// Number of edges in the symbol-level call graph (0 if not built).
    pub symbol_edge_count: usize,
}

/// Statistics returned by [`Engine::sync`].
#[derive(Debug, Clone, Serialize)]
pub struct SyncStats {
    /// Files present on disk but not yet in the index (new files).
    pub added: usize,
    /// Files whose content changed since the last index save.
    pub modified: usize,
    /// Files that were in the index but no longer exist on disk.
    pub removed: usize,
    /// Files that are unchanged and were skipped.
    pub unchanged: usize,
}

/// Statistics returned by [`Engine::git_sync`].
#[derive(Debug, Clone, Default)]
pub struct GitSyncStats {
    /// Number of modified or added files that were re-indexed.
    pub modified: usize,
    /// Number of deleted files that were removed from the index.
    pub removed: usize,
    /// `true` when HEAD already matches the stored commit — nothing was done.
    pub unchanged: bool,
}

/// Report on how stale the index is relative to the current filesystem.
///
/// Produced by [`Engine::check_staleness`]. All detection uses `stat()` only
/// (mtime + size comparison) — no file content is read, so this is fast even
/// on large projects.
#[derive(Debug, Clone)]
pub struct StaleReport {
    /// `true` if any tracked file has been modified, added, or deleted.
    pub is_stale: bool,
    /// Number of indexed files whose mtime or size has changed.
    pub modified_files: usize,
    /// Number of on-disk source files not present in the index.
    pub new_files: usize,
    /// Number of indexed files no longer present on disk.
    pub deleted_files: usize,
    /// Timestamp of the last index build/sync (parsed from `IndexMeta`).
    pub last_sync: Option<SystemTime>,
    /// Human-readable suggestion for the user.
    pub suggestion: String,
}

/// Validation result for a proposed symbol rename.
///
/// Produced by [`Engine::validate_rename`] before any files are modified,
/// so the user can review potential conflicts.
#[derive(Debug, Clone)]
pub struct RenameValidation {
    /// `true` if no conflicts were detected.
    pub is_safe: bool,
    /// Potential conflicts found during validation.
    pub conflicts: Vec<RenameConflict>,
    /// Files that contain the old name and would be modified.
    pub affected_files: Vec<String>,
    /// Total number of occurrences of the old name across all files.
    pub occurrence_count: usize,
}

/// A single conflict detected during rename validation.
#[derive(Debug, Clone)]
pub struct RenameConflict {
    /// Relative file path where the conflict was found.
    pub file_path: String,
    /// Line number (1-indexed) of the conflict.
    pub line: usize,
    /// The kind of conflict.
    pub kind: ConflictKind,
    /// Human-readable explanation.
    pub message: String,
}

/// Types of conflicts that can arise during a rename.
#[derive(Debug, Clone, PartialEq)]
pub enum ConflictKind {
    /// The new name already exists as a symbol definition in an affected file.
    NameCollision,
    /// The new name shadows an existing symbol in the same scope.
    Shadowing,
    /// The new name collides with an existing import.
    ImportConflict,
}

/// A single regex/literal match produced by [`Engine::grep_code`].
#[derive(Debug, Clone)]
pub struct GrepMatch {
    /// Relative file path from the project root.
    pub file_path: String,
    /// 0-indexed line number of the matching line.
    pub line_number: u64,
    /// The full text of the matching line.
    pub line: String,
    /// Byte offset of the match start within `line`.
    pub match_start: usize,
    /// Byte offset of the match end within `line`.
    pub match_end: usize,
    /// Context lines immediately before the match (oldest first).
    pub before: Vec<String>,
    /// Context lines immediately after the match.
    pub after: Vec<String>,
}

// -------------------------------------------------------------------------
// Git helpers (private free functions, no external dependency)
// -------------------------------------------------------------------------

/// Return the current HEAD commit hash, or `None` if git is unavailable / not a repo.
fn git_head_commit(root: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Return files changed between `since_commit` and the working tree HEAD.
///
/// Returns `(modified_or_added, deleted)` path lists (absolute).
/// Returns `None` if git is unavailable or the command fails.
fn git_diff_since(root: &Path, since_commit: &str) -> Option<(Vec<PathBuf>, Vec<PathBuf>)> {
    let out = std::process::Command::new("git")
        .args(["diff", "--name-status", since_commit])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;

    let mut modified = Vec::new();
    let mut deleted = Vec::new();
    for line in text.lines() {
        let mut parts = line.split('\t');
        let status = parts.next()?;
        match status.chars().next()? {
            'D' => {
                let path_str = parts.next()?.trim();
                deleted.push(root.join(path_str));
            }
            // Rename/copy records are: Rxxx <old> <new> / Cxxx <old> <new>
            'R' | 'C' => {
                let old_path = parts.next()?.trim();
                let new_path = parts.next()?.trim();
                deleted.push(root.join(old_path));
                modified.push(root.join(new_path));
            }
            // A=Added, M=Modified, T=Type changed, etc.
            _ => {
                let path_str = parts.next()?.trim();
                modified.push(root.join(path_str));
            }
        }
    }
    Some((modified, deleted))
}

/// Top-level facade that wires together parsing, chunking, indexing,
/// and retrieval into a single coherent API.
pub struct Engine {
    pub(super) config: IndexConfig,
    pub(super) store: IndexStore,
    pub(super) parser: Parser,
    pub(super) tantivy: TantivyIndex,
    pub(super) symbols: SymbolTable,
    /// Per-file chunk counts, used for stats.
    pub(super) file_chunk_counts: HashMap<String, usize>,
    /// Optional fastembed model for vector embeddings.
    pub(super) embedder: Option<Arc<Embedder>>,
    /// Optional RustQueue instance for queue-based embedding.
    #[cfg(feature = "rustqueue")]
    pub(super) embed_queue: Option<Arc<rustqueue::RustQueue>>,
    /// Optional usearch HNSW vector index.
    ///
    /// Wrapped in `Arc<RwLock<...>>` so that a background embedding thread
    /// (Task 5) can write new vectors while search queries hold read locks.
    pub(super) vector: Arc<RwLock<Option<VectorIndex>>>,
    /// Chunk metadata hydration table for vector results.
    ///
    /// Wrapped in `Arc` so the background embedding thread can share it
    /// without taking a write lock on the whole engine.
    pub(super) chunk_meta: Arc<DashMap<u64, ChunkMeta>>,
    /// Optional code dependency graph with PageRank scores.
    pub(super) graph: Option<CodeGraph>,
    /// Optional cross-encoder reranker (BGE-Reranker-Base) for the `deep` strategy.
    pub(super) reranker: Option<Arc<Reranker>>,
    /// Trigram index for sub-millisecond exact substring search (Strategy::Exact).
    /// Lazy-loaded from disk on first use via OnceLock.
    pub(super) trigram: std::sync::OnceLock<crate::index::TrigramIndex>,
    /// Session state for tracking agent interactions.
    session: Arc<SessionState>,
    /// Shared session store for multi-agent context sharing.
    shared_session: SharedSession,
    /// `true` when the index was opened without the Tantivy write lock.
    /// All search / read operations work; write operations return
    /// [`CodixingError::ReadOnly`].
    read_only: bool,
    /// File-level trigram index for fast grep pre-filtering.
    /// Lazy-loaded from disk on first use via OnceLock.
    pub(super) file_trigram: std::sync::OnceLock<FileTrigramIndex>,
    /// Lazy-initialised git recency map (file path → last commit timestamp).
    recency_map: std::sync::OnceLock<std::collections::HashMap<String, i64>>,
    /// When this engine was last loaded/reloaded from disk (mtime of `meta.json`).
    last_load_time: Option<std::time::SystemTime>,
    /// Minimum interval between reload checks (default: 30s).
    reload_interval: std::time::Duration,
    /// Last time we checked for staleness.
    last_staleness_check: Option<std::time::Instant>,
    /// Background embedding state — `None` when embeddings were synchronous or index was opened.
    pub(super) embed_state: Option<Arc<embed_state::EmbedState>>,
    /// Lazily-initialised concept reranker for future A/B experiments.
    ///
    /// NOT used in the active search path — general-purpose rerankers hurt code search
    /// quality by favouring prose matches over code structure.  Initialised on first call
    /// to `get_concept_reranker`.  `None` inside the lock means the model failed to load.
    pub(super) concept_reranker: std::sync::OnceLock<Option<Arc<Reranker>>>,
}

impl Engine {
    /// Get the concept reranker, lazily loading it on first access.
    ///
    /// Returns `None` if the model failed to load (e.g. ONNX runtime not available).
    ///
    /// **NOT used in the active search path** — general-purpose rerankers (Jina, BGE)
    /// consistently hurt code search quality by preferring prose matches over code
    /// structure.  This is infrastructure for future A/B testing once a code-specific
    /// cross-encoder becomes available.
    #[allow(dead_code)]
    pub(super) fn get_concept_reranker(&self) -> Option<&Arc<Reranker>> {
        use crate::reranker::Reranker;
        use fastembed::RerankerModel;
        self.concept_reranker
            .get_or_init(
                || match Reranker::with_model(RerankerModel::JINARerankerV1TurboEn) {
                    Ok(r) => Some(Arc::new(r)),
                    Err(e) => {
                        tracing::warn!("concept reranker unavailable: {e}");
                        None
                    }
                },
            )
            .as_ref()
    }

    /// Return the git recency map, lazily building it on first access.
    ///
    /// The map covers the last 180 days and maps relative file paths to
    /// their most recent commit timestamp (Unix epoch seconds).
    pub(super) fn get_recency_map(&self) -> &std::collections::HashMap<String, i64> {
        self.recency_map
            .get_or_init(|| recency::build_recency_map(self.store.root(), 180))
    }

    /// Get or lazily load the chunk-level trigram index from disk.
    pub(super) fn get_trigram(&self) -> &crate::index::TrigramIndex {
        self.trigram.get_or_init(|| {
            if self.store.chunk_trigram_path().exists() {
                match crate::index::TrigramIndex::load_binary(&self.store.chunk_trigram_path()) {
                    Ok(idx) => idx,
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to load chunk trigram; rebuilding");
                        rebuild_trigram_from_tantivy(&self.tantivy)
                    }
                }
            } else {
                rebuild_trigram_from_tantivy(&self.tantivy)
            }
        })
    }

    /// Get or lazily load the file-level trigram index from disk.
    pub(super) fn get_file_trigram(&self) -> &FileTrigramIndex {
        self.file_trigram.get_or_init(|| {
            if self.store.file_trigram_path().exists() {
                match FileTrigramIndex::load_binary(&self.store.file_trigram_path()) {
                    Ok(idx) => idx,
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to load file trigram; rebuilding");
                        build_file_trigram_from_tantivy(&self.tantivy)
                    }
                }
            } else {
                build_file_trigram_from_tantivy(&self.tantivy)
            }
        })
    }

    /// Return summary statistics about the current index.
    pub fn stats(&self) -> IndexStats {
        let (graph_node_count, graph_edge_count, symbol_node_count, symbol_edge_count) = self
            .graph
            .as_ref()
            .map(|g| {
                let s = g.stats();
                (s.node_count, s.edge_count, s.symbol_nodes, s.symbol_edges)
            })
            .unwrap_or((0, 0, 0, 0));
        IndexStats {
            file_count: self.file_chunk_counts.len(),
            chunk_count: self.file_chunk_counts.values().sum(),
            symbol_count: self.symbols.len(),
            vector_count: self
                .vector
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|v| v.len())
                .unwrap_or(0),
            graph_node_count,
            graph_edge_count,
            symbol_node_count,
            symbol_edge_count,
        }
    }

    /// Access the underlying index configuration.
    pub fn config(&self) -> &IndexConfig {
        &self.config
    }

    /// Access the underlying symbol table.
    pub fn symbol_table(&self) -> &SymbolTable {
        &self.symbols
    }

    /// Return `true` if a `.codixing/` index directory exists at `root`.
    ///
    /// Used by the MCP server to decide whether auto-init is needed.
    pub fn index_exists(root: impl AsRef<Path>) -> bool {
        IndexStore::exists(root.as_ref())
    }

    /// Access the session state.
    pub fn session(&self) -> &Arc<SessionState> {
        &self.session
    }

    /// Replace the session state (e.g. to disable session tracking).
    pub fn set_session(&mut self, session: Arc<SessionState>) {
        self.session = session;
    }

    /// Access the shared (multi-agent) session store.
    pub fn shared_session(&self) -> &SharedSession {
        &self.shared_session
    }

    /// Replace the shared session store (e.g. to inject a shared instance
    /// across multiple engine references in daemon mode).
    pub fn set_shared_session(&mut self, shared: SharedSession) {
        self.shared_session = shared;
    }

    /// Retrieve chunk content from Tantivy stored fields.
    ///
    /// Used when `chunk_meta.content` is empty (compact persistence mode).
    /// Returns the content string, or `None` if the chunk is not found.
    pub fn get_chunk_content(&self, chunk_id: u64) -> Option<String> {
        let ids: std::collections::HashSet<u64> = [chunk_id].into_iter().collect();
        let docs = self.tantivy.lookup_chunks_by_ids(&ids).ok()?;
        let fields = self.tantivy.fields();
        docs.into_iter().next().and_then(|doc| {
            doc.get_first(fields.content)
                .and_then(|v| tantivy::schema::Value::as_str(&v))
                .map(|s| s.to_string())
        })
    }

    /// Retrieve chunk content, first checking the in-memory `chunk_meta` map
    /// and falling back to Tantivy stored fields if the content is empty.
    pub fn resolve_chunk_content(&self, chunk_id: u64) -> Option<String> {
        if let Some(meta) = self.chunk_meta.get(&chunk_id) {
            if !meta.content.is_empty() {
                return Some(meta.content.clone());
            }
        }
        self.get_chunk_content(chunk_id)
    }

    /// Get combined callers + callees for a file (used for graph-propagated session boost).
    pub fn file_neighbors(&self, file: &str) -> Vec<String> {
        let mut neighbors = self.callers(file);
        neighbors.extend(self.callees(file));
        neighbors.sort();
        neighbors.dedup();
        neighbors
    }

    /// Return a reference to the chunk metadata table.
    ///
    /// Used by `bench-embed` to inspect which chunks exist without taking ownership.
    pub fn chunk_meta_ref(&self) -> &DashMap<u64, ChunkMeta> {
        &self.chunk_meta
    }

    /// Return a clone of the `Arc` wrapping the chunk metadata table.
    ///
    /// Used by the background embedding thread to share ownership without
    /// locking the full engine.
    pub fn chunk_meta_arc(&self) -> Arc<DashMap<u64, ChunkMeta>> {
        Arc::clone(&self.chunk_meta)
    }

    /// Collect chunk IDs that have no vector representation yet.
    ///
    /// Returns a `DashMap<chunk_id, file_path>` of chunks missing from the
    /// vector index.  When the engine has no vector index, every chunk is
    /// considered unembedded.
    pub fn find_unembedded_chunks(&self) -> crate::error::Result<DashMap<u64, String>> {
        let pending = DashMap::new();
        // Build the set of chunk IDs that already have vectors.
        let vec_guard = self.vector.read().unwrap_or_else(|e| e.into_inner());
        let embedded: std::collections::HashSet<u64> = vec_guard
            .as_ref()
            .map(|v| v.file_chunks().values().flatten().copied().collect())
            .unwrap_or_default();
        drop(vec_guard);

        for entry in self.chunk_meta.iter() {
            if !embedded.contains(entry.key()) {
                pending.insert(*entry.key(), entry.value().file_path.clone());
            }
        }
        Ok(pending)
    }

    /// Run embedding on the given pending chunks and return timing stats.
    ///
    /// Writes vectors into the in-memory index but does not persist to disk.
    /// Note: with --force on a fully-embedded index, this will duplicate key IDs
    /// in the HNSW graph for the lifetime of the process.
    pub fn bench_embed(
        &self,
        pending: &DashMap<u64, String>,
    ) -> crate::error::Result<EmbedTimingStats> {
        let embedder = self
            .embedder
            .as_ref()
            .ok_or_else(|| CodixingError::Config("no embedder configured".into()))?
            .clone();
        let mut vec_guard = self.vector.write().unwrap_or_else(|e| e.into_inner());
        let vec_idx = vec_guard
            .as_mut()
            .ok_or_else(|| CodixingError::Config("no vector index".into()))?;
        let contextual = self.config.embedding.contextual_embeddings;
        let root = self.store.root().to_path_buf();
        indexing::embed_and_index_chunks(
            pending,
            &self.chunk_meta,
            &embedder,
            vec_idx,
            contextual,
            &root,
        )
    }

    /// Returns (completed, total) embedding progress. (0, 0) if no background embedding.
    pub fn embedding_progress(&self) -> (usize, usize) {
        self.embed_state
            .as_ref()
            .map(|s| s.progress())
            .unwrap_or((0, 0))
    }

    /// True when embeddings are complete (or were never started in background).
    pub fn embeddings_ready(&self) -> bool {
        self.embed_state
            .as_ref()
            .map(|s| s.is_ready())
            .unwrap_or(true) // No background embed = always ready
    }

    /// Block until background embeddings complete. No-op if already done.
    pub fn wait_for_embeddings(&self) {
        if let Some(state) = &self.embed_state {
            while !state.is_ready() {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            if let Some(handle) = state
                .handle
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                let _ = handle.join();
            }
        }
    }

    /// Request background embedding to stop and join the thread.
    pub fn shutdown_embeddings(&self) {
        if let Some(state) = &self.embed_state {
            state.request_cancel();
            if let Some(handle) = state
                .handle
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Do NOT cancel background embeddings on drop — let the thread finish
        // and persist the vector index. Only explicit shutdown_embeddings()
        // should cancel. The thread holds Arc clones of shared state, so it
        // will complete safely even after Engine is dropped.
        if let Some(state) = &self.embed_state {
            if !state.is_ready() {
                tracing::debug!(
                    "Engine dropped while background embedding in progress — thread will continue"
                );
            }
        }
    }
}

// Bring indexing helpers into scope for tests and this module's remaining code.
use indexing::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retriever::{SearchQuery, Strategy};
    use std::fs;
    use tempfile::tempdir;

    /// Create a temporary project with some source files.
    fn setup_project() -> (tempfile::TempDir, PathBuf) {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();

        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();

        fs::write(
            src_dir.join("main.rs"),
            r#"
/// Entry point.
fn main() {
    println!("Hello, world!");
}

/// Add two numbers.
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub struct Config {
    pub verbose: bool,
    pub threads: usize,
}
"#,
        )
        .unwrap();

        fs::write(
            src_dir.join("lib.rs"),
            r#"
/// A helper function.
pub fn helper() -> String {
    "help".to_string()
}

pub trait Processor {
    fn process(&self, input: &str) -> String;
}
"#,
        )
        .unwrap();

        (dir, root)
    }

    fn setup_engine_bm25_only() -> (tempfile::TempDir, Engine) {
        let (dir, root) = setup_project();
        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false; // disable embeddings for fast tests
        let engine = Engine::init(&root, config).unwrap();
        (dir, engine)
    }

    #[test]
    fn init_indexes_project() {
        let (_dir, engine) = setup_engine_bm25_only();
        let stats = engine.stats();

        assert_eq!(stats.file_count, 2, "expected 2 source files");
        assert!(stats.chunk_count > 0, "expected at least 1 chunk");
        assert!(stats.symbol_count > 0, "expected at least 1 symbol");
    }

    #[test]
    fn search_instant_finds_function() {
        let (_dir, engine) = setup_engine_bm25_only();

        let results = engine
            .search(
                SearchQuery::new("add")
                    .with_limit(5)
                    .with_strategy(Strategy::Instant),
            )
            .unwrap();
        assert!(!results.is_empty(), "expected search results for 'add'");
        assert!(
            results.iter().any(|r| r.file_path.contains("main.rs")),
            "expected result from main.rs"
        );
    }

    #[test]
    fn search_fast_falls_back_without_embedder() {
        let (_dir, engine) = setup_engine_bm25_only();

        // Fast strategy without embedder should fall back to BM25 gracefully.
        let results = engine
            .search(
                SearchQuery::new("helper")
                    .with_limit(5)
                    .with_strategy(Strategy::Fast),
            )
            .unwrap();
        assert!(!results.is_empty());
    }

    #[test]
    fn symbols_returns_matching() {
        let (_dir, engine) = setup_engine_bm25_only();

        let syms = engine.symbols("Config", None).unwrap();
        assert!(
            !syms.is_empty(),
            "expected at least 1 symbol matching 'Config'"
        );
        assert!(syms.iter().any(|s| s.name == "Config"));
    }

    #[test]
    fn open_restores_index() {
        let (dir, root) = setup_project();
        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;

        {
            let engine = Engine::init(&root, config).unwrap();
            assert!(engine.stats().chunk_count > 0);
        }

        let engine = Engine::open(&root).unwrap();
        let results = engine
            .search(
                SearchQuery::new("helper")
                    .with_limit(5)
                    .with_strategy(Strategy::Instant),
            )
            .unwrap();
        assert!(
            !results.is_empty(),
            "expected results after re-opening index"
        );

        drop(dir);
    }

    #[test]
    fn reindex_file_updates_index() {
        let (dir, root) = setup_project();
        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;

        let mut engine = Engine::init(&root, config).unwrap();

        fs::write(
            root.join("src/main.rs"),
            r#"
/// New entry point.
fn main() {
    println!("Modified!");
}

pub fn unique_new_function() -> bool {
    true
}
"#,
        )
        .unwrap();

        engine.reindex_file(Path::new("src/main.rs")).unwrap();

        let results = engine
            .search(
                SearchQuery::new("unique_new_function")
                    .with_limit(5)
                    .with_strategy(Strategy::Instant),
            )
            .unwrap();
        assert!(
            !results.is_empty(),
            "expected to find newly added function after reindex"
        );

        drop(dir);
    }

    #[test]
    fn stats_includes_vector_count() {
        let (_dir, engine) = setup_engine_bm25_only();
        let stats = engine.stats();
        // embeddings disabled → vector_count = 0.
        assert_eq!(stats.vector_count, 0);
    }

    #[test]
    fn test_build_context_prefix() {
        let meta = ChunkMeta {
            chunk_id: 42,
            file_path: "crates/core/src/engine/search.rs".to_string(),
            language: "rust".to_string(),
            line_start: 10,
            line_end: 30,
            signature: "fn search(&self) -> Result<Vec<SearchResult>>".to_string(),
            scope_chain: vec!["Engine".to_string(), "search".to_string()],
            entity_names: vec!["search".to_string(), "apply_graph_boost".to_string()],
            content: "fn search(&self) { }".to_string(),
            content_hash: 0,
        };
        let prefix = build_context_prefix(&meta);
        assert_eq!(
            prefix,
            "File: crates/core/src/engine/search.rs | Language: rust | Scope: Engine > search | Entities: search, apply_graph_boost\n"
        );
    }

    #[test]
    fn test_context_prefix_empty_scope() {
        let meta = ChunkMeta {
            chunk_id: 1,
            file_path: "src/main.rs".to_string(),
            language: "rust".to_string(),
            line_start: 0,
            line_end: 5,
            signature: String::new(),
            scope_chain: vec![],
            entity_names: vec!["main".to_string()],
            content: "fn main() {}".to_string(),
            content_hash: 0,
        };
        let prefix = build_context_prefix(&meta);
        assert_eq!(
            prefix,
            "File: src/main.rs | Language: rust | Entities: main\n"
        );
        // No " | Scope:" segment when scope_chain is empty.
        assert!(!prefix.contains("Scope:"));
    }

    #[test]
    fn test_context_prefix_with_entities() {
        let meta = ChunkMeta {
            chunk_id: 7,
            file_path: "lib/parser.py".to_string(),
            language: "python".to_string(),
            line_start: 0,
            line_end: 50,
            signature: String::new(),
            scope_chain: vec!["Parser".to_string()],
            entity_names: vec![
                "parse".to_string(),
                "tokenize".to_string(),
                "validate".to_string(),
            ],
            content: "class Parser: ...".to_string(),
            content_hash: 0,
        };
        let prefix = build_context_prefix(&meta);
        assert!(prefix.contains("Entities: parse, tokenize, validate"));
        assert!(prefix.contains("Scope: Parser"));
        assert!(prefix.ends_with('\n'));
    }

    #[test]
    fn test_context_prefix_no_entities_no_scope() {
        let meta = ChunkMeta {
            chunk_id: 99,
            file_path: "config.toml".to_string(),
            language: "toml".to_string(),
            line_start: 0,
            line_end: 10,
            signature: String::new(),
            scope_chain: vec![],
            entity_names: vec![],
            content: "[package]\nname = \"foo\"".to_string(),
            content_hash: 0,
        };
        let prefix = build_context_prefix(&meta);
        assert_eq!(prefix, "File: config.toml | Language: toml\n");
        assert!(!prefix.contains("Scope:"));
        assert!(!prefix.contains("Entities:"));
    }

    #[test]
    fn test_make_embed_text_contextual() {
        let meta = ChunkMeta {
            chunk_id: 42,
            file_path: "src/lib.rs".to_string(),
            language: "rust".to_string(),
            line_start: 0,
            line_end: 5,
            signature: String::new(),
            scope_chain: vec!["Foo".to_string()],
            entity_names: vec!["bar".to_string()],
            content: "fn bar() {}".to_string(),
            content_hash: 0,
        };
        let text = make_embed_text(&meta, true);
        assert!(text.starts_with("File: src/lib.rs | Language: rust"));
        assert!(text.contains("Scope: Foo"));
        assert!(text.contains("Entities: bar"));
        assert!(text.ends_with("fn bar() {}"));
    }

    #[test]
    fn test_make_embed_text_non_contextual() {
        let meta = ChunkMeta {
            chunk_id: 42,
            file_path: "src/lib.rs".to_string(),
            language: "rust".to_string(),
            line_start: 0,
            line_end: 5,
            signature: String::new(),
            scope_chain: vec!["Foo".to_string()],
            entity_names: vec!["bar".to_string()],
            content: "fn bar() {}".to_string(),
            content_hash: 0,
        };
        let text = make_embed_text(&meta, false);
        // Non-contextual mode returns raw content only.
        assert_eq!(text, "fn bar() {}");
    }

    #[test]
    fn read_only_search_works() {
        let (dir, root) = setup_project();
        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;

        // Build the index first.
        {
            let _engine = Engine::init(&root, config).unwrap();
        }

        // Open in explicit read-only mode.
        let engine = Engine::open_read_only(&root).unwrap();
        assert!(engine.is_read_only());

        // Search should work normally.
        let results = engine
            .search(
                SearchQuery::new("add")
                    .with_limit(5)
                    .with_strategy(Strategy::Instant),
            )
            .unwrap();
        assert!(
            !results.is_empty(),
            "expected search results in read-only mode"
        );

        // Symbol lookup should also work.
        let syms = engine.symbols("Config", None).unwrap();
        assert!(!syms.is_empty(), "expected symbols in read-only mode");

        drop(dir);
    }

    #[test]
    fn read_only_write_fails_gracefully() {
        let (dir, root) = setup_project();
        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;

        // Build the index first.
        {
            let _engine = Engine::init(&root, config).unwrap();
        }

        // Open in explicit read-only mode.
        let mut engine = Engine::open_read_only(&root).unwrap();
        assert!(engine.is_read_only());

        // reindex_file should fail with ReadOnly.
        let err = engine.reindex_file(Path::new("src/main.rs")).unwrap_err();
        assert!(
            matches!(err, CodixingError::ReadOnly),
            "expected ReadOnly error, got: {err}"
        );

        // remove_file should fail with ReadOnly.
        let err = engine.remove_file(Path::new("src/main.rs")).unwrap_err();
        assert!(
            matches!(err, CodixingError::ReadOnly),
            "expected ReadOnly error, got: {err}"
        );

        // sync should fail with ReadOnly.
        let err = engine.sync().unwrap_err();
        assert!(
            matches!(err, CodixingError::ReadOnly),
            "expected ReadOnly error, got: {err}"
        );

        // apply_changes should fail with ReadOnly.
        let err = engine
            .apply_changes(&[crate::watcher::FileChange {
                path: root.join("src/main.rs"),
                kind: crate::watcher::ChangeKind::Modified,
            }])
            .unwrap_err();
        assert!(
            matches!(err, CodixingError::ReadOnly),
            "expected ReadOnly error, got: {err}"
        );

        drop(dir);
    }

    #[test]
    fn fallback_to_read_only_on_lock() {
        let (dir, root) = setup_project();
        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;

        // Build the index and keep the engine alive (holds the write lock).
        let _engine_rw = Engine::init(&root, config).unwrap();
        assert!(!_engine_rw.is_read_only());

        // Opening a second engine should fall back to read-only mode.
        let engine_ro = Engine::open(&root).unwrap();
        assert!(
            engine_ro.is_read_only(),
            "second engine should be read-only when write lock is held"
        );

        // Verify search works on the read-only instance.
        let results = engine_ro
            .search(
                SearchQuery::new("add")
                    .with_limit(5)
                    .with_strategy(Strategy::Instant),
            )
            .unwrap();
        assert!(
            !results.is_empty(),
            "search should work in fallback read-only mode"
        );

        drop(dir);
    }

    #[test]
    fn read_only_reload_if_stale_no_op_for_writer() {
        let (_dir, mut engine) = setup_engine_bm25_only();
        // A read-write engine should always return false.
        assert!(!engine.reload_if_stale().unwrap());
    }

    #[test]
    fn read_only_reload_if_stale_rate_limits() {
        let (dir, root) = setup_project();
        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;

        // Build the index.
        {
            let _engine = Engine::init(&root, config).unwrap();
        }

        let mut engine = Engine::open_read_only(&root).unwrap();
        assert!(engine.is_read_only());
        engine.set_reload_interval(std::time::Duration::from_secs(60));

        // First call should check (no reload needed since nothing changed).
        assert!(!engine.reload_if_stale().unwrap());

        // Second call within the interval should be rate-limited (return false immediately).
        assert!(!engine.reload_if_stale().unwrap());

        drop(dir);
    }

    #[test]
    #[cfg_attr(windows, ignore)] // Mmap file locking on Windows can prevent reader reload
    fn read_only_engine_reloads_after_writer_update() {
        let (dir, root) = setup_project();
        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;

        // Build the index with the writer engine.
        let mut engine_rw = Engine::init(&root, config).unwrap();

        // Open a read-only copy.
        let mut engine_ro = Engine::open(&root).unwrap();
        assert!(
            engine_ro.is_read_only(),
            "second engine should be read-only when write lock is held"
        );
        engine_ro.set_reload_interval(std::time::Duration::from_secs(0));

        // The reader should not find "unique_reload_function" yet.
        let syms = engine_ro.symbols("unique_reload_function", None).unwrap();
        assert!(syms.is_empty(), "reader should not yet see the new symbol");

        // Add a new file via the writer and persist.
        fs::write(
            root.join("src/new_file.rs"),
            r#"
/// A unique function for reload testing.
pub fn unique_reload_function() -> bool {
    true
}
"#,
        )
        .unwrap();
        engine_rw
            .reindex_file(Path::new("src/new_file.rs"))
            .unwrap();
        engine_rw.save().unwrap();

        // Now the reader should detect staleness and reload.
        let reloaded = engine_ro.reload_if_stale().unwrap();
        assert!(reloaded, "reader should have reloaded");

        // The reader should now find the new symbol.
        let syms = engine_ro.symbols("unique_reload_function", None).unwrap();
        assert!(
            !syms.is_empty(),
            "reader should find the new symbol after reload"
        );

        drop(dir);
    }

    #[test]
    fn streaming_batch_constant_is_reasonable() {
        // STREAM_BATCH_SIZE must be > 0 and within a reasonable range
        // for memory-bounded embedding.
        assert!(STREAM_BATCH_SIZE > 0);
        assert!(STREAM_BATCH_SIZE <= 1024);
    }

    #[test]
    fn streaming_embed_empty_pending_is_noop() {
        use dashmap::DashMap;

        // With an empty pending map, embed_and_index_chunks should return Ok
        // immediately without requiring an embedder or vector index.
        let pending: DashMap<u64, String> = DashMap::new();
        let chunk_meta_map: DashMap<u64, ChunkMeta> = DashMap::new();

        // embed_and_index_chunks returns early for empty pending — no embedder needed.
        assert!(pending.is_empty());
        assert!(chunk_meta_map.is_empty());

        // Verify the constant is exposed and correct.
        assert_eq!(STREAM_BATCH_SIZE, 256);
    }

    #[test]
    fn chunk_meta_content_hash_populated_on_init() {
        let (dir, root) = setup_project();
        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;
        let engine = Engine::init(&root, config).unwrap();

        // Every chunk_meta entry should have a non-zero content_hash.
        let mut total = 0;
        let mut hashed = 0;
        for entry in engine.chunk_meta.iter() {
            total += 1;
            if entry.value().content_hash != 0 {
                hashed += 1;
            }
        }
        assert!(total > 0, "expected at least one chunk");
        assert_eq!(
            total, hashed,
            "all chunk_meta entries should have non-zero content_hash"
        );

        drop(dir);
    }

    #[test]
    fn content_hash_matches_xxh3() {
        let (dir, root) = setup_project();
        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;
        let engine = Engine::init(&root, config).unwrap();

        // Verify each chunk's content_hash matches xxh3 of its content.
        for entry in engine.chunk_meta.iter() {
            let meta = entry.value();
            let expected = xxhash_rust::xxh3::xxh3_64(meta.content.as_bytes());
            assert_eq!(
                meta.content_hash, expected,
                "content_hash mismatch for chunk {} in {}",
                meta.chunk_id, meta.file_path
            );
        }

        drop(dir);
    }

    #[test]
    fn reindex_preserves_content_hash() {
        let (dir, root) = setup_project();
        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;
        let mut engine = Engine::init(&root, config).unwrap();

        // Collect content hashes before reindex.
        let hashes_before: std::collections::HashMap<String, Vec<u64>> = engine
            .chunk_meta
            .iter()
            .fold(std::collections::HashMap::new(), |mut acc, entry| {
                acc.entry(entry.value().file_path.clone())
                    .or_default()
                    .push(entry.value().content_hash);
                acc
            });

        // Reindex without changing file content — hashes should remain consistent.
        engine.reindex_file(Path::new("src/lib.rs")).unwrap();

        let hashes_after: std::collections::HashMap<String, Vec<u64>> = engine
            .chunk_meta
            .iter()
            .filter(|e| e.value().file_path.contains("lib.rs"))
            .fold(std::collections::HashMap::new(), |mut acc, entry| {
                acc.entry(entry.value().file_path.clone())
                    .or_default()
                    .push(entry.value().content_hash);
                acc
            });

        // lib.rs chunks should have the same hashes (content unchanged).
        for (file, mut after_hashes) in hashes_after {
            if let Some(mut before_hashes) = hashes_before.get(&file).cloned() {
                before_hashes.sort();
                after_hashes.sort();
                assert_eq!(
                    before_hashes, after_hashes,
                    "content hashes should be identical for unchanged file"
                );
            }
        }

        drop(dir);
    }

    #[test]
    fn reindex_changes_hash_for_modified_content() {
        let (dir, root) = setup_project();
        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;
        let mut engine = Engine::init(&root, config).unwrap();

        // Collect hashes before.
        let hashes_before: Vec<u64> = engine
            .chunk_meta
            .iter()
            .filter(|e| e.value().file_path.contains("main.rs"))
            .map(|e| e.value().content_hash)
            .collect();

        // Modify main.rs significantly.
        fs::write(
            root.join("src/main.rs"),
            r#"
/// Completely different content.
fn totally_new_main() {
    let x = 42;
    println!("x = {}", x);
}

pub fn another_function(a: f64, b: f64) -> f64 {
    a * b + 1.0
}
"#,
        )
        .unwrap();

        engine.reindex_file(Path::new("src/main.rs")).unwrap();

        let hashes_after: Vec<u64> = engine
            .chunk_meta
            .iter()
            .filter(|e| e.value().file_path.contains("main.rs"))
            .map(|e| e.value().content_hash)
            .collect();

        // At least some hashes should differ (content changed).
        assert!(
            !hashes_after.is_empty(),
            "expected chunks for main.rs after reindex"
        );

        // The hash sets should be different since content was completely replaced.
        let before_set: std::collections::HashSet<u64> = hashes_before.into_iter().collect();
        let after_set: std::collections::HashSet<u64> = hashes_after.into_iter().collect();
        assert_ne!(
            before_set, after_set,
            "content hashes should differ after modifying file content"
        );

        drop(dir);
    }
}

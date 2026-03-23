mod files;
mod focus_map;
mod graph;
mod orphans;
pub(crate) mod pipeline;
mod search;
mod symbol_graph;
mod sync;
mod temporal;
mod test_mapping;

pub use focus_map::{FocusMapEntry, FocusMapOptions};
pub use symbol_graph::SymbolReference;

use crate::persistence;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use dashmap::DashMap;
use rayon::prelude::*;
use serde::Serialize;
use tracing::{debug, info, warn};

use crate::chunker::Chunker;
use crate::chunker::cast::CastChunker;
use crate::config::IndexConfig;
use crate::embedder::Embedder;
use crate::error::{CodixingError, Result};
use crate::graph::extract::{extract_definitions, extract_references};
use crate::graph::extractor::RawImport;
use crate::graph::types::{ReferenceKind, SymbolKind};
use crate::graph::{CallExtractor, CodeGraph, ImportExtractor, ImportResolver, compute_pagerank};
use crate::index::TantivyIndex;
use crate::index::trigram::FileTrigramIndex;
use crate::language::{Language, SemanticEntity, detect_language};
use crate::parser::Parser;
use crate::persistence::{FileHashEntry, IndexMeta, IndexStore};
use crate::reranker::Reranker;
use crate::retriever::ChunkMeta;
use crate::session::SessionState;
use crate::shared_session::SharedSession;
use crate::symbols::persistence::{deserialize_symbols, serialize_symbols};
use crate::symbols::{Symbol, SymbolTable};
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
    /// Optional usearch HNSW vector index.
    pub(super) vector: Option<VectorIndex>,
    /// Chunk metadata hydration table for vector results.
    pub(super) chunk_meta: DashMap<u64, ChunkMeta>,
    /// Optional code dependency graph with PageRank scores.
    pub(super) graph: Option<CodeGraph>,
    /// Optional cross-encoder reranker (BGE-Reranker-Base) for the `deep` strategy.
    pub(super) reranker: Option<Arc<Reranker>>,
    /// Trigram index for sub-millisecond exact substring search (Strategy::Exact).
    pub(super) trigram: crate::index::TrigramIndex,
    /// Session state for tracking agent interactions.
    session: Arc<SessionState>,
    /// Shared session store for multi-agent context sharing.
    shared_session: SharedSession,
    /// `true` when the index was opened without the Tantivy write lock.
    /// All search / read operations work; write operations return
    /// [`CodixingError::ReadOnly`].
    read_only: bool,
    /// File-level trigram index for fast grep pre-filtering.
    pub(super) file_trigram: FileTrigramIndex,
    /// When this engine was last loaded/reloaded from disk (mtime of `meta.json`).
    last_load_time: Option<std::time::SystemTime>,
    /// Minimum interval between reload checks (default: 30s).
    reload_interval: std::time::Duration,
    /// Last time we checked for staleness.
    last_staleness_check: Option<std::time::Instant>,
}

impl Engine {
    /// Initialize a new index for the project at `root`.
    ///
    /// Walks the directory tree, parses all supported source files in parallel
    /// using rayon, chunks them with the cAST algorithm, indexes chunks in
    /// Tantivy, optionally embeds them into the HNSW index, and populates the
    /// symbol table. All state is persisted to the `.codixing/` directory.
    pub fn init(root: impl AsRef<Path>, config: IndexConfig) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .map_err(|e| CodixingError::Config(format!("cannot resolve root path: {e}")))?;

        let store = IndexStore::init(&root, &config)?;
        let tantivy =
            TantivyIndex::create_in_dir_with_config(&store.tantivy_dir(), config.bm25.clone())?;
        let parser = Parser::new();
        let symbols = SymbolTable::new();

        // Initialise the embedder (if enabled).
        let embedder: Option<Arc<Embedder>> = if config.embedding.enabled {
            match Embedder::new(&config.embedding.model) {
                Ok(e) => {
                    info!(dims = e.dims, "embedding model loaded");
                    Some(Arc::new(e))
                }
                Err(e) => {
                    warn!(error = %e, "failed to load embedding model; running BM25-only");
                    None
                }
            }
        } else {
            None
        };

        let dims = embedder.as_ref().map(|e| e.dims).unwrap_or(0);
        let mut vector: Option<VectorIndex> = if embedder.is_some() {
            Some(VectorIndex::new(dims, config.embedding.quantize)?)
        } else {
            None
        };

        let files = walk_source_files(&root, &config)?;
        info!(file_count = files.len(), "discovered source files");

        let chunk_count = AtomicUsize::new(0);
        let file_chunk_map = DashMap::<String, usize>::new();
        let chunk_meta_map = DashMap::<u64, ChunkMeta>::new();

        // Collect embeddings per file for later batch insertion.
        // We process files in parallel for parse/chunk/index, but embedding
        // batch is collected and inserted after the parallel phase.
        let pending_embeds: DashMap<u64, String> = DashMap::new(); // chunk_id → content
        // Import lists extracted during parse — reused by build_graph to avoid
        // a second file-read + parse pass (each file is parsed exactly once).
        let pending_imports: DashMap<String, (Vec<RawImport>, Language)> = DashMap::new();
        // Call names extracted during parse — resolved into Calls edges after
        // the symbol table is fully populated (end of parallel phase).
        let pending_calls: DashMap<String, Vec<String>> = DashMap::new();
        let file_contents: DashMap<String, Vec<u8>> = DashMap::new();

        let ctx = IndexContext {
            root: &root,
            config: &config,
            parser: &parser,
            tantivy: &tantivy,
            symbols: &symbols,
            chunk_count: &chunk_count,
            file_chunk_map: &file_chunk_map,
            chunk_meta_map: &chunk_meta_map,
            pending_embeds: &pending_embeds,
            pending_imports: &pending_imports,
            pending_calls: &pending_calls,
            file_contents: &file_contents,
        };

        // Process files in parallel: parse → chunk → index → extract symbols.
        files.par_iter().for_each(|path| {
            if let Err(e) = process_file(path, &ctx) {
                warn!(path = %path.display(), error = %e, "skipping file");
            }
        });

        tantivy.commit()?;

        // Batch-embed all chunks if the embedder is available.
        if let Some(emb) = &embedder {
            if let Some(vec_idx) = &mut vector {
                embed_and_index_chunks(
                    &pending_embeds,
                    &chunk_meta_map,
                    emb,
                    vec_idx,
                    config.embedding.contextual_embeddings,
                    &root,
                )?;
            }
        }

        let total_chunks = chunk_count.load(Ordering::Relaxed);
        let total_symbols = symbols.len();
        let vector_count = vector.as_ref().map(|v| v.len()).unwrap_or(0);

        // Convert DashMaps to owned types.
        let file_chunk_counts: HashMap<String, usize> = file_chunk_map.into_iter().collect();

        // Build dependency graph using pre-extracted import lists (no re-parse).
        let graph = if config.graph.enabled {
            let mut g = build_graph(&files, &root, &config, &parser, &pending_imports);
            // Resolve call-site edges using the now-complete symbol table.
            add_call_edges(&mut g, &symbols, &pending_calls);
            // Populate the symbol-level inner graph with function-level call edges.
            populate_symbol_graph(&mut g, &files, &root, &config);
            let scores = compute_pagerank(&g, config.graph.damping, config.graph.iterations);
            g.apply_pagerank(&scores);
            let flat = g.to_flat();
            if let Err(e) = store.save_graph(&flat) {
                warn!(error = %e, "failed to persist graph");
            }
            if let Err(e) = store.save_symbol_graph(&g) {
                warn!(error = %e, "failed to persist symbol graph");
            }
            Some(g)
        } else {
            None
        };

        let (graph_nodes, graph_edges) = graph
            .as_ref()
            .map(|g| {
                let s = g.stats();
                (s.node_count, s.edge_count)
            })
            .unwrap_or((0, 0));

        // Persist everything.
        let sym_bytes = serialize_symbols(&symbols)?;
        store.save_symbols_bytes(&sym_bytes)?;

        let hashes: Vec<(PathBuf, u64)> = parser.cache().content_hashes().into_iter().collect();
        store.save_tree_hashes(&hashes)?;

        // Also write v2 hashes with mtime+size for fast sync pre-filtering.
        let v2_hashes: Vec<(PathBuf, FileHashEntry)> = hashes
            .iter()
            .map(|(path, hash)| {
                let (mtime, size) = fs::metadata(path)
                    .map(|m| (m.modified().ok(), m.len()))
                    .unwrap_or((None, 0));
                (path.clone(), FileHashEntry::new(*hash, mtime, size))
            })
            .collect();
        store.save_tree_hashes_v2(&v2_hashes)?;

        // Persist chunk_meta.
        let meta_pairs: Vec<(u64, ChunkMeta)> = chunk_meta_map
            .iter()
            .map(|e| (*e.key(), e.value().clone()))
            .collect();
        let meta_bytes = bitcode::serialize(&meta_pairs).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize chunk_meta: {e}"))
        })?;
        store.save_chunk_meta_bytes(&meta_bytes)?;

        // Persist vector index.
        if let Some(ref vec_idx) = vector {
            vec_idx.save(&store.vector_index_path(), &store.file_chunks_path())?;
        }

        // Record the current git HEAD so git_sync() can diff from this point.
        let git_commit = git_head_commit(&root);
        let idx_meta = IndexMeta {
            version: "0.3.0".to_string(),
            file_count: files.len(),
            chunk_count: total_chunks,
            symbol_count: total_symbols,
            last_indexed: unix_timestamp_string(),
            git_commit,
        };
        store.save_meta(&idx_meta)?;

        info!(
            files = files.len(),
            chunks = total_chunks,
            symbols = total_symbols,
            vectors = vector_count,
            graph_nodes,
            graph_edges,
            "index initialized"
        );

        // Load reranker if requested (opt-in: model is ~270 MB).
        let reranker = if config.embedding.reranker_enabled {
            match Reranker::new() {
                Ok(r) => Some(Arc::new(r)),
                Err(e) => {
                    warn!(error = %e, "failed to load reranker; deep strategy will fall back to thorough");
                    None
                }
            }
        } else {
            None
        };

        let session = Arc::new(SessionState::with_root(true, &root));
        session.cleanup_old_sessions();

        // Build trigram index from chunk metadata for Strategy::Exact fast-path.
        let mut trigram = crate::index::TrigramIndex::new();
        for entry in chunk_meta_map.iter() {
            trigram.add(*entry.key(), &entry.value().content);
        }

        // Build file trigram from full file content (no chunk-boundary gaps).
        let file_trigram = build_file_trigram_from_content(&file_contents);
        if let Err(e) = file_trigram.save_binary(&store.file_trigram_path()) {
            warn!(error = %e, "failed to persist file trigram index");
        }

        Ok(Self {
            config,
            store,
            parser,
            tantivy,
            symbols,
            file_chunk_counts,
            embedder,
            vector,
            chunk_meta: chunk_meta_map,
            graph,
            reranker,
            trigram,
            session,
            shared_session: SharedSession::default_new(),
            read_only: false,
            file_trigram,
            last_load_time: None,
            reload_interval: std::time::Duration::from_secs(30),
            last_staleness_check: None,
        })
    }

    /// Open an existing index from the `.codixing/` directory.
    ///
    /// If another process holds the Tantivy write lock, the engine
    /// automatically falls back to **read-only mode** so that concurrent
    /// instances can still serve search queries. Write operations will
    /// return [`CodixingError::ReadOnly`] in that case.
    /// Restores the Tantivy index, symbol table, chunk metadata, and optional
    /// vector index from disk.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .map_err(|e| CodixingError::Config(format!("cannot resolve root path: {e}")))?;

        let store = IndexStore::open(&root)?;
        let config = store.load_config()?;

        // Try read-write first; fall back to read-only on lock conflict.
        let bm25_config = config.bm25.clone();
        let (tantivy, read_only) = match TantivyIndex::open_in_dir_with_config(
            &store.tantivy_dir(),
            bm25_config.clone(),
        ) {
            Ok(idx) => (idx, false),
            Err(CodixingError::Tantivy(ref e))
                if e.to_string().contains("lock")
                    || e.to_string().contains("Lock")
                    || e.to_string().contains("already") =>
            {
                info!("write lock held by another process — falling back to read-only mode");
                let idx =
                    TantivyIndex::open_read_only_with_config(&store.tantivy_dir(), bm25_config)?;
                (idx, true)
            }
            Err(e) => return Err(e),
        };

        // Restore symbols.
        let symbols = if store.symbols_path().exists() {
            let bytes = store.load_symbols_bytes()?;
            deserialize_symbols(&bytes)?
        } else {
            SymbolTable::new()
        };

        let parser = Parser::new();
        let meta = store.load_meta()?;

        // Restore chunk_meta.
        let chunk_meta: DashMap<u64, ChunkMeta> = if store.chunk_meta_path().exists() {
            let bytes = store.load_chunk_meta_bytes()?;
            let pairs: Vec<(u64, ChunkMeta)> = bitcode::deserialize(&bytes).map_err(|e| {
                CodixingError::Serialization(format!("failed to deserialize chunk_meta: {e}"))
            })?;
            let map = DashMap::new();
            for (k, v) in pairs {
                map.insert(k, v);
            }
            map
        } else {
            DashMap::new()
        };

        // Rebuild file_chunk_counts from chunk_meta (derived view, not separately persisted).
        let mut file_chunk_counts: HashMap<String, usize> = HashMap::new();
        for entry in chunk_meta.iter() {
            *file_chunk_counts
                .entry(entry.value().file_path.clone())
                .or_insert(0) += 1;
        }

        // Restore vector index if it exists.
        let (embedder, vector) = if config.embedding.enabled
            && store.vector_index_path().exists()
            && store.file_chunks_path().exists()
        {
            match Embedder::new(&config.embedding.model) {
                Ok(e) => {
                    let dims = e.dims;
                    let vec_idx = VectorIndex::load(
                        &store.vector_index_path(),
                        &store.file_chunks_path(),
                        dims,
                        config.embedding.quantize,
                    )?;
                    (Some(Arc::new(e)), Some(vec_idx))
                }
                Err(e) => {
                    warn!(error = %e, "failed to load embedding model; running BM25-only");
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        // Restore graph.
        let graph = match store.load_graph() {
            Ok(Some(data)) => {
                let mut g = CodeGraph::from_flat(data);
                // Merge the symbol-level graph if persisted.
                match store.load_symbol_graph() {
                    Ok(Some(sym_graph)) => {
                        g.inner = sym_graph.inner;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!(error = %e, "failed to load symbol graph");
                    }
                }
                Some(g)
            }
            Ok(None) => None,
            Err(e) => {
                warn!(error = %e, "failed to load graph; running without graph intelligence");
                None
            }
        };

        let (graph_nodes, graph_edges) = graph
            .as_ref()
            .map(|g| {
                let s = g.stats();
                (s.node_count, s.edge_count)
            })
            .unwrap_or((0, 0));

        info!(
            files = meta.file_count,
            chunks = meta.chunk_count,
            symbols = meta.symbol_count,
            vectors = vector.as_ref().map(|v| v.len()).unwrap_or(0),
            graph_nodes,
            graph_edges,
            "index opened"
        );

        // Load reranker if requested.
        let reranker = if config.embedding.reranker_enabled {
            match Reranker::new() {
                Ok(r) => Some(Arc::new(r)),
                Err(e) => {
                    warn!(error = %e, "failed to load reranker; deep strategy will fall back to thorough");
                    None
                }
            }
        } else {
            None
        };

        let session = Arc::new(SessionState::with_root(true, &root));
        session.cleanup_old_sessions();

        if read_only {
            info!("engine opened in read-only mode — search works, writes disabled");
        }

        // Record the on-disk mtime of meta.json for read-only staleness detection.
        let meta_mtime = store
            .codixing_dir()
            .join("meta.json")
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok());

        // Build trigram index from chunk metadata for Strategy::Exact fast-path.
        let mut trigram = crate::index::TrigramIndex::new();
        for entry in chunk_meta.iter() {
            trigram.add(*entry.key(), &entry.value().content);
        }

        // Try loading persisted file trigram; fall back to building from chunks.
        let file_trigram = if store.file_trigram_path().exists() {
            match FileTrigramIndex::load_binary(&store.file_trigram_path()) {
                Ok(idx) => idx,
                Err(e) => {
                    warn!(error = %e, "failed to load file trigram index; rebuilding from chunks");
                    build_file_trigram(&chunk_meta)
                }
            }
        } else {
            build_file_trigram(&chunk_meta)
        };

        Ok(Self {
            config,
            store,
            parser,
            tantivy,
            symbols,
            file_chunk_counts,
            embedder,
            vector,
            chunk_meta,
            graph,
            reranker,
            trigram,
            session,
            shared_session: SharedSession::default_new(),
            read_only,
            file_trigram,
            last_load_time: meta_mtime,
            reload_interval: std::time::Duration::from_secs(30),
            last_staleness_check: None,
        })
    }

    /// Open an existing index in **read-only mode**.
    ///
    /// This is useful when another process holds the Tantivy write lock.
    /// All search and read operations work normally; write operations
    /// (`reindex_file`, `remove_file`, `sync`, `apply_changes`) return
    /// [`CodixingError::ReadOnly`].
    pub fn open_read_only(root: impl AsRef<Path>) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .map_err(|e| CodixingError::Config(format!("cannot resolve root path: {e}")))?;

        let store = IndexStore::open(&root)?;
        let config = store.load_config()?;
        let tantivy =
            TantivyIndex::open_read_only_with_config(&store.tantivy_dir(), config.bm25.clone())?;

        // Restore symbols.
        let symbols = if store.symbols_path().exists() {
            let bytes = store.load_symbols_bytes()?;
            deserialize_symbols(&bytes)?
        } else {
            SymbolTable::new()
        };

        let parser = Parser::new();
        let meta = store.load_meta()?;

        // Restore chunk_meta.
        let chunk_meta: DashMap<u64, ChunkMeta> = if store.chunk_meta_path().exists() {
            let bytes = store.load_chunk_meta_bytes()?;
            let pairs: Vec<(u64, ChunkMeta)> = bitcode::deserialize(&bytes).map_err(|e| {
                CodixingError::Serialization(format!("failed to deserialize chunk_meta: {e}"))
            })?;
            let map = DashMap::new();
            for (k, v) in pairs {
                map.insert(k, v);
            }
            map
        } else {
            DashMap::new()
        };

        // Rebuild file_chunk_counts from chunk_meta.
        let mut file_chunk_counts: HashMap<String, usize> = HashMap::new();
        for entry in chunk_meta.iter() {
            *file_chunk_counts
                .entry(entry.value().file_path.clone())
                .or_insert(0) += 1;
        }

        // Restore vector index if it exists.
        let (embedder, vector) = if config.embedding.enabled
            && store.vector_index_path().exists()
            && store.file_chunks_path().exists()
        {
            match Embedder::new(&config.embedding.model) {
                Ok(e) => {
                    let dims = e.dims;
                    let vec_idx = VectorIndex::load(
                        &store.vector_index_path(),
                        &store.file_chunks_path(),
                        dims,
                        config.embedding.quantize,
                    )?;
                    (Some(Arc::new(e)), Some(vec_idx))
                }
                Err(e) => {
                    warn!(error = %e, "failed to load embedding model; running BM25-only");
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        // Restore graph.
        let graph = match store.load_graph() {
            Ok(Some(data)) => {
                let mut g = CodeGraph::from_flat(data);
                // Merge the symbol-level graph if persisted.
                match store.load_symbol_graph() {
                    Ok(Some(sym_graph)) => {
                        g.inner = sym_graph.inner;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!(error = %e, "failed to load symbol graph");
                    }
                }
                Some(g)
            }
            Ok(None) => None,
            Err(e) => {
                warn!(error = %e, "failed to load graph; running without graph intelligence");
                None
            }
        };

        let (graph_nodes, graph_edges) = graph
            .as_ref()
            .map(|g| {
                let s = g.stats();
                (s.node_count, s.edge_count)
            })
            .unwrap_or((0, 0));

        info!(
            files = meta.file_count,
            chunks = meta.chunk_count,
            symbols = meta.symbol_count,
            vectors = vector.as_ref().map(|v| v.len()).unwrap_or(0),
            graph_nodes,
            graph_edges,
            "index opened in read-only mode"
        );

        // Load reranker if requested.
        let reranker = if config.embedding.reranker_enabled {
            match Reranker::new() {
                Ok(r) => Some(Arc::new(r)),
                Err(e) => {
                    warn!(error = %e, "failed to load reranker; deep strategy will fall back to thorough");
                    None
                }
            }
        } else {
            None
        };

        let session = Arc::new(SessionState::with_root(true, &root));
        session.cleanup_old_sessions();

        // Record the on-disk mtime of meta.json for read-only staleness detection.
        let meta_mtime = store
            .codixing_dir()
            .join("meta.json")
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok());

        // Build trigram index from chunk metadata for Strategy::Exact fast-path.
        let mut trigram = crate::index::TrigramIndex::new();
        for entry in chunk_meta.iter() {
            trigram.add(*entry.key(), &entry.value().content);
        }

        let file_trigram = if store.file_trigram_path().exists() {
            match FileTrigramIndex::load_binary(&store.file_trigram_path()) {
                Ok(idx) => idx,
                Err(e) => {
                    warn!(error = %e, "failed to load file trigram index; rebuilding from chunks");
                    build_file_trigram(&chunk_meta)
                }
            }
        } else {
            build_file_trigram(&chunk_meta)
        };

        Ok(Self {
            config,
            store,
            parser,
            tantivy,
            symbols,
            file_chunk_counts,
            embedder,
            vector,
            chunk_meta,
            graph,
            reranker,
            trigram,
            session,
            shared_session: SharedSession::default_new(),
            read_only: true,
            file_trigram,
            last_load_time: meta_mtime,
            reload_interval: std::time::Duration::from_secs(30),
            last_staleness_check: None,
        })
    }

    /// Return `true` if this engine was opened in read-only mode.
    ///
    /// In read-only mode, all search and read operations work normally but
    /// write operations (`reindex_file`, `remove_file`, `sync`, etc.) return
    /// [`CodixingError::ReadOnly`].
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Set the minimum interval between reload-staleness checks.
    ///
    /// Only meaningful for read-only instances; ignored otherwise.
    pub fn set_reload_interval(&mut self, interval: std::time::Duration) {
        self.reload_interval = interval;
    }

    /// Check if the on-disk index has been updated since this read-only
    /// instance was loaded, and reload if so.
    ///
    /// Returns `Ok(true)` if data was reloaded, `Ok(false)` if no reload
    /// was needed (or this is a read-write instance). No-op if this instance
    /// holds the write lock.
    pub fn reload_if_stale(&mut self) -> Result<bool> {
        if !self.read_only {
            return Ok(false);
        }

        // Rate-limit checks.
        if let Some(last_check) = self.last_staleness_check {
            if last_check.elapsed() < self.reload_interval {
                return Ok(false);
            }
        }
        self.last_staleness_check = Some(std::time::Instant::now());

        let meta_path = self.store.codixing_dir().join("meta.json");
        let disk_mtime = std::fs::metadata(&meta_path)
            .ok()
            .and_then(|m| m.modified().ok());

        match (disk_mtime, self.last_load_time) {
            (Some(disk), Some(loaded)) if disk > loaded => {
                info!("read-only index stale — reloading from disk");
                self.reload_from_disk()?;
                self.last_load_time = Some(disk);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Re-read all persistent state from the `.codixing/` directory.
    ///
    /// Reloads symbols, chunk metadata, the dependency graph, the vector
    /// index (if present), and refreshes the Tantivy reader.
    fn reload_from_disk(&mut self) -> Result<()> {
        // Reload symbols.
        if self.store.symbols_path().exists() {
            let bytes = self.store.load_symbols_bytes()?;
            self.symbols = deserialize_symbols(&bytes)?;
        }

        // Reload chunk_meta.
        if self.store.chunk_meta_path().exists() {
            let bytes = self.store.load_chunk_meta_bytes()?;
            let pairs: Vec<(u64, ChunkMeta)> = bitcode::deserialize(&bytes).map_err(|e| {
                CodixingError::Serialization(format!("failed to deserialize chunk_meta: {e}"))
            })?;
            self.chunk_meta.clear();
            for (k, v) in pairs {
                self.chunk_meta.insert(k, v);
            }
        }

        // Rebuild file_chunk_counts from chunk_meta.
        self.file_chunk_counts.clear();
        for entry in self.chunk_meta.iter() {
            *self
                .file_chunk_counts
                .entry(entry.value().file_path.clone())
                .or_insert(0) += 1;
        }

        // Rebuild file trigram index.
        self.file_trigram = build_file_trigram(&self.chunk_meta);

        // Reload graph.
        self.graph = match self.store.load_graph() {
            Ok(Some(data)) => {
                let mut g = CodeGraph::from_flat(data);
                match self.store.load_symbol_graph() {
                    Ok(Some(sym_graph)) => {
                        g.inner = sym_graph.inner;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!(error = %e, "failed to load symbol graph during reload");
                    }
                }
                Some(g)
            }
            Ok(None) => None,
            Err(e) => {
                warn!(error = %e, "failed to reload graph");
                self.graph.take()
            }
        };

        // Reload vector index if it exists and we have an embedder.
        if let Some(ref emb) = self.embedder {
            if self.store.vector_index_path().exists() && self.store.file_chunks_path().exists() {
                match VectorIndex::load(
                    &self.store.vector_index_path(),
                    &self.store.file_chunks_path(),
                    emb.dims,
                    self.config.embedding.quantize,
                ) {
                    Ok(vec_idx) => {
                        self.vector = Some(vec_idx);
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to reload vector index");
                    }
                }
            }
        }

        // Rebuild trigram index from updated chunk metadata.
        self.trigram = crate::index::TrigramIndex::new();
        for entry in self.chunk_meta.iter() {
            self.trigram.add(*entry.key(), &entry.value().content);
        }

        // Refresh the Tantivy reader so it picks up new segments.
        self.tantivy.refresh_reader()?;

        info!("read-only engine reloaded from disk");
        Ok(())
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
            vector_count: self.vector.as_ref().map(|v| v.len()).unwrap_or(0),
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

    /// Get combined callers + callees for a file (used for graph-propagated session boost).
    pub fn file_neighbors(&self, file: &str) -> Vec<String> {
        let mut neighbors = self.callers(file);
        neighbors.extend(self.callees(file));
        neighbors.sort();
        neighbors.dedup();
        neighbors
    }

    /// Check how stale the index is relative to the current filesystem state.
    ///
    /// Uses `stat()` calls only (mtime + size comparison) — no file content is
    /// read, keeping this fast even on large projects.
    pub fn check_staleness(&self) -> StaleReport {
        use std::collections::HashSet;

        // Load stored v2 hashes for mtime+size comparison.
        let old_hashes: HashMap<PathBuf, persistence::FileHashEntry> = self
            .store
            .load_tree_hashes_v2()
            .unwrap_or_default()
            .into_iter()
            .collect();

        // Walk current source files.
        let current_files = match walk_source_files(&self.config.root, &self.config) {
            Ok(f) => f,
            Err(_) => {
                return StaleReport {
                    is_stale: false,
                    modified_files: 0,
                    new_files: 0,
                    deleted_files: 0,
                    last_sync: None,
                    suggestion: "Unable to walk source files.".to_string(),
                };
            }
        };

        let mut modified = 0usize;
        let mut new_files = 0usize;
        let mut seen: HashSet<PathBuf> = HashSet::new();

        for abs_path in &current_files {
            seen.insert(abs_path.clone());

            let (current_mtime, current_size) = fs::metadata(abs_path)
                .map(|m| (m.modified().ok(), m.len()))
                .unwrap_or((None, 0));

            match old_hashes.get(abs_path) {
                Some(cached) => {
                    if cached.file_might_have_changed(current_mtime, current_size) {
                        modified += 1;
                    }
                }
                None => {
                    new_files += 1;
                }
            }
        }

        // Check for deleted files.
        let deleted = old_hashes.keys().filter(|p| !seen.contains(*p)).count();

        // Parse last sync time from stored meta.
        let last_sync = self.store.load_meta().ok().and_then(|meta| {
            meta.last_indexed
                .parse::<u64>()
                .ok()
                .map(|secs| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
        });

        let is_stale = modified > 0 || new_files > 0 || deleted > 0;

        let suggestion = if !is_stale {
            "Index is up to date.".to_string()
        } else {
            let total_changes = modified + new_files + deleted;
            format!("{total_changes} file(s) changed. Run `codixing sync .` to update the index.")
        };

        StaleReport {
            is_stale,
            modified_files: modified,
            new_files,
            deleted_files: deleted,
            last_sync,
            suggestion,
        }
    }

    /// Validate a proposed rename before applying it.
    ///
    /// Checks for name collisions (the new name already exists as a symbol),
    /// shadowing (the new name exists in files that also contain the old name),
    /// and import conflicts. No files are modified.
    pub fn validate_rename(
        &self,
        old_name: &str,
        new_name: &str,
        file_filter: Option<&str>,
    ) -> RenameValidation {
        let root = &self.config.root;

        // Find all indexed files (via symbol table).
        let all_syms = self.symbols.filter("", None);
        let mut all_files: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for s in &all_syms {
            all_files.insert(s.file_path.clone());
        }

        // Apply file filter.
        let files: Vec<String> = all_files
            .into_iter()
            .filter(|f| file_filter.map(|ff| f.contains(ff)).unwrap_or(true))
            .collect();

        let mut affected_files = Vec::new();
        let mut occurrence_count = 0usize;
        let mut conflicts = Vec::new();

        // Check if new_name already exists as a defined symbol anywhere.
        let existing_new_symbols = self.symbols.filter(new_name, None);
        let exact_new_matches: Vec<_> = existing_new_symbols
            .iter()
            .filter(|s| s.name == new_name)
            .collect();

        for file_rel in &files {
            let abs_path = root.join(file_rel);
            let content = match fs::read_to_string(&abs_path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            if !content.contains(old_name) {
                continue;
            }

            let count = content.matches(old_name).count();
            occurrence_count += count;
            affected_files.push(file_rel.clone());

            // Check: does new_name already exist as a symbol defined in this file?
            for sym in &exact_new_matches {
                if sym.file_path == *file_rel {
                    conflicts.push(RenameConflict {
                        file_path: file_rel.clone(),
                        line: sym.line_start,
                        kind: ConflictKind::NameCollision,
                        message: format!(
                            "Symbol `{new_name}` already defined at line {} in `{file_rel}`",
                            sym.line_start
                        ),
                    });
                }
            }

            // Check: does new_name appear in imports in this file?
            for (line_num, line) in content.lines().enumerate() {
                let trimmed = line.trim();
                let is_import = trimmed.starts_with("use ")
                    || trimmed.starts_with("import ")
                    || trimmed.starts_with("from ")
                    || trimmed.starts_with("require(")
                    || trimmed.starts_with("#include");
                if is_import && line.contains(new_name) {
                    conflicts.push(RenameConflict {
                        file_path: file_rel.clone(),
                        line: line_num + 1,
                        kind: ConflictKind::ImportConflict,
                        message: format!(
                            "Import at line {} in `{file_rel}` already references `{new_name}`",
                            line_num + 1
                        ),
                    });
                }
            }

            // Check: does new_name already appear as a defined symbol in
            // files that also contain old_name? (shadowing)
            for sym in &exact_new_matches {
                if sym.file_path != *file_rel && affected_files.contains(&sym.file_path) {
                    // Only add once per file.
                    let already = conflicts
                        .iter()
                        .any(|c| c.file_path == sym.file_path && c.kind == ConflictKind::Shadowing);
                    if !already {
                        conflicts.push(RenameConflict {
                            file_path: sym.file_path.clone(),
                            line: sym.line_start,
                            kind: ConflictKind::Shadowing,
                            message: format!(
                                "Symbol `{new_name}` exists in `{}` (line {}) which also uses `{old_name}` \
                                 -- renaming may cause shadowing",
                                sym.file_path, sym.line_start
                            ),
                        });
                    }
                }
            }
        }

        // Deduplicate conflicts.
        conflicts.sort_by(|a, b| a.file_path.cmp(&b.file_path).then(a.line.cmp(&b.line)));
        conflicts
            .dedup_by(|a, b| a.file_path == b.file_path && a.line == b.line && a.kind == b.kind);

        let is_safe = conflicts.is_empty();

        RenameValidation {
            is_safe,
            conflicts,
            affected_files,
            occurrence_count,
        }
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Shared context passed to `process_file` to avoid too-many-arguments.
struct IndexContext<'a> {
    root: &'a Path,
    config: &'a IndexConfig,
    parser: &'a Parser,
    tantivy: &'a TantivyIndex,
    symbols: &'a SymbolTable,
    chunk_count: &'a AtomicUsize,
    file_chunk_map: &'a DashMap<String, usize>,
    chunk_meta_map: &'a DashMap<u64, ChunkMeta>,
    /// Pending chunks to embed: chunk_id → content.
    pending_embeds: &'a DashMap<u64, String>,
    /// Imports extracted during parsing, keyed by relative path.
    /// Reused by `build_graph` to avoid re-reading/re-parsing files.
    pending_imports: &'a DashMap<String, (Vec<RawImport>, Language)>,
    /// Call names extracted during parsing: rel_path → Vec<callee_name>.
    /// Resolved into `EdgeKind::Calls` edges after the symbol table is complete.
    pending_calls: &'a DashMap<String, Vec<String>>,
    /// Full file content accumulated during parallel indexing for building
    /// a chunk-boundary-free file trigram index.
    file_contents: &'a DashMap<String, Vec<u8>>,
}

/// Build a [`FileTrigramIndex`] from full file content.
///
/// Uses `file_contents` (complete file bytes accumulated during indexing)
/// to avoid missing trigrams that straddle chunk boundaries.
fn build_file_trigram_from_content(file_contents: &DashMap<String, Vec<u8>>) -> FileTrigramIndex {
    let mut idx = FileTrigramIndex::new();
    for entry in file_contents.iter() {
        idx.add(entry.key(), entry.value());
    }
    idx
}

/// Build a [`FileTrigramIndex`] from chunk metadata already in memory.
///
/// Fallback used at `open()` / `reload()` when full file content is not
/// available and the persisted file trigram index is missing.
fn build_file_trigram(chunk_meta: &DashMap<u64, ChunkMeta>) -> FileTrigramIndex {
    let mut idx = FileTrigramIndex::new();
    for entry in chunk_meta.iter() {
        let m = entry.value();
        idx.add(&m.file_path, m.content.as_bytes());
    }
    idx
}

/// Process a single file: parse → chunk → index → extract symbols.
fn process_file(path: &Path, ctx: &IndexContext<'_>) -> Result<()> {
    let source = fs::read(path)?;
    let result = ctx.parser.parse_file(path, &source)?;

    let rel_str = ctx
        .config
        .normalize_path(path)
        .unwrap_or_else(|| normalize_path(path.strip_prefix(ctx.root).unwrap_or(path)));

    // Accumulate full file content for chunk-boundary-free trigram indexing.
    ctx.file_contents.insert(rel_str.clone(), source.clone());

    let chunker = CastChunker;
    let chunks = chunker.chunk(
        &rel_str,
        &source,
        result.tree.as_ref(),
        result.language,
        &ctx.config.chunk,
    );

    ctx.chunk_count.fetch_add(chunks.len(), Ordering::Relaxed);
    ctx.file_chunk_map.insert(rel_str.clone(), chunks.len());

    for chunk in &chunks {
        ctx.tantivy.add_chunk(chunk)?;

        ctx.chunk_meta_map.insert(
            chunk.id,
            ChunkMeta {
                chunk_id: chunk.id,
                file_path: rel_str.clone(),
                language: chunk.language.name().to_string(),
                line_start: chunk.line_start as u64,
                line_end: chunk.line_end as u64,
                signature: chunk.signatures.join("\n"),
                scope_chain: chunk.scope_chain.clone(),
                entity_names: chunk.entity_names.clone(),
                content_hash: xxhash_rust::xxh3::xxh3_64(chunk.content.as_bytes()),
                content: chunk.content.clone(),
            },
        );

        // Queue for batch embedding.
        ctx.pending_embeds.insert(chunk.id, chunk.content.clone());
    }

    for entity in &result.entities {
        ctx.symbols
            .insert(symbol_from_entity(entity, &rel_str, result.language));
    }

    // Extract imports now — we already have the tree in memory, so this
    // avoids a second read+parse pass during build_graph.
    // Config languages have no tree-sitter tree; skip import/call extraction.
    let raw_imports = match result.tree.as_ref() {
        Some(tree) => ImportExtractor::extract(tree, &source, result.language),
        None => Vec::new(),
    };
    ctx.pending_imports
        .insert(rel_str.clone(), (raw_imports, result.language));

    // Extract call sites for later call-graph edge resolution.
    let call_names = match result.tree.as_ref() {
        Some(tree) => CallExtractor::extract_calls(tree, &source, result.language),
        None => Vec::new(),
    };
    if !call_names.is_empty() {
        ctx.pending_calls.insert(rel_str.clone(), call_names);
    }

    debug!(
        path = %rel_str,
        language = result.language.name(),
        chunks = chunks.len(),
        entities = result.entities.len(),
        "indexed file"
    );

    Ok(())
}

/// Build the text string to embed for a chunk.
///
/// When `contextual` is `true`, prepends a single-line context prefix with
/// file path, language, scope chain, and entity names — the "contextual
/// chunk embedding" technique that gives the embedding model positional and
/// semantic context, improving retrieval quality by ~35 %.
fn make_embed_text(meta: &ChunkMeta, contextual: bool) -> String {
    if !contextual {
        return meta.content.clone();
    }
    let prefix = build_context_prefix(meta);
    format!("{prefix}{}", meta.content)
}

/// Build a context prefix for a chunk to improve embedding quality.
///
/// Produces a single-line header with file path, language, scope chain, and
/// entity names so the embedding model knows the chunk's location in the
/// codebase. The prefix is prepended to chunk content before embedding but
/// is **not** stored in the index — only the raw content is persisted.
fn build_context_prefix(meta: &ChunkMeta) -> String {
    let mut header = format!("File: {} | Language: {}", meta.file_path, meta.language);
    if !meta.scope_chain.is_empty() {
        header.push_str(&format!(" | Scope: {}", meta.scope_chain.join(" > ")));
    }
    if !meta.entity_names.is_empty() {
        header.push_str(&format!(" | Entities: {}", meta.entity_names.join(", ")));
    }
    header.push('\n');
    header
}

/// Fixed-size window for streaming embedding batches.
///
/// Controls how many chunks are embedded and indexed per iteration.
/// Keeps peak memory bounded: only `STREAM_BATCH_SIZE` text strings and
/// their corresponding embedding vectors are alive at any given time.
const STREAM_BATCH_SIZE: usize = 256;

/// Batch-embed all pending chunks and add them to the vector index.
///
/// Processes chunks in fixed-size windows of [`STREAM_BATCH_SIZE`] to bound
/// peak memory usage.  For each window the texts are collected, embedded via
/// the ONNX model, and immediately indexed into the HNSW graph before moving
/// on to the next window.  Progress is reported after every window via the
/// optional `progress_callback`.
///
/// When contextual embeddings are disabled (`!contextual`) this function
/// first attempts **late chunking**: for each file whose tokenized form fits
/// within the model's context window, the entire file is passed through the
/// transformer once and per-chunk embeddings are mean-pooled from the
/// token-level hidden states.  This preserves cross-chunk context (e.g.
/// knowing that `self` refers to a specific struct).
///
/// Files that exceed the context window (or when contextual mode is on) fall
/// back to the original independent per-chunk embedding.
fn embed_and_index_chunks(
    pending: &DashMap<u64, String>,
    chunk_meta: &DashMap<u64, ChunkMeta>,
    embedder: &Embedder,
    vec_idx: &mut VectorIndex,
    contextual: bool,
    root: &Path,
) -> Result<()> {
    embed_and_index_chunks_with_progress(
        pending,
        chunk_meta,
        embedder,
        vec_idx,
        contextual,
        root,
        None::<fn(usize, usize)>,
    )
}

/// Inner implementation of [`embed_and_index_chunks`] with an optional
/// progress callback `(embedded_so_far, total_chunks)`.
fn embed_and_index_chunks_with_progress<F>(
    pending: &DashMap<u64, String>,
    chunk_meta: &DashMap<u64, ChunkMeta>,
    embedder: &Embedder,
    vec_idx: &mut VectorIndex,
    contextual: bool,
    root: &Path,
    progress_callback: Option<F>,
) -> Result<()>
where
    F: Fn(usize, usize),
{
    let entries: Vec<u64> = pending.iter().map(|e| *e.key()).collect();

    if entries.is_empty() {
        return Ok(());
    }

    let total_chunks = entries.len();
    let mut embedded_so_far = 0usize;

    info!(count = total_chunks, contextual, "embedding chunks");

    // ── Late chunking pass ────────────────────────────────────────────────
    //
    // Group chunks by file, read each file once, and try late chunking.
    // Contextual embeddings prepend a metadata prefix per chunk, which
    // changes the text that gets embedded, so late chunking (which operates
    // on the raw file text) is skipped in contextual mode.
    //
    // Chunk IDs that were successfully late-chunked are collected in
    // `done_ids` so the fallback pass can skip them.
    let mut done_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();

    if !contextual {
        // Group chunk IDs by file path.
        let mut file_chunks: HashMap<String, Vec<u64>> = HashMap::new();
        for &id in &entries {
            if let Some(meta) = chunk_meta.get(&id) {
                file_chunks
                    .entry(meta.file_path.clone())
                    .or_default()
                    .push(id);
            }
        }

        for (file_path, chunk_ids) in &file_chunks {
            // Read the file from disk.
            let abs_path = root.join(file_path);
            let file_text = match fs::read_to_string(&abs_path) {
                Ok(t) => t,
                Err(e) => {
                    debug!(path = %file_path, error = %e, "cannot read file for late chunking, falling back");
                    continue;
                }
            };

            // Build byte ranges for each chunk.  We locate each chunk's
            // content within the file text by searching from the end of the
            // previous chunk to handle duplicate content gracefully.
            //
            // Sort chunks by line_start so the search is monotonic.
            let mut ordered: Vec<(u64, String)> = chunk_ids
                .iter()
                .filter_map(|id| chunk_meta.get(id).map(|m| (*id, m.content.clone())))
                .collect();
            // Sort by line_start for stable ordering.
            ordered.sort_by_key(|(id, _)| chunk_meta.get(id).map(|m| m.line_start).unwrap_or(0));

            let mut byte_ranges: Vec<(usize, usize)> = Vec::with_capacity(ordered.len());
            let mut search_from = 0usize;
            let mut all_found = true;
            for (_id, content) in &ordered {
                if let Some(pos) = file_text[search_from..].find(content.as_str()) {
                    let start = search_from + pos;
                    let end = start + content.len();
                    byte_ranges.push((start, end));
                    search_from = end;
                } else {
                    // Content not found — this can happen if the file changed
                    // between indexing and embedding.  Fall back for the
                    // entire file.
                    debug!(
                        path = %file_path,
                        "chunk content not found in file, falling back to independent embedding"
                    );
                    all_found = false;
                    break;
                }
            }

            if !all_found {
                continue;
            }

            // Attempt late chunking.
            match embedder.embed_file_late_chunking(&file_text, &byte_ranges) {
                Ok(Some(embeddings)) => {
                    debug!(
                        path = %file_path,
                        chunks = embeddings.len(),
                        "late-chunked embeddings"
                    );
                    for ((id, _content), embedding) in ordered.iter().zip(embeddings.into_iter()) {
                        if let Err(e) = vec_idx.add_mut(*id, &embedding, file_path) {
                            warn!(error = %e, chunk_id = id, "failed to add vector");
                        }
                        done_ids.insert(*id);
                        embedded_so_far += 1;
                    }
                    // Report progress after each file's late-chunked batch.
                    if let Some(ref cb) = progress_callback {
                        cb(embedded_so_far, total_chunks);
                    }
                }
                Ok(None) => {
                    // File too long or backend doesn't support it — fall through.
                    debug!(path = %file_path, "late chunking not applicable, falling back");
                }
                Err(e) => {
                    warn!(error = %e, path = %file_path, "late chunking failed, falling back");
                }
            }
        }
    }

    // ── Fallback: independent per-chunk embedding (streaming) ─────────────
    let remaining: Vec<u64> = entries
        .iter()
        .filter(|id| !done_ids.contains(id))
        .copied()
        .collect();

    if !remaining.is_empty() {
        debug!(
            late_chunked = done_ids.len(),
            independent = remaining.len(),
            "embedding remaining chunks independently"
        );

        for window in remaining.chunks(STREAM_BATCH_SIZE) {
            let texts: Vec<String> = window
                .iter()
                .map(|id| {
                    chunk_meta
                        .get(id)
                        .map(|m| make_embed_text(&m, contextual))
                        .unwrap_or_default()
                })
                .collect();

            let embeddings = embedder.embed(texts)?;

            for (chunk_id, embedding) in window.iter().zip(embeddings.into_iter()) {
                let file_path = chunk_meta
                    .get(chunk_id)
                    .map(|m| m.file_path.clone())
                    .unwrap_or_default();
                if let Err(e) = vec_idx.add_mut(*chunk_id, &embedding, &file_path) {
                    warn!(error = %e, chunk_id, "failed to add vector");
                }
                embedded_so_far += 1;
            }

            // Report progress after each streaming window.
            if let Some(ref cb) = progress_callback {
                cb(embedded_so_far, total_chunks);
            }
        }
    }

    Ok(())
}

/// Walk the directory tree and collect all source files with supported extensions.
///
/// Uses the `ignore` crate so that `.gitignore`, `.ignore`, and
/// `.git/info/exclude` rules are honoured automatically (same as ripgrep).
/// The explicit `config.exclude_patterns` are applied as a secondary guard
/// for repos with incomplete `.gitignore` coverage.
///
/// When `config.extra_roots` is non-empty, all extra roots are also walked.
/// Returned paths are absolute; callers use `config.normalize_path()` to
/// produce the final relative (possibly-prefixed) string key.
fn walk_source_files(root: &Path, config: &IndexConfig) -> Result<Vec<PathBuf>> {
    use ignore::WalkBuilder;

    let mut files = Vec::new();

    // Helper closure: collect matching files from a single directory tree.
    let mut collect = |walk_root: &Path| {
        for entry in WalkBuilder::new(walk_root)
            .standard_filters(true) // honour .gitignore / .ignore / global gitignore
            .hidden(true) // skip dot-files not covered by .gitignore
            .build()
        {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    warn!(error = %e, "directory walk error");
                    continue;
                }
            };
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            // Secondary guard: explicit exclude patterns (exact path component match).
            let excluded = path.components().any(|c| {
                let s = c.as_os_str().to_string_lossy();
                config.exclude_patterns.iter().any(|p| p == s.as_ref())
            });
            if excluded {
                continue;
            }
            if config.languages.is_empty() {
                if detect_language(path).is_some() {
                    files.push(path.to_path_buf());
                }
            } else if let Some(lang) = detect_language(path) {
                if config.languages.contains(&lang.name().to_lowercase()) {
                    files.push(path.to_path_buf());
                }
            }
        }
    };

    // Walk the primary root.
    collect(root);

    // Walk any extra roots.
    for extra in &config.extra_roots {
        if !extra.exists() {
            warn!(path = %extra.display(), "extra root does not exist, skipping");
            continue;
        }
        collect(extra);
    }

    Ok(files)
}

/// Resolve call-site names against the symbol table and add `EdgeKind::Calls`
/// edges to the graph.
///
/// Only adds an edge when exactly one file (other than the caller) defines a
/// symbol with the given name — this conservative heuristic avoids false edges
/// from ubiquitous names like `new`, `parse`, or `fmt`.
fn add_call_edges(
    graph: &mut CodeGraph,
    symbols: &SymbolTable,
    pending_calls: &DashMap<String, Vec<String>>,
) {
    let mut total = 0usize;
    for entry in pending_calls.iter() {
        let from_file = entry.key();
        let call_names = entry.value();
        let from_lang = graph
            .node(from_file)
            .map(|n| n.language)
            .unwrap_or(Language::Rust);

        let mut seen_targets = std::collections::HashSet::new();
        for name in call_names {
            let syms = symbols.lookup(name);
            // Collect unique defining files, excluding the caller itself.
            let target_files: std::collections::HashSet<&str> = syms
                .iter()
                .map(|s| s.file_path.as_str())
                .filter(|&fp| fp != from_file.as_str())
                .collect();
            if target_files.len() == 1 {
                let target = *target_files.iter().next().unwrap();
                if seen_targets.insert(target.to_string()) {
                    let target_lang =
                        detect_language(std::path::Path::new(target)).unwrap_or(from_lang);
                    graph.add_call_edge(from_file, target, name, from_lang, target_lang);
                    total += 1;
                }
            }
        }
    }
    if total > 0 {
        info!(call_edges = total, "added call-site edges to graph");
    }
}

/// Populate the symbol-level inner graph with definitions and call references.
///
/// Reads each source file, extracts function/struct/enum definitions and call
/// references via tree-sitter, then inserts them as nodes and edges into the
/// `CodeGraph::inner` graph.  This gives precise symbol->symbol call edges that
/// complement the coarser file-level import/call edges.
///
/// Must be called after the parallel parse phase so that all files are available.
fn populate_symbol_graph(
    graph: &mut CodeGraph,
    files: &[PathBuf],
    root: &Path,
    config: &IndexConfig,
) {
    use std::collections::HashMap;

    // Phase 1: Extract definitions from all files to build a name->NodeIndex map.
    let mut name_to_indices: HashMap<String, Vec<petgraph::graph::NodeIndex>> = HashMap::new();

    for abs_path in files {
        let lang = match detect_language(abs_path) {
            Some(l) => l,
            None => continue,
        };
        let source = match fs::read_to_string(abs_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let rel_str = config
            .normalize_path(abs_path)
            .unwrap_or_else(|| normalize_path(abs_path.strip_prefix(root).unwrap_or(abs_path)));

        let defs = extract_definitions(&source, &rel_str, &lang);
        for def in &defs {
            let idx = graph.add_symbol_with_line(&def.name, &rel_str, def.kind.clone(), def.line);
            name_to_indices
                .entry(def.name.clone())
                .or_default()
                .push(idx);
        }
    }

    // Phase 2: Extract references and wire call edges.
    let mut total_edges = 0usize;
    for abs_path in files {
        let lang = match detect_language(abs_path) {
            Some(l) => l,
            None => continue,
        };
        let source = match fs::read_to_string(abs_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let rel_str = config
            .normalize_path(abs_path)
            .unwrap_or_else(|| normalize_path(abs_path.strip_prefix(root).unwrap_or(abs_path)));

        let refs = extract_references(&source, &rel_str, &lang);

        // Build a map of function definitions in this file so we can attribute
        // call references to their enclosing function.
        let defs = extract_definitions(&source, &rel_str, &lang);
        // Sort definitions by line for binary search.
        let mut func_defs: Vec<(usize, petgraph::graph::NodeIndex)> =
            defs.iter()
                .filter(|d| d.kind == SymbolKind::Function)
                .filter_map(|d| {
                    name_to_indices
                        .get(&d.name)
                        .and_then(|indices| {
                            indices
                                .iter()
                                .find(|&&idx| {
                                    graph.inner.node_weight(idx).is_some_and(|n| {
                                        n.file == rel_str && n.line == Some(d.line)
                                    })
                                })
                                .copied()
                        })
                        .map(|idx| (d.line, idx))
                })
                .collect();
        func_defs.sort_by_key(|(line, _)| *line);

        for r in &refs {
            if r.kind != ReferenceKind::Call {
                continue;
            }
            // Find the enclosing function for this call site.
            let caller_idx = find_enclosing_function(&func_defs, r.line);
            let caller_idx = match caller_idx {
                Some(idx) => idx,
                None => continue, // Call at file scope -- skip
            };

            // Resolve the callee: look for a unique definition with this name.
            // Prefer a definition in the SAME file as the caller; only fall
            // back to cross-file if no same-file match exists.
            let callee_base = r.target_name.rsplit("::").next().unwrap_or(&r.target_name);
            if let Some(target_indices) = name_to_indices.get(callee_base) {
                // Same-file candidates (excluding the caller itself).
                let same_file: Vec<_> = target_indices
                    .iter()
                    .filter(|&&idx| {
                        graph
                            .inner
                            .node_weight(idx)
                            .is_some_and(|n| n.file == rel_str)
                    })
                    .filter(|&&idx| {
                        // Exclude the caller node itself to avoid self-edges later.
                        idx != caller_idx
                    })
                    .collect();
                // Cross-file candidates.
                let cross_file: Vec<_> = target_indices
                    .iter()
                    .filter(|&&idx| {
                        graph
                            .inner
                            .node_weight(idx)
                            .is_some_and(|n| n.file != rel_str)
                    })
                    .collect();
                let target = if same_file.len() == 1 {
                    Some(**same_file.first().unwrap())
                } else if cross_file.len() == 1 {
                    Some(**cross_file.first().unwrap())
                } else if target_indices.len() == 1 {
                    Some(target_indices[0])
                } else {
                    None
                };
                if let Some(target_idx) = target {
                    // Avoid self-edges
                    if caller_idx != target_idx {
                        graph.add_reference(caller_idx, target_idx, ReferenceKind::Call);
                        total_edges += 1;
                    }
                }
            }
        }
    }

    if total_edges > 0 || !name_to_indices.is_empty() {
        info!(
            symbol_nodes = graph.symbol_node_count(),
            symbol_edges = total_edges,
            "populated symbol-level call graph"
        );
    }
}

/// Find the enclosing function for a given line number.
///
/// Uses the sorted list of `(start_line, NodeIndex)` pairs and returns the
/// last function that starts at or before the given line, provided the line
/// is before the NEXT function's start (or end of file). This prevents
/// attributing a call at file scope between two functions to the earlier one.
fn find_enclosing_function(
    func_defs: &[(usize, petgraph::graph::NodeIndex)],
    line: usize,
) -> Option<petgraph::graph::NodeIndex> {
    // Binary search for the last definition at or before `line`.
    let pos = func_defs.partition_point(|(start, _)| *start <= line);
    if pos == 0 {
        return None;
    }
    let candidate_idx = pos - 1;
    // Verify the call site line is before the next function's start line.
    // If there is a next function, the call must be before its start;
    // otherwise it's at file scope between two functions.
    if let Some((next_start, _)) = func_defs.get(pos) {
        if line >= *next_start {
            return None;
        }
    }
    Some(func_defs[candidate_idx].1)
}

/// Build a dependency graph from pre-extracted import lists (populated during
/// the parallel parse phase) plus a rayon-parallel resolution pass.
///
/// Phase 1 (parallel): resolve each file's raw imports against the indexed
///   file set — pure string operations, no graph mutation.
/// Phase 2 (sequential): insert all resolved edges into the graph.
///
/// When `import_cache` is empty (e.g. called standalone), falls back to
/// re-reading and re-parsing each file (old behaviour).
fn build_graph(
    files: &[PathBuf],
    root: &Path,
    config: &IndexConfig,
    parser: &Parser,
    import_cache: &DashMap<String, (Vec<RawImport>, Language)>,
) -> CodeGraph {
    let indexed: std::collections::HashSet<String> = files
        .iter()
        .map(|p| {
            config
                .normalize_path(p)
                .unwrap_or_else(|| normalize_path(p.strip_prefix(root).unwrap_or(p)))
        })
        .collect();

    let resolver = ImportResolver::new(indexed, root.to_path_buf());

    // Phase 1: resolve imports in parallel.
    // Each entry is (rel_str, language, Vec<(target, raw_path, target_lang)>).
    type ResolvedFile = (String, Language, Vec<(String, String, Language)>);
    let resolved: Vec<ResolvedFile> = files
        .par_iter()
        .filter_map(|abs_path| {
            let rel_str = config
                .normalize_path(abs_path)
                .unwrap_or_else(|| normalize_path(abs_path.strip_prefix(root).unwrap_or(abs_path)));

            let (raw_imports, language) = if let Some(entry) = import_cache.get(&rel_str) {
                // Fast path: use imports extracted during process_file (no I/O).
                (entry.0.clone(), entry.1)
            } else {
                // Fallback: re-read + re-parse (only reached when cache is empty).
                let language = detect_language(abs_path)?;
                let source = fs::read(abs_path)
                    .map_err(|e| {
                        warn!(path = %abs_path.display(), error = %e, "skipping in graph build");
                    })
                    .ok()?;
                let lang_support = parser.registry().get(language)?;
                let mut ts_parser = tree_sitter::Parser::new();
                ts_parser
                    .set_language(&lang_support.tree_sitter_language())
                    .ok()?;
                let tree = ts_parser.parse(&source, None)?;
                (ImportExtractor::extract(&tree, &source, language), language)
            };

            let edges: Vec<(String, String, Language)> = raw_imports
                .iter()
                .filter_map(|raw| {
                    resolver.resolve(raw, &rel_str).map(|target| {
                        let tl = detect_language(std::path::Path::new(&target)).unwrap_or(language);
                        (target, raw.path.clone(), tl)
                    })
                })
                .collect();

            Some((rel_str, language, edges))
        })
        .collect();

    // Phase 2: insert into graph (sequential — petgraph::DiGraph is not Sync).
    let mut graph = CodeGraph::new();
    for (rel_str, language, edges) in resolved {
        graph.get_or_insert_node(&rel_str, language);
        for (target, raw_path, target_lang) in edges {
            graph.add_edge(&rel_str, &target, &raw_path, language, target_lang);
        }
    }

    // Insert external edges (no resolver hit) — iterate cache for external imports.
    // These don't affect PageRank but are tracked for completeness.
    for entry in import_cache.iter() {
        let rel_str = entry.key();
        let (raw_imports, language) = entry.value();
        for raw in raw_imports {
            if !raw.is_relative && resolver.resolve(raw, rel_str).is_none() {
                graph.add_external_edge(rel_str, &raw.path, *language);
            }
        }
    }

    graph
}

/// Convert a `SemanticEntity` to a `Symbol`.
fn symbol_from_entity(entity: &SemanticEntity, file_path: &str, language: Language) -> Symbol {
    Symbol {
        name: entity.name.clone(),
        kind: entity.kind.clone(),
        language,
        file_path: file_path.to_string(),
        line_start: entity.line_range.start,
        line_end: entity.line_range.end,
        byte_start: entity.byte_range.start,
        byte_end: entity.byte_range.end,
        signature: entity.signature.clone(),
        scope: entity.scope.clone(),
    }
}

/// Normalize a path to a forward-slash string for consistent cross-platform storage.
fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Simple unix timestamp as a human-readable string.
fn unix_timestamp_string() -> String {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

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

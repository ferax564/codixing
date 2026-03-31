use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use dashmap::DashMap;
use rayon::prelude::*;
use tracing::{debug, info, warn};

use crate::config::IndexConfig;
use crate::embedder::Embedder;
use crate::error::{CodixingError, Result};
use crate::graph::extractor::RawImport;
use crate::graph::{CodeGraph, compute_pagerank};
use crate::index::TantivyIndex;

use crate::language::Language;
use crate::parser::Parser;
use crate::persistence::{FileHashEntry, IndexMeta, IndexStore};
use crate::reranker::Reranker;
use crate::retriever::{ChunkMeta, ChunkMetaCompact};
use crate::session::SessionState;
use crate::shared_session::SharedSession;
use crate::symbols::SymbolTable;
use crate::symbols::persistence::{deserialize_symbols, serialize_symbols};
use crate::symbols::writer::write_mmap_symbols;
use crate::vector::VectorIndex;

use super::indexing::{
    IndexContext, add_call_edges, build_file_trigram_from_content, build_graph,
    populate_symbol_graph, process_file, unix_timestamp_string, walk_source_files,
};
use super::{Engine, git_head_commit};

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

        // Initialise the embedding job queue (if rustqueue feature is enabled
        // and embeddings are active).
        #[cfg(feature = "rustqueue")]
        let embed_queue: Option<Arc<rustqueue::RustQueue>> = if embedder.is_some() {
            let queue_path = root.join(".codixing").join("embed_queue.db");
            match rustqueue::RustQueue::redb(&queue_path) {
                Ok(builder) => match builder.build() {
                    Ok(rq) => {
                        info!("embedding job queue initialised");
                        Some(Arc::new(rq))
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to build embedding queue; using sync path");
                        None
                    }
                },
                Err(e) => {
                    warn!(error = %e, "failed to open embedding queue; using sync path");
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
                #[cfg(feature = "rustqueue")]
                {
                    super::embed_queue::embed_pending(
                        embed_queue.as_ref(),
                        &pending_embeds,
                        &chunk_meta_map,
                        emb,
                        vec_idx,
                        config.embedding.contextual_embeddings,
                        &root,
                        &config.embedding.model,
                    )?;
                }
                #[cfg(not(feature = "rustqueue"))]
                {
                    let _stats = super::indexing::embed_and_index_chunks(
                        &pending_embeds,
                        &chunk_meta_map,
                        emb,
                        vec_idx,
                        config.embedding.contextual_embeddings,
                        &root,
                    )?;
                }
            }
        }

        let total_chunks = chunk_count.load(Ordering::Relaxed);
        let total_symbols = symbols.len();
        let vector_count = vector.as_ref().map(|v| v.len()).unwrap_or(0);

        // Convert DashMaps to owned types.
        let file_chunk_counts: HashMap<String, usize> = file_chunk_map.into_iter().collect();

        // Build graph and trigram indexes in parallel — they read from shared
        // DashMaps but don't write to each other.
        let (graph, (trigram_idx, ft_idx)) = rayon::join(
            || {
                // Graph construction
                if config.graph.enabled {
                    let mut g = build_graph(&files, &root, &config, &parser, &pending_imports);
                    // Resolve call-site edges using the now-complete symbol table.
                    add_call_edges(&mut g, &symbols, &pending_calls);
                    // Populate the symbol-level inner graph with function-level call edges.
                    populate_symbol_graph(&mut g, &files, &root, &config, &file_contents);
                    let scores =
                        compute_pagerank(&g, config.graph.damping, config.graph.iterations);
                    g.apply_pagerank(&scores);
                    Some(g)
                } else {
                    None
                }
            },
            || {
                // Trigram index construction (chunk + file level)
                let mut tri = crate::index::TrigramIndex::new();
                tri.build_batch(
                    chunk_meta_map
                        .iter()
                        .map(|e| (*e.key(), e.value().content.clone())),
                );
                let ft = build_file_trigram_from_content(&file_contents);
                (tri, ft)
            },
        );

        // Persist graph.
        if let Some(ref g) = graph {
            let flat = g.to_flat();
            if let Err(e) = store.save_graph(&flat) {
                warn!(error = %e, "failed to persist graph");
            }
            if let Err(e) = store.save_symbol_graph(g) {
                warn!(error = %e, "failed to persist symbol graph");
            }
        }

        // Persist trigram indexes.
        if let Err(e) = trigram_idx.save_mmap_binary(&store.chunk_trigram_path()) {
            warn!(error = %e, "failed to persist chunk trigram index");
        }
        if let Err(e) = ft_idx.save_binary(&store.file_trigram_path()) {
            warn!(error = %e, "failed to persist file trigram index");
        }

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

        // Also write the mmap-format v2 for zero-deserialization open().
        if let Some(in_mem) = symbols.as_in_memory() {
            if let Err(e) = write_mmap_symbols(in_mem, &store.symbols_v2_path()) {
                warn!(error = %e, "failed to write symbols_v2.bin (non-fatal)");
            }
        }

        let hashes: Vec<(std::path::PathBuf, u64)> =
            parser.cache().content_hashes().into_iter().collect();
        store.save_tree_hashes(&hashes)?;

        // Also write v2 hashes with mtime+size for fast sync pre-filtering.
        let v2_hashes: Vec<(std::path::PathBuf, FileHashEntry)> = hashes
            .iter()
            .map(|(path, hash)| {
                let (mtime, size) = std::fs::metadata(path)
                    .map(|m| (m.modified().ok(), m.len()))
                    .unwrap_or((None, 0));
                (path.clone(), FileHashEntry::new(*hash, mtime, size))
            })
            .collect();
        store.save_tree_hashes_v2(&v2_hashes)?;

        // Persist chunk_meta in compact format (without content — content lives in Tantivy).
        let meta_pairs: Vec<(u64, ChunkMetaCompact)> = chunk_meta_map
            .iter()
            .map(|e| (*e.key(), ChunkMetaCompact::from(e.value())))
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

        let trigram = std::sync::OnceLock::new();
        let _ = trigram.set(trigram_idx);
        let file_trigram = std::sync::OnceLock::new();
        let _ = file_trigram.set(ft_idx);

        Ok(Self {
            config,
            store,
            parser,
            tantivy,
            symbols,
            file_chunk_counts,
            embedder,
            #[cfg(feature = "rustqueue")]
            embed_queue,
            vector,
            chunk_meta: chunk_meta_map,
            graph,
            reranker,
            trigram,
            session,
            shared_session: SharedSession::default_new(),
            read_only: false,
            file_trigram,
            recency_map: std::sync::OnceLock::new(),
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

        // Restore symbols: try mmap v2 first, fall back to bitcode v1.
        let symbols = if store.symbols_v2_path().exists() {
            match crate::symbols::mmap::MmapSymbolTable::load(&store.symbols_v2_path()) {
                Ok(mmap_table) => {
                    debug!("loaded symbols_v2.bin via mmap (zero-deser)");
                    SymbolTable::Mmap(mmap_table)
                }
                Err(e) => {
                    warn!(error = %e, "failed to load symbols_v2.bin — falling back to symbols.bin");
                    if store.symbols_path().exists() {
                        let bytes = store.load_symbols_bytes()?;
                        deserialize_symbols(&bytes)?
                    } else {
                        SymbolTable::new()
                    }
                }
            }
        } else if store.symbols_path().exists() {
            let bytes = store.load_symbols_bytes()?;
            deserialize_symbols(&bytes)?
        } else {
            SymbolTable::new()
        };

        let parser = Parser::new();
        let meta = store.load_meta()?;

        // Restore chunk_meta (compact format first, fall back to legacy with content).
        let chunk_meta: DashMap<u64, ChunkMeta> = if store.chunk_meta_path().exists() {
            let bytes = store.load_chunk_meta_bytes()?;
            super::indexing::deserialize_chunk_meta(&bytes)?
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

        // Initialise the embedding job queue for re-embedding during sync.
        #[cfg(feature = "rustqueue")]
        let embed_queue: Option<Arc<rustqueue::RustQueue>> = if embedder.is_some() && !read_only {
            let queue_path = store.codixing_dir().join("embed_queue.db");
            match rustqueue::RustQueue::redb(&queue_path) {
                Ok(builder) => match builder.build() {
                    Ok(rq) => Some(Arc::new(rq)),
                    Err(e) => {
                        warn!(error = %e, "failed to build embedding queue; using sync path");
                        None
                    }
                },
                Err(e) => {
                    warn!(error = %e, "failed to open embedding queue; using sync path");
                    None
                }
            }
        } else {
            None
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

        // Trigram indexes are lazy-loaded on first use via OnceLock.
        // The 175MB chunk trigram takes ~55s to deserialize — too slow for
        // eager loading. Stays lazy so open() is fast; only paid on first
        // exact-strategy search.
        let trigram = std::sync::OnceLock::new();
        let file_trigram = std::sync::OnceLock::new();

        Ok(Self {
            config,
            store,
            parser,
            tantivy,
            symbols,
            file_chunk_counts,
            embedder,
            #[cfg(feature = "rustqueue")]
            embed_queue,
            vector,
            chunk_meta,
            graph,
            reranker,
            trigram,
            session,
            shared_session: SharedSession::default_new(),
            read_only,
            file_trigram,
            recency_map: std::sync::OnceLock::new(),
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

        // Restore symbols: try mmap v2 first, fall back to bitcode v1.
        let symbols = if store.symbols_v2_path().exists() {
            match crate::symbols::mmap::MmapSymbolTable::load(&store.symbols_v2_path()) {
                Ok(mmap_table) => {
                    debug!("loaded symbols_v2.bin via mmap (zero-deser, read-only)");
                    SymbolTable::Mmap(mmap_table)
                }
                Err(e) => {
                    warn!(error = %e, "failed to load symbols_v2.bin — falling back to symbols.bin");
                    if store.symbols_path().exists() {
                        let bytes = store.load_symbols_bytes()?;
                        deserialize_symbols(&bytes)?
                    } else {
                        SymbolTable::new()
                    }
                }
            }
        } else if store.symbols_path().exists() {
            let bytes = store.load_symbols_bytes()?;
            deserialize_symbols(&bytes)?
        } else {
            SymbolTable::new()
        };

        let parser = Parser::new();
        let meta = store.load_meta()?;

        // Restore chunk_meta (compact format first, fall back to legacy with content).
        let chunk_meta: DashMap<u64, ChunkMeta> = if store.chunk_meta_path().exists() {
            let bytes = store.load_chunk_meta_bytes()?;
            super::indexing::deserialize_chunk_meta(&bytes)?
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

        // Trigram indexes are lazy-loaded on first use via OnceLock.

        Ok(Self {
            config,
            store,
            parser,
            tantivy,
            symbols,
            file_chunk_counts,
            embedder,
            #[cfg(feature = "rustqueue")]
            embed_queue: None,
            vector,
            chunk_meta,
            graph,
            reranker,
            trigram: std::sync::OnceLock::new(),
            session,
            shared_session: SharedSession::default_new(),
            read_only: true,
            file_trigram: std::sync::OnceLock::new(),
            recency_map: std::sync::OnceLock::new(),
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
}

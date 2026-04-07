use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use dashmap::DashMap;
use rayon::prelude::*;
use tracing::{debug, info, warn};

use super::embed_state::EmbedState;

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
    IndexContext, add_call_edges, add_doc_edges, build_file_trigram_from_content, build_graph,
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
        let quantize = config.embedding.quantize;

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
        let pending_doc_refs: DashMap<String, Vec<crate::language::doc::SymbolRef>> =
            DashMap::new();

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
            pending_doc_refs: &pending_doc_refs,
        };

        // Process files in parallel: parse → chunk → index → extract symbols.
        files.par_iter().for_each(|path| {
            if let Err(e) = process_file(path, &ctx) {
                warn!(path = %path.display(), error = %e, "skipping file");
            }
        });

        tantivy.commit()?;

        let total_chunks = chunk_count.load(Ordering::Relaxed);
        let total_symbols = symbols.len();

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
                    // Resolve doc symbol references into DocumentedBy edges.
                    add_doc_edges(&mut g, &symbols, &pending_doc_refs);
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

        // Build concept index from symbols + graph co-occurrences.
        let concept_index = {
            let mut builder = super::concepts::ConceptIndexBuilder::new();
            for sym in symbols.all_symbols() {
                // doc_comment not available on Symbol yet (Task 7) — pass None.
                builder.add_symbol(&sym.name, &sym.file_path, None);
            }
            // Add import co-occurrences from graph edges.
            if let Some(ref g) = graph {
                let flat = g.to_flat();
                for (from, to, _edge) in &flat.edges {
                    builder.add_cooccurrence(from, to);
                }
            }
            let idx = builder.build();
            if !idx.is_empty() {
                // Persist concept index.
                match bitcode::serialize(&idx) {
                    Ok(bytes) => {
                        if let Err(e) = std::fs::write(store.concepts_path(), &bytes) {
                            warn!(error = %e, "failed to persist concept index");
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to serialize concept index");
                    }
                }
                Some(idx)
            } else {
                None
            }
        };

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

        // Note: the vector index is built and persisted by the background embedding
        // thread spawned below. We do not persist it here.

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
            graph_nodes,
            graph_edges,
            "index initialized (embeddings starting in background)"
        );

        // Shared vector slot — starts as None. The background thread will
        // populate it and swap it in when embedding completes.
        let vector_arc: Arc<RwLock<Option<VectorIndex>>> = Arc::new(RwLock::new(None));

        // Wrap chunk_meta in Arc so the background thread can share it.
        let chunk_meta_arc: Arc<DashMap<u64, ChunkMeta>> = Arc::new(chunk_meta_map);

        // Spawn background embedding thread (if an embedder and pending work exist).
        let embed_state = if let Some(emb) = &embedder {
            if !pending_embeds.is_empty() {
                let total = pending_embeds.len();
                let state = Arc::new(EmbedState::new(total));
                let state_clone = Arc::clone(&state);
                let vector_slot = Arc::clone(&vector_arc);
                let emb_clone = Arc::clone(emb);
                let chunk_meta_clone = Arc::clone(&chunk_meta_arc);
                let contextual = config.embedding.contextual_embeddings;
                let root_clone = root.to_path_buf();
                let file_chunks_path = store.file_chunks_path().to_path_buf();
                let vector_index_path = store.vector_index_path().to_path_buf();

                let handle = std::thread::Builder::new()
                    .name("codixing-embed-bg".into())
                    .spawn(move || {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            match VectorIndex::new(dims, quantize) {
                                Ok(bg_vector) => background_embed(
                                    &emb_clone,
                                    &pending_embeds,
                                    &chunk_meta_clone,
                                    bg_vector,
                                    contextual,
                                    &root_clone,
                                    &state_clone,
                                ),
                                Err(e) => {
                                    tracing::error!(
                                        error = %e,
                                        "background embedding: failed to create VectorIndex"
                                    );
                                    Err(e)
                                }
                            }
                        }));
                        match result {
                            Ok(Ok(completed_vector)) => {
                                // Persist to disk before exposing to readers.
                                if let Err(e) =
                                    completed_vector.save(&vector_index_path, &file_chunks_path)
                                {
                                    tracing::error!(
                                        error = %e,
                                        "background embedding: failed to persist vector index"
                                    );
                                }
                                *vector_slot.write().unwrap_or_else(|e| e.into_inner()) =
                                    Some(completed_vector);
                                state_clone.mark_ready();
                                tracing::info!(
                                    chunks = state_clone.progress().0,
                                    "background embedding complete"
                                );
                            }
                            Ok(Err(e)) => {
                                tracing::error!(
                                    error = %e,
                                    "background embedding failed"
                                );
                                state_clone.mark_ready();
                            }
                            Err(_panic) => {
                                tracing::error!("background embedding panicked");
                                state_clone.mark_ready();
                            }
                        }
                    })
                    .map_err(|e| {
                        CodixingError::Config(format!("failed to spawn embed thread: {e}"))
                    })?;

                state
                    .handle
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .replace(handle);

                Some(state)
            } else {
                None
            }
        } else {
            None
        };

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
            vector: vector_arc,
            chunk_meta: chunk_meta_arc,
            graph,
            concept_index,
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
            embed_state,
            concept_reranker: std::sync::OnceLock::new(),
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

        // Restore concept index.
        let concept_index = if store.concepts_path().exists() {
            match std::fs::read(store.concepts_path()) {
                Ok(bytes) => match bitcode::deserialize::<super::concepts::ConceptIndex>(&bytes) {
                    Ok(idx) => Some(idx),
                    Err(e) => {
                        warn!(error = %e, "failed to deserialize concept index");
                        None
                    }
                },
                Err(e) => {
                    warn!(error = %e, "failed to read concept index");
                    None
                }
            }
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
            vector: Arc::new(RwLock::new(vector)),
            chunk_meta: Arc::new(chunk_meta),
            graph,
            concept_index,
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
            embed_state: None,
            concept_reranker: std::sync::OnceLock::new(),
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

        // Restore concept index.
        let concept_index = if store.concepts_path().exists() {
            match std::fs::read(store.concepts_path()) {
                Ok(bytes) => match bitcode::deserialize::<super::concepts::ConceptIndex>(&bytes) {
                    Ok(idx) => Some(idx),
                    Err(e) => {
                        warn!(error = %e, "failed to deserialize concept index (read-only)");
                        None
                    }
                },
                Err(e) => {
                    warn!(error = %e, "failed to read concept index (read-only)");
                    None
                }
            }
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
            vector: Arc::new(RwLock::new(vector)),
            chunk_meta: Arc::new(chunk_meta),
            graph,
            concept_index,
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
            embed_state: None,
            concept_reranker: std::sync::OnceLock::new(),
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

/// Embed all pending chunks in a background thread, processing file by file.
///
/// Returns the populated `VectorIndex` on success. The caller swaps it into
/// the shared `Arc<RwLock<Option<VectorIndex>>>` slot.
fn background_embed(
    embedder: &crate::embedder::Embedder,
    pending: &DashMap<u64, String>,
    chunk_meta: &DashMap<u64, ChunkMeta>,
    mut vector: VectorIndex,
    contextual: bool,
    root: &std::path::Path,
    state: &EmbedState,
) -> Result<VectorIndex> {
    // Group chunks by file path.
    let mut by_file: std::collections::HashMap<String, Vec<u64>> = std::collections::HashMap::new();
    for entry in pending.iter() {
        let chunk_id = *entry.key();
        let file_path = chunk_meta
            .get(&chunk_id)
            .map(|m| m.file_path.clone())
            .unwrap_or_default();
        if !file_path.is_empty() {
            by_file.entry(file_path).or_default().push(chunk_id);
        }
    }

    for (file_path, chunk_ids) in &by_file {
        if state.is_cancelled() {
            tracing::info!("background embedding cancelled");
            break;
        }
        let (embedded, _used_late_chunking) = super::indexing::embed_single_file(
            embedder,
            chunk_meta,
            &mut vector,
            contextual,
            root,
            file_path,
            chunk_ids,
        )?;
        state.increment_completed(embedded);
    }

    Ok(vector)
}

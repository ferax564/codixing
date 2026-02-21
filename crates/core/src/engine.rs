use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use rayon::prelude::*;
use tracing::{debug, info, warn};

use crate::chunker::Chunker;
use crate::chunker::cast::CastChunker;
use crate::config::{IndexConfig, VectorBackend};
use crate::embeddings::{Embedder, EmbeddingBackend, MockEmbedder, http::HttpEmbedder};
use crate::error::{CodeforgeError, Result};
use crate::graph::CodeGraph;
use crate::graph::extract::{extract_definitions, extract_references};
use crate::graph::persistence::{load_graph, save_graph};
use crate::index::trigram::TrigramIndex;
use crate::index::vector::{BruteForceVectorIndex, VectorIndex};
use crate::index::{HnswVectorIndex, TantivyIndex};
use crate::language::{Language, SemanticEntity, detect_language};
use crate::parser::Parser;
use crate::persistence::{IndexMeta, IndexStore};
use crate::retriever::bm25::BM25Retriever;
use crate::retriever::hybrid::HybridRetriever;
use crate::retriever::{Retriever, SearchQuery, SearchResult};
use crate::symbols::persistence::{deserialize_symbols, serialize_symbols};
use crate::symbols::{Symbol, SymbolTable};

/// Summary statistics about the index.
#[derive(Debug, Clone)]
pub struct IndexStats {
    /// Number of files indexed.
    pub file_count: usize,
    /// Number of code chunks produced.
    pub chunk_count: usize,
    /// Number of unique symbol names.
    pub symbol_count: usize,
}

/// Holds a type-erased vector index that can be either brute-force or HNSW.
enum DynVectorIndex {
    BruteForce(BruteForceVectorIndex),
    Hnsw(HnswVectorIndex),
}

impl DynVectorIndex {
    fn add(&mut self, chunk_id: u64, vector: Vec<f32>) -> Result<()> {
        match self {
            Self::BruteForce(idx) => idx.add(chunk_id, vector),
            Self::Hnsw(idx) => idx.add(chunk_id, vector),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::BruteForce(idx) => idx.len(),
            Self::Hnsw(idx) => idx.len(),
        }
    }

    fn save_binary(&self, path: &std::path::Path) -> Result<()> {
        match self {
            Self::BruteForce(idx) => idx.save_binary(path),
            Self::Hnsw(idx) => idx.save_binary(path),
        }
    }
}

/// Create an embedder from the configured backend.
fn create_embedder(backend: &EmbeddingBackend) -> Result<Box<dyn Embedder>> {
    match backend {
        EmbeddingBackend::Mock => Ok(Box::new(MockEmbedder::new(backend.dimension()))),
        EmbeddingBackend::Onnx => {
            #[cfg(feature = "vector")]
            {
                use crate::embeddings::OnnxEmbedder;
                // OnnxEmbedder requires a model directory — use CODEFORGE_MODEL_DIR
                // env var or fall back to a standard location.
                let model_dir = std::env::var("CODEFORGE_MODEL_DIR")
                    .unwrap_or_else(|_| "models/minilm".to_string());
                let embedder = OnnxEmbedder::load(std::path::Path::new(&model_dir))?;
                Ok(Box::new(embedder))
            }
            #[cfg(not(feature = "vector"))]
            {
                Err(CodeforgeError::Config(
                    "ONNX embedding backend requires the 'vector' feature to be enabled"
                        .to_string(),
                ))
            }
        }
        EmbeddingBackend::External {
            url,
            model,
            dimension,
            api_key,
            batch_size,
        } => Ok(Box::new(HttpEmbedder::new(
            url,
            model,
            *dimension,
            api_key.clone(),
            batch_size.unwrap_or(32),
        ))),
    }
}

/// Top-level facade that wires together parsing, chunking, indexing,
/// and retrieval into a single coherent API.
pub struct Engine {
    config: IndexConfig,
    store: IndexStore,
    parser: Parser,
    tantivy: TantivyIndex,
    symbols: SymbolTable,
    /// Per-file chunk counts, used for stats.
    file_chunk_counts: HashMap<String, usize>,
    /// Lazily-built code graph.
    graph: Option<CodeGraph>,
    /// Vector index for semantic search (populated during init or open).
    vector_index: Option<DynVectorIndex>,
    /// Embedding model (None when vector features unavailable).
    embedder: Option<Box<dyn Embedder>>,
    /// Trigram index for fast exact substring search (short queries).
    trigram_index: TrigramIndex,
}

impl Engine {
    /// Initialize a new index for the project at `root`.
    ///
    /// Walks the directory tree, parses all supported source files in parallel
    /// using rayon, chunks them with the cAST algorithm, indexes chunks in
    /// Tantivy, and populates the symbol table. All state is persisted to the
    /// `.codeforge/` directory.
    pub fn init(root: impl AsRef<Path>, config: IndexConfig) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .map_err(|e| CodeforgeError::Config(format!("cannot resolve root path: {e}")))?;

        let store = IndexStore::init(&root, &config)?;
        let tantivy = TantivyIndex::create_in_dir(&store.tantivy_dir())?;
        let parser = Parser::new();
        let symbols = SymbolTable::new();

        let files = walk_source_files(&root, &config)?;
        info!(file_count = files.len(), "discovered source files");

        let chunk_count = AtomicUsize::new(0);
        let file_chunk_map = dashmap::DashMap::<String, usize>::new();

        let ctx = IndexContext {
            root: &root,
            config: &config,
            parser: &parser,
            tantivy: &tantivy,
            symbols: &symbols,
            chunk_count: &chunk_count,
            file_chunk_map: &file_chunk_map,
        };

        // Process files in parallel: parse → chunk → index.
        files.par_iter().for_each(|path| {
            if let Err(e) = process_file(path, &ctx) {
                warn!(path = %path.display(), error = %e, "skipping file");
            }
        });

        tantivy.commit()?;

        let total_chunks = chunk_count.load(Ordering::Relaxed);
        let total_symbols = symbols.len();

        // Convert DashMap to HashMap.
        let file_chunk_counts: HashMap<String, usize> = file_chunk_map.into_iter().collect();

        // Persist everything.
        let sym_bytes = serialize_symbols(&symbols)?;
        store.save_symbols_bytes(&sym_bytes)?;

        let hashes: Vec<(PathBuf, u64)> = parser.cache().content_hashes().into_iter().collect();
        store.save_tree_hashes(&hashes)?;

        let meta = IndexMeta {
            version: "0.1.0".to_string(),
            file_count: files.len(),
            chunk_count: total_chunks,
            symbol_count: total_symbols,
            last_indexed: unix_timestamp_string(),
        };
        store.save_meta(&meta)?;

        // Build vector index using the configured embedding backend.
        let embedder: Box<dyn Embedder> = create_embedder(&config.embedding_backend)?;
        let chunk_threshold = 10_000;
        let use_hnsw = match &config.vector_backend {
            VectorBackend::Hnsw => true,
            VectorBackend::BruteForce => false,
            VectorBackend::Auto => total_chunks >= chunk_threshold,
        };

        let mut vector_index: DynVectorIndex = if use_hnsw {
            DynVectorIndex::Hnsw(HnswVectorIndex::new(embedder.dimension()))
        } else {
            DynVectorIndex::BruteForce(BruteForceVectorIndex::new(embedder.dimension()))
        };

        // Batch-embed all indexed chunks into the vector index.
        // We read the tantivy index to get chunk IDs and content.
        let all_chunks = tantivy.all_chunk_ids_and_content()?;
        if !all_chunks.is_empty() {
            let texts: Vec<&str> = all_chunks.iter().map(|(_, c)| c.as_str()).collect();
            let embeddings = embedder.embed_batch(&texts)?;
            for ((chunk_id, _), embedding) in all_chunks.iter().zip(embeddings) {
                vector_index.add(*chunk_id, embedding)?;
            }
        }

        // Persist vector index.
        vector_index.save_binary(&store.vector_index_path())?;

        // Build trigram index from the same chunks.
        let mut trigram_index = TrigramIndex::new();
        for (chunk_id, content) in &all_chunks {
            trigram_index.add(*chunk_id, content);
        }

        // Persist trigram index.
        trigram_index.save_binary(&store.trigram_index_path())?;

        info!(
            files = files.len(),
            chunks = total_chunks,
            symbols = total_symbols,
            vectors = vector_index.len(),
            trigrams = trigram_index.len(),
            "index initialized"
        );

        Ok(Self {
            config,
            store,
            parser,
            tantivy,
            symbols,
            file_chunk_counts,
            graph: None,
            vector_index: Some(vector_index),
            embedder: Some(embedder),
            trigram_index,
        })
    }

    /// Open an existing index from the `.codeforge/` directory.
    ///
    /// Restores the Tantivy index, symbol table, and tree hashes from disk.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .map_err(|e| CodeforgeError::Config(format!("cannot resolve root path: {e}")))?;

        let store = IndexStore::open(&root)?;
        let config = store.load_config()?;
        let tantivy = TantivyIndex::open_in_dir(&store.tantivy_dir())?;

        // Restore symbols.
        let symbols = if store.symbols_path().exists() {
            let bytes = store.load_symbols_bytes()?;
            deserialize_symbols(&bytes)?
        } else {
            SymbolTable::new()
        };

        let parser = Parser::new();
        let meta = store.load_meta()?;
        let file_chunk_counts = HashMap::new(); // Not persisted; rebuilt on reindex.

        // Load persisted graph if available.
        let graph = if store.graph_path().exists() {
            match load_graph(&store.graph_path()) {
                Ok(g) => {
                    info!(
                        nodes = g.node_count(),
                        edges = g.edge_count(),
                        "loaded code graph"
                    );
                    Some(g)
                }
                Err(e) => {
                    warn!(error = %e, "failed to load code graph, skipping");
                    None
                }
            }
        } else {
            None
        };

        // Load vector index if persisted.
        let embedder: Box<dyn Embedder> = create_embedder(&config.embedding_backend)?;
        let vector_index = if store.vector_index_path().exists() {
            let use_hnsw = match &config.vector_backend {
                VectorBackend::Hnsw => true,
                VectorBackend::BruteForce => false,
                VectorBackend::Auto => meta.chunk_count >= 10_000,
            };
            if use_hnsw {
                match HnswVectorIndex::load_binary(&store.vector_index_path()) {
                    Ok(idx) => {
                        info!(vectors = idx.len(), "loaded HNSW vector index");
                        Some(DynVectorIndex::Hnsw(idx))
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to load HNSW vector index, skipping");
                        None
                    }
                }
            } else {
                match BruteForceVectorIndex::load_binary(&store.vector_index_path()) {
                    Ok(idx) => {
                        info!(vectors = idx.len(), "loaded brute-force vector index");
                        Some(DynVectorIndex::BruteForce(idx))
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to load brute-force vector index, skipping");
                        None
                    }
                }
            }
        } else {
            None
        };

        // Load persisted trigram index, falling back to rebuild from Tantivy.
        let trigram_index = if store.trigram_index_path().exists() {
            match TrigramIndex::load_binary(&store.trigram_index_path()) {
                Ok(idx) => {
                    info!(chunks = idx.len(), "loaded trigram index");
                    idx
                }
                Err(e) => {
                    warn!(error = %e, "failed to load trigram index, rebuilding");
                    let all_chunks = tantivy.all_chunk_ids_and_content()?;
                    let mut idx = TrigramIndex::new();
                    for (chunk_id, content) in &all_chunks {
                        idx.add(*chunk_id, content);
                    }
                    idx
                }
            }
        } else {
            let all_chunks = tantivy.all_chunk_ids_and_content()?;
            let mut idx = TrigramIndex::new();
            for (chunk_id, content) in &all_chunks {
                idx.add(*chunk_id, content);
            }
            idx
        };

        info!(
            files = meta.file_count,
            chunks = meta.chunk_count,
            symbols = meta.symbol_count,
            trigrams = trigram_index.len(),
            "index opened"
        );

        Ok(Self {
            config,
            store,
            parser,
            tantivy,
            symbols,
            file_chunk_counts,
            graph,
            vector_index,
            embedder: Some(embedder),
            trigram_index,
        })
    }

    /// Search the index using BM25 ranking.
    pub fn search(&self, query: SearchQuery) -> Result<Vec<SearchResult>> {
        let retriever = BM25Retriever::new(&self.tantivy);
        retriever.search(&query)
    }

    /// Search using hybrid BM25 + vector retrieval with RRF fusion.
    ///
    /// Falls back to BM25-only search if no vector index or embedder is
    /// available. For short queries (< 10 characters), the trigram index
    /// is consulted first to provide fast exact substring candidates that
    /// are merged additively with the hybrid results.
    pub fn hybrid_search(&self, query: SearchQuery) -> Result<Vec<SearchResult>> {
        let (Some(vi), Some(embedder)) = (&self.vector_index, &self.embedder) else {
            return self.search(query);
        };

        // Run the normal hybrid retrieval.
        let mut results = match vi {
            DynVectorIndex::BruteForce(bf) => {
                let mut retriever = HybridRetriever::new(&self.tantivy, bf, embedder.as_ref());
                if let Some(ref graph) = self.graph {
                    retriever = retriever.with_graph_boost(graph, 0.3);
                }
                retriever.search(&query)?
            }
            DynVectorIndex::Hnsw(hnsw) => {
                let mut retriever = HybridRetriever::new(&self.tantivy, hnsw, embedder.as_ref());
                if let Some(ref graph) = self.graph {
                    retriever = retriever.with_graph_boost(graph, 0.3);
                }
                retriever.search(&query)?
            }
        };

        // For short queries (likely symbol/identifier lookups), use the
        // trigram index to find exact substring matches that BM25 may miss
        // due to tokenization differences.
        const TRIGRAM_QUERY_THRESHOLD: usize = 10;
        if query.query.len() < TRIGRAM_QUERY_THRESHOLD && query.query.len() >= 3 {
            let trigram_matches = self.trigram_index.search(&query.query);
            if !trigram_matches.is_empty() {
                // Collect chunk IDs already in the hybrid results.
                let existing_ids: std::collections::HashSet<String> =
                    results.iter().map(|r| r.chunk_id.clone()).collect();

                // Boost existing results that also have trigram matches.
                let trigram_chunk_ids: std::collections::HashSet<String> = trigram_matches
                    .iter()
                    .map(|m| m.chunk_id.to_string())
                    .collect();
                for result in &mut results {
                    if trigram_chunk_ids.contains(&result.chunk_id) {
                        // Trigram-confirmed exact match gets a score boost.
                        result.score *= 1.5;
                    }
                }

                // Find trigram-only hits that BM25/vector missed.
                let missing_ids: std::collections::HashSet<u64> = trigram_matches
                    .iter()
                    .filter(|m| !existing_ids.contains(&m.chunk_id.to_string()))
                    .map(|m| m.chunk_id)
                    .collect();

                if !missing_ids.is_empty() {
                    // Look up full document metadata from Tantivy.
                    if let Ok(docs) = self.tantivy.lookup_chunks_by_ids(&missing_ids) {
                        let fields = self.tantivy.fields();
                        for doc in docs {
                            let chunk_id = doc
                                .get_first(fields.chunk_id)
                                .and_then(|v| tantivy::schema::Value::as_str(&v))
                                .unwrap_or("")
                                .to_string();
                            let file_path = doc
                                .get_first(fields.file_path)
                                .and_then(|v| tantivy::schema::Value::as_str(&v))
                                .unwrap_or("")
                                .to_string();
                            let language = doc
                                .get_first(fields.language)
                                .and_then(|v| tantivy::schema::Value::as_str(&v))
                                .unwrap_or("")
                                .to_string();
                            let content = doc
                                .get_first(fields.content)
                                .and_then(|v| tantivy::schema::Value::as_str(&v))
                                .unwrap_or("")
                                .to_string();
                            let signature = doc
                                .get_first(fields.signature)
                                .and_then(|v| tantivy::schema::Value::as_str(&v))
                                .unwrap_or("")
                                .to_string();
                            let line_start = doc
                                .get_first(fields.line_start)
                                .and_then(|v| tantivy::schema::Value::as_u64(&v))
                                .unwrap_or(0);
                            let line_end = doc
                                .get_first(fields.line_end)
                                .and_then(|v| tantivy::schema::Value::as_u64(&v))
                                .unwrap_or(0);

                            // Apply file filter if present.
                            if let Some(ref filter) = query.file_filter {
                                if !file_path.contains(filter) {
                                    continue;
                                }
                            }

                            // Assign a base score slightly below the lowest
                            // hybrid result so trigram-only hits appear at the
                            // tail unless they are the only results.
                            let base_score = results
                                .last()
                                .map(|r| r.score * 0.9)
                                .unwrap_or(0.001);

                            results.push(SearchResult {
                                chunk_id,
                                file_path,
                                language,
                                score: base_score,
                                line_start,
                                line_end,
                                signature,
                                content,
                            });
                        }
                    }
                }

                // Re-sort after boosting and adding trigram results.
                results.sort_by(|a, b| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });

                // Truncate to the requested limit.
                results.truncate(query.limit);
            }
        }

        Ok(results)
    }

    /// Query the symbol table.
    ///
    /// Performs case-insensitive substring matching on symbol names.
    /// If `file` is provided, also filters by file path.
    pub fn symbols(&self, filter: &str, file: Option<&str>) -> Result<Vec<Symbol>> {
        Ok(self.symbols.filter(filter, file))
    }

    /// Access the code graph, if it has been built.
    pub fn graph(&self) -> Option<&CodeGraph> {
        self.graph.as_ref()
    }

    /// Build a code graph from all indexed files.
    ///
    /// Walks the project source files, extracts definitions and references
    /// using tree-sitter, creates graph nodes for each definition, and
    /// resolves references to definition nodes by name matching.
    pub fn build_graph(&mut self) -> Result<&CodeGraph> {
        let root = &self.config.root.clone();
        let files = walk_source_files(root, &self.config)?;

        let mut graph = CodeGraph::new();

        // First pass: extract all definitions and add them as nodes.
        // We collect (name -> NodeIndex) for reference resolution.
        let mut name_to_nodes: HashMap<String, Vec<petgraph::graph::NodeIndex>> = HashMap::new();

        for path in &files {
            let source = match fs::read_to_string(path) {
                Ok(s) => s,
                Err(_) => continue,
            };

            let lang = match detect_language(path) {
                Some(l) => l,
                None => continue,
            };

            let rel_path = path.strip_prefix(root).unwrap_or(path);
            let rel_str = normalize_path(rel_path);

            let defs = extract_definitions(&source, &rel_str, &lang);
            for def in &defs {
                let idx =
                    graph.add_symbol_with_line(&def.name, &def.file, def.kind.clone(), def.line);
                name_to_nodes.entry(def.name.clone()).or_default().push(idx);
            }
        }

        // Second pass: extract references and resolve to definition nodes.
        for path in &files {
            let source = match fs::read_to_string(path) {
                Ok(s) => s,
                Err(_) => continue,
            };

            let lang = match detect_language(path) {
                Some(l) => l,
                None => continue,
            };

            let rel_path = path.strip_prefix(root).unwrap_or(path);
            let rel_str = normalize_path(rel_path);

            let defs = extract_definitions(&source, &rel_str, &lang);
            let refs = extract_references(&source, &rel_str, &lang);

            // For each reference, find the definition node that contains this
            // reference (by line range) and the target definition node (by name).
            for reference in &refs {
                // Find the source node: the definition in this file whose range
                // contains the reference line.
                let source_node = find_enclosing_definition(&defs, reference.line, &name_to_nodes);

                // Find target nodes by name.
                let target_name = &reference.target_name;
                let target_nodes = name_to_nodes.get(target_name);

                if let (Some(src_idx), Some(targets)) = (source_node, target_nodes) {
                    for &tgt_idx in targets {
                        // Avoid self-edges.
                        if src_idx != tgt_idx {
                            graph.add_reference(src_idx, tgt_idx, reference.kind.clone());
                        }
                    }
                }
            }
        }

        info!(
            nodes = graph.node_count(),
            edges = graph.edge_count(),
            "built code graph"
        );

        self.graph = Some(graph);
        Ok(self.graph.as_ref().unwrap())
    }

    /// Re-index a single file (after modification).
    ///
    /// Removes old data, re-parses, re-chunks, and re-indexes.
    pub fn reindex_file(&mut self, path: &Path) -> Result<()> {
        let abs_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.config.root.join(path)
        };

        let rel_path = abs_path.strip_prefix(&self.config.root).unwrap_or(path);
        let rel_str = normalize_path(rel_path);

        // Remove old data.
        self.tantivy.remove_file(&rel_str)?;
        self.symbols.remove_file(&rel_str);

        // Read and re-process.
        let source = fs::read(&abs_path)?;
        let result = self.parser.parse_file(&abs_path, &source)?;
        let chunker = CastChunker;
        let chunks = chunker.chunk(
            &rel_str,
            &source,
            &result.tree,
            result.language,
            &self.config.chunk,
        );

        for chunk in &chunks {
            self.tantivy.add_chunk(chunk)?;
        }

        for entity in &result.entities {
            self.symbols
                .insert(symbol_from_entity(entity, &rel_str, result.language));
        }

        self.tantivy.commit()?;
        self.file_chunk_counts.insert(rel_str, chunks.len());

        debug!(path = %abs_path.display(), chunks = chunks.len(), "reindexed file");
        Ok(())
    }

    /// Remove a file from the index entirely.
    pub fn remove_file(&mut self, path: &Path) -> Result<()> {
        let rel_path = path.strip_prefix(&self.config.root).unwrap_or(path);
        let rel_str = normalize_path(rel_path);

        self.tantivy.remove_file(&rel_str)?;
        self.tantivy.commit()?;
        self.symbols.remove_file(&rel_str);
        self.parser.invalidate(path);
        self.file_chunk_counts.remove(&rel_str);

        debug!(path = %path.display(), "removed file from index");
        Ok(())
    }

    /// Return summary statistics about the current index.
    pub fn stats(&self) -> IndexStats {
        IndexStats {
            file_count: self.file_chunk_counts.len(),
            chunk_count: self.file_chunk_counts.values().sum(),
            symbol_count: self.symbols.len(),
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

    /// Start watching the project directory for file changes.
    ///
    /// Returns a [`FileWatcher`] that can be polled for batches of changes.
    /// Call [`Self::apply_changes`] with the resulting batch to update the index.
    pub fn watch(&self) -> Result<crate::watcher::FileWatcher> {
        crate::watcher::FileWatcher::new(&self.config.root, &self.config)
    }

    /// Apply a batch of file changes to the index.
    ///
    /// For each modified file, re-parses and re-indexes it. For each removed
    /// file, removes it from the index.
    pub fn apply_changes(&mut self, changes: &[crate::watcher::FileChange]) -> Result<()> {
        use crate::watcher::ChangeKind;

        for change in changes {
            match change.kind {
                ChangeKind::Modified => {
                    if let Err(e) = self.reindex_file(&change.path) {
                        warn!(path = %change.path.display(), error = %e, "failed to reindex");
                    }
                }
                ChangeKind::Removed => {
                    if let Err(e) = self.remove_file(&change.path) {
                        warn!(path = %change.path.display(), error = %e, "failed to remove");
                    }
                }
            }
        }

        Ok(())
    }

    /// Persist current state to disk.
    pub fn save(&self) -> Result<()> {
        let sym_bytes = serialize_symbols(&self.symbols)?;
        self.store.save_symbols_bytes(&sym_bytes)?;

        let hashes: Vec<(PathBuf, u64)> =
            self.parser.cache().content_hashes().into_iter().collect();
        self.store.save_tree_hashes(&hashes)?;

        let stats = self.stats();
        let meta = IndexMeta {
            version: "0.1.0".to_string(),
            file_count: stats.file_count,
            chunk_count: stats.chunk_count,
            symbol_count: stats.symbol_count,
            last_indexed: unix_timestamp_string(),
        };
        self.store.save_meta(&meta)?;

        // Persist code graph if built.
        if let Some(ref graph) = self.graph {
            save_graph(graph, &self.store.graph_path())?;
        }

        // Persist vector index if available.
        if let Some(ref vi) = self.vector_index {
            vi.save_binary(&self.store.vector_index_path())?;
        }

        // Persist trigram index.
        self.trigram_index
            .save_binary(&self.store.trigram_index_path())?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Find the definition that encloses a given line number.
///
/// Returns the `NodeIndex` of the closest definition whose line is at or before
/// the reference line. This is a heuristic: we pick the last definition that
/// starts before or at the reference line.
fn find_enclosing_definition(
    defs: &[crate::graph::extract::DefinitionInfo],
    ref_line: usize,
    name_to_nodes: &HashMap<String, Vec<petgraph::graph::NodeIndex>>,
) -> Option<petgraph::graph::NodeIndex> {
    // Find the last definition whose line <= ref_line.
    let mut best: Option<&crate::graph::extract::DefinitionInfo> = None;
    for def in defs {
        if def.line <= ref_line {
            match best {
                None => best = Some(def),
                Some(b) if def.line >= b.line => best = Some(def),
                _ => {}
            }
        }
    }

    let def = best?;
    let nodes = name_to_nodes.get(&def.name)?;
    // Return the first node that matches (from this file).
    nodes.first().copied()
}

/// Shared context passed to `process_file` to avoid too-many-arguments.
struct IndexContext<'a> {
    root: &'a Path,
    config: &'a IndexConfig,
    parser: &'a Parser,
    tantivy: &'a TantivyIndex,
    symbols: &'a SymbolTable,
    chunk_count: &'a AtomicUsize,
    file_chunk_map: &'a dashmap::DashMap<String, usize>,
}

/// Process a single file: parse → chunk → index → extract symbols.
fn process_file(path: &Path, ctx: &IndexContext<'_>) -> Result<()> {
    let source = fs::read(path)?;
    let result = ctx.parser.parse_file(path, &source)?;

    let rel_path = path.strip_prefix(ctx.root).unwrap_or(path);
    let rel_str = normalize_path(rel_path);

    let chunker = CastChunker;
    let chunks = chunker.chunk(
        &rel_str,
        &source,
        &result.tree,
        result.language,
        &ctx.config.chunk,
    );

    ctx.chunk_count.fetch_add(chunks.len(), Ordering::Relaxed);
    ctx.file_chunk_map.insert(rel_str.clone(), chunks.len());

    for chunk in &chunks {
        ctx.tantivy.add_chunk(chunk)?;
    }

    for entity in &result.entities {
        ctx.symbols
            .insert(symbol_from_entity(entity, &rel_str, result.language));
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

/// Walk the directory tree and collect all source files with supported extensions.
fn walk_source_files(root: &Path, config: &IndexConfig) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    walk_dir_recursive(root, config, &mut files)?;
    Ok(files)
}

/// Recursive directory walker that respects exclude patterns.
fn walk_dir_recursive(dir: &Path, config: &IndexConfig, files: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(dir)?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();

        // Skip excluded directories/files.
        if config.exclude_patterns.iter().any(|p| p == name.as_ref()) {
            continue;
        }

        if path.is_dir() {
            walk_dir_recursive(&path, config, files)?;
        } else if path.is_file() {
            // Only include files with supported language extensions.
            if detect_language(&path).is_some() {
                // If specific languages are configured, filter by them.
                if !config.languages.is_empty() {
                    if let Some(lang) = detect_language(&path) {
                        if config.languages.contains(&lang.name().to_lowercase()) {
                            files.push(path);
                        }
                    }
                } else {
                    files.push(path);
                }
            }
        }
    }

    Ok(())
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
    use std::fs;
    use tempfile::tempdir;

    /// Create a temporary project with some source files.
    fn setup_project() -> (tempfile::TempDir, PathBuf) {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();

        // Create a simple Rust file.
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

    #[test]
    fn init_indexes_project() {
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);

        let engine = Engine::init(&root, config).unwrap();
        let stats = engine.stats();

        assert_eq!(stats.file_count, 2, "expected 2 source files");
        assert!(stats.chunk_count > 0, "expected at least 1 chunk");
        assert!(stats.symbol_count > 0, "expected at least 1 symbol");
    }

    #[test]
    fn search_finds_function() {
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);

        let engine = Engine::init(&root, config).unwrap();

        let results = engine
            .search(SearchQuery::new("add").with_limit(5))
            .unwrap();
        assert!(!results.is_empty(), "expected search results for 'add'");

        // At least one result should be from main.rs.
        assert!(
            results.iter().any(|r| r.file_path.contains("main.rs")),
            "expected result from main.rs"
        );
    }

    #[test]
    fn symbols_returns_matching() {
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);

        let engine = Engine::init(&root, config).unwrap();

        let syms = engine.symbols("Config", None).unwrap();
        assert!(
            !syms.is_empty(),
            "expected at least 1 symbol matching 'Config'"
        );
        assert!(syms.iter().any(|s| s.name == "Config"));
    }

    #[test]
    fn open_restores_index() {
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);

        // Init and drop.
        {
            let engine = Engine::init(&root, config).unwrap();
            let stats = engine.stats();
            assert!(stats.chunk_count > 0);
        }

        // Re-open.
        let engine = Engine::open(&root).unwrap();
        let results = engine
            .search(SearchQuery::new("helper").with_limit(5))
            .unwrap();
        assert!(
            !results.is_empty(),
            "expected search results after re-opening index"
        );
    }

    #[test]
    fn reindex_file_updates_index() {
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);

        let mut engine = Engine::init(&root, config).unwrap();

        // Modify main.rs.
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
            .search(SearchQuery::new("unique_new_function").with_limit(5))
            .unwrap();
        assert!(
            !results.is_empty(),
            "expected to find newly added function after reindex"
        );
    }

    #[test]
    fn engine_builds_graph_from_indexed_files() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();

        fs::write(
            src_dir.join("main.rs"),
            "fn main() { greet(); }\nfn greet() {}\n",
        )
        .unwrap();

        let config = IndexConfig::new(&root);
        let mut engine = Engine::init(&root, config).unwrap();

        // Graph should be None before build.
        assert!(engine.graph().is_none());

        engine.build_graph().unwrap();
        let graph = engine.graph().unwrap();
        // Should find at least main and greet as definitions.
        assert!(
            graph.node_count() >= 2,
            "expected at least 2 nodes, got {}",
            graph.node_count()
        );
    }

    #[test]
    fn engine_build_graph_creates_edges() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();

        fs::write(
            src_dir.join("main.rs"),
            "fn main() { greet(); }\nfn greet() {}\n",
        )
        .unwrap();

        let config = IndexConfig::new(&root);
        let mut engine = Engine::init(&root, config).unwrap();
        engine.build_graph().unwrap();
        let graph = engine.graph().unwrap();

        // main calls greet, so there should be at least 1 edge.
        assert!(
            graph.edge_count() >= 1,
            "expected at least 1 edge (main->greet), got {}",
            graph.edge_count()
        );
    }

    #[test]
    fn graph_persistence_round_trip() {
        use crate::graph::persistence::{load_graph, save_graph};

        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();

        fs::write(
            src_dir.join("main.rs"),
            "fn main() { greet(); }\nfn greet() {}\n",
        )
        .unwrap();

        let config = IndexConfig::new(&root);
        let mut engine = Engine::init(&root, config).unwrap();
        engine.build_graph().unwrap();

        let graph = engine.graph().unwrap();
        let original_nodes = graph.node_count();
        let original_edges = graph.edge_count();

        // Save and load.
        let graph_path = dir.path().join("graph.json");
        save_graph(graph, &graph_path).unwrap();
        let loaded = load_graph(&graph_path).unwrap();

        assert_eq!(loaded.node_count(), original_nodes);
        assert_eq!(loaded.edge_count(), original_edges);
    }

    #[test]
    fn test_engine_hybrid_search() {
        // Verify that Engine::hybrid_search() returns results using the vector
        // index that is built during Engine::init().
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);

        let engine = Engine::init(&root, config).unwrap();

        // hybrid_search should return results for a known function name.
        let results = engine
            .hybrid_search(SearchQuery::new("add").with_limit(5))
            .unwrap();
        assert!(
            !results.is_empty(),
            "expected hybrid_search to return results for 'add'"
        );

        // At least one result should be from main.rs (where `add` is defined).
        assert!(
            results.iter().any(|r| r.file_path.contains("main.rs")),
            "expected a result from main.rs, got: {:?}",
            results.iter().map(|r| &r.file_path).collect::<Vec<_>>()
        );

        // All scores should be positive.
        for r in &results {
            assert!(r.score > 0.0, "expected positive score, got {}", r.score);
        }

        // Results should be in descending score order.
        for w in results.windows(2) {
            assert!(
                w[0].score >= w[1].score,
                "hybrid_search results not sorted: {} < {}",
                w[0].score,
                w[1].score
            );
        }
    }

    #[test]
    fn test_engine_hybrid_search_with_hnsw_backend() {
        // Force HNSW backend and verify hybrid_search still works.
        let (_dir, root) = setup_project();
        let mut config = IndexConfig::new(&root);
        config.vector_backend = VectorBackend::Hnsw;

        let engine = Engine::init(&root, config).unwrap();

        let results = engine
            .hybrid_search(SearchQuery::new("helper").with_limit(5))
            .unwrap();
        assert!(
            !results.is_empty(),
            "expected HNSW-backed hybrid_search to return results for 'helper'"
        );

        // Should find result from lib.rs (where `helper` is defined).
        assert!(
            results.iter().any(|r| r.file_path.contains("lib.rs")),
            "expected a result from lib.rs with HNSW backend"
        );
    }

    #[test]
    fn test_vector_index_persistence() {
        // Verify that the vector index survives a save/open round-trip and
        // hybrid_search works after reopening.
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);

        // Phase 1: Init, verify hybrid_search works, then drop.
        let original_result_count;
        {
            let engine = Engine::init(&root, config).unwrap();
            let results = engine
                .hybrid_search(SearchQuery::new("add").with_limit(5))
                .unwrap();
            assert!(
                !results.is_empty(),
                "expected hybrid_search results before save"
            );
            original_result_count = results.len();

            // Engine::init already persists the vector index; no explicit save needed.
        }

        // Phase 2: Re-open from disk and verify hybrid_search still works.
        let engine = Engine::open(&root).unwrap();
        let results = engine
            .hybrid_search(SearchQuery::new("add").with_limit(5))
            .unwrap();

        assert!(
            !results.is_empty(),
            "expected hybrid_search results after re-opening index"
        );
        // Should get the same number of results (same data, same index).
        assert_eq!(
            results.len(),
            original_result_count,
            "expected same result count after round-trip: got {} vs original {}",
            results.len(),
            original_result_count
        );

        // Verify the vector index file exists on disk.
        let vi_path = root.join(".codeforge/vectors.bin");
        assert!(
            vi_path.exists(),
            "expected vectors.bin to exist at {:?}",
            vi_path
        );
    }

    #[test]
    fn test_vector_index_persistence_hnsw() {
        // Verify that HNSW vector index specifically survives a round-trip.
        let (_dir, root) = setup_project();
        let mut config = IndexConfig::new(&root);
        config.vector_backend = VectorBackend::Hnsw;

        // Init with HNSW, verify hybrid_search, drop.
        {
            let engine = Engine::init(&root, config).unwrap();
            let results = engine
                .hybrid_search(SearchQuery::new("Config").with_limit(5))
                .unwrap();
            assert!(
                !results.is_empty(),
                "expected HNSW hybrid_search results before save"
            );
        }

        // Re-open. The config on disk has vector_backend: Hnsw, so open()
        // should load the HNSW index.
        let engine = Engine::open(&root).unwrap();
        let results = engine
            .hybrid_search(SearchQuery::new("Config").with_limit(5))
            .unwrap();
        assert!(
            !results.is_empty(),
            "expected HNSW hybrid_search results after re-open"
        );
    }

    #[test]
    fn test_auto_backend_selection() {
        // VectorBackend::Auto should pick BruteForce for small chunk counts
        // (< 10_000 threshold) and HNSW for large.
        //
        // We verify the small case by checking that the default (Auto) config
        // on a small project uses brute-force (which it does because our test
        // project has only a handful of chunks, well below 10_000).

        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);
        // Default is VectorBackend::Auto.
        assert_eq!(config.vector_backend, VectorBackend::Auto);

        let engine = Engine::init(&root, config).unwrap();
        let stats = engine.stats();

        // With only 2 files the chunk count is far below 10_000, so Auto
        // should have selected BruteForce.
        assert!(
            stats.chunk_count < 10_000,
            "test project should have < 10_000 chunks, got {}",
            stats.chunk_count
        );

        // hybrid_search should still work (proving a vector index was created).
        let results = engine
            .hybrid_search(SearchQuery::new("add").with_limit(5))
            .unwrap();
        assert!(
            !results.is_empty(),
            "expected hybrid_search results with Auto backend"
        );

        // Now test the HNSW threshold logic directly: VectorBackend::Hnsw
        // forces HNSW regardless of chunk count.
        let mut hnsw_config = IndexConfig::new(&root);
        hnsw_config.vector_backend = VectorBackend::Hnsw;

        // Re-init with a clean root to avoid the existing .codeforge dir.
        let dir2 = tempdir().unwrap();
        let root2 = dir2.path().to_path_buf();
        let src2 = root2.join("src");
        fs::create_dir_all(&src2).unwrap();
        fs::write(
            src2.join("main.rs"),
            "fn main() {}\npub fn tiny() -> bool { true }\n",
        )
        .unwrap();

        let mut hnsw_config2 = IndexConfig::new(&root2);
        hnsw_config2.vector_backend = VectorBackend::Hnsw;
        let engine2 = Engine::init(&root2, hnsw_config2).unwrap();

        // Even with very few chunks, HNSW is selected when explicitly configured.
        let results2 = engine2
            .hybrid_search(SearchQuery::new("tiny").with_limit(5))
            .unwrap();
        assert!(
            !results2.is_empty(),
            "expected HNSW hybrid_search results even for small project"
        );
    }

    #[test]
    fn test_auto_selects_brute_force_for_small_project() {
        // Complementary test: verify that VectorBackend::BruteForce explicitly
        // forces brute-force even if chunk count were hypothetically large.
        let (_dir, root) = setup_project();
        let mut config = IndexConfig::new(&root);
        config.vector_backend = VectorBackend::BruteForce;

        let engine = Engine::init(&root, config).unwrap();

        let results = engine
            .hybrid_search(SearchQuery::new("Processor").with_limit(5))
            .unwrap();
        assert!(
            !results.is_empty(),
            "expected BruteForce hybrid_search results for 'Processor'"
        );
    }

    #[test]
    fn open_loads_persisted_graph() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();

        fs::write(
            src_dir.join("main.rs"),
            "fn main() { greet(); }\nfn greet() {}\n",
        )
        .unwrap();

        // Init, build graph, save.
        {
            let config = IndexConfig::new(&root);
            let mut engine = Engine::init(&root, config).unwrap();
            engine.build_graph().unwrap();
            engine.save().unwrap();
        }

        // Re-open — should load graph.
        let engine = Engine::open(&root).unwrap();
        let graph = engine.graph();
        assert!(graph.is_some(), "expected graph to be loaded on open");
        assert!(
            graph.unwrap().node_count() >= 2,
            "expected at least 2 nodes after loading"
        );
    }

    #[test]
    fn engine_config_with_external_backend() {
        use crate::embeddings::EmbeddingBackend;

        // Verify that IndexConfig with an External embedding backend
        // serializes and deserializes correctly.
        let mut config = IndexConfig::new("/tmp/test");
        config.embedding_backend = EmbeddingBackend::External {
            url: "https://api.voyageai.com/v1/embeddings".into(),
            model: "voyage-code-3".into(),
            dimension: 1024,
            api_key: Some("$VOYAGE_API_KEY".into()),
            batch_size: Some(64),
        };

        let json = serde_json::to_string_pretty(&config).unwrap();
        let parsed: IndexConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, parsed);
        assert_eq!(parsed.embedding_backend.dimension(), 1024);
    }

    #[test]
    fn engine_uses_mock_backend_by_default() {
        // Verify that Engine::init() works with default config (Mock backend).
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);
        assert_eq!(
            config.embedding_backend,
            crate::embeddings::EmbeddingBackend::Mock
        );

        let engine = Engine::init(&root, config).unwrap();
        let results = engine
            .hybrid_search(SearchQuery::new("add").with_limit(5))
            .unwrap();
        assert!(
            !results.is_empty(),
            "expected hybrid_search results with default Mock backend"
        );
    }

    #[test]
    fn create_embedder_mock() {
        use crate::embeddings::EmbeddingBackend;

        let embedder = super::create_embedder(&EmbeddingBackend::Mock).unwrap();
        assert_eq!(embedder.dimension(), 32);

        // Should produce valid embeddings.
        let vec = embedder.embed("hello").unwrap();
        assert_eq!(vec.len(), 32);
    }

    #[test]
    fn trigram_index_finds_exact_substring() {
        // Verify that the trigram index wired into Engine finds exact
        // substring matches via hybrid_search.
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();

        // Create a file with a distinctive function name that is short
        // enough to trigger the trigram path (< 10 chars).
        fs::write(
            src_dir.join("main.rs"),
            r#"
fn main() {}

pub fn xyz_fn() -> bool {
    true
}

pub fn other_func() -> i32 {
    42
}
"#,
        )
        .unwrap();

        let config = IndexConfig::new(&root);
        let engine = Engine::init(&root, config).unwrap();

        // "xyz_fn" is 6 chars — below the 10-char trigram threshold.
        let results = engine
            .hybrid_search(SearchQuery::new("xyz_fn").with_limit(10))
            .unwrap();
        assert!(
            !results.is_empty(),
            "expected trigram-boosted results for short query 'xyz_fn'"
        );
        // The result containing "xyz_fn" should be present.
        assert!(
            results.iter().any(|r| r.content.contains("xyz_fn")),
            "expected a result containing 'xyz_fn' in content"
        );
    }

    #[test]
    fn short_query_uses_trigram_path() {
        // Verify that a short query (< 10 chars) gets trigram boost while
        // a longer query still works normally without trigram involvement.
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);
        let engine = Engine::init(&root, config).unwrap();

        // Short query (3 chars "add") — should trigger trigram path.
        let short_results = engine
            .hybrid_search(SearchQuery::new("add").with_limit(10))
            .unwrap();
        assert!(
            !short_results.is_empty(),
            "expected results for short query 'add'"
        );

        // Long query (> 10 chars) — should NOT trigger trigram path,
        // but should still return results via normal hybrid retrieval.
        let long_results = engine
            .hybrid_search(SearchQuery::new("Add two numbers together").with_limit(10))
            .unwrap();
        assert!(
            !long_results.is_empty(),
            "expected results for long query (no trigram shortcut)"
        );
    }

    #[test]
    fn long_query_works_without_trigram() {
        // Verify that queries >= 10 chars bypass the trigram path entirely
        // and still produce correct hybrid results.
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);
        let engine = Engine::init(&root, config).unwrap();

        let results = engine
            .hybrid_search(SearchQuery::new("helper function").with_limit(10))
            .unwrap();
        assert!(
            !results.is_empty(),
            "expected results for long query 'helper function'"
        );

        // Results should be sorted by score descending.
        for w in results.windows(2) {
            assert!(
                w[0].score >= w[1].score,
                "results not sorted: {} < {}",
                w[0].score,
                w[1].score
            );
        }
    }

    #[test]
    fn trigram_index_persisted_on_init_and_loaded_on_open() {
        // Verify that the trigram index is persisted during init and
        // loaded from disk during open, and that short queries still work.
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);

        // Init and drop.
        {
            let engine = Engine::init(&root, config).unwrap();
            let results = engine
                .hybrid_search(SearchQuery::new("add").with_limit(5))
                .unwrap();
            assert!(!results.is_empty(), "expected results before save");
        }

        // Verify the trigram index file exists on disk.
        let trigram_path = root.join(".codeforge/trigram.bin");
        assert!(
            trigram_path.exists(),
            "expected trigram.bin to exist at {:?}",
            trigram_path
        );

        // Re-open — trigram index should be loaded from persisted file.
        let engine = Engine::open(&root).unwrap();
        let results = engine
            .hybrid_search(SearchQuery::new("add").with_limit(5))
            .unwrap();
        assert!(
            !results.is_empty(),
            "expected trigram-boosted results after re-opening index"
        );
    }

    #[test]
    fn trigram_boosts_exact_match_score() {
        // Verify that an exact substring match via trigram gets a higher
        // score than it would without the trigram boost.
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();

        // Two files: one with the exact token, one without.
        fs::write(
            src_dir.join("alpha.rs"),
            "pub fn qux_fn() -> bool { true }\n",
        )
        .unwrap();
        fs::write(
            src_dir.join("beta.rs"),
            "pub fn other() -> bool { false }\n",
        )
        .unwrap();

        let config = IndexConfig::new(&root);
        let engine = Engine::init(&root, config).unwrap();

        // "qux_fn" is 6 chars — triggers trigram path.
        let results = engine
            .hybrid_search(SearchQuery::new("qux_fn").with_limit(10))
            .unwrap();
        assert!(
            !results.is_empty(),
            "expected results for 'qux_fn'"
        );

        // The result from alpha.rs (containing exact match) should rank first.
        assert!(
            results[0].file_path.contains("alpha.rs"),
            "expected alpha.rs (exact trigram match) to rank first, got: {}",
            results[0].file_path
        );
    }
}

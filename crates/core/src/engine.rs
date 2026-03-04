use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use dashmap::DashMap;
use rayon::prelude::*;
use tracing::{debug, info, warn};

use crate::chunker::Chunker;
use crate::chunker::cast::CastChunker;
use crate::config::IndexConfig;
use crate::embedder::Embedder;
use crate::error::{CodeforgeError, Result};
use crate::formatter;
use crate::graph::{
    CodeGraph, GraphStats, ImportExtractor, ImportResolver, RepoMapOptions, compute_pagerank,
    generate_repo_map,
};
use crate::graph::extractor::RawImport;
use crate::index::TantivyIndex;
use crate::language::{Language, SemanticEntity, detect_language};
use crate::parser::Parser;
use crate::persistence::{IndexMeta, IndexStore};
use crate::reranker::Reranker;
use crate::retriever::bm25::BM25Retriever;
use crate::retriever::hybrid::HybridRetriever;
use crate::retriever::mmr::mmr_select;
use crate::retriever::{ChunkMeta, Retriever, SearchQuery, SearchResult, Strategy};
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
    /// Optional fastembed model for vector embeddings.
    embedder: Option<Arc<Embedder>>,
    /// Optional usearch HNSW vector index.
    vector: Option<VectorIndex>,
    /// Chunk metadata hydration table for vector results.
    chunk_meta: DashMap<u64, ChunkMeta>,
    /// Optional code dependency graph with PageRank scores.
    graph: Option<CodeGraph>,
    /// Optional cross-encoder reranker (BGE-Reranker-Base) for the `deep` strategy.
    reranker: Option<Arc<Reranker>>,
}

impl Engine {
    /// Initialize a new index for the project at `root`.
    ///
    /// Walks the directory tree, parses all supported source files in parallel
    /// using rayon, chunks them with the cAST algorithm, indexes chunks in
    /// Tantivy, optionally embeds them into the HNSW index, and populates the
    /// symbol table. All state is persisted to the `.codeforge/` directory.
    pub fn init(root: impl AsRef<Path>, config: IndexConfig) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .map_err(|e| CodeforgeError::Config(format!("cannot resolve root path: {e}")))?;

        let store = IndexStore::init(&root, &config)?;
        let tantivy = TantivyIndex::create_in_dir(&store.tantivy_dir())?;
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
            let g = build_graph(&files, &root, &config, &parser, &pending_imports);
            let scores = compute_pagerank(&g, config.graph.damping, config.graph.iterations);
            let mut g = g;
            g.apply_pagerank(&scores);
            let flat = g.to_flat();
            if let Err(e) = store.save_graph(&flat) {
                warn!(error = %e, "failed to persist graph");
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

        // Persist chunk_meta.
        let meta_pairs: Vec<(u64, ChunkMeta)> = chunk_meta_map
            .iter()
            .map(|e| (*e.key(), e.value().clone()))
            .collect();
        let meta_bytes = bitcode::serialize(&meta_pairs).map_err(|e| {
            CodeforgeError::Serialization(format!("failed to serialize chunk_meta: {e}"))
        })?;
        store.save_chunk_meta_bytes(&meta_bytes)?;

        // Persist vector index.
        if let Some(ref vec_idx) = vector {
            vec_idx.save(&store.vector_index_path(), &store.file_chunks_path())?;
        }

        let idx_meta = IndexMeta {
            version: "0.3.0".to_string(),
            file_count: files.len(),
            chunk_count: total_chunks,
            symbol_count: total_symbols,
            last_indexed: unix_timestamp_string(),
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
        })
    }

    /// Open an existing index from the `.codeforge/` directory.
    ///
    /// Restores the Tantivy index, symbol table, chunk metadata, and optional
    /// vector index from disk.
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

        // Restore chunk_meta.
        let chunk_meta: DashMap<u64, ChunkMeta> = if store.chunk_meta_path().exists() {
            let bytes = store.load_chunk_meta_bytes()?;
            let pairs: Vec<(u64, ChunkMeta)> = bitcode::deserialize(&bytes).map_err(|e| {
                CodeforgeError::Serialization(format!("failed to deserialize chunk_meta: {e}"))
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
            Ok(Some(data)) => Some(CodeGraph::from_flat(data)),
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
        })
    }

    /// Search the index using the strategy specified in `query`.
    ///
    /// - `Instant` → BM25 only
    /// - `Fast`    → BM25 + vector + RRF fusion (falls back to BM25 if no embedder)
    /// - `Thorough` → hybrid + MMR deduplication
    pub fn search(&self, query: SearchQuery) -> Result<Vec<SearchResult>> {
        match query.strategy {
            Strategy::Instant => {
                let retriever = BM25Retriever::new(&self.tantivy);
                retriever.search(&query)
                // NOTE: Instant is NOT graph-boosted (speed-first path).
            }
            Strategy::Fast => {
                let mut results = if let (Some(emb), Some(vec_idx)) = (&self.embedder, &self.vector)
                {
                    let retriever = HybridRetriever::new(
                        &self.tantivy,
                        Arc::clone(emb),
                        vec_idx,
                        &self.chunk_meta,
                        self.config.embedding.rrf_k,
                    );
                    retriever.search(&query)?
                } else {
                    debug!("no embedder available; falling back to BM25 for Fast strategy");
                    BM25Retriever::new(&self.tantivy).search(&query)?
                };
                self.apply_graph_boost(&mut results, self.config.graph.boost_weight);
                Ok(results)
            }
            Strategy::Explore => self.search_explore(query),
            Strategy::Thorough => {
                let mut results = if let (Some(emb), Some(vec_idx)) = (&self.embedder, &self.vector)
                {
                    let hybrid = HybridRetriever::new(
                        &self.tantivy,
                        Arc::clone(emb),
                        vec_idx,
                        &self.chunk_meta,
                        self.config.embedding.rrf_k,
                    );
                    let fetch_query = SearchQuery {
                        limit: query.limit * 3,
                        ..query.clone()
                    };
                    let candidates = hybrid.search(&fetch_query)?;

                    if candidates.is_empty() {
                        return Ok(Vec::new());
                    }

                    let (results_with_meta, embeddings): (Vec<SearchResult>, Vec<Vec<f32>>) =
                        candidates
                            .into_iter()
                            .filter_map(|r| {
                                let emb_vec = emb.embed_one(&r.content).ok()?;
                                Some((r, emb_vec))
                            })
                            .unzip();

                    let query_vec = emb.embed_one(&query.query)?;
                    mmr_select(
                        results_with_meta,
                        &query_vec,
                        &embeddings,
                        self.config.embedding.mmr_lambda,
                        query.limit,
                    )
                } else {
                    debug!("no embedder available; falling back to BM25 for Thorough strategy");
                    BM25Retriever::new(&self.tantivy).search(&query)?
                };
                self.apply_graph_boost(&mut results, self.config.graph.boost_weight);
                Ok(results)
            }
            Strategy::Deep => self.search_deep(query),
        }
    }

    /// Graph-expanded search (RepoHyper "Search-then-Expand" pattern).
    ///
    /// Phase 1: broad BM25 retrieval identifies anchor files.
    /// Phase 2: import graph expands anchor set to direct callers/callees.
    /// Phase 3: each newly-discovered neighbour file contributes its best
    ///          BM25 chunk, scored by PageRank to penalise low-importance files.
    ///
    /// This surfaces transitively-relevant code that a single BM25 pass misses
    /// — especially useful on 3 M+ LoC codebases where related logic is spread
    /// across many files connected only via import chains.
    fn search_explore(&self, query: SearchQuery) -> Result<Vec<SearchResult>> {
        use std::collections::HashSet;

        let bm25 = BM25Retriever::new(&self.tantivy);

        // Phase 1 — broad BM25 over-fetch.
        let wide_q = SearchQuery {
            limit: query.limit * 3,
            strategy: Strategy::Instant,
            ..query.clone()
        };
        let mut results = bm25.search(&wide_q)?;
        self.apply_graph_boost(&mut results, self.config.graph.boost_weight);

        // Phase 2 — expand via import graph.
        if let Some(ref graph) = self.graph {
            // Anchor = files in the top-limit initial results.
            let anchor_files: HashSet<String> = results
                .iter()
                .take(query.limit)
                .map(|r| r.file_path.clone())
                .collect();

            // Already-covered = all files in the full result set.
            let covered_files: HashSet<String> =
                results.iter().map(|r| r.file_path.clone()).collect();

            // Collect graph neighbours not already in the anchor set.
            let mut neighbour_files: HashSet<String> = HashSet::new();
            for file in &anchor_files {
                for n in graph.callers(file) {
                    if !anchor_files.contains(&n) {
                        neighbour_files.insert(n);
                    }
                }
                for n in graph.callees(file) {
                    if !anchor_files.contains(&n) {
                        neighbour_files.insert(n);
                    }
                }
            }

            // Phase 3 — for each uncovered neighbour, fetch its best BM25 chunk.
            // Cap at 8 neighbours to keep latency predictable.
            let mut expansion: Vec<SearchResult> = Vec::new();
            for neighbour in neighbour_files.iter().take(8) {
                if covered_files.contains(neighbour) {
                    continue;
                }
                let nq = SearchQuery {
                    query: query.query.clone(),
                    limit: 1,
                    file_filter: Some(neighbour.clone()),
                    strategy: Strategy::Instant,
                    token_budget: None,
                };
                if let Ok(mut exp) = bm25.search(&nq) {
                    for r in exp.iter_mut() {
                        // Scale by PageRank: neighbour files must be architecturally
                        // important to surface above the direct BM25 hits.
                        let pr = graph.node(&r.file_path).map(|n| n.pagerank).unwrap_or(0.0);
                        r.score *= 0.6 + 0.6 * pr;
                    }
                    expansion.extend(exp);
                }
            }
            results.extend(expansion);
            results.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }

        results.truncate(query.limit);
        Ok(results)
    }

    /// Multiply each result's score by `1 + weight * pagerank` then re-sort descending.
    fn apply_graph_boost(&self, results: &mut [SearchResult], weight: f32) {
        if let Some(ref graph) = self.graph {
            for r in results.iter_mut() {
                let pr = graph.node(&r.file_path).map(|n| n.pagerank).unwrap_or(0.0);
                r.score *= 1.0 + weight * pr;
            }
            results.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
    }

    /// Format search results as an LLM-friendly context block.
    pub fn format_results(&self, results: &[SearchResult], token_budget: Option<usize>) -> String {
        formatter::format_context(results, token_budget)
    }

    /// Query the symbol table.
    ///
    /// Performs case-insensitive substring matching on symbol names.
    /// If `file` is provided, also filters by file path.
    pub fn symbols(&self, filter: &str, file: Option<&str>) -> Result<Vec<Symbol>> {
        Ok(self.symbols.filter(filter, file))
    }

    /// Re-index a single file (after modification).
    ///
    /// Removes old data, re-parses, re-chunks, and re-indexes.
    /// When called directly, also recomputes PageRank and persists the graph.
    /// Use `apply_changes` to batch multiple files with a single PageRank pass.
    pub fn reindex_file(&mut self, path: &Path) -> Result<()> {
        self.reindex_file_impl(path, true)
    }

    fn reindex_file_impl(&mut self, path: &Path, do_graph_finalize: bool) -> Result<()> {
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
        if let Some(ref mut vec_idx) = self.vector {
            vec_idx.remove_file(&rel_str)?;
        }
        // Remove old chunk_meta entries for this file.
        self.chunk_meta.retain(|_, v| v.file_path != rel_str);

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

            // Store chunk_meta for vector hydration.
            self.chunk_meta.insert(
                chunk.id,
                ChunkMeta {
                    chunk_id: chunk.id,
                    file_path: rel_str.clone(),
                    language: chunk.language.name().to_string(),
                    line_start: chunk.line_start as u64,
                    line_end: chunk.line_end as u64,
                    signature: chunk.signatures.join("\n"),
                    scope_chain: chunk.scope_chain.clone(),
                    content: chunk.content.clone(),
                },
            );
        }

        for entity in &result.entities {
            self.symbols
                .insert(symbol_from_entity(entity, &rel_str, result.language));
        }

        // Embed new chunks and add to vector index.
        if let (Some(emb), Some(vec_idx)) = (self.embedder.as_ref(), self.vector.as_mut()) {
            let contextual = self.config.embedding.contextual_embeddings;
            let texts: Vec<String> = chunks
                .iter()
                .map(|c| {
                    if contextual {
                        if let Some(meta) = self.chunk_meta.get(&c.id) {
                            return make_embed_text(&meta, true);
                        }
                    }
                    c.content.clone()
                })
                .collect();
            match emb.embed(texts) {
                Ok(embeddings) => {
                    for (chunk, embedding) in chunks.iter().zip(embeddings.iter()) {
                        if let Err(e) = vec_idx.add_mut(chunk.id, embedding, &rel_str) {
                            warn!(error = %e, chunk_id = chunk.id, "failed to add vector");
                        }
                    }
                }
                Err(e) => warn!(error = %e, "embedding failed during reindex"),
            }
        }

        self.tantivy.commit()?;
        self.file_chunk_counts.insert(rel_str.clone(), chunks.len());

        // Update graph edges for this file using the already-parsed tree.
        // PageRank is only recomputed when do_graph_finalize=true (single-file
        // reindex). apply_changes() calls with false and does one pass at the end.
        let file_language = result.language;
        let raw_imports = ImportExtractor::extract(&result.tree, &source, file_language);
        if let Some(ref mut graph) = self.graph {
            graph.remove_file_edges(&rel_str);
            let indexed: std::collections::HashSet<String> =
                self.file_chunk_counts.keys().cloned().collect();
            let resolver = ImportResolver::new(indexed, self.config.root.clone());
            for raw in &raw_imports {
                if let Some(target) = resolver.resolve(raw, &rel_str) {
                    let target_lang =
                        detect_language(std::path::Path::new(&target)).unwrap_or(file_language);
                    graph.add_edge(&rel_str, &target, &raw.path, file_language, target_lang);
                }
            }
            if do_graph_finalize {
                let scores = compute_pagerank(
                    graph,
                    self.config.graph.damping,
                    self.config.graph.iterations,
                );
                graph.apply_pagerank(&scores);
                let flat = graph.to_flat();
                if let Err(e) = self.store.save_graph(&flat) {
                    warn!(error = %e, "failed to persist graph after reindex");
                }
            }
        }

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

        if let Some(ref mut vec_idx) = self.vector {
            vec_idx.remove_file(&rel_str)?;
        }
        self.chunk_meta.retain(|_, v| v.file_path != rel_str);

        // Update graph: remove node + all incident edges, recompute PageRank.
        if let Some(ref mut graph) = self.graph {
            graph.remove_file(&rel_str);
            let scores = compute_pagerank(
                graph,
                self.config.graph.damping,
                self.config.graph.iterations,
            );
            graph.apply_pagerank(&scores);
            let flat = graph.to_flat();
            if let Err(e) = self.store.save_graph(&flat) {
                warn!(error = %e, "failed to persist graph after remove");
            }
        }

        debug!(path = %path.display(), "removed file from index");
        Ok(())
    }

    /// Return summary statistics about the current index.
    pub fn stats(&self) -> IndexStats {
        let (graph_node_count, graph_edge_count) = self
            .graph
            .as_ref()
            .map(|g| {
                let s = g.stats();
                (s.node_count, s.edge_count)
            })
            .unwrap_or((0, 0));
        IndexStats {
            file_count: self.file_chunk_counts.len(),
            chunk_count: self.file_chunk_counts.values().sum(),
            symbol_count: self.symbols.len(),
            vector_count: self.vector.as_ref().map(|v| v.len()).unwrap_or(0),
            graph_node_count,
            graph_edge_count,
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
    pub fn watch(&self) -> Result<crate::watcher::FileWatcher> {
        crate::watcher::FileWatcher::new(&self.config.root, &self.config)
    }

    /// Apply a batch of file changes to the index.
    ///
    /// Processes all files first (parse, chunk, embed, Tantivy commit per file),
    /// then runs PageRank and persists the graph exactly once — regardless of
    /// how many files changed. For N-file batches (e.g. after `git pull`) this
    /// is N× faster than calling `reindex_file` repeatedly.
    pub fn apply_changes(&mut self, changes: &[crate::watcher::FileChange]) -> Result<()> {
        use crate::watcher::ChangeKind;

        if changes.is_empty() {
            return Ok(());
        }

        for change in changes {
            match change.kind {
                ChangeKind::Modified => {
                    // skip_graph_finalize=false — accumulate edge updates but
                    // defer PageRank until after all files are processed.
                    if let Err(e) = self.reindex_file_impl(&change.path, false) {
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

        // Single PageRank recompute for the entire batch.
        if let Some(ref mut graph) = self.graph {
            let scores =
                compute_pagerank(graph, self.config.graph.damping, self.config.graph.iterations);
            graph.apply_pagerank(&scores);
            let flat = graph.to_flat();
            if let Err(e) = self.store.save_graph(&flat) {
                warn!(error = %e, "failed to persist graph after batch changes");
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

        // Persist chunk_meta.
        let meta_pairs: Vec<(u64, ChunkMeta)> = self
            .chunk_meta
            .iter()
            .map(|e| (*e.key(), e.value().clone()))
            .collect();
        let meta_bytes = bitcode::serialize(&meta_pairs).map_err(|e| {
            CodeforgeError::Serialization(format!("failed to serialize chunk_meta: {e}"))
        })?;
        self.store.save_chunk_meta_bytes(&meta_bytes)?;

        // Persist vector index.
        if let Some(ref vec_idx) = self.vector {
            vec_idx.save(
                &self.store.vector_index_path(),
                &self.store.file_chunks_path(),
            )?;
        }

        // Persist graph.
        if let Some(ref g) = self.graph {
            let flat = g.to_flat();
            if let Err(e) = self.store.save_graph(&flat) {
                warn!(error = %e, "failed to persist graph in save()");
            }
        }

        let stats = self.stats();
        let meta = IndexMeta {
            version: "0.3.0".to_string(),
            file_count: stats.file_count,
            chunk_count: stats.chunk_count,
            symbol_count: stats.symbol_count,
            last_indexed: unix_timestamp_string(),
        };
        self.store.save_meta(&meta)?;

        Ok(())
    }

    // -------------------------------------------------------------------------
    // Graph public API
    // -------------------------------------------------------------------------

    /// Generate a token-budgeted repo map.  Returns `None` if the graph is not available.
    pub fn repo_map(&self, options: RepoMapOptions) -> Option<String> {
        self.graph
            .as_ref()
            .map(|g| generate_repo_map(g, &self.symbols, &options))
    }

    /// Return the files that directly import `file_path`.
    pub fn callers(&self, file_path: &str) -> Vec<String> {
        self.graph
            .as_ref()
            .map(|g| g.callers(file_path))
            .unwrap_or_default()
    }

    /// Return the files that `file_path` directly imports.
    pub fn callees(&self, file_path: &str) -> Vec<String> {
        self.graph
            .as_ref()
            .map(|g| g.callees(file_path))
            .unwrap_or_default()
    }

    /// Return transitive dependencies of `file_path` up to `depth` hops.
    pub fn dependencies(&self, file_path: &str, depth: usize) -> Vec<String> {
        self.graph
            .as_ref()
            .map(|g| g.transitive_callees(file_path, depth))
            .unwrap_or_default()
    }

    /// Return graph statistics, or `None` if the graph has not been built.
    pub fn graph_stats(&self) -> Option<GraphStats> {
        self.graph.as_ref().map(|g| g.stats())
    }

    /// Two-stage reranked search: hybrid first-pass then cross-encoder scoring.
    ///
    /// Phase 1: collect up to `max(limit × 3, 30)` candidates via the `Fast`
    ///          hybrid pipeline (BM25 + vector + graph boost).
    /// Phase 2: BGE-Reranker-Base scores each `(query, chunk)` pair jointly.
    ///          Results are re-sorted by reranker score and truncated.
    ///
    /// Falls back to `Thorough` if the reranker is not loaded.
    fn search_deep(&self, query: SearchQuery) -> Result<Vec<SearchResult>> {
        let reranker = match self.reranker.as_ref() {
            Some(r) => Arc::clone(r),
            None => {
                warn!(
                    "deep strategy requested but reranker not loaded \
                     (set reranker_enabled = true in config and re-open the engine)"
                );
                // Graceful degradation: run Thorough instead.
                return self.search(SearchQuery {
                    strategy: Strategy::Thorough,
                    ..query
                });
            }
        };

        // Phase 1: over-fetch candidates.
        let candidate_limit = (query.limit * 3).max(30);
        let candidate_query = SearchQuery {
            limit: candidate_limit,
            strategy: Strategy::Fast,
            ..query.clone()
        };

        let mut candidates = if let (Some(emb), Some(vec_idx)) =
            (&self.embedder, &self.vector)
        {
            let retriever = HybridRetriever::new(
                &self.tantivy,
                Arc::clone(emb),
                vec_idx,
                &self.chunk_meta,
                self.config.embedding.rrf_k,
            );
            retriever.search(&candidate_query)?
        } else {
            BM25Retriever::new(&self.tantivy).search(&candidate_query)?
        };
        self.apply_graph_boost(&mut candidates, self.config.graph.boost_weight);

        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        // Phase 2: rerank with cross-encoder.
        let docs: Vec<String> = candidates.iter().map(|r| r.content.clone()).collect();
        let ranked = reranker.rerank(&query.query, &docs)?;

        // Apply reranker scores — map (original_index, score) back onto candidates.
        for (orig_idx, score) in &ranked {
            candidates[*orig_idx].score = *score;
        }

        // Re-sort descending by the new scores.
        candidates
            .sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

        // Apply file filter and truncate to requested limit.
        if let Some(ref filter) = query.file_filter {
            candidates.retain(|r| r.file_path.contains(filter.as_str()));
        }
        candidates.truncate(query.limit);

        Ok(candidates)
    }

    /// Find all code chunks that reference `symbol` (BM25 full-text search).
    ///
    /// This is the "find usages" operation: given an identifier name, it returns
    /// ranked chunks where that identifier appears — including call sites,
    /// imports, and variable usages, not just the definition.
    pub fn search_usages(&self, symbol: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let query = SearchQuery::new(symbol).with_limit(limit);
        let mut results = BM25Retriever::new(&self.tantivy).search(&query)?;
        // Apply PageRank boost so architecturally central files rank first.
        self.apply_graph_boost(&mut results, self.config.graph.boost_weight);
        Ok(results)
    }

    // -------------------------------------------------------------------------
    // File and symbol reading
    // -------------------------------------------------------------------------

    /// Read raw source lines from a file in the indexed project.
    ///
    /// `path` must be relative to the project root (e.g. `"src/engine.rs"`).
    /// `line_start` and `line_end` are both **0-indexed inclusive** bounds.
    /// Omitting either means "from the beginning" / "to the end of file".
    ///
    /// Returns `None` if the file does not exist on disk.
    pub fn read_file_range(
        &self,
        path: &str,
        line_start: Option<u64>,
        line_end: Option<u64>,
    ) -> Result<Option<String>> {
        let abs = self.config.root.join(path);
        if !abs.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(&abs)?;
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len() as u64;
        let start = line_start.unwrap_or(0).min(total) as usize;
        let end = line_end.map(|e| (e + 1).min(total)).unwrap_or(total) as usize;
        Ok(Some(lines[start..end].join("\n")))
    }

    /// Read the complete source of the first symbol whose name matches `name`.
    ///
    /// Performs the same case-insensitive substring lookup as [`Engine::symbols`],
    /// then reads the exact source lines from disk.
    ///
    /// Returns `None` if no matching symbol is found or the file is not on disk.
    pub fn read_symbol_source(&self, name: &str, file: Option<&str>) -> Result<Option<String>> {
        let matches = self.symbols.filter(name, file);
        let sym = match matches.into_iter().next() {
            Some(s) => s,
            None => return Ok(None),
        };
        self.read_file_range(
            &sym.file_path,
            Some(sym.line_start as u64),
            Some(sym.line_end as u64),
        )
    }

    /// Return `true` if a `.codeforge/` index directory exists at `root`.
    ///
    /// Used by the MCP server to decide whether auto-init is needed.
    pub fn index_exists(root: impl AsRef<Path>) -> bool {
        IndexStore::exists(root.as_ref())
    }

    /// Perform a regex or literal search across all source files in the project.
    ///
    /// Unlike [`Engine::search`] which queries the pre-built BM25/vector index,
    /// `grep_code` scans the raw file content — ideal for exact identifiers,
    /// string literals, TODO comments, or any pattern requiring verbatim matching.
    ///
    /// - `literal`: when `true`, the pattern is treated as a plain string (all
    ///   regex metacharacters are escaped before compilation).
    /// - `file_glob`: optional glob pattern (e.g. `"*.rs"`, `"src/**/*.py"`) to
    ///   restrict which files are searched.  `None` searches all indexed files.
    /// - `context_lines`: number of surrounding lines to include (clamped to 5).
    /// - `limit`: maximum total matches to return (default 50).
    ///
    /// Returns [`CodeforgeError::Index`] if the pattern fails to compile.
    pub fn grep_code(
        &self,
        pattern: &str,
        literal: bool,
        file_glob: Option<&str>,
        context_lines: usize,
        limit: usize,
    ) -> Result<Vec<GrepMatch>> {
        use regex::Regex;

        let context_lines = context_lines.min(5);
        let limit = if limit == 0 { 50 } else { limit };

        let compiled_pattern = if literal {
            regex::escape(pattern)
        } else {
            pattern.to_string()
        };
        let re = Regex::new(&compiled_pattern)
            .map_err(|e| CodeforgeError::Index(format!("grep pattern error: {e}")))?;

        // Build a glob matcher if file_glob is provided.
        let glob_pat: Option<glob::Pattern> = match file_glob {
            Some(g) => Some(
                glob::Pattern::new(g)
                    .map_err(|e| CodeforgeError::Index(format!("invalid file glob: {e}")))?,
            ),
            None => None,
        };

        let mut matches: Vec<GrepMatch> = Vec::new();

        // Iterate over the already-indexed file set (relative paths).
        let mut rel_paths: Vec<String> = self.file_chunk_counts.keys().cloned().collect();
        rel_paths.sort_unstable(); // deterministic ordering

        'files: for rel_path in &rel_paths {
            // Apply glob filter if present.
            if let Some(ref pat) = glob_pat {
                // Match against both the full rel_path and just the filename.
                let filename = std::path::Path::new(rel_path)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                if !pat.matches(rel_path) && !pat.matches(filename) {
                    continue;
                }
            }

            let abs = self.config.root.join(rel_path);
            let content = match fs::read_to_string(&abs) {
                Ok(c) => c,
                Err(e) => {
                    warn!(file = %rel_path, error = %e, "grep_code: skipping unreadable file");
                    continue;
                }
            };

            let lines: Vec<&str> = content.lines().collect();
            let n = lines.len();

            for (i, line) in lines.iter().enumerate() {
                if let Some(m) = re.find(line) {
                    let before: Vec<String> = lines[i.saturating_sub(context_lines)..i]
                        .iter()
                        .map(|s| s.to_string())
                        .collect();
                    let after_start = (i + 1).min(n);
                    let after_end = (i + 1 + context_lines).min(n);
                    let after: Vec<String> = lines[after_start..after_end]
                        .iter()
                        .map(|s| s.to_string())
                        .collect();

                    matches.push(GrepMatch {
                        file_path: rel_path.clone(),
                        line_number: i as u64,
                        line: line.to_string(),
                        match_start: m.start(),
                        match_end: m.end(),
                        before,
                        after,
                    });

                    if matches.len() >= limit {
                        break 'files;
                    }
                }
            }
        }

        Ok(matches)
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
    let raw_imports = ImportExtractor::extract(&result.tree, &source, result.language);
    ctx.pending_imports
        .insert(rel_str.clone(), (raw_imports, result.language));

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
/// When `contextual` is `true`, prepends the file path, language, and AST
/// scope chain — the "contextual embeddings" technique from Sourcegraph Cody
/// that reduces retrieval failure rate by ~35 %.
fn make_embed_text(meta: &ChunkMeta, contextual: bool) -> String {
    if !contextual {
        return meta.content.clone();
    }
    let mut header = format!("File: {}\nLanguage: {}", meta.file_path, meta.language);
    if !meta.scope_chain.is_empty() {
        header.push_str(&format!("\nScope: {}", meta.scope_chain.join(" > ")));
    }
    if !meta.signature.is_empty() {
        header.push_str(&format!("\nSignature: {}", meta.signature));
    }
    format!("{header}\n\n{}", meta.content)
}

/// Batch-embed all pending chunks and add them to the vector index.
fn embed_and_index_chunks(
    pending: &DashMap<u64, String>,
    chunk_meta: &DashMap<u64, ChunkMeta>,
    embedder: &Embedder,
    vec_idx: &mut VectorIndex,
    contextual: bool,
) -> Result<()> {
    let entries: Vec<u64> = pending.iter().map(|e| *e.key()).collect();

    if entries.is_empty() {
        return Ok(());
    }

    info!(count = entries.len(), contextual, "embedding chunks");

    // Embed in batches of 256 (fastembed default).
    const BATCH: usize = 256;
    for batch in entries.chunks(BATCH) {
        let texts: Vec<String> = batch
            .iter()
            .map(|id| {
                chunk_meta
                    .get(id)
                    .map(|m| make_embed_text(&m, contextual))
                    .unwrap_or_default()
            })
            .collect();

        let embeddings = embedder.embed(texts)?;

        for (chunk_id, embedding) in batch.iter().zip(embeddings.into_iter()) {
            let file_path = chunk_meta
                .get(chunk_id)
                .map(|m| m.file_path.clone())
                .unwrap_or_default();
            if let Err(e) = vec_idx.add_mut(*chunk_id, &embedding, &file_path) {
                warn!(error = %e, chunk_id, "failed to add vector");
            }
        }
    }

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

        if config.exclude_patterns.iter().any(|p| p == name.as_ref()) {
            continue;
        }

        if path.is_dir() {
            walk_dir_recursive(&path, config, files)?;
        } else if path.is_file() {
            if config.languages.is_empty() {
                if detect_language(&path).is_some() {
                    files.push(path);
                }
            } else if let Some(lang) = detect_language(&path) {
                if config.languages.contains(&lang.name().to_lowercase()) {
                    files.push(path);
                }
            }
        }
    }

    Ok(())
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
    _config: &IndexConfig,
    parser: &Parser,
    import_cache: &DashMap<String, (Vec<RawImport>, Language)>,
) -> CodeGraph {
    let indexed: std::collections::HashSet<String> = files
        .iter()
        .map(|p| normalize_path(p.strip_prefix(root).unwrap_or(p)))
        .collect();

    let resolver = ImportResolver::new(indexed, root.to_path_buf());

    // Phase 1: resolve imports in parallel.
    // Each entry is (rel_str, language, Vec<(target, raw_path, target_lang)>).
    type ResolvedFile = (String, Language, Vec<(String, String, Language)>);
    let resolved: Vec<ResolvedFile> = files
        .par_iter()
        .filter_map(|abs_path| {
            let rel_str = normalize_path(abs_path.strip_prefix(root).unwrap_or(abs_path));

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
                ts_parser.set_language(&lang_support.tree_sitter_language()).ok()?;
                let tree = ts_parser.parse(&source, None)?;
                (ImportExtractor::extract(&tree, &source, language), language)
            };

            let edges: Vec<(String, String, Language)> = raw_imports
                .iter()
                .filter_map(|raw| {
                    resolver.resolve(raw, &rel_str).map(|target| {
                        let tl = detect_language(std::path::Path::new(&target))
                            .unwrap_or(language);
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
}

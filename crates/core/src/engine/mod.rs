mod files;
mod graph;
mod search;
mod sync;

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
use crate::error::{CodixingError, Result};
use crate::graph::extractor::RawImport;
use crate::graph::{CallExtractor, CodeGraph, ImportExtractor, ImportResolver, compute_pagerank};
use crate::index::TantivyIndex;
use crate::language::{Language, SemanticEntity, detect_language};
use crate::parser::Parser;
use crate::persistence::{IndexMeta, IndexStore};
use crate::reranker::Reranker;
use crate::retriever::ChunkMeta;
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

/// Statistics returned by [`Engine::sync`].
#[derive(Debug, Clone)]
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
        // Call names extracted during parse — resolved into Calls edges after
        // the symbol table is fully populated (end of parallel phase).
        let pending_calls: DashMap<String, Vec<String>> = DashMap::new();

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
            let mut g = build_graph(&files, &root, &config, &parser, &pending_imports);
            // Resolve call-site edges using the now-complete symbol table.
            add_call_edges(&mut g, &symbols, &pending_calls);
            let scores = compute_pagerank(&g, config.graph.damping, config.graph.iterations);
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

    /// Open an existing index from the `.codixing/` directory.
    ///
    /// Restores the Tantivy index, symbol table, chunk metadata, and optional
    /// vector index from disk.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .map_err(|e| CodixingError::Config(format!("cannot resolve root path: {e}")))?;

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

    /// Return `true` if a `.codixing/` index directory exists at `root`.
    ///
    /// Used by the MCP server to decide whether auto-init is needed.
    pub fn index_exists(root: impl AsRef<Path>) -> bool {
        IndexStore::exists(root.as_ref())
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
}

/// Process a single file: parse → chunk → index → extract symbols.
fn process_file(path: &Path, ctx: &IndexContext<'_>) -> Result<()> {
    let source = fs::read(path)?;
    let result = ctx.parser.parse_file(path, &source)?;

    let rel_str = ctx
        .config
        .normalize_path(path)
        .unwrap_or_else(|| normalize_path(path.strip_prefix(ctx.root).unwrap_or(path)));

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

    // Extract call sites for later call-graph edge resolution.
    let call_names = CallExtractor::extract_calls(&result.tree, &source, result.language);
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
}

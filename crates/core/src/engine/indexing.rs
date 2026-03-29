use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
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
use crate::graph::extract::{extract_definitions, extract_references};
use crate::graph::extractor::RawImport;
use crate::graph::types::{ReferenceKind, SymbolKind};
use crate::graph::{CallExtractor, CodeGraph, ImportExtractor, ImportResolver};
use crate::index::TantivyIndex;
use crate::index::trigram::FileTrigramIndex;
use crate::language::{Language, SemanticEntity, detect_language};
use crate::parser::Parser;
use crate::retriever::{ChunkMeta, ChunkMetaCompact};
use crate::symbols::{Symbol, SymbolTable};
use crate::vector::VectorIndex;

/// Shared context passed to `process_file` to avoid too-many-arguments.
pub(super) struct IndexContext<'a> {
    pub(super) root: &'a Path,
    pub(super) config: &'a IndexConfig,
    pub(super) parser: &'a Parser,
    pub(super) tantivy: &'a TantivyIndex,
    pub(super) symbols: &'a SymbolTable,
    pub(super) chunk_count: &'a AtomicUsize,
    pub(super) file_chunk_map: &'a DashMap<String, usize>,
    pub(super) chunk_meta_map: &'a DashMap<u64, ChunkMeta>,
    /// Pending chunks to embed: chunk_id → content.
    pub(super) pending_embeds: &'a DashMap<u64, String>,
    /// Imports extracted during parsing, keyed by relative path.
    /// Reused by `build_graph` to avoid re-reading/re-parsing files.
    pub(super) pending_imports: &'a DashMap<String, (Vec<RawImport>, Language)>,
    /// Call names extracted during parsing: rel_path → Vec<callee_name>.
    /// Resolved into `EdgeKind::Calls` edges after the symbol table is complete.
    pub(super) pending_calls: &'a DashMap<String, Vec<String>>,
    /// Full file content accumulated during parallel indexing for building
    /// a chunk-boundary-free file trigram index.
    pub(super) file_contents: &'a DashMap<String, Vec<u8>>,
}

/// Build a [`FileTrigramIndex`] from full file content.
///
/// Uses `file_contents` (complete file bytes accumulated during indexing)
/// to avoid missing trigrams that straddle chunk boundaries.
pub(super) fn build_file_trigram_from_content(
    file_contents: &DashMap<String, Vec<u8>>,
) -> FileTrigramIndex {
    let mut idx = FileTrigramIndex::new();
    for entry in file_contents.iter() {
        idx.add(entry.key(), entry.value());
    }
    idx
}

/// Build a chunk [`TrigramIndex`] from Tantivy stored fields.
///
/// Used as a fallback when the persisted chunk trigram index is missing and
/// chunk_meta has empty content (compact persistence mode).
pub(super) fn rebuild_trigram_from_tantivy(tantivy: &TantivyIndex) -> crate::index::TrigramIndex {
    let mut t = crate::index::TrigramIndex::new();
    match tantivy.all_chunk_ids_and_content() {
        Ok(pairs) => {
            t.build_batch(pairs.into_iter());
        }
        Err(e) => {
            warn!(error = %e, "failed to read Tantivy content for trigram rebuild");
        }
    }
    t
}

/// Build a [`FileTrigramIndex`] from Tantivy stored fields.
///
/// Groups chunk content by file path and builds the file-level trigram index.
/// Used as a fallback when persisted file trigram is missing and chunk_meta
/// has empty content (compact persistence mode).
pub(super) fn build_file_trigram_from_tantivy(tantivy: &TantivyIndex) -> FileTrigramIndex {
    let mut idx = FileTrigramIndex::new();
    match tantivy.all_file_path_content_pairs() {
        Ok(pairs) => {
            for (file_path, content) in &pairs {
                idx.add(file_path, content.as_bytes());
            }
        }
        Err(e) => {
            warn!(error = %e, "failed to read Tantivy content for file trigram rebuild");
        }
    }
    idx
}

/// Process a single file: parse → chunk → index → extract symbols.
pub(super) fn process_file(path: &Path, ctx: &IndexContext<'_>) -> Result<()> {
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
pub(super) fn make_embed_text(meta: &ChunkMeta, contextual: bool) -> String {
    if !contextual {
        return meta.content.clone();
    }
    let prefix = build_context_prefix(meta);
    format!("{prefix}{}", meta.content)
}

/// Deserialize `chunk_meta.bin` with backward compatibility.
///
/// Tries compact format (`Vec<(u64, ChunkMetaCompact)>`) first. If that fails,
/// falls back to legacy format (`Vec<(u64, ChunkMeta)>`) which includes content.
pub(super) fn deserialize_chunk_meta(bytes: &[u8]) -> Result<DashMap<u64, ChunkMeta>> {
    // Try compact format first (v2 — no content field).
    if let Ok(pairs) = bitcode::deserialize::<Vec<(u64, ChunkMetaCompact)>>(bytes) {
        let map = DashMap::new();
        for (k, compact) in pairs {
            map.insert(k, ChunkMeta::from(compact));
        }
        return Ok(map);
    }

    // Fall back to legacy format (v1 — includes content).
    let pairs: Vec<(u64, ChunkMeta)> = bitcode::deserialize(bytes).map_err(|e| {
        CodixingError::Serialization(format!("failed to deserialize chunk_meta: {e}"))
    })?;
    let map = DashMap::new();
    for (k, v) in pairs {
        map.insert(k, v);
    }
    Ok(map)
}

/// Serialize chunk_meta in compact format (without content).
pub(super) fn serialize_chunk_meta_compact(
    chunk_meta: &DashMap<u64, ChunkMeta>,
) -> Result<Vec<u8>> {
    let meta_pairs: Vec<(u64, ChunkMetaCompact)> = chunk_meta
        .iter()
        .map(|e| (*e.key(), ChunkMetaCompact::from(e.value())))
        .collect();
    bitcode::serialize(&meta_pairs)
        .map_err(|e| CodixingError::Serialization(format!("failed to serialize chunk_meta: {e}")))
}

/// Build a context prefix for a chunk to improve embedding quality.
///
/// Produces a single-line header with file path, language, scope chain, and
/// entity names so the embedding model knows the chunk's location in the
/// codebase. The prefix is prepended to chunk content before embedding but
/// is **not** stored in the index — only the raw content is persisted.
pub(super) fn build_context_prefix(meta: &ChunkMeta) -> String {
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
pub(super) const STREAM_BATCH_SIZE: usize = 256;

/// Embed all chunks from a single file, using late chunking when possible.
///
/// Tries `embed_file_late_chunking` first. If the file is too long or the
/// backend doesn't support it, falls back to independent per-chunk embedding
/// in `STREAM_BATCH_SIZE` windows.
///
/// Returns the number of chunks embedded.
pub(super) fn embed_single_file(
    embedder: &Embedder,
    chunk_meta: &DashMap<u64, ChunkMeta>,
    vec_idx: &mut VectorIndex,
    contextual: bool,
    root: &Path,
    file_path: &str,
    chunk_ids: &[u64],
) -> Result<usize> {
    let mut embedded = 0;

    // ── Late chunking attempt ─────────────────────────────────────────
    if !contextual {
        let abs_path = root.join(file_path);
        if let Ok(file_text) = fs::read_to_string(&abs_path) {
            let mut ordered: Vec<(u64, String)> = chunk_ids
                .iter()
                .filter_map(|id| chunk_meta.get(id).map(|m| (*id, m.content.clone())))
                .collect();
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
                    tracing::debug!(
                        path = %file_path,
                        "chunk content not found in file, falling back"
                    );
                    all_found = false;
                    break;
                }
            }

            if all_found {
                match embedder.embed_file_late_chunking(&file_text, &byte_ranges) {
                    Ok(Some(embeddings)) => {
                        for ((id, _), embedding) in ordered.iter().zip(embeddings.into_iter()) {
                            if let Err(e) = vec_idx.add_mut(*id, &embedding, file_path) {
                                tracing::warn!(error = %e, chunk_id = id, "failed to add vector");
                            }
                            embedded += 1;
                        }
                        return Ok(embedded);
                    }
                    Ok(None) => {
                        tracing::debug!(path = %file_path, "late chunking not applicable");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, path = %file_path, "late chunking failed");
                    }
                }
            }
        }
    }

    // ── Fallback: independent per-chunk embedding ─────────────────────
    for window in chunk_ids.chunks(STREAM_BATCH_SIZE) {
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
            let fp = chunk_meta
                .get(chunk_id)
                .map(|m| m.file_path.clone())
                .unwrap_or_default();
            if let Err(e) = vec_idx.add_mut(*chunk_id, &embedding, &fp) {
                tracing::warn!(error = %e, chunk_id, "failed to add vector");
            }
            embedded += 1;
        }
    }

    Ok(embedded)
}

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
pub(super) fn embed_and_index_chunks(
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
pub(super) fn embed_and_index_chunks_with_progress<F>(
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
        let done = embed_single_file(
            embedder, chunk_meta, vec_idx, contextual, root, file_path, chunk_ids,
        )?;
        embedded_so_far += done;
        if let Some(ref cb) = progress_callback {
            cb(embedded_so_far, total_chunks);
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
pub(super) fn walk_source_files(root: &Path, config: &IndexConfig) -> Result<Vec<PathBuf>> {
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
pub(super) fn add_call_edges(
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
pub(super) fn populate_symbol_graph(
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
pub(super) fn find_enclosing_function(
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
pub(super) fn build_graph(
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
pub(super) fn symbol_from_entity(
    entity: &SemanticEntity,
    file_path: &str,
    language: Language,
) -> Symbol {
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
pub(super) fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Simple unix timestamp as a human-readable string.
pub(super) fn unix_timestamp_string() -> String {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

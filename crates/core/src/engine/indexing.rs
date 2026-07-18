use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Instant, SystemTime};

use dashmap::DashMap;
use rayon::prelude::*;
use tracing::{debug, info, warn};

use crate::chunker::Chunker;
use crate::chunker::cast::CastChunker;
use crate::config::IndexConfig;
use crate::embedder::Embedder;
use crate::error::{CodixingError, Result};
use crate::graph::extract::{
    DefinitionInfo, ReferenceInfo, extract_definitions_from_tree, extract_references_from_tree,
};
use crate::graph::extractor::RawImport;
use crate::graph::types::{ReferenceKind, SymbolKind};
use crate::graph::{CallExtractor, CodeGraph, ImportExtractor, ImportResolver};
use crate::index::TantivyIndex;
use crate::index::trigram::FileTrigramIndex;
use crate::language::ipynb::{self, CellKind};
use crate::language::{Language, SemanticEntity, detect_language};
use crate::parser::Parser;
use crate::retriever::{ChunkMeta, ChunkMetaCompact};
use crate::symbols::{Symbol, SymbolTable};
use crate::vector::VectorIndex;

/// Init-only symbol definition without a repeated per-item file path.
/// The owning `DashMap` key already identifies the file.
pub(super) struct PendingDefinition {
    name: String,
    kind: SymbolKind,
    line: usize,
}

/// Init-only call reference. Non-call reference kinds are discarded before
/// they enter the corpus-wide pending set because symbol-graph population does
/// not consume them.
pub(super) struct PendingCallReference {
    target_name: String,
    line: usize,
}

pub(super) type PendingSymbolGraph =
    DashMap<String, (Vec<PendingDefinition>, Vec<PendingCallReference>)>;

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
    /// Pending chunks to embed. The value is intentionally empty: embedding
    /// reads content from `chunk_meta_map`, avoiding a second corpus-sized copy.
    pub(super) pending_embeds: &'a DashMap<u64, String>,
    /// Whether an embedder was successfully initialized for this run.
    pub(super) queue_embeddings: bool,
    /// Imports extracted during parsing, keyed by relative path.
    /// Reused by `build_graph` to avoid re-reading/re-parsing files.
    pub(super) pending_imports: &'a DashMap<String, (Vec<RawImport>, Language)>,
    /// Call names extracted during parsing: rel_path → Vec<callee_name>.
    /// Resolved into `EdgeKind::Calls` edges after the symbol table is complete.
    pub(super) pending_calls: &'a DashMap<String, Vec<String>>,
    /// Symbol definitions and references extracted from the parser tree during
    /// the primary indexing pass. Reused to build the symbol graph without
    /// re-reading and re-parsing every source file.
    pub(super) pending_symbol_graph: &'a PendingSymbolGraph,
    /// Symbol references extracted from doc files, keyed by relative path.
    /// Resolved into `EdgeKind::DocumentedBy` edges after the symbol table is built.
    pub(super) pending_doc_refs: &'a DashMap<String, Vec<crate::language::doc::SymbolRef>>,
    /// Per-file signature fingerprints (normalized relative path → fingerprint)
    /// for files that have AST entities. Persisted to `tree_signatures.bin` so
    /// the first sync after `init` can classify cosmetic edits. Keyed by the
    /// **normalized relative path** (`config.normalize_path`) so lookups are
    /// invariant to canonical/non-canonical root differences between `init` and
    /// `sync`. Files with no fingerprint are simply absent (→ STRUCTURAL).
    pub(super) pending_signatures: &'a DashMap<String, u64>,
    /// Complete successful-file hash baseline for the initial index.
    pub(super) pending_hashes: &'a DashMap<PathBuf, crate::persistence::FileHashEntry>,
}

/// Build a [`FileTrigramIndex`] from full files in a bounded second pass.
///
/// Files are read and released one at a time. The operating-system page cache
/// normally makes this inexpensive immediately after indexing, while avoiding
/// a corpus-sized `file_contents` map at peak initialization memory.
pub(super) fn build_file_trigram_from_files(
    files: &[PathBuf],
    root: &Path,
    config: &IndexConfig,
) -> FileTrigramIndex {
    let mut idx = FileTrigramIndex::new();
    for path in files {
        let rel_str = config
            .normalize_path(path)
            .unwrap_or_else(|| normalize_path(path.strip_prefix(root).unwrap_or(path)));
        match fs::read(path) {
            Ok(content) => idx.add(&rel_str, &content),
            Err(e) => {
                debug!(path = %path.display(), error = %e, "skipping file in trigram pass");
            }
        }
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
    let metadata_before = file_metadata_tuple(path);
    let source = fs::read(path)?;
    let metadata_after = file_metadata_tuple(path);
    let result = ctx.parser.parse_file_transient(path, &source)?;
    let hash_entry = stable_file_hash_entry(result.content_hash, metadata_before, metadata_after);

    let rel_str = ctx
        .config
        .normalize_path(path)
        .unwrap_or_else(|| normalize_path(path.strip_prefix(ctx.root).unwrap_or(path)));

    // Doc language branch — uses DocLanguageSupport instead of tree-sitter/config.
    if result.language.is_doc() {
        process_doc_file(&rel_str, &source, result.language, ctx)?;
        ctx.pending_hashes.insert(path.to_path_buf(), hash_entry);
        return Ok(());
    }

    // Jupyter branch — parse JSON and dispatch per-cell.
    if result.language.is_notebook() {
        process_jupyter_file(&rel_str, &source, ctx)?;
        ctx.pending_hashes.insert(path.to_path_buf(), hash_entry);
        return Ok(());
    }

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

        // Queue only IDs when embedding is active. Content already lives in
        // chunk_meta_map and the String value is deliberately empty.
        if ctx.queue_embeddings {
            ctx.pending_embeds.insert(chunk.id, String::new());
        }
    }

    for entity in &result.entities {
        ctx.symbols
            .insert(symbol_from_entity(entity, &rel_str, result.language));
    }

    // Record the signature fingerprint so the first post-init sync can classify
    // a cosmetic edit. Absent when there are no AST entities (→ STRUCTURAL).
    // Keyed by the normalized relative path (root-invariant).
    if let Some(fp) =
        super::fingerprint::signature_fingerprint(&result.entities, &source, result.language)
    {
        ctx.pending_signatures.insert(rel_str.clone(), fp);
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

    // Reuse the tree for the symbol-level graph too. The old graph phase
    // parsed each file up to three additional times (definitions twice plus
    // references once), which dominated initialization on large repositories.
    if let Some(tree) = result.tree.as_ref() {
        let (definitions, references) =
            extract_pending_symbol_graph(tree, &source, &rel_str, &result.language);
        if !definitions.is_empty() || !references.is_empty() {
            ctx.pending_symbol_graph
                .insert(rel_str.clone(), (definitions, references));
        }
    }

    ctx.pending_hashes.insert(path.to_path_buf(), hash_entry);

    debug!(
        path = %rel_str,
        language = result.language.name(),
        chunks = chunks.len(),
        entities = result.entities.len(),
        "indexed file"
    );

    Ok(())
}

fn file_metadata_tuple(path: &Path) -> Option<(SystemTime, u64)> {
    let metadata = fs::metadata(path).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

/// Pair a content hash only with metadata observed unchanged around its read.
/// If either stat fails or the file changes mid-read, zero metadata forces the
/// next sync to verify the body instead of trusting a mismatched fast-path key.
fn stable_file_hash_entry(
    content_hash: u64,
    before: Option<(SystemTime, u64)>,
    after: Option<(SystemTime, u64)>,
) -> crate::persistence::FileHashEntry {
    match (before, after) {
        (Some((before_time, before_size)), Some((after_time, after_size)))
            if before_time == after_time && before_size == after_size =>
        {
            crate::persistence::FileHashEntry::new(content_hash, Some(after_time), after_size)
        }
        _ => crate::persistence::FileHashEntry::new(content_hash, None, 0),
    }
}

/// Extract the compact symbol-graph records retained across bulk indexing.
/// Public extraction records include a file `String` on every item; here the
/// caller already stores records under a per-file key, so retaining those
/// duplicates would waste substantial memory on call-dense repositories.
pub(super) fn extract_pending_symbol_graph(
    tree: &tree_sitter::Tree,
    source: &[u8],
    file_path: &str,
    language: &Language,
) -> (Vec<PendingDefinition>, Vec<PendingCallReference>) {
    let definitions = extract_definitions_from_tree(tree, source, file_path, language)
        .into_iter()
        .map(|definition: DefinitionInfo| PendingDefinition {
            name: definition.name,
            kind: definition.kind,
            line: definition.line,
        })
        .collect();
    let references = extract_references_from_tree(tree, source, file_path, language)
        .into_iter()
        .filter(|reference: &ReferenceInfo| reference.kind == ReferenceKind::Call)
        .map(|reference| PendingCallReference {
            target_name: reference.target_name,
            line: reference.line,
        })
        .collect();
    (definitions, references)
}

/// Process a doc file (Markdown, HTML): parse sections → chunk → index.
///
/// Uses `DocLanguageSupport` instead of tree-sitter. Symbol refs are stored
/// in `pending_doc_refs` for later resolution into `DocumentedBy` graph edges.
fn process_doc_file(
    rel_str: &str,
    source: &[u8],
    language: Language,
    ctx: &IndexContext<'_>,
) -> Result<()> {
    let doc_support = ctx.parser.registry().get_doc(language).ok_or_else(|| {
        CodixingError::UnsupportedLanguage {
            path: std::path::PathBuf::from(rel_str),
        }
    })?;

    let file_name = std::path::Path::new(rel_str)
        .file_name()
        .and_then(|n| n.to_str());
    let sections = doc_support.parse_sections(source, file_name);
    let symbol_refs = doc_support.extract_symbol_refs(source);

    if !symbol_refs.is_empty() {
        ctx.pending_doc_refs
            .insert(rel_str.to_string(), symbol_refs.clone());
    }

    let mut chunks =
        crate::chunker::doc::chunk_doc(rel_str, source, &sections, language, &ctx.config.chunk);

    // Enrich chunks with symbol refs found in their byte range.
    for chunk in &mut chunks {
        chunk.entity_names = symbol_refs
            .iter()
            .filter(|r| {
                r.byte_range.start >= chunk.byte_start && r.byte_range.end <= chunk.byte_end
            })
            .map(|r| r.name.clone())
            .collect();
    }

    ctx.chunk_count.fetch_add(chunks.len(), Ordering::Relaxed);
    ctx.file_chunk_map.insert(rel_str.to_string(), chunks.len());

    for chunk in &chunks {
        ctx.tantivy.add_chunk(chunk)?;

        ctx.chunk_meta_map.insert(
            chunk.id,
            ChunkMeta {
                chunk_id: chunk.id,
                file_path: rel_str.to_string(),
                language: chunk.language.name().to_string(),
                line_start: chunk.line_start as u64,
                line_end: chunk.line_end as u64,
                signature: String::new(),
                scope_chain: chunk.scope_chain.clone(),
                entity_names: chunk.entity_names.clone(),
                content_hash: xxhash_rust::xxh3::xxh3_64(chunk.content.as_bytes()),
                content: chunk.content.clone(),
            },
        );

        if ctx.queue_embeddings {
            ctx.pending_embeds.insert(chunk.id, String::new());
        }
    }

    debug!(
        path = %rel_str,
        language = language.name(),
        chunks = chunks.len(),
        "indexed doc file"
    );

    Ok(())
}

/// Process a Jupyter `.ipynb` notebook: parse JSON → per-cell dispatch.
///
/// Each code cell routes through tree-sitter according to
/// `metadata.kernelspec.language` (default: Python). Markdown cells route
/// through `DocLanguageSupport::<Markdown>`. Raw and output cells are
/// skipped. All resulting chunks and entities are attributed to the real
/// notebook path (`rel_str`); the cell identifier is prepended onto
/// `scope_chain` so search surfaces the cell of origin without creating
/// synthetic file_path entries that would confuse filesystem consumers.
///
/// Known limitations:
/// - Byte and line ranges are cell-local, not notebook-local. Maps to
///   the chunk content but not back into the `.ipynb` JSON.
/// - Cell imports do not participate in the cross-file import graph yet —
///   pending_imports / pending_calls are not populated for notebook cells.
///
/// **Parallelism (v0.42)**: cells are processed via `par_iter().try_for_each`.
/// Each rayon worker owns a fresh `tree_sitter::Parser` (the one place
/// per-cell that can't be shared) and writes chunks into the lock-free
/// DashMap-backed sinks on `IndexContext`. `tantivy.add_chunk` and
/// `symbols.insert` are internally synchronized, so concurrent writes are
/// safe but serialize at the lock — the win is in tree-sitter parse +
/// chunk-builder time, not Tantivy ingest. Expected ~4-5× on notebooks
/// with ≥10 code cells.
fn process_jupyter_file(rel_str: &str, source: &[u8], ctx: &IndexContext<'_>) -> Result<()> {
    let cells = match ipynb::parse_notebook(source) {
        Ok(cells) => cells,
        Err(e) => {
            warn!(path = %rel_str, error = %e, "malformed notebook, skipping");
            return Ok(());
        }
    };

    let total_chunks = AtomicUsize::new(0);
    let total_entities = AtomicUsize::new(0);

    cells.par_iter().try_for_each(|cell| -> Result<()> {
        if matches!(cell.kind, CellKind::Raw) {
            return Ok(());
        }
        let cell_scope = format!("cell-{}", cell.id);
        let cell_bytes = cell.source.as_bytes();

        match cell.kind {
            CellKind::Code => {
                let ext = ipynb::kernel_language_extension(cell.kernel_language.as_deref())
                    .unwrap_or("py");
                // Detect target language off a synthetic extension — the
                // real notebook path has `.ipynb` and would resolve back
                // to Jupyter, so use a throwaway path for dispatch only.
                let synthetic = PathBuf::from(format!("cell.{ext}"));
                let Some(cell_lang) = detect_language(&synthetic) else {
                    return Ok(());
                };
                let Some(lang_support) = ctx.parser.registry().get(cell_lang) else {
                    return Ok(());
                };

                // Direct tree-sitter parse, bypassing the path-keyed cache
                // so we don't thrash it with per-cell synthetic paths. Each
                // rayon worker owns its own parser (tree_sitter::Parser is
                // !Send-by-value, but constructed inside the closure each
                // call so this is fine).
                let mut ts_parser = tree_sitter::Parser::new();
                if ts_parser
                    .set_language(&lang_support.tree_sitter_language())
                    .is_err()
                {
                    return Ok(());
                }
                let Some(tree) = ts_parser.parse(cell_bytes, None) else {
                    return Ok(());
                };
                let entities = lang_support.extract_entities(&tree, cell_bytes);

                let chunker = CastChunker;
                let mut chunks = chunker.chunk(
                    rel_str,
                    cell_bytes,
                    Some(&tree),
                    cell_lang,
                    &ctx.config.chunk,
                );
                for chunk in &mut chunks {
                    let mut scope = vec![cell_scope.clone()];
                    scope.append(&mut chunk.scope_chain);
                    chunk.scope_chain = scope;
                }

                total_chunks.fetch_add(chunks.len(), Ordering::Relaxed);
                for chunk in &chunks {
                    ctx.tantivy.add_chunk(chunk)?;
                    ctx.chunk_meta_map.insert(
                        chunk.id,
                        ChunkMeta {
                            chunk_id: chunk.id,
                            file_path: rel_str.to_string(),
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
                    if ctx.queue_embeddings {
                        ctx.pending_embeds.insert(chunk.id, String::new());
                    }
                }

                for entity in &entities {
                    let mut sym = symbol_from_entity(entity, rel_str, cell_lang);
                    let mut scope = vec![cell_scope.clone()];
                    scope.append(&mut sym.scope);
                    sym.scope = scope;
                    ctx.symbols.insert(sym);
                }
                total_entities.fetch_add(entities.len(), Ordering::Relaxed);
            }
            CellKind::Markdown => {
                let Some(doc_support) = ctx.parser.registry().get_doc(Language::Markdown) else {
                    return Ok(());
                };
                let file_name_hint = format!("{cell_scope}.md");
                let sections =
                    doc_support.parse_sections(cell_bytes, Some(file_name_hint.as_str()));
                let symbol_refs = doc_support.extract_symbol_refs(cell_bytes);

                let mut chunks = crate::chunker::doc::chunk_doc(
                    rel_str,
                    cell_bytes,
                    &sections,
                    Language::Markdown,
                    &ctx.config.chunk,
                );
                for chunk in &mut chunks {
                    let mut scope = vec![cell_scope.clone()];
                    scope.append(&mut chunk.scope_chain);
                    chunk.scope_chain = scope;
                    chunk.entity_names = symbol_refs
                        .iter()
                        .filter(|r| {
                            r.byte_range.start >= chunk.byte_start
                                && r.byte_range.end <= chunk.byte_end
                        })
                        .map(|r| r.name.clone())
                        .collect();
                }

                total_chunks.fetch_add(chunks.len(), Ordering::Relaxed);
                for chunk in &chunks {
                    ctx.tantivy.add_chunk(chunk)?;
                    ctx.chunk_meta_map.insert(
                        chunk.id,
                        ChunkMeta {
                            chunk_id: chunk.id,
                            file_path: rel_str.to_string(),
                            language: chunk.language.name().to_string(),
                            line_start: chunk.line_start as u64,
                            line_end: chunk.line_end as u64,
                            signature: String::new(),
                            scope_chain: chunk.scope_chain.clone(),
                            entity_names: chunk.entity_names.clone(),
                            content_hash: xxhash_rust::xxh3::xxh3_64(chunk.content.as_bytes()),
                            content: chunk.content.clone(),
                        },
                    );
                    if ctx.queue_embeddings {
                        ctx.pending_embeds.insert(chunk.id, String::new());
                    }
                }

                if !symbol_refs.is_empty() {
                    ctx.pending_doc_refs
                        .entry(rel_str.to_string())
                        .or_default()
                        .extend(symbol_refs);
                }
            }
            CellKind::Raw => unreachable!("raw cells filtered above"),
        }
        Ok(())
    })?;

    let total_chunks = total_chunks.load(Ordering::Relaxed);
    let total_entities = total_entities.load(Ordering::Relaxed);
    ctx.chunk_count.fetch_add(total_chunks, Ordering::Relaxed);
    ctx.file_chunk_map.insert(rel_str.to_string(), total_chunks);

    debug!(
        path = %rel_str,
        chunks = total_chunks,
        entities = total_entities,
        "indexed jupyter notebook"
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

/// Inner implementation for per-file embedding with late chunking.
///
/// The `sink` closure receives `(chunk_id, embedding, file_path)` for each
/// embedded chunk. Both [`embed_single_file`] and [`embed_file_collect`]
/// delegate to this function, differing only in what the sink does.
///
/// Returns `(chunks_embedded, used_late_chunking)`.
fn embed_single_file_inner(
    embedder: &Embedder,
    chunk_meta: &DashMap<u64, ChunkMeta>,
    contextual: bool,
    root: &Path,
    file_path: &str,
    chunk_ids: &[u64],
    sink: &mut dyn FnMut(u64, Vec<f32>, &str) -> Result<()>,
) -> Result<(usize, bool)> {
    let mut embedded = 0;

    // ── Late chunking attempt ─────────────────────────────────────────
    if !contextual {
        let abs_path = root.join(file_path);
        let safe_abs_path = root
            .canonicalize()
            .ok()
            .and_then(|canonical_root| {
                abs_path
                    .canonicalize()
                    .ok()
                    .map(|path| (canonical_root, path))
            })
            .and_then(|(canonical_root, path)| path.starts_with(canonical_root).then_some(path));
        if let Some(file_text) = safe_abs_path
            .as_deref()
            .and_then(|path| fs::read_to_string(path).ok())
        {
            // Capture line_start eagerly to avoid DashMap lookups during sort.
            let mut ordered: Vec<(u64, String, u64)> = chunk_ids
                .iter()
                .filter_map(|id| {
                    chunk_meta
                        .get(id)
                        .map(|m| (*id, m.content.clone(), m.line_start))
                })
                .collect();
            ordered.sort_by_key(|(_, _, line_start)| *line_start);

            let mut byte_ranges: Vec<(usize, usize)> = Vec::with_capacity(ordered.len());
            let mut search_from = 0usize;
            let mut all_found = true;
            for (_, content, _) in &ordered {
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
                        for ((id, _, _), embedding) in ordered.iter().zip(embeddings) {
                            sink(*id, embedding, file_path)?;
                            embedded += 1;
                        }
                        return Ok((embedded, true));
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

        for (chunk_id, embedding) in window.iter().zip(embeddings) {
            let fp = chunk_meta
                .get(chunk_id)
                .map(|m| m.file_path.clone())
                .unwrap_or_default();
            sink(*chunk_id, embedding, &fp)?;
            embedded += 1;
        }
    }

    Ok((embedded, false))
}

/// Embed all chunks from a single file, using late chunking when possible.
///
/// Tries `embed_file_late_chunking` first. If the file is too long or the
/// backend doesn't support it, falls back to independent per-chunk embedding
/// in `STREAM_BATCH_SIZE` windows.
///
/// Returns `(chunks_embedded, used_late_chunking)`.
pub(super) fn embed_single_file(
    embedder: &Embedder,
    chunk_meta: &DashMap<u64, ChunkMeta>,
    vec_idx: &mut VectorIndex,
    contextual: bool,
    root: &Path,
    file_path: &str,
    chunk_ids: &[u64],
) -> Result<(usize, bool)> {
    embed_single_file_inner(
        embedder,
        chunk_meta,
        contextual,
        root,
        file_path,
        chunk_ids,
        &mut |id, embedding, fp| vec_idx.add_mut(id, &embedding, fp),
    )
}

/// Like [`embed_single_file`] but collects `(chunk_id, embedding, file_path)` tuples
/// instead of inserting into a VectorIndex. Used by parallel workers that collect
/// embeddings independently and bulk-insert after all workers finish.
#[cfg(feature = "rustqueue")]
pub(super) fn embed_file_collect(
    embedder: &Embedder,
    chunk_meta: &DashMap<u64, ChunkMeta>,
    contextual: bool,
    root: &Path,
    file_path: &str,
    chunk_ids: &[u64],
) -> Result<Vec<(u64, Vec<f32>, String)>> {
    let mut collected = Vec::new();
    embed_single_file_inner(
        embedder,
        chunk_meta,
        contextual,
        root,
        file_path,
        chunk_ids,
        &mut |id, embedding, fp| {
            collected.push((id, embedding, fp.to_string()));
            Ok(())
        },
    )?;
    // Discard the used_late_chunking bool — callers of embed_file_collect
    // don't need per-file stats.
    Ok(collected)
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
) -> Result<super::embed_stats::EmbedTimingStats> {
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
) -> Result<super::embed_stats::EmbedTimingStats>
where
    F: Fn(usize, usize),
{
    use super::embed_stats::EmbedTimingStats;

    let entries: Vec<u64> = pending.iter().map(|e| *e.key()).collect();
    if entries.is_empty() {
        return Ok(EmbedTimingStats {
            embedded_chunks: 0,
            total_files: 0,
            wall_clock: std::time::Duration::ZERO,
            workers: 1, // Sync path — always 1 worker.
            late_chunking_files: 0,
            fallback_files: 0,
        });
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

    let total_files = file_chunks.len();
    let mut late_chunking_files = 0usize;
    let mut fallback_files = 0usize;
    let start = Instant::now();

    for (file_path, chunk_ids) in &file_chunks {
        let (done, used_late_chunking) = embed_single_file(
            embedder, chunk_meta, vec_idx, contextual, root, file_path, chunk_ids,
        )?;
        embedded_so_far += done;
        if used_late_chunking {
            late_chunking_files += 1;
        } else {
            fallback_files += 1;
        }
        if let Some(ref cb) = progress_callback {
            cb(embedded_so_far, total_chunks);
        }
    }

    Ok(EmbedTimingStats {
        embedded_chunks: embedded_so_far,
        total_files,
        wall_clock: start.elapsed(),
        workers: 1, // Sync path — always 1 worker.
        late_chunking_files,
        fallback_files,
    })
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
    let mut seen_canonical = HashSet::new();

    // Helper closure: collect matching files from a single directory tree.
    let mut collect = |walk_root: &Path| {
        let canonical_walk_root = match walk_root.canonicalize() {
            Ok(root) => root,
            Err(error) => {
                warn!(path = %walk_root.display(), %error, "cannot resolve source root, skipping");
                return;
            }
        };
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
            if entry
                .file_type()
                .is_some_and(|file_type| file_type.is_symlink())
            {
                debug!(path = %path.display(), "skipping symlinked source file");
                continue;
            }
            if !path.is_file() {
                continue;
            }
            let canonical_path = match path.canonicalize() {
                Ok(path) if path.starts_with(&canonical_walk_root) => path,
                Ok(path) => {
                    warn!(
                        path = %path.display(),
                        root = %canonical_walk_root.display(),
                        "skipping source file outside its configured root"
                    );
                    continue;
                }
                Err(error) => {
                    warn!(path = %path.display(), %error, "cannot resolve source file, skipping");
                    continue;
                }
            };
            // Secondary guard: explicit exclude patterns (exact path component match).
            let excluded = path.components().any(|c| {
                let s = c.as_os_str().to_string_lossy();
                config.exclude_patterns.iter().any(|p| p == s.as_ref())
            });
            if excluded {
                continue;
            }
            let supported = if config.languages.is_empty() {
                detect_language(path).is_some()
            } else {
                detect_language(path).is_some_and(|language| {
                    config.languages.contains(&language.name().to_lowercase())
                })
            };
            if supported && seen_canonical.insert(canonical_path.clone()) {
                files.push(canonical_path);
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

/// Resolve doc symbol references against the symbol table and add
/// `EdgeKind::DocumentedBy` edges from doc files to code files.
///
/// Only adds an edge when exactly one file defines a symbol with the
/// given name — same conservative heuristic as `add_call_edges`.
pub(super) fn add_doc_edges(
    graph: &mut CodeGraph,
    symbols: &SymbolTable,
    pending_doc_refs: &DashMap<String, Vec<crate::language::doc::SymbolRef>>,
) {
    let mut total = 0usize;
    for entry in pending_doc_refs.iter() {
        let doc_file = entry.key();
        let refs = entry.value();

        let doc_lang = graph
            .node(doc_file)
            .map(|n| n.language)
            .unwrap_or(Language::Markdown);

        let mut seen_targets = std::collections::HashSet::new();
        for sym_ref in refs.iter() {
            let syms = symbols.lookup(&sym_ref.name);
            // Collect unique defining files, excluding the doc file itself.
            let target_files: std::collections::HashSet<&str> = syms
                .iter()
                .map(|s| s.file_path.as_str())
                .filter(|&fp| fp != doc_file.as_str())
                .collect();

            // Only add edge if unambiguous (exactly 1 defining file).
            if target_files.len() == 1 {
                let target = *target_files.iter().next().unwrap();
                if seen_targets.insert(target.to_string()) {
                    let target_lang =
                        detect_language(std::path::Path::new(target)).unwrap_or(Language::Rust);
                    graph.add_doc_edge(doc_file, target, &sym_ref.name, doc_lang, target_lang);
                    total += 1;
                }
            }
        }
    }
    if total > 0 {
        info!(doc_edges = total, "added DocumentedBy edges from doc files");
    }
}

/// Populate the symbol-level inner graph with definitions and call references.
///
/// Consumes definitions and references extracted from the already-parsed trees
/// in the primary indexing pass, then inserts them as nodes and edges into the
/// `CodeGraph::inner` graph. This gives precise symbol->symbol call edges that
/// complement the coarser file-level import/call edges without any extra parse.
///
/// Must be called after the parallel parse phase so that all files are available.
pub(super) fn populate_symbol_graph(graph: &mut CodeGraph, symbol_data: PendingSymbolGraph) {
    use std::collections::HashMap;

    // Phase 1: consume each file's definitions to build the global name map,
    // retaining only function line/node pairs plus call references for phase 2.
    // This releases definition names/kinds and the DashMap allocation while the
    // graph grows instead of overlapping both corpus-wide representations.
    let mut name_to_indices: HashMap<String, Vec<petgraph::graph::NodeIndex>> = HashMap::new();
    let mut pending_references = Vec::with_capacity(symbol_data.len());

    for (rel_str, (definitions, references)) in symbol_data {
        let mut func_defs = Vec::new();
        for def in definitions {
            let is_function = def.kind == SymbolKind::Function;
            let idx = graph.add_symbol_with_line(&def.name, &rel_str, def.kind, def.line);
            name_to_indices.entry(def.name).or_default().push(idx);
            if is_function {
                func_defs.push((def.line, idx));
            }
        }
        func_defs.sort_by_key(|(line, _)| *line);
        pending_references.push((rel_str, func_defs, references));
    }

    // Phase 2: consume references and wire call edges.
    let mut total_edges = 0usize;
    for (rel_str, func_defs, references) in pending_references {
        for r in references {
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

#[cfg(test)]
mod init_hash_tests {
    use super::{stable_file_hash_entry, walk_source_files};
    use crate::config::IndexConfig;
    use std::fs;
    use std::time::{Duration, SystemTime};
    use tempfile::tempdir;

    #[test]
    fn changing_metadata_forces_next_sync_to_verify_content() {
        let first = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        let second = SystemTime::UNIX_EPOCH + Duration::from_secs(11);
        let entry = stable_file_hash_entry(42, Some((first, 100)), Some((second, 100)));

        assert_eq!(entry.content_hash, 42);
        assert_eq!(entry.mtime(), None);
        assert_eq!(entry.size, 0);
        assert!(entry.file_might_have_changed(Some(second), 100));
    }

    #[test]
    fn stable_metadata_is_bound_to_the_indexed_hash() {
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        let entry = stable_file_hash_entry(42, Some((time, 100)), Some((time, 100)));

        assert_eq!(entry.mtime(), Some(time));
        assert_eq!(entry.size, 100);
        assert!(!entry.file_might_have_changed(Some(time), 100));
    }

    #[cfg(unix)]
    #[test]
    fn source_walk_rejects_outside_symlinks_and_deduplicates_overlapping_roots() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let root = dir.path().join("repo");
        let src = root.join("src");
        let outside = dir.path().join("outside.rs");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("inside.rs"), "fn inside() {}\n").unwrap();
        fs::write(&outside, "fn outside_secret() {}\n").unwrap();
        symlink(&outside, src.join("escape.rs")).unwrap();

        let mut config = IndexConfig::new(&root);
        config.extra_roots.push(src.clone());
        let files = walk_source_files(&root, &config).unwrap();

        assert_eq!(files, vec![src.join("inside.rs")]);
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
        doc_comment: entity.doc_comment.clone(),
        visibility: entity.visibility.clone(),
        type_relations: entity.type_relations.clone(),
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

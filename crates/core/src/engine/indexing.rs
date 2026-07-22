use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
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

/// One bounded source read plus metadata bracketing the exact bytes.
pub(super) struct BoundedSourceRead {
    pub(super) bytes: Vec<u8>,
    pub(super) metadata_before: Option<(SystemTime, u64)>,
    pub(super) metadata_after: Option<(SystemTime, u64)>,
}

/// Read at most `max_file_bytes + 1` bytes from a source file.
///
/// `None` means the file exceeded the configured limit, including if it grew
/// after the directory walk. This closes the stat/read TOCTOU that otherwise
/// lets a generated multi-gigabyte file blow the process RSS.
pub(super) fn read_source_bounded(
    path: &Path,
    max_file_bytes: u64,
) -> Result<Option<BoundedSourceRead>> {
    let mut file = fs::File::open(path)?;
    let metadata_before = file
        .metadata()
        .ok()
        .and_then(|metadata| Some((metadata.modified().ok()?, metadata.len())));
    if max_file_bytes > 0 && metadata_before.is_some_and(|(_, bytes)| bytes > max_file_bytes) {
        // Verify the cap against bytes observed from this open handle. A stat
        // alone can race a concurrent truncate and incorrectly remove a file
        // that is already back under the limit. Seeking keeps this O(1) even
        // for sparse or multi-gigabyte generated files.
        file.seek(SeekFrom::Start(max_file_bytes))?;
        let mut probe = [0_u8; 1];
        if file.read(&mut probe)? == 1 {
            return Ok(None);
        }
        file.seek(SeekFrom::Start(0))?;
    }

    let capacity = metadata_before
        .map(|(_, bytes)| {
            let bounded = if max_file_bytes == 0 {
                bytes
            } else {
                bytes.min(max_file_bytes)
            };
            bounded.min(usize::MAX as u64) as usize
        })
        .unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    if max_file_bytes == 0 {
        file.read_to_end(&mut bytes)?;
    } else {
        (&mut file)
            .take(max_file_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max_file_bytes {
            return Ok(None);
        }
    }
    let metadata_after = file_metadata_tuple(path);
    Ok(Some(BoundedSourceRead {
        bytes,
        metadata_before,
        metadata_after,
    }))
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
    pub(super) file_chunk_map: &'a DashMap<String, Box<[u64]>>,
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
    /// Shared per-file trigram union for raw-source grep and exact chunk search.
    ///
    /// The source scan happens before this short lock, so parallel workers do
    /// not retain a second corpus-sized copy or serialize parsing/chunking.
    pub(super) file_trigram: &'a Mutex<FileTrigramIndex>,
}

/// Add both the raw file and every non-verbatim stored chunk representation to
/// the shared trigram index. Parser-produced text can differ from raw container
/// bytes (decoded notebooks, PDFs, normalized HTML), so exact search needs the
/// union while grep still needs the raw bytes. Preparing the union before the
/// lock keeps the serialized publication path short.
fn add_search_trigrams(
    ctx: &IndexContext<'_>,
    path: &str,
    source: &[u8],
    chunks: &[crate::chunker::Chunk],
) {
    let trigrams = FileTrigramIndex::prepare_contents(
        std::iter::once(source).chain(
            chunks
                .iter()
                .filter(|chunk| {
                    source.get(chunk.byte_start..chunk.byte_end) != Some(chunk.content.as_bytes())
                })
                .map(|chunk| chunk.content.as_bytes()),
        ),
    );
    ctx.file_trigram
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .add_prepared(path, &trigrams);
}

/// Stage every Tantivy document for one file before publishing any of its
/// exact postings or in-memory metadata. A late writer failure therefore
/// cannot leave postings that point at missing documents.
fn publish_chunks(
    rel_str: &str,
    chunks: &[crate::chunker::Chunk],
    ctx: &IndexContext<'_>,
) -> Result<()> {
    for chunk in chunks {
        if let Err(error) = ctx.tantivy.add_chunk(chunk) {
            if let Err(rollback_error) = ctx.tantivy.remove_file(rel_str) {
                warn!(
                    path = %rel_str,
                    error = %rollback_error,
                    "failed to roll back partially staged file"
                );
            }
            return Err(error);
        }
        #[cfg(feature = "internal-testing")]
        if chunk
            .content
            .contains("codixing-test-fail-after-first-tantivy-add")
        {
            ctx.tantivy.remove_file(rel_str)?;
            return Err(CodixingError::Index(
                "injected failure after first Tantivy add".to_string(),
            ));
        }
    }

    ctx.chunk_count.fetch_add(chunks.len(), Ordering::Relaxed);
    ctx.file_chunk_map.insert(
        rel_str.to_string(),
        chunks
            .iter()
            .map(|chunk| chunk.id)
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    for chunk in chunks {
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
                content: if ctx.queue_embeddings {
                    chunk.content.clone()
                } else {
                    String::new()
                },
            },
        );
        if ctx.queue_embeddings {
            ctx.pending_embeds.insert(chunk.id, String::new());
        }
    }
    Ok(())
}

/// Build a [`FileTrigramIndex`] from Tantivy stored fields.
///
/// Streams stored chunks into the file-level trigram index. This is the
/// fallback when its persisted artifact is missing or corrupt, and avoids a
/// second corpus-sized vector of source bodies during recovery.
pub(super) fn build_file_trigram_from_tantivy(tantivy: &TantivyIndex) -> FileTrigramIndex {
    let mut idx = FileTrigramIndex::new();
    if let Err(e) = tantivy.visit_all_file_path_content_pairs(|file_path, content| {
        idx.add(file_path, content.as_bytes());
        Ok(())
    }) {
        warn!(error = %e, "failed to read Tantivy content for file trigram rebuild");
    }
    idx
}

/// Process a single file: parse → chunk → index → extract symbols.
fn parse_source_for_init(
    path: &Path,
    source: &[u8],
    ctx: &IndexContext<'_>,
) -> Result<crate::parser::ParseResult> {
    #[cfg(feature = "internal-testing")]
    if source
        .windows(b"codixing-test-skip-before-index-publication".len())
        .any(|window| window == b"codixing-test-skip-before-index-publication")
    {
        return Err(CodixingError::Parse {
            path: path.to_path_buf(),
            message: "injected file-local parse failure".to_string(),
        });
    }

    ctx.parser.parse_file_transient(path, source)
}

pub(super) fn process_file(path: &Path, ctx: &IndexContext<'_>) -> Result<()> {
    let read = match read_source_bounded(path, ctx.config.max_file_bytes) {
        Ok(read) => read,
        Err(error) => {
            // Source trees can change while a large parallel walk is running.
            // Nothing has been staged for this file yet, so a vanished or
            // temporarily unreadable source is safe to omit from this snapshot.
            warn!(path = %path.display(), %error, "cannot read source during initialization, skipping");
            return Ok(());
        }
    };
    let Some(read) = read else {
        debug!(path = %path.display(), limit = ctx.config.max_file_bytes, "source grew beyond max_file_bytes during indexing");
        return Ok(());
    };
    let BoundedSourceRead {
        bytes: source,
        metadata_before,
        metadata_after,
    } = read;
    let result = match parse_source_for_init(path, &source, ctx) {
        Ok(result) => result,
        Err(error) => {
            // Parsing has no index side effects. Keep healthy siblings usable
            // when one malformed or unsupported source cannot be interpreted.
            warn!(path = %path.display(), %error, "cannot parse source during initialization, skipping");
            return Ok(());
        }
    };
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
        if process_jupyter_file(&rel_str, &source, ctx)? == NotebookProcessOutcome::Skipped {
            return Ok(());
        }
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

    publish_chunks(&rel_str, &chunks, ctx)?;

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

    add_search_trigrams(ctx, &rel_str, &source, &chunks);
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
pub(super) fn stable_file_hash_entry(
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

    publish_chunks(rel_str, &chunks, ctx)?;
    if !symbol_refs.is_empty() {
        ctx.pending_doc_refs
            .insert(rel_str.to_string(), symbol_refs);
    }

    add_search_trigrams(ctx, rel_str, source, &chunks);

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
/// Cells are parsed and chunked in parallel, then their prepared artifacts are
/// published as one file transaction. This preserves parallel tree-sitter work
/// without exposing partial notebook state if Tantivy rejects a later chunk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NotebookProcessOutcome {
    Indexed,
    Skipped,
}

fn process_jupyter_file(
    rel_str: &str,
    source: &[u8],
    ctx: &IndexContext<'_>,
) -> Result<NotebookProcessOutcome> {
    let cells = match ipynb::parse_notebook(source) {
        Ok(cells) => cells,
        Err(e) => {
            warn!(path = %rel_str, error = %e, "malformed notebook, skipping");
            return Ok(NotebookProcessOutcome::Skipped);
        }
    };

    #[derive(Default)]
    struct CellArtifacts {
        chunks: Vec<crate::chunker::Chunk>,
        symbols: Vec<Symbol>,
        doc_refs: Vec<crate::language::doc::SymbolRef>,
    }

    let mut id_counts = HashMap::<&str, usize>::new();
    for cell in &cells {
        *id_counts.entry(cell.id.as_str()).or_default() += 1;
    }

    let prepared = cells
        .par_iter()
        .map(|cell| -> Result<CellArtifacts> {
            if matches!(cell.kind, CellKind::Raw) {
                return Ok(CellArtifacts::default());
            }
            let cell_scope = format!("cell-{}", cell.id);
            let identity = if id_counts.get(cell.id.as_str()).copied().unwrap_or(0) == 1 {
                format!("{rel_str}#cell:{}", cell.id)
            } else {
                format!("{rel_str}#cell:{}:{}", cell.id, cell.index)
            };
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
                        return Ok(CellArtifacts::default());
                    };
                    let Some(lang_support) = ctx.parser.registry().get(cell_lang) else {
                        return Ok(CellArtifacts::default());
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
                        return Ok(CellArtifacts::default());
                    }
                    let Some(tree) = ts_parser.parse(cell_bytes, None) else {
                        return Ok(CellArtifacts::default());
                    };
                    let entities = lang_support.extract_entities(&tree, cell_bytes);

                    let chunker = CastChunker;
                    let mut chunks = chunker.chunk(
                        &identity,
                        cell_bytes,
                        Some(&tree),
                        cell_lang,
                        &ctx.config.chunk,
                    );
                    for chunk in &mut chunks {
                        chunk.file_path = rel_str.to_string();
                        let mut scope = vec![cell_scope.clone()];
                        scope.append(&mut chunk.scope_chain);
                        chunk.scope_chain = scope;
                    }

                    let symbols = entities
                        .iter()
                        .map(|entity| {
                            let mut sym = symbol_from_entity(entity, rel_str, cell_lang);
                            let mut scope = vec![cell_scope.clone()];
                            scope.append(&mut sym.scope);
                            sym.scope = scope;
                            sym
                        })
                        .collect();
                    Ok(CellArtifacts {
                        chunks,
                        symbols,
                        doc_refs: Vec::new(),
                    })
                }
                CellKind::Markdown => {
                    let Some(doc_support) = ctx.parser.registry().get_doc(Language::Markdown)
                    else {
                        return Ok(CellArtifacts::default());
                    };
                    let file_name_hint = format!("{cell_scope}.md");
                    let sections =
                        doc_support.parse_sections(cell_bytes, Some(file_name_hint.as_str()));
                    let symbol_refs = doc_support.extract_symbol_refs(cell_bytes);

                    let mut chunks = crate::chunker::doc::chunk_doc(
                        &identity,
                        cell_bytes,
                        &sections,
                        Language::Markdown,
                        &ctx.config.chunk,
                    );
                    for chunk in &mut chunks {
                        chunk.file_path = rel_str.to_string();
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
                    Ok(CellArtifacts {
                        chunks,
                        symbols: Vec::new(),
                        doc_refs: symbol_refs,
                    })
                }
                CellKind::Raw => unreachable!("raw cells filtered above"),
            }
        })
        .collect::<Result<Vec<_>>>()?;

    let total_chunks = prepared.iter().map(|cell| cell.chunks.len()).sum();
    let total_entities = prepared.iter().map(|cell| cell.symbols.len()).sum();
    let mut chunks = Vec::with_capacity(total_chunks);
    let mut symbols = Vec::with_capacity(total_entities);
    let mut doc_refs = Vec::new();
    for cell in prepared {
        chunks.extend(cell.chunks);
        symbols.extend(cell.symbols);
        doc_refs.extend(cell.doc_refs);
    }

    publish_chunks(rel_str, &chunks, ctx)?;
    add_search_trigrams(ctx, rel_str, source, &chunks);
    for symbol in symbols {
        ctx.symbols.insert(symbol);
    }
    if !doc_refs.is_empty() {
        ctx.pending_doc_refs.insert(rel_str.to_string(), doc_refs);
    }

    debug!(
        path = %rel_str,
        chunks = total_chunks,
        entities = total_entities,
        "indexed jupyter notebook"
    );

    Ok(NotebookProcessOutcome::Indexed)
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

/// Reconstructible identity for the exact text used to produce an embedding.
///
/// Compact chunk metadata deliberately omits the body after publication, so a
/// contextual reuse key cannot hash `make_embed_text` directly after reopen.
/// Hash the context prefix together with the already-persisted body hash
/// instead. The domain/version tag keeps this cache identity independent from
/// other xxh3 hashes and makes future context-format changes explicit.
pub(super) fn embedding_reuse_key(meta: &ChunkMeta, contextual: bool) -> u64 {
    if !contextual {
        return meta.content_hash;
    }

    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    hasher.update(b"codixing:contextual-embedding-reuse:v1\0");
    hasher.update(build_context_prefix(meta).as_bytes());
    hasher.update(&meta.content_hash.to_le_bytes());
    hasher.digest()
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

type EmbedSink<'a> = dyn FnMut(u64, Vec<f32>, &str) -> Result<()> + 'a;

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
    sink: &mut EmbedSink<'_>,
) -> Result<(usize, bool)> {
    let mut embedded = 0;

    // ── Late chunking attempt ─────────────────────────────────────────
    if !contextual {
        let abs_path = root.join(file_path);
        let safe_abs_path = root
            .canonicalize()
            .ok()
            .zip(abs_path.canonicalize().ok())
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

const SOURCE_CANONICALIZE_BATCH_SIZE: usize = 4096;

#[cfg(test)]
std::thread_local! {
    static PARALLEL_CANONICALIZE_BATCHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SERIAL_CANONICALIZE_BATCHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static LAST_CANONICALIZE_POOL_SIZE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn canonicalize_source_candidate(path: PathBuf, canonical_walk_root: &Path) -> Option<PathBuf> {
    match path.canonicalize() {
        Ok(path) if path.starts_with(canonical_walk_root) => Some(path),
        Ok(path) => {
            warn!(
                path = %path.display(),
                root = %canonical_walk_root.display(),
                "skipping source file outside its configured root"
            );
            None
        }
        Err(error) => {
            warn!(path = %path.display(), %error, "cannot resolve source file, skipping");
            None
        }
    }
}

fn canonicalize_source_candidates_with_mode(
    candidates: Vec<PathBuf>,
    canonical_walk_root: &Path,
    parallel: bool,
) -> Vec<Option<PathBuf>> {
    if candidates.is_empty() {
        return Vec::new();
    }
    if parallel {
        #[cfg(test)]
        {
            PARALLEL_CANONICALIZE_BATCHES.with(|count| count.set(count.get() + 1));
            LAST_CANONICALIZE_POOL_SIZE.with(|size| size.set(rayon::current_num_threads()));
        }
        // Vec's indexed parallel iterator preserves input order on collect.
        // Use the caller-configured Rayon pool rather than creating another
        // pool or spawning one thread per path.
        candidates
            .into_par_iter()
            .map(|path| canonicalize_source_candidate(path, canonical_walk_root))
            .collect()
    } else {
        #[cfg(test)]
        SERIAL_CANONICALIZE_BATCHES.with(|count| count.set(count.get() + 1));
        candidates
            .into_iter()
            .map(|path| canonicalize_source_candidate(path, canonical_walk_root))
            .collect()
    }
}

fn append_canonicalized_source_candidates(
    candidates: Vec<PathBuf>,
    canonical_walk_root: &Path,
    parallel: bool,
    files: &mut Vec<PathBuf>,
    seen_canonical: &mut HashSet<PathBuf>,
) {
    for canonical_path in
        canonicalize_source_candidates_with_mode(candidates, canonical_walk_root, parallel)
            .into_iter()
            .flatten()
    {
        if seen_canonical.insert(canonical_path.clone()) {
            files.push(canonical_path);
        }
    }
}

fn push_source_candidate(
    candidates: &mut Vec<PathBuf>,
    path: PathBuf,
    canonical_walk_root: &Path,
    files: &mut Vec<PathBuf>,
    seen_canonical: &mut HashSet<PathBuf>,
) {
    candidates.push(path);
    if candidates.len() == SOURCE_CANONICALIZE_BATCH_SIZE {
        let full_batch = std::mem::replace(
            candidates,
            Vec::with_capacity(SOURCE_CANONICALIZE_BATCH_SIZE),
        );
        append_canonicalized_source_candidates(
            full_batch,
            canonical_walk_root,
            true,
            files,
            seen_canonical,
        );
    }
}

fn finish_source_candidates(
    candidates: Vec<PathBuf>,
    canonical_walk_root: &Path,
    files: &mut Vec<PathBuf>,
    seen_canonical: &mut HashSet<PathBuf>,
) {
    // A partial tail is cheaper to resolve serially and never exceeds the
    // bounded batch retained during the directory walk.
    append_canonicalized_source_candidates(
        candidates,
        canonical_walk_root,
        false,
        files,
        seen_canonical,
    );
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
pub(crate) fn walk_source_files(root: &Path, config: &IndexConfig) -> Result<Vec<PathBuf>> {
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
        let mut candidates = Vec::with_capacity(SOURCE_CANONICALIZE_BATCH_SIZE);
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
            if !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
            {
                continue;
            }
            // Apply cheap path/language filters before canonicalization. File
            // size is enforced from the opened handle by `read_source_bounded`;
            // avoid a duplicate serial metadata syscall for every candidate.
            if !config.is_indexable_path(path) {
                continue;
            }
            push_source_candidate(
                &mut candidates,
                path.to_path_buf(),
                &canonical_walk_root,
                &mut files,
                &mut seen_canonical,
            );
        }

        // Canonicalization is independent per candidate and dominates a
        // metadata-only scan on very large repositories. Full bounded batches
        // were resolved and consumed above; keep the final partial batch serial.
        // Sequential consumption preserves global walk and root precedence.
        finish_source_candidates(
            candidates,
            &canonical_walk_root,
            &mut files,
            &mut seen_canonical,
        );
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

/// Walk one newly-created directory without also rescanning configured extra roots.
/// This keeps native watcher event draining O(events) while reusing the exact same
/// ignore, language, symlink, and file-size policy as a full build.
pub(super) fn walk_source_directory(root: &Path, config: &IndexConfig) -> Result<Vec<PathBuf>> {
    let mut scoped = config.clone();
    scoped.extra_roots.clear();
    walk_source_files(root, &scoped)
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
    if let Some((next_start, _)) = func_defs.get(pos)
        && line >= *next_start
    {
        return None;
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
                let source = read_source_bounded(abs_path, config.max_file_bytes)
                    .map_err(|e| {
                        warn!(path = %abs_path.display(), error = %e, "skipping in graph build");
                    })
                    .ok()??
                    .bytes;
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

#[cfg(test)]
mod init_hash_tests {
    use super::{
        LAST_CANONICALIZE_POOL_SIZE, PARALLEL_CANONICALIZE_BATCHES, SERIAL_CANONICALIZE_BATCHES,
        SOURCE_CANONICALIZE_BATCH_SIZE, canonicalize_source_candidates_with_mode,
        finish_source_candidates, push_source_candidate, read_source_bounded,
        stable_file_hash_entry, walk_source_files,
    };
    use crate::config::IndexConfig;
    use std::collections::HashSet;
    use std::fs;
    use std::time::{Duration, SystemTime};
    use tempfile::tempdir;

    fn reset_canonicalize_observations() {
        PARALLEL_CANONICALIZE_BATCHES.with(|count| count.set(0));
        SERIAL_CANONICALIZE_BATCHES.with(|count| count.set(0));
        LAST_CANONICALIZE_POOL_SIZE.with(|size| size.set(0));
    }

    fn canonicalize_observations() -> (usize, usize, usize) {
        (
            PARALLEL_CANONICALIZE_BATCHES.with(std::cell::Cell::get),
            SERIAL_CANONICALIZE_BATCHES.with(std::cell::Cell::get),
            LAST_CANONICALIZE_POOL_SIZE.with(std::cell::Cell::get),
        )
    }

    fn canonicalize_in_bounded_batches(
        candidates: impl IntoIterator<Item = std::path::PathBuf>,
        canonical_root: &std::path::Path,
    ) -> Vec<std::path::PathBuf> {
        let mut batch = Vec::with_capacity(SOURCE_CANONICALIZE_BATCH_SIZE);
        let mut files = Vec::new();
        let mut seen = HashSet::new();
        for path in candidates {
            push_source_candidate(&mut batch, path, canonical_root, &mut files, &mut seen);
        }
        finish_source_candidates(batch, canonical_root, &mut files, &mut seen);
        files
    }

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

    #[test]
    fn source_walk_defers_size_limit_to_bounded_reader() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        let oversized = root.join("oversized.rs");
        fs::write(&oversized, vec![b'x'; 256]).unwrap();

        let mut config = IndexConfig::new(&root);
        config.max_file_bytes = 128;
        let files = walk_source_files(&root, &config).unwrap();

        assert_eq!(files, vec![oversized.canonicalize().unwrap()]);
        assert!(
            read_source_bounded(&files[0], config.max_file_bytes)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn parallel_canonicalization_matches_serial_order_and_rejections() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("repo");
        let outside = dir.path().join("outside.rs");
        fs::create_dir_all(&root).unwrap();
        let first = root.join("first.rs");
        let second = root.join("second.rs");
        fs::write(&first, "fn first() {}\n").unwrap();
        fs::write(&second, "fn second() {}\n").unwrap();
        fs::write(&outside, "fn outside() {}\n").unwrap();
        let missing = root.join("missing.rs");
        let canonical_root = root.canonicalize().unwrap();
        let candidates = vec![
            second.clone(),
            first.clone(),
            second.clone(),
            missing,
            outside,
        ];

        let serial =
            canonicalize_source_candidates_with_mode(candidates.clone(), &canonical_root, false);
        let parallel = canonicalize_source_candidates_with_mode(candidates, &canonical_root, true);

        assert_eq!(parallel, serial);
        assert_eq!(
            serial,
            vec![
                Some(second.canonicalize().unwrap()),
                Some(first.canonicalize().unwrap()),
                Some(second.canonicalize().unwrap()),
                None,
                None,
            ],
            "parallel resolution must preserve candidate and duplicate order while rejecting missing and outside-root paths"
        );
    }

    #[test]
    fn canonicalization_batch_boundaries_select_expected_modes() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.rs");
        fs::write(&source, "fn source() {}\n").unwrap();
        let canonical_root = root.canonicalize().unwrap();
        let canonical_source = source.canonicalize().unwrap();

        for (count, expected_parallel, expected_serial) in
            [(4095, 0, 1), (4096, 1, 0), (4097, 1, 1)]
        {
            reset_canonicalize_observations();
            let files = canonicalize_in_bounded_batches(
                std::iter::repeat_n(source.clone(), count),
                &canonical_root,
            );
            assert_eq!(files, vec![canonical_source.clone()]);
            let (parallel, serial, _) = canonicalize_observations();
            assert_eq!(
                (parallel, serial),
                (expected_parallel, expected_serial),
                "unexpected batch modes for {count} candidates"
            );
        }
    }

    #[test]
    fn multiple_batches_preserve_order_and_reject_invalid_candidates() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("repo");
        let outside = dir.path().join("outside.rs");
        fs::create_dir_all(&root).unwrap();
        let first = root.join("first.rs");
        let second = root.join("second.rs");
        let third = root.join("third.rs");
        for (path, body) in [
            (&first, "fn first() {}\n"),
            (&second, "fn second() {}\n"),
            (&third, "fn third() {}\n"),
            (&outside, "fn outside() {}\n"),
        ] {
            fs::write(path, body).unwrap();
        }
        let canonical_root = root.canonicalize().unwrap();
        let mut candidates = Vec::with_capacity(SOURCE_CANONICALIZE_BATCH_SIZE + 4);
        candidates.push(first.clone());
        candidates.extend(std::iter::repeat_n(
            first.clone(),
            SOURCE_CANONICALIZE_BATCH_SIZE - 2,
        ));
        candidates.push(second.clone());
        candidates.extend([
            root.join("missing.rs"),
            outside,
            first.clone(),
            third.clone(),
        ]);

        reset_canonicalize_observations();
        let files = canonicalize_in_bounded_batches(candidates, &canonical_root);

        assert_eq!(
            files,
            vec![
                first.canonicalize().unwrap(),
                second.canonicalize().unwrap(),
                third.canonicalize().unwrap(),
            ]
        );
        assert_eq!(canonicalize_observations().0, 1);
        assert_eq!(canonicalize_observations().1, 1);
    }

    #[test]
    fn parallel_batch_uses_installed_two_worker_pool() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.rs");
        fs::write(&source, "fn source() {}\n").unwrap();
        let canonical_root = root.canonicalize().unwrap();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap();

        pool.install(|| {
            reset_canonicalize_observations();
            let files = canonicalize_in_bounded_batches(
                std::iter::repeat_n(source.clone(), SOURCE_CANONICALIZE_BATCH_SIZE),
                &canonical_root,
            );
            assert_eq!(files, vec![source.canonicalize().unwrap()]);
            assert_eq!(canonicalize_observations(), (1, 0, 2));
        });
    }

    #[test]
    fn source_walk_honors_filters_and_distinct_extra_root() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("repo");
        let extra = dir.path().join("shared");
        fs::create_dir_all(root.join("blocked")).unwrap();
        fs::create_dir_all(&extra).unwrap();
        let included = root.join("included.rs");
        let ignored = root.join("ignored.rs");
        let hidden = root.join(".hidden.rs");
        let excluded = root.join("blocked/excluded.rs");
        let unsupported = root.join("notes.unsupported");
        let extra_file = extra.join("extra.rs");
        fs::write(root.join(".ignore"), "ignored.rs\n").unwrap();
        fs::write(&included, "fn included() {}\n").unwrap();
        fs::write(&ignored, "fn ignored() {}\n").unwrap();
        fs::write(&hidden, "fn hidden() {}\n").unwrap();
        fs::write(&excluded, "fn excluded() {}\n").unwrap();
        fs::write(&unsupported, "not source\n").unwrap();
        fs::write(&extra_file, "fn extra() {}\n").unwrap();

        let mut config = IndexConfig::new(&root);
        config.exclude_patterns.push("blocked".to_string());
        config.extra_roots.push(extra);
        let files = walk_source_files(&root, &config).unwrap();

        assert_eq!(
            files,
            vec![
                included.canonicalize().unwrap(),
                extra_file.canonicalize().unwrap(),
            ]
        );
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

        assert_eq!(files, vec![src.join("inside.rs").canonicalize().unwrap()]);
    }
}

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::chunker::Chunker;
use crate::chunker::cast::CastChunker;
use crate::embedder::Embedder;
use crate::error::{CodixingError, Result};
use crate::graph::extract::{extract_definitions_from_tree, extract_references_from_tree};
use crate::graph::extractor::RawImport;
use crate::graph::types::ReferenceKind;
use crate::graph::{
    BorrowedImportResolver, CallExtractor, FileGraphSemanticState, FileSymbolDefinition,
    ImportExtractor, compute_pagerank,
};
use crate::language::{Language, detect_language};
use crate::persistence::{FileHashEntry, IndexMeta, IndexStore};
use crate::retriever::ChunkMeta;
use crate::symbols::persistence::{encode_symbol_delta_checkpoint, serialize_symbol_delta};
use crate::symbols::writer::write_mmap_symbol_table;
use crate::vector::VectorIndex;

use super::indexing::{
    PendingSymbolGraph, embedding_reuse_key, extract_pending_symbol_graph, make_embed_text,
    normalize_path, read_source_bounded, serialize_chunk_meta_compact, stable_file_hash_entry,
    symbol_from_entity, unix_timestamp_string,
};
use super::{Engine, GitSyncStats, SyncStats, git_diff_since, git_head_commit};

/// Persist long-running post-hoc embedding work often enough that interruption
/// repeats at most a bounded amount of model time. Vector snapshots rewrite the
/// complete HNSW graph, so a tiny chunk-count interval would create quadratic
/// write amplification on million-chunk repositories.
const EMBED_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Options that modify how [`Engine::sync_with_options`] runs.
#[derive(Debug, Clone, Copy, Default)]
pub struct SyncOptions {
    /// Skip the vector-embedding step during sync.
    ///
    /// When true, the engine's embedder is temporarily stashed for the
    /// duration of the sync so that [`Engine::reindex_file_impl`] cannot
    /// reach the embedding path. BM25, symbols, trigrams, file hashes,
    /// and the dependency graph are all updated as usual; only the
    /// vector index stays stale.
    ///
    /// Use this to avoid runaway CPU on sync. See the Linux kernel
    /// benchmark finding (68 min before kill) for the canonical bad
    /// scenario: existing 2GB embedded index + large change set + no
    /// escape hatch.
    pub skip_embed: bool,

    /// Force a full graph rebuild after the incremental sync completes.
    ///
    /// When true, [`Engine::rebuild_graph_from_disk`] is called after the normal
    /// incremental sync. This re-parses all indexed files to re-extract import
    /// and call edges, clears the existing graph edges, recomputes PageRank, and
    /// persists the updated graph.
    ///
    /// This is faster than a full `codixing init` because it reuses the existing
    /// BM25 / symbol / vector indexes — only the graph is rebuilt. Use this when
    /// the call graph is stale (e.g. after a large refactor) without wanting to
    /// pay the cost of re-chunking and re-embedding.
    pub rebuild_graph: bool,
}

#[derive(Default)]
pub(super) struct ApplyChangesOutcome {
    cosmetic_skipped: usize,
    graph_semantics_changed: bool,
    vector_changed: bool,
    successful_paths: std::collections::HashSet<std::path::PathBuf>,
    successful_hashes: std::collections::HashMap<std::path::PathBuf, Option<FileHashEntry>>,
    successful_signatures: std::collections::HashMap<std::path::PathBuf, Option<u64>>,
    retry_changes: std::collections::HashMap<std::path::PathBuf, crate::watcher::ChangeKind>,
    failures: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileDerivedSemanticState {
    graph: Option<FileGraphSemanticState>,
    concept_symbols: Vec<(String, Option<String>)>,
}

struct ReindexMutation {
    cosmetic_reused: bool,
    resolution_names_changed: Vec<String>,
    vector_changed: bool,
    hash_entry: Option<FileHashEntry>,
    signature: Option<u64>,
    import_update: Option<ImportGraphUpdate>,
    pre_removal_callers: Vec<String>,
}

struct ImportGraphUpdate {
    file_path: String,
    language: Language,
    raw_imports: Vec<RawImport>,
    call_names: Vec<String>,
    definitions: Vec<FileSymbolDefinition>,
    call_references: Vec<CallReferenceUpdate>,
    resolution_names_changed: Vec<String>,
}

struct CallReferenceUpdate {
    target_name: String,
    line: usize,
}

#[cfg(test)]
thread_local! {
    static IMPORT_RESOLVER_BUILD_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static CASCADE_REINDEX_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static EXACT_SIGNATURE_MATCH_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FAIL_REINDEX_AFTER_REMOVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Bound the incremental freshness overlay. Folding at this size amortizes a
/// repository-wide rewrite over thousands of edits while preventing a long-lived
/// daemon or large checkout from growing a second full path table.
const HASH_DELTA_COMPACT_THRESHOLD: usize = 4_096;

impl ApplyChangesOutcome {
    fn merge(&mut self, mut other: Self) {
        // A path may be edited repeatedly inside one deferred checkpoint. The
        // most recent outcome is authoritative: a later failure must revoke an
        // earlier success, and a later success must clear the retry marker.
        let failed_paths: Vec<_> = other.retry_changes.keys().cloned().collect();
        for path in &failed_paths {
            self.successful_paths.remove(path);
            self.successful_hashes.remove(path);
            self.successful_signatures.remove(path);
        }
        for path in &other.successful_paths {
            self.retry_changes.remove(path);
        }
        self.cosmetic_skipped += other.cosmetic_skipped;
        self.graph_semantics_changed |= other.graph_semantics_changed;
        self.vector_changed |= other.vector_changed;
        self.successful_paths.extend(other.successful_paths.drain());
        self.successful_hashes
            .extend(other.successful_hashes.drain());
        self.successful_signatures
            .extend(other.successful_signatures.drain());
        self.retry_changes.extend(other.retry_changes.drain());
        self.failures.append(&mut other.failures);
    }

    fn failure_error(&self) -> Option<CodixingError> {
        (!self.failures.is_empty()).then(|| {
            CodixingError::Index(format!(
                "{} file update(s) failed and remain pending for the next sync:\n{}",
                self.failures.len(),
                self.failures.join("\n")
            ))
        })
    }
}

/// Build the authoritative hash snapshot after a best-effort batch.
///
/// Successful updates adopt the freshly observed hash, successful deletions
/// remove the old entry, and failures retain the prior baseline while the
/// separate dirty-path journal forces a retry. Unchanged files still refresh
/// mtime/size metadata.
fn merge_hashes_after_apply(
    old: &std::collections::HashMap<std::path::PathBuf, FileHashEntry>,
    current: Vec<(std::path::PathBuf, FileHashEntry)>,
    changes: &[crate::watcher::FileChange],
    successful: &std::collections::HashMap<std::path::PathBuf, Option<FileHashEntry>>,
) -> Vec<(std::path::PathBuf, FileHashEntry)> {
    let failed: std::collections::HashSet<std::path::PathBuf> = changes
        .iter()
        .filter(|change| !successful.contains_key(&change.path))
        .map(|change| change.path.clone())
        .collect();
    let mut seen_success = std::collections::HashSet::with_capacity(successful.len());
    let mut seen_failed = std::collections::HashSet::with_capacity(failed.len());
    let mut merged = Vec::with_capacity(current.len().saturating_add(failed.len()));

    for (path, scanned) in current {
        if let Some(exact) = successful.get(&path) {
            seen_success.insert(path.clone());
            if let Some(exact) = exact {
                // Preserve scan metadata only when it describes the exact bytes
                // reindexed. A concurrent edit otherwise publishes unknown
                // metadata and forces a safe verification on the next sync.
                if scanned.content_hash == exact.content_hash {
                    merged.push((path, scanned));
                } else {
                    merged.push((path, exact.clone()));
                }
            }
        } else if failed.contains(&path) {
            seen_failed.insert(path.clone());
            if let Some(previous) = old.get(&path) {
                merged.push((path, previous.clone()));
            }
        } else {
            merged.push((path, scanned));
        }
    }

    // Cascades or files created after the scan may have exact indexed hashes
    // without a corresponding scan entry. Publish those with unknown metadata.
    for (path, exact) in successful {
        if !seen_success.contains(path)
            && let Some(exact) = exact
        {
            merged.push((path.clone(), exact.clone()));
        }
    }

    // A failed removal has no current scan entry; retain its previous baseline
    // while the dirty journal forces a retry.
    for path in failed {
        if !seen_failed.contains(&path)
            && let Some(previous) = old.get(&path)
        {
            merged.push((path, previous.clone()));
        }
    }
    merged
}

/// Compute a stable identity key for a chunk from its scope chain and entity
/// names, used to map a chunk to its previous embedding vector across a COSMETIC
/// edit (one that changes bodies/comments but not signatures).
///
/// Returns `None` for anonymous chunks that carry neither a scope nor any entity
/// name — those have no stable identity, so the caller falls back to re-embedding
/// them rather than risk reusing the wrong vector.
fn chunk_stable_key(scope_chain: &[String], entity_names: &[String]) -> Option<u64> {
    if scope_chain.is_empty() && entity_names.is_empty() {
        return None;
    }
    // Sort entity names so a pure reordering within a chunk is treated as stable.
    let mut names: Vec<&str> = entity_names.iter().map(String::as_str).collect();
    names.sort_unstable();
    let key_text = format!("{}\u{1f}{}", scope_chain.join("/"), names.join(","));
    Some(xxhash_rust::xxh3::xxh3_64(key_text.as_bytes()))
}

/// Build the complete signature-sidecar contents for the files currently on
/// disk (`seen_rel`, normalized relative paths): start from the previous sync's
/// fingerprints, drop entries for files no longer present, and overlay the
/// freshly-computed fingerprints for files that changed this sync. Files with no
/// fingerprint (no AST entities) carry no entry — their absence makes the next
/// sync treat them as STRUCTURAL.
///
/// All maps are keyed by normalized relative path (root-invariant).
fn merge_signatures(
    old: &std::collections::HashMap<std::path::PathBuf, u64>,
    new: &std::collections::HashMap<std::path::PathBuf, u64>,
    seen_rel: &std::collections::HashSet<std::path::PathBuf>,
    modified_rel: &std::collections::HashSet<std::path::PathBuf>,
) -> Vec<(std::path::PathBuf, u64)> {
    // Keep an old fingerprint only for files still present (`seen_rel`) AND not
    // modified this sync. A modified file's old fingerprint is dropped here and
    // re-added below only if a fresh one was computed — otherwise the file lost
    // its fingerprint (e.g. it stopped producing AST entities) and must NOT keep
    // a stale baseline that a later edit could be misclassified COSMETIC against.
    let mut merged: std::collections::HashMap<std::path::PathBuf, u64> = old
        .iter()
        .filter(|(p, _)| seen_rel.contains(*p) && !modified_rel.contains(*p))
        .map(|(p, &h)| (p.clone(), h))
        .collect();
    for (p, &h) in new {
        if seen_rel.contains(p) {
            merged.insert(p.clone(), h);
        }
    }
    merged.into_iter().collect()
}

impl Engine {
    /// Disable the two auxiliary semantic caches before any searchable mutation.
    /// The primary indexes remain available; a failed mutation/rebuild can only
    /// reduce expansion quality, never expose mappings from the previous tree.
    fn invalidate_semantic_artifacts(&mut self) -> Result<()> {
        self.concept_index = std::sync::OnceLock::new();
        self.reformulations = std::sync::OnceLock::new();
        super::semantic_artifacts::invalidate_semantic_artifacts(&self.store)
    }

    fn rebuild_semantic_artifacts(&mut self) -> Result<()> {
        // Reset even if a caller loaded one of the artifacts after invalidation.
        self.concept_index = std::sync::OnceLock::new();
        self.reformulations = std::sync::OnceLock::new();
        super::semantic_artifacts::rebuild_semantic_artifacts(
            &self.store,
            &self.symbols,
            self.graph.as_ref(),
        )
    }

    fn resolve_mutation_path(
        &self,
        path: &Path,
        allow_missing: bool,
    ) -> Result<std::path::PathBuf> {
        let resolved = if path.is_absolute() {
            self.config.resolve_absolute_path(path, allow_missing)
        } else {
            let path = path.to_str().ok_or_else(|| {
                CodixingError::Config("mutation path is not valid UTF-8".to_string())
            })?;
            if allow_missing {
                self.config.resolve_path_for_write(path)
            } else {
                self.config.resolve_path(path)
            }
        };
        resolved.ok_or_else(|| {
            CodixingError::Config(format!(
                "path is missing or outside the configured project roots: {}",
                path.display()
            ))
        })
    }

    /// Persist a complete, authoritative file-hash snapshot.
    ///
    /// Current generations write only the metadata-aware v2 format. Readers
    /// still migrate legacy v1-only indexes, but recreating v1 on every sync
    /// doubled path storage and write amplification without serving a reader.
    fn persist_hash_snapshot(&self, hashes: &[(std::path::PathBuf, FileHashEntry)]) -> Result<()> {
        let mut hashes = hashes.to_vec();
        hashes.sort_by(|a, b| a.0.cmp(&b.0));
        self.store.save_tree_hashes_v2(&hashes)?;
        match fs::remove_file(self.store.tree_hashes_path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Fold the incremental overlay into a complete hash snapshot without a
    /// replay race. Existing overlay keys are first rewritten to the exact
    /// values in `hashes`; replay is then idempotent if the process stops after
    /// the baseline rename but before the final overlay clear.
    fn fold_hash_snapshot(&self, hashes: &[(std::path::PathBuf, FileHashEntry)]) -> Result<()> {
        let overlay = self.store.load_tree_hash_delta()?;
        if !overlay.is_empty() {
            // Sort borrowed pointers rather than cloning every repository path
            // into a second full HashMap merely to resolve the tiny overlay.
            // Peak scratch space is one pointer per file and is released before
            // serializing either baseline format.
            let mut authoritative: Vec<_> = hashes.iter().collect();
            authoritative.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            let aligned: Vec<_> = overlay
                .into_iter()
                .map(|(path, _)| {
                    let entry = authoritative
                        .binary_search_by(|candidate| candidate.0.cmp(&path))
                        .ok()
                        .map(|index| authoritative[index].1.clone());
                    (path, entry)
                })
                .collect();
            self.store.replace_tree_hash_delta(&aligned)?;
        }
        self.persist_hash_snapshot(hashes)?;
        self.store.clear_tree_hash_delta()
    }

    fn compact_hash_delta_if_needed(&self) -> Result<()> {
        if self.store.load_tree_hash_delta()?.len() <= HASH_DELTA_COMPACT_THRESHOLD {
            return Ok(());
        }
        let effective = self.store.load_tree_hashes_v2()?;
        self.fold_hash_snapshot(&effective)
    }

    /// Restore the git publication marker when checkpoint persistence fails.
    /// The unpublished generation remains retriable while the active snapshot
    /// and its commit marker stay authoritative.
    fn restore_git_commit(&self, commit: Option<&str>) -> Result<()> {
        let mut meta = self.store.load_meta()?;
        meta.git_commit = commit.map(str::to_string);
        self.store.save_meta(&meta)
    }

    /// Re-index a single file (after modification).
    ///
    /// Removes old data, re-parses, re-chunks, and re-indexes.
    /// When called directly, also recomputes PageRank and persists the graph.
    /// Use `apply_changes` to batch multiple files with a single PageRank pass.
    pub fn reindex_file(&mut self, path: &Path) -> Result<()> {
        if self.read_only {
            return Err(CodixingError::ReadOnly);
        }
        let abs_path = self.resolve_mutation_path(path, false)?;
        self.apply_changes(&[crate::watcher::FileChange {
            path: abs_path,
            kind: crate::watcher::ChangeKind::Modified,
        }])?;
        Ok(())
    }

    /// Re-index a single file.
    ///
    /// `expected_cosmetic_signature` is present when the classifier observed a
    /// COSMETIC edit. It is revalidated against the exact indexed bytes before
    /// vector reuse. For a COSMETIC file the embedding vectors
    /// are reused via a stable per-chunk identity key (scope + entity names) even
    /// when chunk *content* changed, since the structure is unchanged. This is
    /// the broadened reuse that avoids the expensive dense-embedding round-trip
    /// on body/comment/whitespace edits.
    ///
    /// Returns the observed whole-file hash alongside whether a COSMETIC file
    /// reused every vector. The hash comes from the same bytes that were parsed,
    /// avoiding a second source-file read on watcher updates.
    fn reindex_file_impl(
        &mut self,
        path: &Path,
        expected_cosmetic_signature: Option<u64>,
    ) -> Result<ReindexMutation> {
        // Wait for any background embedding to complete before modifying the vector index.
        self.wait_for_embeddings();

        let abs_path = self.resolve_mutation_path(path, false)?;
        let rel_str = self.config.normalize_path(&abs_path).ok_or_else(|| {
            CodixingError::Config(format!(
                "cannot normalize contained path: {}",
                abs_path.display()
            ))
        })?;
        let was_indexed = self.file_chunk_ids.contains_key(&rel_str);
        let (vector_file_present, vector_reindex_enabled) = {
            let vector = self
                .vector
                .read()
                .unwrap_or_else(|error| error.into_inner());
            (
                vector
                    .as_ref()
                    .is_some_and(|index| index.file_chunks().contains_key(&rel_str)),
                self.embedder.is_some() && vector.is_some(),
            )
        };

        // Read and parse before inspecting or mutating live indexes. Besides
        // keeping failures atomic, this lets us revalidate a COSMETIC decision
        // against the exact bytes that will be indexed. The file may have
        // changed after the classifier's earlier read.
        let Some(read) = read_source_bounded(&abs_path, self.config.max_file_bytes)? else {
            info!(path = %abs_path.display(), limit = self.config.max_file_bytes, "file exceeds max_file_bytes; removing it from the index");
            let resolution_names_changed = self
                .graph
                .as_ref()
                .map(|graph| graph.file_symbol_resolution_names(&rel_str))
                .unwrap_or_default();
            let pre_removal_callers = self
                .graph
                .as_ref()
                .map(|graph| graph.callers(&rel_str))
                .unwrap_or_default();
            self.remove_file_inner(&abs_path)?;
            return Ok(ReindexMutation {
                cosmetic_reused: false,
                resolution_names_changed,
                vector_changed: vector_file_present,
                hash_entry: None,
                signature: None,
                import_update: None,
                pre_removal_callers,
            });
        };
        let source = read.bytes;
        let file_hash = xxhash_rust::xxh3::xxh3_64(&source);
        let result = self.parser.parse_file(&abs_path, &source)?;
        let signature =
            super::fingerprint::signature_fingerprint(&result.entities, &source, result.language);
        let cosmetic =
            expected_cosmetic_signature.is_some() && signature == expected_cosmetic_signature;
        let contextual = self.config.embedding.contextual_embeddings;
        #[cfg(test)]
        if cosmetic {
            EXACT_SIGNATURE_MATCH_COUNT.with(|count| count.set(count.get() + 1));
        }

        // Jupyter notebooks need per-cell dispatch — the incremental sync path
        // does not implement that yet. Leave old chunks in place so this path
        // does not partially de-index a notebook it cannot re-add.
        if result.language.is_notebook() {
            return Err(CodixingError::Config(format!(
                "notebook incremental sync is not supported for {rel_str}; run `codixing init` to reindex"
            )));
        }

        // ── Collect old chunk content hashes before removing data ────────
        // Used for incremental vector updates: chunks whose content hash is
        // unchanged can reuse their existing embedding vector.
        //
        // For a COSMETIC change we additionally build a *stable-identity* map
        // keyed on (scope chain + sorted entity names). A body/comment edit
        // changes the chunk content hash but NOT this key, so it lets us reuse
        // the vector even though the embed text shifted. The key is only trusted
        // when it is unique within the file — duplicate keys are dropped so an
        // ambiguous match never reuses the wrong vector (conservative).
        let mut old_embedding_keys: std::collections::HashMap<u64, (u64, Vec<f32>)> =
            std::collections::HashMap::new();
        let mut old_stable_keys: std::collections::HashMap<u64, Vec<f32>> =
            std::collections::HashMap::new();
        let mut stable_key_dupes: std::collections::HashSet<u64> = std::collections::HashSet::new();
        {
            let vec_guard = self.vector.read().unwrap_or_else(|e| e.into_inner());
            if vec_guard.is_some()
                && let Some(chunk_ids) = self.file_chunk_ids.get(&rel_str)
            {
                for chunk_id in chunk_ids.iter() {
                    if let Some(meta) = self.chunk_meta.get(chunk_id)
                        && meta.content_hash != 0
                    {
                        // Try to retrieve the existing vector for this chunk.
                        let existing_vec =
                            vec_guard.as_ref().and_then(|v| v.get_vector(meta.chunk_id));
                        if let Some(vec) = existing_vec {
                            old_embedding_keys.insert(
                                embedding_reuse_key(meta.value(), contextual),
                                (meta.chunk_id, vec.clone()),
                            );
                            if cosmetic
                                && let Some(key) =
                                    chunk_stable_key(&meta.scope_chain, &meta.entity_names)
                                && old_stable_keys.insert(key, vec).is_some()
                            {
                                // Two old chunks collide on this key — ambiguous.
                                stable_key_dupes.insert(key);
                            }
                        }
                    }
                }
            }
            drop(vec_guard);
        }
        // Drop ambiguous keys so they are never used for reuse.
        for k in &stable_key_dupes {
            old_stable_keys.remove(k);
        }

        // Remove old data.
        self.tantivy.remove_file(&rel_str)?;
        self.symbols.remove_file(&rel_str);
        // Only remove this file's vectors when we will actually re-embed (the
        // re-add/reuse path below is gated on `self.embedder.is_some()`). Under
        // `skip_embed` (sync --no-embed) the embedder is stashed but
        // `self.vector` stays `Some`; removing here without re-adding would leave
        // the changed file with ZERO vectors — the opposite of the documented
        // `SyncOptions::skip_embed` contract ("the vector index stays stale").
        // With this guard the existing vectors are left in place (genuinely
        // stale) when embedding is skipped.
        if self.embedder.is_some() {
            let mut vec_guard = self.vector.write().unwrap_or_else(|e| e.into_inner());
            if let Some(ref mut vec_idx) = *vec_guard {
                vec_idx.remove_file(&rel_str)?;
            }
        }
        if let Some(old_chunk_ids) = self.file_chunk_ids.remove(&rel_str) {
            for chunk_id in old_chunk_ids.iter() {
                self.chunk_meta.remove(chunk_id);
            }
        }

        #[cfg(test)]
        if FAIL_REINDEX_AFTER_REMOVE.with(|armed| armed.replace(false)) {
            return Err(CodixingError::Index(format!(
                "injected post-removal reindex failure for {rel_str}"
            )));
        }

        let chunker = CastChunker;
        let chunks = chunker.chunk(
            &rel_str,
            &source,
            result.tree.as_ref(),
            result.language,
            &self.config.chunk,
        );

        for chunk in &chunks {
            if let Err(error) = self.tantivy.add_chunk(chunk) {
                // The per-file posting is deliberately installed only after
                // every add succeeds. Remove metadata from earlier chunks so a
                // failed addition cannot become an unowned stale sidecar entry.
                for pending in &chunks {
                    self.chunk_meta.remove(&pending.id);
                }
                return Err(error);
            }

            // Tantivy is the authoritative body store. Keep a transient copy
            // only when the immediate vector pass may need contextual embed
            // text, then release it once that pass completes below.
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
                    entity_names: chunk.entity_names.clone(),
                    content_hash: xxhash_rust::xxh3::xxh3_64(chunk.content.as_bytes()),
                    content: if vector_reindex_enabled && contextual {
                        chunk.content.clone()
                    } else {
                        String::new()
                    },
                },
            );
        }

        for entity in &result.entities {
            self.symbols
                .insert(symbol_from_entity(entity, &rel_str, result.language));
        }

        // ── Incremental vector update ────────────────────────────────────
        // Compare new chunk content hashes against old ones.  Chunks whose
        // content is identical reuse the previous embedding vector, avoiding
        // an expensive re-embedding round-trip through the ONNX model.
        //
        // For a COSMETIC file we additionally try a stable-identity key
        // (scope + entity names) so a body/comment edit reuses the vector even
        // though the chunk content (and embed text) shifted. This is the
        // intended cost/accuracy tradeoff: when the file's signature fingerprint
        // is unchanged, reusing a vector whose embed text drifted is acceptable
        // because the retrieval-relevant structure is identical.
        let mut all_reused = false;
        let mut vec_guard = self.vector.write().unwrap_or_else(|e| e.into_inner());
        if let (Some(emb), Some(vec_idx)) = (self.embedder.as_ref(), vec_guard.as_mut()) {
            let mut reused = 0usize;
            let mut reused_via_stable_key = 0usize;
            let mut needs_embed: Vec<usize> = Vec::new();

            // Count stable keys among the NEW chunks. A cosmetic edit that pushes
            // a body across a chunk boundary can produce two new chunks sharing
            // one key; reusing the single old vector for both would duplicate a
            // stale vector. Reuse therefore requires a one-to-one match: the key
            // must be unique on the new side here AND on the old side (dupes were
            // already dropped from `old_stable_keys`).
            let mut new_key_counts: std::collections::HashMap<u64, usize> =
                std::collections::HashMap::new();
            for chunk in chunks.iter() {
                if let Some(k) = chunk_stable_key(&chunk.scope_chain, &chunk.entity_names) {
                    *new_key_counts.entry(k).or_insert(0) += 1;
                }
            }

            for (i, chunk) in chunks.iter().enumerate() {
                // Reconstruct the same key from compact old metadata and the
                // newly indexed metadata. Missing metadata conservatively falls
                // back to the body hash, which cannot match a contextual key.
                let new_key = self
                    .chunk_meta
                    .get(&chunk.id)
                    .map(|meta| embedding_reuse_key(meta.value(), contextual))
                    .unwrap_or_else(|| xxhash_rust::xxh3::xxh3_64(chunk.content.as_bytes()));
                if let Some((_old_id, old_vec)) = old_embedding_keys.get(&new_key) {
                    // Content unchanged — reuse the existing vector.
                    if let Err(e) = vec_idx.add_mut(chunk.id, old_vec, &rel_str) {
                        warn!(error = %e, chunk_id = chunk.id, "failed to reuse vector");
                    }
                    reused += 1;
                } else if cosmetic {
                    // Content changed but the file is COSMETIC — try the stable
                    // identity key. Reuse only on a one-to-one match: the key must
                    // be unique among the new chunks AND present (already unique)
                    // in the old map. Ambiguous keys on either side re-embed.
                    let stable = chunk_stable_key(&chunk.scope_chain, &chunk.entity_names)
                        .filter(|k| new_key_counts.get(k).copied() == Some(1))
                        .and_then(|k| old_stable_keys.get(&k));
                    if let Some(old_vec) = stable {
                        if let Err(e) = vec_idx.add_mut(chunk.id, old_vec, &rel_str) {
                            warn!(error = %e, chunk_id = chunk.id, "failed to reuse vector (cosmetic)");
                        }
                        reused += 1;
                        reused_via_stable_key += 1;
                    } else {
                        // No stable match (anonymous chunk, new chunk, or chunk
                        // count changed) — fall back to re-embedding it.
                        needs_embed.push(i);
                    }
                } else {
                    needs_embed.push(i);
                }
            }

            // A file counts as a cosmetic-skip when it was classified COSMETIC
            // and every new chunk reused an existing vector, regardless of
            // whether the match came from identical content or a stable key.
            all_reused = cosmetic && reused > 0 && needs_embed.is_empty() && !chunks.is_empty();

            if !needs_embed.is_empty() {
                let texts: Vec<String> = needs_embed
                    .iter()
                    .map(|&i| {
                        let c = &chunks[i];
                        if contextual && let Some(meta) = self.chunk_meta.get(&c.id) {
                            return make_embed_text(&meta, true);
                        }
                        c.content.clone()
                    })
                    .collect();
                match emb.embed(texts) {
                    Ok(embeddings) => {
                        for (&idx, embedding) in needs_embed.iter().zip(embeddings.iter()) {
                            let chunk = &chunks[idx];
                            if let Err(e) = vec_idx.add_mut(chunk.id, embedding, &rel_str) {
                                warn!(error = %e, chunk_id = chunk.id, "failed to add vector");
                            }
                        }
                    }
                    Err(e) => warn!(error = %e, "embedding failed during reindex"),
                }
            }

            if reused > 0 {
                debug!(
                    reused,
                    reused_via_stable_key,
                    re_embedded = needs_embed.len(),
                    cosmetic,
                    "incremental vector update"
                );
            }
        }
        drop(vec_guard); // Release write lock before graph/PageRank work

        // Vector reuse/embedding has consumed every body it needs. Replacing
        // (rather than clearing) releases each allocation even when embedding
        // failed; search and graph paths hydrate bodies from Tantivy on demand.
        if vector_reindex_enabled {
            for chunk in &chunks {
                if let Some(mut meta) = self.chunk_meta.get_mut(&chunk.id) {
                    meta.content = String::new();
                }
            }
        }

        self.file_chunk_ids.insert(
            rel_str.clone(),
            chunks
                .iter()
                .map(|chunk| chunk.id)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );

        // Replace the path atomically from the caller's perspective with the
        // union of exact source bytes (grep) and parser-produced chunk text
        // (exact search). Preparing outside the mutation avoids retaining old
        // postings while scanning the new representations.
        let trigrams = crate::index::trigram::FileTrigramIndex::prepare_contents(
            std::iter::once(source.as_slice()).chain(
                chunks
                    .iter()
                    .filter(|chunk| {
                        source.get(chunk.byte_start..chunk.byte_end)
                            != Some(chunk.content.as_bytes())
                    })
                    .map(|chunk| chunk.content.as_bytes()),
            ),
        );
        let file_trigram = self.file_trigram.get_mut().unwrap();
        file_trigram.remove_file(&rel_str);
        file_trigram.add_prepared(&rel_str, &trigrams);

        // Update graph call/symbol edges for this file using the already-parsed
        // tree. Import resolution is applied once per completed batch phase,
        // after the exact post-mutation indexed-file set is known.
        let file_language = result.language;
        // Config languages have no tree-sitter tree; skip import/call extraction.
        let raw_imports = match result.tree.as_ref() {
            Some(tree) => ImportExtractor::extract(tree, &source, file_language),
            None => Vec::new(),
        };
        let call_names = match result.tree.as_ref() {
            Some(tree) => CallExtractor::extract_calls(tree, &source, file_language),
            None => Vec::new(),
        };
        let (definitions, references) = match result.tree.as_ref() {
            Some(tree) => (
                extract_definitions_from_tree(tree, &source, &rel_str, &file_language)
                    .into_iter()
                    .map(|definition| FileSymbolDefinition {
                        name: definition.name,
                        kind: definition.kind,
                        line: definition.line,
                    })
                    .collect(),
                extract_references_from_tree(tree, &source, &rel_str, &file_language)
                    .into_iter()
                    .filter(|reference| reference.kind == ReferenceKind::Call)
                    .map(|reference| CallReferenceUpdate {
                        target_name: reference.target_name,
                        line: reference.line,
                    })
                    .collect(),
            ),
            None => (Vec::new(), Vec::new()),
        };
        let resolution_names_changed = if was_indexed {
            self.graph
                .as_ref()
                .map(|graph| graph.file_symbol_resolution_changed_names(&rel_str, &definitions))
                .unwrap_or_default()
        } else {
            // A newly indexed path normally has no old graph definitions. Keep
            // the conservative union for inconsistent/stale in-memory state so
            // recovery cannot hide a resolution transition.
            let mut names = self
                .graph
                .as_ref()
                .map(|graph| graph.file_symbol_resolution_names(&rel_str))
                .unwrap_or_default();
            names.extend(definitions.iter().map(|definition| definition.name.clone()));
            names.sort_unstable();
            names.dedup();
            names
        };
        if let Some(ref mut graph) = self.graph {
            // Incoming file edges identify callers if the resolution keys
            // changed. Only this file's outgoing edges are replaced here.
            graph.remove_file_edges(&rel_str);
        }

        debug!(path = %abs_path.display(), chunks = chunks.len(), "reindexed file");
        Ok(ReindexMutation {
            cosmetic_reused: all_reused,
            resolution_names_changed: resolution_names_changed.clone(),
            vector_changed: vector_reindex_enabled,
            // Unknown metadata deliberately makes the next full sync verify
            // content once. It cannot hide a write racing this indexed read.
            hash_entry: Some(FileHashEntry::new(file_hash, None, 0)),
            signature,
            import_update: self.graph.is_some().then_some(ImportGraphUpdate {
                file_path: rel_str,
                language: file_language,
                raw_imports,
                call_names,
                definitions,
                call_references: references,
                resolution_names_changed,
            }),
            pre_removal_callers: Vec::new(),
        })
    }

    /// Resolve all successful files in one completed mutation phase against
    /// the exact resulting file and symbol sets. Deferring calls until every
    /// file in the phase has replaced its symbols makes batch results
    /// independent of caller/callee input order. Repository-key collection is
    /// still bounded to the existing map and never cloned.
    fn apply_import_graph_updates(&mut self, updates: Vec<ImportGraphUpdate>) {
        if updates.is_empty() || self.graph.is_none() {
            return;
        }

        let indexed = &self.file_chunk_ids;
        let contains_indexed = |path: &str| indexed.contains_key(path);
        let visit_indexed = |visitor: &mut dyn FnMut(&str) -> bool| {
            for path in indexed.keys() {
                if !visitor(path) {
                    break;
                }
            }
        };
        let resolver = BorrowedImportResolver::with_lookup(
            &contains_indexed,
            &visit_indexed,
            self.config.root.clone(),
        );
        #[cfg(test)]
        IMPORT_RESOLVER_BUILD_COUNT.with(|count| count.set(count.get() + 1));

        let symbols = &self.symbols;
        let graph = self.graph.as_mut().expect("graph presence checked above");

        // Install every phase's final definition set before resolving any
        // reference. This makes caller/callee batches independent of input
        // order. Definitions with unchanged resolution-name multiplicity keep
        // stable node indices (and incoming cross-file references); only names
        // whose multiplicity changed are removed and rebuilt.
        for update in &updates {
            graph.get_or_insert_node(&update.file_path, update.language);
            graph.refresh_file_symbols(
                &update.file_path,
                &update.definitions,
                &update.resolution_names_changed,
            );
        }

        for update in updates {
            for raw in &update.raw_imports {
                if let Some(target) = resolver.resolve(raw, &update.file_path) {
                    let target_lang =
                        detect_language(std::path::Path::new(&target)).unwrap_or(update.language);
                    graph.add_edge(
                        &update.file_path,
                        &target,
                        &raw.path,
                        update.language,
                        target_lang,
                    );
                }
            }

            let function_nodes = graph.function_nodes_in_file(&update.file_path);
            for reference in &update.call_references {
                let Some(caller_idx) =
                    super::indexing::find_enclosing_function(&function_nodes, reference.line)
                else {
                    continue;
                };
                let callee_base = reference
                    .target_name
                    .rsplit("::")
                    .next()
                    .unwrap_or(&reference.target_name);

                let same_file: Vec<_> = graph
                    .symbol_nodes_named_in_file(&update.file_path, callee_base)
                    .into_iter()
                    .filter(|target| *target != caller_idx)
                    .collect();

                // SymbolTable already indexes names globally. Use it to find
                // candidate files, then the graph's per-file postings to find
                // node indices. This avoids a repository-wide symbol scan on
                // every editor call site.
                let mut cross_file = Vec::new();
                for symbol in symbols.lookup(callee_base) {
                    if symbol.file_path == update.file_path {
                        continue;
                    }
                    cross_file
                        .extend(graph.symbol_nodes_named_in_file(&symbol.file_path, callee_base));
                }
                cross_file.sort_unstable_by_key(|index| index.index());
                cross_file.dedup();

                let target = if same_file.len() == 1 {
                    same_file.first().copied()
                } else if cross_file.len() == 1 {
                    cross_file.first().copied()
                } else {
                    None
                };
                if let Some(target_idx) = target
                    && caller_idx != target_idx
                {
                    graph.add_reference(caller_idx, target_idx, ReferenceKind::Call);
                }
            }

            let mut seen_call_targets = std::collections::HashSet::new();
            for name in &update.call_names {
                let syms = symbols.lookup(name);
                let targets: std::collections::HashSet<&str> = syms
                    .iter()
                    .map(|symbol| symbol.file_path.as_str())
                    .filter(|file| *file != update.file_path.as_str())
                    .collect();
                if targets.len() != 1 {
                    continue;
                }
                let target = *targets.iter().next().expect("one call target checked");
                if seen_call_targets.insert(target.to_string()) {
                    let target_lang =
                        detect_language(std::path::Path::new(target)).unwrap_or(update.language);
                    graph.add_call_edge(
                        &update.file_path,
                        target,
                        name,
                        update.language,
                        target_lang,
                    );
                }
            }
        }
    }

    /// Re-resolve doc-to-code edges for virtual external documents whose
    /// referenced symbol names changed global uniqueness. Their source lives
    /// only in the index, so the normal filesystem reindex cascade cannot
    /// reopen it; compact chunk metadata retains the exact extracted names.
    fn refresh_external_doc_edges(
        &mut self,
        documents: &std::collections::BTreeSet<String>,
    ) -> bool {
        if documents.is_empty() || self.graph.is_none() {
            return false;
        }

        let mut desired_edges = Vec::with_capacity(documents.len());
        for document in documents {
            let mut names = std::collections::BTreeSet::new();
            if let Some(chunk_ids) = self.file_chunk_ids.get(document) {
                for chunk_id in chunk_ids.iter() {
                    if let Some(metadata) = self.chunk_meta.get(chunk_id) {
                        names.extend(metadata.entity_names.iter().cloned());
                    }
                }
            }

            let mut edges = Vec::new();
            for name in names {
                let targets: std::collections::HashSet<_> = self
                    .symbols
                    .lookup(&name)
                    .into_iter()
                    .map(|symbol| symbol.file_path)
                    .filter(|path| path != document)
                    .collect();
                if targets.len() != 1 {
                    continue;
                }
                let target = targets
                    .into_iter()
                    .next()
                    .expect("one doc symbol target checked");
                let language =
                    detect_language(std::path::Path::new(&target)).unwrap_or(Language::Markdown);
                edges.push((target, name, language));
            }
            desired_edges.push((document.clone(), edges));
        }

        let graph = self.graph.as_mut().expect("graph presence checked above");
        let mut changed = false;
        for (document, edges) in desired_edges {
            let before = graph.file_semantic_state(&document);
            graph.remove_file_edges(&document);
            for (target, name, target_language) in edges {
                graph.add_doc_edge(
                    &document,
                    &target,
                    &name,
                    Language::Markdown,
                    target_language,
                );
            }
            changed |= before != graph.file_semantic_state(&document);
        }
        changed
    }

    /// Inner removal: all index ops except `tantivy.commit()` and graph PageRank finalization.
    /// Called by both `remove_file()` (single-file public API) and `apply_changes()` (batch).
    pub(super) fn remove_file_inner(&mut self, path: &Path) -> Result<()> {
        let abs_path = self.resolve_mutation_path(path, true)?;
        let rel_str = self.config.normalize_path(&abs_path).ok_or_else(|| {
            CodixingError::Config(format!(
                "cannot normalize contained path: {}",
                abs_path.display()
            ))
        })?;
        self.tantivy.remove_file(&rel_str)?;
        self.symbols.remove_file(&rel_str);
        self.parser.invalidate(&abs_path);
        {
            let mut vec_guard = self.vector.write().unwrap_or_else(|e| e.into_inner());
            if let Some(ref mut vec_idx) = *vec_guard {
                vec_idx.remove_file(&rel_str)?;
            }
        }
        if let Some(chunk_ids) = self.file_chunk_ids.remove(&rel_str) {
            for chunk_id in chunk_ids.iter() {
                self.chunk_meta.remove(chunk_id);
            }
        }
        // Incremental file trigram removal.
        self.file_trigram.get_mut().unwrap().remove_file(&rel_str);

        // Remove graph node + incident edges (PageRank deferred to caller).
        if let Some(ref mut graph) = self.graph {
            graph.remove_file(&rel_str);
            graph.remove_file_symbols(&rel_str);
        }

        Ok(())
    }

    /// Remove a file from the index entirely.
    pub fn remove_file(&mut self, path: &Path) -> Result<()> {
        if self.read_only {
            return Err(CodixingError::ReadOnly);
        }
        let abs_path = self.resolve_mutation_path(path, true)?;
        self.apply_changes(&[crate::watcher::FileChange {
            path: abs_path,
            kind: crate::watcher::ChangeKind::Removed,
        }])?;
        Ok(())
    }

    /// Start watching the project directory for file changes.
    pub fn watch(&self) -> Result<crate::watcher::FileWatcher> {
        crate::watcher::FileWatcher::new(&self.config.root, &self.config)
    }

    /// Canonical graph and auxiliary-semantic inputs owned by one source file.
    ///
    /// This is deliberately narrower than the lexical/chunk state, which must
    /// always be persisted after a content edit. Equality proves that retaining
    /// the active generation's hard-linked graph, concept, and reformulation
    /// artifacts is equivalent to rebuilding them.
    fn file_derived_semantic_state(&self, path: &Path) -> Option<FileDerivedSemanticState> {
        let relative = self.config.normalize_path(path)?;
        let graph = self
            .graph
            .as_ref()
            .map(|graph| graph.file_semantic_state(&relative));
        let mut concept_symbols: Vec<_> = self
            .symbols
            .symbols_in_file(&relative, None)
            .into_iter()
            .map(|symbol| (symbol.name, symbol.doc_comment))
            .collect();
        concept_symbols.sort();
        Some(FileDerivedSemanticState {
            graph,
            concept_symbols,
        })
    }

    fn file_derived_semantics_changed(
        before: Option<FileDerivedSemanticState>,
        after: Option<FileDerivedSemanticState>,
    ) -> bool {
        match (before, after) {
            (Some(before), Some(after)) => before != after,
            // Path normalization failure is unexpected after change resolution;
            // conservatively rebuild instead of retaining possibly stale data.
            _ => true,
        }
    }

    /// Fork the active immutable snapshot before the first mutation in a
    /// batch. A working generation keeps the rebuild lock until publication;
    /// subsequent editor events reuse it without another tree fork.
    pub(super) fn ensure_working_generation(&mut self) -> Result<()> {
        if self.store.owns_rebuild_lock() {
            return Ok(());
        }
        if self.writer_lock.is_none() {
            return Err(CodixingError::ReadOnly);
        }

        // Background embedding may still be publishing vector sidecars into
        // the freshly initialized generation. Join it before taking the
        // copy-on-write snapshot so every inherited artifact is coherent.
        self.wait_for_embeddings();
        let working_store = self.store.begin_checkpoint()?;
        let working_tantivy = crate::index::TantivyIndex::open_in_dir_with_config(
            &working_store.tantivy_dir(),
            self.config.bm25.clone(),
        )?;

        let old_tantivy = std::mem::replace(&mut self.tantivy, working_tantivy);
        let old_store = std::mem::replace(&mut self.store, working_store);
        drop(old_tantivy);
        drop(old_store);
        Ok(())
    }

    /// Discard an unpublished checkpoint and restore every resident view of the
    /// active generation. The dirty-path journal lives in the control directory,
    /// so it deliberately survives this rollback and makes the whole batch
    /// retriable on the next sync.
    fn abort_working_generation(&mut self) -> Result<()> {
        self.pending_checkpoint = ApplyChangesOutcome::default();
        if !self.store.owns_rebuild_lock() {
            return Ok(());
        }

        self.wait_for_embeddings();
        let active_store = IndexStore::open(&self.config.root)?;
        let active_tantivy = crate::index::TantivyIndex::open_in_dir_with_config(
            &active_store.tantivy_dir(),
            self.config.bm25.clone(),
        )?;

        let old_tantivy = std::mem::replace(&mut self.tantivy, active_tantivy);
        let old_store = std::mem::replace(&mut self.store, active_store);

        // A working vector index may have been destructively updated in memory.
        // Force the active checkpoint to attach even when its publication token
        // matches the token observed before the failed batch.
        self.symbols = crate::symbols::SymbolTable::default();
        self.file_trigram = std::sync::OnceLock::new();
        self.last_vector_publication = None;
        *self
            .vector
            .write()
            .unwrap_or_else(|error| error.into_inner()) = None;

        // Replace all mmap-backed state before dropping the unpublished store;
        // Windows cannot remove a generation while one of its mappings is live.
        let reload_result = self.reload_from_disk();
        drop(old_tantivy);
        drop(old_store);
        reload_result
    }

    pub(super) fn abort_batch_error(&mut self, error: CodixingError) -> CodixingError {
        match self.abort_working_generation() {
            Ok(()) => error,
            Err(abort_error) => CodixingError::Index(format!(
                "{error}; failed to restore the active generation after abort: {abort_error}"
            )),
        }
    }

    /// Validate mmap-backed resident views before committing the generation,
    /// then install the already-open mappings after publication.
    ///
    /// Loading before the manifest swap keeps format or I/O failures safely
    /// abortable. Once publication succeeds, replacing the accumulated symbol
    /// overlay and materialized trigram table is infallible and bounds a
    /// long-lived daemon's resident state to the newly active snapshot.
    pub(super) fn publish_generation_with_preopened_indexes(&mut self) -> Result<()> {
        // A pre-sidecar active generation is still a valid read base. Any new
        // publication upgrades it to an explicit empty pair before validation;
        // sidecar-without-base remains an error in the paired loader below.
        if self.store.file_trigram_path().is_file()
            && !self.store.file_trigram_delta_path().exists()
            && !self.store.file_trigram_delta_required()
        {
            // Very old bases used bitcode rather than the current mmap format.
            // Rewrite only that compatibility representation before pairing it;
            // a current mmap base recognizes the same destination and is a no-op.
            let legacy_compatible = crate::index::trigram::FileTrigramIndex::load_binary(
                &self.store.file_trigram_path(),
            )?;
            legacy_compatible.save_binary(&self.store.file_trigram_path())?;
            self.store.save_file_trigram_delta_bytes(
                &crate::index::trigram::FileTrigramIndex::empty_delta_checkpoint()?,
            )?;
        }
        let symbols = super::init::load_persisted_symbols(&self.store)?;
        if matches!(&symbols, crate::symbols::SymbolTable::InMemory(_)) {
            return Err(CodixingError::Serialization(format!(
                "checkpoint symbols_v2.bin could not be reopened before publication (base exists: {}, delta exists: {}, legacy exists: {})",
                self.store.symbols_v2_path().is_file(),
                self.store.symbols_delta_path().is_file(),
                self.store.symbols_path().is_file()
            )));
        }
        let file_trigram = (self.store.file_trigram_path().is_file()
            || self.store.file_trigram_delta_path().is_file())
        .then(|| super::load_persisted_file_trigram(&self.store))
        .transpose()?;

        self.store.publish_generation()?;

        self.symbols = symbols;
        self.file_trigram = match file_trigram {
            Some(index) => std::sync::OnceLock::from(index),
            None => std::sync::OnceLock::new(),
        };
        // Publication's first cleanup can legitimately lose a race with this
        // engine's old symbol/trigram mappings on Windows. Retry only after the
        // assignments above have released those superseded mappings.
        self.store.retry_inactive_generation_cleanup();
        Ok(())
    }

    /// Persist repository-wide derived artifacts once per checkpoint rather
    /// than once per editor event. Tantivy, symbols, chunk metadata, and graph
    /// overlays remain queryable in memory while this work is deferred.
    pub(super) fn persist_checkpoint_artifacts(&mut self) -> Result<()> {
        self.persist_checkpoint_artifacts_with_graph_state(true, true)
    }

    fn persist_checkpoint_artifacts_with_graph_state(
        &mut self,
        graph_semantics_changed: bool,
        vector_changed: bool,
    ) -> Result<()> {
        let _ = self.get_file_trigram();
        let file_trigram = self.file_trigram.get_mut().ok_or_else(|| {
            CodixingError::Index("file trigram failed to initialize for checkpoint".to_string())
        })?;
        if let Some(bytes) = file_trigram.delta_checkpoint_bytes()? {
            self.store.save_file_trigram_delta_bytes(&bytes)?;
        } else {
            file_trigram.compact_tombstones();
            file_trigram.save_binary(&self.store.file_trigram_path())?;
            self.store.save_file_trigram_delta_bytes(
                &crate::index::trigram::FileTrigramIndex::empty_delta_checkpoint()?,
            )?;
        }
        // The shared file-level artifact now serves raw grep and transformed
        // exact-search recall. Drop any legacy duplicate inherited by the COW
        // fork before publishing the checkpoint.
        match std::fs::remove_file(self.store.chunk_trigram_path()) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        if graph_semantics_changed {
            if let Some(ref mut graph) = self.graph {
                let scores = compute_pagerank(
                    graph,
                    self.config.graph.damping,
                    self.config.graph.iterations,
                );
                graph.apply_pagerank(&scores);
                let flat = graph.to_flat();
                self.store.save_graph(&flat)?;
                self.store.save_symbol_graph(graph)?;
            }

            // Concepts/reformulations were already invalidated by the mutation
            // path. Rebuild them only for small repositories: construction walks
            // the whole symbol table and dominated one-file latency/rewrite cost
            // at 10K+ files. Primary BM25/symbol/graph search remains correct
            // without them; large repos pick concepts back up on the next full
            // init or explicit graph rebuild.
            const SEMANTIC_REBUILD_FILE_LIMIT: usize = 1_000;
            let file_count = self.stats().file_count;
            if file_count <= SEMANTIC_REBUILD_FILE_LIMIT {
                self.rebuild_semantic_artifacts()?;
            } else {
                debug!(
                    file_count,
                    limit = SEMANTIC_REBUILD_FILE_LIMIT,
                    "skipping concept/reformulation rebuild on large-repo incremental checkpoint"
                );
            }
        } else {
            debug!("retaining graph-derived checkpoint artifacts after graph-neutral mutations");
        }
        // Graph-derived artifacts were either persisted explicitly above or
        // proven identical to their inherited hardlinks. Persist the remaining
        // line/content-sensitive state without serializing graph.bin twice.
        self.save_checkpoint_state(false, vector_changed)
    }

    /// Signature baseline matching the resident graph/search state.
    ///
    /// A daemon can apply several editor batches to one unpublished working
    /// generation. The on-disk sidecar still describes the active generation,
    /// so pending exact parses must overlay it (including removals) before the
    /// next batch decides whether embedding vectors are reusable.
    fn effective_prior_signatures(&self) -> std::collections::HashMap<std::path::PathBuf, u64> {
        let mut signatures: std::collections::HashMap<_, _> = self
            .store
            .load_tree_signatures()
            .unwrap_or_default()
            .into_iter()
            .collect();
        for (path, signature) in &self.pending_checkpoint.successful_signatures {
            let Some(relative) = self.config.normalize_path(path) else {
                continue;
            };
            let relative = std::path::PathBuf::from(relative);
            if let Some(signature) = signature {
                signatures.insert(relative, *signature);
            } else {
                signatures.remove(&relative);
            }
        }
        signatures
    }

    /// Apply changed files to the unpublished working generation without
    /// rebuilding repository-wide sidecars. The daemon uses this hot path and
    /// calls [`Self::checkpoint_pending_changes`] after an idle window.
    pub fn apply_changes_deferred(&mut self, changes: &[crate::watcher::FileChange]) -> Result<()> {
        if self.read_only {
            return Err(CodixingError::ReadOnly);
        }
        use crate::watcher::{ChangeKind, FileChange};

        let mut combined = std::collections::BTreeMap::new();
        if !self.store.owns_rebuild_lock() || self.pending_checkpoint.successful_paths.is_empty() {
            for path in self.store.load_dirty_paths()? {
                let kind = if path.is_file() && self.config.is_indexable_path(&path) {
                    ChangeKind::Modified
                } else {
                    ChangeKind::RemovedDirectory
                };
                combined.insert(path, kind);
            }
        }
        for change in changes {
            let kind = if matches!(change.kind, ChangeKind::Modified)
                && !self.config.is_indexable_path(&change.path)
            {
                ChangeKind::RemovedDirectory
            } else {
                change.kind.clone()
            };
            combined.insert(change.path.clone(), kind);
        }
        if combined.is_empty() {
            return Ok(());
        }
        let changes: Vec<_> = combined
            .into_iter()
            .map(|(path, kind)| FileChange { path, kind })
            .collect();
        let prior_signatures = self.effective_prior_signatures();
        let outcome = match self.apply_changes_classified(&changes, &prior_signatures) {
            Ok(outcome) => outcome,
            Err(error) => return Err(self.abort_batch_error(error)),
        };
        if let Some(error) = outcome.failure_error() {
            return Err(self.abort_batch_error(error));
        }
        self.pending_checkpoint.merge(outcome);
        Ok(())
    }

    /// Durably publish a deferred batch. `active-generation.json` is replaced
    /// last; until then, all other processes continue to read the old complete
    /// generation. Direct mutation APIs call this synchronously.
    pub fn checkpoint_pending_changes(&mut self) -> Result<()> {
        if self.read_only {
            return Err(CodixingError::ReadOnly);
        }
        if let Some(error) = self.pending_checkpoint.failure_error() {
            return Err(self.abort_batch_error(error));
        }
        if self.pending_checkpoint.successful_paths.is_empty() {
            return Ok(());
        }

        self.persist_checkpoint_artifacts_with_graph_state(
            self.pending_checkpoint.graph_semantics_changed,
            self.pending_checkpoint.vector_changed,
        )?;
        self.persist_exact_signatures(&self.pending_checkpoint)?;
        let successful_delta: Vec<_> = self
            .pending_checkpoint
            .successful_hashes
            .iter()
            .map(|(path, entry)| (path.clone(), entry.clone()))
            .collect();
        if !successful_delta.is_empty() {
            self.store.update_tree_hash_delta(&successful_delta)?;
            self.compact_hash_delta_if_needed()?;
        }

        let successful_paths = self.pending_checkpoint.successful_paths.clone();
        self.store
            .prepare_dirty_paths_for_publication(&successful_paths)?;
        self.publish_generation_with_preopened_indexes()?;
        self.pending_checkpoint = ApplyChangesOutcome::default();
        if let Err(error) = self.store.clear_dirty_paths(&successful_paths) {
            // The manifest is already durable: publication succeeded. The
            // generation-bound journal self-identifies as complete on the next
            // open, so cleanup must never turn this into a retriable failure
            // that could mutate the newly active generation in place.
            warn!(%error, "published checkpoint journal cleanup deferred");
        }
        Ok(())
    }

    /// Apply a batch of file changes to the index.
    ///
    /// Processes all files first (parse, chunk, embed), then issues a single
    /// Tantivy commit for the entire batch, then runs PageRank exactly once.
    /// For N-file batches (e.g. after `git pull`) this reduces N fsyncs to 1.
    pub fn apply_changes(&mut self, changes: &[crate::watcher::FileChange]) -> Result<()> {
        let deferred_error = self.apply_changes_deferred(changes).err();
        match self.checkpoint_pending_changes() {
            Err(error) => Err(error),
            Ok(()) => match deferred_error {
                Some(error) => Err(error),
                None => Ok(()),
            },
        }
    }

    fn resolve_change_paths(
        &self,
        changes: &[crate::watcher::FileChange],
    ) -> Result<Vec<crate::watcher::FileChange>> {
        use crate::watcher::ChangeKind;

        changes
            .iter()
            .map(|change| {
                let allow_missing = matches!(
                    change.kind,
                    ChangeKind::Removed | ChangeKind::RemovedDirectory
                );
                match self.resolve_mutation_path(&change.path, allow_missing) {
                    Ok(path) => Ok(crate::watcher::FileChange {
                        path,
                        kind: change.kind.clone(),
                    }),
                    Err(original) if matches!(change.kind, ChangeKind::Modified) => {
                        // Editor/VCS temp files routinely disappear between the
                        // event and lock acquisition. If the lexical path is
                        // still safely contained, reinterpret the ambiguity as
                        // a prefix-capable removal instead of aborting siblings.
                        match self.resolve_mutation_path(&change.path, true) {
                            Ok(path) if !path.exists() => Ok(crate::watcher::FileChange {
                                path,
                                kind: ChangeKind::RemovedDirectory,
                            }),
                            _ => Err(original),
                        }
                    }
                    Err(error) => Err(error),
                }
            })
            .collect()
    }

    /// Expand directory intents into concrete per-file changes.
    ///
    /// Some native watchers report only the directory for a rename or recursive
    /// delete. Search artifacts are keyed by file, so removing the literal
    /// directory would otherwise leave every descendant under its stale path.
    fn expand_directory_changes(
        &self,
        changes: &[crate::watcher::FileChange],
    ) -> Result<Vec<crate::watcher::FileChange>> {
        use crate::watcher::{ChangeKind, FileChange};

        let mut expanded = std::collections::HashMap::<std::path::PathBuf, ChangeKind>::new();
        let mut sorted_indexed_paths: Option<Vec<String>> = None;
        for change in changes {
            if change.kind == ChangeKind::CreatedDirectory {
                for path in super::indexing::walk_source_directory(&change.path, &self.config)? {
                    expanded.insert(path, ChangeKind::Modified);
                }
                continue;
            }
            if change.kind == ChangeKind::RemovedDirectory
                && let Some(rel) = self.config.normalize_path(&change.path)
            {
                // Normal file removals are by far the common case. Avoid a
                // whole-repository prefix scan when the exact key is known.
                if self.file_chunk_ids.contains_key(&rel) {
                    expanded.insert(change.path.clone(), ChangeKind::Removed);
                    continue;
                }

                let rel = rel.trim_end_matches('/');
                let prefix = if rel.is_empty() {
                    String::new()
                } else {
                    format!("{rel}/")
                };
                // Build and sort the repository key set at most once per batch.
                // Each possible directory removal then uses O(log N + matches)
                // prefix lookup instead of rescanning N files.
                let indexed_paths = sorted_indexed_paths.get_or_insert_with(|| {
                    let mut paths: Vec<String> = self.file_chunk_ids.keys().cloned().collect();
                    paths.sort_unstable();
                    paths
                });
                let start = indexed_paths.partition_point(|indexed| indexed < &prefix);
                let descendants = indexed_paths[start..]
                    .iter()
                    .take_while(|indexed| indexed.starts_with(&prefix));
                let mut found_descendant = false;
                for indexed in descendants {
                    if let Some(path) = self.config.resolve_path_for_write(indexed)
                        && (!rel.is_empty() || path.starts_with(&change.path))
                    {
                        found_descendant = true;
                        expanded.insert(path, ChangeKind::Removed);
                    }
                }
                if found_descendant {
                    continue;
                }
            }
            expanded.insert(change.path.clone(), change.kind.clone());
        }

        Ok(expanded
            .into_iter()
            .map(|(path, kind)| FileChange { path, kind })
            .collect())
    }

    /// Apply a batch using signature fingerprints from the exact prior live
    /// state. Reindex compares each fingerprint with the bytes it parses, so
    /// watcher and git paths avoid a separate read/parse classification pass.
    ///
    /// Returns successful mutations, per-file failures, and embedding-reuse
    /// counts. Callers publish only when the entire outcome is successful; any
    /// failure discards the working generation while the dirty journal keeps
    /// every path retriable.
    fn apply_changes_classified(
        &mut self,
        changes: &[crate::watcher::FileChange],
        prior_signatures: &std::collections::HashMap<std::path::PathBuf, u64>,
    ) -> Result<ApplyChangesOutcome> {
        if self.read_only {
            return Err(CodixingError::ReadOnly);
        }
        use crate::watcher::ChangeKind;

        if changes.is_empty() {
            return Ok(ApplyChangesOutcome::default());
        }

        let resolved_changes = self.resolve_change_paths(changes)?;
        let expanded_changes = self.expand_directory_changes(&resolved_changes)?;
        let changes = expanded_changes.as_slice();
        if changes.is_empty() {
            return Ok(ApplyChangesOutcome::default());
        }

        self.ensure_working_generation()?;
        self.symbols.ensure_mutable();

        // Snapshot callers before direct removals only. A removed graph node
        // loses its incoming edges, so those callers cannot be recovered after
        // the mutation. Modified files keep their incoming edges: defer their
        // caller discovery until the exact parsed signature proves that the
        // edit was structural. This makes a cosmetic edit to a common hub O(1)
        // in its fan-in instead of eagerly re-indexing the whole repository.
        // Directory removals have already expanded to descendants.
        let changed_paths: std::collections::HashSet<String> = changes
            .iter()
            .filter_map(|change| self.config.normalize_path(&change.path))
            .collect();
        let mut cascade_paths = std::collections::BTreeSet::new();
        if let Some(ref graph) = self.graph {
            for changed in changes.iter().filter(|change| {
                matches!(
                    change.kind,
                    ChangeKind::Removed | ChangeKind::RemovedDirectory
                )
            }) {
                let Some(changed) = self.config.normalize_path(&changed.path) else {
                    continue;
                };
                for caller in graph.callers(&changed) {
                    if changed_paths.contains(&caller) {
                        continue;
                    }
                    match self.config.resolve_path(&caller) {
                        Some(abs) => {
                            cascade_paths.insert(abs);
                        }
                        None => {
                            warn!(caller = %caller, "rejected unsafe or missing cascade path");
                        }
                    }
                }
            }
        }

        // Force-init the shared file trigram so it is available for mutation.
        let _ = self.get_file_trigram();

        // Journal every changed path before mutating search artifacts. The tiny
        // sidecar avoids rewriting the repository-sized hash snapshot twice per
        // editor batch while still covering both crash windows.
        let mut pending_paths: Vec<_> = changes.iter().map(|change| change.path.clone()).collect();
        pending_paths.extend(cascade_paths.iter().cloned());
        self.store.mark_dirty_paths(&pending_paths)?;

        let mut outcome = ApplyChangesOutcome::default();
        let mut direct_import_updates = Vec::new();
        // Capture the complete phase before its first mutation. Sequential
        // snapshots are order-dependent because changing one file can remove
        // symbol edges owned by a later file in the same batch.
        let direct_semantic_before: Vec<_> = changes
            .iter()
            .map(|change| {
                (
                    change.path.clone(),
                    self.file_derived_semantic_state(&change.path),
                )
            })
            .collect();
        let mut pre_removal_callers = Vec::new();
        let mut changed_resolution_names = std::collections::BTreeSet::new();

        for change in changes {
            match change.kind {
                ChangeKind::Modified => {
                    // The bounded exact read decides oversized status from the
                    // bytes actually observed, avoiding a stat/shrink race.
                    let expected_signature = self
                        .config
                        .normalize_path(&change.path)
                        .and_then(|path| prior_signatures.get(std::path::Path::new(&path)))
                        .copied();
                    let update = self.reindex_file_impl(&change.path, expected_signature);
                    match update {
                        Ok(mut update) => {
                            outcome.vector_changed |= update.vector_changed;
                            changed_resolution_names
                                .extend(update.resolution_names_changed.drain(..));
                            pre_removal_callers.append(&mut update.pre_removal_callers);
                            if let Some(import_update) = update.import_update.take() {
                                direct_import_updates.push(import_update);
                            }
                            if update.cosmetic_reused {
                                outcome.cosmetic_skipped += 1;
                            }
                            outcome.successful_paths.insert(change.path.clone());
                            outcome
                                .successful_hashes
                                .insert(change.path.clone(), update.hash_entry);
                            outcome
                                .successful_signatures
                                .insert(change.path.clone(), update.signature);
                        }
                        Err(e) => {
                            warn!(path = %change.path.display(), error = %e, "failed to reindex");
                            outcome
                                .retry_changes
                                .insert(change.path.clone(), change.kind.clone());
                            outcome
                                .failures
                                .push(format!("{}: {e}", change.path.display()));
                        }
                    }
                }
                ChangeKind::Removed | ChangeKind::RemovedDirectory => {
                    let relative_path = self.config.normalize_path(&change.path);
                    let resolution_names = relative_path
                        .as_deref()
                        .and_then(|path| {
                            self.graph
                                .as_ref()
                                .map(|graph| graph.file_symbol_resolution_names(path))
                        })
                        .unwrap_or_default();
                    let vector_changed = relative_path.as_deref().is_some_and(|path| {
                        self.vector
                            .read()
                            .unwrap_or_else(|error| error.into_inner())
                            .as_ref()
                            .is_some_and(|index| index.file_chunks().contains_key(path))
                    });
                    let removal = self.remove_file_inner(&change.path);
                    match removal {
                        Ok(()) => {
                            outcome.vector_changed |= vector_changed;
                            changed_resolution_names.extend(resolution_names);
                            outcome.successful_paths.insert(change.path.clone());
                            outcome.successful_hashes.insert(change.path.clone(), None);
                            outcome
                                .successful_signatures
                                .insert(change.path.clone(), None);
                        }
                        Err(e) => {
                            warn!(path = %change.path.display(), error = %e, "failed to remove");
                            outcome
                                .retry_changes
                                .insert(change.path.clone(), change.kind.clone());
                            outcome
                                .failures
                                .push(format!("{}: {e}", change.path.display()));
                        }
                    }
                }
                ChangeKind::CreatedDirectory => {
                    outcome
                        .retry_changes
                        .insert(change.path.clone(), change.kind.clone());
                    outcome.failures.push(format!(
                        "{}: created directory was not expanded",
                        change.path.display()
                    ));
                }
            }
        }
        self.apply_import_graph_updates(direct_import_updates);

        // A definition can become globally resolvable without having had an
        // old incoming edge: adding the first definition resolves previously
        // unresolved calls, while removing one duplicate resolves formerly
        // ambiguous calls. Use the exact source trigram to find only files
        // that can contain each affected name; short names or corrupt postings
        // deliberately fall back to all indexed files to preserve correctness.
        // Unchanged definition files remain candidates because they can also
        // contain calls whose unique/ambiguous resolution just changed.
        let mut external_doc_paths = std::collections::BTreeSet::new();
        for name in changed_resolution_names {
            let candidates = self
                .get_file_trigram()
                .candidates_for_literal(name.as_bytes())
                .map(|paths| paths.into_iter().map(str::to_owned).collect::<Vec<_>>())
                .unwrap_or_else(|| self.file_chunk_ids.keys().cloned().collect());
            for candidate in candidates {
                if changed_paths.contains(&candidate) {
                    continue;
                }
                if candidate.starts_with(crate::external::EXTERNAL_PATH_PREFIX) {
                    external_doc_paths.insert(candidate);
                    continue;
                }
                match self.config.resolve_path(&candidate) {
                    Some(abs) => {
                        cascade_paths.insert(abs);
                    }
                    None => {
                        warn!(caller = %candidate, symbol = %name, "rejected unsafe or missing symbol-resolution cascade path");
                    }
                }
            }
        }
        outcome.graph_semantics_changed |= self.refresh_external_doc_edges(&external_doc_paths);

        // An oversized file is effectively removed, so it supplies the callers
        // captured before its graph node disappeared. Normal definition
        // transitions use the exact changed-name candidates collected above.
        for caller in pre_removal_callers {
            if changed_paths.contains(&caller) {
                continue;
            }
            match self.config.resolve_path(&caller) {
                Some(abs) => {
                    cascade_paths.insert(abs);
                }
                None => {
                    warn!(caller = %caller, "rejected unsafe or missing cascade path");
                }
            }
        }
        let cascade_paths: Vec<_> = cascade_paths.into_iter().collect();
        if !cascade_paths.is_empty() {
            // Dynamic cascades are known only after the exact parse above, but
            // they are still journaled before any caller itself is mutated.
            self.store.mark_dirty_paths(&cascade_paths)?;
        }

        for (path, before) in direct_semantic_before {
            let after = self.file_derived_semantic_state(&path);
            outcome.graph_semantics_changed |= Self::file_derived_semantics_changed(before, after);
        }

        // Cascade only for removals, oversized transitions, or exact changed-
        // name candidates. Verified cosmetic edits never enumerate callers.
        if !cascade_paths.is_empty() {
            info!(
                count = cascade_paths.len(),
                "cascading re-index to callers of changed files"
            );
            let mut cascade_import_updates = Vec::new();
            let cascade_semantic_before: Vec<_> = cascade_paths
                .iter()
                .map(|path| (path.clone(), self.file_derived_semantic_state(path)))
                .collect();
            for path in &cascade_paths {
                #[cfg(test)]
                CASCADE_REINDEX_COUNT.with(|count| count.set(count.get() + 1));
                // Cascade reindexes (callers of changed files) are never treated
                // as cosmetic — their resolved import/call edges may shift even
                // when their own signatures did not.
                let update = self.reindex_file_impl(path, None);
                match update {
                    Ok(mut update) => {
                        outcome.vector_changed |= update.vector_changed;
                        if let Some(import_update) = update.import_update.take() {
                            cascade_import_updates.push(import_update);
                        }
                        outcome.successful_paths.insert(path.clone());
                        outcome
                            .successful_hashes
                            .insert(path.clone(), update.hash_entry);
                        outcome
                            .successful_signatures
                            .insert(path.clone(), update.signature);
                    }
                    Err(e) => {
                        warn!(path = %path.display(), error = %e, "cascade-reindex caller failed");
                        outcome
                            .retry_changes
                            .insert(path.clone(), ChangeKind::Modified);
                        outcome
                            .failures
                            .push(format!("cascade {}: {e}", path.display()));
                    }
                }
            }
            self.apply_import_graph_updates(cascade_import_updates);
            for (path, before) in cascade_semantic_before {
                let after = self.file_derived_semantic_state(&path);
                outcome.graph_semantics_changed |=
                    Self::file_derived_semantics_changed(before, after);
            }
        }

        // A cascade failure is causally tied to the direct changes. Keep the
        // whole direct batch pending so a later sync repeats the cascade rather
        // than publishing hashes that would make it unreachable.
        if outcome
            .failures
            .iter()
            .any(|failure| failure.starts_with("cascade"))
        {
            outcome.retry_changes.extend(
                changes
                    .iter()
                    .map(|change| (change.path.clone(), change.kind.clone())),
            );
            outcome.retry_changes.extend(
                cascade_paths
                    .iter()
                    .cloned()
                    .map(|path| (path, ChangeKind::Modified)),
            );
            outcome.successful_paths.clear();
            outcome.successful_hashes.clear();
            outcome.successful_signatures.clear();
            outcome.cosmetic_skipped = 0;
            outcome.vector_changed = false;
        }

        if outcome.graph_semantics_changed {
            // The working generation is still unpublished, so invalidating
            // inherited auxiliary files here cannot affect active readers. A
            // failed rebuild prevents publication and leaves A authoritative.
            self.invalidate_semantic_artifacts()?;
        }

        // Single Tantivy commit for all pending adds + deletes.
        self.tantivy.commit()?;

        // Repository-wide PageRank, semantic artifacts, and full mmap/bitcode
        // sidecars are intentionally deferred to the checkpoint boundary.
        Ok(outcome)
    }

    /// Map a set of absolute file paths to their normalized relative paths,
    /// matching the keys used in the signature sidecar.
    fn normalized_rel_set(
        &self,
        abs_paths: &std::collections::HashSet<std::path::PathBuf>,
    ) -> std::collections::HashSet<std::path::PathBuf> {
        abs_paths
            .iter()
            .filter_map(|path| self.config.normalize_path(path))
            .map(std::path::PathBuf::from)
            .collect()
    }

    /// Publish signature fingerprints only for direct file changes that were
    /// actually applied. Failed modifications retain their previous fingerprint,
    /// and failed removals remain in the seen set so they are retried without
    /// losing their prior classification baseline.
    fn persist_signatures_after_apply(
        &self,
        changes: &[crate::watcher::FileChange],
        outcome: &ApplyChangesOutcome,
        seen: &std::collections::HashSet<std::path::PathBuf>,
    ) -> Result<()> {
        use crate::watcher::ChangeKind;

        let mut signature_seen = seen.clone();
        for change in changes {
            if matches!(
                change.kind,
                ChangeKind::Removed | ChangeKind::RemovedDirectory
            ) && !outcome.successful_paths.contains(&change.path)
            {
                signature_seen.insert(change.path.clone());
            }
        }

        let successful_modified: std::collections::HashSet<std::path::PathBuf> =
            outcome.successful_signatures.keys().cloned().collect();
        let seen_rel = self.normalized_rel_set(&signature_seen);
        let modified_rel = self.normalized_rel_set(&successful_modified);
        let successful_signatures: std::collections::HashMap<std::path::PathBuf, u64> = outcome
            .successful_signatures
            .iter()
            .filter_map(|(path, signature)| {
                let rel = self.config.normalize_path(path)?;
                let rel = std::path::PathBuf::from(rel);
                if !modified_rel.contains(&rel) {
                    return None;
                }
                signature.map(|signature| (rel, signature))
            })
            .collect();

        self.store.update_tree_signatures(|latest| {
            let latest: std::collections::HashMap<std::path::PathBuf, u64> =
                latest.into_iter().collect();
            merge_signatures(&latest, &successful_signatures, &seen_rel, &modified_rel)
        })
    }

    /// Apply targeted signature updates from the exact parse used for an
    /// incremental transaction. `None` removes a stale fingerprint.
    fn persist_exact_signatures(&self, outcome: &ApplyChangesOutcome) -> Result<()> {
        let updates: Vec<_> = outcome
            .successful_signatures
            .iter()
            .filter_map(|(path, signature)| {
                self.config
                    .normalize_path(path)
                    .map(std::path::PathBuf::from)
                    .map(|path| (path, *signature))
            })
            .collect();
        if updates.is_empty() {
            return Ok(());
        }
        self.store.update_tree_signatures(|latest| {
            let mut latest: std::collections::HashMap<_, _> = latest.into_iter().collect();
            for (path, signature) in &updates {
                if let Some(signature) = signature {
                    latest.insert(path.clone(), *signature);
                } else {
                    latest.remove(path);
                }
            }
            latest.into_iter().collect()
        })
    }

    /// Persist only the vector artifacts used to resume `embed_remaining`.
    ///
    /// A full [`Self::save`] rewrites symbols, compact chunk metadata, graph, and
    /// index metadata as well. Doing that every few thousand chunks makes a
    /// large repository's one-time embedding pass needlessly I/O-bound.
    fn checkpoint_vector_index(&self) -> Result<()> {
        let vec_guard = self.vector.read().unwrap_or_else(|e| e.into_inner());
        let vec_idx = vec_guard
            .as_ref()
            .ok_or_else(|| CodixingError::Config("vector index bootstrap failed".into()))?;
        vec_idx.save(
            &self.store.vector_index_path(),
            &self.store.file_chunks_path(),
        )
    }

    /// Embed all chunks that are in the BM25 index but not yet in the vector index.
    ///
    /// Useful when a project was initialized without `--embed` and embeddings
    /// are later desired. Persists the updated vector index to disk.
    /// Returns the number of chunks that were embedded.
    pub fn embed_remaining(&mut self) -> Result<usize> {
        if self.read_only {
            return Err(CodixingError::ReadOnly);
        }

        // Wait for any background embedding to complete before modifying the vector index.
        self.wait_for_embeddings();

        // Bootstrap embedding state for a BM25-only index. Previously open()
        // only created these objects when vector artifacts already existed,
        // making the documented post-hoc `codixing embed` command impossible.
        let embedder = match self.embedder.as_ref() {
            Some(embedder) => Arc::clone(embedder),
            None => {
                let embedder = Arc::new(Embedder::new(&self.config.embedding.model)?);
                self.embedder = Some(Arc::clone(&embedder));
                embedder
            }
        };
        let bootstrapped_vector = {
            let mut vec_guard = self.vector.write().unwrap_or_else(|e| e.into_inner());
            if vec_guard.is_none() {
                *vec_guard = Some(VectorIndex::new(
                    embedder.dims,
                    self.config.embedding.quantize,
                )?);
                true
            } else {
                false
            }
        };

        // Treat config.json as the readiness marker for vector artifacts. Write
        // the current (possibly empty) index first so a crash can never expose
        // `enabled = true` with stale artifacts, then enable it before doing any
        // expensive work so every later checkpoint is discoverable on reopen.
        if bootstrapped_vector {
            self.checkpoint_vector_index()?;
        }
        if !self.config.embedding.enabled {
            self.config.embedding.enabled = true;
            self.store.save_config(&self.config)?;
        }

        // Determine which chunk IDs already have vector representations.
        let embedded: std::collections::HashSet<u64> = self
            .vector
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|index| index.file_chunks().values().flatten().copied().collect())
            .unwrap_or_default();

        // Build the per-file work queue directly. Keeping a separate flat list
        // first doubles the chunk-ID allocation on million-chunk repositories.
        let mut by_file: std::collections::BTreeMap<String, Vec<u64>> =
            std::collections::BTreeMap::new();
        let mut remaining_count = 0usize;
        for meta in self.chunk_meta.iter() {
            let chunk_id = *meta.key();
            if !embedded.contains(&chunk_id) {
                by_file
                    .entry(meta.file_path.clone())
                    .or_default()
                    .push(chunk_id);
                remaining_count += 1;
            }
        }

        if remaining_count == 0 {
            info!("all chunks already embedded; nothing to do");
            return Ok(0);
        }

        info!(count = remaining_count, "embedding remaining chunks");

        // Group by file so compact chunk bodies can be hydrated from Tantivy in
        // bounded batches, embedded, and immediately released again.
        let contextual = self.config.embedding.contextual_embeddings;
        let mut embedded_count = 0usize;
        let mut last_checkpoint = Instant::now();

        for (file_path, chunk_ids) in by_file {
            let ids: std::collections::HashSet<u64> = chunk_ids.iter().copied().collect();
            let contents = match self.hydrate_chunk_contents(&ids) {
                Ok(contents) => contents,
                Err(error) => {
                    self.checkpoint_vector_index()?;
                    return Err(error);
                }
            };
            for (chunk_id, content) in contents {
                if let Some(mut meta) = self.chunk_meta.get_mut(&chunk_id) {
                    meta.content = content;
                }
            }

            let result = {
                let mut vec_guard = self.vector.write().unwrap_or_else(|e| e.into_inner());
                match vec_guard.as_mut() {
                    Some(vec_idx) => super::indexing::embed_single_file(
                        &embedder,
                        &self.chunk_meta,
                        vec_idx,
                        contextual,
                        self.store.root(),
                        &file_path,
                        &chunk_ids,
                    ),
                    None => Err(CodixingError::Config(
                        "vector index bootstrap failed".into(),
                    )),
                }
            };

            // Release the hydrated batch even when the model/runtime failed.
            for chunk_id in &chunk_ids {
                if let Some(mut meta) = self.chunk_meta.get_mut(chunk_id) {
                    // `String::clear` retains the hydrated source allocation,
                    // keeping the corpus resident after post-hoc embedding.
                    // Replace the buffer so each completed file batch releases
                    // its capacity before the next one is hydrated.
                    meta.content = String::new();
                }
            }
            let added = match result {
                Ok((added, _used_late_chunking)) => added,
                Err(error) => {
                    // `embed_single_file` can fail after one or more streaming
                    // batches have already reached the vector index. Preserve
                    // those partial results before returning the model error.
                    self.checkpoint_vector_index()?;
                    return Err(error);
                }
            };
            embedded_count += added;

            if last_checkpoint.elapsed() >= EMBED_CHECKPOINT_INTERVAL {
                self.checkpoint_vector_index()?;
                last_checkpoint = Instant::now();
            }
        }

        // A published lexical generation is immutable, so `save()` deliberately
        // becomes a no-op when no working generation is open. Vector checkpoints
        // have their own atomic publication protocol and may be attached after
        // lexical publication; persist the completed pass through that path so
        // the final interval is never lost on drop/reopen.
        self.checkpoint_vector_index()?;
        Ok(embedded_count)
    }

    /// Sync the index with the current filesystem state using stored content hashes.
    ///
    /// Uses a two-tier change detection strategy:
    /// 1. **Fast pre-filter (mtime+size)**: For each file, compare the current
    ///    filesystem mtime and size against cached values. If both match, the
    ///    file is assumed unchanged — no I/O beyond a `stat()` call.
    /// 2. **Full content hash (xxh3)**: Only files that fail the mtime+size
    ///    check are read and hashed. This catches actual changes while
    ///    eliminating ~95% of file reads on a typical sync.
    ///
    /// This works without `git` and handles any form of file drift (editor saves,
    /// `git pull`, manual copies, etc.). For an already-current index the method
    /// returns in milliseconds (stat scan only, no file reads or Tantivy commit).
    pub fn sync(&mut self) -> Result<SyncStats> {
        if self.read_only {
            return Err(CodixingError::ReadOnly);
        }
        self.migrate_graph_schema_if_outdated()?;
        let previous_git_commit = self.store.load_meta()?.git_commit;
        let scanned_git_commit = git_head_commit(&self.config.root);
        use crate::watcher::{ChangeKind, FileChange};
        use std::collections::{HashMap, HashSet};

        // Load stored hashes (v2 format with mtime+size, falls back to v1).
        let old_hashes: HashMap<std::path::PathBuf, FileHashEntry> = self
            .store
            .load_tree_hashes_v2()
            .unwrap_or_default()
            .into_iter()
            .collect();
        let dirty_paths: HashSet<std::path::PathBuf> =
            self.store.load_dirty_paths()?.into_iter().collect();
        let had_hash_delta = !self.store.load_tree_hash_delta()?.is_empty();
        // Load stored signature fingerprints (empty for indexes built before this
        // feature → every change classified STRUCTURAL on the first sync).
        let old_signatures: HashMap<std::path::PathBuf, u64> = self
            .store
            .load_tree_signatures()
            .unwrap_or_default()
            .into_iter()
            .collect();

        let current_files = super::indexing::walk_source_files(&self.config.root, &self.config)?;

        let mut changes: Vec<FileChange> = Vec::new();
        let mut seen: HashSet<std::path::PathBuf> = HashSet::new();
        let mut unchanged = 0usize;
        let mut skipped_by_mtime = 0usize;
        // Collect all current hashes so we can persist the complete set.
        let mut current_hashes: Vec<(std::path::PathBuf, FileHashEntry)> = Vec::new();

        for abs_path in &current_files {
            seen.insert(abs_path.clone());
            let force_dirty = dirty_paths.contains(abs_path);

            // Phase 1: Fast mtime+size pre-filter (stat only, no file read).
            let metadata = fs::metadata(abs_path);
            let (current_mtime, current_size) = match &metadata {
                Ok(m) => (m.modified().ok(), m.len()),
                Err(_) => (None, 0),
            };

            if !force_dirty
                && let Some(cached) = old_hashes.get(abs_path)
                && !cached.file_might_have_changed(current_mtime, current_size)
            {
                // mtime+size unchanged — skip the expensive content hash.
                unchanged += 1;
                skipped_by_mtime += 1;
                current_hashes.push((abs_path.clone(), cached.clone()));
                continue;
            }

            // Phase 2: enforce the byte cap again while reading, closing the
            // walk/stat race if a generated file grows concurrently.
            let Some(source) = read_source_bounded(abs_path, self.config.max_file_bytes)? else {
                if old_hashes.contains_key(abs_path) || force_dirty {
                    changes.push(FileChange {
                        path: abs_path.clone(),
                        kind: ChangeKind::Removed,
                    });
                }
                continue;
            };
            let hash = xxhash_rust::xxh3::xxh3_64(&source.bytes);
            let entry = stable_file_hash_entry(hash, source.metadata_before, source.metadata_after);

            match old_hashes.get(abs_path) {
                Some(cached) if !force_dirty && cached.content_hash == hash => {
                    // mtime/size changed but content is identical (e.g. touch).
                    // Update the cached mtime+size but don't reindex.
                    unchanged += 1;
                    current_hashes.push((abs_path.clone(), entry));
                }
                _ => {
                    // Genuinely changed or new file.
                    current_hashes.push((abs_path.clone(), entry));
                    changes.push(FileChange {
                        path: abs_path.clone(),
                        kind: ChangeKind::Modified,
                    });
                }
            }
        }

        // Files that were indexed before but are gone now.
        for old_path in old_hashes.keys() {
            if !seen.contains(old_path) {
                changes.push(FileChange {
                    path: old_path.clone(),
                    kind: ChangeKind::Removed,
                });
            }
        }
        let mut scheduled_paths: HashSet<std::path::PathBuf> =
            changes.iter().map(|change| change.path.clone()).collect();
        for dirty_path in &dirty_paths {
            if !seen.contains(dirty_path) && scheduled_paths.insert(dirty_path.clone()) {
                changes.push(FileChange {
                    path: dirty_path.clone(),
                    kind: ChangeKind::Removed,
                });
            }
        }

        let added = changes
            .iter()
            .filter(|c| matches!(c.kind, ChangeKind::Modified) && !old_hashes.contains_key(&c.path))
            .count();
        let modified = changes
            .iter()
            .filter(|c| matches!(c.kind, ChangeKind::Modified) && old_hashes.contains_key(&c.path))
            .count();
        let removed = changes
            .iter()
            .filter(|c| matches!(c.kind, ChangeKind::Removed | ChangeKind::RemovedDirectory))
            .count();

        info!(
            added,
            modified, removed, unchanged, skipped_by_mtime, "syncing index"
        );

        let mut cosmetic_skipped = 0usize;
        let mut published_paths = HashSet::new();
        if !changes.is_empty() {
            let outcome = match self.apply_changes_classified(&changes, &old_signatures) {
                Ok(outcome) => outcome,
                Err(error) => {
                    self.filter_pipeline.cleanup();
                    return Err(self.abort_batch_error(error));
                }
            };
            cosmetic_skipped = outcome.cosmetic_skipped;
            if let Some(error) = outcome.failure_error() {
                self.filter_pipeline.cleanup();
                return Err(self.abort_batch_error(error));
            }
            // Sidecars are publication markers for change detection. The
            // failure check above makes this an all-success batch; write
            // signatures first and hashes last before atomic publication.
            let authoritative_hashes = merge_hashes_after_apply(
                &old_hashes,
                std::mem::take(&mut current_hashes),
                &changes,
                &outcome.successful_hashes,
            );
            let persist_result = (|| -> Result<()> {
                self.persist_checkpoint_artifacts_with_graph_state(
                    outcome.graph_semantics_changed,
                    outcome.vector_changed,
                )?;
                self.persist_signatures_after_apply(&changes, &outcome, &seen)?;
                self.fold_hash_snapshot(&authoritative_hashes)
            })();
            if let Err(error) = persist_result {
                self.restore_git_commit(previous_git_commit.as_deref())?;
                self.filter_pipeline.cleanup();
                return Err(error);
            }
            published_paths = outcome.successful_paths.clone();
        } else {
            // Even if nothing changed content-wise, update the v2 hashes
            // to capture any mtime+size updates (e.g. file was touched).
            if had_hash_delta {
                self.ensure_working_generation()?;
                self.fold_hash_snapshot(&current_hashes)?;
            } else if skipped_by_mtime != unchanged {
                self.ensure_working_generation()?;
                self.store.save_tree_hashes_v2(&current_hashes)?;
            }
            info!("index already up-to-date");
        }

        debug!(cosmetic_skipped, "exact-parse signature reuse");

        // The full filesystem scan covered the whole configured corpus. Publish
        // its git position only after every searchable artifact and freshness
        // sidecar is durable.
        if let Some(scanned_git_commit) = scanned_git_commit.as_deref()
            && git_head_commit(&self.config.root).as_deref() == Some(scanned_git_commit)
            && previous_git_commit.as_deref() != Some(scanned_git_commit)
        {
            self.ensure_working_generation()?;
            self.restore_git_commit(Some(scanned_git_commit))?;
        }

        if self.store.owns_rebuild_lock() {
            self.store
                .prepare_dirty_paths_for_publication(&published_paths)?;
            self.publish_generation_with_preopened_indexes()?;
            self.pending_checkpoint = ApplyChangesOutcome::default();
            if let Err(error) = self.store.clear_dirty_paths(&published_paths) {
                warn!(%error, "published sync journal cleanup deferred");
            }
        }

        self.filter_pipeline.cleanup();

        Ok(SyncStats {
            added,
            modified,
            removed,
            unchanged,
            cosmetic_skipped,
        })
    }

    /// Sync with explicit options.
    ///
    /// When `options.skip_embed` is true, the embedder is temporarily
    /// stashed for the duration of the sync so `reindex_file_impl` skips
    /// the vector-embedding path entirely. This is the safety valve for
    /// the case where a sync on an existing hybrid index would otherwise
    /// burn minutes of CPU re-embedding an already-embedded corpus — see
    /// the Linux kernel benchmark finding that hit 68 minutes before
    /// being killed.
    ///
    /// When `options.rebuild_graph` is true, a full graph rebuild is
    /// performed after the normal incremental sync completes (see
    /// [`Engine::rebuild_graph_from_disk`]).
    ///
    /// After sync completes, if the graph is missing (older indexes
    /// that predate graph support, or a corrupted graph file), a warning
    /// is emitted via `on_progress` directing the user to run `init`
    /// to rebuild. Sync does NOT rebuild the graph itself unless
    /// `options.rebuild_graph` is set.
    pub fn sync_with_options<F>(
        &mut self,
        options: SyncOptions,
        on_progress: F,
    ) -> Result<SyncStats>
    where
        F: FnMut(&str) + Send + 'static,
    {
        // Stash the embedder if the caller opted out of embedding, and restore
        // it after sync_with_progress runs so state is intact whatever happens.
        let stashed_embedder = options.skip_embed.then(|| self.embedder.take()).flatten();

        let graph_was_missing = self.graph.is_none();
        let result = self.sync_with_progress(on_progress);

        if stashed_embedder.is_some() {
            self.embedder = stashed_embedder;
        }

        // Emit a helpful warning if the index has no graph. We don't fail
        // the sync — search still works on BM25+symbols — but we tell the
        // user that graph-dependent features (impact, caller/callee,
        // graph --map, community detection) will return empty until they
        // run `codixing init` to rebuild.
        if graph_was_missing && !options.rebuild_graph {
            warn!(
                "index has no dependency graph — graph-dependent features \
                 (impact, callers, callees, graph --map, communities) will \
                 return empty results. Run `codixing init` to rebuild the \
                 graph. This is normal for indexes created before v0.27."
            );
        }

        // If the caller requested a full graph rebuild, do it now — but only
        // after a successful incremental sync. Running rebuild on a failed
        // sync wastes work on a half-updated state and would mask the original
        // sync error with a secondary rebuild error.
        if options.rebuild_graph && result.is_ok() {
            self.rebuild_graph_from_disk()?;
        }

        result
    }

    /// Auto-rebuild the graph when it was persisted by an older edge-extraction
    /// schema (see [`crate::graph::GRAPH_SCHEMA_VERSION`]).
    ///
    /// Extractor/resolver fixes change which edges exist, but incremental sync
    /// only re-extracts edges for changed files — unchanged files keep edges
    /// resolved by the old code forever. This check runs at the start of every
    /// sync flavor so upgraded binaries heal existing indexes exactly once.
    ///
    /// Returns `true` if a rebuild ran.
    fn migrate_graph_schema_if_outdated(&mut self) -> Result<bool> {
        if self.graph.is_none() {
            // No graph to migrate — the missing-graph warning path handles this.
            return Ok(false);
        }
        let stored = self.store.load_graph_schema_version();
        let current = crate::graph::GRAPH_SCHEMA_VERSION;
        if stored >= current {
            return Ok(false);
        }
        info!(
            stored,
            current, "graph built by an older edge-extraction schema — rebuilding"
        );
        self.rebuild_graph_from_disk()?;
        Ok(true)
    }

    /// Rebuild the dependency graph from scratch by re-parsing all indexed files.
    ///
    /// This re-extracts import and call edges for every file currently in the
    /// index, replaces the in-memory graph with the fresh result, recomputes
    /// PageRank, and persists the updated graph to disk.
    ///
    /// Unlike a full `codixing init`, this method does **not** re-chunk,
    /// re-embed, or update BM25 — only the graph is rebuilt. This makes it
    /// significantly faster when the BM25 / vector indexes are already fresh
    /// but the call graph has drifted (e.g. after a large refactor).
    ///
    /// If the engine has no graph (older index that predates graph support),
    /// a new graph is created and populated.
    pub fn rebuild_graph_from_disk(&mut self) -> Result<()> {
        if self.read_only {
            return Err(CodixingError::ReadOnly);
        }

        // Flush both in-memory deferred work and any durable retry journal
        // before graph-only mutation starts. This keeps graph rebuilds from
        // hiding or stranding filesystem changes in an existing checkpoint.
        self.apply_changes(&[])?;
        self.ensure_working_generation()?;

        self.invalidate_semantic_artifacts()?;

        info!(
            files = self.file_chunk_ids.len(),
            "rebuilding dependency graph from disk"
        );

        // Persisted relative paths are not filesystem authority. Resolve every
        // file canonically and discard stale, corrupt, or symlink-escaped entries
        // before the graph rebuild performs any reads.
        let indexed_files: Vec<std::path::PathBuf> = self
            .file_chunk_ids
            .keys()
            .filter_map(|rel| {
                let resolved = self.config.resolve_path(rel);
                if resolved.is_none() {
                    warn!(path = %rel, "skipping unsafe or missing indexed path in graph rebuild");
                }
                resolved
            })
            .collect();

        // Build a fresh graph using the same code path as init, but
        // passing an empty import cache so build_graph falls back to
        // re-reading + re-parsing each file.
        let empty_imports: dashmap::DashMap<
            String,
            (
                Vec<crate::graph::extractor::RawImport>,
                crate::language::Language,
            ),
        > = dashmap::DashMap::new();

        let mut new_graph = super::indexing::build_graph(
            &indexed_files,
            &self.config.root,
            &self.config,
            &self.parser,
            &empty_imports,
        );

        // Refresh the symbol table AND collect call edges in one parallel pass.
        // SymbolTable::insert / remove_file take &self (DashMap-backed), so the
        // pass only needs &self. We also refresh symbols here so the downstream
        // add_call_edges resolves against a current symbol table — important
        // when this method is called directly on a file set whose contents
        // changed since the last sync.
        use rayon::prelude::*;
        let pending_calls: dashmap::DashMap<String, Vec<String>> = dashmap::DashMap::new();
        let pending_symbol_graph = PendingSymbolGraph::new();
        let fresh_symbols = crate::symbols::InMemorySymbolTable::new();
        indexed_files.par_iter().for_each(|file| {
            let rel_str = self.config.normalize_path(file).unwrap_or_else(|| {
                normalize_path(file.strip_prefix(&self.config.root).unwrap_or(file))
            });
            let preserve_existing_symbols = || {
                // A graph-only rebuild must preserve the prior searchable
                // state for a file it cannot authoritatively re-parse. This is
                // deliberately a failure-only compatibility path; v1/v2 mmap
                // tables may scan globally to recover this one exact file.
                for symbol in self.symbols.symbols_in_file(&rel_str, None) {
                    fresh_symbols.insert(symbol);
                }
            };
            let source = match read_source_bounded(file, self.config.max_file_bytes) {
                Ok(Some(source)) => source.bytes,
                Ok(None) => {
                    warn!(path = %file.display(), "skipping oversized file in rebuild_graph");
                    preserve_existing_symbols();
                    return;
                }
                Err(e) => {
                    warn!(path = %file.display(), error = %e, "skipping file in rebuild_graph");
                    preserve_existing_symbols();
                    return;
                }
            };
            let result = match self.parser.parse_file_transient(file, &source) {
                Ok(r) => r,
                Err(e) => {
                    warn!(path = %file.display(), error = %e, "parse failed in rebuild_graph");
                    preserve_existing_symbols();
                    return;
                }
            };

            // Populate an isolated table. Swapping it in after the parallel
            // pass avoids concurrent remove/rebuild cycles against the live
            // table and makes this rebuild authoritative for the file set.
            for entity in &result.entities {
                fresh_symbols.insert(symbol_from_entity(entity, &rel_str, result.language));
            }

            let Some(tree) = result.tree.as_ref() else {
                return;
            };
            let call_names =
                crate::graph::CallExtractor::extract_calls(tree, &source, result.language);
            if !call_names.is_empty() {
                pending_calls.insert(rel_str.clone(), call_names);
            }
            let (definitions, references) =
                extract_pending_symbol_graph(tree, &source, &rel_str, &result.language);
            if !definitions.is_empty() || !references.is_empty() {
                pending_symbol_graph.insert(rel_str, (definitions, references));
            }
        });
        self.symbols = crate::symbols::SymbolTable::InMemory(fresh_symbols);
        super::indexing::add_call_edges(&mut new_graph, &self.symbols, &pending_calls);

        // Populate the symbol graph from the same parsed trees rather than
        // triggering another corpus-wide read/parse pass.
        super::indexing::populate_symbol_graph(&mut new_graph, pending_symbol_graph);

        // Compute PageRank and apply scores.
        let scores = crate::graph::compute_pagerank(
            &new_graph,
            self.config.graph.damping,
            self.config.graph.iterations,
        );
        new_graph.apply_pagerank(&scores);

        // Replace the in-memory graph.
        self.graph = Some(new_graph);

        // Persist the updated graph. The caller explicitly requested a fresh
        // on-disk graph — if persistence fails, surface the error so next process
        // start doesn't silently load the stale graph.
        if let Some(ref g) = self.graph {
            let flat = g.to_flat();
            self.store.save_graph(&flat)?;
            self.store.save_symbol_graph(g)?;
        }

        self.rebuild_semantic_artifacts()?;

        self.save()?;
        self.publish_generation_with_preopened_indexes()?;

        info!("graph rebuild complete");
        Ok(())
    }

    /// Sync the index with progress callbacks for streaming updates.
    ///
    /// Identical to [`Self::sync`] but calls `on_progress` at key stages so
    /// callers (e.g. the SSE endpoint) can relay real-time feedback.
    pub fn sync_with_progress<F>(&mut self, mut on_progress: F) -> Result<SyncStats>
    where
        F: FnMut(&str) + Send + 'static,
    {
        if self.read_only {
            return Err(CodixingError::ReadOnly);
        }
        self.migrate_graph_schema_if_outdated()?;
        let previous_git_commit = self.store.load_meta()?.git_commit;
        let scanned_git_commit = git_head_commit(&self.config.root);
        use crate::watcher::{ChangeKind, FileChange};
        use std::collections::{HashMap, HashSet};

        on_progress("scanning files");

        // Load stored hashes (v2 format with mtime+size, falls back to v1).
        let old_hashes: HashMap<std::path::PathBuf, FileHashEntry> = self
            .store
            .load_tree_hashes_v2()
            .unwrap_or_default()
            .into_iter()
            .collect();
        let dirty_paths: HashSet<std::path::PathBuf> =
            self.store.load_dirty_paths()?.into_iter().collect();
        let had_hash_delta = !self.store.load_tree_hash_delta()?.is_empty();
        // Load stored signature fingerprints (empty for pre-feature indexes).
        let old_signatures: HashMap<std::path::PathBuf, u64> = self
            .store
            .load_tree_signatures()
            .unwrap_or_default()
            .into_iter()
            .collect();

        let current_files = super::indexing::walk_source_files(&self.config.root, &self.config)?;

        on_progress(&format!(
            "found {} files, detecting changes",
            current_files.len()
        ));

        let mut changes: Vec<FileChange> = Vec::new();
        let mut seen: HashSet<std::path::PathBuf> = HashSet::new();
        let mut unchanged = 0usize;
        let mut skipped_by_mtime = 0usize;
        let mut current_hashes: Vec<(std::path::PathBuf, FileHashEntry)> = Vec::new();

        for abs_path in &current_files {
            seen.insert(abs_path.clone());
            let force_dirty = dirty_paths.contains(abs_path);

            let metadata = fs::metadata(abs_path);
            let (current_mtime, current_size) = match &metadata {
                Ok(m) => (m.modified().ok(), m.len()),
                Err(_) => (None, 0),
            };

            if !force_dirty
                && let Some(cached) = old_hashes.get(abs_path)
                && !cached.file_might_have_changed(current_mtime, current_size)
            {
                unchanged += 1;
                skipped_by_mtime += 1;
                current_hashes.push((abs_path.clone(), cached.clone()));
                continue;
            }

            let Some(source) = read_source_bounded(abs_path, self.config.max_file_bytes)? else {
                if old_hashes.contains_key(abs_path) || force_dirty {
                    changes.push(FileChange {
                        path: abs_path.clone(),
                        kind: ChangeKind::Removed,
                    });
                }
                continue;
            };
            let hash = xxhash_rust::xxh3::xxh3_64(&source.bytes);
            let entry = stable_file_hash_entry(hash, source.metadata_before, source.metadata_after);

            match old_hashes.get(abs_path) {
                Some(cached) if !force_dirty && cached.content_hash == hash => {
                    unchanged += 1;
                    current_hashes.push((abs_path.clone(), entry));
                }
                _ => {
                    current_hashes.push((abs_path.clone(), entry));
                    changes.push(FileChange {
                        path: abs_path.clone(),
                        kind: ChangeKind::Modified,
                    });
                }
            }
        }

        for old_path in old_hashes.keys() {
            if !seen.contains(old_path) {
                changes.push(FileChange {
                    path: old_path.clone(),
                    kind: ChangeKind::Removed,
                });
            }
        }
        let mut scheduled_paths: HashSet<std::path::PathBuf> =
            changes.iter().map(|change| change.path.clone()).collect();
        for dirty_path in &dirty_paths {
            if !seen.contains(dirty_path) && scheduled_paths.insert(dirty_path.clone()) {
                changes.push(FileChange {
                    path: dirty_path.clone(),
                    kind: ChangeKind::Removed,
                });
            }
        }

        let added = changes
            .iter()
            .filter(|c| matches!(c.kind, ChangeKind::Modified) && !old_hashes.contains_key(&c.path))
            .count();
        let modified = changes
            .iter()
            .filter(|c| matches!(c.kind, ChangeKind::Modified) && old_hashes.contains_key(&c.path))
            .count();
        let removed = changes
            .iter()
            .filter(|c| matches!(c.kind, ChangeKind::Removed | ChangeKind::RemovedDirectory))
            .count();

        let total_changes = added + modified + removed;
        on_progress(&format!(
            "{} changes detected (added: {}, modified: {}, removed: {}), indexing",
            total_changes, added, modified, removed,
        ));

        let mut cosmetic_skipped = 0usize;
        let mut published_paths = HashSet::new();
        if !changes.is_empty() {
            let outcome = match self.apply_changes_classified(&changes, &old_signatures) {
                Ok(outcome) => outcome,
                Err(error) => {
                    self.filter_pipeline.cleanup();
                    return Err(self.abort_batch_error(error));
                }
            };
            cosmetic_skipped = outcome.cosmetic_skipped;
            if let Some(error) = outcome.failure_error() {
                self.filter_pipeline.cleanup();
                return Err(self.abort_batch_error(error));
            }
            on_progress("persisting index");
            let authoritative_hashes = merge_hashes_after_apply(
                &old_hashes,
                std::mem::take(&mut current_hashes),
                &changes,
                &outcome.successful_hashes,
            );
            let persist_result = (|| -> Result<()> {
                self.persist_checkpoint_artifacts_with_graph_state(
                    outcome.graph_semantics_changed,
                    outcome.vector_changed,
                )?;
                self.persist_signatures_after_apply(&changes, &outcome, &seen)?;
                self.fold_hash_snapshot(&authoritative_hashes)
            })();
            if let Err(error) = persist_result {
                self.restore_git_commit(previous_git_commit.as_deref())?;
                self.filter_pipeline.cleanup();
                return Err(error);
            }
            published_paths = outcome.successful_paths.clone();
        } else if had_hash_delta {
            self.ensure_working_generation()?;
            self.fold_hash_snapshot(&current_hashes)?;
        } else if skipped_by_mtime != unchanged {
            self.ensure_working_generation()?;
            self.store.save_tree_hashes_v2(&current_hashes)?;
        }

        if let Some(scanned_git_commit) = scanned_git_commit.as_deref()
            && git_head_commit(&self.config.root).as_deref() == Some(scanned_git_commit)
            && previous_git_commit.as_deref() != Some(scanned_git_commit)
        {
            self.ensure_working_generation()?;
            self.restore_git_commit(Some(scanned_git_commit))?;
        }
        if self.store.owns_rebuild_lock() {
            self.store
                .prepare_dirty_paths_for_publication(&published_paths)?;
            self.publish_generation_with_preopened_indexes()?;
            self.pending_checkpoint = ApplyChangesOutcome::default();
            if let Err(error) = self.store.clear_dirty_paths(&published_paths) {
                warn!(%error, "published progress sync journal cleanup deferred");
            }
        }
        on_progress("sync complete");

        self.filter_pipeline.cleanup();

        Ok(SyncStats {
            added,
            modified,
            removed,
            unchanged,
            cosmetic_skipped,
        })
    }

    /// Git-aware incremental sync: re-indexes only files that changed since the
    /// last indexed git commit.
    ///
    /// # Algorithm
    /// 1. Read the `git_commit` stored in `IndexMeta` (written by the last
    ///    `init` / `save` / `git_sync` that ran in a git repo).
    /// 2. Query `git rev-parse HEAD` for the current commit.
    /// 3. If they are equal the index is already up to date — return immediately.
    /// 4. Run `git diff --name-status <stored_commit>` to get the exact file
    ///    delta.
    /// 5. Convert it to [`FileChange`] events and pass them to
    ///    [`Self::apply_changes`] (single Tantivy commit, single PageRank pass).
    /// 6. Persist searchable state, merge the successful file delta into the
    ///    complete hash baseline, and record the new HEAD.
    ///
    /// # No-op conditions
    /// - git is not installed or the project is not in a git repository.
    /// - The index was created without git (no stored commit).
    /// - HEAD already equals the stored commit.
    ///
    /// In all no-op cases the method returns [`GitSyncStats::unchanged`] = `true`
    /// and skips all I/O.
    pub fn git_sync(&mut self) -> Result<GitSyncStats> {
        if self.read_only {
            return Err(CodixingError::ReadOnly);
        }
        self.symbols.ensure_mutable();
        self.migrate_graph_schema_if_outdated()?;
        use crate::watcher::{ChangeKind, FileChange};
        let pending_dirty = self.store.load_dirty_paths()?;

        // Load stored git commit from the persisted meta.
        let stored_commit = match self.store.load_meta()?.git_commit {
            Some(c) => c,
            None => {
                if !pending_dirty.is_empty() {
                    let modified = pending_dirty.iter().filter(|path| path.is_file()).count();
                    let removed = pending_dirty.len().saturating_sub(modified);
                    self.apply_changes(&[])?;
                    return Ok(GitSyncStats {
                        modified,
                        removed,
                        unchanged: false,
                    });
                }
                debug!("git_sync: no stored git commit in meta — skipping");
                return Ok(GitSyncStats {
                    unchanged: true,
                    ..Default::default()
                });
            }
        };

        // Get the current HEAD.
        let head = match git_head_commit(&self.config.root) {
            Some(h) => h,
            None => {
                if !pending_dirty.is_empty() {
                    let modified = pending_dirty.iter().filter(|path| path.is_file()).count();
                    let removed = pending_dirty.len().saturating_sub(modified);
                    self.apply_changes(&[])?;
                    return Ok(GitSyncStats {
                        modified,
                        removed,
                        unchanged: false,
                    });
                }
                debug!("git_sync: git unavailable or not a repo — skipping");
                return Ok(GitSyncStats {
                    unchanged: true,
                    ..Default::default()
                });
            }
        };

        if head == stored_commit && pending_dirty.is_empty() {
            debug!(commit = %head, "git_sync: already up-to-date");
            return Ok(GitSyncStats {
                unchanged: true,
                ..Default::default()
            });
        }

        if head == stored_commit {
            let modified = pending_dirty.iter().filter(|path| path.is_file()).count();
            let removed = pending_dirty.len().saturating_sub(modified);
            info!(
                modified,
                removed, "git_sync: recovering pending index changes"
            );
            self.apply_changes(&[])?;
            return Ok(GitSyncStats {
                modified,
                removed,
                unchanged: false,
            });
        }

        info!(from = %stored_commit, to = %head, "git_sync: computing diff");

        let (modified_paths, deleted_paths) =
            match git_diff_since(&self.config.root, &stored_commit) {
                Some(delta) => delta,
                None => {
                    if !pending_dirty.is_empty() {
                        let modified = pending_dirty.iter().filter(|path| path.is_file()).count();
                        let removed = pending_dirty.len().saturating_sub(modified);
                        self.apply_changes(&[])?;
                        return Ok(GitSyncStats {
                            modified,
                            removed,
                            unchanged: false,
                        });
                    }
                    warn!("git_sync: git diff failed — falling back to no-op");
                    return Ok(GitSyncStats {
                        unchanged: true,
                        ..Default::default()
                    });
                }
            };

        // Build FileChange list, filtering to supported source files.
        let mut changes: Vec<FileChange> = Vec::new();

        for path in &modified_paths {
            if crate::language::detect_language(path).is_some() {
                let path = self
                    .config
                    .resolve_absolute_path(path, false)
                    .ok_or_else(|| {
                        CodixingError::Config(format!(
                            "git reported a modified source path that is missing or outside the project roots: {}",
                            path.display()
                        ))
                    })?;
                let kind = if self.config.is_indexable_path(&path) {
                    ChangeKind::Modified
                } else {
                    ChangeKind::RemovedDirectory
                };
                changes.push(FileChange { path, kind });
            }
        }
        for path in &deleted_paths {
            let path = self
                .config
                .resolve_absolute_path(path, true)
                .ok_or_else(|| {
                    CodixingError::Config(format!(
                        "git reported a deleted path outside the project roots: {}",
                        path.display()
                    ))
                })?;
            changes.push(FileChange {
                path,
                kind: ChangeKind::Removed,
            });
        }

        // A previous interrupted transaction may not be represented by the git
        // diff (including when its file changed outside git). Fold that bounded
        // journal into this batch before mutation and de-duplicate by path.
        let mut scheduled: std::collections::HashSet<_> =
            changes.iter().map(|change| change.path.clone()).collect();
        for path in pending_dirty {
            if scheduled.insert(path.clone()) {
                changes.push(FileChange {
                    kind: if path.is_file() && self.config.is_indexable_path(&path) {
                        ChangeKind::Modified
                    } else {
                        ChangeKind::RemovedDirectory
                    },
                    path,
                });
            }
        }

        let n_modified = changes
            .iter()
            .filter(|c| matches!(c.kind, ChangeKind::Modified))
            .count();
        let n_removed = changes
            .iter()
            .filter(|c| matches!(c.kind, ChangeKind::Removed | ChangeKind::RemovedDirectory))
            .count();

        info!(
            modified = n_modified,
            removed = n_removed,
            "git_sync: applying changes"
        );

        if !changes.is_empty() {
            let prior_signatures = self.effective_prior_signatures();
            let outcome = match self.apply_changes_classified(&changes, &prior_signatures) {
                Ok(outcome) => outcome,
                Err(error) => return Err(self.abort_batch_error(error)),
            };
            if let Some(error) = outcome.failure_error() {
                return Err(self.abort_batch_error(error));
            }
            let persist_result = (|| -> Result<()> {
                self.persist_checkpoint_artifacts_with_graph_state(
                    outcome.graph_semantics_changed,
                    outcome.vector_changed,
                )?;

                // Publish fingerprints from the exact parse that produced the
                // searchable artifacts, never from the earlier git/scan view.
                self.persist_exact_signatures(&outcome)?;

                let successful_delta: Vec<_> = outcome
                    .successful_hashes
                    .iter()
                    .map(|(path, entry)| (path.clone(), entry.clone()))
                    .collect();
                self.store.update_tree_hash_delta(&successful_delta)?;
                self.compact_hash_delta_if_needed()
            })();
            if let Err(error) = persist_result {
                self.restore_git_commit(Some(&stored_commit))?;
                return Err(error);
            }
            self.restore_git_commit(Some(&head))?;
            self.store
                .prepare_dirty_paths_for_publication(&outcome.successful_paths)?;
            self.publish_generation_with_preopened_indexes()?;
            self.pending_checkpoint = ApplyChangesOutcome::default();
            if let Err(error) = self.store.clear_dirty_paths(&outcome.successful_paths) {
                warn!(%error, "published git sync journal cleanup deferred");
            }
        } else {
            // Diff produced no indexable changes (e.g. only docs/assets changed).
            // Still update the stored commit so next call is a true no-op.
            self.ensure_working_generation()?;
            self.restore_git_commit(Some(&head))?;
            self.publish_generation_with_preopened_indexes()?;
            self.pending_checkpoint = ApplyChangesOutcome::default();
        }

        Ok(GitSyncStats {
            modified: n_modified,
            removed: n_removed,
            unchanged: false,
        })
    }

    /// Persist current searchable state to disk.
    ///
    /// Preserves the published git commit marker. Only a completed full sync or
    /// `git_sync` may advance it; watcher/editor transactions intentionally do
    /// not claim unrelated committed files they never inspected.
    ///
    /// This method deliberately preserves the complete file-hash baseline.
    /// Only `init`, full filesystem sync, and explicit successful hash deltas
    /// have enough information to replace or update that baseline safely.
    pub fn save(&self) -> Result<()> {
        self.save_checkpoint_state(true, true)
    }

    /// Keep the immutable mmap base hard-linked for small checkpoints and
    /// publish only complete per-file replacements. Once the bounded overlay
    /// reaches either limit, fold it into a fresh base and reset the sidecar.
    fn persist_symbol_checkpoint(&self) -> Result<()> {
        let replacements = self.symbols.checkpoint_file_replacements();
        if let Some(replacements) = replacements
            && let Some(bytes) = encode_symbol_delta_checkpoint(&replacements)?
        {
            return self.store.save_symbol_delta_bytes(&bytes);
        }

        write_mmap_symbol_table(&self.symbols, &self.store.symbols_v2_path())?;
        self.store
            .save_symbol_delta_bytes(&serialize_symbol_delta(&[])?)
    }

    fn save_checkpoint_state(&self, persist_graph: bool, persist_vectors: bool) -> Result<()> {
        if self.read_only {
            return Err(CodixingError::ReadOnly);
        }
        // Public callers historically used `save()` immediately after init.
        // A published snapshot is already durable; rewriting it would violate
        // the immutable-generation invariant. Internal checkpoint callers own
        // the rebuild lock and therefore persist into unpublished W.
        if !self.store.owns_rebuild_lock() {
            return Ok(());
        }

        self.persist_symbol_checkpoint()?;
        match std::fs::remove_file(self.store.symbols_path()) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                warn!(
                    %error,
                    path = %self.store.symbols_path().display(),
                    "full-fidelity mmap symbols were saved but legacy symbols.bin could not be removed"
                );
            }
        }

        // Persist chunk_meta in compact format (without content).
        let meta_bytes = serialize_chunk_meta_compact(&self.chunk_meta)?;
        self.store.save_chunk_meta_bytes(&meta_bytes)?;

        // Persist vector index.
        if persist_vectors {
            let vec_guard = self.vector.read().unwrap_or_else(|e| e.into_inner());
            if let Some(ref vec_idx) = *vec_guard {
                vec_idx.save(
                    &self.store.vector_index_path(),
                    &self.store.file_chunks_path(),
                )?;
            }
        }

        // Persist graph.
        if persist_graph && let Some(ref g) = self.graph {
            let flat = g.to_flat();
            self.store.save_graph(&flat)?;
        }

        let stats = self.stats();
        let git_commit = self.store.load_meta()?.git_commit;
        let meta = IndexMeta {
            version: "0.3.0".to_string(),
            file_count: stats.file_count,
            chunk_count: stats.chunk_count,
            symbol_count: stats.symbol_count,
            last_indexed: unix_timestamp_string(),
            git_commit,
        };
        self.store.save_meta(&meta)?;

        Ok(())
    }

    /// Persist symbols, chunk_meta, vectors, and graph while preserving tree
    /// hashes.
    ///
    /// Use this after `reindex_file()` / `remove_file()` when the Tantivy
    /// index has already been committed: the symbol table, chunk metadata, and
    /// graph are updated in memory and need to be written to disk so that
    /// subsequent engine opens (e.g. a new MCP invocation) see the changes.
    ///
    /// Like [`Self::save`], this method does not touch the stored file-hash
    /// table, so a subsequent [`Self::sync`] can detect changes against the
    /// last authoritative baseline.
    pub fn persist_incremental(&self) -> Result<()> {
        self.save()
    }
}

#[cfg(test)]
mod incremental_batch_scale_tests {
    use super::*;
    use crate::config::IndexConfig;
    use crate::retriever::{SearchQuery, Strategy};
    use crate::watcher::{ChangeKind, FileChange};
    use std::collections::HashSet;
    use std::fs;
    use tempfile::tempdir;

    fn no_embed_config(root: &std::path::Path) -> IndexConfig {
        let mut config = IndexConfig::new(root);
        config.embedding.enabled = false;
        config
    }

    fn assert_exact_chunk_postings(engine: &Engine) {
        let posting_ids: HashSet<u64> = engine
            .file_chunk_ids
            .values()
            .flat_map(|ids| ids.iter().copied())
            .collect();
        let metadata_ids: HashSet<u64> =
            engine.chunk_meta.iter().map(|entry| *entry.key()).collect();
        assert_eq!(posting_ids, metadata_ids);
        for (path, ids) in &engine.file_chunk_ids {
            for chunk_id in ids.iter() {
                assert_eq!(
                    engine
                        .chunk_meta
                        .get(chunk_id)
                        .expect("posting must reference chunk metadata")
                        .file_path,
                    *path
                );
            }
        }
    }

    fn exact_has(engine: &Engine, needle: &str) -> bool {
        !engine
            .search(SearchQuery::new(needle).with_strategy(Strategy::Exact))
            .unwrap()
            .is_empty()
    }

    #[test]
    fn bm25_sync_releases_changed_bodies_and_hydrates_updated_content() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let source = root.join("lib.rs");
        fs::write(&source, "pub fn before_sync() { let value = 1; }\n").unwrap();

        let mut engine = Engine::init(root, no_embed_config(root)).unwrap();
        assert!(
            engine
                .chunk_meta
                .iter()
                .all(|entry| entry.value().content.is_empty())
        );

        const UPDATED: &str = "resident_sync_hydration_marker_98172";
        fs::write(
            &source,
            format!("pub fn after_sync() {{ let value = \"{UPDATED}\"; }}\n"),
        )
        .unwrap();
        let stats = engine.sync().unwrap();
        assert_eq!(stats.modified, 1);
        assert!(
            engine
                .chunk_meta
                .iter()
                .all(|entry| entry.value().content.is_empty()),
            "incremental BM25 sync must release bodies retained by resident metadata"
        );

        for strategy in [Strategy::Instant, Strategy::Exact] {
            let results = engine
                .search(
                    SearchQuery::new(UPDATED)
                        .with_limit(5)
                        .with_strategy(strategy),
                )
                .unwrap();
            assert!(
                results.iter().any(|result| {
                    result.file_path == "lib.rs" && result.content.contains(UPDATED)
                }),
                "{strategy:?} search must hydrate the updated body from Tantivy"
            );
        }

        let chunk_ids = engine
            .file_chunk_ids
            .get("lib.rs")
            .expect("updated source must retain exact chunk postings");
        assert!(chunk_ids.iter().any(|chunk_id| {
            engine
                .resolve_chunk_content(*chunk_id)
                .is_some_and(|content| content.contains(UPDATED))
        }));
    }

    #[test]
    fn completed_posthoc_vector_checkpoint_reopens_from_published_generation() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("lib.rs"), "pub fn checkpointed_vector() {}\n").unwrap();

        let engine = Engine::init(root, no_embed_config(root)).unwrap();
        assert!(
            !engine.store.owns_rebuild_lock(),
            "fresh init should expose an already-published lexical generation"
        );
        let chunk_id = engine
            .file_chunk_ids
            .get("lib.rs")
            .and_then(|chunk_ids| chunk_ids.first().copied())
            .expect("indexed source should have one chunk");

        let mut vector = VectorIndex::new(4, false).unwrap();
        vector
            .add_mut(chunk_id, &[0.1, 0.2, 0.3, 0.4], "lib.rs")
            .unwrap();
        *engine
            .vector
            .write()
            .unwrap_or_else(|error| error.into_inner()) = Some(vector);

        // `embed_remaining()` uses this completion path. It must persist even
        // though the lexical generation has no unpublished rebuild checkpoint.
        engine.checkpoint_vector_index().unwrap();
        drop(engine);

        let reopened_store = IndexStore::open_read_only(root).unwrap();
        let reopened = VectorIndex::load(
            &reopened_store.vector_index_path(),
            &reopened_store.file_chunks_path(),
            4,
            false,
        )
        .unwrap();
        assert_eq!(reopened.len(), 1);
        assert!(reopened.get_vector(chunk_id).is_some());
        assert_eq!(
            reopened.file_chunks().get("lib.rs").cloned(),
            Some(vec![chunk_id])
        );
    }

    fn remove_file_trigram_delta_manifest_marker(root: &Path) {
        let manifest_path = root.join(".codixing/active-generation.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest
            .as_object_mut()
            .unwrap()
            .remove("file_trigram_delta_required");
        fs::write(manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
    }

    #[test]
    fn consecutive_checkpoints_keep_cumulative_symbol_and_file_trigram_overlays() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let first = root.join("first.rs");
        let second = root.join("second.rs");
        fs::write(&first, "pub fn original_first() {}\n").unwrap();
        fs::write(&second, "pub fn original_second() {}\n").unwrap();

        let mut engine = Engine::init(root, no_embed_config(root)).unwrap();
        assert!(matches!(
            &engine.symbols,
            crate::symbols::SymbolTable::Mmap(_)
        ));
        assert!(engine.get_file_trigram().is_mmap_backed_for_test());
        #[cfg(unix)]
        let initial_file_trigram_inode = {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(engine.store.file_trigram_path())
                .unwrap()
                .ino()
        };

        fs::write(&first, "pub fn first_checkpoint_value() {}\n").unwrap();
        engine
            .apply_changes_deferred(&[FileChange {
                path: first.clone(),
                kind: ChangeKind::Modified,
            }])
            .unwrap();
        assert!(matches!(
            &engine.symbols,
            crate::symbols::SymbolTable::Overlay(_)
        ));
        let file_trigram = engine
            .file_trigram
            .get()
            .expect("changed file keeps the mapped trigram base");
        assert!(file_trigram.is_mmap_backed_for_test());
        assert_eq!(file_trigram.pending_delta_len_for_test(), 1);
        engine.checkpoint_pending_changes().unwrap();
        assert!(matches!(
            &engine.symbols,
            crate::symbols::SymbolTable::Overlay(_)
        ));
        let first_delta = crate::symbols::persistence::deserialize_symbol_delta(
            &engine
                .store
                .load_symbol_delta_bytes()
                .unwrap()
                .expect("first checkpoint must persist a symbol delta"),
        )
        .unwrap();
        assert_eq!(first_delta.len(), 1);
        assert_eq!(first_delta[0].0, "first.rs");
        assert_eq!(first_delta[0].1.len(), 1);
        assert_eq!(first_delta[0].1[0].name, "first_checkpoint_value");
        let file_trigram = engine
            .file_trigram
            .get()
            .expect("checkpoint reopens file trigrams");
        assert!(file_trigram.is_mmap_backed_for_test());
        assert_eq!(file_trigram.pending_delta_len_for_test(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                fs::metadata(engine.store.file_trigram_path())
                    .unwrap()
                    .ino(),
                initial_file_trigram_inode,
                "small checkpoints must retain the immutable hard-linked base"
            );
        }
        assert!(exact_has(&engine, "first_checkpoint_value"));
        assert_eq!(IndexStore::audit_layout(root).generation_count, 1);

        fs::write(&second, "pub fn second_checkpoint_value() {}\n").unwrap();
        engine
            .apply_changes_deferred(&[FileChange {
                path: second,
                kind: ChangeKind::Modified,
            }])
            .unwrap();
        assert!(matches!(
            &engine.symbols,
            crate::symbols::SymbolTable::Overlay(_)
        ));
        let file_trigram = engine
            .file_trigram
            .get()
            .expect("second batch keeps the mapped trigram base");
        assert!(file_trigram.is_mmap_backed_for_test());
        assert_eq!(file_trigram.pending_delta_len_for_test(), 2);
        engine.checkpoint_pending_changes().unwrap();
        assert!(matches!(
            &engine.symbols,
            crate::symbols::SymbolTable::Overlay(_)
        ));
        let second_delta = crate::symbols::persistence::deserialize_symbol_delta(
            &engine
                .store
                .load_symbol_delta_bytes()
                .unwrap()
                .expect("second checkpoint must persist a symbol delta"),
        )
        .unwrap();
        assert_eq!(
            second_delta
                .iter()
                .map(|(path, _)| path.as_str())
                .collect::<Vec<_>>(),
            ["first.rs", "second.rs"]
        );
        assert_eq!(second_delta[0].1.len(), 1);
        assert_eq!(second_delta[0].1[0].name, "first_checkpoint_value");
        assert_eq!(second_delta[1].1.len(), 1);
        assert_eq!(second_delta[1].1[0].name, "second_checkpoint_value");
        let file_trigram = engine
            .file_trigram
            .get()
            .expect("second checkpoint reopens file trigrams");
        assert!(file_trigram.is_mmap_backed_for_test());
        assert_eq!(file_trigram.pending_delta_len_for_test(), 2);
        assert!(exact_has(&engine, "first_checkpoint_value"));
        assert!(exact_has(&engine, "second_checkpoint_value"));
        assert!(!exact_has(&engine, "original_first"));
        assert!(!exact_has(&engine, "original_second"));
        assert_eq!(IndexStore::audit_layout(root).generation_count, 1);

        drop(engine);
        let reopened = Engine::open(root).unwrap();
        assert!(exact_has(&reopened, "first_checkpoint_value"));
        assert!(exact_has(&reopened, "second_checkpoint_value"));
        assert!(!exact_has(&reopened, "original_first"));
        assert!(!exact_has(&reopened, "original_second"));
        drop(reopened);

        let read_only = Engine::open_read_only(root).unwrap();
        assert!(exact_has(&read_only, "first_checkpoint_value"));
        assert!(exact_has(&read_only, "second_checkpoint_value"));
        assert!(!exact_has(&read_only, "original_first"));
        assert!(!exact_has(&read_only, "original_second"));
    }

    #[test]
    fn metadata_only_publication_upgrades_a_pre_sidecar_trigram_base() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("lib.rs"), "pub fn durable_target() {}\n").unwrap();
        let engine = Engine::init(root, no_embed_config(root)).unwrap();

        // Simulate a manifest written before the sidecar capability marker was
        // introduced. Its absent field deserializes as `false`, so base-only
        // compatibility is reserved for genuinely old generations.
        remove_file_trigram_delta_manifest_marker(root);
        fs::remove_file(engine.store.file_trigram_delta_path()).unwrap();
        drop(engine);
        let mut engine = Engine::open(root).unwrap();
        assert!(engine.file_trigram.get().is_none());
        engine.ensure_working_generation().unwrap();
        assert!(
            engine
                .get_file_trigram()
                .candidates_for_literal(b"durable_target")
                .unwrap()
                .contains(&"lib.rs"),
            "an inherited pre-sidecar base remains lazily loadable after the checkpoint fork"
        );

        engine.publish_generation_with_preopened_indexes().unwrap();

        assert!(engine.store.file_trigram_delta_path().is_file());
        engine.store.validate_for_publication().unwrap();
        assert_eq!(
            engine
                .get_file_trigram()
                .candidates_for_literal(b"durable_target")
                .unwrap(),
            vec!["lib.rs"]
        );
    }

    #[test]
    fn file_trigram_delta_reload_keeps_both_artifacts_absent_legacy_fallback() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("lib.rs"), "pub fn tantivy_fallback_target() {}\n").unwrap();
        let engine = Engine::init(root, no_embed_config(root)).unwrap();

        remove_file_trigram_delta_manifest_marker(root);
        fs::remove_file(engine.store.file_trigram_path()).unwrap();
        fs::remove_file(engine.store.file_trigram_delta_path()).unwrap();
        drop(engine);
        let mut engine = Engine::open(root).unwrap();
        engine.reload_from_disk().unwrap();

        assert!(exact_has(&engine, "tantivy_fallback_target"));
    }

    #[test]
    fn file_trigram_delta_corruption_disables_prefilter_and_blocks_publication() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("lib.rs"), "pub fn full_scan_target() {}\n").unwrap();
        let engine = Engine::init(root, no_embed_config(root)).unwrap();
        let sidecar = engine.store.file_trigram_delta_path();
        drop(engine);
        fs::write(&sidecar, b"corrupt").unwrap();

        let store = IndexStore::open(root).unwrap();
        assert!(store.validate_for_publication().is_err());
        drop(store);

        let reader = Engine::open_read_only(root).unwrap();
        assert!(
            reader
                .get_file_trigram()
                .candidates_for_literal(b"full_scan_target")
                .is_none()
        );
        assert!(exact_has(&reader, "full_scan_target"));
    }

    #[test]
    fn missing_required_file_trigram_delta_disables_prefilter_without_false_negatives() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("lib.rs"), "pub fn missing_sidecar_target() {}\n").unwrap();
        let engine = Engine::init(root, no_embed_config(root)).unwrap();
        let sidecar = engine.store.file_trigram_delta_path();
        drop(engine);
        fs::remove_file(sidecar).unwrap();

        let mut reload = Engine::open(root).unwrap();
        assert!(reload.reload_from_disk().is_err());
        drop(reload);

        let reader = Engine::open_read_only(root).unwrap();
        assert!(
            reader
                .get_file_trigram()
                .candidates_for_literal(b"missing_sidecar_target")
                .is_none(),
            "a missing required sidecar must never expose the stale base"
        );
        assert!(exact_has(&reader, "missing_sidecar_target"));
    }

    #[test]
    fn missing_required_file_trigram_pair_keeps_raw_grep_on_full_scan() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("analysis.ipynb"),
            r#"{"cells":[{"cell_type":"code","metadata":{},"source":["print('cell')\n"],"outputs":[],"execution_count":null}],"metadata":{"raw_only_marker":"RAW_METADATA_ONLY_TARGET"},"nbformat":4,"nbformat_minor":5}"#,
        )
        .unwrap();
        let engine = Engine::init(root, no_embed_config(root)).unwrap();
        let base = engine.store.file_trigram_path();
        let sidecar = engine.store.file_trigram_delta_path();
        drop(engine);
        fs::remove_file(base).unwrap();
        fs::remove_file(sidecar).unwrap();

        let mut reload = Engine::open(root).unwrap();
        assert!(reload.reload_from_disk().is_err());
        drop(reload);

        let reader = Engine::open_read_only(root).unwrap();
        assert!(
            reader
                .get_file_trigram()
                .candidates_for_literal(b"RAW_METADATA_ONLY_TARGET")
                .is_none(),
            "a missing required pair must disable prefiltering"
        );
        let matches = reader
            .grep_code("RAW_METADATA_ONLY_TARGET", true, None, 0, 10)
            .unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].file_path, "analysis.ipynb");
    }

    #[test]
    fn symbol_delta_file_limit_compacts_to_a_new_full_fidelity_mmap_base() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("lib.rs"), "pub fn before_compaction() {}\n").unwrap();
        let mut engine = Engine::init(root, no_embed_config(root)).unwrap();
        let mut replacement = engine.symbols.lookup("before_compaction").pop().unwrap();
        replacement.name = "after_compaction".to_string();
        replacement.signature = Some("pub fn after_compaction()".to_string());

        engine.ensure_working_generation().unwrap();
        #[cfg(unix)]
        let inherited_base_identity = {
            use std::os::unix::fs::MetadataExt;

            let metadata = fs::metadata(engine.store.symbols_v2_path()).unwrap();
            (metadata.dev(), metadata.ino())
        };
        engine.symbols.ensure_mutable();
        engine.symbols.remove_file("lib.rs");
        engine.symbols.insert(replacement);
        for index in 0..crate::symbols::persistence::SYMBOL_DELTA_COMPACT_FILES {
            engine.symbols.remove_file(&format!("virtual/{index}.rs"));
        }
        assert_eq!(
            engine.symbols.checkpoint_file_replacements().unwrap().len(),
            crate::symbols::persistence::SYMBOL_DELTA_COMPACT_FILES + 1
        );

        engine.persist_symbol_checkpoint().unwrap();

        let delta = crate::symbols::persistence::deserialize_symbol_delta(
            &engine
                .store
                .load_symbol_delta_bytes()
                .unwrap()
                .expect("compaction must publish a valid empty delta"),
        )
        .unwrap();
        assert!(delta.is_empty(), "compaction must clear every tombstone");
        let compacted =
            crate::symbols::mmap::MmapSymbolTable::load(&engine.store.symbols_v2_path()).unwrap();
        assert!(compacted.preserves_full_fidelity());
        assert!(compacted.lookup("before_compaction").is_empty());
        assert_eq!(compacted.lookup("after_compaction").len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            let metadata = fs::metadata(engine.store.symbols_v2_path()).unwrap();
            assert_ne!(
                (metadata.dev(), metadata.ino()),
                inherited_base_identity,
                "compaction must replace, not mutate, the inherited hard link"
            );
        }

        engine.publish_generation_with_preopened_indexes().unwrap();
        assert!(matches!(
            &engine.symbols,
            crate::symbols::SymbolTable::Mmap(_)
        ));
        assert!(engine.symbols.lookup("before_compaction").is_empty());
        assert_eq!(engine.symbols.lookup("after_compaction").len(), 1);
    }

    #[test]
    fn adding_first_definition_resolves_previously_unresolved_call() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let caller = root.join("caller.rs");
        let target = root.join("target.rs");
        fs::write(
            &caller,
            "pub fn caller() -> usize { late_resolution_target() }\n",
        )
        .unwrap();

        let mut engine = Engine::init(root, no_embed_config(root)).unwrap();
        assert!(engine.callees("caller.rs").is_empty());

        fs::write(&target, "pub fn late_resolution_target() -> usize { 42 }\n").unwrap();
        CASCADE_REINDEX_COUNT.with(|count| count.set(0));
        engine
            .apply_changes(&[FileChange {
                path: target,
                kind: ChangeKind::Modified,
            }])
            .unwrap();
        assert_eq!(CASCADE_REINDEX_COUNT.with(std::cell::Cell::get), 1);
        assert_eq!(engine.callees("caller.rs"), vec!["target.rs"]);

        drop(engine);
        let reopened = Engine::open_read_only(root).unwrap();
        assert_eq!(reopened.callees("caller.rs"), vec!["target.rs"]);

        let fresh_dir = tempdir().unwrap();
        fs::write(
            fresh_dir.path().join("caller.rs"),
            "pub fn caller() -> usize { late_resolution_target() }\n",
        )
        .unwrap();
        fs::write(
            fresh_dir.path().join("target.rs"),
            "pub fn late_resolution_target() -> usize { 42 }\n",
        )
        .unwrap();
        let fresh = Engine::init(fresh_dir.path(), no_embed_config(fresh_dir.path())).unwrap();
        assert_eq!(reopened.callees("caller.rs"), fresh.callees("caller.rs"));
    }

    #[test]
    fn adding_unique_definition_ignores_unchanged_common_names() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let target = root.join("target.rs");
        fs::write(&target, "pub fn new() -> usize { 0 }\n").unwrap();
        const COMMON_CALLERS: usize = 128;
        for index in 0..COMMON_CALLERS {
            fs::write(
                root.join(format!("common_caller_{index:03}.rs")),
                format!("pub fn common_caller_{index:03}() -> usize {{ new() }}\n"),
            )
            .unwrap();
        }
        fs::write(
            root.join("late_caller.rs"),
            "pub fn late_caller() -> usize { late_unique_target() }\n",
        )
        .unwrap();

        let mut engine = Engine::init(root, no_embed_config(root)).unwrap();
        assert_eq!(engine.callees("common_caller_000.rs"), vec!["target.rs"]);
        assert_eq!(
            engine
                .graph
                .as_ref()
                .unwrap()
                .get_symbol_callers("new")
                .len(),
            COMMON_CALLERS
        );
        assert!(engine.callees("late_caller.rs").is_empty());

        fs::write(
            &target,
            "pub fn new() -> usize { 0 }\npub fn late_unique_target() -> usize { 42 }\n",
        )
        .unwrap();
        CASCADE_REINDEX_COUNT.with(|count| count.set(0));
        engine
            .apply_changes(&[FileChange {
                path: target,
                kind: ChangeKind::Modified,
            }])
            .unwrap();

        assert_eq!(
            CASCADE_REINDEX_COUNT.with(std::cell::Cell::get),
            1,
            "only the caller containing the added name should refresh; unchanged `new` definitions must not fan out"
        );
        assert_eq!(
            engine
                .graph
                .as_ref()
                .unwrap()
                .get_symbol_callers("new")
                .len(),
            COMMON_CALLERS,
            "incoming references to the retained `new` node must survive the target refresh"
        );
        assert_eq!(engine.callees("common_caller_000.rs"), vec!["target.rs"]);
        assert_eq!(engine.callees("late_caller.rs"), vec!["target.rs"]);
    }

    #[test]
    fn removing_duplicate_definition_resolves_previously_ambiguous_call() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("caller.rs"),
            "pub fn caller() -> usize { ambiguity_target() }\n",
        )
        .unwrap();
        fs::write(
            root.join("target_a.rs"),
            "pub fn ambiguity_target() -> usize { 1 }\n",
        )
        .unwrap();
        let target_b = root.join("target_b.rs");
        fs::write(&target_b, "pub fn ambiguity_target() -> usize { 2 }\n").unwrap();

        let mut engine = Engine::init(root, no_embed_config(root)).unwrap();
        assert!(engine.callees("caller.rs").is_empty());

        fs::remove_file(&target_b).unwrap();
        CASCADE_REINDEX_COUNT.with(|count| count.set(0));
        engine
            .apply_changes(&[FileChange {
                path: target_b,
                kind: ChangeKind::Removed,
            }])
            .unwrap();
        assert_eq!(
            CASCADE_REINDEX_COUNT.with(std::cell::Cell::get),
            2,
            "the external caller and unchanged remaining definition must both refresh"
        );
        assert_eq!(engine.callees("caller.rs"), vec!["target_a.rs"]);

        drop(engine);
        let reopened = Engine::open_read_only(root).unwrap();
        assert_eq!(reopened.callees("caller.rs"), vec!["target_a.rs"]);

        let fresh_dir = tempdir().unwrap();
        fs::write(
            fresh_dir.path().join("caller.rs"),
            "pub fn caller() -> usize { ambiguity_target() }\n",
        )
        .unwrap();
        fs::write(
            fresh_dir.path().join("target_a.rs"),
            "pub fn ambiguity_target() -> usize { 1 }\n",
        )
        .unwrap();
        let fresh = Engine::init(fresh_dir.path(), no_embed_config(fresh_dir.path())).unwrap();
        assert_eq!(reopened.callees("caller.rs"), fresh.callees("caller.rs"));
    }

    #[test]
    fn unchanged_definition_callers_refresh_across_definition_set_changes() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        let target_a = src.join("a.rs");
        let target_b = src.join("b.rs");
        let source_a = "pub fn refresh_target() -> usize { 1 }\npub fn caller_a() -> usize { refresh_target() }\n";
        let source_b = "pub fn refresh_target() -> usize { 2 }\n";
        fs::write(&target_a, source_a).unwrap();

        let mut engine = Engine::init(root, no_embed_config(root)).unwrap();
        assert!(engine.graph.as_ref().unwrap().call_edges().is_empty());

        fs::write(&target_b, source_b).unwrap();
        CASCADE_REINDEX_COUNT.with(|count| count.set(0));
        engine
            .apply_changes(&[FileChange {
                path: target_b.clone(),
                kind: ChangeKind::Modified,
            }])
            .unwrap();
        assert_eq!(CASCADE_REINDEX_COUNT.with(std::cell::Cell::get), 1);
        assert_eq!(
            engine.graph.as_ref().unwrap().call_edges(),
            vec![("src/a.rs".to_string(), "refresh_target".to_string())],
            "the unchanged defining file must be refreshed when another definition appears"
        );

        drop(engine);
        let mut engine = Engine::open(root).unwrap();
        let added_edges = engine.graph.as_ref().unwrap().call_edges();
        assert_eq!(
            added_edges,
            vec![("src/a.rs".to_string(), "refresh_target".to_string())]
        );

        let fresh_with_duplicate = tempdir().unwrap();
        let fresh_src = fresh_with_duplicate.path().join("src");
        fs::create_dir_all(&fresh_src).unwrap();
        fs::write(fresh_src.join("a.rs"), source_a).unwrap();
        fs::write(fresh_src.join("b.rs"), source_b).unwrap();
        let fresh = Engine::init(
            fresh_with_duplicate.path(),
            no_embed_config(fresh_with_duplicate.path()),
        )
        .unwrap();
        assert_eq!(added_edges, fresh.graph.as_ref().unwrap().call_edges());

        fs::remove_file(&target_b).unwrap();
        CASCADE_REINDEX_COUNT.with(|count| count.set(0));
        engine
            .apply_changes(&[FileChange {
                path: target_b,
                kind: ChangeKind::Removed,
            }])
            .unwrap();
        assert_eq!(CASCADE_REINDEX_COUNT.with(std::cell::Cell::get), 1);
        assert!(
            engine.graph.as_ref().unwrap().call_edges().is_empty(),
            "the unchanged defining file must be refreshed when the duplicate disappears"
        );

        drop(engine);
        let reopened = Engine::open_read_only(root).unwrap();
        assert!(reopened.graph.as_ref().unwrap().call_edges().is_empty());

        let fresh_unique = tempdir().unwrap();
        let fresh_src = fresh_unique.path().join("src");
        fs::create_dir_all(&fresh_src).unwrap();
        fs::write(fresh_src.join("a.rs"), source_a).unwrap();
        let fresh =
            Engine::init(fresh_unique.path(), no_embed_config(fresh_unique.path())).unwrap();
        assert_eq!(
            reopened.graph.as_ref().unwrap().call_edges(),
            fresh.graph.as_ref().unwrap().call_edges()
        );
    }

    #[test]
    fn large_mixed_batch_builds_one_resolver_from_successful_postings() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("caller.ts"),
            "import { oldValue } from './old';\nexport const value = oldValue;\n",
        )
        .unwrap();
        fs::write(src.join("old.ts"), "export const oldValue = 1;\n").unwrap();

        let mut engine = Engine::init(root, no_embed_config(root)).unwrap();
        assert_eq!(engine.callees("src/caller.ts"), vec!["src/old.ts"]);
        let before_generation = engine.store.generation().map(str::to_owned);

        fs::remove_file(src.join("old.ts")).unwrap();
        let mut changes = vec![FileChange {
            path: src.join("old.ts"),
            kind: ChangeKind::Removed,
        }];
        for index in 0..64 {
            let path = src.join(format!("good_{index}.ts"));
            fs::write(&path, format!("export const good{index} = {index};\n")).unwrap();
            changes.push(FileChange {
                path,
                kind: ChangeKind::Modified,
            });
        }
        fs::write(
            src.join("caller.ts"),
            "import { good63 } from './good_63';\nimport failed from './failed.ipynb';\nexport const value = good63 + String(failed);\n",
        )
        .unwrap();
        changes.push(FileChange {
            path: src.join("caller.ts"),
            kind: ChangeKind::Modified,
        });
        fs::write(
            src.join("failed.ipynb"),
            r#"{"cells":[],"metadata":{},"nbformat":4,"nbformat_minor":5}"#,
        )
        .unwrap();
        changes.push(FileChange {
            path: src.join("failed.ipynb"),
            kind: ChangeKind::Modified,
        });

        IMPORT_RESOLVER_BUILD_COUNT.with(|count| count.set(0));
        assert!(
            engine.apply_changes(&changes).is_err(),
            "unsupported incremental notebook must remain a retriable failure"
        );
        assert_eq!(
            IMPORT_RESOLVER_BUILD_COUNT.with(std::cell::Cell::get),
            1,
            "one direct phase must build one resolver regardless of batch size"
        );

        assert_eq!(
            engine.store.generation().map(str::to_owned),
            before_generation
        );
        assert_eq!(engine.callees("src/caller.ts"), vec!["src/old.ts"]);
        assert!(engine.file_chunk_ids.contains_key("src/old.ts"));
        assert!(!engine.file_chunk_ids.contains_key("src/failed.ipynb"));
        assert!(!engine.file_chunk_ids.contains_key("src/good_63.ts"));
        assert_exact_chunk_postings(&engine);

        // The control-directory journal retained every path. Removing the
        // unsupported file lets the entire batch replay from active state.
        fs::remove_file(src.join("failed.ipynb")).unwrap();
        engine.apply_changes(&[]).unwrap();
        assert_eq!(
            IMPORT_RESOLVER_BUILD_COUNT.with(std::cell::Cell::get),
            2,
            "the retried direct phase must still build one resolver"
        );
        assert_eq!(engine.callees("src/caller.ts"), vec!["src/good_63.ts"]);
        assert!(!engine.file_chunk_ids.contains_key("src/old.ts"));
        assert!(!engine.file_chunk_ids.contains_key("src/failed.ipynb"));
        assert!(engine.file_chunk_ids.contains_key("src/good_63.ts"));
        assert_exact_chunk_postings(&engine);

        drop(engine);
        let reopened = Engine::open_read_only(root).unwrap();
        assert_eq!(reopened.callees("src/caller.ts"), vec!["src/good_63.ts"]);
        assert!(!reopened.file_chunk_ids.contains_key("src/old.ts"));
        assert!(!reopened.file_chunk_ids.contains_key("src/failed.ipynb"));
        assert_exact_chunk_postings(&reopened);
    }

    #[test]
    fn post_removal_failure_restores_active_state_and_replays_whole_batch() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let source = root.join("lib.rs");
        fs::write(&source, "pub fn rollback_old_value() -> usize { 1 }\n").unwrap();
        let mut engine = Engine::init(root, no_embed_config(root)).unwrap();
        let before_generation = IndexStore::active_generation(root).unwrap();

        fs::write(&source, "pub fn rollback_new_value() -> usize { 2 }\n").unwrap();
        let source = source.canonicalize().unwrap();
        FAIL_REINDEX_AFTER_REMOVE.with(|armed| armed.set(true));
        let error = engine
            .apply_changes(&[FileChange {
                path: source.clone(),
                kind: ChangeKind::Modified,
            }])
            .unwrap_err();
        assert!(error.to_string().contains("injected post-removal"));

        assert_eq!(
            IndexStore::active_generation(root).unwrap(),
            before_generation
        );
        assert!(exact_has(&engine, "rollback_old_value"));
        assert!(!exact_has(&engine, "rollback_new_value"));
        assert_eq!(engine.store.load_dirty_paths().unwrap(), vec![source]);
        assert_exact_chunk_postings(&engine);

        engine.apply_changes(&[]).unwrap();
        assert_ne!(
            IndexStore::active_generation(root).unwrap(),
            before_generation
        );
        assert!(!exact_has(&engine, "rollback_old_value"));
        assert!(exact_has(&engine, "rollback_new_value"));
        assert!(engine.store.load_dirty_paths().unwrap().is_empty());
        assert_exact_chunk_postings(&engine);
    }
}

#[cfg(test)]
mod sigfp_tests {
    //! Tests for signature-fingerprint COSMETIC/STRUCTURAL sync classification.
    //!
    //! The embedding-reuse assertions need a working embedder (ONNX runtime).
    //! When the model cannot be loaded (e.g. CI without `ORT_DYLIB_PATH`) the
    //! engine runs BM25-only and these tests skip gracefully with a printed
    //! note rather than failing — the deterministic classification logic itself
    //! is covered by the unit tests in `engine::fingerprint`.

    use super::*;
    use crate::config::IndexConfig;
    use crate::retriever::{SearchQuery, Strategy};
    use std::collections::HashMap;
    use std::fs;
    use tempfile::tempdir;

    const ORIGINAL: &str = r#"
pub fn add(a: i32, b: i32) -> i32 {
    // original comment
    a + b
}

pub struct Config {
    pub verbose: bool,
}
"#;

    /// Same signatures, different body + comment (COSMETIC).
    const BODY_ONLY: &str = r#"
pub fn add(a: i32, b: i32) -> i32 {
    // a completely different comment explaining the addition in detail
    let sum = a + b;
    sum
}

pub struct Config {
    pub verbose: bool,
}
"#;

    /// A new parameter on `add` (STRUCTURAL).
    const SIGNATURE_CHANGE: &str = r#"
pub fn add(a: i32, b: i32, c: i32) -> i32 {
    a + b + c
}

pub struct Config {
    pub verbose: bool,
}
"#;

    const MULTI_CHUNK_ORIGINAL: &str = r#"
pub fn changed(input: u32) -> u32 {
    let next = input + 1;
    let doubled = next * 2;
    doubled
}

pub fn preserved(input: u32) -> u32 {
    let next = input + 10;
    let tripled = next * 3;
    tripled
}
"#;

    const MULTI_CHUNK_STRUCTURAL_EDIT: &str = r#"
pub fn changed(input: u64) -> u64 {
    let next = input + 1;
    let doubled = next * 2;
    doubled
}

pub fn preserved(input: u32) -> u32 {
    let next = input + 10;
    let tripled = next * 3;
    tripled
}
"#;

    fn embedded_config(root: &std::path::Path) -> IndexConfig {
        let mut cfg = IndexConfig::new(root);
        cfg.embedding.enabled = true;
        cfg
    }

    fn no_embed_config(root: &std::path::Path) -> IndexConfig {
        let mut cfg = IndexConfig::new(root);
        cfg.embedding.enabled = false;
        cfg
    }

    fn write_main(root: &std::path::Path, body: &str) {
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("main.rs"), body).unwrap();
    }

    /// True when the ONNX Runtime dylib is available to load.
    ///
    /// The `ort` crate **panics** (not `Err`) on a failed dylib load, so we must
    /// probe *before* constructing an embedder. The project documents
    /// `ORT_DYLIB_PATH` for embedder use; require it to point at an existing file.
    /// When absent (e.g. Windows CI `--no-default-features`, or any box without
    /// the runtime) the embedding-reuse tests skip instead of aborting.
    fn onnx_available() -> bool {
        std::env::var_os("ORT_DYLIB_PATH")
            .map(std::path::PathBuf::from)
            .is_some_and(|p| p.exists())
    }

    /// Build an embedded engine over `src/main.rs`. Returns `None` (after printing
    /// a skip note) when the embedder is unavailable — vector-reuse assertions
    /// cannot run without it.
    fn build_embedded(root: &std::path::Path) -> Option<Engine> {
        if !onnx_available() {
            eprintln!(
                "SKIP: ONNX runtime unavailable (set ORT_DYLIB_PATH to an existing \
                 libonnxruntime) — cosmetic-reuse assertions skipped; classification \
                 logic covered by fingerprint unit tests"
            );
            return None;
        }
        let engine = Engine::init(root, embedded_config(root)).unwrap();
        if engine.embedder.is_none() {
            eprintln!(
                "SKIP: embedder unavailable (set ORT_DYLIB_PATH) — cosmetic-reuse \
                 assertions skipped; classification logic covered by fingerprint unit tests"
            );
            return None;
        }
        // init embeds in a background thread — block until vectors are populated
        // so a snapshot taken immediately after build is complete.
        engine.wait_for_embeddings();
        Some(engine)
    }

    /// Snapshot every chunk vector for `src/main.rs` keyed by stable identity key.
    fn vector_snapshot(engine: &Engine) -> HashMap<u64, Vec<f32>> {
        let mut out = HashMap::new();
        let vec_guard = engine.vector.read().unwrap_or_else(|e| e.into_inner());
        if let Some(v) = vec_guard.as_ref() {
            for entry in engine.chunk_meta.iter() {
                let meta = entry.value();
                if meta.file_path == "src/main.rs"
                    && let Some(key) = chunk_stable_key(&meta.scope_chain, &meta.entity_names)
                    && let Some(vec) = v.get_vector(meta.chunk_id)
                {
                    out.insert(key, vec);
                }
            }
        }
        out
    }

    fn vector_for_entity(engine: &Engine, entity: &str) -> Option<Vec<f32>> {
        let vec_guard = engine.vector.read().unwrap_or_else(|e| e.into_inner());
        let vector = vec_guard.as_ref()?;
        engine.chunk_meta.iter().find_map(|entry| {
            let meta = entry.value();
            (meta.file_path == "src/main.rs" && meta.entity_names.iter().any(|name| name == entity))
                .then(|| vector.get_vector(meta.chunk_id))
                .flatten()
        })
    }

    #[test]
    fn cosmetic_edit_reuses_embeddings() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_main(root, ORIGINAL);

        let mut engine = match build_embedded(root) {
            Some(e) => e,
            None => return,
        };

        let before = vector_snapshot(&engine);
        assert!(!before.is_empty(), "expected at least one embedded chunk");

        // Edit body + comment only — signatures unchanged.
        write_main(root, BODY_ONLY);
        let stats = engine.sync().unwrap();

        assert_eq!(
            stats.cosmetic_skipped, 1,
            "body-only edit must be classified COSMETIC and reuse embeddings"
        );

        // The reused vectors must be (near-)identical to the pre-edit vectors.
        // We use cosine similarity rather than byte equality because the HNSW
        // backend may round-trip f32 with negligible noise; a genuine re-embed
        // of the changed body would shift the vector far more than that.
        let after = vector_snapshot(&engine);
        let mut compared = 0usize;
        for (key, old_vec) in &before {
            if let Some(new_vec) = after.get(key) {
                let sim = cosine(old_vec, new_vec);
                // HNSW f32 round-trip introduces ~1e-3 noise; a real re-embed of
                // the changed body drops cosine to ~0.98 (see signature test).
                assert!(
                    sim > 0.999,
                    "COSMETIC reuse must keep the embedding vector; cosine={sim}"
                );
                compared += 1;
            }
        }
        assert!(
            compared > 0,
            "expected to compare at least one reused vector"
        );
    }

    /// Cosine similarity between two equal-length vectors.
    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if na == 0.0 || nb == 0.0 {
            return 0.0;
        }
        dot / (na * nb)
    }

    #[test]
    fn signature_change_reembeds() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_main(root, ORIGINAL);

        let mut engine = match build_embedded(root) {
            Some(e) => e,
            None => return,
        };

        let before = vector_snapshot(&engine);

        // Add a parameter to `add` — STRUCTURAL.
        write_main(root, SIGNATURE_CHANGE);
        let stats = engine.sync().unwrap();

        assert_eq!(
            stats.cosmetic_skipped, 0,
            "a parameter change must be classified STRUCTURAL (re-embed)"
        );

        // The function chunk's vector must have genuinely changed (re-embedded).
        // Its stable key (entity name `add`) is identical across the edit, so a
        // materially different vector proves a real re-embed, not reuse.
        let after = vector_snapshot(&engine);
        let mut any_reembedded = false;
        let mut compared = 0usize;
        for (key, old_vec) in &before {
            if let Some(new_vec) = after.get(key) {
                compared += 1;
                // A genuine re-embed shifts cosine well below the HNSW noise
                // floor (~0.98 measured vs ~0.9997 for pure round-trip).
                if cosine(old_vec, new_vec) < 0.99 {
                    any_reembedded = true;
                }
            }
        }
        // Guard against silent pass if no chunk was comparable across the edit.
        assert!(compared > 0, "expected to compare at least one chunk");
        assert!(
            any_reembedded,
            "STRUCTURAL change must re-embed at least one chunk (vector must differ)"
        );
    }

    #[test]
    fn reopened_contextual_index_reuses_unchanged_chunk_on_structural_edit() {
        if !onnx_available() {
            eprintln!("SKIP: ONNX runtime unavailable — reopen vector-reuse assertion skipped");
            return;
        }

        let dir = tempdir().unwrap();
        let root = dir.path();
        write_main(root, MULTI_CHUNK_ORIGINAL);
        let mut config = embedded_config(root);
        config.chunk.max_chars = 96;
        config.chunk.min_chars = 1;
        config.chunk.overlap_ratio = 0.0;

        let engine = Engine::init(root, config).unwrap();
        if engine.embedder.is_none() {
            eprintln!("SKIP: embedder unavailable — reopen vector-reuse assertion skipped");
            return;
        }
        engine.wait_for_embeddings();
        assert!(
            engine
                .chunk_meta
                .iter()
                .filter(|entry| entry.value().file_path == "src/main.rs")
                .count()
                >= 2,
            "fixture must split the file so one chunk can remain unchanged"
        );
        drop(engine);

        let mut reopened = Engine::open(root).unwrap();
        assert!(
            reopened
                .chunk_meta
                .iter()
                .all(|entry| entry.value().content.is_empty()),
            "reopen must exercise compact metadata without hydrated bodies"
        );
        let before = vector_for_entity(&reopened, "preserved")
            .expect("preserved chunk must have a vector before the edit");

        write_main(root, MULTI_CHUNK_STRUCTURAL_EDIT);
        let stats = reopened.sync().unwrap();
        assert_eq!(stats.cosmetic_skipped, 0, "signature edit is structural");

        let after = vector_for_entity(&reopened, "preserved")
            .expect("preserved chunk must have a vector after the edit");
        assert!(
            cosine(&before, &after) > 0.999,
            "an unchanged contextual chunk must reuse its pre-reopen vector"
        );
    }

    #[test]
    fn old_index_without_signatures_is_structural() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_main(root, ORIGINAL);

        let mut engine = match build_embedded(root) {
            Some(e) => e,
            None => return,
        };

        // Simulate a pre-feature index: the signature sidecar does not exist.
        // (init does not write it; sync writes it on the first run.) Ensure it
        // is absent before the first sync.
        let sig_path = engine.store.tree_signatures_path();
        let _ = fs::remove_file(&sig_path);
        assert!(
            !sig_path.exists(),
            "precondition: signature sidecar must be absent (old index)"
        );

        // A body-only edit on an index with no stored fingerprints must be
        // treated as STRUCTURAL — there is nothing to compare against, so we
        // must NOT reuse (which could otherwise reuse a stale vector).
        write_main(root, BODY_ONLY);
        let stats = engine.sync().unwrap();
        assert_eq!(
            stats.cosmetic_skipped, 0,
            "first sync on a pre-feature index must be STRUCTURAL, not COSMETIC"
        );

        // The sidecar must now have been written, so the *next* equivalent edit
        // can be classified COSMETIC.
        assert!(
            sig_path.exists(),
            "sync must write the signature sidecar for subsequent syncs"
        );
    }

    #[test]
    fn structural_then_cosmetic_transition() {
        // After a STRUCTURAL change (which rewrites the signature baseline), a
        // subsequent body-only edit on the new structure must be COSMETIC.
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_main(root, ORIGINAL);

        let mut engine = match build_embedded(root) {
            Some(e) => e,
            None => return,
        };

        // A signature change: STRUCTURAL, and it refreshes the stored fingerprint
        // to match the new (3-arg) signature.
        write_main(root, SIGNATURE_CHANGE);
        let first = engine.sync().unwrap();
        assert_eq!(
            first.cosmetic_skipped, 0,
            "parameter change must be STRUCTURAL"
        );

        // Now edit only the body of the 3-arg `add` — signatures unchanged vs the
        // new baseline → COSMETIC.
        let body_edit_3arg = r#"
pub fn add(a: i32, b: i32, c: i32) -> i32 {
    // reworked body, identical signature
    let total = a + b;
    total + c
}

pub struct Config {
    pub verbose: bool,
}
"#;
        write_main(root, body_edit_3arg);
        let second = engine.sync().unwrap();
        assert_eq!(
            second.cosmetic_skipped, 1,
            "body-only edit on the refreshed baseline must be COSMETIC"
        );
    }

    #[test]
    fn direct_reindex_persists_exact_signature_sidecar() {
        // BM25-only engine (no ONNX needed): init still records signature
        // fingerprints. A direct `reindex_file` (CLI `update --file`, MCP/LSP/
        // server writes) must replace the old fingerprint with the one computed
        // from the exact bytes that produced the searchable artifacts.
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_main(root, "pub fn f(a: u32) -> u32 { a + 1 }\n");
        let mut engine = Engine::init(root, IndexConfig::new(root)).unwrap();

        let key = std::path::PathBuf::from("src/main.rs");
        let before = engine.store.load_tree_signatures().unwrap();
        let before_signature = before
            .iter()
            .find_map(|(path, signature)| (*path == key).then_some(*signature))
            .expect("init should record a signature fingerprint for src/main.rs");

        let path = root.join("src/main.rs");
        let changed = "pub fn f(a: u64) -> u64 { a + 1 }\n";
        write_main(root, changed);
        engine.reindex_file(&path).unwrap();

        let after = engine.store.load_tree_signatures().unwrap();
        let after_signature = after
            .iter()
            .find_map(|(path, signature)| (*path == key).then_some(*signature))
            .expect("direct reindex should publish the newly parsed signature fingerprint");
        assert_ne!(
            after_signature, before_signature,
            "a signature-changing direct reindex must replace the old fingerprint"
        );

        let parsed = engine.parser.parse_file(&path, changed.as_bytes()).unwrap();
        let expected = super::super::fingerprint::signature_fingerprint(
            &parsed.entities,
            changed.as_bytes(),
            parsed.language,
        )
        .expect("the changed Rust function should have a fingerprint");
        assert_eq!(
            after_signature, expected,
            "the sidecar must match the exact bytes used to build the index"
        );

        // The freshly published value is immediately useful: a later body-only
        // edit should classify as cosmetic rather than forcing structural work.
        let body_edit = "pub fn f(a: u64) -> u64 { let n = a + 1; n }\n";
        write_main(root, body_edit);
        let old_signatures: HashMap<_, _> = after.into_iter().collect();
        let parsed = engine
            .parser
            .parse_file(&path, body_edit.as_bytes())
            .unwrap();
        let body_signature = super::super::fingerprint::signature_fingerprint(
            &parsed.entities,
            body_edit.as_bytes(),
            parsed.language,
        )
        .unwrap();
        assert_eq!(body_signature, after_signature);
        assert_eq!(
            old_signatures.get(&std::path::PathBuf::from("src/main.rs")),
            Some(&body_signature)
        );
    }

    #[test]
    fn fresh_bm25_reindex_updates_exact_search() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let path = root.join("src/main.rs");
        write_main(root, "pub fn marker() { let _ = \"OLDMARKERABC\"; }\n");

        let mut engine = Engine::init(root, IndexConfig::new(root)).unwrap();
        assert!(
            engine
                .chunk_meta
                .iter()
                .all(|entry| entry.value().content.is_empty()),
            "BM25 init should retain only compact chunk metadata"
        );
        assert!(
            !engine
                .search(SearchQuery::new("OLDMARKERABC").with_strategy(Strategy::Exact))
                .unwrap()
                .is_empty()
        );

        write_main(root, "pub fn marker() { let _ = \"NEWMARKERXYZ\"; }\n");
        engine.reindex_file(&path).unwrap();

        assert!(
            engine
                .search(SearchQuery::new("OLDMARKERABC").with_strategy(Strategy::Exact))
                .unwrap()
                .is_empty(),
            "reindex must remove the compact old chunk from exact search"
        );
        assert!(
            !engine
                .search(SearchQuery::new("NEWMARKERXYZ").with_strategy(Strategy::Exact))
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    fn inode_identity(path: &std::path::Path) -> (u64, u64) {
        use std::os::unix::fs::MetadataExt;

        let metadata = fs::metadata(path).unwrap();
        (metadata.dev(), metadata.ino())
    }

    #[cfg(unix)]
    fn optional_inode_identity(path: &std::path::Path) -> Option<(u64, u64)> {
        path.is_file().then(|| inode_identity(path))
    }

    #[cfg(unix)]
    fn vector_artifact_identities(
        engine: &Engine,
    ) -> std::collections::BTreeMap<std::ffi::OsString, (u64, u64)> {
        use std::os::unix::fs::MetadataExt;

        fs::read_dir(engine.store.vectors_dir())
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let metadata = entry.metadata().ok()?;
                metadata
                    .is_file()
                    .then(|| (entry.file_name(), (metadata.dev(), metadata.ino())))
            })
            .collect()
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn no_embed_sync_retains_vectors_but_removal_persists_vector_changes() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let source = root.join("src/main.rs");
        write_main(root, "pub fn vector_marker() -> usize { 1 }\n");
        let mut engine = Engine::init(root, no_embed_config(root)).unwrap();
        let original_chunk_id = engine.file_chunk_ids["src/main.rs"][0];

        engine.ensure_working_generation().unwrap();
        let mut vector = VectorIndex::new(2, false).unwrap();
        vector
            .add_mut(original_chunk_id, &[1.0, 0.0], "src/main.rs")
            .unwrap();
        *engine
            .vector
            .write()
            .unwrap_or_else(|error| error.into_inner()) = Some(vector);
        engine.save_checkpoint_state(false, true).unwrap();
        engine.publish_generation_with_preopened_indexes().unwrap();

        let before = vector_artifact_identities(&engine);
        assert!(
            !before.is_empty(),
            "the fixture must publish vector artifacts"
        );

        write_main(root, "pub fn vector_marker() -> usize { 2 }\n");
        engine
            .sync_with_options(
                SyncOptions {
                    skip_embed: true,
                    rebuild_graph: false,
                },
                |_| {},
            )
            .unwrap();
        assert_eq!(
            vector_artifact_identities(&engine),
            before,
            "a no-embed content sync must retain the hard-linked vector generation"
        );
        let preserved = VectorIndex::load(
            &engine.store.vector_index_path(),
            &engine.store.file_chunks_path(),
            2,
            false,
        )
        .unwrap();
        assert_eq!(
            preserved.file_chunks().get("src/main.rs"),
            Some(&vec![original_chunk_id])
        );
        assert_eq!(
            preserved.get_vector(original_chunk_id),
            Some(vec![1.0, 0.0])
        );

        fs::remove_file(&source).unwrap();
        engine
            .sync_with_options(
                SyncOptions {
                    skip_embed: true,
                    rebuild_graph: false,
                },
                |_| {},
            )
            .unwrap();
        assert_ne!(
            vector_artifact_identities(&engine),
            before,
            "removing a file with vectors must publish a new vector generation"
        );
        assert!(
            engine
                .vector
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .as_ref()
                .is_some_and(|index| !index.file_chunks().contains_key("src/main.rs"))
        );
        let persisted = VectorIndex::load(
            &engine.store.vector_index_path(),
            &engine.store.file_chunks_path(),
            2,
            false,
        )
        .unwrap();
        assert!(!persisted.file_chunks().contains_key("src/main.rs"));
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn graph_neutral_checkpoint_retains_graph_sidecars_and_reopens_new_lexical_state() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_main(
            root,
            "pub fn marker() -> u32 { let _ = \"OLD_NEUTRAL_TOKEN\"; 1 }\n",
        );
        let mut engine = Engine::init(root, IndexConfig::new(root)).unwrap();

        let before_graph = inode_identity(&engine.store.graph_path());
        let before_symbol_graph = inode_identity(&engine.store.symbol_graph_path());
        let before_concepts = optional_inode_identity(&engine.store.concepts_path());
        let before_reformulations = optional_inode_identity(&engine.store.reformulations_path());

        write_main(
            root,
            "pub fn marker() -> u32 { let _ = \"NEW_NEUTRAL_TOKEN\"; 2 }\n",
        );
        engine.sync().unwrap();

        assert_eq!(before_graph, inode_identity(&engine.store.graph_path()));
        assert_eq!(
            before_symbol_graph,
            inode_identity(&engine.store.symbol_graph_path())
        );
        assert_eq!(
            before_concepts,
            optional_inode_identity(&engine.store.concepts_path())
        );
        assert_eq!(
            before_reformulations,
            optional_inode_identity(&engine.store.reformulations_path())
        );
        assert!(
            !engine.store.tree_hashes_path().exists(),
            "current checkpoints must not recreate the legacy v1 hash snapshot"
        );

        drop(engine);
        let reopened = Engine::open(root).unwrap();
        assert!(
            !reopened.store.tree_hashes_path().exists(),
            "reopen must not migrate a current v2 snapshot back to legacy v1"
        );
        assert!(
            reopened
                .search(SearchQuery::new("OLD_NEUTRAL_TOKEN").with_strategy(Strategy::Exact))
                .unwrap()
                .is_empty()
        );
        assert!(
            !reopened
                .search(SearchQuery::new("NEW_NEUTRAL_TOKEN").with_strategy(Strategy::Exact))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn cosmetic_edit_to_common_hub_does_not_reindex_callers() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        let hub = src.join("hub.rs");
        fs::write(&hub, "pub fn shared_value() -> usize { 1 }\n").unwrap();
        const CALLERS: usize = 128;
        for index in 0..CALLERS {
            fs::write(
                src.join(format!("caller_{index}.rs")),
                format!(
                    "use crate::hub::shared_value;\npub fn value_{index}() -> usize {{ shared_value() }}\n"
                ),
            )
            .unwrap();
        }

        let mut engine = Engine::init(root, no_embed_config(root)).unwrap();
        assert_eq!(
            engine.graph.as_ref().unwrap().caller_count("src/hub.rs"),
            CALLERS,
            "fixture must exercise a genuinely high-fan-in indexed file"
        );

        fs::write(
            &hub,
            "pub fn shared_value() -> usize { let value = 2; value }\n",
        )
        .unwrap();
        CASCADE_REINDEX_COUNT.with(|count| count.set(0));
        engine.sync().unwrap();

        assert_eq!(
            CASCADE_REINDEX_COUNT.with(std::cell::Cell::get),
            0,
            "an unchanged resolution-key set must not cascade across hub fan-in"
        );
        assert_eq!(
            engine.graph.as_ref().unwrap().caller_count("src/hub.rs"),
            CALLERS,
            "skipping the cascade must retain existing incoming graph edges"
        );
    }

    #[test]
    fn embedding_signature_change_does_not_invalidate_stable_resolution_keys() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        let hub = src.join("hub.rs");
        fs::write(&hub, "pub fn shared_value(input: u32) -> u32 { input }\n").unwrap();
        fs::write(
            src.join("caller.rs"),
            "pub fn caller() -> u32 { shared_value(1) }\n",
        )
        .unwrap();
        let mut engine = Engine::init(root, no_embed_config(root)).unwrap();
        assert_eq!(engine.graph.as_ref().unwrap().caller_count("src/hub.rs"), 1);

        // Rust's embedding fingerprint includes the type signature, but call
        // target resolution depends on the exported name. The former must
        // trigger re-embedding without forcing an unrelated caller reindex.
        fs::write(&hub, "pub fn shared_value(input: u64) -> u64 { input }\n").unwrap();
        CASCADE_REINDEX_COUNT.with(|count| count.set(0));
        engine
            .apply_changes(&[crate::watcher::FileChange {
                path: hub,
                kind: crate::watcher::ChangeKind::Modified,
            }])
            .unwrap();
        assert_eq!(
            CASCADE_REINDEX_COUNT.with(std::cell::Cell::get),
            0,
            "embedding signatures must not drive dependency invalidation"
        );
        assert_eq!(engine.graph.as_ref().unwrap().caller_count("src/hub.rs"), 1);
    }

    #[test]
    fn body_only_edits_preserve_callers_without_cascades_across_languages() {
        let cases = [
            (
                "rs",
                "shared_value",
                "pub fn shared_value() -> usize { 1 }\n",
                "pub fn shared_value() -> usize { let value = 2; value }\n",
                "pub fn caller() -> usize { shared_value() }\n",
            ),
            (
                "ts",
                "sharedValue",
                "export function sharedValue(): number { return 1; }\n",
                "export function sharedValue(): number { const value = 2; return value; }\n",
                "export function caller(): number { return sharedValue(); }\n",
            ),
            (
                "py",
                "shared_value",
                "def shared_value():\n    return 1\n",
                "def shared_value():\n    value = 2\n    return value\n",
                "def caller():\n    return shared_value()\n",
            ),
        ];

        for (extension, symbol, original, edited, caller_source) in cases {
            let dir = tempdir().unwrap();
            let root = dir.path();
            let src = root.join("src");
            fs::create_dir_all(&src).unwrap();
            let hub = src.join(format!("hub.{extension}"));
            let caller = src.join(format!("caller.{extension}"));
            fs::write(&hub, original).unwrap();
            fs::write(&caller, caller_source).unwrap();

            let hub_rel = format!("src/hub.{extension}");
            let caller_rel = format!("src/caller.{extension}");
            let mut engine = Engine::init(root, no_embed_config(root)).unwrap();
            assert_eq!(
                engine.graph.as_ref().unwrap().caller_count(&hub_rel),
                1,
                "{extension} fixture must start with one resolved file caller"
            );
            assert!(
                engine
                    .graph
                    .as_ref()
                    .unwrap()
                    .get_symbol_callers(symbol)
                    .iter()
                    .any(|(file, name)| file == &caller_rel && name == "caller"),
                "{extension} fixture must start with a cross-file symbol reference"
            );

            fs::write(&hub, edited).unwrap();
            CASCADE_REINDEX_COUNT.with(|count| count.set(0));
            engine
                .apply_changes(&[crate::watcher::FileChange {
                    path: hub.clone(),
                    kind: crate::watcher::ChangeKind::Modified,
                }])
                .unwrap();

            assert_eq!(
                CASCADE_REINDEX_COUNT.with(std::cell::Cell::get),
                0,
                "{extension} body-only edit must not reindex callers"
            );
            assert_eq!(
                engine.graph.as_ref().unwrap().caller_count(&hub_rel),
                1,
                "{extension} body-only edit must retain the file caller"
            );
            let live_callers = engine.graph.as_ref().unwrap().get_symbol_callers(symbol);
            assert!(
                live_callers
                    .iter()
                    .any(|(file, name)| file == &caller_rel && name == "caller"),
                "{extension} live graph must retain the incoming symbol reference"
            );

            drop(engine);
            let reopened = Engine::open(root).unwrap();
            let reopened_callers = reopened.graph.as_ref().unwrap().get_symbol_callers(symbol);
            assert_eq!(
                reopened_callers, live_callers,
                "{extension} live and reopened symbol graphs must agree"
            );
        }
    }

    #[test]
    fn deferred_batches_use_pending_exact_signature_as_the_next_baseline() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let path = root.join("src/main.rs");
        write_main(root, "pub fn value(input: u32) -> u32 { input + 1 }\n");
        let mut engine = Engine::init(root, no_embed_config(root)).unwrap();
        let relative = std::path::PathBuf::from("src/main.rs");
        let persisted_before: HashMap<_, _> = engine
            .store
            .load_tree_signatures()
            .unwrap()
            .into_iter()
            .collect();
        let old_signature = persisted_before[&relative];

        write_main(root, "pub fn value(input: u64) -> u64 { input + 1 }\n");
        engine
            .apply_changes_deferred(&[crate::watcher::FileChange {
                path: path.clone(),
                kind: crate::watcher::ChangeKind::Modified,
            }])
            .unwrap();
        let pending_signature = engine
            .pending_checkpoint
            .successful_signatures
            .iter()
            .find_map(|(path, signature)| {
                (engine.config.normalize_path(path).as_deref() == Some("src/main.rs"))
                    .then_some(*signature)
                    .flatten()
            })
            .expect("first deferred parse must retain its exact signature");
        assert_ne!(pending_signature, old_signature);
        let persisted_mid_batch: HashMap<_, _> = engine
            .store
            .load_tree_signatures()
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(
            persisted_mid_batch[&relative], old_signature,
            "the unpublished working generation must not rewrite the active baseline"
        );
        assert_eq!(
            engine.effective_prior_signatures()[&relative],
            pending_signature,
            "the resident pending parse must overlay the persisted baseline"
        );

        write_main(
            root,
            "pub fn value(input: u64) -> u64 { let next = input + 1; next }\n",
        );
        EXACT_SIGNATURE_MATCH_COUNT.with(|count| count.set(0));
        engine
            .apply_changes_deferred(&[crate::watcher::FileChange {
                path,
                kind: crate::watcher::ChangeKind::Modified,
            }])
            .unwrap();
        assert_eq!(
            EXACT_SIGNATURE_MATCH_COUNT.with(std::cell::Cell::get),
            1,
            "the second editor batch must compare against the first pending exact parse"
        );
        engine.checkpoint_pending_changes().unwrap();
    }

    #[test]
    fn direct_batch_call_resolution_is_independent_of_caller_callee_order() {
        for caller_first in [true, false] {
            let dir = tempdir().unwrap();
            let root = dir.path();
            let src = root.join("src");
            fs::create_dir_all(&src).unwrap();
            let caller = src.join("main.rs");
            let callee = src.join("callee.rs");
            fs::write(&caller, "pub fn caller() { old_target(); }\n").unwrap();
            fs::write(&callee, "pub fn old_target() {}\n").unwrap();

            let mut engine = Engine::init(root, no_embed_config(root)).unwrap();
            assert!(
                engine
                    .callees("src/main.rs")
                    .contains(&"src/callee.rs".to_string()),
                "fixture must start with a resolved call edge"
            );
            assert!(
                engine
                    .graph
                    .as_ref()
                    .unwrap()
                    .get_symbol_callers("old_target")
                    .iter()
                    .any(|(file, name)| file == "src/main.rs" && name == "caller"),
                "fixture must start with a resolved symbol edge"
            );

            fs::write(&caller, "pub fn caller() { new_target(); }\n").unwrap();
            fs::write(&callee, "pub fn new_target() {}\n").unwrap();
            engine.ensure_working_generation().unwrap();
            engine.symbols.ensure_mutable();
            let _ = engine.get_file_trigram();
            let ordered = if caller_first {
                [&caller, &callee]
            } else {
                [&callee, &caller]
            };
            let mut graph_updates = Vec::new();
            for path in ordered {
                let mut mutation = engine.reindex_file_impl(path, None).unwrap();
                graph_updates.push(
                    mutation
                        .import_update
                        .take()
                        .expect("graph-enabled reindex must defer its graph update"),
                );
            }
            engine.apply_import_graph_updates(graph_updates);
            assert!(
                engine
                    .callees("src/main.rs")
                    .contains(&"src/callee.rs".to_string()),
                "final symbols must resolve calls with caller_first={caller_first}"
            );
            assert!(
                engine
                    .graph
                    .as_ref()
                    .unwrap()
                    .get_symbol_callers("new_target")
                    .iter()
                    .any(|(file, name)| file == "src/main.rs" && name == "caller"),
                "symbol resolution must use the completed batch with caller_first={caller_first}"
            );
            assert!(
                engine
                    .graph
                    .as_ref()
                    .unwrap()
                    .get_symbol_callers("old_target")
                    .is_empty(),
                "stale symbol edges must be removed with caller_first={caller_first}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn body_call_change_rebuilds_graph_sidecars_and_survives_reopen() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.rs"), "pub fn callee_a() {}\n").unwrap();
        fs::write(src.join("b.rs"), "pub fn callee_b() {}\n").unwrap();
        write_main(root, "pub fn caller() { callee_a(); }\n");
        let mut engine = Engine::init(root, IndexConfig::new(root)).unwrap();
        let before_graph = inode_identity(&engine.store.graph_path());
        let before_symbol_graph = inode_identity(&engine.store.symbol_graph_path());
        assert!(
            engine
                .graph
                .as_ref()
                .unwrap()
                .callees("src/main.rs")
                .contains(&"src/a.rs".to_string())
        );

        write_main(root, "pub fn caller() { callee_b(); }\n");
        engine.sync().unwrap();
        assert_ne!(before_graph, inode_identity(&engine.store.graph_path()));
        assert_ne!(
            before_symbol_graph,
            inode_identity(&engine.store.symbol_graph_path())
        );

        drop(engine);
        let reopened = Engine::open(root).unwrap();
        let callees = reopened.graph.as_ref().unwrap().callees("src/main.rs");
        assert!(callees.contains(&"src/b.rs".to_string()));
        assert!(!callees.contains(&"src/a.rs".to_string()));
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn removing_target_only_file_detects_incoming_graph_state() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        write_main(root, "mod target;\npub fn caller() {}\n");
        let target = src.join("target.rs");
        fs::write(&target, "// target-only module\n").unwrap();
        let mut engine = Engine::init(root, IndexConfig::new(root)).unwrap();
        let before_graph = inode_identity(&engine.store.graph_path());
        assert!(
            engine
                .graph
                .as_ref()
                .unwrap()
                .callees("src/main.rs")
                .contains(&"src/target.rs".to_string())
        );

        fs::remove_file(&target).unwrap();
        engine.sync().unwrap();
        assert_ne!(before_graph, inode_identity(&engine.store.graph_path()));

        drop(engine);
        let reopened = Engine::open(root).unwrap();
        assert!(
            !reopened
                .graph
                .as_ref()
                .unwrap()
                .callees("src/main.rs")
                .contains(&"src/target.rs".to_string())
        );
    }

    #[test]
    fn fresh_bm25_remove_file_updates_exact_search() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let path = root.join("src/main.rs");
        write_main(root, "pub fn marker() { let _ = \"DELETEMARKER\"; }\n");

        let mut engine = Engine::init(root, IndexConfig::new(root)).unwrap();
        assert!(
            !engine
                .search(SearchQuery::new("DELETEMARKER").with_strategy(Strategy::Exact))
                .unwrap()
                .is_empty()
        );

        engine.remove_file(&path).unwrap();

        assert!(
            engine
                .search(SearchQuery::new("DELETEMARKER").with_strategy(Strategy::Exact))
                .unwrap()
                .is_empty(),
            "file removal must remove compact metadata from exact search"
        );
    }

    #[test]
    fn reindex_replaces_parser_transformed_file_trigrams() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let path = root.join("src/main.rs");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            b"pub fn marker() { let _ = \"OLD\xffTRANSFORMED\"; }\n",
        )
        .unwrap();

        let mut engine = Engine::init(root, IndexConfig::new(root)).unwrap();
        let old_query = "OLD\u{FFFD}TRANSFORMED";
        assert_eq!(
            engine
                .get_file_trigram()
                .candidates_for_literal(old_query.as_bytes())
                .unwrap(),
            vec!["src/main.rs"]
        );

        fs::write(
            &path,
            b"pub fn marker() { let _ = \"NEW\xffTRANSFORMED\"; }\n",
        )
        .unwrap();
        engine.reindex_file(&path).unwrap();

        assert!(
            engine
                .get_file_trigram()
                .candidates_for_literal(old_query.as_bytes())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            engine
                .get_file_trigram()
                .candidates_for_literal("NEW\u{FFFD}TRANSFORMED".as_bytes())
                .unwrap(),
            vec!["src/main.rs"]
        );
    }

    #[test]
    fn failed_sync_batch_keeps_every_authoritative_hash_on_active_generation() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let rust_path = root.join("src/main.rs");
        let notebook_path = root.join("analysis.ipynb");
        write_main(root, "pub fn version() -> u32 { 1 }\n");
        fs::write(
            &notebook_path,
            r#"{
                "nbformat": 4,
                "metadata": {"kernelspec": {"language": "python"}},
                "cells": [{"cell_type": "code", "id": "one", "source": "def value():\n    return 1\n"}]
            }"#,
        )
        .unwrap();

        let mut config = IndexConfig::new(root);
        config.embedding.enabled = false;
        let mut engine = Engine::init(root, config).unwrap();
        let before: HashMap<_, _> = engine
            .store
            .load_tree_hashes_v2()
            .unwrap()
            .into_iter()
            .collect();
        let rust_path = rust_path.canonicalize().unwrap();
        let notebook_path = notebook_path.canonicalize().unwrap();

        write_main(root, "pub fn version() -> u32 { 222 }\n");
        fs::write(
            &notebook_path,
            r#"{
                "nbformat": 4,
                "metadata": {"kernelspec": {"language": "python"}},
                "cells": [{"cell_type": "code", "id": "one", "source": "def value_with_a_longer_name():\n    return 222\n"}]
            }"#,
        )
        .unwrap();

        let error = engine
            .sync()
            .expect_err("notebook update must remain pending");
        assert!(error.to_string().contains("notebook incremental sync"));

        let after: HashMap<_, _> = engine
            .store
            .load_tree_hashes_v2()
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(
            after[&rust_path].content_hash, before[&rust_path].content_hash,
            "a successful sibling must roll back with the poisoned batch"
        );
        assert_eq!(
            after[&notebook_path].content_hash, before[&notebook_path].content_hash,
            "a failed update must retain its previous authoritative hash"
        );

        let retry = engine
            .sync()
            .expect_err("failed update must be detected again");
        assert!(retry.to_string().contains("notebook incremental sync"));
    }

    #[cfg(unix)]
    #[test]
    fn direct_index_mutations_reject_symlink_escape() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let root = dir.path().join("repo");
        let outside = dir.path().join("outside.rs");
        let indexed = root.join("src/main.rs");
        write_main(&root, "pub fn inside() {}\n");
        fs::write(&outside, "pub fn outside_secret() {}\n").unwrap();

        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;
        let mut engine = Engine::init(&root, config).unwrap();
        fs::remove_file(&indexed).unwrap();
        symlink(&outside, &indexed).unwrap();

        assert!(engine.reindex_file(&indexed).is_err());
        assert!(engine.remove_file(&indexed).is_err());
        assert!(
            engine.sync().is_err(),
            "filesystem sync must not treat an outside symlink as a safe deletion"
        );
        assert!(
            engine.sync().is_err(),
            "the rejected path must remain pending on the next sync"
        );
        assert_eq!(
            fs::read_to_string(&outside).unwrap(),
            "pub fn outside_secret() {}\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn direct_reindex_through_symlinked_root_persists_exact_sidecar() {
        // Engine roots are canonicalized, but callers may address files
        // through a symlinked project path (macOS tempdirs live under
        // /var -> /private/var). normalize_path must canonicalize on miss so
        // the sidecar key still resolves to the relative path.
        let dir = tempfile::tempdir().unwrap();
        let real_root = dir.path().join("real");
        fs::create_dir_all(&real_root).unwrap();
        write_main(&real_root, "pub fn f(a: u32) -> u32 { a + 1 }\n");

        let link_root = dir.path().join("link");
        std::os::unix::fs::symlink(&real_root, &link_root).unwrap();

        let mut engine = Engine::init(&link_root, IndexConfig::new(&link_root)).unwrap();

        let key = std::path::PathBuf::from("src/main.rs");
        let before = engine.store.load_tree_signatures().unwrap();
        let before_signature = before
            .iter()
            .find_map(|(path, signature)| (*path == key).then_some(*signature))
            .expect("init should record a signature fingerprint for src/main.rs");
        write_main(&real_root, "pub fn f(a: u64) -> u64 { a + 1 }\n");
        // Address the file through the symlinked (non-canonical) root.
        engine.reindex_file(&link_root.join("src/main.rs")).unwrap();

        let after = engine.store.load_tree_signatures().unwrap();
        let after_signature = after
            .iter()
            .find_map(|(path, signature)| (*path == key).then_some(*signature))
            .expect("reindex via a symlinked path should publish the new fingerprint");
        assert_ne!(
            after_signature, before_signature,
            "reindex via a symlinked path must replace the old fingerprint"
        );
        assert_eq!(
            after.iter().filter(|(path, _)| *path == key).count(),
            1,
            "canonical and symlinked roots must share one normalized sidecar key"
        );
    }
}

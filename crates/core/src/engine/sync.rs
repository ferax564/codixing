use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::chunker::Chunker;
use crate::chunker::cast::CastChunker;
use crate::embedder::Embedder;
use crate::error::{CodixingError, Result};
use crate::graph::extract::{extract_definitions, extract_references};
use crate::graph::types::{ReferenceKind, SymbolKind};
use crate::graph::{CallExtractor, ImportExtractor, ImportResolver, compute_pagerank};
use crate::language::detect_language;
use crate::persistence::{FileHashEntry, IndexMeta};
use crate::retriever::ChunkMeta;
use crate::symbols::persistence::serialize_symbols;
use crate::symbols::writer::write_mmap_symbols;
use crate::vector::VectorIndex;

use super::indexing::{
    PendingSymbolGraph, extract_pending_symbol_graph, make_embed_text, normalize_path,
    read_source_bounded, serialize_chunk_meta_compact, stable_file_hash_entry, symbol_from_entity,
    unix_timestamp_string,
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
struct ApplyChangesOutcome {
    cosmetic_skipped: usize,
    successful_paths: std::collections::HashSet<std::path::PathBuf>,
    successful_hashes: std::collections::HashMap<std::path::PathBuf, Option<FileHashEntry>>,
    successful_signatures: std::collections::HashMap<std::path::PathBuf, Option<u64>>,
    failures: Vec<String>,
}

struct ReindexMutation {
    cosmetic_reused: bool,
    hash_entry: Option<FileHashEntry>,
    signature: Option<u64>,
}

/// Bound the incremental freshness overlay. Folding at this size amortizes a
/// repository-wide rewrite over thousands of edits while preventing a long-lived
/// daemon or large checkout from growing a second full path table.
const HASH_DELTA_COMPACT_THRESHOLD: usize = 4_096;

impl ApplyChangesOutcome {
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
    /// Both formats use the store's atomic-write path. The v2 snapshot is
    /// written last because it is the authoritative format read by `sync()`.
    fn persist_hash_snapshot(&self, hashes: &[(std::path::PathBuf, FileHashEntry)]) -> Result<()> {
        let mut hashes = hashes.to_vec();
        hashes.sort_by(|a, b| a.0.cmp(&b.0));
        let v1_hashes: Vec<(std::path::PathBuf, u64)> = hashes
            .iter()
            .map(|(path, entry)| (path.clone(), entry.content_hash))
            .collect();
        self.store.save_tree_hashes(&v1_hashes)?;
        self.store.save_tree_hashes_v2(&hashes)
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

    /// Roll back only the git publication marker after a partially successful
    /// git sync. Search artifacts and successful hash deltas remain durable, but
    /// the old commit makes the complete diff retriable on the next invocation.
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
        do_graph_finalize: bool,
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

        // Read and parse before inspecting or mutating live indexes. Besides
        // keeping failures atomic, this lets us revalidate a COSMETIC decision
        // against the exact bytes that will be indexed. The file may have
        // changed after the classifier's earlier read.
        let Some(read) = read_source_bounded(&abs_path, self.config.max_file_bytes)? else {
            info!(path = %abs_path.display(), limit = self.config.max_file_bytes, "file exceeds max_file_bytes; removing it from the index");
            self.remove_file_inner(&abs_path)?;
            return Ok(ReindexMutation {
                cosmetic_reused: false,
                hash_entry: None,
                signature: None,
            });
        };
        let source = read.bytes;
        let file_hash = xxhash_rust::xxh3::xxh3_64(&source);
        let result = self.parser.parse_file(&abs_path, &source)?;
        let signature =
            super::fingerprint::signature_fingerprint(&result.entities, &source, result.language);
        let cosmetic =
            expected_cosmetic_signature.is_some() && signature == expected_cosmetic_signature;

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
        let mut old_chunk_hashes: std::collections::HashMap<u64, (u64, Vec<f32>)> =
            std::collections::HashMap::new();
        let mut old_stable_keys: std::collections::HashMap<u64, Vec<f32>> =
            std::collections::HashMap::new();
        let mut stable_key_dupes: std::collections::HashSet<u64> = std::collections::HashSet::new();
        {
            let vec_guard = self.vector.read().unwrap_or_else(|e| e.into_inner());
            if vec_guard.is_some() {
                for entry in self.chunk_meta.iter() {
                    let meta = entry.value();
                    if meta.file_path == rel_str && meta.content_hash != 0 {
                        // Try to retrieve the existing vector for this chunk.
                        let existing_vec =
                            vec_guard.as_ref().and_then(|v| v.get_vector(meta.chunk_id));
                        if let Some(vec) = existing_vec {
                            old_chunk_hashes
                                .insert(meta.content_hash, (meta.chunk_id, vec.clone()));
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

        // Hydrate old bodies before deleting their stored Tantivy documents.
        // BM25 init intentionally leaves compact metadata bodies empty, but the
        // trigram index needs the original text to remove every old posting.
        let old_chunk_ids: std::collections::HashSet<u64> = self
            .chunk_meta
            .iter()
            .filter_map(|entry| (entry.value().file_path == rel_str).then_some(*entry.key()))
            .collect();
        let old_chunk_contents = self.hydrate_chunk_contents(&old_chunk_ids)?;

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
        // Remove old chunk metadata and every posting derived from its hydrated
        // body. Hydration happens above, before the Tantivy delete is queued.
        self.chunk_meta.retain(|k, _| !old_chunk_ids.contains(k));
        for (id, content) in &old_chunk_contents {
            self.trigram.get_mut().unwrap().remove(*id, content);
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
                    entity_names: chunk.entity_names.clone(),
                    content_hash: xxhash_rust::xxh3::xxh3_64(chunk.content.as_bytes()),
                    content: chunk.content.clone(),
                },
            );

            // Update trigram index for Strategy::Exact fast-path.
            self.trigram
                .get_mut()
                .unwrap()
                .add(chunk.id, &chunk.content);
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
            let contextual = self.config.embedding.contextual_embeddings;
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
                // Hash the full embed text (including context metadata like scope,
                // file path, entities) so that moving a block to a different scope
                // or renaming the enclosing symbol triggers re-embedding.
                let hash_text = if contextual {
                    if let Some(meta) = self.chunk_meta.get(&chunk.id) {
                        make_embed_text(&meta, true)
                    } else {
                        chunk.content.clone()
                    }
                } else {
                    chunk.content.clone()
                };
                let new_hash = xxhash_rust::xxh3::xxh3_64(hash_text.as_bytes());
                if let Some((_old_id, old_vec)) = old_chunk_hashes.get(&new_hash) {
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

        self.file_chunk_counts.insert(rel_str.clone(), chunks.len());

        // Incremental file trigram update: remove old, add new from full content.
        self.file_trigram.get_mut().unwrap().remove_file(&rel_str);
        self.file_trigram.get_mut().unwrap().add(&rel_str, &source);

        // Update graph edges for this file using the already-parsed tree.
        // PageRank is only recomputed when do_graph_finalize=true (single-file
        // reindex). apply_changes() calls with false and does one pass at the end.
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
            // Resolve call edges using the global symbol table.
            let mut seen_call_targets = std::collections::HashSet::new();
            for name in &call_names {
                let syms = self.symbols.lookup(name);
                let targets: Vec<&str> = syms
                    .iter()
                    .map(|s| s.file_path.as_str())
                    .filter(|&fp| fp != rel_str.as_str())
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect();
                if targets.len() == 1 && seen_call_targets.insert(targets[0].to_string()) {
                    let target_lang =
                        detect_language(std::path::Path::new(targets[0])).unwrap_or(file_language);
                    graph.add_call_edge(&rel_str, targets[0], name, file_language, target_lang);
                }
            }
            // Update symbol-level inner graph for this file.
            graph.remove_file_symbols(&rel_str);
            let source_str = String::from_utf8_lossy(&source);
            let defs = extract_definitions(&source_str, &rel_str, &file_language);
            let refs = extract_references(&source_str, &rel_str, &file_language);

            // Insert definition nodes.
            let mut local_indices = std::collections::HashMap::new();
            let mut func_defs_sorted = Vec::new();
            for def in &defs {
                let idx =
                    graph.add_symbol_with_line(&def.name, &rel_str, def.kind.clone(), def.line);
                local_indices.insert(def.name.clone(), idx);
                if def.kind == SymbolKind::Function {
                    func_defs_sorted.push((def.line, idx));
                }
            }
            func_defs_sorted.sort_by_key(|(line, _)| *line);

            // Wire call edges.
            for r in &refs {
                if r.kind != ReferenceKind::Call {
                    continue;
                }
                let caller_idx =
                    super::indexing::find_enclosing_function(&func_defs_sorted, r.line);
                let caller_idx = match caller_idx {
                    Some(idx) => idx,
                    None => continue,
                };
                let callee_base = r.target_name.rsplit("::").next().unwrap_or(&r.target_name);
                // Look up the callee in local definitions first.
                if let Some(&target_idx) = local_indices.get(callee_base)
                    && caller_idx != target_idx
                {
                    graph.add_reference(caller_idx, target_idx, ReferenceKind::Call);
                }
                // Cross-file resolution is skipped here; will be done on next full reindex.
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
                if let Err(e) = self.store.save_symbol_graph(graph) {
                    warn!(error = %e, "failed to persist symbol graph after reindex");
                }
            }
        }

        debug!(path = %abs_path.display(), chunks = chunks.len(), "reindexed file");
        Ok(ReindexMutation {
            cosmetic_reused: all_reused,
            // Unknown metadata deliberately makes the next full sync verify
            // content once. It cannot hide a write racing this indexed read.
            hash_entry: Some(FileHashEntry::new(file_hash, None, 0)),
            signature,
        })
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
        let old_chunk_ids: std::collections::HashSet<u64> = self
            .chunk_meta
            .iter()
            .filter_map(|entry| (entry.value().file_path == rel_str).then_some(*entry.key()))
            .collect();
        let old_chunk_contents = self.hydrate_chunk_contents(&old_chunk_ids)?;

        self.tantivy.remove_file(&rel_str)?;
        self.symbols.remove_file(&rel_str);
        self.parser.invalidate(&abs_path);
        self.file_chunk_counts.remove(&rel_str);

        {
            let mut vec_guard = self.vector.write().unwrap_or_else(|e| e.into_inner());
            if let Some(ref mut vec_idx) = *vec_guard {
                vec_idx.remove_file(&rel_str)?;
            }
        }
        // Remove compact metadata and the old trigram postings using the
        // bodies hydrated before Tantivy deletion.
        self.chunk_meta.retain(|k, _| !old_chunk_ids.contains(k));
        for (id, content) in &old_chunk_contents {
            self.trigram.get_mut().unwrap().remove(*id, content);
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

    /// Apply a batch of file changes to the index.
    ///
    /// Processes all files first (parse, chunk, embed), then issues a single
    /// Tantivy commit for the entire batch, then runs PageRank exactly once.
    /// For N-file batches (e.g. after `git pull`) this reduces N fsyncs to 1.
    pub fn apply_changes(&mut self, changes: &[crate::watcher::FileChange]) -> Result<()> {
        if self.read_only {
            return Err(CodixingError::ReadOnly);
        }
        use crate::watcher::{ChangeKind, FileChange};

        // Replay any transaction left dirty by a crash or transient failure.
        // This is O(K) in pending paths and makes the next watcher batch (or an
        // explicit empty recovery call) self-healing without a full O(N) sync.
        let mut combined = std::collections::BTreeMap::new();
        for path in self.store.load_dirty_paths()? {
            let kind = if path.is_file() && self.config.is_indexable_path(&path) {
                ChangeKind::Modified
            } else {
                ChangeKind::RemovedDirectory
            };
            combined.insert(path, kind);
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
        // No cosmetic classification — every modified file re-embeds as usual.
        let outcome = self.apply_changes_classified(&changes, &std::collections::HashMap::new())?;
        // `apply_changes_classified` commits the hot indexes, while `save`
        // publishes symbols, metadata, vectors, and the remaining sidecars.
        // Only then may successful paths publish their small freshness overlay
        // and leave the write-ahead dirty journal.
        self.save()?;
        self.persist_exact_signatures(&outcome)?;
        let successful_delta: Vec<_> = outcome
            .successful_hashes
            .iter()
            .map(|(path, entry)| (path.clone(), entry.clone()))
            .collect();
        if !successful_delta.is_empty() {
            self.store.update_tree_hash_delta(&successful_delta)?;
            self.compact_hash_delta_if_needed()?;
        }
        self.store.clear_dirty_paths(&outcome.successful_paths)?;
        match outcome.failure_error() {
            Some(error) => Err(error),
            None => Ok(()),
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
                if self.file_chunk_counts.contains_key(&rel) {
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
                    let mut paths: Vec<String> = self.file_chunk_counts.keys().cloned().collect();
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

    /// Apply a batch of changes, treating the absolute paths in `cosmetic` as
    /// COSMETIC (signature fingerprint unchanged → reuse embedding vectors).
    ///
    /// Returns the successful paths and any per-file failures as well as the
    /// number of files whose embedding vectors were fully reused. Callers use
    /// the successful-path set to publish only authoritative hash/signature
    /// state while leaving failures retriable.
    fn apply_changes_classified(
        &mut self,
        changes: &[crate::watcher::FileChange],
        cosmetic: &std::collections::HashMap<std::path::PathBuf, u64>,
    ) -> Result<ApplyChangesOutcome> {
        if self.read_only {
            return Err(CodixingError::ReadOnly);
        }
        self.symbols.ensure_mutable();
        use crate::watcher::ChangeKind;

        if changes.is_empty() {
            return Ok(ApplyChangesOutcome::default());
        }

        let resolved_changes = self.resolve_change_paths(changes)?;
        let expanded_changes = self.expand_directory_changes(&resolved_changes)?;
        let changes = expanded_changes.as_slice();

        self.invalidate_semantic_artifacts()?;

        // Snapshot callers before any direct mutation. Removing a graph node
        // also removes its incoming edges, so discovering cascades afterwards
        // would miss exactly the callers that need their stale resolution
        // refreshed. Directory removals have already expanded to descendants.
        let changed_paths: std::collections::HashSet<String> = changes
            .iter()
            .filter_map(|change| self.config.normalize_path(&change.path))
            .collect();
        let mut cascade_paths = std::collections::BTreeSet::new();
        if let Some(ref graph) = self.graph {
            for changed in &changed_paths {
                for caller in graph.callers(changed) {
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
        let cascade_paths: Vec<_> = cascade_paths.into_iter().collect();

        // Force-init lazy trigram indexes so they're available for mutation.
        let _ = self.get_trigram();
        let _ = self.get_file_trigram();

        // Journal every changed path before mutating search artifacts. The tiny
        // sidecar avoids rewriting the repository-sized hash snapshot twice per
        // editor batch while still covering both crash windows.
        let mut pending_paths: Vec<_> = changes.iter().map(|change| change.path.clone()).collect();
        pending_paths.extend(cascade_paths.iter().cloned());
        self.store.mark_dirty_paths(&pending_paths)?;

        let mut outcome = ApplyChangesOutcome::default();

        for change in changes {
            match change.kind {
                ChangeKind::Modified => {
                    // The bounded exact read decides oversized status from the
                    // bytes actually observed, avoiding a stat/shrink race.
                    let expected_signature = cosmetic.get(&change.path).copied();
                    match self.reindex_file_impl(&change.path, false, expected_signature) {
                        Ok(update) => {
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
                                .failures
                                .push(format!("{}: {e}", change.path.display()));
                        }
                    }
                }
                ChangeKind::Removed | ChangeKind::RemovedDirectory => {
                    match self.remove_file_inner(&change.path) {
                        Ok(()) => {
                            outcome.successful_paths.insert(change.path.clone());
                            outcome.successful_hashes.insert(change.path.clone(), None);
                            outcome
                                .successful_signatures
                                .insert(change.path.clone(), None);
                        }
                        Err(e) => {
                            warn!(path = %change.path.display(), error = %e, "failed to remove");
                            outcome
                                .failures
                                .push(format!("{}: {e}", change.path.display()));
                        }
                    }
                }
                ChangeKind::CreatedDirectory => {
                    outcome.failures.push(format!(
                        "{}: created directory was not expanded",
                        change.path.display()
                    ));
                }
            }
        }

        // Cascade: re-index the pre-mutation caller snapshot to refresh stale
        // import/call edges after removals and renames.
        if !cascade_paths.is_empty() {
            info!(
                count = cascade_paths.len(),
                "cascading re-index to callers of changed files"
            );
            for path in &cascade_paths {
                // Cascade reindexes (callers of changed files) are never treated
                // as cosmetic — their resolved import/call edges may shift even
                // when their own signatures did not.
                match self.reindex_file_impl(path, false, None) {
                    Ok(update) => {
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
                            .failures
                            .push(format!("cascade {}: {e}", path.display()));
                    }
                }
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
            outcome.successful_paths.clear();
            outcome.successful_hashes.clear();
            outcome.successful_signatures.clear();
            outcome.cosmetic_skipped = 0;
        }

        // Single Tantivy commit for all pending adds + deletes.
        self.tantivy.commit()?;

        // file_trigram already updated incrementally per-file above.
        // Persist the updated index.
        self.get_file_trigram()
            .save_binary(&self.store.file_trigram_path())?;
        self.get_trigram().save_mmap_binary_v3(
            &self.store.chunk_trigram_path(),
            crate::index::trigram::PostingCodec::DeltaVarint,
        )?;

        // Single PageRank recompute for the entire batch.
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

        self.rebuild_semantic_artifacts()?;

        Ok(outcome)
    }

    /// Classify modified files as COSMETIC or STRUCTURAL by comparing each file's
    /// freshly-computed signature fingerprint against the one stored last sync.
    ///
    /// Inputs:
    /// - `changes`: the full change set; only [`ChangeKind::Modified`] files are
    ///   candidates. A brand-new file has no prior fingerprint to compare against
    ///   and is therefore always STRUCTURAL.
    /// - `old_signatures`: per-file fingerprints from the previous sync, keyed by
    ///   normalized relative path.
    ///
    /// Returns:
    /// - `cosmetic`: **absolute** paths whose content changed but whose
    ///   fingerprint did NOT, mapped to the fingerprint observed during
    ///   classification. Reindex revalidates it against the exact indexed bytes.
    /// - `new_signatures`: freshly-computed fingerprints keyed by **normalized
    ///   relative path** (root-invariant) for every changed file that has one.
    ///   Files with no fingerprint (no AST entities) are omitted.
    ///
    /// Conservative by construction: any file that can't be parsed, has no AST
    /// entities, or has no stored prior fingerprint is left out of `cosmetic`
    /// (→ STRUCTURAL → full re-embed).
    fn classify_changes(
        &self,
        changes: &[crate::watcher::FileChange],
        old_signatures: &std::collections::HashMap<std::path::PathBuf, u64>,
    ) -> (
        std::collections::HashMap<std::path::PathBuf, u64>,
        std::collections::HashMap<std::path::PathBuf, u64>,
    ) {
        use super::fingerprint::signature_fingerprint;
        use crate::watcher::ChangeKind;

        let mut cosmetic = std::collections::HashMap::new();
        let mut new_signatures = std::collections::HashMap::new();

        for change in changes {
            if !matches!(change.kind, ChangeKind::Modified) {
                continue;
            }
            // Parse the file to extract its current entities. A read/parse failure
            // is non-fatal here — we simply leave the file STRUCTURAL.
            let source = match read_source_bounded(&change.path, self.config.max_file_bytes) {
                Ok(Some(source)) => source.bytes,
                Err(_) => continue,
                Ok(None) => continue,
            };
            let result = match self.parser.parse_file(&change.path, &source) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let Some(fp) = signature_fingerprint(&result.entities, &source, result.language) else {
                // Not allowlisted, or no AST entities — STRUCTURAL.
                continue;
            };
            // Key signatures by the normalized relative path so they match the
            // sidecar written by `init` regardless of canonical-vs-config root.
            let Some(rel) = self.config.normalize_path(&change.path) else {
                warn!(path = %change.path.display(), "skipping signature for path outside project roots");
                continue;
            };
            let rel_key = std::path::PathBuf::from(&rel);
            new_signatures.insert(rel_key.clone(), fp);

            // A file is COSMETIC iff it has a stored prior fingerprint that
            // matches the freshly-computed one. The presence of a stored
            // fingerprint already implies the file was previously indexed, so we
            // don't additionally gate on `old_hashes` (whose keys can differ in
            // canonical-vs-config-root form between `init` and `sync`). A
            // brand-new file simply has no stored fingerprint → STRUCTURAL.
            if let Some(&old_fp) = old_signatures.get(&rel_key)
                && old_fp == fp
            {
                cosmetic.insert(change.path.clone(), fp);
            }
        }

        (cosmetic, new_signatures)
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
                    meta.content.clear();
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

        // The final full save records the completed vector index and refreshes
        // the regular engine metadata. The enabled config was persisted before
        // model work began, making every intermediate vector checkpoint usable.
        self.save()?;
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
        self.symbols.ensure_mutable();
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

        // SIGFP: keep this classification + sidecar persistence in sync with the
        // identical block in `sync_with_progress`.
        let (cosmetic, _classified_signatures) = self.classify_changes(&changes, &old_signatures);
        let cosmetic_count = cosmetic.len();

        let mut cosmetic_skipped = 0usize;
        if !changes.is_empty() {
            let outcome = self.apply_changes_classified(&changes, &cosmetic)?;
            cosmetic_skipped = outcome.cosmetic_skipped;
            // Sidecars are publication markers for change detection. Write
            // signatures first, then hashes last, and advance each only for
            // files whose index mutation succeeded.
            let authoritative_hashes = merge_hashes_after_apply(
                &old_hashes,
                std::mem::take(&mut current_hashes),
                &changes,
                &outcome.successful_hashes,
            );
            let persist_result = (|| -> Result<()> {
                self.save()?;
                self.persist_signatures_after_apply(&changes, &outcome, &seen)?;
                self.fold_hash_snapshot(&authoritative_hashes)?;
                self.store.clear_dirty_paths(&outcome.successful_paths)
            })();
            if let Err(error) = persist_result {
                self.restore_git_commit(previous_git_commit.as_deref())?;
                self.filter_pipeline.cleanup();
                return Err(error);
            }
            if let Some(error) = outcome.failure_error() {
                self.restore_git_commit(previous_git_commit.as_deref())?;
                self.filter_pipeline.cleanup();
                return Err(error);
            }
        } else {
            // Even if nothing changed content-wise, update the v2 hashes
            // to capture any mtime+size updates (e.g. file was touched).
            if had_hash_delta {
                self.fold_hash_snapshot(&current_hashes)?;
            } else if skipped_by_mtime != unchanged {
                self.store.save_tree_hashes_v2(&current_hashes)?;
            }
            info!("index already up-to-date");
        }

        debug!(
            cosmetic_classified = cosmetic_count,
            cosmetic_skipped, "signature-fingerprint classification"
        );

        // The full filesystem scan covered the whole configured corpus. Publish
        // its git position only after every searchable artifact and freshness
        // sidecar is durable.
        if let Some(scanned_git_commit) = scanned_git_commit.as_deref()
            && git_head_commit(&self.config.root).as_deref() == Some(scanned_git_commit)
        {
            self.restore_git_commit(Some(scanned_git_commit))?;
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

        self.invalidate_semantic_artifacts()?;

        info!(
            files = self.file_chunk_counts.len(),
            "rebuilding dependency graph from disk"
        );

        // Persisted relative paths are not filesystem authority. Resolve every
        // file canonically and discard stale, corrupt, or symlink-escaped entries
        // before the graph rebuild performs any reads.
        let indexed_files: Vec<std::path::PathBuf> = self
            .file_chunk_counts
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
        indexed_files.par_iter().for_each(|file| {
            let rel_str = self.config.normalize_path(file).unwrap_or_else(|| {
                normalize_path(file.strip_prefix(&self.config.root).unwrap_or(file))
            });
            let source = match read_source_bounded(file, self.config.max_file_bytes) {
                Ok(Some(source)) => source.bytes,
                Ok(None) => {
                    warn!(path = %file.display(), "skipping oversized file in rebuild_graph");
                    return;
                }
                Err(e) => {
                    warn!(path = %file.display(), error = %e, "skipping file in rebuild_graph");
                    return;
                }
            };
            let result = match self.parser.parse_file_transient(file, &source) {
                Ok(r) => r,
                Err(e) => {
                    warn!(path = %file.display(), error = %e, "parse failed in rebuild_graph");
                    return;
                }
            };

            // Refresh symbols for this file (drop stale, insert fresh).
            self.symbols.remove_file(&rel_str);
            for entity in &result.entities {
                self.symbols
                    .insert(symbol_from_entity(entity, &rel_str, result.language));
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

        // SIGFP: keep this classification + sidecar persistence in sync with the
        // identical block in `sync`.
        let (cosmetic, _classified_signatures) = self.classify_changes(&changes, &old_signatures);

        let mut cosmetic_skipped = 0usize;
        if !changes.is_empty() {
            let outcome = self.apply_changes_classified(&changes, &cosmetic)?;
            cosmetic_skipped = outcome.cosmetic_skipped;
            on_progress("persisting index");
            let authoritative_hashes = merge_hashes_after_apply(
                &old_hashes,
                std::mem::take(&mut current_hashes),
                &changes,
                &outcome.successful_hashes,
            );
            let persist_result = (|| -> Result<()> {
                self.save()?;
                self.persist_signatures_after_apply(&changes, &outcome, &seen)?;
                self.fold_hash_snapshot(&authoritative_hashes)?;
                self.store.clear_dirty_paths(&outcome.successful_paths)
            })();
            if let Err(error) = persist_result {
                self.restore_git_commit(previous_git_commit.as_deref())?;
                self.filter_pipeline.cleanup();
                return Err(error);
            }
            if let Some(error) = outcome.failure_error() {
                self.restore_git_commit(previous_git_commit.as_deref())?;
                self.filter_pipeline.cleanup();
                return Err(error);
            }
        } else if had_hash_delta {
            self.fold_hash_snapshot(&current_hashes)?;
        } else if skipped_by_mtime != unchanged {
            self.store.save_tree_hashes_v2(&current_hashes)?;
        }

        if let Some(scanned_git_commit) = scanned_git_commit.as_deref()
            && git_head_commit(&self.config.root).as_deref() == Some(scanned_git_commit)
        {
            self.restore_git_commit(Some(scanned_git_commit))?;
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
            let outcome =
                self.apply_changes_classified(&changes, &std::collections::HashMap::new())?;
            let persist_result = (|| -> Result<()> {
                self.save()?;

                // Publish fingerprints from the exact parse that produced the
                // searchable artifacts, never from the earlier git/scan view.
                self.persist_exact_signatures(&outcome)?;

                let successful_delta: Vec<_> = outcome
                    .successful_hashes
                    .iter()
                    .map(|(path, entry)| (path.clone(), entry.clone()))
                    .collect();
                self.store.update_tree_hash_delta(&successful_delta)?;
                self.compact_hash_delta_if_needed()?;
                self.store.clear_dirty_paths(&outcome.successful_paths)
            })();
            if let Err(error) = persist_result {
                self.restore_git_commit(Some(&stored_commit))?;
                return Err(error);
            }
            if let Some(error) = outcome.failure_error() {
                // `save()` records the current HEAD. A partially successful batch
                // cannot claim that commit: keeping the old marker makes the full
                // git delta (and therefore every failed file) retriable.
                self.restore_git_commit(Some(&stored_commit))?;
                return Err(error);
            }
            self.restore_git_commit(Some(&head))?;
        } else {
            // Diff produced no indexable changes (e.g. only docs/assets changed).
            // Still update the stored commit so next call is a true no-op.
            self.save()?;
            self.restore_git_commit(Some(&head))?;
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
        if self.read_only {
            return Err(CodixingError::ReadOnly);
        }
        let sym_bytes = serialize_symbols(&self.symbols)?;
        self.store.save_symbols_bytes(&sym_bytes)?;

        // Also write mmap-format v2 for zero-deserialization open().
        if let Some(in_mem) = self.symbols.as_in_memory() {
            write_mmap_symbols(in_mem, &self.store.symbols_v2_path())?;
        }

        // Persist chunk_meta in compact format (without content).
        let meta_bytes = serialize_chunk_meta_compact(&self.chunk_meta)?;
        self.store.save_chunk_meta_bytes(&meta_bytes)?;

        // Persist vector index.
        {
            let vec_guard = self.vector.read().unwrap_or_else(|e| e.into_inner());
            if let Some(ref vec_idx) = *vec_guard {
                vec_idx.save(
                    &self.store.vector_index_path(),
                    &self.store.file_chunks_path(),
                )?;
            }
        }

        // Persist graph.
        if let Some(ref g) = self.graph {
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

    fn embedded_config(root: &std::path::Path) -> IndexConfig {
        let mut cfg = IndexConfig::new(root);
        cfg.embedding.enabled = true;
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
                if meta.file_path == "src/main.rs" {
                    if let Some(key) = chunk_stable_key(&meta.scope_chain, &meta.entity_names) {
                        if let Some(vec) = v.get_vector(meta.chunk_id) {
                            out.insert(key, vec);
                        }
                    }
                }
            }
        }
        out
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
        write_main(root, "pub fn f(a: u64) -> u64 { let n = a + 1; n }\n");
        let old_signatures = after.into_iter().collect();
        let changes = [crate::watcher::FileChange {
            path: path.clone(),
            kind: crate::watcher::ChangeKind::Modified,
        }];
        let (cosmetic, _) = engine.classify_changes(&changes, &old_signatures);
        assert_eq!(cosmetic.get(&path), Some(&after_signature));
    }

    #[test]
    fn fresh_bm25_reindex_removes_old_trigram_postings() {
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
        assert!(!engine.get_trigram().search("OLDMARKERABC").is_empty());

        write_main(root, "pub fn marker() { let _ = \"NEWMARKERXYZ\"; }\n");
        engine.reindex_file(&path).unwrap();

        assert!(
            engine.get_trigram().search("OLDMARKERABC").is_empty(),
            "reindex must remove postings derived from the compact old chunk"
        );
        assert!(!engine.get_trigram().search("NEWMARKERXYZ").is_empty());
    }

    #[test]
    fn fresh_bm25_remove_file_removes_old_trigram_postings() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let path = root.join("src/main.rs");
        write_main(root, "pub fn marker() { let _ = \"DELETEMARKER\"; }\n");

        let mut engine = Engine::init(root, IndexConfig::new(root)).unwrap();
        assert!(!engine.get_trigram().search("DELETEMARKER").is_empty());

        engine.remove_file(&path).unwrap();

        assert!(
            engine.get_trigram().search("DELETEMARKER").is_empty(),
            "file removal must remove postings derived from compact metadata"
        );
    }

    #[test]
    fn failed_sync_file_keeps_old_hash_while_successful_file_advances() {
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
        assert_ne!(
            after[&rust_path].content_hash, before[&rust_path].content_hash,
            "a successful sibling update must advance its authoritative hash"
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

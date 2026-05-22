use dashmap::DashMap;
use std::fs;
use std::path::Path;
use tracing::{debug, info, warn};

use crate::chunker::cast::CastChunker;
use crate::chunker::Chunker;
use crate::error::{CodixingError, Result};
use crate::graph::extract::{extract_definitions, extract_references};
use crate::graph::types::{ReferenceKind, SymbolKind};
use crate::graph::{compute_pagerank, CallExtractor, ImportExtractor, ImportResolver};
use crate::language::detect_language;
use crate::persistence::{FileHashEntry, IndexMeta};
use crate::retriever::ChunkMeta;
use crate::symbols::persistence::serialize_symbols;
use crate::symbols::writer::write_mmap_symbols;

use super::indexing::{
    make_embed_text, normalize_path, serialize_chunk_meta_compact, symbol_from_entity,
    unix_timestamp_string,
};
use super::{git_diff_since, git_head_commit, Engine, GitSyncStats, SyncStats};

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
) -> Vec<(std::path::PathBuf, u64)> {
    let mut merged: std::collections::HashMap<std::path::PathBuf, u64> = old
        .iter()
        .filter(|(p, _)| seen_rel.contains(*p))
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
    /// Re-index a single file (after modification).
    ///
    /// Removes old data, re-parses, re-chunks, and re-indexes.
    /// When called directly, also recomputes PageRank and persists the graph.
    /// Use `apply_changes` to batch multiple files with a single PageRank pass.
    pub fn reindex_file(&mut self, path: &Path) -> Result<()> {
        if self.read_only {
            return Err(CodixingError::ReadOnly);
        }
        self.symbols.ensure_mutable();
        let _ = self.get_trigram();
        let _ = self.get_file_trigram();
        self.reindex_file_impl(path, true, false)?;
        self.tantivy.commit()?;
        // file_trigram already updated incrementally in reindex_file_impl.
        if let Err(e) = self
            .get_file_trigram()
            .save_binary(&self.store.file_trigram_path())
        {
            warn!(error = %e, "failed to persist file trigram index");
        }
        // chunk trigram also updated incrementally; persist to disk.
        if let Err(e) = self.get_trigram().save_mmap_binary_v2(
            &self.store.chunk_trigram_path(),
            crate::index::trigram::PostingCodec::DeltaVarint,
        ) {
            warn!(error = %e, "failed to persist chunk trigram index");
        }
        Ok(())
    }

    /// Re-index a single file.
    ///
    /// `cosmetic` is `true` when the caller has classified this file's change as
    /// COSMETIC — its content changed but its signature fingerprint did not (see
    /// [`crate::engine::fingerprint`]). For a COSMETIC file the embedding vectors
    /// are reused via a stable per-chunk identity key (scope + entity names) even
    /// when chunk *content* changed, since the structure is unchanged. This is
    /// the broadened reuse that avoids the expensive dense-embedding round-trip
    /// on body/comment/whitespace edits.
    ///
    /// Returns `true` when the file was COSMETIC **and** every chunk's vector was
    /// successfully reused (no chunk needed re-embedding). The caller uses this to
    /// increment `SyncStats::cosmetic_skipped` only when embed work was actually
    /// avoided.
    pub(super) fn reindex_file_impl(
        &mut self,
        path: &Path,
        do_graph_finalize: bool,
        cosmetic: bool,
    ) -> Result<bool> {
        // Wait for any background embedding to complete before modifying the vector index.
        self.wait_for_embeddings();

        let abs_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.config.root.join(path)
        };

        let rel_str = self.config.normalize_path(&abs_path).unwrap_or_else(|| {
            normalize_path(abs_path.strip_prefix(&self.config.root).unwrap_or(path))
        });

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
                            if cosmetic {
                                if let Some(key) =
                                    chunk_stable_key(&meta.scope_chain, &meta.entity_names)
                                {
                                    if old_stable_keys.insert(key, vec).is_some() {
                                        // Two old chunks collide on this key — ambiguous.
                                        stable_key_dupes.insert(key);
                                    }
                                }
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
        {
            let mut vec_guard = self.vector.write().unwrap_or_else(|e| e.into_inner());
            if let Some(ref mut vec_idx) = *vec_guard {
                vec_idx.remove_file(&rel_str)?;
            }
        }
        // Remove old chunk_meta entries for this file and update trigram index.
        // Collect content before removal so trigram.remove() can clean up posting lists.
        let mut removed: Vec<(u64, String)> = Vec::new();
        self.chunk_meta.retain(|k, v| {
            if v.file_path == rel_str {
                removed.push((*k, v.content.clone()));
                false
            } else {
                true
            }
        });
        for (id, content) in &removed {
            self.trigram.get_mut().unwrap().remove(*id, content);
        }

        // Read and re-process.
        let source = fs::read(&abs_path)?;
        let result = self.parser.parse_file(&abs_path, &source)?;

        // Jupyter notebooks need per-cell dispatch — the incremental sync
        // path does not implement that yet. Old chunks have already been
        // removed above; a subsequent `codixing init` or full reindex will
        // repopulate the notebook via `process_jupyter_file`.
        if result.language.is_notebook() {
            warn!(
                path = %rel_str,
                "notebook incremental sync not supported — run `codixing init` to reindex"
            );
            return Ok(false);
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

            // A file counts as a cosmetic-skip only when it was classified
            // COSMETIC, at least one chunk was reused via the stable key (proving
            // the broadened reuse actually fired), and NO chunk required
            // re-embedding — i.e. the embed round-trip was fully avoided.
            all_reused = cosmetic
                && reused_via_stable_key > 0
                && needs_embed.is_empty()
                && !chunks.is_empty();

            if !needs_embed.is_empty() {
                let texts: Vec<String> = needs_embed
                    .iter()
                    .map(|&i| {
                        let c = &chunks[i];
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
                if let Some(&target_idx) = local_indices.get(callee_base) {
                    if caller_idx != target_idx {
                        graph.add_reference(caller_idx, target_idx, ReferenceKind::Call);
                    }
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
        Ok(all_reused)
    }

    /// Inner removal: all index ops except `tantivy.commit()` and graph PageRank finalization.
    /// Called by both `remove_file()` (single-file public API) and `apply_changes()` (batch).
    pub(super) fn remove_file_inner(&mut self, abs_path: &Path, rel_str: &str) -> Result<()> {
        self.tantivy.remove_file(rel_str)?;
        self.symbols.remove_file(rel_str);
        self.parser.invalidate(abs_path);
        self.file_chunk_counts.remove(rel_str);

        {
            let mut vec_guard = self.vector.write().unwrap_or_else(|e| e.into_inner());
            if let Some(ref mut vec_idx) = *vec_guard {
                vec_idx.remove_file(rel_str)?;
            }
        }
        // Remove chunk_meta entries and update trigram index.
        let mut removed: Vec<(u64, String)> = Vec::new();
        self.chunk_meta.retain(|k, v| {
            if v.file_path == rel_str {
                removed.push((*k, v.content.clone()));
                false
            } else {
                true
            }
        });
        for (id, content) in &removed {
            self.trigram.get_mut().unwrap().remove(*id, content);
        }
        // Incremental file trigram removal.
        self.file_trigram.get_mut().unwrap().remove_file(rel_str);

        // Remove graph node + incident edges (PageRank deferred to caller).
        if let Some(ref mut graph) = self.graph {
            graph.remove_file(rel_str);
            graph.remove_file_symbols(rel_str);
        }

        Ok(())
    }

    /// Remove a file from the index entirely.
    pub fn remove_file(&mut self, path: &Path) -> Result<()> {
        if self.read_only {
            return Err(CodixingError::ReadOnly);
        }
        self.symbols.ensure_mutable();
        let _ = self.get_trigram();
        let _ = self.get_file_trigram();
        let rel_str = self.config.normalize_path(path).unwrap_or_else(|| {
            normalize_path(path.strip_prefix(&self.config.root).unwrap_or(path))
        });

        self.remove_file_inner(path, &rel_str)?;
        self.tantivy.commit()?;
        // file_trigram already updated incrementally in remove_file_inner.
        if let Err(e) = self
            .get_file_trigram()
            .save_binary(&self.store.file_trigram_path())
        {
            warn!(error = %e, "failed to persist file trigram index");
        }
        if let Err(e) = self.get_trigram().save_mmap_binary_v2(
            &self.store.chunk_trigram_path(),
            crate::index::trigram::PostingCodec::DeltaVarint,
        ) {
            warn!(error = %e, "failed to persist chunk trigram index");
        }

        // Recompute PageRank + persist graph for single-file removal.
        if let Some(ref mut graph) = self.graph {
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
        // No cosmetic classification — every modified file re-embeds as usual.
        self.apply_changes_classified(changes, &std::collections::HashSet::new())?;
        Ok(())
    }

    /// Apply a batch of changes, treating the absolute paths in `cosmetic` as
    /// COSMETIC (signature fingerprint unchanged → reuse embedding vectors).
    ///
    /// Returns the number of files that were classified COSMETIC **and** had all
    /// chunk vectors successfully reused (the embed round-trip fully avoided).
    /// Used by [`Engine::sync`] / [`Engine::sync_with_progress`] to populate
    /// [`SyncStats::cosmetic_skipped`].
    pub(super) fn apply_changes_classified(
        &mut self,
        changes: &[crate::watcher::FileChange],
        cosmetic: &std::collections::HashSet<std::path::PathBuf>,
    ) -> Result<usize> {
        if self.read_only {
            return Err(CodixingError::ReadOnly);
        }
        self.symbols.ensure_mutable();
        use crate::watcher::ChangeKind;

        if changes.is_empty() {
            return Ok(0);
        }

        // Force-init lazy trigram indexes so they're available for mutation.
        let _ = self.get_trigram();
        let _ = self.get_file_trigram();

        let mut cosmetic_skipped = 0usize;

        for change in changes {
            match change.kind {
                ChangeKind::Modified => {
                    // do_graph_finalize=false — accumulate edge updates but
                    // defer PageRank until after all files are processed.
                    let is_cosmetic = cosmetic.contains(&change.path);
                    match self.reindex_file_impl(&change.path, false, is_cosmetic) {
                        Ok(true) => cosmetic_skipped += 1,
                        Ok(false) => {}
                        Err(e) => {
                            warn!(path = %change.path.display(), error = %e, "failed to reindex")
                        }
                    }
                }
                ChangeKind::Removed => {
                    let rel_str = self.config.normalize_path(&change.path).unwrap_or_else(|| {
                        normalize_path(
                            change
                                .path
                                .strip_prefix(&self.config.root)
                                .unwrap_or(&change.path),
                        )
                    });
                    if let Err(e) = self.remove_file_inner(&change.path, &rel_str) {
                        warn!(path = %change.path.display(), error = %e, "failed to remove");
                    }
                }
            }
        }

        // Cascade: re-index direct callers of changed files to refresh stale edges.
        let changed_paths: std::collections::HashSet<String> = changes
            .iter()
            .filter_map(|c| self.config.normalize_path(&c.path))
            .collect();

        let mut cascade_paths: Vec<std::path::PathBuf> = Vec::new();
        if let Some(ref graph) = self.graph {
            for changed in &changed_paths {
                for caller in graph.callers(changed) {
                    if !changed_paths.contains(&caller) {
                        let abs = self.config.root.join(&caller);
                        if abs.exists()
                            && !cascade_paths.iter().any(|p| p.as_path() == abs.as_path())
                        {
                            cascade_paths.push(abs);
                        }
                    }
                }
            }
        }

        if !cascade_paths.is_empty() {
            info!(
                count = cascade_paths.len(),
                "cascading re-index to callers of changed files"
            );
            for path in &cascade_paths {
                // Cascade reindexes (callers of changed files) are never treated
                // as cosmetic — their resolved import/call edges may shift even
                // when their own signatures did not.
                if let Err(e) = self.reindex_file_impl(path, false, false) {
                    warn!(path = %path.display(), error = %e, "cascade-reindex caller failed");
                }
            }
        }

        // Single Tantivy commit for all pending adds + deletes.
        self.tantivy.commit()?;

        // file_trigram already updated incrementally per-file above.
        // Persist the updated index.
        if let Err(e) = self
            .get_file_trigram()
            .save_binary(&self.store.file_trigram_path())
        {
            warn!(error = %e, "failed to persist file trigram index");
        }
        if let Err(e) = self.get_trigram().save_mmap_binary_v2(
            &self.store.chunk_trigram_path(),
            crate::index::trigram::PostingCodec::DeltaVarint,
        ) {
            warn!(error = %e, "failed to persist chunk trigram index");
        }

        // Single PageRank recompute for the entire batch.
        if let Some(ref mut graph) = self.graph {
            let scores = compute_pagerank(
                graph,
                self.config.graph.damping,
                self.config.graph.iterations,
            );
            graph.apply_pagerank(&scores);
            let flat = graph.to_flat();
            if let Err(e) = self.store.save_graph(&flat) {
                warn!(error = %e, "failed to persist graph after batch changes");
            }
            if let Err(e) = self.store.save_symbol_graph(graph) {
                warn!(error = %e, "failed to persist symbol graph after batch changes");
            }
        }

        Ok(cosmetic_skipped)
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
    ///   fingerprint did NOT — safe to reuse embeddings. Absolute so they match
    ///   the `FileChange::path` keys used by [`Engine::apply_changes_classified`].
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
        std::collections::HashSet<std::path::PathBuf>,
        std::collections::HashMap<std::path::PathBuf, u64>,
    ) {
        use super::fingerprint::signature_fingerprint;
        use crate::watcher::ChangeKind;

        let mut cosmetic = std::collections::HashSet::new();
        let mut new_signatures = std::collections::HashMap::new();

        for change in changes {
            if !matches!(change.kind, ChangeKind::Modified) {
                continue;
            }
            // Parse the file to extract its current entities. A read/parse failure
            // is non-fatal here — we simply leave the file STRUCTURAL.
            let source = match fs::read(&change.path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let result = match self.parser.parse_file(&change.path, &source) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let Some(fp) = signature_fingerprint(&result.entities, &source) else {
                // No AST entities (config/doc/unsupported) — STRUCTURAL.
                continue;
            };
            // Key signatures by the normalized relative path so they match the
            // sidecar written by `init` regardless of canonical-vs-config root.
            let rel = self.config.normalize_path(&change.path).unwrap_or_else(|| {
                normalize_path(
                    change
                        .path
                        .strip_prefix(&self.config.root)
                        .unwrap_or(&change.path),
                )
            });
            let rel_key = std::path::PathBuf::from(&rel);
            new_signatures.insert(rel_key.clone(), fp);

            // A file is COSMETIC iff it has a stored prior fingerprint that
            // matches the freshly-computed one. The presence of a stored
            // fingerprint already implies the file was previously indexed, so we
            // don't additionally gate on `old_hashes` (whose keys can differ in
            // canonical-vs-config-root form between `init` and `sync`). A
            // brand-new file simply has no stored fingerprint → STRUCTURAL.
            if let Some(&old_fp) = old_signatures.get(&rel_key) {
                if old_fp == fp {
                    cosmetic.insert(change.path.clone());
                }
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
            .map(|p| {
                let rel = self.config.normalize_path(p).unwrap_or_else(|| {
                    normalize_path(p.strip_prefix(&self.config.root).unwrap_or(p))
                });
                std::path::PathBuf::from(rel)
            })
            .collect()
    }

    /// Embed all chunks that are in the BM25 index but not yet in the vector index.
    ///
    /// Useful when a project was first initialized with `--no-embeddings` and
    /// embeddings are later desired.  Persists the updated vector index to disk.
    /// Returns the number of chunks that were embedded.
    pub fn embed_remaining(&mut self) -> Result<usize> {
        if self.read_only {
            return Err(CodixingError::ReadOnly);
        }

        // Wait for any background embedding to complete before modifying the vector index.
        self.wait_for_embeddings();

        let embedder = self
            .embedder
            .as_ref()
            .ok_or_else(|| {
                CodixingError::Config(
                    "embeddings not enabled — re-init with embedding support".into(),
                )
            })?
            .clone();

        let mut vec_guard = self.vector.write().unwrap_or_else(|e| e.into_inner());
        let vec_idx = vec_guard
            .as_mut()
            .ok_or_else(|| CodixingError::Config("vector index not available".into()))?;

        // Determine which chunk IDs already have vector representations.
        let embedded: std::collections::HashSet<u64> =
            vec_idx.file_chunks().values().flatten().copied().collect();

        let unembedded: Vec<u64> = self
            .chunk_meta
            .iter()
            .map(|e| *e.key())
            .filter(|id| !embedded.contains(id))
            .collect();

        if unembedded.is_empty() {
            info!("all chunks already embedded; nothing to do");
            return Ok(0);
        }

        info!(count = unembedded.len(), "embedding remaining chunks");

        // Re-use the existing batch helper — build a pending DashMap with the IDs.
        let pending: DashMap<u64, String> =
            unembedded.iter().map(|&id| (id, String::new())).collect();
        let contextual = self.config.embedding.contextual_embeddings;

        #[cfg(feature = "rustqueue")]
        {
            super::embed_queue::embed_pending(
                self.embed_queue.as_ref(),
                &pending,
                &self.chunk_meta,
                &embedder,
                vec_idx,
                contextual,
                self.store.root(),
                &self.config.embedding.model,
            )?;
        }
        #[cfg(not(feature = "rustqueue"))]
        {
            let _stats = super::indexing::embed_and_index_chunks(
                &pending,
                &self.chunk_meta,
                &embedder,
                vec_idx,
                contextual,
                self.store.root(),
            )?;
        }
        drop(vec_guard);

        self.save()?;
        Ok(unembedded.len())
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
        use crate::watcher::{ChangeKind, FileChange};
        use std::collections::{HashMap, HashSet};

        // Load stored hashes (v2 format with mtime+size, falls back to v1).
        let old_hashes: HashMap<std::path::PathBuf, FileHashEntry> = self
            .store
            .load_tree_hashes_v2()
            .unwrap_or_default()
            .into_iter()
            .collect();
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

            // Phase 1: Fast mtime+size pre-filter (stat only, no file read).
            let metadata = fs::metadata(abs_path);
            let (current_mtime, current_size) = match &metadata {
                Ok(m) => (m.modified().ok(), m.len()),
                Err(_) => (None, 0),
            };

            if let Some(cached) = old_hashes.get(abs_path) {
                if !cached.file_might_have_changed(current_mtime, current_size) {
                    // mtime+size unchanged — skip the expensive content hash.
                    unchanged += 1;
                    skipped_by_mtime += 1;
                    current_hashes.push((abs_path.clone(), cached.clone()));
                    continue;
                }
            }

            // Phase 2: File potentially changed — read and compute xxh3.
            let content = fs::read(abs_path)?;
            let hash = xxhash_rust::xxh3::xxh3_64(&content);
            let entry = FileHashEntry::new(hash, current_mtime, current_size);

            match old_hashes.get(abs_path) {
                Some(cached) if cached.content_hash == hash => {
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
            .filter(|c| matches!(c.kind, ChangeKind::Removed))
            .count();

        info!(
            added,
            modified, removed, unchanged, skipped_by_mtime, "syncing index"
        );

        // SIGFP: keep this classification + sidecar persistence in sync with the
        // identical block in `sync_with_progress`.
        let (cosmetic, new_signatures) = self.classify_changes(&changes, &old_signatures);
        let cosmetic_count = cosmetic.len();

        let mut cosmetic_skipped = 0usize;
        if !changes.is_empty() {
            cosmetic_skipped = self.apply_changes_classified(&changes, &cosmetic)?;
            self.save()?;
            // Override the hashes written by save() (which only covers files parsed
            // this session) with the complete current-file set computed here.
            // Write both v1 (for backward compat) and v2 (for mtime+size).
            let v1_hashes: Vec<(std::path::PathBuf, u64)> = current_hashes
                .iter()
                .map(|(p, e)| (p.clone(), e.content_hash))
                .collect();
            self.store.save_tree_hashes(&v1_hashes)?;
            self.store.save_tree_hashes_v2(&current_hashes)?;
            // Persist the refreshed signature sidecar for the next sync, keyed by
            // normalized relative path (root-invariant).
            let seen_rel = self.normalized_rel_set(&seen);
            let sigs = merge_signatures(&old_signatures, &new_signatures, &seen_rel);
            if let Err(e) = self.store.save_tree_signatures(&sigs) {
                warn!(error = %e, "failed to persist tree signatures");
            }
        } else {
            // Even if nothing changed content-wise, update the v2 hashes
            // to capture any mtime+size updates (e.g. file was touched).
            if skipped_by_mtime != unchanged || !current_hashes.is_empty() {
                self.store.save_tree_hashes_v2(&current_hashes)?;
            }
            info!("index already up-to-date");
        }

        debug!(
            cosmetic_classified = cosmetic_count,
            cosmetic_skipped, "signature-fingerprint classification"
        );

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

        info!(
            files = self.file_chunk_counts.len(),
            "rebuilding dependency graph from disk"
        );

        // Collect the set of currently-indexed files (absolute paths). Missing
        // files are handled downstream by build_graph / the call-extraction
        // loop — each has its own warn!+continue, so we don't pre-stat here.
        let indexed_files: Vec<std::path::PathBuf> = self
            .file_chunk_counts
            .keys()
            .map(|rel| self.config.root.join(rel))
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
        indexed_files.par_iter().for_each(|file| {
            let rel_str = self.config.normalize_path(file).unwrap_or_else(|| {
                normalize_path(file.strip_prefix(&self.config.root).unwrap_or(file))
            });
            let source = match fs::read(file) {
                Ok(s) => s,
                Err(e) => {
                    warn!(path = %file.display(), error = %e, "skipping file in rebuild_graph");
                    return;
                }
            };
            let result = match self.parser.parse_file(file, &source) {
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
                pending_calls.insert(rel_str, call_names);
            }
        });
        super::indexing::add_call_edges(&mut new_graph, &self.symbols, &pending_calls);

        // Populate symbol-level call graph.
        // We pass an empty file_contents DashMap so populate_symbol_graph reads from disk.
        let empty_contents: dashmap::DashMap<String, Vec<u8>> = dashmap::DashMap::new();
        super::indexing::populate_symbol_graph(
            &mut new_graph,
            &indexed_files,
            &self.config.root,
            &self.config,
            &empty_contents,
        );

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

            let metadata = fs::metadata(abs_path);
            let (current_mtime, current_size) = match &metadata {
                Ok(m) => (m.modified().ok(), m.len()),
                Err(_) => (None, 0),
            };

            if let Some(cached) = old_hashes.get(abs_path) {
                if !cached.file_might_have_changed(current_mtime, current_size) {
                    unchanged += 1;
                    skipped_by_mtime += 1;
                    current_hashes.push((abs_path.clone(), cached.clone()));
                    continue;
                }
            }

            let content = fs::read(abs_path)?;
            let hash = xxhash_rust::xxh3::xxh3_64(&content);
            let entry = FileHashEntry::new(hash, current_mtime, current_size);

            match old_hashes.get(abs_path) {
                Some(cached) if cached.content_hash == hash => {
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
            .filter(|c| matches!(c.kind, ChangeKind::Removed))
            .count();

        let total_changes = added + modified + removed;
        on_progress(&format!(
            "{} changes detected (added: {}, modified: {}, removed: {}), indexing",
            total_changes, added, modified, removed,
        ));

        // SIGFP: keep this classification + sidecar persistence in sync with the
        // identical block in `sync`.
        let (cosmetic, new_signatures) = self.classify_changes(&changes, &old_signatures);

        let mut cosmetic_skipped = 0usize;
        if !changes.is_empty() {
            cosmetic_skipped = self.apply_changes_classified(&changes, &cosmetic)?;
            on_progress("persisting index");
            self.save()?;
            let v1_hashes: Vec<(std::path::PathBuf, u64)> = current_hashes
                .iter()
                .map(|(p, e)| (p.clone(), e.content_hash))
                .collect();
            self.store.save_tree_hashes(&v1_hashes)?;
            self.store.save_tree_hashes_v2(&current_hashes)?;
            let seen_rel = self.normalized_rel_set(&seen);
            let sigs = merge_signatures(&old_signatures, &new_signatures, &seen_rel);
            if let Err(e) = self.store.save_tree_signatures(&sigs) {
                warn!(error = %e, "failed to persist tree signatures");
            }
        } else if skipped_by_mtime != unchanged || !current_hashes.is_empty() {
            self.store.save_tree_hashes_v2(&current_hashes)?;
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
    /// 6. Call [`Self::save`] to persist everything including the new HEAD.
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
        use crate::watcher::{ChangeKind, FileChange};

        // Load stored git commit from the persisted meta.
        let stored_commit = match self.store.load_meta()?.git_commit {
            Some(c) => c,
            None => {
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
                debug!("git_sync: git unavailable or not a repo — skipping");
                return Ok(GitSyncStats {
                    unchanged: true,
                    ..Default::default()
                });
            }
        };

        if head == stored_commit {
            debug!(commit = %head, "git_sync: already up-to-date");
            return Ok(GitSyncStats {
                unchanged: true,
                ..Default::default()
            });
        }

        info!(from = %stored_commit, to = %head, "git_sync: computing diff");

        let (modified_paths, deleted_paths) =
            match git_diff_since(&self.config.root, &stored_commit) {
                Some(delta) => delta,
                None => {
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
                changes.push(FileChange {
                    path: path.clone(),
                    kind: ChangeKind::Modified,
                });
            }
        }
        for path in &deleted_paths {
            changes.push(FileChange {
                path: path.clone(),
                kind: ChangeKind::Removed,
            });
        }

        let n_modified = changes
            .iter()
            .filter(|c| matches!(c.kind, ChangeKind::Modified))
            .count();
        let n_removed = changes
            .iter()
            .filter(|c| matches!(c.kind, ChangeKind::Removed))
            .count();

        info!(
            modified = n_modified,
            removed = n_removed,
            "git_sync: applying changes"
        );

        if !changes.is_empty() {
            self.apply_changes(&changes)?;
            self.save()?;
        } else {
            // Diff produced no indexable changes (e.g. only docs/assets changed).
            // Still update the stored commit so next call is a true no-op.
            self.save()?;
        }

        Ok(GitSyncStats {
            modified: n_modified,
            removed: n_removed,
            unchanged: false,
        })
    }

    /// Persist current state to disk.
    ///
    /// Records the current git HEAD commit (if available) in the stored
    /// [`IndexMeta`] so that subsequent [`Engine::git_sync`] calls can compute
    /// the minimal diff rather than doing a full re-index.
    pub fn save(&self) -> Result<()> {
        if self.read_only {
            return Err(CodixingError::ReadOnly);
        }
        let sym_bytes = serialize_symbols(&self.symbols)?;
        self.store.save_symbols_bytes(&sym_bytes)?;

        // Also write mmap-format v2 for zero-deserialization open().
        if let Some(in_mem) = self.symbols.as_in_memory() {
            if let Err(e) = write_mmap_symbols(in_mem, &self.store.symbols_v2_path()) {
                warn!(error = %e, "failed to write symbols_v2.bin (non-fatal)");
            }
        }

        let hashes: Vec<(std::path::PathBuf, u64)> =
            self.parser.cache().content_hashes().into_iter().collect();
        self.store.save_tree_hashes(&hashes)?;

        // Also write v2 hashes with mtime+size for fast sync pre-filtering.
        let v2_hashes: Vec<(std::path::PathBuf, FileHashEntry)> = hashes
            .iter()
            .map(|(path, hash)| {
                let (mtime, size) = fs::metadata(path)
                    .map(|m| (m.modified().ok(), m.len()))
                    .unwrap_or((None, 0));
                (path.clone(), FileHashEntry::new(*hash, mtime, size))
            })
            .collect();
        self.store.save_tree_hashes_v2(&v2_hashes)?;

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
            if let Err(e) = self.store.save_graph(&flat) {
                warn!(error = %e, "failed to persist graph in save()");
            }
        }

        let stats = self.stats();
        // Record the current git HEAD so git_sync() can diff from this point.
        let git_commit = git_head_commit(&self.config.root);
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

    /// Persist symbols, chunk_meta, vectors, and graph — but **not** tree
    /// hashes.
    ///
    /// Use this after `reindex_file()` / `remove_file()` when the Tantivy
    /// index has already been committed: the symbol table, chunk metadata, and
    /// graph are updated in memory and need to be written to disk so that
    /// subsequent engine opens (e.g. a new MCP invocation) see the changes.
    ///
    /// Unlike [`Self::save`], this method does not touch the stored file-hash
    /// table, so a subsequent [`Self::sync`] will correctly detect the changed
    /// file rather than re-indexing the entire repo.
    pub fn persist_incremental(&self) -> Result<()> {
        if self.read_only {
            return Err(CodixingError::ReadOnly);
        }
        let sym_bytes = serialize_symbols(&self.symbols)?;
        self.store.save_symbols_bytes(&sym_bytes)?;

        // Also write mmap-format v2 for zero-deserialization open().
        if let Some(in_mem) = self.symbols.as_in_memory() {
            if let Err(e) = write_mmap_symbols(in_mem, &self.store.symbols_v2_path()) {
                warn!(error = %e, "failed to write symbols_v2.bin (non-fatal)");
            }
        }

        let meta_bytes = serialize_chunk_meta_compact(&self.chunk_meta)?;
        self.store.save_chunk_meta_bytes(&meta_bytes)?;

        {
            let vec_guard = self.vector.read().unwrap_or_else(|e| e.into_inner());
            if let Some(ref vec_idx) = *vec_guard {
                vec_idx.save(
                    &self.store.vector_index_path(),
                    &self.store.file_chunks_path(),
                )?;
            }
        }

        if let Some(ref g) = self.graph {
            let flat = g.to_flat();
            if let Err(e) = self.store.save_graph(&flat) {
                warn!(error = %e, "failed to persist graph in persist_incremental()");
            }
        }

        let stats = self.stats();
        let git_commit = git_head_commit(&self.config.root);
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

    /// Build an embedded engine over `src/main.rs`. Returns `None` (after printing
    /// a skip note) when the embedder failed to load — vector-reuse assertions
    /// cannot run without it.
    fn build_embedded(root: &std::path::Path) -> Option<Engine> {
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
}

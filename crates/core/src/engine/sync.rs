use dashmap::DashMap;
use std::fs;
use std::path::Path;
use tracing::{debug, info, warn};

use crate::chunker::Chunker;
use crate::chunker::cast::CastChunker;
use crate::error::{CodixingError, Result};
use crate::graph::extract::{extract_definitions, extract_references};
use crate::graph::types::{ReferenceKind, SymbolKind};
use crate::graph::{CallExtractor, ImportExtractor, ImportResolver, compute_pagerank};
use crate::language::detect_language;
use crate::persistence::{FileHashEntry, IndexMeta};
use crate::retriever::ChunkMeta;
use crate::symbols::persistence::serialize_symbols;
use crate::symbols::writer::write_mmap_symbols;

use super::indexing::{
    make_embed_text, normalize_path, serialize_chunk_meta_compact, symbol_from_entity,
    unix_timestamp_string,
};
use super::{Engine, GitSyncStats, SyncStats, git_diff_since, git_head_commit};

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
        self.reindex_file_impl(path, true)?;
        self.tantivy.commit()?;
        // file_trigram already updated incrementally in reindex_file_impl.
        if let Err(e) = self
            .get_file_trigram()
            .save_binary(&self.store.file_trigram_path())
        {
            warn!(error = %e, "failed to persist file trigram index");
        }
        // chunk trigram also updated incrementally; persist to disk.
        if let Err(e) = self
            .get_trigram()
            .save_mmap_binary(&self.store.chunk_trigram_path())
        {
            warn!(error = %e, "failed to persist chunk trigram index");
        }
        Ok(())
    }

    pub(super) fn reindex_file_impl(&mut self, path: &Path, do_graph_finalize: bool) -> Result<()> {
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
        let old_chunk_hashes: std::collections::HashMap<u64, (u64, Vec<f32>)> = {
            let mut map = std::collections::HashMap::new();
            let vec_guard = self.vector.read().unwrap_or_else(|e| e.into_inner());
            if vec_guard.is_some() {
                for entry in self.chunk_meta.iter() {
                    let meta = entry.value();
                    if meta.file_path == rel_str && meta.content_hash != 0 {
                        // Try to retrieve the existing vector for this chunk.
                        let existing_vec =
                            vec_guard.as_ref().and_then(|v| v.get_vector(meta.chunk_id));
                        if let Some(vec) = existing_vec {
                            map.insert(meta.content_hash, (meta.chunk_id, vec));
                        }
                    }
                }
            }
            drop(vec_guard);
            map
        };

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
        let mut vec_guard = self.vector.write().unwrap_or_else(|e| e.into_inner());
        if let (Some(emb), Some(vec_idx)) = (self.embedder.as_ref(), vec_guard.as_mut()) {
            let contextual = self.config.embedding.contextual_embeddings;
            let mut reused = 0usize;
            let mut needs_embed: Vec<usize> = Vec::new();

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
                } else {
                    needs_embed.push(i);
                }
            }

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
                    re_embedded = needs_embed.len(),
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
        Ok(())
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
        if let Err(e) = self
            .get_trigram()
            .save_mmap_binary(&self.store.chunk_trigram_path())
        {
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
        if self.read_only {
            return Err(CodixingError::ReadOnly);
        }
        self.symbols.ensure_mutable();
        use crate::watcher::ChangeKind;

        if changes.is_empty() {
            return Ok(());
        }

        // Force-init lazy trigram indexes so they're available for mutation.
        let _ = self.get_trigram();
        let _ = self.get_file_trigram();

        for change in changes {
            match change.kind {
                ChangeKind::Modified => {
                    // do_graph_finalize=false — accumulate edge updates but
                    // defer PageRank until after all files are processed.
                    if let Err(e) = self.reindex_file_impl(&change.path, false) {
                        warn!(path = %change.path.display(), error = %e, "failed to reindex");
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
                if let Err(e) = self.reindex_file_impl(path, false) {
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
        if let Err(e) = self
            .get_trigram()
            .save_mmap_binary(&self.store.chunk_trigram_path())
        {
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

        Ok(())
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

        if !changes.is_empty() {
            self.apply_changes(&changes)?;
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
        } else {
            // Even if nothing changed content-wise, update the v2 hashes
            // to capture any mtime+size updates (e.g. file was touched).
            if skipped_by_mtime != unchanged || !current_hashes.is_empty() {
                self.store.save_tree_hashes_v2(&current_hashes)?;
            }
            info!("index already up-to-date");
        }

        self.filter_pipeline.cleanup();

        Ok(SyncStats {
            added,
            modified,
            removed,
            unchanged,
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

        if !changes.is_empty() {
            self.apply_changes(&changes)?;
            on_progress("persisting index");
            self.save()?;
            let v1_hashes: Vec<(std::path::PathBuf, u64)> = current_hashes
                .iter()
                .map(|(p, e)| (p.clone(), e.content_hash))
                .collect();
            self.store.save_tree_hashes(&v1_hashes)?;
            self.store.save_tree_hashes_v2(&current_hashes)?;
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

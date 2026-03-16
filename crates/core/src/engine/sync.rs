use dashmap::DashMap;
use std::fs;
use std::path::Path;
use tracing::{debug, info, warn};

use crate::chunker::Chunker;
use crate::chunker::cast::CastChunker;
use crate::error::{CodixingError, Result};
use crate::graph::{CallExtractor, ImportExtractor, ImportResolver, compute_pagerank};
use crate::language::detect_language;
use crate::persistence::{FileHashEntry, IndexMeta};
use crate::retriever::ChunkMeta;
use crate::symbols::persistence::serialize_symbols;

use super::{
    Engine, GitSyncStats, SyncStats, git_diff_since, git_head_commit, make_embed_text,
    normalize_path, symbol_from_entity, unix_timestamp_string,
};

impl Engine {
    /// Re-index a single file (after modification).
    ///
    /// Removes old data, re-parses, re-chunks, and re-indexes.
    /// When called directly, also recomputes PageRank and persists the graph.
    /// Use `apply_changes` to batch multiple files with a single PageRank pass.
    pub fn reindex_file(&mut self, path: &Path) -> Result<()> {
        self.reindex_file_impl(path, true)?;
        self.tantivy.commit()?;
        Ok(())
    }

    pub(super) fn reindex_file_impl(&mut self, path: &Path, do_graph_finalize: bool) -> Result<()> {
        let abs_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.config.root.join(path)
        };

        let rel_str = self.config.normalize_path(&abs_path).unwrap_or_else(|| {
            normalize_path(abs_path.strip_prefix(&self.config.root).unwrap_or(path))
        });

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

        self.file_chunk_counts.insert(rel_str.clone(), chunks.len());

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

    /// Inner removal: all index ops except `tantivy.commit()` and graph PageRank finalization.
    /// Called by both `remove_file()` (single-file public API) and `apply_changes()` (batch).
    pub(super) fn remove_file_inner(&mut self, abs_path: &Path, rel_str: &str) -> Result<()> {
        self.tantivy.remove_file(rel_str)?;
        self.symbols.remove_file(rel_str);
        self.parser.invalidate(abs_path);
        self.file_chunk_counts.remove(rel_str);

        if let Some(ref mut vec_idx) = self.vector {
            vec_idx.remove_file(rel_str)?;
        }
        self.chunk_meta.retain(|_, v| v.file_path != rel_str);

        // Remove graph node + incident edges (PageRank deferred to caller).
        if let Some(ref mut graph) = self.graph {
            graph.remove_file(rel_str);
        }

        Ok(())
    }

    /// Remove a file from the index entirely.
    pub fn remove_file(&mut self, path: &Path) -> Result<()> {
        let rel_str = self.config.normalize_path(path).unwrap_or_else(|| {
            normalize_path(path.strip_prefix(&self.config.root).unwrap_or(path))
        });

        self.remove_file_inner(path, &rel_str)?;
        self.tantivy.commit()?;

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
        use crate::watcher::ChangeKind;

        if changes.is_empty() {
            return Ok(());
        }

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

        // Single Tantivy commit for all pending adds + deletes.
        self.tantivy.commit()?;

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
        }

        Ok(())
    }

    /// Embed all chunks that are in the BM25 index but not yet in the vector index.
    ///
    /// Useful when a project was first initialized with `--no-embeddings` and
    /// embeddings are later desired.  Persists the updated vector index to disk.
    /// Returns the number of chunks that were embedded.
    pub fn embed_remaining(&mut self) -> Result<usize> {
        use super::embed_and_index_chunks;

        let embedder = self
            .embedder
            .as_ref()
            .ok_or_else(|| {
                CodixingError::Config(
                    "embeddings not enabled — re-init with embedding support".into(),
                )
            })?
            .clone();

        let vec_idx = self
            .vector
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
        embed_and_index_chunks(
            &pending,
            &self.chunk_meta,
            &embedder,
            vec_idx,
            contextual,
            self.store.root(),
        )?;

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
        use crate::watcher::{ChangeKind, FileChange};
        use std::collections::{HashMap, HashSet};

        // Load stored hashes (v2 format with mtime+size, falls back to v1).
        let old_hashes: HashMap<std::path::PathBuf, FileHashEntry> = self
            .store
            .load_tree_hashes_v2()
            .unwrap_or_default()
            .into_iter()
            .collect();

        let current_files = super::walk_source_files(&self.config.root, &self.config)?;

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
        let sym_bytes = serialize_symbols(&self.symbols)?;
        self.store.save_symbols_bytes(&sym_bytes)?;

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

        // Persist chunk_meta.
        let meta_pairs: Vec<(u64, ChunkMeta)> = self
            .chunk_meta
            .iter()
            .map(|e| (*e.key(), e.value().clone()))
            .collect();
        let meta_bytes = bitcode::serialize(&meta_pairs).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize chunk_meta: {e}"))
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
        let sym_bytes = serialize_symbols(&self.symbols)?;
        self.store.save_symbols_bytes(&sym_bytes)?;

        let meta_pairs: Vec<(u64, ChunkMeta)> = self
            .chunk_meta
            .iter()
            .map(|e| (*e.key(), e.value().clone()))
            .collect();
        let meta_bytes = bitcode::serialize(&meta_pairs).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize chunk_meta: {e}"))
        })?;
        self.store.save_chunk_meta_bytes(&meta_bytes)?;

        if let Some(ref vec_idx) = self.vector {
            vec_idx.save(
                &self.store.vector_index_path(),
                &self.store.file_chunks_path(),
            )?;
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

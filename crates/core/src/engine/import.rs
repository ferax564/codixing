//! External-context import: index normalized [`ExternalDocument`]s into the
//! live engine.
//!
//! Imported documents are not on the filesystem. Each one is rendered to a
//! Markdown blob and indexed under a virtual path
//! (`_external/<source>/<id>.md`) through the same document pipeline used for
//! real Markdown files. The result is indistinguishable from an indexed `.md`
//! file to search, grep, the doc→code symbol graph, and persistence — but it
//! is invisible to the filesystem walk, so `codixing sync` never treats it as
//! a deleted file. (A full `codixing init` rebuilds from disk only, so
//! re-run the import afterward.)
//!
//! Re-importing a source is idempotent at source granularity: every existing
//! document under `_external/<source>/` is removed before the new set is
//! added, so a refreshed export replaces the old one wholesale.

use std::collections::HashSet;

use tracing::{debug, warn};

use crate::chunker::doc::chunk_doc;
use crate::error::{CodixingError, Result};
use crate::external::{EXTERNAL_PATH_PREFIX, ExternalDocument};
use crate::graph::compute_pagerank;
use crate::language::{Language, detect_language};
use crate::retriever::ChunkMeta;

use super::Engine;
use super::indexing::make_embed_text;

/// Summary of an [`Engine::import_external`] run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportStats {
    /// Number of documents indexed.
    pub documents: usize,
    /// Number of chunks produced across all documents.
    pub chunks: usize,
    /// Number of doc→code graph edges created from symbol references.
    pub doc_edges: usize,
    /// Number of previously-imported documents removed for idempotency.
    pub replaced: usize,
}

impl Engine {
    /// Index a batch of external documents into the live index and persist.
    ///
    /// All documents sharing a `source` are treated as one set: any documents
    /// previously imported under that source are removed first. Returns counts
    /// for reporting. Errors with [`CodixingError::ReadOnly`] when the engine
    /// holds no write lock.
    pub fn import_external(&mut self, docs: Vec<ExternalDocument>) -> Result<ImportStats> {
        if self.read_only {
            return Err(CodixingError::ReadOnly);
        }
        if docs.is_empty() {
            return Ok(ImportStats::default());
        }
        // Don't race the background embedder against our vector writes.
        self.wait_for_embeddings();
        // Force lazy trigram indexes to load before we mutate them.
        let _ = self.get_trigram();
        let _ = self.get_file_trigram();

        let mut stats = ImportStats::default();

        // ── Idempotency: clear prior docs under each incoming source ──────
        let sources: HashSet<String> = docs.iter().map(|d| d.source.clone()).collect();
        for source in &sources {
            let prefix = format!(
                "{EXTERNAL_PATH_PREFIX}{}/",
                crate::external::slugify(source)
            );
            stats.replaced += self.remove_external_prefix(&prefix)?;
        }

        // ── Index each document via the doc pipeline ─────────────────────
        let doc_support = self
            .parser
            .registry()
            .get_doc(Language::Markdown)
            .ok_or_else(|| {
                CodixingError::Import("Markdown document support is unavailable".to_string())
            })?;

        // Collected for the post-pass: (chunk_id, rel_path, embed_text).
        let mut to_embed: Vec<(u64, String, String)> = Vec::new();
        // Collected for doc→code edges: (rel_path, symbol names).
        let mut doc_refs: Vec<(String, Vec<String>)> = Vec::new();
        let contextual = self.config.embedding.contextual_embeddings;

        for doc in &docs {
            let rel = doc.virtual_path();
            let markdown = doc.to_markdown();
            let source_bytes = markdown.as_bytes();
            let file_name = std::path::Path::new(&rel)
                .file_name()
                .and_then(|n| n.to_str());

            let sections = doc_support.parse_sections(source_bytes, file_name);
            let symbol_refs = doc_support.extract_symbol_refs(source_bytes);

            let mut chunks = chunk_doc(
                &rel,
                source_bytes,
                &sections,
                Language::Markdown,
                &self.config.chunk,
            );
            // Attribute each symbol ref to the chunk whose byte range contains it.
            for chunk in &mut chunks {
                chunk.entity_names = symbol_refs
                    .iter()
                    .filter(|r| {
                        r.byte_range.start >= chunk.byte_start && r.byte_range.end <= chunk.byte_end
                    })
                    .map(|r| r.name.clone())
                    .collect();
            }

            for chunk in &chunks {
                self.tantivy.add_chunk(chunk)?;
                let meta = ChunkMeta {
                    chunk_id: chunk.id,
                    file_path: rel.clone(),
                    language: chunk.language.name().to_string(),
                    line_start: chunk.line_start as u64,
                    line_end: chunk.line_end as u64,
                    signature: String::new(),
                    scope_chain: chunk.scope_chain.clone(),
                    entity_names: chunk.entity_names.clone(),
                    content_hash: xxhash_rust::xxh3::xxh3_64(chunk.content.as_bytes()),
                    content: chunk.content.clone(),
                };
                let embed_text = if contextual {
                    make_embed_text(&meta, true)
                } else {
                    chunk.content.clone()
                };
                self.chunk_meta.insert(chunk.id, meta);
                self.trigram
                    .get_mut()
                    .unwrap()
                    .add(chunk.id, &chunk.content);
                to_embed.push((chunk.id, rel.clone(), embed_text));
            }

            self.file_trigram.get_mut().unwrap().add(&rel, source_bytes);
            self.file_chunk_counts.insert(rel.clone(), chunks.len());

            if !symbol_refs.is_empty() {
                let mut names: Vec<String> = symbol_refs.into_iter().map(|r| r.name).collect();
                names.sort();
                names.dedup();
                doc_refs.push((rel.clone(), names));
            }

            stats.documents += 1;
            stats.chunks += chunks.len();
        }

        // ── Embed new chunks (if an embedder is configured) ──────────────
        if let Some(emb) = self.embedder.clone() {
            let texts: Vec<String> = to_embed.iter().map(|(_, _, t)| t.clone()).collect();
            match emb.embed(texts) {
                Ok(embeddings) => {
                    let mut vec_guard = self.vector.write().unwrap_or_else(|e| e.into_inner());
                    if let Some(vec_idx) = vec_guard.as_mut() {
                        for ((id, rel, _), embedding) in to_embed.iter().zip(embeddings.iter()) {
                            if let Err(e) = vec_idx.add_mut(*id, embedding, rel) {
                                warn!(error = %e, chunk_id = id, "failed to add import vector");
                            }
                        }
                    }
                }
                Err(e) => warn!(error = %e, "embedding failed during import — indexed BM25-only"),
            }
        }

        // ── Doc→code graph edges from symbol references ──────────────────
        if let Some(graph) = self.graph.as_mut() {
            for (rel, names) in &doc_refs {
                for name in names {
                    let syms = self.symbols.lookup(name);
                    let targets: HashSet<&str> = syms
                        .iter()
                        .map(|s| s.file_path.as_str())
                        .filter(|&fp| fp != rel.as_str())
                        .collect();
                    // Only link when the reference resolves unambiguously.
                    if targets.len() == 1 {
                        let target = *targets.iter().next().unwrap();
                        let target_lang = detect_language(std::path::Path::new(target))
                            .unwrap_or(Language::Markdown);
                        graph.add_doc_edge(rel, target, name, Language::Markdown, target_lang);
                        stats.doc_edges += 1;
                    }
                }
            }
            let scores = compute_pagerank(
                graph,
                self.config.graph.damping,
                self.config.graph.iterations,
            );
            graph.apply_pagerank(&scores);
        }

        // ── Commit + persist ─────────────────────────────────────────────
        self.tantivy.commit()?;
        self.persist_incremental()?;
        if let Err(e) = self
            .get_file_trigram()
            .save_binary(&self.store.file_trigram_path())
        {
            warn!(error = %e, "failed to persist file trigram after import");
        }
        if let Err(e) = self.get_trigram().save_mmap_binary_v2(
            &self.store.chunk_trigram_path(),
            crate::index::trigram::PostingCodec::DeltaVarint,
        ) {
            warn!(error = %e, "failed to persist chunk trigram after import");
        }

        debug!(
            documents = stats.documents,
            chunks = stats.chunks,
            doc_edges = stats.doc_edges,
            replaced = stats.replaced,
            "imported external documents"
        );
        Ok(stats)
    }

    /// Remove every indexed chunk whose `file_path` starts with `prefix`
    /// (an `_external/<source>/` namespace). Returns the number of distinct
    /// documents (virtual files) removed. Mirrors the removal half of
    /// [`Engine::reindex_file_impl`] but for virtual paths.
    fn remove_external_prefix(&mut self, prefix: &str) -> Result<usize> {
        // Collect virtual paths and hydrate only their chunk bodies before the
        // stored Tantivy documents are removed. Reopened BM25 indexes keep
        // compact metadata without duplicate body strings.
        let mut files: HashSet<String> = HashSet::new();
        let mut removed_chunk_ids: HashSet<u64> = HashSet::new();
        for entry in self.chunk_meta.iter() {
            let meta = entry.value();
            if meta.file_path.starts_with(prefix) {
                files.insert(meta.file_path.clone());
                removed_chunk_ids.insert(meta.chunk_id);
            }
        }
        if files.is_empty() {
            return Ok(0);
        }
        let removed_chunk_contents = self.hydrate_chunk_contents(&removed_chunk_ids)?;

        for rel in &files {
            self.tantivy.remove_file(rel)?;
            self.file_trigram.get_mut().unwrap().remove_file(rel);
            self.file_chunk_counts.remove(rel);
            if let Some(graph) = self.graph.as_mut() {
                graph.remove_file_edges(rel);
            }
        }
        {
            let mut vec_guard = self.vector.write().unwrap_or_else(|e| e.into_inner());
            if let Some(vec_idx) = vec_guard.as_mut() {
                for rel in &files {
                    vec_idx.remove_file(rel)?;
                }
            }
        }
        self.chunk_meta
            .retain(|_, v| !v.file_path.starts_with(prefix));
        for (id, content) in &removed_chunk_contents {
            self.trigram.get_mut().unwrap().remove(*id, content);
        }
        Ok(files.len())
    }
}

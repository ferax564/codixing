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
use std::ops::Range;

use tracing::{debug, warn};

use crate::chunker::doc::chunk_doc;
use crate::error::{CodixingError, Result};
use crate::external::{EXTERNAL_PATH_PREFIX, ExternalDocument};
use crate::language::{Language, detect_language};
use crate::retriever::ChunkMeta;
use crate::vector::VectorIndex;

use super::Engine;
use super::indexing::{STREAM_BATCH_SIZE, make_embed_text};

fn import_embedding_batch_ranges(total: usize) -> impl Iterator<Item = Range<usize>> {
    (0..total).step_by(STREAM_BATCH_SIZE).map(move |start| {
        let end = start.saturating_add(STREAM_BATCH_SIZE).min(total);
        start..end
    })
}

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
        // Never mix an external import into an unpublished watcher batch.
        // Replay durable retries and publish pending filesystem changes first,
        // then fork a fresh working generation so readers retain their complete
        // immutable snapshot.
        self.apply_changes(&[])?;
        self.ensure_working_generation()?;
        let import_result = (|| -> Result<ImportStats> {
            // An embedding-enabled empty repository has no background work, so
            // init legitimately leaves the shared vector slot empty. Bootstrap
            // it before removing or adding any imported data. Constructing the
            // candidate outside the lock keeps model/vector setup from blocking
            // concurrent readers of the shared slot.
            let embedder = self.embedder.clone();
            if let Some(embedder) = embedder.as_ref() {
                let existing_dims = self
                    .vector
                    .read()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_ref()
                    .map(|index| index.dims);
                match existing_dims {
                    Some(dims) if dims != embedder.dims => {
                        return Err(CodixingError::Config(format!(
                            "import vector dimensions {dims} do not match embedder dimensions {}",
                            embedder.dims
                        )));
                    }
                    Some(_) => {}
                    None => {
                        let candidate =
                            VectorIndex::new(embedder.dims, self.config.embedding.quantize)?;
                        let mut vector = self
                            .vector
                            .write()
                            .unwrap_or_else(|error| error.into_inner());
                        if let Some(existing) = vector.as_ref() {
                            if existing.dims != embedder.dims {
                                return Err(CodixingError::Config(format!(
                                    "import vector dimensions {} do not match embedder dimensions {}",
                                    existing.dims, embedder.dims
                                )));
                            }
                        } else {
                            *vector = Some(candidate);
                        }
                    }
                }
            }

            // Force the shared file trigram to load before we mutate it.
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

            // Collected for doc→code edges: (rel_path, symbol names).
            let mut doc_refs: Vec<(String, Vec<String>)> = Vec::new();
            let contextual = self.config.embedding.contextual_embeddings;
            let mut embedding_failed = false;

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
                            r.byte_range.start >= chunk.byte_start
                                && r.byte_range.end <= chunk.byte_end
                        })
                        .map(|r| r.name.clone())
                        .collect();
                }

                for range in import_embedding_batch_ranges(chunks.len()) {
                    let batch = &chunks[range];
                    // BM25-only imports allocate no embed text. Once a model
                    // batch fails, later batches stay BM25-only instead of
                    // repeatedly invoking a broken runtime.
                    let mut embed_texts = (embedder.is_some() && !embedding_failed)
                        .then(|| Vec::with_capacity(batch.len()));

                    for chunk in batch {
                        if let Err(error) = self.tantivy.add_chunk(chunk) {
                            for pending in &chunks {
                                self.chunk_meta.remove(&pending.id);
                            }
                            return Err(error);
                        }

                        let mut meta = ChunkMeta {
                            chunk_id: chunk.id,
                            file_path: rel.clone(),
                            language: chunk.language.name().to_string(),
                            line_start: chunk.line_start as u64,
                            line_end: chunk.line_end as u64,
                            signature: String::new(),
                            scope_chain: chunk.scope_chain.clone(),
                            entity_names: chunk.entity_names.clone(),
                            content_hash: xxhash_rust::xxh3::xxh3_64(chunk.content.as_bytes()),
                            content: if contextual && embed_texts.is_some() {
                                chunk.content.clone()
                            } else {
                                String::new()
                            },
                        };
                        if let Some(texts) = embed_texts.as_mut() {
                            texts.push(if contextual {
                                make_embed_text(&meta, true)
                            } else {
                                chunk.content.clone()
                            });
                        }
                        // Embed text now owns the only transient body copy.
                        // Resident metadata is compact even while the bounded
                        // model batch is running.
                        meta.content = String::new();
                        self.chunk_meta.insert(chunk.id, meta);
                    }

                    if let (Some(embedder), Some(texts)) = (embedder.as_ref(), embed_texts) {
                        let batch_failed = match embedder.embed(texts) {
                            Ok(embeddings) if embeddings.len() == batch.len() => {
                                // Model inference is complete before taking the
                                // vector lock; the critical section contains
                                // only bounded in-memory insertions.
                                let mut vector = self
                                    .vector
                                    .write()
                                    .unwrap_or_else(|error| error.into_inner());
                                let vec_idx = vector.as_mut().ok_or_else(|| {
                                    CodixingError::Config(
                                        "import vector index disappeared after bootstrap"
                                            .to_string(),
                                    )
                                })?;
                                let mut add_failed = false;
                                for (chunk, embedding) in batch.iter().zip(embeddings.iter()) {
                                    if let Err(error) = vec_idx.add_mut(chunk.id, embedding, &rel) {
                                        warn!(
                                            %error,
                                            chunk_id = chunk.id,
                                            "failed to add import vector"
                                        );
                                        add_failed = true;
                                    }
                                }
                                add_failed
                            }
                            Ok(embeddings) => {
                                warn!(
                                    expected = batch.len(),
                                    actual = embeddings.len(),
                                    "embedding count mismatch during import; document and remaining batches are BM25-only"
                                );
                                true
                            }
                            Err(error) => {
                                warn!(
                                    %error,
                                    "embedding failed during import; document and remaining batches are BM25-only"
                                );
                                true
                            }
                        };
                        if batch_failed {
                            // Earlier windows from this document may already
                            // have succeeded. Remove them as one bounded-path
                            // operation so a published document is either fully
                            // vectorized or consistently BM25-only.
                            let mut vector = self
                                .vector
                                .write()
                                .unwrap_or_else(|error| error.into_inner());
                            vector
                                .as_mut()
                                .ok_or_else(|| {
                                    CodixingError::Config(
                                        "import vector index disappeared during failure cleanup"
                                            .to_string(),
                                    )
                                })?
                                .remove_file(&rel)?;
                            embedding_failed = true;
                        }
                    }
                }

                let trigrams = crate::index::trigram::FileTrigramIndex::prepare_contents(
                    std::iter::once(source_bytes).chain(
                        chunks
                            .iter()
                            .filter(|chunk| {
                                source_bytes.get(chunk.byte_start..chunk.byte_end)
                                    != Some(chunk.content.as_bytes())
                            })
                            .map(|chunk| chunk.content.as_bytes()),
                    ),
                );
                self.file_trigram
                    .get_mut()
                    .unwrap()
                    .add_prepared(&rel, &trigrams);
                self.file_chunk_ids.insert(
                    rel.clone(),
                    chunks
                        .iter()
                        .map(|chunk| chunk.id)
                        .collect::<Vec<_>>()
                        .into_boxed_slice(),
                );

                if !symbol_refs.is_empty() {
                    let mut names: Vec<String> = symbol_refs.into_iter().map(|r| r.name).collect();
                    names.sort();
                    names.dedup();
                    doc_refs.push((rel.clone(), names));
                }

                stats.documents += 1;
                stats.chunks += chunks.len();
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
            }

            #[cfg(any(test, feature = "internal-testing"))]
            if docs
                .iter()
                .any(|doc| doc.body.contains("codixing-test-fail-external-import"))
            {
                return Err(CodixingError::Import(
                    "injected external import failure after resident mutation".to_string(),
                ));
            }

            // ── Commit + publish ─────────────────────────────────────────
            self.tantivy.commit()?;
            self.persist_checkpoint_artifacts()?;
            self.publish_generation_with_preopened_indexes()?;
            Ok(stats)
        })();

        match import_result {
            Ok(stats) => {
                debug!(
                    documents = stats.documents,
                    chunks = stats.chunks,
                    doc_edges = stats.doc_edges,
                    replaced = stats.replaced,
                    "imported external documents"
                );
                Ok(stats)
            }
            Err(error) => Err(self.abort_batch_error(error)),
        }
    }

    /// Remove every indexed chunk whose `file_path` starts with `prefix`
    /// (an `_external/<source>/` namespace). Returns the number of distinct
    /// documents (virtual files) removed. Mirrors the removal half of
    /// [`Engine::reindex_file_impl`] but for virtual paths.
    fn remove_external_prefix(&mut self, prefix: &str) -> Result<usize> {
        // Collect virtual paths from the per-file postings instead of scanning
        // every chunk's duplicated metadata.
        let files: Vec<String> = self
            .file_chunk_ids
            .keys()
            .filter(|path| path.starts_with(prefix))
            .cloned()
            .collect();
        if files.is_empty() {
            return Ok(0);
        }
        for rel in &files {
            self.tantivy.remove_file(rel)?;
            self.file_trigram.get_mut().unwrap().remove_file(rel);
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
        for rel in &files {
            if let Some(chunk_ids) = self.file_chunk_ids.remove(rel) {
                for chunk_id in chunk_ids.iter() {
                    self.chunk_meta.remove(chunk_id);
                }
            }
        }
        Ok(files.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IndexConfig;
    use crate::retriever::{SearchQuery, Strategy};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn import_embedding_batches_are_complete_deterministic_and_bounded() {
        let total = STREAM_BATCH_SIZE * 2 + 17;
        let ranges: Vec<_> = import_embedding_batch_ranges(total).collect();

        assert_eq!(
            ranges,
            vec![
                0..STREAM_BATCH_SIZE,
                STREAM_BATCH_SIZE..STREAM_BATCH_SIZE * 2,
                STREAM_BATCH_SIZE * 2..total,
            ]
        );
        assert!(ranges.iter().all(|range| !range.is_empty()));
        assert!(
            ranges
                .iter()
                .all(|range| range.end - range.start <= STREAM_BATCH_SIZE)
        );
        assert_eq!(ranges.iter().map(|range| range.len()).sum::<usize>(), total);
        assert_eq!(import_embedding_batch_ranges(0).count(), 0);
    }

    #[test]
    fn external_import_keeps_compact_metadata_and_hydrates_search_and_graph_results() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/engine.rs"),
            "pub fn add_chunk() { todo!() }\n",
        )
        .unwrap();
        let mut config = IndexConfig::new(root);
        config.embedding.enabled = false;
        let mut engine = Engine::init(root, config).unwrap();

        const MARKER: &str = "external_resident_hydration_marker_27461";
        let document = ExternalDocument::new(
            "github",
            "issue-compact",
            "Compact external metadata",
            format!("Investigate `{MARKER}` while updating `add_chunk`."),
        );
        let stats = engine.import_external(vec![document]).unwrap();
        assert_eq!(stats.documents, 1);
        assert_eq!(stats.doc_edges, 1);
        assert!(
            engine
                .chunk_meta
                .iter()
                .all(|entry| entry.value().content.is_empty()),
            "external imports must not duplicate Tantivy bodies in resident metadata"
        );

        for strategy in [Strategy::Instant, Strategy::Exact] {
            let results = engine
                .search(
                    SearchQuery::new(MARKER)
                        .with_limit(5)
                        .with_strategy(strategy),
                )
                .unwrap();
            assert!(
                results.iter().any(|result| {
                    result.file_path == "_external/github/issue-compact.md"
                        && result.content.contains(MARKER)
                }),
                "{strategy:?} search must hydrate imported content from Tantivy"
            );
        }

        assert!(
            engine
                .callers("src/engine.rs")
                .iter()
                .any(|path| path == "_external/github/issue-compact.md"),
            "the compact imported metadata must preserve its doc-to-code graph edge"
        );
        let imported_ids = engine
            .file_chunk_ids
            .get("_external/github/issue-compact.md")
            .expect("imported document must retain exact chunk postings");
        assert!(imported_ids.iter().any(|chunk_id| {
            engine
                .resolve_chunk_content(*chunk_id)
                .is_some_and(|content| content.contains(MARKER))
        }));
    }

    #[test]
    fn embedding_enabled_empty_repo_import_bootstraps_and_persists_vectors() {
        let onnx_available = std::env::var_os("ORT_DYLIB_PATH")
            .map(std::path::PathBuf::from)
            .is_some_and(|path| path.exists());
        if !onnx_available {
            eprintln!(
                "SKIP: ONNX runtime unavailable; empty-repo import vector bootstrap not exercised"
            );
            return;
        }

        let dir = tempdir().unwrap();
        let root = dir.path();
        let mut config = IndexConfig::new(root);
        config.embedding.enabled = true;
        let mut engine = Engine::init(root, config).unwrap();
        if engine.embedder.is_none() {
            eprintln!(
                "SKIP: embedding model unavailable; empty-repo import vector bootstrap not exercised"
            );
            return;
        }
        engine.wait_for_embeddings();
        assert!(
            engine
                .vector
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .is_none(),
            "an embedding-enabled empty init has no vector work to publish"
        );

        const MARKER: &str = "empty_repo_import_vector_marker_81742";
        let stats = engine
            .import_external(vec![ExternalDocument::new(
                "github",
                "issue-vector-bootstrap",
                "Bootstrap imported vectors",
                format!("Imported semantic content {MARKER}."),
            )])
            .unwrap();
        assert!(stats.chunks > 0);
        assert_eq!(engine.stats().vector_count, stats.chunks);
        assert!(
            engine
                .chunk_meta
                .iter()
                .all(|entry| entry.value().content.is_empty())
        );
        let results = engine
            .search(
                SearchQuery::new(MARKER)
                    .with_limit(5)
                    .with_strategy(Strategy::Fast),
            )
            .unwrap();
        assert!(results.iter().any(|result| {
            result.file_path == "_external/github/issue-vector-bootstrap.md"
                && result.content.contains(MARKER)
        }));

        drop(engine);
        let reopened = Engine::open_read_only(root).unwrap();
        assert_eq!(reopened.stats().vector_count, stats.chunks);
        assert!(
            reopened
                .chunk_meta
                .iter()
                .all(|entry| entry.value().content.is_empty())
        );
    }
}

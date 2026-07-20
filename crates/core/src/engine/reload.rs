use tracing::{info, warn};

use crate::embedder::Embedder;
use crate::error::Result;
use crate::graph::CodeGraph;
use crate::persistence::IndexStore;
use crate::vector::VectorIndex;

use super::Engine;
use super::indexing::{build_file_trigram_from_tantivy, deserialize_chunk_meta};
use super::init::{load_persisted_symbols, persisted_meta_mtime};

impl Engine {
    /// Set the minimum interval between reload-staleness checks.
    ///
    /// Only meaningful for read-only instances; ignored otherwise.
    pub fn set_reload_interval(&mut self, interval: std::time::Duration) {
        self.reload_interval = interval;
    }

    /// Check if the on-disk index has been updated since this read-only
    /// instance was loaded, and reload if so.
    ///
    /// Returns `Ok(true)` if data was reloaded, `Ok(false)` if no reload
    /// was needed (or this is a read-write instance). No-op if this instance
    /// holds the write lock.
    pub fn reload_if_stale(&mut self) -> Result<bool> {
        if !self.read_only {
            return Ok(false);
        }

        // Rate-limit checks.
        if let Some(last_check) = self.last_staleness_check
            && last_check.elapsed() < self.reload_interval
        {
            return Ok(false);
        }
        let check_time = std::time::Instant::now();
        self.last_staleness_check = Some(check_time);

        // A generation switch replaces every persisted artifact as one
        // coherent snapshot. Build a complete replacement engine first and
        // swap only after it opens successfully, leaving this leased snapshot
        // usable if publication is malformed or incomplete.
        let root = self.config.root.clone();
        let active_generation = IndexStore::active_generation(&root)?;
        let loaded_generation = self.store.generation().map(str::to_owned);
        if active_generation != loaded_generation {
            let reload_interval = self.reload_interval;
            let mut replacement = Self::open_read_only_with_mode(&root, self.read_only_load_mode)?;
            replacement.reload_interval = reload_interval;
            replacement.last_staleness_check = Some(check_time);
            *self = replacement;
            info!(
                generation = ?active_generation,
                "read-only index generation changed — reopened active snapshot"
            );
            return Ok(true);
        }

        let disk_mtime = persisted_meta_mtime(&self.store);
        let mut reloaded = false;
        if matches!(
            (disk_mtime, self.last_load_time),
            (Some(disk), Some(loaded)) if disk > loaded
        ) {
            info!("read-only index stale — reloading from disk");
            self.reload_from_disk()?;
            self.last_load_time = disk_mtime;
            reloaded = true;
        }

        // Background embedding publishes its two vector artifacts after the
        // lexical generation is already active. Their joint existence is the
        // completion signal, so attaching vectors does not require an mtime
        // marker or a corpus-wide lexical reload.
        reloaded |= self.reload_vector_from_disk();
        Ok(reloaded)
    }

    /// Re-read all persistent state from the `.codixing/` directory.
    ///
    /// Reloads symbols, chunk metadata, the dependency graph, the vector
    /// index (if present), and refreshes the Tantivy reader.
    pub(super) fn reload_from_disk(&mut self) -> Result<()> {
        self.symbols = load_persisted_symbols(&self.store)?;

        // Reload chunk_meta (compact format first, fall back to legacy).
        if self.store.chunk_meta_path().exists() {
            let bytes = self.store.load_chunk_meta_bytes()?;
            let loaded = deserialize_chunk_meta(&bytes)?;
            self.chunk_meta.clear();
            for entry in loaded.iter() {
                self.chunk_meta.insert(*entry.key(), entry.value().clone());
            }
        }

        // Rebuild exact per-file chunk postings from compact metadata.
        self.file_chunk_ids = super::collect_file_chunk_ids(&self.chunk_meta);

        // Refresh first so the rebuild observes the newest committed segments.
        self.tantivy.refresh_reader()?;

        // Reload the published shared trigram artifact so raw-source grep
        // trigrams and parser-transformed exact-search trigrams stay intact.
        // Indexes old enough to predate both trigram artifacts retain their
        // historical Tantivy recovery path. Once either half of the durable
        // pair exists, malformed or unpaired state must fail the reload rather
        // than resurrecting deleted/replaced paths from a stale base.
        let ft = if !self.store.file_trigram_path().exists()
            && !self.store.file_trigram_delta_path().exists()
        {
            if self.store.file_trigram_delta_required() {
                return Err(crate::error::CodixingError::Serialization(
                    "active generation is missing the required file trigram pair".to_string(),
                ));
            }
            build_file_trigram_from_tantivy(&self.tantivy)
        } else {
            super::load_persisted_file_trigram(&self.store)?
        };
        self.file_trigram = std::sync::OnceLock::new();
        let _ = self.file_trigram.set(ft);

        if self.read_only_load_mode.loads_graph() {
            self.graph = match self.store.load_graph() {
                Ok(Some(data)) => {
                    let mut g = CodeGraph::from_flat(data);
                    match self.store.load_symbol_graph() {
                        Ok(Some(sym_graph)) => {
                            g.replace_symbol_graph(sym_graph);
                        }
                        Ok(None) => {}
                        Err(e) => {
                            warn!(error = %e, "failed to load symbol graph during reload");
                        }
                    }
                    Some(g)
                }
                Ok(None) => None,
                Err(e) => {
                    warn!(error = %e, "failed to reload graph");
                    self.graph.take()
                }
            };
        } else {
            self.graph = None;
        }

        self.reload_vector_from_disk();

        // Reset lazy caches so the next access re-reads fresh data from disk.
        self.concept_index = std::sync::OnceLock::new();
        self.reformulations = std::sync::OnceLock::new();

        info!("engine reloaded from disk");
        Ok(())
    }

    /// Attach a complete vector checkpoint without touching lexical state.
    ///
    /// Each atomically published vector generation has its own identity, so a
    /// resident reader replaces old/empty checkpoints as post-hoc embedding
    /// progresses. Failures preserve the previous snapshot and are retried on
    /// a later staleness check.
    fn reload_vector_from_disk(&mut self) -> bool {
        if !self.read_only_load_mode.loads_vectors() {
            return false;
        }

        let index_path = self.store.vector_index_path();
        let file_chunks_path = self.store.file_chunks_path();
        let Some(publication) = crate::vector::publication_token(&index_path, &file_chunks_path)
        else {
            return false;
        };
        if self.last_vector_publication.as_ref() == Some(&publication) {
            return false;
        }

        // `embed_remaining()` can enable embeddings after this engine opened.
        // Refresh the persisted embedding settings independently of meta.json
        // before interpreting the new vector checkpoint.
        let disk_config = match self.store.load_config() {
            Ok(config) => config,
            Err(error) => {
                warn!(%error, "failed to reload embedding config");
                return false;
            }
        };
        if !disk_config.embedding.enabled {
            return false;
        }
        let disk_embedding = disk_config.embedding;
        let candidate_embedder = if self.config.embedding.model == disk_embedding.model {
            self.embedder.clone()
        } else {
            None
        };
        let candidate_embedder = match candidate_embedder {
            Some(embedder) => embedder,
            None => match Embedder::new(&disk_embedding.model) {
                Ok(embedder) => std::sync::Arc::new(embedder),
                Err(error) => {
                    warn!(%error, "failed to load embedding model during read-only reload");
                    return false;
                }
            },
        };
        match VectorIndex::load(
            &index_path,
            &file_chunks_path,
            candidate_embedder.dims,
            disk_embedding.quantize,
        ) {
            Ok(vector) => {
                // Commit the complete model/config/vector snapshot together.
                // Failed candidate construction above leaves every previously
                // attached hybrid-search component untouched.
                self.config.embedding = disk_embedding;
                self.embedder = Some(candidate_embedder);
                *self
                    .vector
                    .write()
                    .unwrap_or_else(|error| error.into_inner()) = Some(vector);
                // If another checkpoint won the race while this one loaded,
                // leave the token unset so the next check attaches it too.
                self.last_vector_publication =
                    (crate::vector::publication_token(&index_path, &file_chunks_path)
                        == Some(publication.clone()))
                    .then_some(publication);
                info!("read-only engine attached completed vector checkpoint");
                true
            }
            Err(error) => {
                warn!(%error, "failed to reload vector index");
                false
            }
        }
    }
}

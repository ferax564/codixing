use tracing::{debug, info, warn};

use crate::error::Result;
use crate::graph::CodeGraph;
use crate::symbols::SymbolTable;
use crate::symbols::persistence::deserialize_symbols;
use crate::vector::VectorIndex;

use super::Engine;
use super::indexing::{
    build_file_trigram_from_tantivy, deserialize_chunk_meta, rebuild_trigram_from_tantivy,
};

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
        if let Some(last_check) = self.last_staleness_check {
            if last_check.elapsed() < self.reload_interval {
                return Ok(false);
            }
        }
        self.last_staleness_check = Some(std::time::Instant::now());

        let meta_path = self.store.codixing_dir().join("meta.json");
        let disk_mtime = std::fs::metadata(&meta_path)
            .ok()
            .and_then(|m| m.modified().ok());

        match (disk_mtime, self.last_load_time) {
            (Some(disk), Some(loaded)) if disk > loaded => {
                info!("read-only index stale — reloading from disk");
                self.reload_from_disk()?;
                self.last_load_time = Some(disk);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Re-read all persistent state from the `.codixing/` directory.
    ///
    /// Reloads symbols, chunk metadata, the dependency graph, the vector
    /// index (if present), and refreshes the Tantivy reader.
    fn reload_from_disk(&mut self) -> Result<()> {
        // Reload symbols: try mmap v2 first, fall back to bitcode v1.
        if self.store.symbols_v2_path().exists() {
            match crate::symbols::mmap::MmapSymbolTable::load(&self.store.symbols_v2_path()) {
                Ok(mmap_table) => {
                    debug!("reloaded symbols_v2.bin via mmap");
                    self.symbols = SymbolTable::Mmap(mmap_table);
                }
                Err(e) => {
                    warn!(error = %e, "failed to reload symbols_v2.bin — trying symbols.bin");
                    if self.store.symbols_path().exists() {
                        let bytes = self.store.load_symbols_bytes()?;
                        self.symbols = deserialize_symbols(&bytes)?;
                    }
                }
            }
        } else if self.store.symbols_path().exists() {
            let bytes = self.store.load_symbols_bytes()?;
            self.symbols = deserialize_symbols(&bytes)?;
        }

        // Reload chunk_meta (compact format first, fall back to legacy).
        if self.store.chunk_meta_path().exists() {
            let bytes = self.store.load_chunk_meta_bytes()?;
            let loaded = deserialize_chunk_meta(&bytes)?;
            self.chunk_meta.clear();
            for entry in loaded.iter() {
                self.chunk_meta.insert(*entry.key(), entry.value().clone());
            }
        }

        // Rebuild file_chunk_counts from chunk_meta.
        self.file_chunk_counts.clear();
        for entry in self.chunk_meta.iter() {
            *self
                .file_chunk_counts
                .entry(entry.value().file_path.clone())
                .or_insert(0) += 1;
        }

        // Rebuild file trigram index from Tantivy (chunk_meta may have empty content).
        let ft = build_file_trigram_from_tantivy(&self.tantivy);
        self.file_trigram = std::sync::OnceLock::new();
        let _ = self.file_trigram.set(ft);

        // Reload graph.
        self.graph = match self.store.load_graph() {
            Ok(Some(data)) => {
                let mut g = CodeGraph::from_flat(data);
                match self.store.load_symbol_graph() {
                    Ok(Some(sym_graph)) => {
                        g.inner = sym_graph.inner;
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

        // Reload vector index if it exists and we have an embedder.
        if let Some(ref emb) = self.embedder {
            if self.store.vector_index_path().exists() && self.store.file_chunks_path().exists() {
                match VectorIndex::load(
                    &self.store.vector_index_path(),
                    &self.store.file_chunks_path(),
                    emb.dims,
                    self.config.embedding.quantize,
                ) {
                    Ok(vec_idx) => {
                        *self.vector.write().unwrap_or_else(|e| e.into_inner()) = Some(vec_idx);
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to reload vector index");
                    }
                }
            }
        }

        // Refresh the Tantivy reader FIRST so the trigram rebuild below reads
        // the newest committed segments. Previously this was done after the
        // rebuild, which meant the caches were populated from the old
        // pre-sync segment view — stale for ~one reload cycle.
        self.tantivy.refresh_reader()?;

        // Rebuild trigram index from Tantivy content (chunk_meta may have empty content).
        let t = rebuild_trigram_from_tantivy(&self.tantivy);
        self.trigram = std::sync::OnceLock::new();
        let _ = self.trigram.set(t);

        // Reset lazy caches so the next access re-reads fresh data from disk.
        self.concept_index = std::sync::OnceLock::new();
        self.reformulations = std::sync::OnceLock::new();

        info!("read-only engine reloaded from disk");
        Ok(())
    }
}

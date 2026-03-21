pub mod qdrant;

use std::collections::HashMap;
use std::path::Path;

#[cfg(feature = "usearch")]
use usearch::{Index, IndexOptions, MetricKind, ScalarKind, new_index};

use crate::error::{CodixingError, Result};

/// Pluggable vector search backend.
///
/// Implement this trait to provide alternative storage for code chunk vectors.
/// The default implementation is [`VectorIndex`] (usearch HNSW, in-process).
/// An optional Qdrant backend is available behind `#[cfg(feature = "qdrant")]`.
pub trait VectorBackend: Send + Sync {
    /// Add a vector associated with `chunk_id` and `file_path`.
    fn add(&self, chunk_id: u64, vector: &[f32], file_path: &str) -> Result<()>;

    /// Search for the `k` nearest vectors to `query`.
    ///
    /// Returns `(chunk_id, score)` pairs.
    fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u64, f32)>>;

    /// Remove all vectors associated with `file_path`.
    fn remove_file(&mut self, file_path: &str) -> Result<()>;

    /// Total number of indexed vectors.
    fn size(&self) -> usize;

    /// Return the file-to-chunk-id mapping (owned clone, suitable for diagnostics).
    fn file_chunks_owned(&self) -> HashMap<String, Vec<u64>>;

    /// Persist the index to `dir`.
    fn save(&self, dir: &Path) -> Result<()>;
}

// ===========================================================================
// usearch-backed VectorIndex (default on Linux / macOS)
// ===========================================================================

#[cfg(feature = "usearch")]
mod usearch_impl {
    use super::*;

    /// Approximate nearest-neighbour HNSW index backed by usearch.
    ///
    /// Wraps a usearch [`Index`] and maintains a per-file chunk map
    /// (`file_chunks`) so entire files can be efficiently removed.
    pub struct VectorIndex {
        inner: Index,
        /// Maps file path -> list of chunk IDs stored in this index.
        file_chunks: HashMap<String, Vec<u64>>,
        /// Vector dimensionality (must match the embedder).
        pub dims: usize,
    }

    impl VectorIndex {
        /// Create a new empty index with the given vector dimensionality.
        ///
        /// When `quantize` is `true` the HNSW graph stores vectors as int8 instead
        /// of float32, reducing memory usage by 8x -- critical for repos with 1 M+
        /// LoC where the vector index alone can exceed 2 GB at full precision.
        pub fn new(dims: usize, quantize: bool) -> Result<Self> {
            let options = IndexOptions {
                dimensions: dims,
                metric: MetricKind::Cos,
                quantization: if quantize {
                    ScalarKind::I8
                } else {
                    ScalarKind::F32
                },
                connectivity: 0,
                expansion_add: 0,
                expansion_search: 0,
                multi: false,
            };
            let inner = new_index(&options)
                .map_err(|e| CodixingError::VectorIndex(format!("failed to create index: {e}")))?;
            Ok(Self {
                inner,
                file_chunks: HashMap::new(),
                dims,
            })
        }

        /// Add a vector to the index, associating it with `chunk_id`.
        ///
        /// `file_path` is tracked so the chunk can be removed when the file
        /// is removed from the index.
        pub fn add(&self, chunk_id: u64, vector: &[f32], file_path: &str) -> Result<()> {
            // Reserve additional capacity if needed (usearch grows, but an explicit
            // reserve of +1 keeps performance predictable).
            let needed = self.inner.size() + 1;
            self.inner
                .reserve(needed)
                .map_err(|e| CodixingError::VectorIndex(format!("reserve failed: {e}")))?;
            self.inner
                .add(chunk_id, vector)
                .map_err(|e| CodixingError::VectorIndex(format!("add failed: {e}")))?;
            // Caller is responsible for updating file_chunks (needs &mut self).
            let _ = file_path; // acknowledged here; see add_mut below
            Ok(())
        }

        /// Add a vector and record the file->chunk mapping (requires `&mut self`).
        pub fn add_mut(&mut self, chunk_id: u64, vector: &[f32], file_path: &str) -> Result<()> {
            let needed = self.inner.size() + 1;
            self.inner
                .reserve(needed)
                .map_err(|e| CodixingError::VectorIndex(format!("reserve failed: {e}")))?;
            self.inner
                .add(chunk_id, vector)
                .map_err(|e| CodixingError::VectorIndex(format!("add failed: {e}")))?;
            self.file_chunks
                .entry(file_path.to_string())
                .or_default()
                .push(chunk_id);
            Ok(())
        }

        /// Search for the `k` nearest vectors to `query`.
        ///
        /// Returns a list of `(chunk_id, distance)` pairs sorted by ascending distance.
        pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u64, f32)>> {
            if self.inner.size() == 0 {
                return Ok(Vec::new());
            }
            let matches = self
                .inner
                .search(query, k)
                .map_err(|e| CodixingError::VectorIndex(format!("search failed: {e}")))?;
            Ok(matches.keys.into_iter().zip(matches.distances).collect())
        }

        /// Remove all vectors belonging to the given file.
        pub fn remove_file(&mut self, file_path: &str) -> Result<()> {
            if let Some(chunk_ids) = self.file_chunks.remove(file_path) {
                for id in chunk_ids {
                    // Ignore errors for individual removes (chunk may not be present).
                    let _ = self.inner.remove(id);
                }
            }
            Ok(())
        }

        /// Total number of vectors currently in the index.
        pub fn len(&self) -> usize {
            self.inner.size()
        }

        /// Returns `true` if the index contains no vectors.
        pub fn is_empty(&self) -> bool {
            self.len() == 0
        }

        /// Persist the HNSW graph to `index_path` and the file-chunk map to
        /// `file_chunks_path` (bitcode binary).
        pub fn save(&self, index_path: &Path, file_chunks_path: &Path) -> Result<()> {
            self.inner
                .save(index_path.to_string_lossy().as_ref())
                .map_err(|e| CodixingError::VectorIndex(format!("save index failed: {e}")))?;

            let bytes = bitcode::serialize(&self.file_chunks).map_err(|e| {
                CodixingError::Serialization(format!("failed to serialize file_chunks: {e}"))
            })?;
            std::fs::write(file_chunks_path, bytes)?;
            Ok(())
        }

        /// Load an existing index from disk.
        ///
        /// Creates a fresh usearch `Index` with matching options then loads the
        /// persisted graph and the file-chunk map.  `quantize` must match the
        /// setting used when the index was originally created.
        pub fn load(
            index_path: &Path,
            file_chunks_path: &Path,
            dims: usize,
            quantize: bool,
        ) -> Result<Self> {
            let idx = Self::new(dims, quantize)?;
            idx.inner
                .load(index_path.to_string_lossy().as_ref())
                .map_err(|e| CodixingError::VectorIndex(format!("load index failed: {e}")))?;

            let bytes = std::fs::read(file_chunks_path)?;
            let file_chunks: HashMap<String, Vec<u64>> =
                bitcode::deserialize(&bytes).map_err(|e| {
                    CodixingError::Serialization(format!("failed to deserialize file_chunks: {e}"))
                })?;

            Ok(Self {
                inner: idx.inner,
                file_chunks,
                dims,
            })
        }

        /// Access the file-chunk map (for persistence).
        pub fn file_chunks(&self) -> &HashMap<String, Vec<u64>> {
            &self.file_chunks
        }

        /// Retrieve the stored vector for the given `chunk_id`.
        ///
        /// Returns `None` if the chunk is not in the index.  Uses the usearch
        /// `get` API to read the vector back from the HNSW graph.
        pub fn get_vector(&self, chunk_id: u64) -> Option<Vec<f32>> {
            let mut buf = vec![0.0f32; self.dims];
            match self.inner.get(chunk_id, &mut buf) {
                Ok(found) if found > 0 => Some(buf),
                _ => None,
            }
        }
    }

    impl VectorBackend for VectorIndex {
        fn add(&self, chunk_id: u64, vector: &[f32], _file_path: &str) -> Result<()> {
            // Shared-reference add (no file_chunks tracking). Use add_mut for full tracking.
            let needed = self.inner.size() + 1;
            self.inner
                .reserve(needed)
                .map_err(|e| CodixingError::VectorIndex(format!("reserve failed: {e}")))?;
            self.inner
                .add(chunk_id, vector)
                .map_err(|e| CodixingError::VectorIndex(format!("add failed: {e}")))?;
            Ok(())
        }

        fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u64, f32)>> {
            VectorIndex::search(self, query, k)
        }

        fn remove_file(&mut self, file_path: &str) -> Result<()> {
            VectorIndex::remove_file(self, file_path)
        }

        fn size(&self) -> usize {
            self.len()
        }

        fn file_chunks_owned(&self) -> HashMap<String, Vec<u64>> {
            self.file_chunks.clone()
        }

        fn save(&self, dir: &Path) -> Result<()> {
            let index_path = dir.join("vectors.usearch");
            let file_chunks_path = dir.join("file_chunks.bin");
            VectorIndex::save(self, &index_path, &file_chunks_path)
        }
    }
}

#[cfg(feature = "usearch")]
pub use usearch_impl::VectorIndex;

// ===========================================================================
// Brute-force fallback VectorIndex (used on Windows / --no-default-features)
// ===========================================================================

#[cfg(not(feature = "usearch"))]
mod brute_force_impl {
    use super::*;

    /// Brute-force vector index using cosine similarity.
    ///
    /// Drop-in replacement for the usearch-backed `VectorIndex` when the
    /// `usearch` feature is disabled (e.g. on Windows where usearch uses
    /// POSIX `MAP_FAILED`). O(N) per query but works on all platforms.
    pub struct VectorIndex {
        /// Per-chunk vectors, keyed by chunk ID.
        entries: Vec<(u64, Vec<f32>)>,
        /// Maps file path -> list of chunk IDs stored in this index.
        file_chunks: HashMap<String, Vec<u64>>,
        /// Vector dimensionality (must match the embedder).
        pub dims: usize,
    }

    impl VectorIndex {
        /// Create a new empty index with the given vector dimensionality.
        ///
        /// The `quantize` parameter is accepted for API compatibility with
        /// the usearch backend but is ignored (brute-force always uses f32).
        pub fn new(dims: usize, _quantize: bool) -> Result<Self> {
            Ok(Self {
                entries: Vec::new(),
                file_chunks: HashMap::new(),
                dims,
            })
        }

        /// Add a vector to the index, associating it with `chunk_id`.
        ///
        /// `file_path` is tracked so the chunk can be removed when the file
        /// is removed from the index.
        pub fn add(&self, _chunk_id: u64, _vector: &[f32], _file_path: &str) -> Result<()> {
            // Shared-reference add is not supported in the brute-force backend.
            // The engine always uses add_mut, so this is only for VectorBackend trait compat.
            Err(CodixingError::VectorIndex(
                "brute-force VectorIndex requires &mut self; use add_mut instead".to_string(),
            ))
        }

        /// Add a vector and record the file->chunk mapping (requires `&mut self`).
        pub fn add_mut(&mut self, chunk_id: u64, vector: &[f32], file_path: &str) -> Result<()> {
            if vector.len() != self.dims {
                return Err(CodixingError::VectorIndex(format!(
                    "vector dimension mismatch: expected {}, got {}",
                    self.dims,
                    vector.len()
                )));
            }
            // Update existing entry or push new one.
            if let Some(entry) = self.entries.iter_mut().find(|(id, _)| *id == chunk_id) {
                entry.1 = vector.to_vec();
            } else {
                self.entries.push((chunk_id, vector.to_vec()));
            }
            self.file_chunks
                .entry(file_path.to_string())
                .or_default()
                .push(chunk_id);
            Ok(())
        }

        /// Search for the `k` nearest vectors to `query`.
        ///
        /// Returns a list of `(chunk_id, distance)` pairs sorted by ascending
        /// cosine distance (0.0 = identical, 1.0 = orthogonal), matching the
        /// usearch backend's return convention.
        pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u64, f32)>> {
            if self.entries.is_empty() || k == 0 {
                return Ok(Vec::new());
            }

            const BRUTE_FORCE_WARN_THRESHOLD: usize = 50_000;
            if self.entries.len() > BRUTE_FORCE_WARN_THRESHOLD {
                tracing::warn!(
                    count = self.entries.len(),
                    "brute-force vector search over {} vectors — consider enabling the \
                     `usearch` feature for sub-linear ANN search",
                    self.entries.len()
                );
            }

            let mut scored: Vec<(u64, f32)> = self
                .entries
                .iter()
                .map(|(id, vec)| {
                    let sim = cosine_similarity(query, vec);
                    // Convert to distance: usearch returns cosine distance, not similarity.
                    (*id, 1.0 - sim)
                })
                .collect();

            // Sort by ascending distance (lowest = most similar).
            scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            scored.truncate(k);
            Ok(scored)
        }

        /// Remove all vectors belonging to the given file.
        pub fn remove_file(&mut self, file_path: &str) -> Result<()> {
            if let Some(chunk_ids) = self.file_chunks.remove(file_path) {
                let id_set: std::collections::HashSet<u64> = chunk_ids.into_iter().collect();
                self.entries.retain(|(id, _)| !id_set.contains(id));
            }
            Ok(())
        }

        /// Total number of vectors currently in the index.
        pub fn len(&self) -> usize {
            self.entries.len()
        }

        /// Returns `true` if the index contains no vectors.
        pub fn is_empty(&self) -> bool {
            self.entries.is_empty()
        }

        /// Persist the index to `index_path` (JSON) and the file-chunk map to
        /// `file_chunks_path` (bitcode binary).
        pub fn save(&self, index_path: &Path, file_chunks_path: &Path) -> Result<()> {
            // Save vectors as JSON for cross-platform portability.
            let data = serde_json::json!({
                "type": "brute_force",
                "dims": self.dims,
                "entries": self.entries.iter().map(|(id, vec)| {
                    serde_json::json!({ "chunk_id": id, "vector": vec })
                }).collect::<Vec<_>>(),
            });
            let bytes = serde_json::to_vec(&data).map_err(|e| {
                CodixingError::VectorIndex(format!("failed to serialize vector index: {e}"))
            })?;
            std::fs::write(index_path, bytes)?;

            let fc_bytes = bitcode::serialize(&self.file_chunks).map_err(|e| {
                CodixingError::Serialization(format!("failed to serialize file_chunks: {e}"))
            })?;
            std::fs::write(file_chunks_path, fc_bytes)?;
            Ok(())
        }

        /// Load an existing index from disk.
        ///
        /// The `quantize` parameter is accepted for API compatibility but ignored.
        pub fn load(
            index_path: &Path,
            file_chunks_path: &Path,
            dims: usize,
            _quantize: bool,
        ) -> Result<Self> {
            let bytes = std::fs::read(index_path)?;
            let data: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
                CodixingError::VectorIndex(format!("failed to deserialize vector index: {e}"))
            })?;

            let mut entries = Vec::new();
            if let Some(arr) = data["entries"].as_array() {
                for entry in arr {
                    let chunk_id = entry["chunk_id"].as_u64().unwrap_or(0);
                    let vector: Vec<f32> = entry["vector"]
                        .as_array()
                        .map(|a| a.iter().map(|v| v.as_f64().unwrap_or(0.0) as f32).collect())
                        .unwrap_or_default();
                    entries.push((chunk_id, vector));
                }
            }

            let fc_bytes = std::fs::read(file_chunks_path)?;
            let file_chunks: HashMap<String, Vec<u64>> =
                bitcode::deserialize(&fc_bytes).map_err(|e| {
                    CodixingError::Serialization(format!("failed to deserialize file_chunks: {e}"))
                })?;

            Ok(Self {
                entries,
                file_chunks,
                dims,
            })
        }

        /// Access the file-chunk map (for persistence).
        pub fn file_chunks(&self) -> &HashMap<String, Vec<u64>> {
            &self.file_chunks
        }

        /// Retrieve the stored vector for the given `chunk_id`.
        ///
        /// Returns `None` if the chunk is not in the index.
        pub fn get_vector(&self, chunk_id: u64) -> Option<Vec<f32>> {
            self.entries
                .iter()
                .find(|(id, _)| *id == chunk_id)
                .map(|(_, vec)| vec.clone())
        }
    }

    impl VectorBackend for VectorIndex {
        fn add(&self, chunk_id: u64, vector: &[f32], file_path: &str) -> Result<()> {
            VectorIndex::add(self, chunk_id, vector, file_path)
        }

        fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u64, f32)>> {
            VectorIndex::search(self, query, k)
        }

        fn remove_file(&mut self, file_path: &str) -> Result<()> {
            VectorIndex::remove_file(self, file_path)
        }

        fn size(&self) -> usize {
            self.len()
        }

        fn file_chunks_owned(&self) -> HashMap<String, Vec<u64>> {
            self.file_chunks.clone()
        }

        fn save(&self, dir: &Path) -> Result<()> {
            let index_path = dir.join("vectors.usearch");
            let file_chunks_path = dir.join("file_chunks.bin");
            VectorIndex::save(self, &index_path, &file_chunks_path)
        }
    }

    /// Cosine similarity between two vectors.
    ///
    /// Returns a value in [-1.0, 1.0]. Returns 0.0 if either vector has zero magnitude.
    fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        let mut dot = 0.0f32;
        let mut norm_a = 0.0f32;
        let mut norm_b = 0.0f32;
        for (x, y) in a.iter().zip(b.iter()) {
            dot += x * y;
            norm_a += x * x;
            norm_b += y * y;
        }
        let denom = norm_a.sqrt() * norm_b.sqrt();
        if denom == 0.0 { 0.0 } else { dot / denom }
    }
}

#[cfg(not(feature = "usearch"))]
pub use brute_force_impl::VectorIndex;

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_vec(dims: usize, dominant: usize) -> Vec<f32> {
        let mut v = vec![0.01f32; dims];
        v[dominant] = 1.0;
        v
    }

    #[test]
    fn add_and_search() {
        let mut idx = VectorIndex::new(4, false).unwrap();
        let a = unit_vec(4, 0);
        let b = unit_vec(4, 1);
        let c = unit_vec(4, 2);
        idx.add_mut(1, &a, "a.rs").unwrap();
        idx.add_mut(2, &b, "b.rs").unwrap();
        idx.add_mut(3, &c, "c.rs").unwrap();

        // Query close to 'a' -- should rank chunk 1 first.
        let results = idx.search(&a, 3).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].0, 1);
    }

    #[test]
    fn remove_file_drops_vectors() {
        let mut idx = VectorIndex::new(4, false).unwrap();
        idx.add_mut(10, &unit_vec(4, 0), "x.rs").unwrap();
        idx.add_mut(11, &unit_vec(4, 1), "y.rs").unwrap();

        idx.remove_file("x.rs").unwrap();

        // x.rs chunks should be gone; y.rs still present.
        assert!(!idx.file_chunks().contains_key("x.rs"));
        assert!(idx.file_chunks().contains_key("y.rs"));
    }

    #[test]
    fn empty_index_search_returns_empty() {
        let idx = VectorIndex::new(4, false).unwrap();
        let results = idx.search(&unit_vec(4, 0), 5).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn vector_index_implements_backend_trait() {
        // Compile-time check: VectorIndex must satisfy VectorBackend.
        fn _assert_backend<T: VectorBackend>() {}
        _assert_backend::<VectorIndex>();
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let idx_path = dir.path().join("test.usearch");
        let fc_path = dir.path().join("file_chunks.bin");

        let mut idx = VectorIndex::new(4, false).unwrap();
        idx.add_mut(42, &unit_vec(4, 0), "foo.rs").unwrap();
        idx.save(&idx_path, &fc_path).unwrap();

        let loaded = VectorIndex::load(&idx_path, &fc_path, 4, false).unwrap();
        assert_eq!(loaded.len(), 1);
        let results = loaded.search(&unit_vec(4, 0), 1).unwrap();
        assert_eq!(results[0].0, 42);
    }

    /// Verify 384d vectors (BgeSmallEn) work with and without quantization.
    #[test]
    fn dims_384_f32_and_quantized() {
        for quantize in [false, true] {
            let mut idx = VectorIndex::new(384, quantize).unwrap();
            let a = unit_vec(384, 0);
            let b = unit_vec(384, 100);
            idx.add_mut(1, &a, "a.rs").unwrap();
            idx.add_mut(2, &b, "b.rs").unwrap();
            assert_eq!(idx.len(), 2, "quantize={quantize}: expected 2 vectors");
            let results = idx.search(&a, 2).unwrap();
            assert_eq!(
                results[0].0, 1,
                "quantize={quantize}: nearest should be chunk 1"
            );
        }
    }

    /// Verify 768d vectors (BgeBaseEn) work with and without quantization.
    #[test]
    fn dims_768_f32_and_quantized() {
        for quantize in [false, true] {
            let mut idx = VectorIndex::new(768, quantize).unwrap();
            let a = unit_vec(768, 0);
            let b = unit_vec(768, 500);
            idx.add_mut(1, &a, "a.rs").unwrap();
            idx.add_mut(2, &b, "b.rs").unwrap();
            assert_eq!(idx.len(), 2, "quantize={quantize}: expected 2 vectors");
            let results = idx.search(&a, 2).unwrap();
            assert_eq!(
                results[0].0, 1,
                "quantize={quantize}: nearest should be chunk 1"
            );
        }
    }

    /// Verify 1024d vectors (BgeLargeEn / SnowflakeArctic / Qwen3) work.
    ///
    /// This is a regression test for the 1024d vector index bug where adds
    /// silently failed for high-dimension vectors.
    #[test]
    fn dims_1024_f32_and_quantized() {
        for quantize in [false, true] {
            let mut idx = VectorIndex::new(1024, quantize).unwrap();
            let a = unit_vec(1024, 0);
            let b = unit_vec(1024, 512);
            let c = unit_vec(1024, 1023);
            idx.add_mut(1, &a, "a.rs").unwrap();
            idx.add_mut(2, &b, "b.rs").unwrap();
            idx.add_mut(3, &c, "c.rs").unwrap();
            assert_eq!(idx.len(), 3, "quantize={quantize}: expected 3 vectors");
            let results = idx.search(&a, 3).unwrap();
            assert_eq!(
                results[0].0, 1,
                "quantize={quantize}: nearest should be chunk 1"
            );
        }
    }

    /// Verify 1024d save/load round-trip works correctly.
    #[test]
    fn dims_1024_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let idx_path = dir.path().join("test_1024.usearch");
        let fc_path = dir.path().join("fc_1024.bin");

        let mut idx = VectorIndex::new(1024, true).unwrap();
        for i in 0..50u64 {
            let mut v = vec![0.01f32; 1024];
            v[(i as usize) % 1024] = 1.0;
            idx.add_mut(i, &v, &format!("file_{i}.rs")).unwrap();
        }
        assert_eq!(idx.len(), 50);
        idx.save(&idx_path, &fc_path).unwrap();

        let loaded = VectorIndex::load(&idx_path, &fc_path, 1024, true).unwrap();
        assert_eq!(loaded.len(), 50);
        let results = loaded.search(&unit_vec(1024, 0), 5).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].0, 0); // chunk 0 has dominant dim 0
    }

    #[test]
    fn get_vector_retrieves_stored() {
        let mut idx = VectorIndex::new(4, false).unwrap();
        let a = unit_vec(4, 0);
        idx.add_mut(42, &a, "a.rs").unwrap();

        let retrieved = idx.get_vector(42).expect("vector should exist");
        // Cosine distance is used, so the retrieved vector should be very close.
        for (orig, got) in a.iter().zip(retrieved.iter()) {
            assert!(
                (orig - got).abs() < 0.01,
                "vector mismatch: orig={orig} got={got}"
            );
        }

        // Non-existent chunk should return None.
        assert!(idx.get_vector(999).is_none());
    }

    // -----------------------------------------------------------------------
    // Brute-force-specific tests (run on both backends to ensure parity)
    // -----------------------------------------------------------------------

    #[test]
    fn brute_force_search_returns_nearest() {
        let mut idx = VectorIndex::new(3, false).unwrap();
        idx.add_mut(1, &[1.0, 0.0, 0.0], "a.rs").unwrap();
        idx.add_mut(2, &[0.0, 1.0, 0.0], "b.rs").unwrap();
        idx.add_mut(3, &[0.9, 0.1, 0.0], "c.rs").unwrap();

        let results = idx.search(&[1.0, 0.0, 0.0], 2).unwrap();
        assert_eq!(results.len(), 2);
        // Chunk 1 is an exact match, should be first (lowest distance).
        assert_eq!(results[0].0, 1);
        assert!(
            results[0].1 < 0.01,
            "exact match should have near-zero distance"
        );
    }

    #[test]
    fn brute_force_add_remove() {
        let mut idx = VectorIndex::new(3, false).unwrap();
        idx.add_mut(10, &[1.0, 0.0, 0.0], "a.rs").unwrap();
        idx.add_mut(20, &[0.0, 1.0, 0.0], "a.rs").unwrap();
        idx.add_mut(30, &[0.0, 0.0, 1.0], "b.rs").unwrap();
        assert_eq!(idx.len(), 3);

        idx.remove_file("a.rs").unwrap();
        assert_eq!(idx.len(), 1);

        let results = idx.search(&[1.0, 0.0, 0.0], 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 30);
    }

    #[test]
    fn brute_force_empty_search() {
        let idx = VectorIndex::new(4, false).unwrap();
        let results = idx.search(&[1.0, 0.0, 0.0, 0.0], 10).unwrap();
        assert!(results.is_empty());
    }
}

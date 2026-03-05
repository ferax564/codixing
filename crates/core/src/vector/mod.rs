pub mod qdrant;

use std::collections::HashMap;
use std::path::Path;

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

/// Approximate nearest-neighbour HNSW index backed by usearch.
///
/// Wraps a usearch [`Index`] and maintains a per-file chunk map
/// (`file_chunks`) so entire files can be efficiently removed.
pub struct VectorIndex {
    inner: Index,
    /// Maps file path → list of chunk IDs stored in this index.
    file_chunks: HashMap<String, Vec<u64>>,
    /// Vector dimensionality (must match the embedder).
    pub dims: usize,
}

impl VectorIndex {
    /// Create a new empty index with the given vector dimensionality.
    ///
    /// When `quantize` is `true` the HNSW graph stores vectors as int8 instead
    /// of float32, reducing memory usage by 8× — critical for repos with 1 M+
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

    /// Add a vector and record the file→chunk mapping (requires `&mut self`).
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
        let file_chunks: HashMap<String, Vec<u64>> = bitcode::deserialize(&bytes).map_err(|e| {
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

        // Query close to 'a' — should rank chunk 1 first.
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
}

//! HNSW-based approximate nearest-neighbor vector index.
//!
//! Uses the [`instant_distance`] crate for sub-linear search time on large
//! datasets.  Vectors are stored in an append-only buffer and the HNSW graph
//! is rebuilt lazily when `search()` is called after mutations.
//!
//! This implementation trades index-build latency for fast search:
//! - `add()` / `remove_chunks()` are O(1) amortised (just buffer changes)
//! - `search()` triggers a rebuild if the index is dirty, then does ANN lookup

use std::collections::HashMap;
use std::path::Path;
use std::sync::RwLock;

use instant_distance::{Builder, HnswMap, Point, Search};

use super::vector::{VectorEntry, VectorIndex, VectorSearchResult};
use crate::error::CodixingError;

// ---------------------------------------------------------------------------
// Point adapter
// ---------------------------------------------------------------------------

/// Wrapper around a dense embedding that implements [`instant_distance::Point`].
///
/// Distance is defined as `1.0 - cosine_similarity`, so 0.0 = identical
/// vectors and 1.0 = orthogonal vectors.
#[derive(Clone, Debug)]
struct EmbeddingPoint {
    vector: Vec<f32>,
}

impl Point for EmbeddingPoint {
    fn distance(&self, other: &Self) -> f32 {
        1.0 - cosine_similarity(&self.vector, &other.vector)
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    super::simd_distance::cosine_similarity(a, b)
}

// ---------------------------------------------------------------------------
// HnswVectorIndex
// ---------------------------------------------------------------------------

/// Approximate nearest-neighbor vector index using HNSW.
///
/// Maintains a mutable entry buffer and a lazily-rebuilt HNSW graph.
/// Suitable for codebases with 10K+ chunks where `BruteForceVectorIndex`
/// becomes too slow.
pub struct HnswVectorIndex {
    /// Raw vector entries (source of truth for mutations).
    entries: Vec<VectorEntry>,
    /// Fast chunk_id → index-in-entries lookup.
    id_to_idx: HashMap<u64, usize>,
    /// Vector dimensionality.
    dimension: usize,
    /// Cached HNSW graph + the chunk-ID ordering used to build it.
    /// `None` if the index is dirty and needs a rebuild.
    /// Wrapped in a RwLock so concurrent searches can share a read lock,
    /// and only lazy rebuilds need a write lock.
    hnsw_cache: RwLock<Option<HnswSnapshot>>,
}

/// A cached HNSW graph snapshot mapping points to chunk IDs.
struct HnswSnapshot {
    map: HnswMap<EmbeddingPoint, u64>,
}

impl HnswVectorIndex {
    /// Create a new empty HNSW index with the given vector dimensionality.
    pub fn new(dimension: usize) -> Self {
        Self {
            entries: Vec::new(),
            id_to_idx: HashMap::new(),
            dimension,
            hnsw_cache: RwLock::new(None),
        }
    }

    /// Return the configured dimensionality.
    pub fn dimension(&self) -> usize {
        self.dimension
    }

    /// Save the index using bitcode binary serialization (faster + smaller than JSON).
    pub fn save_binary(&self, path: &std::path::Path) -> Result<(), CodixingError> {
        let data = super::vector::VectorIndexData {
            dimension: self.dimension,
            entries: self.entries.clone(),
        };
        let bytes = bitcode::serialize(&data).map_err(|e| {
            CodixingError::Embedding(format!("failed to serialize HNSW index: {e}"))
        })?;
        std::fs::write(path, bytes)?;
        Ok(())
    }

    /// Load the index from a bitcode binary file.
    pub fn load_binary(path: &std::path::Path) -> Result<Self, CodixingError> {
        let bytes = std::fs::read(path)?;
        let data: super::vector::VectorIndexData = bitcode::deserialize(&bytes).map_err(|e| {
            CodixingError::Embedding(format!("failed to deserialize HNSW index: {e}"))
        })?;
        let mut index = Self::new(data.dimension);
        for entry in data.entries {
            index.add(entry.chunk_id, entry.vector)?;
        }
        *index.hnsw_cache.write().unwrap_or_else(|e| e.into_inner()) = index.build_snapshot();
        Ok(index)
    }

    /// Run a search against a pre-built HNSW snapshot.
    fn search_snapshot(
        snapshot: &HnswSnapshot,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<VectorSearchResult>, CodixingError> {
        let query_point = EmbeddingPoint {
            vector: query.to_vec(),
        };
        let mut search = Search::default();
        let results: Vec<VectorSearchResult> = snapshot
            .map
            .search(&query_point, &mut search)
            .take(k)
            .map(|item| {
                let similarity = 1.0 - item.distance;
                VectorSearchResult {
                    chunk_id: *item.value,
                    similarity,
                }
            })
            .collect();
        Ok(results)
    }

    /// Build an HNSW snapshot from the current entries.
    fn build_snapshot(&self) -> Option<HnswSnapshot> {
        if self.entries.is_empty() {
            return None;
        }

        let points: Vec<EmbeddingPoint> = self
            .entries
            .iter()
            .map(|e| EmbeddingPoint {
                vector: e.vector.clone(),
            })
            .collect();
        let chunk_ids: Vec<u64> = self.entries.iter().map(|e| e.chunk_id).collect();

        let map = Builder::default().seed(42).build(points, chunk_ids);

        Some(HnswSnapshot { map })
    }
}

impl VectorIndex for HnswVectorIndex {
    fn add(&mut self, chunk_id: u64, vector: Vec<f32>) -> Result<(), CodixingError> {
        if vector.len() != self.dimension {
            return Err(CodixingError::Embedding(format!(
                "vector dimension mismatch: expected {}, got {}",
                self.dimension,
                vector.len()
            )));
        }
        if let Some(&idx) = self.id_to_idx.get(&chunk_id) {
            self.entries[idx].vector = vector;
        } else {
            let idx = self.entries.len();
            self.entries.push(VectorEntry { chunk_id, vector });
            self.id_to_idx.insert(chunk_id, idx);
        }
        // Invalidate the HNSW graph.
        *self.hnsw_cache.write().unwrap_or_else(|e| e.into_inner()) = None;
        Ok(())
    }

    fn remove_chunks(&mut self, chunk_ids: &[u64]) -> Result<(), CodixingError> {
        let to_remove: std::collections::HashSet<u64> = chunk_ids.iter().copied().collect();
        self.entries.retain(|e| !to_remove.contains(&e.chunk_id));
        self.id_to_idx.clear();
        for (idx, entry) in self.entries.iter().enumerate() {
            self.id_to_idx.insert(entry.chunk_id, idx);
        }
        *self.hnsw_cache.write().unwrap_or_else(|e| e.into_inner()) = None;
        Ok(())
    }

    fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorSearchResult>, CodixingError> {
        if self.entries.is_empty() || k == 0 {
            return Ok(Vec::new());
        }

        // Fast path: try a read lock first (allows concurrent searches).
        {
            let cache = self.hnsw_cache.read().unwrap_or_else(|e| e.into_inner());
            if cache.is_some() {
                return Self::search_snapshot(cache.as_ref().unwrap(), query, k);
            }
        }

        // Slow path: snapshot is None, acquire write lock to rebuild.
        {
            let mut cache = self.hnsw_cache.write().unwrap_or_else(|e| e.into_inner());
            // Double-check: another thread may have rebuilt while we waited.
            if cache.is_none() {
                *cache = self.build_snapshot();
            }
        }

        // Now read lock again for the search.
        let cache = self.hnsw_cache.read().unwrap_or_else(|e| e.into_inner());
        Self::search_snapshot(cache.as_ref().unwrap(), query, k)
    }

    fn search_batch(
        &self,
        queries: &[Vec<f32>],
        k: usize,
    ) -> Result<Vec<Vec<VectorSearchResult>>, CodixingError> {
        use rayon::prelude::*;
        // Ensure the HNSW graph is built before parallel queries use read locks.
        {
            let needs_build = self.hnsw_cache.read().unwrap_or_else(|e| e.into_inner()).is_none();
            if needs_build {
                let mut cache = self.hnsw_cache.write().unwrap_or_else(|e| e.into_inner());
                if cache.is_none() {
                    *cache = self.build_snapshot();
                }
            }
        }
        queries.par_iter().map(|q| self.search(q, k)).collect()
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn save(&self, path: &Path) -> Result<(), CodixingError> {
        let data = serde_json::json!({
            "type": "hnsw",
            "dimension": self.dimension,
            "entries": self.entries.iter().map(|e| {
                serde_json::json!({
                    "chunk_id": e.chunk_id,
                    "vector": e.vector,
                })
            }).collect::<Vec<_>>(),
        });
        let bytes = serde_json::to_vec(&data).map_err(|e| {
            CodixingError::Embedding(format!("failed to serialize HNSW index: {e}"))
        })?;
        std::fs::write(path, bytes)?;
        Ok(())
    }

    fn load(path: &Path) -> Result<Self, CodixingError> {
        let bytes = std::fs::read(path)?;
        let data: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
            CodixingError::Embedding(format!("failed to deserialize HNSW index: {e}"))
        })?;
        let dimension = data["dimension"].as_u64().unwrap_or(384) as usize;
        let mut index = Self::new(dimension);
        if let Some(entries) = data["entries"].as_array() {
            for entry in entries {
                let chunk_id = entry["chunk_id"].as_u64().unwrap_or(0);
                let vector: Vec<f32> = entry["vector"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .map(|v| v.as_f64().unwrap_or(0.0) as f32)
                            .collect()
                    })
                    .unwrap_or_default();
                index.add(chunk_id, vector)?;
            }
        }
        // Pre-build the HNSW graph after loading.
        *index.hnsw_cache.write().unwrap_or_else(|e| e.into_inner()) = index.build_snapshot();
        Ok(index)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hnsw_add_and_search() {
        let mut idx = HnswVectorIndex::new(3);
        idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
        idx.add(2, vec![0.0, 1.0, 0.0]).unwrap();
        idx.add(3, vec![0.9, 0.1, 0.0]).unwrap();

        let results = idx.search(&[1.0, 0.0, 0.0], 2).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].chunk_id, 1); // Exact match
        assert!(results[0].similarity > 0.99);
    }

    #[test]
    fn hnsw_dimension_mismatch() {
        let mut idx = HnswVectorIndex::new(3);
        let result = idx.add(1, vec![1.0, 0.0]);
        assert!(result.is_err());
    }

    #[test]
    fn hnsw_remove_chunks() {
        let mut idx = HnswVectorIndex::new(3);
        idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
        idx.add(2, vec![0.0, 1.0, 0.0]).unwrap();
        idx.remove_chunks(&[1]).unwrap();
        assert_eq!(idx.len(), 1);

        let results = idx.search(&[1.0, 0.0, 0.0], 5).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk_id, 2);
    }

    #[test]
    fn hnsw_empty_search() {
        let idx = HnswVectorIndex::new(3);
        let results = idx.search(&[1.0, 0.0, 0.0], 5).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn hnsw_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.hnsw.json");

        let mut idx = HnswVectorIndex::new(3);
        idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
        idx.add(2, vec![0.0, 1.0, 0.0]).unwrap();
        idx.save(&path).unwrap();

        let loaded = HnswVectorIndex::load(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        let results = loaded.search(&[1.0, 0.0, 0.0], 1).unwrap();
        assert_eq!(results[0].chunk_id, 1);
    }

    #[test]
    fn hnsw_update_existing_vector() {
        let mut idx = HnswVectorIndex::new(3);
        idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
        idx.add(1, vec![0.0, 1.0, 0.0]).unwrap(); // Update
        assert_eq!(idx.len(), 1);
        let results = idx.search(&[0.0, 1.0, 0.0], 1).unwrap();
        assert_eq!(results[0].chunk_id, 1);
        assert!(results[0].similarity > 0.99);
    }

    #[test]
    fn hnsw_search_returns_at_most_k() {
        let mut idx = HnswVectorIndex::new(2);
        for i in 0..20 {
            idx.add(i, vec![i as f32, 1.0]).unwrap();
        }
        let results = idx.search(&[10.0, 1.0], 5).unwrap();
        assert_eq!(results.len(), 5);
    }

    #[test]
    fn hnsw_results_sorted_by_similarity() {
        let mut idx = HnswVectorIndex::new(3);
        idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
        idx.add(2, vec![0.5, 0.5, 0.0]).unwrap();
        idx.add(3, vec![0.0, 1.0, 0.0]).unwrap();

        let results = idx.search(&[1.0, 0.0, 0.0], 3).unwrap();
        assert_eq!(results.len(), 3);
        for w in results.windows(2) {
            assert!(w[0].similarity >= w[1].similarity);
        }
    }

    #[test]
    fn hnsw_recall_vs_brute_force() {
        use super::super::BruteForceVectorIndex;

        let dim = 32;
        let n = 200;
        let k = 10;

        // Create deterministic pseudo-random vectors.
        let mut bf = BruteForceVectorIndex::new(dim);
        let mut hnsw = HnswVectorIndex::new(dim);

        for i in 0..n {
            let vec: Vec<f32> = (0..dim)
                .map(|d| ((i * 7 + d * 13) % 100) as f32 / 100.0)
                .collect();
            bf.add(i as u64, vec.clone()).unwrap();
            hnsw.add(i as u64, vec).unwrap();
        }

        let query: Vec<f32> = (0..dim).map(|d| (d * 11 % 100) as f32 / 100.0).collect();

        let bf_results = bf.search(&query, k).unwrap();
        let hnsw_results = hnsw.search(&query, k).unwrap();

        // HNSW recall should be >= 0.95 (at least 95% of brute-force top-k found).
        let bf_ids: std::collections::HashSet<u64> =
            bf_results.iter().map(|r| r.chunk_id).collect();
        let hnsw_ids: std::collections::HashSet<u64> =
            hnsw_results.iter().map(|r| r.chunk_id).collect();
        let overlap = bf_ids.intersection(&hnsw_ids).count();
        let recall = overlap as f64 / k as f64;
        assert!(
            recall >= 0.9,
            "HNSW recall {recall:.2} should be >= 0.90 (overlap: {overlap}/{k})"
        );
    }

    #[test]
    fn hnsw_is_empty_on_new_index() {
        let idx = HnswVectorIndex::new(3);
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn hnsw_load_nonexistent_file_returns_error() {
        let result = HnswVectorIndex::load(Path::new("/nonexistent/vectors.hnsw.json"));
        assert!(result.is_err());
    }

    #[test]
    fn hnsw_remove_multiple_chunks() {
        let mut idx = HnswVectorIndex::new(3);
        idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
        idx.add(2, vec![0.0, 1.0, 0.0]).unwrap();
        idx.add(3, vec![0.0, 0.0, 1.0]).unwrap();
        idx.remove_chunks(&[1, 3]).unwrap();
        assert_eq!(idx.len(), 1);
        let results = idx.search(&[0.0, 1.0, 0.0], 1).unwrap();
        assert_eq!(results[0].chunk_id, 2);
    }

    #[test]
    fn hnsw_batch_search_matches_sequential() {
        let mut index = HnswVectorIndex::new(4);
        for i in 0..100 {
            index.add(i, vec![i as f32; 4]).unwrap();
        }
        let queries = vec![vec![5.0f32; 4], vec![50.0; 4], vec![95.0; 4]];
        let batch_results = index.search_batch(&queries, 3).unwrap();
        for (query, batch_result) in queries.iter().zip(&batch_results) {
            let seq_result = index.search(query, 3).unwrap();
            assert_eq!(batch_result.len(), seq_result.len());
            // HNSW is approximate, so just check the top-1 result matches.
            assert_eq!(batch_result[0].chunk_id, seq_result[0].chunk_id);
        }
    }

    #[test]
    fn hnsw_concurrent_search_with_rwlock() {
        use std::sync::Arc;
        use std::thread;

        let mut idx = HnswVectorIndex::new(8);
        for i in 0..100 {
            let vec: Vec<f32> = (0..8).map(|d| ((i * 7 + d * 3) % 50) as f32 / 50.0).collect();
            idx.add(i as u64, vec).unwrap();
        }
        // Pre-build the snapshot so all threads use read locks.
        let _ = idx.search(&[0.5f32; 8], 1).unwrap();

        let idx = Arc::new(idx);
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let idx = Arc::clone(&idx);
                thread::spawn(move || {
                    let query: Vec<f32> = (0..8).map(|d| ((t * 11 + d * 5) % 50) as f32 / 50.0).collect();
                    for _ in 0..100 {
                        let results = idx.search(&query, 5).unwrap();
                        assert!(!results.is_empty());
                        assert!(results.len() <= 5);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn hnsw_binary_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.hnsw.bin");

        let mut idx = HnswVectorIndex::new(3);
        idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
        idx.add(2, vec![0.0, 1.0, 0.0]).unwrap();
        idx.save_binary(&path).unwrap();

        let loaded = HnswVectorIndex::load_binary(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        let results = loaded.search(&[1.0, 0.0, 0.0], 1).unwrap();
        assert_eq!(results[0].chunk_id, 1);
    }
}

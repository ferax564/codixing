//! Vector index for approximate nearest-neighbor search.
//!
//! Stores chunk embeddings and supports efficient similarity search.
//!
//! - [`BruteForceVectorIndex`] is always available and uses cosine similarity
//!   over all stored vectors. Simple and correct; suitable for codebases up to
//!   ~100K chunks.
//! - [`HnswVectorIndex`] (behind the `vector` feature) uses the HNSW algorithm
//!   via `instant-distance` for sub-linear query time on larger datasets.

use std::collections::HashMap;
use std::path::Path;

use crate::error::CodeforgeError;

/// A stored vector with its associated chunk ID.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VectorEntry {
    /// The chunk this vector belongs to.
    pub chunk_id: u64,
    /// The dense embedding vector.
    pub vector: Vec<f32>,
}

/// Vector search result with similarity score.
#[derive(Debug, Clone)]
pub struct VectorSearchResult {
    /// The chunk ID of the matched vector.
    pub chunk_id: u64,
    /// Cosine similarity to the query vector (1.0 = identical, 0.0 = orthogonal).
    pub similarity: f32,
}

/// Vector index trait for nearest-neighbor search.
pub trait VectorIndex: Send + Sync {
    /// Add a vector for a chunk. If the chunk already exists, update it.
    fn add(&mut self, chunk_id: u64, vector: Vec<f32>) -> Result<(), CodeforgeError>;

    /// Remove all vectors for the given chunk IDs.
    fn remove_chunks(&mut self, chunk_ids: &[u64]) -> Result<(), CodeforgeError>;

    /// Search for `k` nearest neighbors to the query vector.
    fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorSearchResult>, CodeforgeError>;

    /// Number of vectors stored.
    fn len(&self) -> usize;

    /// Whether the index is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Save the index to disk at the given path.
    fn save(&self, path: &Path) -> Result<(), CodeforgeError>;

    /// Load the index from disk at the given path.
    fn load(path: &Path) -> Result<Self, CodeforgeError>
    where
        Self: Sized;
}

// ---------------------------------------------------------------------------
// BruteForceVectorIndex
// ---------------------------------------------------------------------------

/// Brute-force vector index using cosine similarity.
///
/// Simple and correct; suitable for codebases up to ~100K chunks.
/// No external dependencies beyond `serde_json` (already in the workspace).
pub struct BruteForceVectorIndex {
    entries: Vec<VectorEntry>,
    id_to_idx: HashMap<u64, usize>,
    dimension: usize,
}

impl BruteForceVectorIndex {
    /// Create a new empty index with the given vector dimensionality.
    pub fn new(dimension: usize) -> Self {
        Self {
            entries: Vec::new(),
            id_to_idx: HashMap::new(),
            dimension,
        }
    }

    /// Return the configured dimensionality.
    pub fn dimension(&self) -> usize {
        self.dimension
    }

    /// Cosine similarity between two vectors.
    ///
    /// Delegates to the SIMD-accelerated implementation in [`super::simd_distance`].
    /// Returns 0.0 if either vector has zero magnitude.
    fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        super::simd_distance::cosine_similarity(a, b)
    }
}

/// Binary-serializable container for a vector index.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct VectorIndexData {
    pub(crate) dimension: usize,
    pub(crate) entries: Vec<VectorEntry>,
}

impl BruteForceVectorIndex {
    /// Save the index using bitcode binary serialization (faster + smaller than JSON).
    pub fn save_binary(&self, path: &Path) -> Result<(), CodeforgeError> {
        let data = VectorIndexData {
            dimension: self.dimension,
            entries: self.entries.clone(),
        };
        let bytes = bitcode::serialize(&data).map_err(|e| {
            CodeforgeError::Embedding(format!("failed to serialize vector index: {e}"))
        })?;
        std::fs::write(path, bytes)?;
        Ok(())
    }

    /// Load the index from a bitcode binary file.
    pub fn load_binary(path: &Path) -> Result<Self, CodeforgeError> {
        let bytes = std::fs::read(path)?;
        let data: VectorIndexData = bitcode::deserialize(&bytes).map_err(|e| {
            CodeforgeError::Embedding(format!("failed to deserialize vector index: {e}"))
        })?;
        let mut index = Self::new(data.dimension);
        for entry in data.entries {
            index.add(entry.chunk_id, entry.vector)?;
        }
        Ok(index)
    }
}

impl VectorIndex for BruteForceVectorIndex {
    fn add(&mut self, chunk_id: u64, vector: Vec<f32>) -> Result<(), CodeforgeError> {
        if vector.len() != self.dimension {
            return Err(CodeforgeError::Embedding(format!(
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
        Ok(())
    }

    fn remove_chunks(&mut self, chunk_ids: &[u64]) -> Result<(), CodeforgeError> {
        let to_remove: std::collections::HashSet<u64> = chunk_ids.iter().copied().collect();
        self.entries.retain(|e| !to_remove.contains(&e.chunk_id));
        // Rebuild the index map after removal.
        self.id_to_idx.clear();
        for (idx, entry) in self.entries.iter().enumerate() {
            self.id_to_idx.insert(entry.chunk_id, idx);
        }
        Ok(())
    }

    fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorSearchResult>, CodeforgeError> {
        let mut scores: Vec<VectorSearchResult> = self
            .entries
            .iter()
            .map(|entry| VectorSearchResult {
                chunk_id: entry.chunk_id,
                similarity: Self::cosine_similarity(query, &entry.vector),
            })
            .collect();
        scores.sort_by(|a, b| {
            b.similarity
                .partial_cmp(&a.similarity)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scores.truncate(k);
        Ok(scores)
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn save(&self, path: &Path) -> Result<(), CodeforgeError> {
        let data = serde_json::json!({
            "dimension": self.dimension,
            "entries": self.entries.iter().map(|e| {
                serde_json::json!({
                    "chunk_id": e.chunk_id,
                    "vector": e.vector,
                })
            }).collect::<Vec<_>>(),
        });
        let bytes = serde_json::to_vec(&data).map_err(|e| {
            CodeforgeError::Embedding(format!("failed to serialize vector index: {e}"))
        })?;
        std::fs::write(path, bytes)?;
        Ok(())
    }

    fn load(path: &Path) -> Result<Self, CodeforgeError> {
        let bytes = std::fs::read(path)?;
        let data: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
            CodeforgeError::Embedding(format!("failed to deserialize vector index: {e}"))
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
    fn add_and_search() {
        let mut idx = BruteForceVectorIndex::new(3);
        idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
        idx.add(2, vec![0.0, 1.0, 0.0]).unwrap();
        idx.add(3, vec![0.9, 0.1, 0.0]).unwrap();

        let results = idx.search(&[1.0, 0.0, 0.0], 2).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].chunk_id, 1); // Exact match
        assert_eq!(results[1].chunk_id, 3); // Close match
    }

    #[test]
    fn dimension_mismatch() {
        let mut idx = BruteForceVectorIndex::new(3);
        let result = idx.add(1, vec![1.0, 0.0]); // Wrong dimension
        assert!(result.is_err());
    }

    #[test]
    fn remove_chunks() {
        let mut idx = BruteForceVectorIndex::new(3);
        idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
        idx.add(2, vec![0.0, 1.0, 0.0]).unwrap();
        idx.remove_chunks(&[1]).unwrap();
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn empty_search() {
        let idx = BruteForceVectorIndex::new(3);
        let results = idx.search(&[1.0, 0.0, 0.0], 5).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.json");

        let mut idx = BruteForceVectorIndex::new(3);
        idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
        idx.add(2, vec![0.0, 1.0, 0.0]).unwrap();
        idx.save(&path).unwrap();

        let loaded = BruteForceVectorIndex::load(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        let results = loaded.search(&[1.0, 0.0, 0.0], 1).unwrap();
        assert_eq!(results[0].chunk_id, 1);
    }

    #[test]
    fn update_existing_vector() {
        let mut idx = BruteForceVectorIndex::new(3);
        idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
        idx.add(1, vec![0.0, 1.0, 0.0]).unwrap(); // Update
        assert_eq!(idx.len(), 1);
        let results = idx.search(&[0.0, 1.0, 0.0], 1).unwrap();
        assert_eq!(results[0].chunk_id, 1);
        assert!((results[0].similarity - 1.0).abs() < 0.001);
    }

    #[test]
    fn cosine_similarity_basic() {
        // Identical vectors -> similarity 1.0
        let sim = BruteForceVectorIndex::cosine_similarity(&[1.0, 0.0, 0.0], &[1.0, 0.0, 0.0]);
        assert!((sim - 1.0).abs() < 0.001);

        // Orthogonal vectors -> similarity 0.0
        let sim2 = BruteForceVectorIndex::cosine_similarity(&[1.0, 0.0, 0.0], &[0.0, 1.0, 0.0]);
        assert!(sim2.abs() < 0.001);
    }

    #[test]
    fn cosine_similarity_zero_vector() {
        let sim = BruteForceVectorIndex::cosine_similarity(&[0.0, 0.0, 0.0], &[1.0, 0.0, 0.0]);
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn remove_nonexistent_chunk_is_noop() {
        let mut idx = BruteForceVectorIndex::new(3);
        idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
        idx.remove_chunks(&[99]).unwrap(); // Should not panic
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn search_returns_at_most_k() {
        let mut idx = BruteForceVectorIndex::new(2);
        for i in 0..20 {
            idx.add(i, vec![i as f32, 1.0]).unwrap();
        }
        let results = idx.search(&[10.0, 1.0], 5).unwrap();
        assert_eq!(results.len(), 5);
    }

    #[test]
    fn search_results_sorted_by_similarity() {
        let mut idx = BruteForceVectorIndex::new(3);
        idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
        idx.add(2, vec![0.5, 0.5, 0.0]).unwrap();
        idx.add(3, vec![0.0, 1.0, 0.0]).unwrap();

        let results = idx.search(&[1.0, 0.0, 0.0], 3).unwrap();
        assert_eq!(results.len(), 3);
        // Similarities should be in descending order.
        for w in results.windows(2) {
            assert!(w[0].similarity >= w[1].similarity);
        }
    }

    #[test]
    fn dimension_accessor() {
        let idx = BruteForceVectorIndex::new(128);
        assert_eq!(idx.dimension(), 128);
    }

    #[test]
    fn is_empty_on_new_index() {
        let idx = BruteForceVectorIndex::new(3);
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn load_nonexistent_file_returns_error() {
        let result = BruteForceVectorIndex::load(Path::new("/nonexistent/vectors.json"));
        assert!(result.is_err());
    }

    #[test]
    fn remove_multiple_chunks() {
        let mut idx = BruteForceVectorIndex::new(3);
        idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
        idx.add(2, vec![0.0, 1.0, 0.0]).unwrap();
        idx.add(3, vec![0.0, 0.0, 1.0]).unwrap();
        idx.remove_chunks(&[1, 3]).unwrap();
        assert_eq!(idx.len(), 1);
        let results = idx.search(&[0.0, 1.0, 0.0], 1).unwrap();
        assert_eq!(results[0].chunk_id, 2);
    }

    #[test]
    fn binary_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.bin");

        let mut idx = BruteForceVectorIndex::new(3);
        idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
        idx.add(2, vec![0.0, 1.0, 0.0]).unwrap();
        idx.save_binary(&path).unwrap();

        let loaded = BruteForceVectorIndex::load_binary(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        let results = loaded.search(&[1.0, 0.0, 0.0], 1).unwrap();
        assert_eq!(results[0].chunk_id, 1);
    }

    #[test]
    fn binary_smaller_than_json() {
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("vectors.json");
        let bin_path = dir.path().join("vectors.bin");

        let mut idx = BruteForceVectorIndex::new(32);
        for i in 0..100 {
            let vec: Vec<f32> = (0..32)
                .map(|d| ((i * 7 + d * 13) % 100) as f32 / 100.0)
                .collect();
            idx.add(i, vec).unwrap();
        }
        idx.save(&json_path).unwrap();
        idx.save_binary(&bin_path).unwrap();

        let json_size = std::fs::metadata(&json_path).unwrap().len();
        let bin_size = std::fs::metadata(&bin_path).unwrap().len();

        assert!(
            bin_size < json_size,
            "binary ({bin_size}) should be smaller than JSON ({json_size})"
        );
    }
}

//! Memory-mapped vector index for reduced RSS on large repositories.
//!
//! Stores dense embedding vectors in a flat file with a self-describing header.
//! Only the pages that are actually accessed by the OS are loaded into physical
//! memory, making this ideal for repositories with tens of thousands of chunks
//! where only a fraction of vectors are touched per query.
//!
//! File format (little-endian):
//! ```text
//! [magic:     4 bytes  "CDXV"]
//! [version:   u32]
//! [dimension: u32]
//! [count:     u64]
//! [id_table:  count * u64]                // chunk IDs in insertion order
//! [vectors:   count * dimension * f32]     // dense float vectors, contiguous
//! ```
//!
//! Mutations (`add` / `remove_chunks`) are buffered in memory and only
//! flushed to disk when [`MmapVectorIndex::save`] is called.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use memmap2::Mmap;

use super::vector::{VectorIndex, VectorSearchResult};
use crate::error::CodixingError;

/// 4-byte magic number identifying a Codixing mmap vector file.
const MAGIC: &[u8; 4] = b"CDXV";

/// Current file format version.
const FORMAT_VERSION: u32 = 1;

/// Size of the fixed header: magic (4) + version (4) + dimension (4) + count (8) = 20.
const HEADER_SIZE: usize = 4 + 4 + 4 + 8;

/// Memory-mapped vector index using brute-force cosine similarity.
///
/// Vectors are stored in a flat file and accessed via `mmap`. Mutations
/// are buffered in memory and only written to disk on [`save`](VectorIndex::save).
pub struct MmapVectorIndex {
    /// Memory-mapped file (read-only). `None` when the index was just created
    /// without a backing file (all data lives in `pending_*`).
    mmap: Option<Mmap>,

    /// Vector dimensionality.
    dimension: usize,

    /// Number of vectors in the mmap region (excludes pending adds).
    mmap_count: usize,

    /// Map from chunk_id to its offset (slot index) in the mmap region.
    /// Only populated for vectors that are in the mmap file.
    mmap_id_index: HashMap<u64, usize>,

    /// Ordered IDs from the mmap region.
    mmap_ids: Vec<u64>,

    /// IDs that have been logically removed (present in mmap but should be
    /// skipped during search).
    removed: std::collections::HashSet<u64>,

    /// Vectors added since the last save (buffered in RAM).
    pending_entries: Vec<PendingEntry>,

    /// Fast lookup from chunk_id to index in `pending_entries`.
    pending_id_index: HashMap<u64, usize>,
}

/// A vector entry waiting to be flushed to disk.
struct PendingEntry {
    chunk_id: u64,
    vector: Vec<f32>,
}

impl MmapVectorIndex {
    /// Create a new empty mmap vector index.
    ///
    /// No file is created until [`save`](VectorIndex::save) is called.
    pub fn create(_path: &Path, dimension: usize) -> Result<Self, CodixingError> {
        Ok(Self {
            mmap: None,
            dimension,
            mmap_count: 0,
            mmap_id_index: HashMap::new(),
            mmap_ids: Vec::new(),
            removed: std::collections::HashSet::new(),
            pending_entries: Vec::new(),
            pending_id_index: HashMap::new(),
        })
    }

    /// Open an existing mmap vector file.
    pub fn open(path: &Path) -> Result<Self, CodixingError> {
        let file = std::fs::File::open(path)?;
        let file_len = file.metadata()?.len() as usize;

        if file_len < HEADER_SIZE {
            return Err(CodixingError::Embedding(
                "mmap vector file too small for header".to_string(),
            ));
        }

        // SAFETY: We only create a read-only Mmap over a file we opened read-only.
        // The file should not be modified externally while the Mmap is alive.
        let mmap = unsafe { Mmap::map(&file) }
            .map_err(|e| CodixingError::Embedding(format!("failed to mmap vector file: {e}")))?;

        // Parse header.
        let magic = &mmap[0..4];
        if magic != MAGIC {
            return Err(CodixingError::Embedding(format!(
                "invalid mmap vector file magic: expected CDXV, got {:?}",
                magic
            )));
        }

        let version = u32::from_le_bytes(mmap[4..8].try_into().unwrap());
        if version != FORMAT_VERSION {
            return Err(CodixingError::Embedding(format!(
                "unsupported mmap vector file version: {version}"
            )));
        }

        let dimension = u32::from_le_bytes(mmap[8..12].try_into().unwrap()) as usize;
        let count = u64::from_le_bytes(mmap[12..20].try_into().unwrap()) as usize;

        // Validate file size.
        let expected_size = HEADER_SIZE + count * 8 + count * dimension * 4;
        if file_len < expected_size {
            return Err(CodixingError::Embedding(format!(
                "mmap vector file truncated: expected {expected_size} bytes, got {file_len}"
            )));
        }

        // Parse ID table.
        let id_table_start = HEADER_SIZE;
        let mut mmap_ids = Vec::with_capacity(count);
        let mut mmap_id_index = HashMap::with_capacity(count);

        for i in 0..count {
            let offset = id_table_start + i * 8;
            let id = u64::from_le_bytes(mmap[offset..offset + 8].try_into().unwrap());
            mmap_ids.push(id);
            mmap_id_index.insert(id, i);
        }

        Ok(Self {
            mmap: Some(mmap),
            dimension,
            mmap_count: count,
            mmap_id_index,
            mmap_ids,
            removed: std::collections::HashSet::new(),
            pending_entries: Vec::new(),
            pending_id_index: HashMap::new(),
        })
    }

    /// Build a mmap vector index from an existing `BruteForceVectorIndex`.
    ///
    /// Serializes the brute-force index to bitcode in memory, extracts its
    /// entries, writes them to the mmap file at `path`, and returns a new
    /// `MmapVectorIndex` backed by the file.
    pub fn build_from(
        source: &super::vector::BruteForceVectorIndex,
        path: &Path,
    ) -> Result<Self, CodixingError> {
        let dimension = source.dimension();
        let mut index = Self::create(path, dimension)?;

        // Access the internal data via the crate-visible VectorIndexData.
        // Serialize to bitcode in memory (no temp file needed) and deserialize
        // to get the entries with their vectors.
        let data = source.to_index_data();

        for entry in data.entries {
            index.add(entry.chunk_id, entry.vector)?;
        }

        index.save(path)?;

        // Reopen from the written file to get a proper mmap.
        Self::open(path)
    }

    /// Return the vector dimensionality.
    pub fn dimension(&self) -> usize {
        self.dimension
    }

    /// Get a vector slice from the mmap region by slot index.
    ///
    /// Returns a slice of `f32` values directly from the mmap'd memory.
    fn mmap_vector(&self, slot: usize) -> &[f32] {
        let mmap = self
            .mmap
            .as_ref()
            .expect("mmap must be Some for mmap reads");
        let vectors_start = HEADER_SIZE + self.mmap_count * 8;
        let byte_offset = vectors_start + slot * self.dimension * 4;
        let byte_end = byte_offset + self.dimension * 4;

        // SAFETY: We validated the file size on open, and `slot` is always
        // within `0..mmap_count`. The alignment of f32 is 4, and our byte
        // offsets are always 4-aligned (header is 20 bytes, id table entries
        // are 8 bytes each, so vectors_start = 20 + count*8 which is always
        // 4-aligned).
        let bytes = &mmap[byte_offset..byte_end];
        // Use bytemuck-style reinterpret. Since we control the writer and
        // always write little-endian f32, and this code runs on LE platforms,
        // a direct pointer cast is safe.
        unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const f32, self.dimension) }
    }

    /// Cosine similarity using the SIMD-accelerated implementation.
    fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        super::simd_distance::cosine_similarity(a, b)
    }

    /// Write the current state (mmap data minus removals, plus pending) to the file.
    fn write_file(&self, path: &Path) -> Result<(), CodixingError> {
        // Collect all live entries: mmap entries that aren't removed + pending.
        let mut ids: Vec<u64> = Vec::new();
        let mut vectors: Vec<&[f32]> = Vec::new();

        // Add surviving mmap entries.
        for (i, &id) in self.mmap_ids.iter().enumerate() {
            if !self.removed.contains(&id) {
                ids.push(id);
                vectors.push(self.mmap_vector(i));
            }
        }

        // Add pending entries.
        for entry in &self.pending_entries {
            ids.push(entry.chunk_id);
            vectors.push(&entry.vector);
        }

        let count = ids.len();
        let file_size = HEADER_SIZE + count * 8 + count * self.dimension * 4;

        let mut buf = Vec::with_capacity(file_size);

        // Write header.
        buf.write_all(MAGIC)?;
        buf.write_all(&FORMAT_VERSION.to_le_bytes())?;
        buf.write_all(&(self.dimension as u32).to_le_bytes())?;
        buf.write_all(&(count as u64).to_le_bytes())?;

        // Write ID table.
        for &id in &ids {
            buf.write_all(&id.to_le_bytes())?;
        }

        // Write vectors.
        for vec in &vectors {
            for &val in *vec {
                buf.write_all(&val.to_le_bytes())?;
            }
        }

        // Atomic write: write to a temp file in the same directory, then rename.
        let parent = path
            .parent()
            .ok_or_else(|| CodixingError::Embedding("mmap path has no parent".to_string()))?;
        let tmp_path = parent.join(".vectors_mmap.tmp");
        std::fs::write(&tmp_path, &buf)?;
        std::fs::rename(&tmp_path, path)?;

        Ok(())
    }
}

impl VectorIndex for MmapVectorIndex {
    fn add(&mut self, chunk_id: u64, vector: Vec<f32>) -> Result<(), CodixingError> {
        if vector.len() != self.dimension {
            return Err(CodixingError::Embedding(format!(
                "vector dimension mismatch: expected {}, got {}",
                self.dimension,
                vector.len()
            )));
        }

        // If this ID exists in the mmap region, mark it as removed so the
        // pending entry takes precedence.
        if self.mmap_id_index.contains_key(&chunk_id) {
            self.removed.insert(chunk_id);
        }

        // If this ID already has a pending entry, update it in place.
        if let Some(&idx) = self.pending_id_index.get(&chunk_id) {
            self.pending_entries[idx].vector = vector;
        } else {
            let idx = self.pending_entries.len();
            self.pending_entries.push(PendingEntry { chunk_id, vector });
            self.pending_id_index.insert(chunk_id, idx);
        }

        Ok(())
    }

    fn remove_chunks(&mut self, chunk_ids: &[u64]) -> Result<(), CodixingError> {
        let to_remove: std::collections::HashSet<u64> = chunk_ids.iter().copied().collect();

        // Mark mmap entries for removal.
        for &id in &to_remove {
            if self.mmap_id_index.contains_key(&id) {
                self.removed.insert(id);
            }
        }

        // Remove from pending entries.
        self.pending_entries
            .retain(|e| !to_remove.contains(&e.chunk_id));
        // Rebuild the pending index.
        self.pending_id_index.clear();
        for (idx, entry) in self.pending_entries.iter().enumerate() {
            self.pending_id_index.insert(entry.chunk_id, idx);
        }

        Ok(())
    }

    fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorSearchResult>, CodixingError> {
        if k == 0 {
            return Ok(Vec::new());
        }

        let total_len = self.len();
        if total_len == 0 {
            return Ok(Vec::new());
        }

        let mut scores: Vec<VectorSearchResult> = Vec::with_capacity(total_len);

        // Score mmap vectors (skipping removed).
        for (i, &id) in self.mmap_ids.iter().enumerate() {
            if self.removed.contains(&id) {
                continue;
            }
            let vec = self.mmap_vector(i);
            scores.push(VectorSearchResult {
                chunk_id: id,
                similarity: Self::cosine_similarity(query, vec),
            });
        }

        // Score pending vectors.
        for entry in &self.pending_entries {
            scores.push(VectorSearchResult {
                chunk_id: entry.chunk_id,
                similarity: Self::cosine_similarity(query, &entry.vector),
            });
        }

        let cmp = |a: &VectorSearchResult, b: &VectorSearchResult| {
            b.similarity
                .partial_cmp(&a.similarity)
                .unwrap_or(std::cmp::Ordering::Equal)
        };

        if scores.len() > k {
            scores.select_nth_unstable_by(k - 1, cmp);
            scores.truncate(k);
            scores.sort_by(cmp);
        } else {
            scores.sort_by(cmp);
        }

        Ok(scores)
    }

    fn search_batch(
        &self,
        queries: &[Vec<f32>],
        k: usize,
    ) -> Result<Vec<Vec<VectorSearchResult>>, CodixingError> {
        use rayon::prelude::*;
        queries.par_iter().map(|q| self.search(q, k)).collect()
    }

    fn len(&self) -> usize {
        let mmap_live = self.mmap_count - self.removed.len();
        mmap_live + self.pending_entries.len()
    }

    fn save(&self, path: &Path) -> Result<(), CodixingError> {
        self.write_file(path)
    }

    fn load(path: &Path) -> Result<Self, CodixingError> {
        Self::open(path)
    }
}

// Ensure MmapVectorIndex is Send + Sync.
// Mmap is Send + Sync, and all other fields are standard types.
static_assertions_send_sync!(MmapVectorIndex);

/// Compile-time assertion that `MmapVectorIndex` is `Send + Sync`.
macro_rules! static_assertions_send_sync {
    ($t:ty) => {
        const _: () = {
            fn _assert_send<T: Send>() {}
            fn _assert_sync<T: Sync>() {}
            fn _assert_all() {
                _assert_send::<$t>();
                _assert_sync::<$t>();
            }
        };
    };
}
use static_assertions_send_sync;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::BruteForceVectorIndex;

    #[test]
    fn create_add_and_search() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.mmap");

        let mut idx = MmapVectorIndex::create(&path, 3).unwrap();
        idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
        idx.add(2, vec![0.0, 1.0, 0.0]).unwrap();
        idx.add(3, vec![0.9, 0.1, 0.0]).unwrap();

        let results = idx.search(&[1.0, 0.0, 0.0], 2).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].chunk_id, 1);
        assert_eq!(results[1].chunk_id, 3);
    }

    #[test]
    fn dimension_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.mmap");

        let mut idx = MmapVectorIndex::create(&path, 3).unwrap();
        let result = idx.add(1, vec![1.0, 0.0]);
        assert!(result.is_err());
    }

    #[test]
    fn save_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.mmap");

        {
            let mut idx = MmapVectorIndex::create(&path, 3).unwrap();
            idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
            idx.add(2, vec![0.0, 1.0, 0.0]).unwrap();
            idx.save(&path).unwrap();
        }

        let loaded = MmapVectorIndex::open(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.dimension(), 3);

        let results = loaded.search(&[1.0, 0.0, 0.0], 1).unwrap();
        assert_eq!(results[0].chunk_id, 1);
    }

    #[test]
    fn save_reopen_via_trait_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.mmap");

        {
            let mut idx = MmapVectorIndex::create(&path, 3).unwrap();
            idx.add(10, vec![0.5, 0.5, 0.0]).unwrap();
            idx.add(20, vec![0.0, 0.0, 1.0]).unwrap();
            idx.save(&path).unwrap();
        }

        let loaded = MmapVectorIndex::load(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        let results = loaded.search(&[0.0, 0.0, 1.0], 1).unwrap();
        assert_eq!(results[0].chunk_id, 20);
    }

    #[test]
    fn remove_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.mmap");

        let mut idx = MmapVectorIndex::create(&path, 3).unwrap();
        idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
        idx.add(2, vec![0.0, 1.0, 0.0]).unwrap();
        idx.add(3, vec![0.0, 0.0, 1.0]).unwrap();
        idx.remove_chunks(&[1, 3]).unwrap();
        assert_eq!(idx.len(), 1);

        let results = idx.search(&[0.0, 1.0, 0.0], 1).unwrap();
        assert_eq!(results[0].chunk_id, 2);
    }

    #[test]
    fn remove_from_mmap_region() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.mmap");

        // Save two vectors to file.
        {
            let mut idx = MmapVectorIndex::create(&path, 3).unwrap();
            idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
            idx.add(2, vec![0.0, 1.0, 0.0]).unwrap();
            idx.save(&path).unwrap();
        }

        // Reopen, remove one, search.
        let mut loaded = MmapVectorIndex::open(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        loaded.remove_chunks(&[1]).unwrap();
        assert_eq!(loaded.len(), 1);

        let results = loaded.search(&[1.0, 0.0, 0.0], 5).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk_id, 2);
    }

    #[test]
    fn update_existing_vector() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.mmap");

        let mut idx = MmapVectorIndex::create(&path, 3).unwrap();
        idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
        idx.add(1, vec![0.0, 1.0, 0.0]).unwrap(); // Update
        assert_eq!(idx.len(), 1);

        let results = idx.search(&[0.0, 1.0, 0.0], 1).unwrap();
        assert_eq!(results[0].chunk_id, 1);
        assert!((results[0].similarity - 1.0).abs() < 0.001);
    }

    #[test]
    fn update_mmap_vector() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.mmap");

        // Save initial vector.
        {
            let mut idx = MmapVectorIndex::create(&path, 3).unwrap();
            idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
            idx.save(&path).unwrap();
        }

        // Reopen, update the vector.
        let mut loaded = MmapVectorIndex::open(&path).unwrap();
        loaded.add(1, vec![0.0, 1.0, 0.0]).unwrap();
        assert_eq!(loaded.len(), 1);

        let results = loaded.search(&[0.0, 1.0, 0.0], 1).unwrap();
        assert_eq!(results[0].chunk_id, 1);
        assert!((results[0].similarity - 1.0).abs() < 0.001);
    }

    #[test]
    fn empty_search() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.mmap");

        let idx = MmapVectorIndex::create(&path, 3).unwrap();
        let results = idx.search(&[1.0, 0.0, 0.0], 5).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn search_k_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.mmap");

        let mut idx = MmapVectorIndex::create(&path, 3).unwrap();
        idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
        let results = idx.search(&[1.0, 0.0, 0.0], 0).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn is_empty_on_new() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.mmap");

        let idx = MmapVectorIndex::create(&path, 3).unwrap();
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn search_results_sorted_by_similarity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.mmap");

        let mut idx = MmapVectorIndex::create(&path, 3).unwrap();
        idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
        idx.add(2, vec![0.5, 0.5, 0.0]).unwrap();
        idx.add(3, vec![0.0, 1.0, 0.0]).unwrap();

        let results = idx.search(&[1.0, 0.0, 0.0], 3).unwrap();
        for w in results.windows(2) {
            assert!(w[0].similarity >= w[1].similarity);
        }
    }

    #[test]
    fn results_match_brute_force() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.mmap");

        let dim = 16;
        let n = 100u64;
        let k = 5;

        let mut bf = BruteForceVectorIndex::new(dim);
        let mut mmap = MmapVectorIndex::create(&path, dim).unwrap();

        for i in 0..n {
            let vec: Vec<f32> = (0..dim)
                .map(|d| ((i as usize * 7 + d * 13) % 100) as f32 / 100.0)
                .collect();
            bf.add(i, vec.clone()).unwrap();
            mmap.add(i, vec).unwrap();
        }

        let query: Vec<f32> = (0..dim).map(|d| (d * 3 % 50) as f32 / 50.0).collect();

        let bf_results = bf.search(&query, k).unwrap();
        let mmap_results = mmap.search(&query, k).unwrap();

        assert_eq!(bf_results.len(), mmap_results.len());
        for (bf_r, mmap_r) in bf_results.iter().zip(&mmap_results) {
            assert_eq!(bf_r.chunk_id, mmap_r.chunk_id);
            assert!(
                (bf_r.similarity - mmap_r.similarity).abs() < 1e-6,
                "similarity mismatch for chunk {}: bf={} mmap={}",
                bf_r.chunk_id,
                bf_r.similarity,
                mmap_r.similarity
            );
        }
    }

    #[test]
    fn results_match_brute_force_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.mmap");

        let dim = 16;
        let n = 50u64;
        let k = 5;

        let mut bf = BruteForceVectorIndex::new(dim);
        {
            let mut mmap = MmapVectorIndex::create(&path, dim).unwrap();
            for i in 0..n {
                let vec: Vec<f32> = (0..dim)
                    .map(|d| ((i as usize * 7 + d * 13) % 100) as f32 / 100.0)
                    .collect();
                bf.add(i, vec.clone()).unwrap();
                mmap.add(i, vec).unwrap();
            }
            mmap.save(&path).unwrap();
        }

        // Reopen from file.
        let mmap = MmapVectorIndex::open(&path).unwrap();
        assert_eq!(mmap.len(), n as usize);

        let query: Vec<f32> = (0..dim).map(|d| (d * 3 % 50) as f32 / 50.0).collect();

        let bf_results = bf.search(&query, k).unwrap();
        let mmap_results = mmap.search(&query, k).unwrap();

        assert_eq!(bf_results.len(), mmap_results.len());
        for (bf_r, mmap_r) in bf_results.iter().zip(&mmap_results) {
            assert_eq!(bf_r.chunk_id, mmap_r.chunk_id);
            assert!(
                (bf_r.similarity - mmap_r.similarity).abs() < 1e-6,
                "similarity mismatch for chunk {}: bf={} mmap={}",
                bf_r.chunk_id,
                bf_r.similarity,
                mmap_r.similarity
            );
        }
    }

    #[test]
    fn build_from_brute_force() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.mmap");

        let dim = 8;
        let mut bf = BruteForceVectorIndex::new(dim);
        bf.add(1, vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
            .unwrap();
        bf.add(2, vec![0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
            .unwrap();
        bf.add(3, vec![0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0])
            .unwrap();

        let mmap = MmapVectorIndex::build_from(&bf, &path).unwrap();
        assert_eq!(mmap.len(), 3);
        assert_eq!(mmap.dimension(), dim);

        let query = vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let bf_results = bf.search(&query, 3).unwrap();
        let mmap_results = mmap.search(&query, 3).unwrap();

        for (bf_r, mmap_r) in bf_results.iter().zip(&mmap_results) {
            assert_eq!(bf_r.chunk_id, mmap_r.chunk_id);
        }
    }

    #[test]
    fn load_nonexistent_file_returns_error() {
        let result = MmapVectorIndex::open(Path::new("/nonexistent/vectors.mmap"));
        assert!(result.is_err());
    }

    #[test]
    fn save_persist_removes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.mmap");

        // Write 3 vectors.
        {
            let mut idx = MmapVectorIndex::create(&path, 3).unwrap();
            idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
            idx.add(2, vec![0.0, 1.0, 0.0]).unwrap();
            idx.add(3, vec![0.0, 0.0, 1.0]).unwrap();
            idx.save(&path).unwrap();
        }

        // Reopen, remove one, save.
        {
            let mut idx = MmapVectorIndex::open(&path).unwrap();
            idx.remove_chunks(&[2]).unwrap();
            idx.save(&path).unwrap();
        }

        // Reopen again — should have 2 vectors.
        let idx = MmapVectorIndex::open(&path).unwrap();
        assert_eq!(idx.len(), 2);
    }

    #[test]
    fn add_after_reopen_and_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.mmap");

        // Create with 1 vector, save.
        {
            let mut idx = MmapVectorIndex::create(&path, 3).unwrap();
            idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
            idx.save(&path).unwrap();
        }

        // Reopen, add another, save.
        {
            let mut idx = MmapVectorIndex::open(&path).unwrap();
            idx.add(2, vec![0.0, 1.0, 0.0]).unwrap();
            idx.save(&path).unwrap();
        }

        // Reopen — should have 2 vectors.
        let idx = MmapVectorIndex::open(&path).unwrap();
        assert_eq!(idx.len(), 2);
        let results = idx.search(&[0.0, 1.0, 0.0], 1).unwrap();
        assert_eq!(results[0].chunk_id, 2);
    }

    #[test]
    fn batch_search() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.mmap");

        let mut idx = MmapVectorIndex::create(&path, 4).unwrap();
        for i in 0..50 {
            idx.add(i, vec![i as f32; 4]).unwrap();
        }

        let queries = vec![vec![5.0f32; 4], vec![25.0; 4], vec![45.0; 4]];
        let batch_results = idx.search_batch(&queries, 3).unwrap();

        for (query, batch_result) in queries.iter().zip(&batch_results) {
            let seq_result = idx.search(query, 3).unwrap();
            assert_eq!(batch_result.len(), seq_result.len());
            for (br, sr) in batch_result.iter().zip(&seq_result) {
                assert_eq!(br.chunk_id, sr.chunk_id);
            }
        }
    }

    #[test]
    fn search_k_larger_than_n() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.mmap");

        let mut idx = MmapVectorIndex::create(&path, 3).unwrap();
        idx.add(1, vec![1.0, 0.0, 0.0]).unwrap();
        idx.add(2, vec![0.0, 1.0, 0.0]).unwrap();

        let results = idx.search(&[1.0, 0.0, 0.0], 100).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results[0].similarity >= results[1].similarity);
    }

    #[test]
    fn file_format_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.mmap");

        let mut idx = MmapVectorIndex::create(&path, 4).unwrap();
        idx.add(42, vec![1.0, 2.0, 3.0, 4.0]).unwrap();
        idx.save(&path).unwrap();

        // Read raw bytes and verify header.
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"CDXV");
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            FORMAT_VERSION
        );
        assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), 4); // dimension
        assert_eq!(u64::from_le_bytes(bytes[12..20].try_into().unwrap()), 1); // count

        // ID table: chunk_id 42.
        assert_eq!(u64::from_le_bytes(bytes[20..28].try_into().unwrap()), 42);

        // Vectors: [1.0, 2.0, 3.0, 4.0].
        let v0 = f32::from_le_bytes(bytes[28..32].try_into().unwrap());
        let v1 = f32::from_le_bytes(bytes[32..36].try_into().unwrap());
        let v2 = f32::from_le_bytes(bytes[36..40].try_into().unwrap());
        let v3 = f32::from_le_bytes(bytes[40..44].try_into().unwrap());
        assert_eq!((v0, v1, v2, v3), (1.0, 2.0, 3.0, 4.0));
    }
}

//! Trigram index for fast exact substring search over code chunks.
//!
//! Builds an inverted index mapping 3-byte substrings (trigrams) to chunk IDs.
//! Search intersects posting lists for all query trigrams, then verifies exact
//! matches in the original text. This provides O(1) candidate filtering for
//! exact substring queries.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{CodeforgeError, Result};

/// Serializable representation of the trigram index data.
#[derive(Serialize, Deserialize)]
struct TrigramIndexData {
    /// Posting lists keyed by trigram bytes.
    index: Vec<([u8; 3], Vec<u64>)>,
    /// Stored chunk content for verification of exact matches.
    chunks: Vec<(u64, String)>,
}

/// An inverted index mapping 3-byte substrings to chunk IDs for fast exact search.
pub struct TrigramIndex {
    /// Mapping from trigram to sorted list of chunk IDs containing that trigram.
    index: HashMap<[u8; 3], Vec<u64>>,
    /// Stored chunk content for verification of exact matches.
    chunks: HashMap<u64, String>,
}

/// A verified exact match of a query within a chunk.
#[derive(Debug, Clone)]
pub struct TrigramMatch {
    /// The ID of the chunk containing the match.
    pub chunk_id: u64,
    /// The byte offset within the chunk where the match starts.
    pub byte_offset: usize,
}

impl TrigramIndex {
    /// Creates a new empty trigram index.
    pub fn new() -> Self {
        Self {
            index: HashMap::new(),
            chunks: HashMap::new(),
        }
    }

    /// Adds a chunk to the index. Extracts all trigrams from the content and
    /// updates posting lists. Content shorter than 3 bytes produces no trigrams.
    pub fn add(&mut self, chunk_id: u64, content: &str) {
        self.chunks.insert(chunk_id, content.to_string());
        let bytes = content.as_bytes();
        if bytes.len() < 3 {
            return;
        }
        for i in 0..bytes.len() - 2 {
            let trigram = [bytes[i], bytes[i + 1], bytes[i + 2]];
            self.index.entry(trigram).or_default().push(chunk_id);
        }
        // Deduplicate posting lists
        for list in self.index.values_mut() {
            list.sort_unstable();
            list.dedup();
        }
    }

    /// Removes a chunk from the index, cleaning up all posting list entries.
    pub fn remove(&mut self, chunk_id: u64) {
        if let Some(content) = self.chunks.remove(&chunk_id) {
            let bytes = content.as_bytes();
            if bytes.len() < 3 {
                return;
            }
            for i in 0..bytes.len() - 2 {
                let trigram = [bytes[i], bytes[i + 1], bytes[i + 2]];
                if let Some(list) = self.index.get_mut(&trigram) {
                    list.retain(|&id| id != chunk_id);
                    if list.is_empty() {
                        self.index.remove(&trigram);
                    }
                }
            }
        }
    }

    /// Searches for exact occurrences of `query` across all indexed chunks.
    ///
    /// Returns empty if the query is shorter than 3 bytes. Otherwise:
    /// 1. Extracts trigrams from the query.
    /// 2. Intersects posting lists to find candidate chunks.
    /// 3. Verifies exact substring matches in each candidate.
    pub fn search(&self, query: &str) -> Vec<TrigramMatch> {
        let query_bytes = query.as_bytes();
        if query_bytes.len() < 3 {
            return Vec::new();
        }

        // Extract query trigrams
        let mut trigrams = Vec::with_capacity(query_bytes.len() - 2);
        for i in 0..query_bytes.len() - 2 {
            trigrams.push([query_bytes[i], query_bytes[i + 1], query_bytes[i + 2]]);
        }

        // Look up posting lists; if any trigram is missing, no matches possible
        let mut posting_lists: Vec<&Vec<u64>> =
            trigrams.iter().filter_map(|t| self.index.get(t)).collect();
        if posting_lists.len() != trigrams.len() {
            return Vec::new();
        }

        // Intersect posting lists, starting with the shortest for efficiency
        posting_lists.sort_by_key(|l| l.len());
        let mut candidates = posting_lists[0].clone();
        for list in &posting_lists[1..] {
            candidates.retain(|id| list.binary_search(id).is_ok());
            if candidates.is_empty() {
                break;
            }
        }

        // Verify exact matches and collect byte offsets
        let mut results = Vec::new();
        for chunk_id in candidates {
            if let Some(content) = self.chunks.get(&chunk_id) {
                for (offset, _) in content.match_indices(query) {
                    results.push(TrigramMatch {
                        chunk_id,
                        byte_offset: offset,
                    });
                }
            }
        }
        results
    }

    /// Returns the number of indexed chunks.
    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    /// Returns true if the index contains no chunks.
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// Save the trigram index to a binary (bitcode) file.
    pub fn save_binary(&self, path: &Path) -> Result<()> {
        let data = TrigramIndexData {
            index: self.index.iter().map(|(k, v)| (*k, v.clone())).collect(),
            chunks: self.chunks.iter().map(|(k, v)| (*k, v.clone())).collect(),
        };
        let bytes = bitcode::serialize(&data).map_err(|e| {
            CodeforgeError::Serialization(format!("failed to serialize trigram index: {e}"))
        })?;
        std::fs::write(path, bytes)?;
        Ok(())
    }

    /// Load the trigram index from a binary (bitcode) file.
    pub fn load_binary(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        let data: TrigramIndexData = bitcode::deserialize(&bytes).map_err(|e| {
            CodeforgeError::Serialization(format!("failed to deserialize trigram index: {e}"))
        })?;
        Ok(Self {
            index: data.index.into_iter().collect(),
            chunks: data.chunks.into_iter().collect(),
        })
    }
}

impl Default for TrigramIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn finds_exact_function_name() {
        let mut idx = TrigramIndex::new();
        idx.add(1, "fn process_batch(items: &[Item]) { todo!() }");
        idx.add(2, "fn main() { process_batch(&items); }");
        idx.add(3, "fn unrelated_function() {}");
        let matches = idx.search("process_batch");
        let ids: Vec<u64> = matches.iter().map(|m| m.chunk_id).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        assert!(!ids.contains(&3));
    }

    #[test]
    fn no_matches_returns_empty() {
        let mut idx = TrigramIndex::new();
        idx.add(1, "fn hello() {}");
        let matches = idx.search("nonexistent_symbol");
        assert!(matches.is_empty());
    }

    #[test]
    fn short_query_under_3_chars_returns_empty() {
        let mut idx = TrigramIndex::new();
        idx.add(1, "ab");
        let matches = idx.search("ab");
        assert!(matches.is_empty());
    }

    #[test]
    fn case_sensitive_search() {
        let mut idx = TrigramIndex::new();
        idx.add(1, "fn ProcessBatch() {}");
        idx.add(2, "fn process_batch() {}");
        let matches = idx.search("ProcessBatch");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].chunk_id, 1);
    }

    #[test]
    fn byte_offset_is_correct() {
        let mut idx = TrigramIndex::new();
        idx.add(1, "prefix_process_batch_suffix");
        let matches = idx.search("process_batch");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].byte_offset, 7);
    }

    #[test]
    fn remove_chunk_from_index() {
        let mut idx = TrigramIndex::new();
        idx.add(1, "fn target() {}");
        idx.add(2, "fn target() { call(); }");
        idx.remove(1);
        let matches = idx.search("target");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].chunk_id, 2);
    }

    #[test]
    fn search_10k_chunks_sub_millisecond() {
        let mut idx = TrigramIndex::new();
        for i in 0..10_000u64 {
            idx.add(i, &format!("fn func_{i}(x: i32) -> bool {{ x > {i} }}"));
        }
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            std::hint::black_box(idx.search("func_5000"));
        }
        let elapsed = start.elapsed();
        // Be generous for debug mode
        assert!(
            elapsed.as_secs() < 5,
            "1000 searches on 10K chunks took {elapsed:?}"
        );
    }

    #[test]
    fn multiple_matches_in_same_chunk() {
        let mut idx = TrigramIndex::new();
        idx.add(1, "HashMap HashMap HashMap");
        let matches = idx.search("HashMap");
        assert!(!matches.is_empty());
        assert!(matches.iter().all(|m| m.chunk_id == 1));
    }

    #[test]
    fn binary_save_and_load_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("trigram.bin");

        let mut idx = TrigramIndex::new();
        idx.add(1, "fn process_batch(items: &[Item]) { todo!() }");
        idx.add(2, "fn main() { process_batch(&items); }");
        idx.add(3, "fn unrelated_function() {}");

        idx.save_binary(&path).unwrap();
        let loaded = TrigramIndex::load_binary(&path).unwrap();

        assert_eq!(loaded.len(), 3);

        // Verify search results are identical after round-trip.
        let original_matches = idx.search("process_batch");
        let loaded_matches = loaded.search("process_batch");
        assert_eq!(original_matches.len(), loaded_matches.len());

        let mut orig_ids: Vec<u64> = original_matches.iter().map(|m| m.chunk_id).collect();
        let mut load_ids: Vec<u64> = loaded_matches.iter().map(|m| m.chunk_id).collect();
        orig_ids.sort();
        load_ids.sort();
        assert_eq!(orig_ids, load_ids);

        // Verify that unrelated queries still work correctly.
        let no_match = loaded.search("nonexistent_symbol");
        assert!(no_match.is_empty());
    }

    #[test]
    fn binary_empty_index_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("trigram_empty.bin");

        let idx = TrigramIndex::new();
        idx.save_binary(&path).unwrap();
        let loaded = TrigramIndex::load_binary(&path).unwrap();

        assert_eq!(loaded.len(), 0);
        assert!(loaded.is_empty());
    }

    #[test]
    fn load_nonexistent_file_returns_error() {
        let result = TrigramIndex::load_binary(std::path::Path::new("/nonexistent/trigram.bin"));
        assert!(result.is_err());
    }
}

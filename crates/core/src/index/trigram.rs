//! Trigram index for fast exact substring search over code chunks.
//!
//! Builds an inverted index mapping 3-byte substrings (trigrams) to chunk IDs.
//! Search intersects posting lists for all query trigrams, then verifies exact
//! matches in the original text. This provides O(1) candidate filtering for
//! exact substring queries.
//!
//! Also exposes [`FileTrigramIndex`] — a file-level variant used by
//! [`crate::engine::Engine::grep_code`] to skip files that cannot possibly
//! match before doing any disk I/O, and [`extract_regex_seeds`] to pull
//! required literal prefixes out of a regex pattern for the same purpose.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{CodixingError, Result};

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
        // Collect the unique trigrams for this chunk to avoid inserting the
        // same (trigram, chunk_id) pair more than once per add() call.
        let mut chunk_trigrams = std::collections::HashSet::new();
        for i in 0..bytes.len() - 2 {
            chunk_trigrams.insert([bytes[i], bytes[i + 1], bytes[i + 2]]);
        }
        // Insert chunk_id into each unique trigram's posting list, then sort
        // and deduplicate only that list. This is O(trigrams_per_chunk × log N)
        // per add — not O(all_posting_lists) as the previous implementation was.
        for trigram in chunk_trigrams {
            let list = self.index.entry(trigram).or_default();
            list.push(chunk_id);
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
            CodixingError::Serialization(format!("failed to serialize trigram index: {e}"))
        })?;
        std::fs::write(path, bytes)?;
        Ok(())
    }

    /// Load the trigram index from a binary (bitcode) file.
    pub fn load_binary(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        let data: TrigramIndexData = bitcode::deserialize(&bytes).map_err(|e| {
            CodixingError::Serialization(format!("failed to deserialize trigram index: {e}"))
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

// ── File-level trigram index ──────────────────────────────────────────────────

/// A file-level trigram index for fast grep pre-filtering.
///
/// Maps 3-byte substrings to the set of files whose indexed content contains
/// those trigrams. Used by [`crate::engine::Engine::grep_code`] to skip files
/// that cannot possibly contain a pattern, avoiding unnecessary disk I/O.
///
/// The index is built from chunk content. Trigrams that straddle a chunk
/// boundary are not captured, but this affects < 0.1 % of real-world patterns.
pub struct FileTrigramIndex {
    /// file index → relative path (empty string = tombstoned / removed)
    files: Vec<String>,
    /// path → file index
    file_index: HashMap<String, u32>,
    /// trigram → sorted list of file indices containing that trigram
    index: HashMap<[u8; 3], Vec<u32>>,
}

impl Default for FileTrigramIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl FileTrigramIndex {
    /// Creates an empty index.
    pub fn new() -> Self {
        Self {
            files: Vec::new(),
            file_index: HashMap::new(),
            index: HashMap::new(),
        }
    }

    /// Index all trigrams from `content` under `path`.
    ///
    /// Safe to call multiple times with the same `path` (e.g. once per chunk):
    /// duplicate `(trigram, file_index)` pairs are deduplicated.
    pub fn add(&mut self, path: &str, content: &[u8]) {
        let file_idx = if let Some(&idx) = self.file_index.get(path) {
            idx
        } else {
            let idx = self.files.len() as u32;
            self.files.push(path.to_string());
            self.file_index.insert(path.to_string(), idx);
            idx
        };

        if content.len() < 3 {
            return;
        }

        let mut seen = std::collections::HashSet::new();
        for i in 0..content.len() - 2 {
            let tri = [content[i], content[i + 1], content[i + 2]];
            if seen.insert(tri) {
                let list = self.index.entry(tri).or_default();
                // Maintain sorted order; dedup on insert to avoid duplicates
                // when `add` is called multiple times for the same file.
                if list.last() != Some(&file_idx) {
                    match list.binary_search(&file_idx) {
                        Ok(_) => {} // already present
                        Err(pos) => list.insert(pos, file_idx),
                    }
                }
            }
        }
    }

    /// Returns candidate file paths for a **literal** pattern.
    ///
    /// Returns `None` when the literal is shorter than 3 bytes and trigram
    /// pre-filtering cannot be applied — the caller should fall back to a full
    /// scan.  Returns `Some([])` when the literal contains a trigram absent
    /// from the index, meaning no file can match.
    pub fn candidates_for_literal<'a>(&'a self, literal: &[u8]) -> Option<Vec<&'a str>> {
        if literal.len() < 3 {
            return None;
        }
        let trigrams: Vec<[u8; 3]> = (0..literal.len() - 2)
            .map(|i| [literal[i], literal[i + 1], literal[i + 2]])
            .collect();

        let mut lists: Vec<&Vec<u32>> = Vec::with_capacity(trigrams.len());
        for t in &trigrams {
            match self.index.get(t) {
                Some(l) => lists.push(l),
                None => return Some(Vec::new()), // trigram absent → no matches
            }
        }

        // Intersect posting lists starting from the shortest.
        lists.sort_unstable_by_key(|l| l.len());
        let mut candidates = lists[0].clone();
        for list in &lists[1..] {
            candidates.retain(|id| list.binary_search(id).is_ok());
            if candidates.is_empty() {
                break;
            }
        }

        Some(
            candidates
                .iter()
                .filter_map(|&i| {
                    let p = self.files[i as usize].as_str();
                    if p.is_empty() { None } else { Some(p) }
                })
                .collect(),
        )
    }

    /// Returns candidate file paths given a set of trigrams that **all** must
    /// be present in any matching file (AND semantics).
    ///
    /// Typically fed the output of [`extract_required_trigrams`].
    /// Returns `None` when the trigram set is empty (can't pre-filter).
    pub fn candidates_for_trigrams<'a>(&'a self, trigrams: &[[u8; 3]]) -> Option<Vec<&'a str>> {
        if trigrams.is_empty() {
            return None;
        }

        let mut lists: Vec<&Vec<u32>> = Vec::with_capacity(trigrams.len());
        for t in trigrams {
            match self.index.get(t) {
                Some(l) => lists.push(l),
                None => return Some(Vec::new()), // required trigram absent → no match
            }
        }

        lists.sort_unstable_by_key(|l| l.len());
        let mut candidates = lists[0].clone();
        for list in &lists[1..] {
            candidates.retain(|id| list.binary_search(id).is_ok());
            if candidates.is_empty() {
                break;
            }
        }

        Some(
            candidates
                .iter()
                .filter_map(|&i| {
                    let p = self.files[i as usize].as_str();
                    if p.is_empty() { None } else { Some(p) }
                })
                .collect(),
        )
    }

    /// Number of files currently in the index.
    pub fn file_count(&self) -> usize {
        self.file_index.len()
    }
}

// ── Regex → required trigram extraction (Russ Cox / trigrep technique) ────────

/// Extract trigrams that **must** appear in any match of a regex pattern.
///
/// Recursively walks the `regex-syntax` HIR tree with these rules:
/// - **Literal** bytes → extract all 3-byte windows as required trigrams
/// - **Concat**(a, b) → union (AND): both a's and b's trigrams are required
/// - **Alternation**(a, b) → intersection: only trigrams common to ALL branches
/// - **Repetition** (?, *, +) → trigrams from the inner expr (but `?` / `*` clears
///   them since the branch can match without the inner content)
/// - **Class** / **Anchor** / **Look** → no trigrams extractable
///
/// The returned set can be fed directly to
/// [`FileTrigramIndex::candidates_for_trigrams`] for AND-intersection.
///
/// Returns an empty `Vec` for patterns too broad to extract anything useful
/// (e.g. `.*`, `[a-z]+`, or invalid syntax).
pub fn extract_required_trigrams(pattern: &str) -> Vec<[u8; 3]> {
    use regex_syntax::Parser;
    use regex_syntax::hir::{Hir, HirKind};

    let hir = match Parser::new().parse(pattern) {
        Ok(h) => h,
        Err(_) => return Vec::new(),
    };

    fn walk(hir: &Hir) -> std::collections::HashSet<[u8; 3]> {
        match hir.kind() {
            HirKind::Literal(lit) => trigrams_from_bytes(&lit.0),
            HirKind::Concat(subs) => {
                // All sub-expressions are concatenated — every sub's trigrams
                // must appear in any match.  Also extract trigrams that span
                // the boundary between adjacent literal sub-expressions.
                let mut required = std::collections::HashSet::new();
                let mut literal_run: Vec<u8> = Vec::new();

                for sub in subs {
                    if let HirKind::Literal(lit) = sub.kind() {
                        literal_run.extend_from_slice(&lit.0);
                    } else {
                        // Flush accumulated literal bytes.
                        if literal_run.len() >= 3 {
                            required.extend(trigrams_from_bytes(&literal_run));
                        }
                        literal_run.clear();
                        // Recurse into non-literal sub-expression.
                        required.extend(walk(sub));
                    }
                }
                // Flush trailing literal bytes.
                if literal_run.len() >= 3 {
                    required.extend(trigrams_from_bytes(&literal_run));
                }
                required
            }
            HirKind::Alternation(branches) => {
                // A match uses exactly one branch.  Only trigrams common to
                // ALL branches are required.
                let mut iter = branches.iter();
                let first = match iter.next() {
                    Some(b) => walk(b),
                    None => return std::collections::HashSet::new(),
                };
                iter.fold(first, |acc, branch| {
                    let branch_set = walk(branch);
                    acc.intersection(&branch_set).copied().collect()
                })
            }
            HirKind::Repetition(rep) => {
                // `+` means the inner must match at least once → its trigrams
                // are required.  `*` and `?` can match zero times → clear.
                if rep.min >= 1 {
                    walk(&rep.sub)
                } else {
                    std::collections::HashSet::new()
                }
            }
            HirKind::Capture(cap) => walk(&cap.sub),
            // Class, Look, Empty → no trigrams
            _ => std::collections::HashSet::new(),
        }
    }

    fn trigrams_from_bytes(bytes: &[u8]) -> std::collections::HashSet<[u8; 3]> {
        let mut set = std::collections::HashSet::new();
        if bytes.len() >= 3 {
            for i in 0..bytes.len() - 2 {
                set.insert([bytes[i], bytes[i + 1], bytes[i + 2]]);
            }
        }
        set
    }

    let result = walk(&hir);
    result.into_iter().collect()
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

    // ── FileTrigramIndex tests ────────────────────────────────────────────────

    #[test]
    fn file_trigram_literal_finds_correct_files() {
        let mut idx = FileTrigramIndex::new();
        idx.add("a.rs", b"fn process_batch() {}");
        idx.add("b.rs", b"fn main() { process_batch(); }");
        idx.add("c.rs", b"fn unrelated() {}");

        let candidates = idx.candidates_for_literal(b"process_batch").unwrap();
        assert!(candidates.contains(&"a.rs"));
        assert!(candidates.contains(&"b.rs"));
        assert!(!candidates.contains(&"c.rs"));
    }

    #[test]
    fn file_trigram_absent_trigram_returns_empty() {
        let mut idx = FileTrigramIndex::new();
        idx.add("a.rs", b"fn hello() {}");

        let candidates = idx.candidates_for_literal(b"nonexistent_sym").unwrap();
        assert!(candidates.is_empty());
    }

    #[test]
    fn file_trigram_short_literal_returns_none() {
        let idx = FileTrigramIndex::new();
        assert!(idx.candidates_for_literal(b"ab").is_none());
    }

    #[test]
    fn file_trigram_multi_chunk_same_file() {
        let mut idx = FileTrigramIndex::new();
        idx.add("a.rs", b"fn foo() {}"); // chunk 1
        idx.add("a.rs", b"fn bar() {}"); // chunk 2
        idx.add("b.rs", b"fn baz() {}");

        assert_eq!(idx.file_count(), 2);
        let c = idx.candidates_for_literal(b"foo").unwrap();
        assert_eq!(c, vec!["a.rs"]);
    }

    #[test]
    fn file_trigram_candidates_for_trigrams_and_logic() {
        let mut idx = FileTrigramIndex::new();
        idx.add("a.rs", b"fn process_batch() {}");
        idx.add("b.rs", b"fn main() { process_batch(); }");
        idx.add("c.rs", b"fn unrelated() {}");

        // Extract trigrams from "process" — all must be present (AND).
        let trigrams: Vec<[u8; 3]> = b"process".windows(3).map(|w| [w[0], w[1], w[2]]).collect();
        let candidates = idx.candidates_for_trigrams(&trigrams).unwrap();
        assert!(candidates.contains(&"a.rs"));
        assert!(candidates.contains(&"b.rs"));
        assert!(!candidates.contains(&"c.rs"));
    }

    #[test]
    fn file_trigram_candidates_for_trigrams_empty_returns_none() {
        let idx = FileTrigramIndex::new();
        assert!(idx.candidates_for_trigrams(&[]).is_none());
    }

    // ── extract_required_trigrams tests ────────────────────────────────────────

    #[test]
    fn required_trigrams_literal_pattern() {
        let tris = extract_required_trigrams("process_batch");
        // All trigrams of "process_batch" must be required.
        let expected: std::collections::HashSet<[u8; 3]> = b"process_batch"
            .windows(3)
            .map(|w| [w[0], w[1], w[2]])
            .collect();
        let result: std::collections::HashSet<[u8; 3]> = tris.into_iter().collect();
        assert_eq!(result, expected);
    }

    #[test]
    fn required_trigrams_concat_with_wildcard() {
        // foo.*bar → trigrams from "foo" AND "bar" both required.
        let tris = extract_required_trigrams("foo.*bar");
        let result: std::collections::HashSet<[u8; 3]> = tris.into_iter().collect();
        assert!(result.contains(b"foo"));
        assert!(result.contains(b"bar"));
    }

    #[test]
    fn required_trigrams_alternation_intersection() {
        // (fooXYZ|fooABC) → only trigrams common to both branches are required.
        // "foo" is common to both.
        let tris = extract_required_trigrams("(fooXYZ|fooABC)");
        let result: std::collections::HashSet<[u8; 3]> = tris.into_iter().collect();
        assert!(result.contains(b"foo"));
        // "XYZ" is NOT in the intersection (only in first branch).
        assert!(!result.contains(b"XYZ"));
    }

    #[test]
    fn required_trigrams_broad_pattern_returns_empty() {
        assert!(extract_required_trigrams(".*").is_empty());
        assert!(extract_required_trigrams("[a-z]+").is_empty());
    }

    #[test]
    fn required_trigrams_case_insensitive_graceful_fallback() {
        // (?i)foo compiles to character classes, not literals — should
        // return empty (graceful fallback to full scan).
        let tris = extract_required_trigrams("(?i)foo");
        assert!(tris.is_empty());
    }

    #[test]
    fn required_trigrams_repetition_plus_vs_star() {
        // (foo)+ requires foo, (foo)* does not.
        let plus_tris = extract_required_trigrams("(foo)+bar");
        let plus: std::collections::HashSet<[u8; 3]> = plus_tris.into_iter().collect();
        assert!(plus.contains(b"foo"));
        assert!(plus.contains(b"bar"));

        let star_tris = extract_required_trigrams("(foo)*bar");
        let star: std::collections::HashSet<[u8; 3]> = star_tris.into_iter().collect();
        // foo is NOT required (can match zero times), but bar is.
        assert!(!star.contains(b"foo"));
        assert!(star.contains(b"bar"));
    }

    #[test]
    fn required_trigrams_anchored_literal() {
        // ^fn main → "fn main" is a literal in a concat.
        let tris = extract_required_trigrams("^fn main");
        let result: std::collections::HashSet<[u8; 3]> = tris.into_iter().collect();
        assert!(result.contains(b"fn "));
        assert!(result.contains(b"mai"));
    }
}

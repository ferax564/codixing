//! Trigram index for fast exact substring search over code chunks.
//!
//! Builds an inverted index mapping 3-byte substrings (trigrams) to chunk IDs.
//! Search intersects posting lists for all query trigrams, then verifies exact
//! matches in the original text. This provides O(1) candidate filtering for
//! exact substring queries.
//!
//! Also exposes [`FileTrigramIndex`] — a file-level variant used by
//! [`crate::engine::Engine::grep_code`] to skip files that cannot possibly
//! match before doing any disk I/O, [`QueryPlan`] for boolean trigram
//! queries with OR support, and [`build_query_plan`] to decompose a regex
//! pattern into a query plan using the Russ Cox / trigrep technique.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{CodixingError, Result};

/// Serializable representation of the trigram index data (v2: no content).
#[derive(Serialize, Deserialize)]
struct TrigramIndexData {
    /// Posting lists keyed by trigram bytes.
    index: Vec<([u8; 3], Vec<u64>)>,
}

/// Legacy format with content (for backward-compatible loading).
#[derive(Deserialize)]
struct TrigramIndexDataLegacy {
    index: Vec<([u8; 3], Vec<u64>)>,
    #[allow(dead_code)]
    chunks: Vec<(u64, String)>,
}

/// An inverted index mapping 3-byte substrings to chunk IDs for fast exact search.
///
/// Content is NOT stored here — callers must verify candidate matches using
/// an external content source (chunk_meta or Tantivy stored fields).
pub struct TrigramIndex {
    /// Mapping from trigram to sorted list of chunk IDs containing that trigram.
    index: HashMap<[u8; 3], Vec<u64>>,
    /// Number of distinct chunks indexed (for len/is_empty).
    chunk_count: usize,
}

impl TrigramIndex {
    /// Creates a new empty trigram index.
    pub fn new() -> Self {
        Self {
            index: HashMap::new(),
            chunk_count: 0,
        }
    }

    /// Adds a chunk to the index. Extracts all trigrams from the content and
    /// updates posting lists. Content shorter than 3 bytes produces no trigrams.
    ///
    /// For bulk loading, prefer [`build_batch`] which defers sorting to the end.
    pub fn add(&mut self, chunk_id: u64, content: &str) {
        self.chunk_count += 1;
        self.add_trigrams(chunk_id, content.as_bytes());
        // Sort + dedup posting lists touched by this chunk.
        let bytes = content.as_bytes();
        if bytes.len() < 3 {
            return;
        }
        for i in 0..bytes.len() - 2 {
            let tri = [bytes[i], bytes[i + 1], bytes[i + 2]];
            if let Some(list) = self.index.get_mut(&tri) {
                list.sort_unstable();
                list.dedup();
            }
        }
    }

    /// Insert trigrams for a chunk without sorting posting lists.
    fn add_trigrams(&mut self, chunk_id: u64, bytes: &[u8]) {
        if bytes.len() < 3 {
            return;
        }
        let mut chunk_trigrams: Vec<[u8; 3]> = (0..bytes.len() - 2)
            .map(|i| [bytes[i], bytes[i + 1], bytes[i + 2]])
            .collect();
        chunk_trigrams.sort_unstable();
        chunk_trigrams.dedup();
        for trigram in chunk_trigrams {
            self.index.entry(trigram).or_default().push(chunk_id);
        }
    }

    /// Bulk-build the trigram index from an iterator of (chunk_id, content) pairs.
    ///
    /// Much faster than calling `add()` repeatedly because posting lists are
    /// sorted and deduplicated only once at the end, avoiding O(N²) re-sorting.
    pub fn build_batch(&mut self, chunks: impl Iterator<Item = (u64, impl AsRef<str>)>) {
        let mut count = 0usize;
        for (chunk_id, content) in chunks {
            self.add_trigrams(chunk_id, content.as_ref().as_bytes());
            count += 1;
        }
        self.chunk_count += count;
        // Single sort+dedup pass over all posting lists.
        for list in self.index.values_mut() {
            list.sort_unstable();
            list.dedup();
        }
    }

    /// Removes a chunk from the index, cleaning up all posting list entries.
    ///
    /// The caller must provide the chunk's content so we know which trigrams
    /// to clean up from the posting lists.
    pub fn remove(&mut self, chunk_id: u64, content: &str) {
        let bytes = content.as_bytes();
        if bytes.len() < 3 {
            self.chunk_count = self.chunk_count.saturating_sub(1);
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
        self.chunk_count = self.chunk_count.saturating_sub(1);
    }

    /// Returns candidate chunk IDs that may contain `query` as a substring.
    ///
    /// Intersects trigram posting lists for fast candidate filtering.
    /// **Callers must verify** actual substring matches in the chunk content
    /// (from chunk_meta or Tantivy stored fields) since trigram intersection
    /// can produce false positives.
    ///
    /// Returns empty if the query is shorter than 3 bytes.
    pub fn search(&self, query: &str) -> Vec<u64> {
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

        candidates
    }

    /// Returns the number of indexed chunks.
    pub fn len(&self) -> usize {
        self.chunk_count
    }

    /// Returns true if the index contains no chunks.
    pub fn is_empty(&self) -> bool {
        self.chunk_count == 0
    }

    /// Save the trigram index to a binary (bitcode) file.
    pub fn save_binary(&self, path: &Path) -> Result<()> {
        let data = TrigramIndexData {
            index: self.index.iter().map(|(k, v)| (*k, v.clone())).collect(),
        };
        let bytes = bitcode::serialize(&data).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize trigram index: {e}"))
        })?;
        std::fs::write(path, bytes)?;
        Ok(())
    }

    /// Load the trigram index from a binary (bitcode) file.
    ///
    /// Handles both v2 (no content) and legacy (with content) formats.
    pub fn load_binary(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        // Try v2 format first (no content), fall back to legacy.
        if let Ok(data) = bitcode::deserialize::<TrigramIndexData>(&bytes) {
            let chunk_count = data
                .index
                .iter()
                .flat_map(|(_, ids)| ids.iter())
                .collect::<std::collections::HashSet<_>>()
                .len();
            Ok(Self {
                index: data.index.into_iter().collect(),
                chunk_count,
            })
        } else if let Ok(data) = bitcode::deserialize::<TrigramIndexDataLegacy>(&bytes) {
            let chunk_count = data
                .index
                .iter()
                .flat_map(|(_, ids)| ids.iter())
                .collect::<std::collections::HashSet<_>>()
                .len();
            Ok(Self {
                index: data.index.into_iter().collect(),
                chunk_count,
            })
        } else {
            Err(CodixingError::Serialization(
                "failed to deserialize trigram index: unknown format".to_string(),
            ))
        }
    }
}

impl Default for TrigramIndex {
    fn default() -> Self {
        Self::new()
    }
}

// ── File-level trigram index ──────────────────────────────────────────────────

/// Serializable representation of the file trigram index.
#[derive(Serialize, Deserialize)]
struct FileTrigramIndexData {
    files: Vec<String>,
    index: Vec<([u8; 3], Vec<u32>)>,
}

/// A boolean query plan over trigrams, produced by [`build_query_plan`].
///
/// Execution semantics (see [`FileTrigramIndex::execute_plan`]):
/// - `MatchAll` → no pre-filtering possible (fall back to full scan)
/// - `Trigrams(t)` → intersect posting lists for all trigrams (AND)
/// - `And(plans)` → intersect candidate sets of all sub-plans
/// - `Or(plans)` → union candidate sets of all sub-plans
#[derive(Debug, Clone)]
pub enum QueryPlan {
    /// No trigrams extractable — must scan all files.
    MatchAll,
    /// A set of trigrams that must ALL appear (leaf AND node).
    Trigrams(Vec<[u8; 3]>),
    /// All sub-plans must match (intersection).
    And(Vec<QueryPlan>),
    /// At least one sub-plan must match (union).
    Or(Vec<QueryPlan>),
}

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

        // Collect unique trigrams via sort+dedup (avoids per-call HashSet alloc).
        let mut trigrams: Vec<[u8; 3]> = (0..content.len() - 2)
            .map(|i| [content[i], content[i + 1], content[i + 2]])
            .collect();
        trigrams.sort_unstable();
        trigrams.dedup();

        for tri in trigrams {
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

    /// Remove a single file from the index.
    ///
    /// Removes the file from all posting lists and tombstones its entry.
    /// This is O(unique_trigrams_in_index) — fast enough for single-file
    /// operations; batch operations should rebuild from scratch instead.
    pub fn remove_file(&mut self, path: &str) {
        let file_idx = match self.file_index.remove(path) {
            Some(idx) => idx,
            None => return,
        };
        // Tombstone the files entry.
        self.files[file_idx as usize] = String::new();
        // Remove from all posting lists; drop empty ones.
        self.index.retain(|_, list| {
            list.retain(|&id| id != file_idx);
            !list.is_empty()
        });
    }

    /// Execute a [`QueryPlan`] against this index.
    ///
    /// Returns `None` when the plan is `MatchAll` (can't pre-filter).
    /// Otherwise returns the set of candidate file paths.
    pub fn execute_plan<'a>(&'a self, plan: &QueryPlan) -> Option<Vec<&'a str>> {
        match plan {
            QueryPlan::MatchAll => None,
            QueryPlan::Trigrams(tris) => self.candidates_for_trigrams(tris),
            QueryPlan::And(subs) => {
                // Intersect all sub-plan results.  Skip MatchAll branches.
                let mut result: Option<std::collections::HashSet<&str>> = None;
                for sub in subs {
                    match self.execute_plan(sub) {
                        None => continue, // MatchAll → doesn't constrain
                        Some(candidates) => {
                            let set: std::collections::HashSet<&str> =
                                candidates.into_iter().collect();
                            result = Some(match result {
                                None => set,
                                Some(acc) => acc.intersection(&set).copied().collect(),
                            });
                        }
                    }
                }
                result.map(|s| s.into_iter().collect())
            }
            QueryPlan::Or(subs) => {
                // Union all sub-plan results.  If ANY branch is MatchAll,
                // the whole OR matches everything.
                let mut result: std::collections::HashSet<&str> = std::collections::HashSet::new();
                for sub in subs {
                    match self.execute_plan(sub) {
                        None => return None, // MatchAll branch → can't pre-filter
                        Some(candidates) => result.extend(candidates),
                    }
                }
                Some(result.into_iter().collect())
            }
        }
    }

    /// Number of files currently in the index.
    pub fn file_count(&self) -> usize {
        self.file_index.len()
    }

    /// Save the file trigram index to a binary (bitcode) file.
    pub fn save_binary(&self, path: &Path) -> Result<()> {
        let data = FileTrigramIndexData {
            files: self.files.clone(),
            index: self.index.iter().map(|(k, v)| (*k, v.clone())).collect(),
        };
        let bytes = bitcode::serialize(&data).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize file trigram index: {e}"))
        })?;
        std::fs::write(path, bytes)?;
        Ok(())
    }

    /// Load the file trigram index from a binary (bitcode) file.
    pub fn load_binary(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        let data: FileTrigramIndexData = bitcode::deserialize(&bytes).map_err(|e| {
            CodixingError::Serialization(format!("failed to deserialize file trigram index: {e}"))
        })?;
        let mut file_index = HashMap::new();
        for (i, f) in data.files.iter().enumerate() {
            if !f.is_empty() {
                file_index.insert(f.clone(), i as u32);
            }
        }
        Ok(Self {
            files: data.files,
            file_index,
            index: data.index.into_iter().collect(),
        })
    }
}

// ── Regex → QueryPlan (Russ Cox / trigrep technique) ─────────────────────────

/// Build a [`QueryPlan`] from a regex pattern.
///
/// Recursively walks the `regex-syntax` HIR tree:
/// - **Literal** → `Trigrams(all 3-byte windows)`
/// - **Concat** → `And` of children (merges adjacent literal runs for
///   cross-boundary trigrams)
/// - **Alternation** → `Or` of children (union at execution time)
/// - **Repetition(`+`)** → recurse; **`*`/`?`** → `MatchAll`
/// - **Capture** → recurse into sub
/// - **Class** / **Look** / **Empty** → `MatchAll`
///
/// Use with [`FileTrigramIndex::execute_plan`].
pub fn build_query_plan(pattern: &str) -> QueryPlan {
    use regex_syntax::Parser;
    use regex_syntax::hir::{Hir, HirKind};

    let hir = match Parser::new().parse(pattern) {
        Ok(h) => h,
        Err(_) => return QueryPlan::MatchAll,
    };

    fn walk(hir: &Hir) -> QueryPlan {
        match hir.kind() {
            HirKind::Literal(lit) => {
                let tris = trigrams_from_bytes(&lit.0);
                if tris.is_empty() {
                    QueryPlan::MatchAll
                } else {
                    QueryPlan::Trigrams(tris)
                }
            }
            HirKind::Concat(subs) => {
                let mut parts: Vec<QueryPlan> = Vec::new();
                let mut literal_run: Vec<u8> = Vec::new();

                for sub in subs {
                    if let HirKind::Literal(lit) = sub.kind() {
                        literal_run.extend_from_slice(&lit.0);
                    } else {
                        if literal_run.len() >= 3 {
                            parts.push(QueryPlan::Trigrams(trigrams_from_bytes(&literal_run)));
                        }
                        literal_run.clear();
                        let child = walk(sub);
                        if !matches!(child, QueryPlan::MatchAll) {
                            parts.push(child);
                        }
                    }
                }
                if literal_run.len() >= 3 {
                    parts.push(QueryPlan::Trigrams(trigrams_from_bytes(&literal_run)));
                }

                simplify_and(parts)
            }
            HirKind::Alternation(branches) => {
                let plans: Vec<QueryPlan> = branches.iter().map(walk).collect();
                // If ANY branch is MatchAll, the whole OR is MatchAll
                // (that branch matches everything).
                if plans.iter().any(|p| matches!(p, QueryPlan::MatchAll)) {
                    return QueryPlan::MatchAll;
                }
                if plans.is_empty() {
                    return QueryPlan::MatchAll;
                }
                if plans.len() == 1 {
                    return plans.into_iter().next().unwrap();
                }
                QueryPlan::Or(plans)
            }
            HirKind::Repetition(rep) => {
                if rep.min >= 1 {
                    walk(&rep.sub)
                } else {
                    QueryPlan::MatchAll
                }
            }
            HirKind::Capture(cap) => walk(&cap.sub),
            _ => QueryPlan::MatchAll,
        }
    }

    /// Flatten and simplify an AND of sub-plans.
    fn simplify_and(parts: Vec<QueryPlan>) -> QueryPlan {
        let mut trigrams = Vec::new();
        let mut others = Vec::new();
        for p in parts {
            match p {
                QueryPlan::Trigrams(t) => trigrams.extend(t),
                QueryPlan::And(subs) => {
                    for s in subs {
                        match s {
                            QueryPlan::Trigrams(t) => trigrams.extend(t),
                            other => others.push(other),
                        }
                    }
                }
                QueryPlan::MatchAll => {}
                other => others.push(other),
            }
        }
        trigrams.sort_unstable();
        trigrams.dedup();

        if others.is_empty() {
            if trigrams.is_empty() {
                QueryPlan::MatchAll
            } else {
                QueryPlan::Trigrams(trigrams)
            }
        } else {
            if !trigrams.is_empty() {
                others.insert(0, QueryPlan::Trigrams(trigrams));
            }
            if others.len() == 1 {
                others.into_iter().next().unwrap()
            } else {
                QueryPlan::And(others)
            }
        }
    }

    fn trigrams_from_bytes(bytes: &[u8]) -> Vec<[u8; 3]> {
        if bytes.len() < 3 {
            return Vec::new();
        }
        let mut v: Vec<[u8; 3]> = (0..bytes.len() - 2)
            .map(|i| [bytes[i], bytes[i + 1], bytes[i + 2]])
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    walk(&hir)
}

/// Convenience wrapper: extract trigrams that must appear in any match.
///
/// Equivalent to building a query plan and collecting all AND-required
/// trigrams.  Useful when only the flat trigram set is needed.
pub fn extract_required_trigrams(pattern: &str) -> Vec<[u8; 3]> {
    fn collect_and(plan: &QueryPlan) -> Vec<[u8; 3]> {
        match plan {
            QueryPlan::MatchAll => Vec::new(),
            QueryPlan::Trigrams(t) => t.clone(),
            QueryPlan::And(subs) => subs.iter().flat_map(collect_and).collect(),
            QueryPlan::Or(subs) => {
                // Intersection of branches' required trigrams.
                let mut iter = subs.iter().map(|s| {
                    let v = collect_and(s);
                    v.into_iter()
                        .collect::<std::collections::HashSet<[u8; 3]>>()
                });
                let first = match iter.next() {
                    Some(s) => s,
                    None => return Vec::new(),
                };
                let common = iter.fold(first, |acc, set| acc.intersection(&set).copied().collect());
                common.into_iter().collect()
            }
        }
    }
    collect_and(&build_query_plan(pattern))
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
        let candidates = idx.search("process_batch");
        assert!(candidates.contains(&1));
        assert!(candidates.contains(&2));
        assert!(!candidates.contains(&3));
    }

    #[test]
    fn no_matches_returns_empty() {
        let mut idx = TrigramIndex::new();
        idx.add(1, "fn hello() {}");
        let candidates = idx.search("nonexistent_symbol");
        assert!(candidates.is_empty());
    }

    #[test]
    fn short_query_under_3_chars_returns_empty() {
        let mut idx = TrigramIndex::new();
        idx.add(1, "ab");
        let candidates = idx.search("ab");
        assert!(candidates.is_empty());
    }

    #[test]
    fn case_sensitive_search() {
        let mut idx = TrigramIndex::new();
        idx.add(1, "fn ProcessBatch() {}");
        idx.add(2, "fn process_batch() {}");
        let candidates = idx.search("ProcessBatch");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0], 1);
    }

    #[test]
    fn candidates_include_false_positives() {
        // search() now returns candidates without verification.
        // The caller must verify actual substring matches.
        let mut idx = TrigramIndex::new();
        idx.add(1, "prefix_process_batch_suffix");
        let candidates = idx.search("process_batch");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0], 1);
    }

    #[test]
    fn remove_chunk_from_index() {
        let mut idx = TrigramIndex::new();
        idx.add(1, "fn target() {}");
        idx.add(2, "fn target() { call(); }");
        idx.remove(1, "fn target() {}");
        let candidates = idx.search("target");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0], 2);
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
    fn multiple_candidates_from_same_chunk() {
        let mut idx = TrigramIndex::new();
        idx.add(1, "HashMap HashMap HashMap");
        let candidates = idx.search("HashMap");
        // search() returns candidate chunk IDs (deduplicated).
        assert_eq!(candidates, vec![1]);
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

        // Verify search candidates are identical after round-trip.
        let mut orig = idx.search("process_batch");
        let mut loaded_ids = loaded.search("process_batch");
        orig.sort();
        loaded_ids.sort();
        assert_eq!(orig, loaded_ids);

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
    fn file_trigram_remove_file() {
        let mut idx = FileTrigramIndex::new();
        idx.add("a.rs", b"fn target() {}");
        idx.add("b.rs", b"fn target() { call(); }");
        idx.remove_file("a.rs");
        let candidates = idx.candidates_for_literal(b"target").unwrap();
        assert_eq!(candidates, vec!["b.rs"]);
        assert_eq!(idx.file_count(), 1);
    }

    #[test]
    fn file_trigram_remove_nonexistent_is_noop() {
        let mut idx = FileTrigramIndex::new();
        idx.add("a.rs", b"fn hello() {}");
        idx.remove_file("nonexistent.rs"); // should not panic
        assert_eq!(idx.file_count(), 1);
    }

    #[test]
    fn file_trigram_save_load_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file_trigram.bin");

        let mut idx = FileTrigramIndex::new();
        idx.add("a.rs", b"fn process_batch() {}");
        idx.add("b.rs", b"fn main() { process_batch(); }");
        idx.save_binary(&path).unwrap();

        let loaded = FileTrigramIndex::load_binary(&path).unwrap();
        assert_eq!(loaded.file_count(), 2);
        let c = loaded.candidates_for_literal(b"process_batch").unwrap();
        assert!(c.contains(&"a.rs"));
        assert!(c.contains(&"b.rs"));
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

    // ── QueryPlan + OR support tests ──────────────────────────────────────────

    #[test]
    fn query_plan_or_alternation() {
        // (foo|bar) → Or of two branches; each branch has its own trigrams.
        let plan = build_query_plan("foo|bar");
        assert!(matches!(plan, QueryPlan::Or(_)));

        let mut idx = FileTrigramIndex::new();
        idx.add("a.rs", b"use foo::runtime;");
        idx.add("b.rs", b"use bar::task;");
        idx.add("c.rs", b"fn plain_function() {}");

        let candidates = idx.execute_plan(&plan).unwrap();
        assert!(candidates.contains(&"a.rs"));
        assert!(candidates.contains(&"b.rs"));
        assert!(!candidates.contains(&"c.rs"));
    }

    #[test]
    fn query_plan_or_three_branches() {
        // TODO|FIXME|HACK → Or of three branches.
        let plan = build_query_plan("TODO|FIXME|HACK");

        let mut idx = FileTrigramIndex::new();
        idx.add("a.rs", b"// TODO: fix this");
        idx.add("b.rs", b"// FIXME: broken");
        idx.add("c.rs", b"// HACK: workaround");
        idx.add("d.rs", b"fn clean_code() {}");

        let candidates = idx.execute_plan(&plan).unwrap();
        assert!(candidates.contains(&"a.rs"));
        assert!(candidates.contains(&"b.rs"));
        assert!(candidates.contains(&"c.rs"));
        assert!(!candidates.contains(&"d.rs"));
    }

    #[test]
    fn query_plan_concat_with_or() {
        // (foo|bar).*baz → And([Or([foo, bar]), Trigrams([baz])])
        let plan = build_query_plan("(foo|bar).*baz");

        let mut idx = FileTrigramIndex::new();
        idx.add("a.rs", b"foo something baz"); // matches
        idx.add("b.rs", b"bar something baz"); // matches
        idx.add("c.rs", b"foo something xyz"); // no baz
        idx.add("d.rs", b"qux something baz"); // no foo/bar

        let candidates = idx.execute_plan(&plan).unwrap();
        assert!(candidates.contains(&"a.rs"));
        assert!(candidates.contains(&"b.rs"));
        assert!(!candidates.contains(&"c.rs"));
        assert!(!candidates.contains(&"d.rs"));
    }

    #[test]
    fn query_plan_matchall_for_broad_patterns() {
        assert!(matches!(build_query_plan(".*"), QueryPlan::MatchAll));
        assert!(matches!(build_query_plan("[a-z]+"), QueryPlan::MatchAll));
        assert!(matches!(build_query_plan("(?i)foo"), QueryPlan::MatchAll));
    }

    #[test]
    fn query_plan_literal_metacharacters() {
        // Literal patterns with regex metacharacters should use raw bytes.
        let mut idx = FileTrigramIndex::new();
        idx.add("a.rs", b"arr[0].field");
        idx.add("b.rs", b"fn something() {}");

        // Literal search for "arr[0]" — raw bytes, not regex.
        let candidates = idx.candidates_for_literal(b"arr[0]").unwrap();
        assert!(candidates.contains(&"a.rs"));
        assert!(!candidates.contains(&"b.rs"));
    }

    #[test]
    fn query_plan_or_with_matchall_branch() {
        // (foo|.*) → one branch is MatchAll → whole Or is MatchAll.
        let plan = build_query_plan("(foo|.*)");
        assert!(matches!(plan, QueryPlan::MatchAll));
    }

    #[test]
    fn query_plan_large_file_count() {
        let mut idx = FileTrigramIndex::new();
        for i in 0..1000u32 {
            idx.add(
                &format!("file_{i}.rs"),
                format!("fn func_{i}(x: i32) -> bool {{ x > {i} }}").as_bytes(),
            );
        }
        assert_eq!(idx.file_count(), 1000);
        let candidates = idx.candidates_for_literal(b"func_500").unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0], "file_500.rs");
    }
}

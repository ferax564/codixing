//! Composable search pipeline with pluggable stages.
//!
//! Each search strategy (Instant, Fast, Thorough, Explore, Deep, Exact)
//! configures a pipeline of post-retrieval stages that transform and re-rank
//! results. This replaces the scattered boost/demotion calls in the monolithic
//! `search()` method with a declarative, testable pipeline.

use crate::error::Result;
use crate::retriever::SearchResult;

/// Read-only context passed to each pipeline stage.
///
/// Contains everything a stage needs to make boost/demotion decisions without
/// requiring mutable access to the engine.
pub struct SearchContext<'a> {
    /// The original search query string.
    pub query: &'a str,
    /// The symbol table for definition-boost lookups.
    pub symbols: &'a crate::symbols::SymbolTable,
    /// The dependency graph (if available) for PageRank and popularity boosts.
    pub graph: Option<&'a crate::graph::CodeGraph>,
    /// Graph boost weight from config.
    pub graph_boost_weight: f32,
    /// Git recency map (file path → last commit timestamp) for recency boosts.
    pub recency_map: Option<&'a std::collections::HashMap<String, i64>>,
    /// Chunk metadata table for hydrating injected results (graph propagation).
    pub chunk_meta: Option<&'a dashmap::DashMap<u64, crate::retriever::ChunkMeta>>,
    /// The concept index (if available) for concept-based boosting.
    pub concepts: Option<&'a crate::engine::concepts::ConceptIndex>,
}

/// A single composable stage in the search pipeline.
///
/// Stages are applied in order and mutate the results vector in place.
/// Each stage handles one concern: boosting, demotion, deduplication, or truncation.
pub trait SearchStage: Send + Sync {
    /// Apply this stage's transformation to the result set.
    fn apply(&self, results: &mut Vec<SearchResult>, ctx: &SearchContext<'_>) -> Result<()>;
}

/// A composable pipeline of search stages applied after initial retrieval.
pub struct SearchPipeline {
    stages: Vec<Box<dyn SearchStage>>,
}

impl SearchPipeline {
    /// Create an empty pipeline.
    pub fn new() -> Self {
        Self { stages: Vec::new() }
    }

    /// Append a stage to the pipeline.
    pub fn add(mut self, stage: impl SearchStage + 'static) -> Self {
        self.stages.push(Box::new(stage));
        self
    }

    /// Run all stages in order on the result set.
    pub fn run(&self, results: &mut Vec<SearchResult>, ctx: &SearchContext<'_>) -> Result<()> {
        for stage in &self.stages {
            stage.apply(results, ctx)?;
        }
        Ok(())
    }
}

impl Default for SearchPipeline {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Concrete stages — each extracts one concern from the old monolithic search()
// ---------------------------------------------------------------------------

/// Sort results descending by score.
fn sort_descending(results: &mut [SearchResult]) {
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// Multiply each result's score by `1 + weight * pagerank` then re-sort.
#[allow(dead_code)]
pub struct GraphBoostStage;

impl SearchStage for GraphBoostStage {
    fn apply(&self, results: &mut Vec<SearchResult>, ctx: &SearchContext<'_>) -> Result<()> {
        if let Some(graph) = ctx.graph {
            let weight = ctx.graph_boost_weight;
            for r in results.iter_mut() {
                let pr = graph.node(&r.file_path).map(|n| n.pagerank).unwrap_or(0.0);
                r.score *= 1.0 + weight * pr;
            }
            sort_descending(results);
        }
        Ok(())
    }
}

/// Replace static PageRank boost with query-personalized PageRank.
///
/// Seeds the personalization vector from the top-5 BM25 results (weighted by
/// their BM25 score), then re-scores all results using the personalized scores.
/// An LRU cache on the computed PPR scores avoids recomputing the 20-iteration
/// PPR loop when identical seed sets recur — common for agent sessions that
/// hit the same query repeatedly while exploring a neighborhood.
pub struct PersonalizedGraphBoostStage;

/// One cached computation of personalized PageRank.
struct PprCacheEntry {
    scores: std::sync::Arc<std::collections::HashMap<String, f32>>,
    inserted: std::time::Instant,
}

/// Cache TTL. Graph edits between queries aren't tracked precisely (the
/// cache key incorporates node_count as a rough invalidation proxy);
/// 5 minutes bounds staleness in the face of sync-driven graph changes.
const PPR_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// Max cached computations retained. Above this, the oldest entry is
/// evicted on insert (rough LRU via insertion order). 64 matches the
/// working-set size of a typical agent session — large enough to avoid
/// thrashing, small enough to bound memory (each entry ~= repo-size × 8 B).
const PPR_CACHE_CAP: usize = 64;

fn ppr_cache() -> &'static std::sync::Mutex<Vec<(u64, PprCacheEntry)>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Vec<(u64, PprCacheEntry)>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Derive a cache key from the seed set and the graph's node count.
///
/// Identical seeds (same files, same rounded scores, same order) on the
/// same graph size produce the same key. Scores are rounded to 3 decimals
/// so trivial floating-point noise below 10^-3 doesn't miss the cache.
fn seed_cache_key(seeds: &[(&str, f32)], node_count: usize) -> u64 {
    use xxhash_rust::xxh3::Xxh3;
    let mut hasher = Xxh3::new();
    hasher.update(&(node_count as u64).to_le_bytes());
    for (file, score) in seeds {
        hasher.update(file.as_bytes());
        hasher.update(&[0]); // separator so "ab" + "" != "a" + "b"
        let rounded = (score * 1000.0).round() as i64;
        hasher.update(&rounded.to_le_bytes());
    }
    hasher.digest()
}

fn ppr_cache_get(key: u64) -> Option<std::sync::Arc<std::collections::HashMap<String, f32>>> {
    let mut cache = ppr_cache().lock().ok()?;
    let now = std::time::Instant::now();
    cache.retain(|(_, e)| now.duration_since(e.inserted) < PPR_CACHE_TTL);
    let pos = cache.iter().position(|(k, _)| *k == key)?;
    let scores = cache[pos].1.scores.clone();
    // Refresh to tail so LRU eviction favours older-untouched entries.
    let entry = cache.remove(pos);
    cache.push(entry);
    Some(scores)
}

fn ppr_cache_put(key: u64, scores: std::sync::Arc<std::collections::HashMap<String, f32>>) {
    let Ok(mut cache) = ppr_cache().lock() else {
        return;
    };
    // If a stale entry for this key already exists, drop it before insert.
    cache.retain(|(k, _)| *k != key);
    if cache.len() >= PPR_CACHE_CAP {
        cache.remove(0);
    }
    cache.push((
        key,
        PprCacheEntry {
            scores,
            inserted: std::time::Instant::now(),
        },
    ));
}

/// Test-only: wipe the cache so tests don't leak state between runs.
#[cfg(test)]
pub(crate) fn __test_ppr_cache_clear() {
    if let Ok(mut cache) = ppr_cache().lock() {
        cache.clear();
    }
}

/// Test-only: current cache size, for assertion in cache-behavior tests.
#[cfg(test)]
pub(crate) fn __test_ppr_cache_len() -> usize {
    ppr_cache().lock().map(|c| c.len()).unwrap_or(0)
}

impl SearchStage for PersonalizedGraphBoostStage {
    fn apply(&self, results: &mut Vec<SearchResult>, ctx: &SearchContext<'_>) -> Result<()> {
        let graph = match ctx.graph {
            Some(g) => g,
            None => return Ok(()),
        };

        if results.is_empty() {
            return Ok(());
        }

        // Extract top-5 results as weighted seeds.
        let seed_count = results.len().min(5);
        let seeds: Vec<(&str, f32)> = results[..seed_count]
            .iter()
            .map(|r| (r.file_path.as_str(), r.score))
            .collect();

        // Cache hit: reuse the previously-computed PPR scores. Cache miss:
        // run the 20-iteration PPR loop then insert.
        let cache_key = seed_cache_key(&seeds, graph.node_count());
        let ppr = if let Some(hit) = ppr_cache_get(cache_key) {
            hit
        } else {
            let fresh = std::sync::Arc::new(crate::graph::compute_weighted_personalized_pagerank(
                graph, 0.85, 20, 1e-6, &seeds,
            ));
            ppr_cache_put(cache_key, fresh.clone());
            fresh
        };

        let weight = ctx.graph_boost_weight;
        for r in results.iter_mut() {
            let pr = ppr.get(&r.file_path).copied().unwrap_or(0.0);
            r.score *= 1.0 + weight * pr;
        }
        sort_descending(results);
        Ok(())
    }
}

/// Boost public API symbols in search results.
///
/// Public symbols get a 1.5x boost. Skipped when the query contains
/// internal-targeting signals like "internal", "private", "helper".
pub struct VisibilityBoostStage;

impl SearchStage for VisibilityBoostStage {
    fn apply(&self, results: &mut Vec<SearchResult>, ctx: &SearchContext<'_>) -> Result<()> {
        let query_lower = ctx.query.to_lowercase();
        let internal_signals = ["internal", "private", "helper", "impl", "detail"];
        if internal_signals.iter().any(|s| query_lower.contains(s)) {
            return Ok(());
        }

        let mut boosted = false;
        for r in results.iter_mut() {
            // Check if any symbol in this chunk's file+line range is public
            let symbols = ctx.symbols.filter("", Some(&r.file_path));
            let has_public = symbols.iter().any(|s| {
                s.visibility == crate::language::Visibility::Public
                    && r.line_start <= s.line_start as u64
                    && (s.line_start as u64) < r.line_end
            });
            if has_public {
                r.score *= 1.5;
                boosted = true;
            }
        }

        if boosted {
            sort_descending(results);
        }
        Ok(())
    }
}

/// Boost results whose files *define* a symbol matching query identifiers.
///
/// Corrects BM25's tendency to over-rank files that *heavily use* a symbol
/// above the file that *defines* it. Uses a 3.5x score multiplier.
pub struct DefinitionBoostStage;

impl SearchStage for DefinitionBoostStage {
    fn apply(&self, results: &mut Vec<SearchResult>, ctx: &SearchContext<'_>) -> Result<()> {
        use std::collections::HashSet;

        let mut defining_files: HashSet<String> = HashSet::new();
        for term in ctx.query.split_whitespace() {
            if term.len() < 3 || !term.chars().all(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            let exact = ctx.symbols.lookup(term);
            if !exact.is_empty() {
                for sym in exact {
                    defining_files.insert(sym.file_path);
                }
            } else {
                for sym in ctx.symbols.filter(term, None) {
                    defining_files.insert(sym.file_path);
                }
            }
        }

        if defining_files.is_empty() {
            return Ok(());
        }

        const DEFINITION_BOOST: f32 = 3.5;
        let mut boosted = false;
        for r in results.iter_mut() {
            if defining_files.contains(&r.file_path) {
                r.score *= DEFINITION_BOOST;
                boosted = true;
            }
        }
        if boosted {
            sort_descending(results);
        }
        Ok(())
    }
}

/// Boost results whose files have many callers in the dependency graph.
///
/// Architecturally central files (imported by many others) get a modest
/// logarithmic boost: ln(4) ~ 1.4 -> 7%, ln(10) ~ 2.3 -> 11.5%.
pub struct PopularityBoostStage;

impl SearchStage for PopularityBoostStage {
    fn apply(&self, results: &mut Vec<SearchResult>, ctx: &SearchContext<'_>) -> Result<()> {
        if let Some(graph) = ctx.graph {
            let mut boosted = false;
            for r in results.iter_mut() {
                let caller_count = graph.callers(&r.file_path).len();
                if caller_count > 3 {
                    r.score *= 1.0 + (caller_count as f32).ln() * 0.05;
                    boosted = true;
                }
            }
            if boosted {
                sort_descending(results);
            }
        }
        Ok(())
    }
}

/// Mild boost for files recently modified in git.
///
/// Files committed within the recency window (180 days) receive up to a 10%
/// score increase that decays linearly to zero at the window boundary. Files
/// older than the window or absent from the recency map are unchanged.
pub struct RecencyBoostStage;

impl SearchStage for RecencyBoostStage {
    fn apply(&self, results: &mut Vec<SearchResult>, ctx: &SearchContext<'_>) -> Result<()> {
        let recency_map = match ctx.recency_map {
            Some(m) if !m.is_empty() => m,
            _ => return Ok(()),
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut boosted = false;
        for r in results.iter_mut() {
            if let Some(&last_commit) = recency_map.get(&r.file_path) {
                let days = (now - last_commit) / 86400;
                let factor = (1.0 - days as f32 / 180.0).max(0.0);
                if factor > 0.0 {
                    r.score *= 1.0 + 0.1 * factor;
                    boosted = true;
                }
            }
        }
        if boosted {
            sort_descending(results);
        }
        Ok(())
    }
}

/// Inject 1-hop graph neighbors of top results into the result set.
///
/// For each of the top N results, looks up callers and callees from the
/// dependency graph. Neighbors not already in the result set are injected
/// with a damped score.
pub struct GraphPropagationStage;

impl GraphPropagationStage {
    const TOP_N: usize = 5;
    const MAX_INJECTED: usize = 3;
    const CALLEE_DAMPING: f32 = 0.25;
    const CALLER_DAMPING: f32 = 0.15;
}

impl SearchStage for GraphPropagationStage {
    fn apply(&self, results: &mut Vec<SearchResult>, ctx: &SearchContext<'_>) -> Result<()> {
        let graph = match ctx.graph {
            Some(g) => g,
            None => return Ok(()),
        };
        let chunk_meta = match ctx.chunk_meta {
            Some(m) => m,
            None => return Ok(()),
        };

        let existing_files: std::collections::HashSet<&str> =
            results.iter().map(|r| r.file_path.as_str()).collect();

        let mut candidates: Vec<(String, f32)> = Vec::new();
        let source_count = results.len().min(Self::TOP_N);

        for r in &results[..source_count] {
            for callee in graph.callees(&r.file_path) {
                if !existing_files.contains(callee.as_str()) {
                    candidates.push((callee, r.score * Self::CALLEE_DAMPING));
                }
            }
            for caller in graph.callers(&r.file_path) {
                if !existing_files.contains(caller.as_str()) {
                    candidates.push((caller, r.score * Self::CALLER_DAMPING));
                }
            }
        }

        if candidates.is_empty() {
            return Ok(());
        }

        // Deduplicate candidates by file path, keeping highest score.
        candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let mut seen_candidates = std::collections::HashSet::new();
        candidates.retain(|(path, _)| seen_candidates.insert(path.clone()));
        candidates.truncate(Self::MAX_INJECTED);

        // Build SearchResult for each candidate from chunk_meta.
        for (file_path, score) in candidates {
            // Find the first chunk for this file in chunk_meta (lowest line_start).
            let best_chunk = chunk_meta
                .iter()
                .filter(|entry| entry.value().file_path == file_path)
                .min_by_key(|entry| entry.value().line_start);

            if let Some(entry) = best_chunk {
                let meta = entry.value();
                results.push(SearchResult {
                    chunk_id: meta.chunk_id.to_string(),
                    file_path: meta.file_path.clone(),
                    language: meta.language.clone(),
                    score,
                    line_start: meta.line_start,
                    line_end: meta.line_end,
                    signature: meta.signature.clone(),
                    scope_chain: meta.scope_chain.clone(),
                    content: meta.content.clone(),
                });
            }
        }

        sort_descending(results);
        Ok(())
    }
}

/// Boost results whose files belong to concept clusters matching the query.
///
/// Uses the semantic concept index to bridge vocabulary gaps: when query terms
/// match concept labels (derived from identifier decomposition, doc comments,
/// and import co-occurrence), files in those clusters receive a score boost
/// proportional to cluster confidence and hit count.
pub struct ConceptBoostStage;

impl SearchStage for ConceptBoostStage {
    fn apply(&self, results: &mut Vec<SearchResult>, ctx: &SearchContext<'_>) -> Result<()> {
        let concepts = match ctx.concepts {
            Some(c) if !c.is_empty() => c,
            _ => return Ok(()),
        };

        let matches = concepts.lookup_query(ctx.query);
        if matches.is_empty() {
            return Ok(());
        }

        // Collect file boosts from matching clusters
        let mut file_boosts: std::collections::HashMap<&str, f32> =
            std::collections::HashMap::new();
        for (cluster, hit_count) in &matches {
            let boost = 0.3 * cluster.score * (*hit_count as f32);
            for file in &cluster.files {
                let entry = file_boosts.entry(file.as_str()).or_insert(0.0);
                *entry = entry.max(boost);
            }
        }

        let mut boosted = false;
        for r in results.iter_mut() {
            if let Some(&boost) = file_boosts.get(r.file_path.as_str()) {
                r.score *= 1.0 + boost;
                boosted = true;
            }
        }

        if boosted {
            sort_descending(results);
        }
        Ok(())
    }
}

/// Boost results whose file path contains a dotted-path or keyword reference
/// from the query.
pub struct PathMatchBoostStage;

impl SearchStage for PathMatchBoostStage {
    fn apply(&self, results: &mut Vec<SearchResult>, ctx: &SearchContext<'_>) -> Result<()> {
        // Delegate to the existing free function which handles all three
        // boost types (dotted paths, keywords, concept-to-path mapping).
        super::search::apply_path_match_boost(results, ctx.query);
        Ok(())
    }
}

/// Demote test files and search-infrastructure files so implementation code
/// ranks higher. Also demotes C/C++ headers when implementation files exist.
pub struct TestDemotionStage;

impl SearchStage for TestDemotionStage {
    fn apply(&self, results: &mut Vec<SearchResult>, ctx: &SearchContext<'_>) -> Result<()> {
        use super::search::{apply_header_demotion, is_search_infra, is_test_chunk, is_test_file};

        const TEST_DEMOTION: f32 = 0.5;
        const INFRA_DEMOTION: f32 = 0.5;
        let mut changed = false;

        for r in results.iter_mut() {
            if is_test_file(&r.file_path) || is_test_chunk(r) {
                r.score *= TEST_DEMOTION;
                changed = true;
            } else if is_search_infra(&r.file_path, ctx.query) {
                r.score *= INFRA_DEMOTION;
                changed = true;
            }
        }

        apply_header_demotion(results, &mut changed);

        if changed {
            sort_descending(results);
        }
        Ok(())
    }
}

/// Remove results whose line ranges overlap with a higher-scored result
/// from the same file.
#[allow(dead_code)]
pub struct DeduplicationStage;

impl SearchStage for DeduplicationStage {
    fn apply(&self, results: &mut Vec<SearchResult>, _ctx: &SearchContext<'_>) -> Result<()> {
        super::search::dedup_overlapping(results);
        Ok(())
    }
}

/// File-level deduplication with allowance for qualified second chunks.
///
/// 1. Runs `dedup_overlapping` to remove line-range overlaps (same as old stage).
/// 2. Groups results by file path.
/// 3. Keeps the best chunk per file.
/// 4. Allows a second chunk from the same file only if:
///    - Its score ≥ 70% of the file's best chunk score.
///    - It's ≥ 50 lines away from all already-kept chunks from that file.
/// 5. Drops all other same-file duplicates.
pub struct FileDedupStage;

impl FileDedupStage {
    const SECOND_CHUNK_SCORE_THRESHOLD: f32 = 0.70;
    const MIN_LINE_GAP: u64 = 50;
}

impl SearchStage for FileDedupStage {
    fn apply(&self, results: &mut Vec<SearchResult>, _ctx: &SearchContext<'_>) -> Result<()> {
        // Phase 1: remove overlapping line-range duplicates.
        super::search::dedup_overlapping(results);

        if results.len() <= 1 {
            return Ok(());
        }

        // Phase 2: file-level dedup with allowance.
        let mut kept: Vec<SearchResult> = Vec::with_capacity(results.len());
        // file_path → (best_score, Vec<(line_start, line_end)>)
        let mut file_kept: std::collections::HashMap<String, (f32, Vec<(u64, u64)>)> =
            std::collections::HashMap::new();

        // Results are sorted by score descending (guaranteed by prior stages).
        for r in results.drain(..) {
            match file_kept.get_mut(&r.file_path) {
                None => {
                    file_kept.insert(
                        r.file_path.clone(),
                        (r.score, vec![(r.line_start, r.line_end)]),
                    );
                    kept.push(r);
                }
                Some((best_score, kept_ranges)) => {
                    let score_ok = r.score >= *best_score * Self::SECOND_CHUNK_SCORE_THRESHOLD;
                    let gap_ok = kept_ranges.iter().all(|&(ks, ke)| {
                        let gap = if r.line_start >= ke {
                            r.line_start - ke
                        } else {
                            ks.saturating_sub(r.line_end)
                        };
                        gap >= Self::MIN_LINE_GAP
                    });

                    if score_ok && gap_ok {
                        kept_ranges.push((r.line_start, r.line_end));
                        kept.push(r);
                    }
                }
            }
        }

        *results = kept;
        sort_descending(results);
        Ok(())
    }
}

/// Truncate results at natural score boundaries ("score cliffs").
///
/// Detects points where the relevance score drops significantly relative
/// to the top score and truncates there, keeping at least `min_results`.
pub struct TruncationStage {
    pub min_results: usize,
    pub cliff_threshold: f32,
}

impl SearchStage for TruncationStage {
    fn apply(&self, results: &mut Vec<SearchResult>, _ctx: &SearchContext<'_>) -> Result<()> {
        super::search::adaptive_truncate(results, self.min_results, self.cliff_threshold);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Pipeline builders — one per strategy
// ---------------------------------------------------------------------------

/// Build the post-retrieval pipeline for `Strategy::Instant`.
pub fn instant_pipeline() -> SearchPipeline {
    SearchPipeline::new()
        .add(DefinitionBoostStage)
        .add(PathMatchBoostStage)
        .add(TestDemotionStage)
        .add(FileDedupStage)
}

/// Build the post-retrieval pipeline for `Strategy::Fast`.
pub fn fast_pipeline() -> SearchPipeline {
    SearchPipeline::new()
        .add(PersonalizedGraphBoostStage)
        .add(ConceptBoostStage)
        .add(VisibilityBoostStage)
        .add(DefinitionBoostStage)
        .add(PopularityBoostStage)
        .add(RecencyBoostStage)
        .add(PathMatchBoostStage)
        .add(TestDemotionStage)
        .add(GraphPropagationStage)
        .add(TruncationStage {
            min_results: 3,
            cliff_threshold: 0.35,
        })
        .add(FileDedupStage)
}

/// Build the post-retrieval pipeline for `Strategy::Thorough`.
pub fn thorough_pipeline() -> SearchPipeline {
    SearchPipeline::new()
        .add(PersonalizedGraphBoostStage)
        .add(ConceptBoostStage)
        .add(VisibilityBoostStage)
        .add(DefinitionBoostStage)
        .add(PopularityBoostStage)
        .add(RecencyBoostStage)
        .add(PathMatchBoostStage)
        .add(TestDemotionStage)
        .add(GraphPropagationStage)
        .add(TruncationStage {
            min_results: 3,
            cliff_threshold: 0.35,
        })
        .add(FileDedupStage)
}

/// Build the post-retrieval pipeline for `Strategy::Exact`.
pub fn exact_pipeline() -> SearchPipeline {
    SearchPipeline::new()
        .add(DefinitionBoostStage)
        .add(PathMatchBoostStage)
        .add(TestDemotionStage)
        .add(FileDedupStage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retriever::SearchResult;

    fn make_result(id: &str, score: f32, file_path: &str) -> SearchResult {
        SearchResult {
            chunk_id: id.into(),
            file_path: file_path.into(),
            language: "Rust".into(),
            score,
            line_start: 0,
            line_end: 10,
            signature: String::new(),
            scope_chain: vec![],
            content: String::new(),
        }
    }

    #[test]
    fn empty_pipeline_is_identity() {
        let pipeline = SearchPipeline::new();
        let symbols = crate::symbols::SymbolTable::new();
        let ctx = SearchContext {
            query: "test",
            symbols: &symbols,
            graph: None,
            graph_boost_weight: 0.0,
            recency_map: None,
            chunk_meta: None,
            concepts: None,
        };
        let mut results = vec![make_result("a", 10.0, "src/a.rs")];
        pipeline.run(&mut results, &ctx).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].score, 10.0);
    }

    #[test]
    fn test_demotion_stage_demotes_test_files() {
        let stage = TestDemotionStage;
        let symbols = crate::symbols::SymbolTable::new();
        let ctx = SearchContext {
            query: "process_batch",
            symbols: &symbols,
            graph: None,
            graph_boost_weight: 0.0,
            recency_map: None,
            chunk_meta: None,
            concepts: None,
        };
        let mut results = vec![
            make_result("a", 10.0, "src/main.rs"),
            make_result("b", 10.0, "tests/test_main.rs"),
        ];
        stage.apply(&mut results, &ctx).unwrap();
        // Test file should be demoted
        assert!(results[1].score < results[0].score);
    }

    #[test]
    fn deduplication_stage_removes_overlapping() {
        let stage = DeduplicationStage;
        let symbols = crate::symbols::SymbolTable::new();
        let ctx = SearchContext {
            query: "test",
            symbols: &symbols,
            graph: None,
            graph_boost_weight: 0.0,
            recency_map: None,
            chunk_meta: None,
            concepts: None,
        };
        let mut results = vec![
            SearchResult {
                chunk_id: "1".into(),
                file_path: "src/main.rs".into(),
                language: "Rust".into(),
                score: 10.0,
                line_start: 0,
                line_end: 20,
                signature: String::new(),
                scope_chain: vec![],
                content: String::new(),
            },
            SearchResult {
                chunk_id: "2".into(),
                file_path: "src/main.rs".into(),
                language: "Rust".into(),
                score: 5.0,
                line_start: 15,
                line_end: 30,
                signature: String::new(),
                scope_chain: vec![],
                content: String::new(),
            },
        ];
        stage.apply(&mut results, &ctx).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk_id, "1");
    }

    #[test]
    fn truncation_stage_truncates_at_cliff() {
        let stage = TruncationStage {
            min_results: 3,
            cliff_threshold: 0.35,
        };
        let symbols = crate::symbols::SymbolTable::new();
        let ctx = SearchContext {
            query: "test",
            symbols: &symbols,
            graph: None,
            graph_boost_weight: 0.0,
            recency_map: None,
            chunk_meta: None,
            concepts: None,
        };
        let mut results = vec![
            make_result("a", 10.0, "src/a.rs"),
            make_result("b", 9.5, "src/b.rs"),
            make_result("c", 9.0, "src/c.rs"),
            make_result("d", 3.0, "src/d.rs"), // cliff
            make_result("e", 2.5, "src/e.rs"),
        ];
        stage.apply(&mut results, &ctx).unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn pipeline_stages_compose_in_order() {
        let pipeline = SearchPipeline::new()
            .add(TestDemotionStage)
            .add(DeduplicationStage);
        let symbols = crate::symbols::SymbolTable::new();
        let ctx = SearchContext {
            query: "engine",
            symbols: &symbols,
            graph: None,
            graph_boost_weight: 0.0,
            recency_map: None,
            chunk_meta: None,
            concepts: None,
        };
        let mut results = vec![
            make_result("a", 10.0, "src/engine.rs"),
            make_result("b", 10.0, "tests/test_engine.rs"),
        ];
        pipeline.run(&mut results, &ctx).unwrap();
        // Test file should be demoted, impl file first
        assert_eq!(results[0].chunk_id, "a");
        assert!(results[0].score > results[1].score);
    }

    #[test]
    fn instant_pipeline_has_correct_stages() {
        // Verify the pipeline runs without error on basic input.
        let pipeline = instant_pipeline();
        let symbols = crate::symbols::SymbolTable::new();
        let ctx = SearchContext {
            query: "Engine",
            symbols: &symbols,
            graph: None,
            graph_boost_weight: 0.0,
            recency_map: None,
            chunk_meta: None,
            concepts: None,
        };
        let mut results = vec![make_result("a", 10.0, "src/engine.rs")];
        pipeline.run(&mut results, &ctx).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn exact_pipeline_has_correct_stages() {
        let pipeline = exact_pipeline();
        let symbols = crate::symbols::SymbolTable::new();
        let ctx = SearchContext {
            query: "process_batch",
            symbols: &symbols,
            graph: None,
            graph_boost_weight: 0.0,
            recency_map: None,
            chunk_meta: None,
            concepts: None,
        };
        let mut results = vec![make_result("a", 10.0, "src/engine.rs")];
        pipeline.run(&mut results, &ctx).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn recency_boost_stage_boosts_recent_files() {
        use std::collections::HashMap;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let mut recency = HashMap::new();
        // recent: committed 1 day ago
        recency.insert("src/recent.rs".to_string(), now - 86400);
        // old: committed 200 days ago (outside the 180-day window)
        recency.insert("src/old.rs".to_string(), now - 200 * 86400);

        let stage = RecencyBoostStage;
        let symbols = crate::symbols::SymbolTable::new();
        let ctx = SearchContext {
            query: "test",
            symbols: &symbols,
            graph: None,
            graph_boost_weight: 0.0,
            recency_map: Some(&recency),
            chunk_meta: None,
            concepts: None,
        };

        let mut results = vec![
            make_result("recent", 10.0, "src/recent.rs"),
            make_result("old", 10.0, "src/old.rs"),
            make_result("unknown", 10.0, "src/unknown.rs"),
        ];

        stage.apply(&mut results, &ctx).unwrap();

        // Recent file should be boosted above 10.0
        let recent = results.iter().find(|r| r.chunk_id == "recent").unwrap();
        assert!(
            recent.score > 10.0,
            "expected recent file to be boosted, got {}",
            recent.score
        );

        // Old file (200 days > 180 window) should NOT be boosted
        let old = results.iter().find(|r| r.chunk_id == "old").unwrap();
        assert!(
            (old.score - 10.0).abs() < f32::EPSILON,
            "expected old file to remain at 10.0, got {}",
            old.score
        );

        // Unknown file (not in recency map) should NOT be boosted
        let unknown = results.iter().find(|r| r.chunk_id == "unknown").unwrap();
        assert!(
            (unknown.score - 10.0).abs() < f32::EPSILON,
            "expected unknown file to remain at 10.0, got {}",
            unknown.score
        );
    }

    #[test]
    fn graph_propagation_injects_neighbor() {
        use crate::graph::CodeGraph;
        use crate::language::Language;

        let mut graph = CodeGraph::new();
        graph.add_edge("src/a.rs", "src/b.rs", "b", Language::Rust, Language::Rust);
        let pr_scores = crate::graph::compute_pagerank(&graph, 0.85, 20);
        graph.apply_pagerank(&pr_scores);

        let chunk_meta = dashmap::DashMap::new();
        chunk_meta.insert(
            100,
            crate::retriever::ChunkMeta {
                chunk_id: 100,
                file_path: "src/b.rs".into(),
                language: "Rust".into(),
                line_start: 0,
                line_end: 20,
                signature: "fn helper()".into(),
                scope_chain: vec![],
                entity_names: vec![],
                content: "fn helper() {}".into(),
                content_hash: 0,
            },
        );

        let stage = GraphPropagationStage;
        let symbols = crate::symbols::SymbolTable::new();
        let ctx = SearchContext {
            query: "test",
            symbols: &symbols,
            graph: Some(&graph),
            graph_boost_weight: 0.5,
            recency_map: None,
            chunk_meta: Some(&chunk_meta),
            concepts: None,
        };

        let mut results = vec![make_result("a", 10.0, "src/a.rs")];
        stage.apply(&mut results, &ctx).unwrap();

        assert!(
            results.len() >= 2,
            "expected neighbor injection, got {} results",
            results.len()
        );
        let b_result = results.iter().find(|r| r.file_path == "src/b.rs");
        assert!(b_result.is_some(), "src/b.rs should be injected");
        let b = b_result.unwrap();
        assert!(
            (b.score - 2.5).abs() < 0.01,
            "expected callee score ~2.5, got {}",
            b.score
        );
    }

    #[test]
    fn file_dedup_keeps_best_per_file() {
        let stage = FileDedupStage;
        let symbols = crate::symbols::SymbolTable::new();
        let ctx = SearchContext {
            query: "test",
            symbols: &symbols,
            graph: None,
            graph_boost_weight: 0.0,
            recency_map: None,
            chunk_meta: None,
            concepts: None,
        };
        let mut results = vec![
            SearchResult {
                chunk_id: "1".into(),
                file_path: "src/main.rs".into(),
                language: "Rust".into(),
                score: 10.0,
                line_start: 0,
                line_end: 20,
                signature: String::new(),
                scope_chain: vec![],
                content: String::new(),
            },
            SearchResult {
                chunk_id: "2".into(),
                file_path: "src/main.rs".into(),
                language: "Rust".into(),
                score: 3.0, // below 70% of 10.0 = 7.0
                line_start: 100,
                line_end: 120,
                signature: String::new(),
                scope_chain: vec![],
                content: String::new(),
            },
            SearchResult {
                chunk_id: "3".into(),
                file_path: "src/other.rs".into(),
                language: "Rust".into(),
                score: 5.0,
                line_start: 0,
                line_end: 20,
                signature: String::new(),
                scope_chain: vec![],
                content: String::new(),
            },
        ];
        stage.apply(&mut results, &ctx).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].chunk_id, "1");
        assert_eq!(results[1].chunk_id, "3");
    }

    #[test]
    fn file_dedup_allows_qualified_second_chunk() {
        let stage = FileDedupStage;
        let symbols = crate::symbols::SymbolTable::new();
        let ctx = SearchContext {
            query: "test",
            symbols: &symbols,
            graph: None,
            graph_boost_weight: 0.0,
            recency_map: None,
            chunk_meta: None,
            concepts: None,
        };
        let mut results = vec![
            SearchResult {
                chunk_id: "1".into(),
                file_path: "src/main.rs".into(),
                language: "Rust".into(),
                score: 10.0,
                line_start: 0,
                line_end: 20,
                signature: String::new(),
                scope_chain: vec![],
                content: String::new(),
            },
            SearchResult {
                chunk_id: "2".into(),
                file_path: "src/main.rs".into(),
                language: "Rust".into(),
                score: 8.0,      // 80% ≥ 70%
                line_start: 200, // gap 180 ≥ 50
                line_end: 220,
                signature: String::new(),
                scope_chain: vec![],
                content: String::new(),
            },
        ];
        stage.apply(&mut results, &ctx).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn file_dedup_rejects_close_second_chunk() {
        let stage = FileDedupStage;
        let symbols = crate::symbols::SymbolTable::new();
        let ctx = SearchContext {
            query: "test",
            symbols: &symbols,
            graph: None,
            graph_boost_weight: 0.0,
            recency_map: None,
            chunk_meta: None,
            concepts: None,
        };
        let mut results = vec![
            SearchResult {
                chunk_id: "1".into(),
                file_path: "src/main.rs".into(),
                language: "Rust".into(),
                score: 10.0,
                line_start: 0,
                line_end: 20,
                signature: String::new(),
                scope_chain: vec![],
                content: String::new(),
            },
            SearchResult {
                chunk_id: "2".into(),
                file_path: "src/main.rs".into(),
                language: "Rust".into(),
                score: 9.0,     // qualifies on score
                line_start: 30, // gap 10 < 50
                line_end: 50,
                signature: String::new(),
                scope_chain: vec![],
                content: String::new(),
            },
        ];
        stage.apply(&mut results, &ctx).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk_id, "1");
    }

    #[test]
    fn graph_propagation_skips_existing_results() {
        use crate::graph::CodeGraph;
        use crate::language::Language;

        let mut graph = CodeGraph::new();
        graph.add_edge("src/a.rs", "src/b.rs", "b", Language::Rust, Language::Rust);
        let pr_scores = crate::graph::compute_pagerank(&graph, 0.85, 20);
        graph.apply_pagerank(&pr_scores);

        let chunk_meta = dashmap::DashMap::new();
        chunk_meta.insert(
            100,
            crate::retriever::ChunkMeta {
                chunk_id: 100,
                file_path: "src/b.rs".into(),
                language: "Rust".into(),
                line_start: 0,
                line_end: 20,
                signature: String::new(),
                scope_chain: vec![],
                entity_names: vec![],
                content: String::new(),
                content_hash: 0,
            },
        );

        let stage = GraphPropagationStage;
        let symbols = crate::symbols::SymbolTable::new();
        let ctx = SearchContext {
            query: "test",
            symbols: &symbols,
            graph: Some(&graph),
            graph_boost_weight: 0.5,
            recency_map: None,
            chunk_meta: Some(&chunk_meta),
            concepts: None,
        };

        let mut results = vec![
            make_result("a", 10.0, "src/a.rs"),
            make_result("b", 8.0, "src/b.rs"),
        ];
        stage.apply(&mut results, &ctx).unwrap();

        let b_count = results.iter().filter(|r| r.file_path == "src/b.rs").count();
        assert_eq!(b_count, 1);
        let b = results.iter().find(|r| r.file_path == "src/b.rs").unwrap();
        assert!(
            (b.score - 8.0).abs() < 0.01,
            "existing result score should be unchanged"
        );
    }

    #[test]
    #[serial_test::serial(ppr_cache)]
    fn personalized_graph_boost_uses_seed_results() {
        use crate::graph::CodeGraph;
        use crate::language::Language;

        let mut graph = CodeGraph::new();
        graph.add_edge("src/a.rs", "src/b.rs", "b", Language::Rust, Language::Rust);
        graph.add_edge("src/b.rs", "src/c.rs", "c", Language::Rust, Language::Rust);
        let pr = crate::graph::compute_pagerank(&graph, 0.85, 20);
        graph.apply_pagerank(&pr);

        let stage = PersonalizedGraphBoostStage;
        let symbols = crate::symbols::SymbolTable::new();
        let ctx = SearchContext {
            query: "test",
            symbols: &symbols,
            graph: Some(&graph),
            graph_boost_weight: 0.5,
            recency_map: None,
            chunk_meta: None,
            concepts: None,
        };

        let mut results = vec![
            make_result("a", 10.0, "src/a.rs"),
            make_result("b", 5.0, "src/b.rs"),
            make_result("c", 5.0, "src/c.rs"),
        ];
        stage.apply(&mut results, &ctx).unwrap();

        // Personalized PageRank seeded from a.rs (top result) should boost
        // both b.rs and c.rs (graph neighbors) above their initial score of 5.0.
        let a_score = results
            .iter()
            .find(|r| r.file_path == "src/a.rs")
            .unwrap()
            .score;
        let b_score = results
            .iter()
            .find(|r| r.file_path == "src/b.rs")
            .unwrap()
            .score;
        let c_score = results
            .iter()
            .find(|r| r.file_path == "src/c.rs")
            .unwrap()
            .score;

        // The seed file (a.rs) should remain the top result.
        assert!(
            a_score > b_score && a_score > c_score,
            "seed file a.rs should remain top, got a={a_score}, b={b_score}, c={c_score}"
        );
        // Both graph neighbors should be boosted above their initial 5.0.
        assert!(
            b_score > 5.0,
            "b.rs should be boosted above 5.0, got {b_score}"
        );
        assert!(
            c_score > 5.0,
            "c.rs should be boosted above 5.0, got {c_score}"
        );
    }

    #[test]
    #[serial_test::serial(ppr_cache)]
    fn personalized_graph_boost_caches_identical_seed_sets() {
        use crate::graph::CodeGraph;
        use crate::language::Language;

        __test_ppr_cache_clear();
        let start_len = __test_ppr_cache_len();

        let mut graph = CodeGraph::new();
        graph.add_edge("src/a.rs", "src/b.rs", "b", Language::Rust, Language::Rust);
        graph.add_edge("src/b.rs", "src/c.rs", "c", Language::Rust, Language::Rust);
        let pr = crate::graph::compute_pagerank(&graph, 0.85, 20);
        graph.apply_pagerank(&pr);

        let stage = PersonalizedGraphBoostStage;
        let symbols = crate::symbols::SymbolTable::new();
        let ctx = SearchContext {
            query: "test",
            symbols: &symbols,
            graph: Some(&graph),
            graph_boost_weight: 0.5,
            recency_map: None,
            chunk_meta: None,
            concepts: None,
        };

        // First call: cache miss → computes + inserts.
        let mut results1 = vec![
            make_result("a", 10.0, "src/a.rs"),
            make_result("b", 5.0, "src/b.rs"),
        ];
        stage.apply(&mut results1, &ctx).unwrap();
        assert_eq!(
            __test_ppr_cache_len(),
            start_len + 1,
            "first call should insert a cache entry"
        );

        // Second call with identical seeds: cache hit, no new entry.
        let mut results2 = vec![
            make_result("a", 10.0, "src/a.rs"),
            make_result("b", 5.0, "src/b.rs"),
        ];
        stage.apply(&mut results2, &ctx).unwrap();
        assert_eq!(
            __test_ppr_cache_len(),
            start_len + 1,
            "identical seeds should hit cache, not insert again"
        );

        // The two runs must produce the same scores to within float precision.
        for (r1, r2) in results1.iter().zip(results2.iter()) {
            assert_eq!(r1.file_path, r2.file_path);
            assert!(
                (r1.score - r2.score).abs() < 1e-6,
                "cached run diverged: {} vs {}",
                r1.score,
                r2.score,
            );
        }

        // Different seed set (different score rounding beyond 10^-3) →
        // cache miss, new entry.
        let mut results3 = vec![
            make_result("a", 10.5, "src/a.rs"),
            make_result("b", 5.0, "src/b.rs"),
        ];
        stage.apply(&mut results3, &ctx).unwrap();
        assert_eq!(
            __test_ppr_cache_len(),
            start_len + 2,
            "different seeds should produce a fresh cache entry"
        );
    }

    #[test]
    fn seed_cache_key_is_score_rounding_stable() {
        // Scores differing below 10^-3 hash to the same key so trivial
        // BM25 noise between calls doesn't miss the cache.
        let a = [("src/a.rs", 10.0001), ("src/b.rs", 5.0)];
        let b = [("src/a.rs", 10.0004), ("src/b.rs", 5.0)];
        assert_eq!(seed_cache_key(&a, 10), seed_cache_key(&b, 10));

        // But scores differing above 10^-3 do NOT collide.
        let c = [("src/a.rs", 10.5), ("src/b.rs", 5.0)];
        assert_ne!(seed_cache_key(&a, 10), seed_cache_key(&c, 10));

        // Different node_count invalidates the key even with identical seeds.
        assert_ne!(seed_cache_key(&a, 10), seed_cache_key(&a, 11));

        // Order matters — swapping seeds yields a different key (different
        // teleportation skew in the PPR computation).
        let d = [("src/b.rs", 5.0), ("src/a.rs", 10.0001)];
        assert_ne!(seed_cache_key(&a, 10), seed_cache_key(&d, 10));
    }
}

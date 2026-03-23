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
pub struct DeduplicationStage;

impl SearchStage for DeduplicationStage {
    fn apply(&self, results: &mut Vec<SearchResult>, _ctx: &SearchContext<'_>) -> Result<()> {
        super::search::dedup_overlapping(results);
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
        .add(DeduplicationStage)
}

/// Build the post-retrieval pipeline for `Strategy::Fast`.
pub fn fast_pipeline() -> SearchPipeline {
    SearchPipeline::new()
        .add(GraphBoostStage)
        .add(DefinitionBoostStage)
        .add(PopularityBoostStage)
        .add(PathMatchBoostStage)
        .add(TestDemotionStage)
        .add(TruncationStage {
            min_results: 3,
            cliff_threshold: 0.35,
        })
        .add(DeduplicationStage)
}

/// Build the post-retrieval pipeline for `Strategy::Thorough`.
pub fn thorough_pipeline() -> SearchPipeline {
    SearchPipeline::new()
        .add(GraphBoostStage)
        .add(DefinitionBoostStage)
        .add(PopularityBoostStage)
        .add(PathMatchBoostStage)
        .add(TestDemotionStage)
        .add(TruncationStage {
            min_results: 3,
            cliff_threshold: 0.35,
        })
        .add(DeduplicationStage)
}

/// Build the post-retrieval pipeline for `Strategy::Exact`.
pub fn exact_pipeline() -> SearchPipeline {
    SearchPipeline::new()
        .add(DefinitionBoostStage)
        .add(PathMatchBoostStage)
        .add(TestDemotionStage)
        .add(DeduplicationStage)
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
        };
        let mut results = vec![make_result("a", 10.0, "src/engine.rs")];
        pipeline.run(&mut results, &ctx).unwrap();
        assert_eq!(results.len(), 1);
    }
}

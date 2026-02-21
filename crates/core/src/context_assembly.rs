//! Intelligent context assembly with dependency-aware ordering.
//!
//! The [`IntelligentContextAssembler`] reorders search results so that
//! definitions appear before usages — both within a file (by line number)
//! and across files (using the [`CodeGraph`] when available). This produces
//! context that is easier for AI agents to consume because every symbol is
//! introduced before it is referenced.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use crate::graph::CodeGraph;
use crate::retriever::SearchResult;
use crate::tokenizer::{ContextBudget, ContextSnippet};

/// Assembles search results into an ordered, budget-constrained context window.
///
/// When a [`CodeGraph`] is provided, the assembler uses the graph's dependency
/// edges to place files that define symbols before files that use them. Without
/// a graph, files are ordered by their best relevance score (matching the
/// existing greedy behaviour).
///
/// Within each file, results are always sorted by `line_start` ascending so
/// that earlier definitions appear first.
pub struct IntelligentContextAssembler {
    budget: ContextBudget,
    graph: Option<FileDependencyGraph>,
}

/// A lightweight directed graph of file-level dependencies derived from
/// the symbol-level [`CodeGraph`].
///
/// An edge A -> B means "file A defines a symbol that file B uses", so A
/// should appear before B in the assembled context.
struct FileDependencyGraph {
    /// For each file, the set of files it depends on (i.e. files that define
    /// symbols this file uses).
    deps: HashMap<String, HashSet<String>>,
}

impl FileDependencyGraph {
    /// Build a file-level dependency graph from a [`CodeGraph`].
    ///
    /// For every edge (caller -> callee) in the symbol graph, we record that
    /// the caller's file depends on the callee's file (the callee's file
    /// defines the symbol, so it should come first).
    fn from_code_graph(graph: &CodeGraph) -> Self {
        let mut deps: HashMap<String, HashSet<String>> = HashMap::new();

        for edge in graph.inner.edge_indices() {
            let (src, dst) = graph
                .inner
                .edge_endpoints(edge)
                .expect("edge must have endpoints");

            let src_node = &graph.inner[src];
            let dst_node = &graph.inner[dst];

            // src references dst — so src's file depends on dst's file.
            // Skip self-file dependencies.
            if src_node.file != dst_node.file {
                deps.entry(src_node.file.clone())
                    .or_default()
                    .insert(dst_node.file.clone());
            }
        }

        Self { deps }
    }

    /// Topological sort of files. Files that are depended upon (define
    /// symbols) come before files that depend on them (use symbols).
    ///
    /// Only files present in `file_set` participate in the sort. Files not
    /// reachable in the dependency graph are appended at the end in the order
    /// they appear in `fallback_order`.
    fn topological_sort(
        &self,
        file_set: &HashSet<String>,
        fallback_order: &[String],
    ) -> Vec<String> {
        // Build in-degree map restricted to file_set.
        // An edge A -> B in self.deps means A depends on B, so in the
        // topological order B must come first. We invert the edges for
        // Kahn's algorithm: edge B -> A (B before A).
        let mut in_degree: HashMap<&str, usize> = HashMap::new();
        let mut forward_edges: HashMap<&str, Vec<&str>> = HashMap::new();

        for file in file_set {
            in_degree.entry(file.as_str()).or_insert(0);
        }

        for (user_file, def_files) in &self.deps {
            if !file_set.contains(user_file.as_str()) {
                continue;
            }
            for def_file in def_files {
                if !file_set.contains(def_file.as_str()) {
                    continue;
                }
                // def_file -> user_file (def before user)
                forward_edges
                    .entry(def_file.as_str())
                    .or_default()
                    .push(user_file.as_str());
                *in_degree.entry(user_file.as_str()).or_insert(0) += 1;
            }
        }

        // Kahn's algorithm.
        let mut queue: VecDeque<&str> = in_degree
            .iter()
            .filter(|&(_, deg)| *deg == 0)
            .map(|(&f, _)| f)
            .collect();

        // Sort the initial queue for deterministic output.
        let mut sorted_queue: Vec<&str> = queue.drain(..).collect();
        sorted_queue.sort();
        queue.extend(sorted_queue);

        let mut result: Vec<String> = Vec::new();
        let mut visited: HashSet<&str> = HashSet::new();

        while let Some(file) = queue.pop_front() {
            if !visited.insert(file) {
                continue;
            }
            result.push(file.to_string());

            if let Some(neighbors) = forward_edges.get(file) {
                let mut neighbors_sorted: Vec<&str> = neighbors.clone();
                neighbors_sorted.sort();
                for &neighbor in &neighbors_sorted {
                    if let Some(deg) = in_degree.get_mut(neighbor) {
                        *deg = deg.saturating_sub(1);
                        if *deg == 0 {
                            queue.push_back(neighbor);
                        }
                    }
                }
            }
        }

        // If there are cycles or disconnected files, append them in
        // fallback order.
        for file in fallback_order {
            if file_set.contains(file) && !visited.contains(file.as_str()) {
                result.push(file.clone());
            }
        }

        result
    }
}

impl IntelligentContextAssembler {
    /// Create a new assembler with the given token budget.
    pub fn new(token_budget: usize) -> Self {
        Self {
            budget: ContextBudget::new(token_budget),
            graph: None,
        }
    }

    /// Attach a [`CodeGraph`] for dependency-aware cross-file ordering.
    pub fn with_graph(mut self, graph: &CodeGraph) -> Self {
        self.graph = Some(FileDependencyGraph::from_code_graph(graph));
        self
    }

    /// Assemble context with dependency-aware ordering.
    ///
    /// 1. Sort results by score descending, keep top candidates (2x budget
    ///    worth to have room after reordering).
    /// 2. Group results by file path.
    /// 3. Within each file, sort by `line_start` ascending.
    /// 4. If a graph is available, topologically sort files so definitions
    ///    come before usages.
    /// 5. If no graph, order files by their best score (highest first).
    /// 6. Pack results into the budget in the reordered sequence.
    pub fn assemble(&mut self, mut results: Vec<SearchResult>) -> Vec<ContextSnippet> {
        if results.is_empty() {
            return Vec::new();
        }

        // Step 1: Sort by score descending, keep generous candidate pool.
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Step 2: Group by file path, preserving per-file insertion order.
        // We use a BTreeMap<file_path, Vec<SearchResult>> but need to track
        // the best score per file for fallback ordering.
        let mut file_groups: BTreeMap<String, Vec<SearchResult>> = BTreeMap::new();
        let mut file_best_score: HashMap<String, f32> = HashMap::new();

        for result in results {
            let score = result.score;
            let file = result.file_path.clone();
            file_groups.entry(file.clone()).or_default().push(result);
            let best = file_best_score.entry(file).or_insert(0.0);
            if score > *best {
                *best = score;
            }
        }

        // Step 3: Within each file, sort by line_start ascending.
        for group in file_groups.values_mut() {
            group.sort_by_key(|r| r.line_start);
        }

        // Step 4/5: Determine file ordering.
        let file_set: HashSet<String> = file_groups.keys().cloned().collect();

        // Fallback order: files sorted by best score descending.
        let mut fallback_order: Vec<String> = file_groups.keys().cloned().collect();
        fallback_order.sort_by(|a, b| {
            let sa = file_best_score.get(a).unwrap_or(&0.0);
            let sb = file_best_score.get(b).unwrap_or(&0.0);
            sb.partial_cmp(sa).unwrap_or(std::cmp::Ordering::Equal)
        });

        let ordered_files = if let Some(ref dep_graph) = self.graph {
            dep_graph.topological_sort(&file_set, &fallback_order)
        } else {
            fallback_order
        };

        // Step 6: Pack into budget in the determined order.
        for file in &ordered_files {
            if let Some(group) = file_groups.get(file) {
                for result in group {
                    let added = self.budget.try_add(
                        result.file_path.clone(),
                        result.language.clone(),
                        result.content.clone(),
                        result.line_start,
                        result.line_end,
                        result.score,
                    );
                    if !added {
                        // Budget exhausted — stop packing.
                        return self.take_snippets();
                    }
                }
            }
        }

        self.take_snippets()
    }

    /// Consume the internal budget and return the collected snippets.
    fn take_snippets(&mut self) -> Vec<ContextSnippet> {
        let budget = std::mem::replace(&mut self.budget, ContextBudget::new(0));
        budget.into_snippets()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{CodeGraph, ReferenceKind, SymbolKind};

    /// Helper to create a SearchResult with the given fields.
    fn make_result(
        file_path: &str,
        line_start: u64,
        line_end: u64,
        score: f32,
        content: &str,
    ) -> SearchResult {
        SearchResult {
            chunk_id: format!("{}:{}", file_path, line_start),
            file_path: file_path.to_string(),
            language: "rust".to_string(),
            score,
            line_start,
            line_end,
            signature: String::new(),
            content: content.to_string(),
        }
    }

    #[test]
    fn test_within_file_ordering() {
        // Two results from same file, one at line 1, one at line 50.
        // Should be ordered line 1 first, regardless of score.
        let results = vec![
            make_result("src/main.rs", 50, 60, 9.0, "fn usage()"),
            make_result("src/main.rs", 1, 10, 5.0, "fn definition()"),
        ];

        let mut assembler = IntelligentContextAssembler::new(10_000);
        let snippets = assembler.assemble(results);

        assert_eq!(snippets.len(), 2);
        // Line 1 should come before line 50, even though its score is lower.
        assert_eq!(snippets[0].line_start, 1);
        assert_eq!(snippets[0].content, "fn definition()");
        assert_eq!(snippets[1].line_start, 50);
        assert_eq!(snippets[1].content, "fn usage()");
    }

    #[test]
    fn test_definition_before_usage_across_files() {
        // File A defines "Config", File B uses "Config".
        // With graph, A should come before B.
        let mut graph = CodeGraph::new();
        let config_def = graph.add_symbol("Config", "src/config.rs", SymbolKind::Struct);
        let main_fn = graph.add_symbol("main", "src/main.rs", SymbolKind::Function);
        // main references Config: main -> Config edge
        graph.add_reference(main_fn, config_def, ReferenceKind::TypeRef);

        let results = vec![
            // main.rs has higher score but uses Config from config.rs
            make_result("src/main.rs", 1, 10, 9.0, "fn main() { let c = Config::new(); }"),
            make_result("src/config.rs", 1, 10, 5.0, "pub struct Config { }"),
        ];

        let assembler = IntelligentContextAssembler::new(10_000);
        let snippets = assembler.with_graph(&graph).assemble(results);

        assert_eq!(snippets.len(), 2);
        // config.rs (definition) should come before main.rs (usage),
        // even though main.rs has a higher score.
        assert_eq!(snippets[0].file_path, "src/config.rs");
        assert_eq!(snippets[1].file_path, "src/main.rs");
    }

    #[test]
    fn test_budget_truncation() {
        // Results that exceed budget — should be truncated.
        // Each content is ~40 chars = 10 tokens at 4 chars/token.
        let results = vec![
            make_result("a.rs", 1, 5, 9.0, &"x".repeat(40)),  // 10 tokens
            make_result("b.rs", 1, 5, 8.0, &"y".repeat(40)),  // 10 tokens
            make_result("c.rs", 1, 5, 7.0, &"z".repeat(40)),  // 10 tokens
        ];

        // Budget for only 15 tokens — fits first 2 results but not 3rd.
        let mut assembler = IntelligentContextAssembler::new(15);
        let snippets = assembler.assemble(results);

        // Should have truncated: only first result fits (10 tokens),
        // second result also fits (10 tokens total = 20 > 15), so only 1.
        assert_eq!(snippets.len(), 1);
        assert_eq!(snippets[0].file_path, "a.rs");
    }

    #[test]
    fn test_without_graph_falls_back_to_score() {
        // Without graph, files ordered by best score.
        let results = vec![
            make_result("src/low.rs", 1, 5, 3.0, "fn low()"),
            make_result("src/high.rs", 1, 5, 9.0, "fn high()"),
            make_result("src/mid.rs", 1, 5, 6.0, "fn mid()"),
        ];

        let mut assembler = IntelligentContextAssembler::new(10_000);
        let snippets = assembler.assemble(results);

        assert_eq!(snippets.len(), 3);
        // Without graph, files should be ordered by best score descending.
        assert_eq!(snippets[0].file_path, "src/high.rs");
        assert_eq!(snippets[1].file_path, "src/mid.rs");
        assert_eq!(snippets[2].file_path, "src/low.rs");
    }

    #[test]
    fn test_empty_results() {
        let mut assembler = IntelligentContextAssembler::new(10_000);
        let snippets = assembler.assemble(vec![]);
        assert!(snippets.is_empty());
    }

    #[test]
    fn test_single_result() {
        let results = vec![make_result("src/main.rs", 1, 10, 5.0, "fn main()")];

        let mut assembler = IntelligentContextAssembler::new(10_000);
        let snippets = assembler.assemble(results);

        assert_eq!(snippets.len(), 1);
        assert_eq!(snippets[0].file_path, "src/main.rs");
        assert_eq!(snippets[0].content, "fn main()");
    }

    #[test]
    fn test_multiple_files_within_file_ordering() {
        // Multiple results across two files, verifying within-file ordering.
        let results = vec![
            make_result("src/b.rs", 30, 40, 8.0, "fn b_usage()"),
            make_result("src/a.rs", 20, 30, 9.0, "fn a_second()"),
            make_result("src/b.rs", 1, 10, 7.0, "fn b_def()"),
            make_result("src/a.rs", 1, 10, 6.0, "fn a_first()"),
        ];

        let mut assembler = IntelligentContextAssembler::new(10_000);
        let snippets = assembler.assemble(results);

        assert_eq!(snippets.len(), 4);

        // Without graph, file a.rs should come first (best score 9.0 > 8.0).
        // Within a.rs: line 1 before line 20.
        assert_eq!(snippets[0].file_path, "src/a.rs");
        assert_eq!(snippets[0].line_start, 1);
        assert_eq!(snippets[1].file_path, "src/a.rs");
        assert_eq!(snippets[1].line_start, 20);

        // Then b.rs: line 1 before line 30.
        assert_eq!(snippets[2].file_path, "src/b.rs");
        assert_eq!(snippets[2].line_start, 1);
        assert_eq!(snippets[3].file_path, "src/b.rs");
        assert_eq!(snippets[3].line_start, 30);
    }

    #[test]
    fn test_graph_with_diamond_dependency() {
        // Diamond: main.rs uses both config.rs and utils.rs.
        // config.rs and utils.rs are independent.
        let mut graph = CodeGraph::new();
        let config = graph.add_symbol("Config", "src/config.rs", SymbolKind::Struct);
        let utils = graph.add_symbol("utils", "src/utils.rs", SymbolKind::Function);
        let main_fn = graph.add_symbol("main", "src/main.rs", SymbolKind::Function);

        graph.add_reference(main_fn, config, ReferenceKind::TypeRef);
        graph.add_reference(main_fn, utils, ReferenceKind::Call);

        let results = vec![
            make_result("src/main.rs", 1, 10, 9.0, "fn main()"),
            make_result("src/config.rs", 1, 10, 5.0, "struct Config"),
            make_result("src/utils.rs", 1, 10, 4.0, "fn utils()"),
        ];

        let assembler = IntelligentContextAssembler::new(10_000);
        let snippets = assembler.with_graph(&graph).assemble(results);

        assert_eq!(snippets.len(), 3);

        // main.rs depends on both config.rs and utils.rs, so main.rs must
        // come last. config.rs and utils.rs are independent — their relative
        // order is deterministic (alphabetical due to sorted initial queue).
        assert_eq!(snippets[0].file_path, "src/config.rs");
        assert_eq!(snippets[1].file_path, "src/utils.rs");
        assert_eq!(snippets[2].file_path, "src/main.rs");
    }

    #[test]
    fn test_graph_with_chain_dependency() {
        // Chain: main.rs -> service.rs -> config.rs
        let mut graph = CodeGraph::new();
        let config = graph.add_symbol("Config", "src/config.rs", SymbolKind::Struct);
        let service = graph.add_symbol("Service", "src/service.rs", SymbolKind::Struct);
        let main_fn = graph.add_symbol("main", "src/main.rs", SymbolKind::Function);

        graph.add_reference(main_fn, service, ReferenceKind::Call);
        graph.add_reference(service, config, ReferenceKind::TypeRef);

        let results = vec![
            make_result("src/main.rs", 1, 10, 9.0, "fn main()"),
            make_result("src/service.rs", 1, 10, 7.0, "struct Service"),
            make_result("src/config.rs", 1, 10, 5.0, "struct Config"),
        ];

        let assembler = IntelligentContextAssembler::new(10_000);
        let snippets = assembler.with_graph(&graph).assemble(results);

        assert_eq!(snippets.len(), 3);
        // config.rs first (no deps), then service.rs (depends on config),
        // then main.rs (depends on service).
        assert_eq!(snippets[0].file_path, "src/config.rs");
        assert_eq!(snippets[1].file_path, "src/service.rs");
        assert_eq!(snippets[2].file_path, "src/main.rs");
    }

    #[test]
    fn test_graph_files_not_in_results_are_ignored() {
        // Graph has edges involving files not in the search results.
        let mut graph = CodeGraph::new();
        let config = graph.add_symbol("Config", "src/config.rs", SymbolKind::Struct);
        let unused = graph.add_symbol("Unused", "src/unused.rs", SymbolKind::Struct);
        let main_fn = graph.add_symbol("main", "src/main.rs", SymbolKind::Function);

        graph.add_reference(main_fn, config, ReferenceKind::TypeRef);
        graph.add_reference(main_fn, unused, ReferenceKind::Call);

        // Only config.rs and main.rs in results — unused.rs should not appear.
        let results = vec![
            make_result("src/main.rs", 1, 10, 9.0, "fn main()"),
            make_result("src/config.rs", 1, 10, 5.0, "struct Config"),
        ];

        let assembler = IntelligentContextAssembler::new(10_000);
        let snippets = assembler.with_graph(&graph).assemble(results);

        assert_eq!(snippets.len(), 2);
        assert_eq!(snippets[0].file_path, "src/config.rs");
        assert_eq!(snippets[1].file_path, "src/main.rs");
    }

    #[test]
    fn test_budget_exactly_fits() {
        // Content exactly fills the budget.
        // 40 chars / 4 = 10 tokens.
        let results = vec![
            make_result("a.rs", 1, 5, 9.0, &"x".repeat(40)),
        ];

        let mut assembler = IntelligentContextAssembler::new(10);
        let snippets = assembler.assemble(results);

        assert_eq!(snippets.len(), 1);
    }

    #[test]
    fn test_scores_preserved_in_snippets() {
        let results = vec![
            make_result("src/a.rs", 1, 5, 7.5, "fn a()"),
            make_result("src/b.rs", 1, 5, 3.2, "fn b()"),
        ];

        let mut assembler = IntelligentContextAssembler::new(10_000);
        let snippets = assembler.assemble(results);

        assert_eq!(snippets.len(), 2);
        // a.rs has higher score, comes first (no graph).
        assert!((snippets[0].score - 7.5).abs() < f32::EPSILON);
        assert!((snippets[1].score - 3.2).abs() < f32::EPSILON);
    }
}

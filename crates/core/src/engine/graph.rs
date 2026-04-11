use std::collections::HashMap;

use crate::error::Result;
use crate::graph::community::CommunityResult;
use crate::graph::surprise::SurprisingEdge;
use crate::graph::{
    CypherExportOptions, GraphData, GraphStats, GraphmlExportOptions, HtmlExportOptions,
    ObsidianExportOptions, RepoMapOptions, generate_repo_map,
};
use crate::symbols::Symbol;

use super::Engine;

impl Engine {
    // -------------------------------------------------------------------------
    // Graph public API
    // -------------------------------------------------------------------------

    /// Generate a token-budgeted repo map.  Returns `None` if the graph is not available.
    pub fn repo_map(&self, options: RepoMapOptions) -> Option<String> {
        self.graph
            .as_ref()
            .map(|g| generate_repo_map(g, &self.symbols, &options))
    }

    /// Return the files that directly import `file_path`.
    pub fn callers(&self, file_path: &str) -> Vec<String> {
        self.graph
            .as_ref()
            .map(|g| g.callers(file_path))
            .unwrap_or_default()
    }

    /// Return the files that `file_path` directly imports.
    pub fn callees(&self, file_path: &str) -> Vec<String> {
        self.graph
            .as_ref()
            .map(|g| g.callees(file_path))
            .unwrap_or_default()
    }

    /// Return files that transitively import `file_path` up to `depth` hops.
    pub fn transitive_callers(&self, file_path: &str, depth: usize) -> Vec<String> {
        self.graph
            .as_ref()
            .map(|g| g.transitive_callers(file_path, depth))
            .unwrap_or_default()
    }

    /// Return files that `file_path` transitively imports up to `depth` hops.
    pub fn transitive_callees(&self, file_path: &str, depth: usize) -> Vec<String> {
        self.graph
            .as_ref()
            .map(|g| g.transitive_callees(file_path, depth))
            .unwrap_or_default()
    }

    /// Return transitive dependencies of `file_path` up to `depth` hops.
    pub fn dependencies(&self, file_path: &str, depth: usize) -> Vec<String> {
        self.transitive_callees(file_path, depth)
    }

    /// Find files under `from_prefix` that import any file under `to_prefix`.
    ///
    /// Answers module-level cross-package queries like "which gateway files
    /// import from the security module?"
    pub fn cross_imports(&self, from_prefix: &str, to_prefix: &str) -> Vec<String> {
        self.graph
            .as_ref()
            .map(|g| g.cross_imports(from_prefix, to_prefix))
            .unwrap_or_default()
    }

    /// Find files under `from_prefix` that import any file under `to_prefix`, ranked by relevance.
    ///
    /// Results are sorted by score descending: score = sum(target_pagerank) × (1 + recency_boost).
    pub fn cross_imports_ranked(
        &self,
        from_prefix: &str,
        to_prefix: &str,
        limit: Option<usize>,
    ) -> Vec<(String, f32)> {
        self.graph
            .as_ref()
            .map(|g| {
                let recency = self.get_recency_map();
                g.cross_imports_ranked(from_prefix, to_prefix, Some(recency), limit)
            })
            .unwrap_or_default()
    }

    /// Return graph statistics, or `None` if the graph has not been built.
    pub fn graph_stats(&self) -> Option<GraphStats> {
        self.graph.as_ref().map(|g| g.stats())
    }

    /// Return the current dependency graph as a flat snapshot.
    pub fn graph_data(&self) -> Option<GraphData> {
        self.graph.as_ref().map(|g| g.to_flat())
    }

    /// Return all symbol-level call edges as `(caller_file, callee_name)` tuples.
    pub fn call_graph_edges(&self) -> Vec<(String, String, String)> {
        self.graph
            .as_ref()
            .map(|g| {
                g.call_edges()
                    .into_iter()
                    .map(|(caller, callee)| (caller.clone(), caller, callee))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Return true if the graph contains any symbol-level call edges.
    pub fn has_call_graph(&self) -> bool {
        self.graph
            .as_ref()
            .map(|g| !g.call_edges().is_empty())
            .unwrap_or(false)
    }

    /// Return true if the graph contains a pre-built symbol-level inner graph.
    pub fn has_symbol_graph(&self) -> bool {
        self.graph
            .as_ref()
            .is_some_and(|g| g.symbol_node_count() > 0)
    }

    /// Query the symbol-level graph for callers of a given symbol name.
    ///
    /// Returns `(file, caller_name)` pairs for each function that calls the symbol.
    pub fn symbol_callers_from_graph(&self, symbol: &str) -> Vec<(String, String)> {
        self.graph
            .as_ref()
            .map(|g| g.get_symbol_callers(symbol))
            .unwrap_or_default()
    }

    /// Query the symbol-level graph for callees of a given symbol name.
    ///
    /// Returns callee symbol names.
    pub fn symbol_callees_from_graph(&self, symbol: &str) -> Vec<String> {
        self.graph
            .as_ref()
            .map(|g| g.get_symbol_callees(symbol))
            .unwrap_or_default()
    }

    /// Compute personalized PageRank anchored to `seed_files`.
    ///
    /// Files closer to the seeds in the import graph score higher.  Useful for
    /// context-aware ranking ("what files matter given I'm working in X?").
    /// Falls back to global PageRank when `seed_files` is empty.
    pub fn personalized_pagerank(&self, seed_files: &[&str]) -> HashMap<String, f32> {
        match &self.graph {
            Some(graph) => crate::graph::compute_personalized_pagerank(
                graph,
                self.config.graph.damping,
                self.config.graph.iterations,
                seed_files,
            ),
            None => HashMap::new(),
        }
    }

    /// Query the symbol table.
    ///
    /// Performs case-insensitive substring matching on symbol names.
    /// If `file` is provided, also filters by file path.
    pub fn symbols(&self, filter: &str, file: Option<&str>) -> Result<Vec<Symbol>> {
        Ok(self.symbols.filter(filter, file))
    }

    // -------------------------------------------------------------------------
    // Graph analysis: community detection, shortest path, surprises, HTML export
    // -------------------------------------------------------------------------

    /// Run Louvain community detection on the dependency graph.
    ///
    /// Returns `None` if the graph is not available. Mutates the graph's
    /// node community assignments in place.
    pub fn detect_communities(&mut self) -> Option<CommunityResult> {
        self.graph.as_mut().map(|g| g.detect_communities())
    }

    /// Return the cached community assignments from the last detection run.
    pub fn communities(&self) -> HashMap<String, usize> {
        self.graph
            .as_ref()
            .map(|g| {
                g.file_paths()
                    .into_iter()
                    .filter_map(|p| {
                        let node = g.node(&p)?;
                        let comm = node.community?;
                        Some((p, comm))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Find the shortest path between two files in the dependency graph.
    pub fn shortest_path(&self, from: &str, to: &str) -> Option<Vec<String>> {
        self.graph.as_ref().and_then(|g| g.shortest_path(from, to))
    }

    /// Return the most surprising edges in the dependency graph.
    pub fn surprising_edges(&self, top_n: usize) -> Vec<SurprisingEdge> {
        self.graph
            .as_ref()
            .map(|g| crate::graph::surprise::detect_surprises(g, top_n))
            .unwrap_or_default()
    }

    /// Export the dependency graph as a self-contained interactive HTML file.
    pub fn export_html(&self, options: HtmlExportOptions) -> Result<()> {
        match &self.graph {
            Some(g) => crate::graph::html_export::export_html(g, &options),
            None => Err(crate::error::CodixingError::Graph(
                "graph not available — run `codixing init` first".into(),
            )),
        }
    }

    /// Export the dependency graph as a GraphML XML file (for Gephi/yEd).
    pub fn export_graphml(&self, options: GraphmlExportOptions) -> Result<()> {
        match &self.graph {
            Some(g) => crate::graph::graphml_export::export_graphml(g, &options),
            None => Err(crate::error::CodixingError::Graph(
                "graph not available — run `codixing init` first".into(),
            )),
        }
    }

    /// Export the dependency graph as Neo4j Cypher MERGE statements.
    pub fn export_cypher(&self, options: CypherExportOptions) -> Result<()> {
        match &self.graph {
            Some(g) => crate::graph::cypher_export::export_cypher(g, &options),
            None => Err(crate::error::CodixingError::Graph(
                "graph not available — run `codixing init` first".into(),
            )),
        }
    }

    /// Export the dependency graph as an Obsidian vault with linked Markdown notes.
    pub fn export_obsidian(&self, options: ObsidianExportOptions) -> Result<usize> {
        match &self.graph {
            Some(g) => crate::graph::obsidian_export::export_obsidian(g, &options),
            None => Err(crate::error::CodixingError::Graph(
                "graph not available — run `codixing init` first".into(),
            )),
        }
    }
}

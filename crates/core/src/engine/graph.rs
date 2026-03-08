use std::collections::HashMap;

use crate::error::Result;
use crate::graph::{GraphData, GraphStats, RepoMapOptions, generate_repo_map};
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
}

pub mod extractor;
pub mod pagerank;
pub mod repomap;
pub mod resolver;

use std::collections::HashMap;

use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use serde::{Deserialize, Serialize};

use crate::language::Language;

// Re-export public types from sub-modules.
pub use extractor::ImportExtractor;
pub use pagerank::compute_pagerank;
pub use repomap::{RepoMapOptions, generate_repo_map};
pub use resolver::ImportResolver;

/// Kind of a dependency edge between files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeKind {
    /// The import resolved to an indexed file in this project.
    Resolved,
    /// The import refers to an external package / stdlib and could not be resolved.
    External,
}

/// A node in the dependency graph representing a single source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeNode {
    /// Relative path, forward-slash normalized.
    pub file_path: String,
    /// Detected language.
    pub language: Language,
    /// PageRank score, 0.0 until `apply_pagerank` is called.
    pub pagerank: f32,
    /// Number of outgoing import edges.
    pub out_degree: usize,
    /// Number of incoming import edges.
    pub in_degree: usize,
}

/// An edge in the dependency graph representing an import relationship.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeEdge {
    /// Import string as it appears in the source code.
    pub raw_import: String,
    /// Whether the import resolved to a known file or is external.
    pub kind: EdgeKind,
}

/// Flat, serialization-friendly representation of the graph.
///
/// Used for bitcode persistence — avoids petgraph index fragility across rebuilds.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GraphData {
    pub nodes: Vec<CodeNode>,
    /// Edges as `(from_path, to_path, edge)` triples.
    pub edges: Vec<(String, String, CodeEdge)>,
}

/// Summary statistics about the dependency graph.
#[derive(Debug, Clone)]
pub struct GraphStats {
    pub node_count: usize,
    pub edge_count: usize,
    pub resolved_edges: usize,
    pub external_edges: usize,
}

/// In-memory dependency graph over source files.
///
/// Wraps a petgraph `DiGraph` with a path→NodeIndex lookup table so callers
/// can work with file paths rather than opaque indices.
pub struct CodeGraph {
    graph: DiGraph<CodeNode, CodeEdge>,
    path_to_node: HashMap<String, NodeIndex>,
}

impl CodeGraph {
    /// Create an empty graph.
    pub fn new() -> Self {
        Self {
            graph: DiGraph::new(),
            path_to_node: HashMap::new(),
        }
    }

    /// Return or insert a node for `file_path`.
    pub fn get_or_insert_node(&mut self, file_path: &str, language: Language) -> NodeIndex {
        if let Some(&idx) = self.path_to_node.get(file_path) {
            return idx;
        }
        let node = CodeNode {
            file_path: file_path.to_string(),
            language,
            pagerank: 0.0,
            out_degree: 0,
            in_degree: 0,
        };
        let idx = self.graph.add_node(node);
        self.path_to_node.insert(file_path.to_string(), idx);
        idx
    }

    /// Add a resolved edge between two indexed files.
    pub fn add_edge(
        &mut self,
        from: &str,
        to: &str,
        raw_import: &str,
        from_lang: Language,
        to_lang: Language,
    ) {
        let from_idx = self.get_or_insert_node(from, from_lang);
        let to_idx = self.get_or_insert_node(to, to_lang);
        self.graph.add_edge(
            from_idx,
            to_idx,
            CodeEdge {
                raw_import: raw_import.to_string(),
                kind: EdgeKind::Resolved,
            },
        );
        // Update degree counters.
        if let Some(n) = self.graph.node_weight_mut(from_idx) {
            n.out_degree += 1;
        }
        if let Some(n) = self.graph.node_weight_mut(to_idx) {
            n.in_degree += 1;
        }
    }

    /// Add an external (unresolved) edge; `to` is the raw import string used as a label.
    pub fn add_external_edge(&mut self, from: &str, raw_import: &str, from_lang: Language) {
        let from_idx = self.get_or_insert_node(from, from_lang);
        // External target represented as a pseudo-node with the raw import as path.
        let ext_key = format!("__ext__:{raw_import}");
        let to_idx = self.get_or_insert_node(&ext_key, from_lang);
        self.graph.add_edge(
            from_idx,
            to_idx,
            CodeEdge {
                raw_import: raw_import.to_string(),
                kind: EdgeKind::External,
            },
        );
        if let Some(n) = self.graph.node_weight_mut(from_idx) {
            n.out_degree += 1;
        }
    }

    /// Remove a file node and all its incident edges from the graph.
    pub fn remove_file(&mut self, file_path: &str) {
        if let Some(idx) = self.path_to_node.remove(file_path) {
            // Collect neighbours whose degree counters need adjustment.
            let in_neighbours: Vec<NodeIndex> = self
                .graph
                .neighbors_directed(idx, petgraph::Direction::Incoming)
                .collect();
            let out_neighbours: Vec<NodeIndex> = self
                .graph
                .neighbors_directed(idx, petgraph::Direction::Outgoing)
                .collect();

            for nb in &in_neighbours {
                if let Some(n) = self.graph.node_weight_mut(*nb) {
                    n.out_degree = n.out_degree.saturating_sub(1);
                }
            }
            for nb in &out_neighbours {
                if let Some(n) = self.graph.node_weight_mut(*nb) {
                    n.in_degree = n.in_degree.saturating_sub(1);
                }
            }

            // petgraph swap_remove_node swaps the last node into position `idx`.
            // We must update path_to_node for the swapped node.
            let last_idx = NodeIndex::new(self.graph.node_count().saturating_sub(1));
            if idx != last_idx {
                if let Some(swapped_path) = self
                    .graph
                    .node_weight(last_idx)
                    .map(|n| n.file_path.clone())
                {
                    self.path_to_node.insert(swapped_path, idx);
                }
            }
            self.graph.remove_node(idx);
        }
    }

    /// Remove only the outgoing edges of `file_path` (used before re-extracting imports).
    pub fn remove_file_edges(&mut self, file_path: &str) {
        if let Some(&idx) = self.path_to_node.get(file_path) {
            let out_neighbours: Vec<NodeIndex> = self
                .graph
                .neighbors_directed(idx, petgraph::Direction::Outgoing)
                .collect();
            for nb in &out_neighbours {
                if let Some(n) = self.graph.node_weight_mut(*nb) {
                    n.in_degree = n.in_degree.saturating_sub(1);
                }
            }
            // Remove all outgoing edges.
            let out_edges: Vec<_> = self
                .graph
                .edges_directed(idx, petgraph::Direction::Outgoing)
                .map(|e| e.id())
                .collect();
            for e in out_edges {
                self.graph.remove_edge(e);
            }
            if let Some(n) = self.graph.node_weight_mut(idx) {
                n.out_degree = 0;
            }
        }
    }

    /// Files that import `file_path` (direct callers).
    pub fn callers(&self, file_path: &str) -> Vec<String> {
        let Some(&idx) = self.path_to_node.get(file_path) else {
            return Vec::new();
        };
        let mut seen = std::collections::HashSet::new();
        self.graph
            .neighbors_directed(idx, petgraph::Direction::Incoming)
            .filter_map(|nb| self.graph.node_weight(nb))
            .filter(|n| !n.file_path.starts_with("__ext__:"))
            .map(|n| n.file_path.clone())
            .filter(|p| seen.insert(p.clone()))
            .collect()
    }

    /// Files that `file_path` imports (direct callees / dependencies).
    pub fn callees(&self, file_path: &str) -> Vec<String> {
        let Some(&idx) = self.path_to_node.get(file_path) else {
            return Vec::new();
        };
        let mut seen = std::collections::HashSet::new();
        self.graph
            .neighbors_directed(idx, petgraph::Direction::Outgoing)
            .filter_map(|nb| self.graph.node_weight(nb))
            .filter(|n| !n.file_path.starts_with("__ext__:"))
            .map(|n| n.file_path.clone())
            .filter(|p| seen.insert(p.clone()))
            .collect()
    }

    /// Transitive callers (files that transitively depend on `file_path`) up to `depth` hops.
    pub fn transitive_callers(&self, file_path: &str, depth: usize) -> Vec<String> {
        self.transitive_traverse(file_path, depth, |f| self.callers(f))
    }

    /// Transitive callees (files that `file_path` transitively imports) up to `depth` hops.
    pub fn transitive_callees(&self, file_path: &str, depth: usize) -> Vec<String> {
        self.transitive_traverse(file_path, depth, |f| self.callees(f))
    }

    fn transitive_traverse(
        &self,
        file_path: &str,
        depth: usize,
        neighbors: impl Fn(&str) -> Vec<String>,
    ) -> Vec<String> {
        let mut visited = std::collections::HashSet::new();
        let mut frontier = vec![file_path.to_string()];
        for _ in 0..depth {
            let mut next = Vec::new();
            for f in &frontier {
                for nb in neighbors(f) {
                    if visited.insert(nb.clone()) {
                        next.push(nb);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        visited.into_iter().collect()
    }

    /// Get a node by file path.
    pub fn node(&self, file_path: &str) -> Option<&CodeNode> {
        self.path_to_node
            .get(file_path)
            .and_then(|idx| self.graph.node_weight(*idx))
    }

    /// Apply computed PageRank scores back to the graph nodes.
    pub fn apply_pagerank(&mut self, scores: &HashMap<String, f32>) {
        for node in self.graph.node_weights_mut() {
            if let Some(&pr) = scores.get(&node.file_path) {
                node.pagerank = pr;
            }
        }
    }

    /// Serialize to the flat `GraphData` format for persistence.
    pub fn to_flat(&self) -> GraphData {
        let nodes: Vec<CodeNode> = self
            .graph
            .node_weights()
            .filter(|n| !n.file_path.starts_with("__ext__:"))
            .cloned()
            .collect();

        let edges: Vec<(String, String, CodeEdge)> = self
            .graph
            .edge_indices()
            .filter_map(|e| {
                let (from_idx, to_idx) = self.graph.edge_endpoints(e)?;
                let edge = self.graph.edge_weight(e)?;
                let from = self.graph.node_weight(from_idx)?;
                let to = self.graph.node_weight(to_idx)?;
                Some((from.file_path.clone(), to.file_path.clone(), edge.clone()))
            })
            .collect();

        GraphData { nodes, edges }
    }

    /// Reconstruct a `CodeGraph` from the flat persistence format.
    pub fn from_flat(data: GraphData) -> Self {
        let mut g = Self::new();
        for node in &data.nodes {
            g.get_or_insert_node(&node.file_path, node.language);
            // Restore persisted PageRank and degree counts.
            if let Some(idx) = g.path_to_node.get(&node.file_path).copied() {
                if let Some(n) = g.graph.node_weight_mut(idx) {
                    n.pagerank = node.pagerank;
                    n.out_degree = node.out_degree;
                    n.in_degree = node.in_degree;
                }
            }
        }
        for (from, to, edge) in data.edges {
            let from_lang = g
                .path_to_node
                .get(&from)
                .and_then(|idx| g.graph.node_weight(*idx))
                .map(|n| n.language)
                .unwrap_or(Language::Rust);
            let to_lang = g
                .path_to_node
                .get(&to)
                .and_then(|idx| g.graph.node_weight(*idx))
                .map(|n| n.language)
                .unwrap_or(Language::Rust);
            let from_idx = g.get_or_insert_node(&from, from_lang);
            let to_idx = g.get_or_insert_node(&to, to_lang);
            g.graph.add_edge(from_idx, to_idx, edge);
        }
        g
    }

    /// Compute graph statistics.
    pub fn stats(&self) -> GraphStats {
        let mut resolved = 0usize;
        let mut external = 0usize;
        for e in self.graph.edge_weights() {
            match e.kind {
                EdgeKind::Resolved => resolved += 1,
                EdgeKind::External => external += 1,
            }
        }
        GraphStats {
            node_count: self.graph.node_count(),
            edge_count: self.graph.edge_count(),
            resolved_edges: resolved,
            external_edges: external,
        }
    }

    /// Iterate over all real (non-external) nodes sorted by PageRank descending.
    pub fn nodes_by_pagerank(&self) -> Vec<&CodeNode> {
        let mut nodes: Vec<&CodeNode> = self
            .graph
            .node_weights()
            .filter(|n| !n.file_path.starts_with("__ext__:"))
            .collect();
        nodes.sort_by(|a, b| {
            b.pagerank
                .partial_cmp(&a.pagerank)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        nodes
    }
}

impl Default for CodeGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_graph_has_zero_stats() {
        let g = CodeGraph::new();
        let s = g.stats();
        assert_eq!(s.node_count, 0);
        assert_eq!(s.edge_count, 0);
    }

    #[test]
    fn add_edge_creates_nodes() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/a.rs",
            "src/b.rs",
            "crate::b",
            Language::Rust,
            Language::Rust,
        );
        let s = g.stats();
        assert_eq!(s.node_count, 2);
        assert_eq!(s.edge_count, 1);
        assert_eq!(s.resolved_edges, 1);
    }

    #[test]
    fn callers_and_callees() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/main.rs",
            "src/parser.rs",
            "crate::parser",
            Language::Rust,
            Language::Rust,
        );
        g.add_edge(
            "src/engine.rs",
            "src/parser.rs",
            "crate::parser",
            Language::Rust,
            Language::Rust,
        );

        let callers = g.callers("src/parser.rs");
        assert_eq!(callers.len(), 2);
        assert!(callers.contains(&"src/main.rs".to_string()));
        assert!(callers.contains(&"src/engine.rs".to_string()));

        let callees = g.callees("src/main.rs");
        assert_eq!(callees.len(), 1);
        assert_eq!(callees[0], "src/parser.rs");
    }

    #[test]
    fn remove_file_drops_edges() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/a.rs",
            "src/b.rs",
            "crate::b",
            Language::Rust,
            Language::Rust,
        );
        g.remove_file("src/b.rs");
        assert!(g.callees("src/a.rs").is_empty());
    }

    #[test]
    fn remove_file_edges_keeps_node() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/a.rs",
            "src/b.rs",
            "crate::b",
            Language::Rust,
            Language::Rust,
        );
        g.remove_file_edges("src/a.rs");
        // Node still exists, but edge is gone.
        assert!(g.node("src/a.rs").is_some());
        assert!(g.callees("src/a.rs").is_empty());
    }

    #[test]
    fn flat_round_trip() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/a.rs",
            "src/b.rs",
            "crate::b",
            Language::Rust,
            Language::Rust,
        );
        g.add_external_edge("src/a.rs", "std::collections::HashMap", Language::Rust);

        let flat = g.to_flat();
        let g2 = CodeGraph::from_flat(flat);

        assert_eq!(g2.callees("src/a.rs"), vec!["src/b.rs"]);
    }
}

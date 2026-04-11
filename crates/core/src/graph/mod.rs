pub mod community;
pub mod extract;
pub mod extractor;
pub mod html_export;
pub mod pagerank;
pub mod persistence;
pub mod repomap;
pub mod resolver;
pub mod surprise;
pub mod types;

use std::collections::HashMap;

use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use serde::{Deserialize, Serialize};

use crate::language::Language;

// Re-export public types from sub-modules.
pub use community::CommunityResult;
pub use extractor::{CallExtractor, ImportExtractor};
pub use html_export::HtmlExportOptions;
pub use pagerank::{
    compute_pagerank, compute_personalized_pagerank, compute_weighted_personalized_pagerank,
};
pub use repomap::{RepoMapOptions, generate_repo_map};
pub use resolver::ImportResolver;
pub use surprise::SurprisingEdge;
pub use types::{ReferenceKind, SymbolKind, SymbolNode};

/// Confidence level of a dependency edge, auto-derived from [`EdgeKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeConfidence {
    /// AST-resolved imports (EdgeKind::Resolved).
    Verified,
    /// Call extraction (EdgeKind::Calls).
    High,
    /// Doc-to-code references (EdgeKind::DocumentedBy).
    Medium,
    /// External/unresolved (EdgeKind::External).
    Low,
}

/// Kind of a dependency edge between files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeKind {
    /// The import resolved to an indexed file in this project.
    Resolved,
    /// The import refers to an external package / stdlib and could not be resolved.
    External,
    /// A function/method call site resolved to a symbol defined in another file.
    /// These edges are extracted from call expressions via [`CallExtractor`] and
    /// complement import edges with fine-grained call-level coupling information.
    Calls,
    /// A documentation file references a symbol defined in a code file.
    DocumentedBy,
}

impl EdgeKind {
    /// Return the default confidence level for this edge kind.
    pub fn default_confidence(&self) -> EdgeConfidence {
        match self {
            EdgeKind::Resolved => EdgeConfidence::Verified,
            EdgeKind::Calls => EdgeConfidence::High,
            EdgeKind::DocumentedBy => EdgeConfidence::Medium,
            EdgeKind::External => EdgeConfidence::Low,
        }
    }
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
    /// Community ID assigned by Louvain detection, `None` until detection runs.
    #[serde(default)]
    pub community: Option<usize>,
}

/// An edge in the dependency graph representing an import relationship.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeEdge {
    /// Import string as it appears in the source code.
    pub raw_import: String,
    /// Whether the import resolved to a known file or is external.
    pub kind: EdgeKind,
    /// Confidence level, auto-derived from [`EdgeKind`].
    #[serde(default = "default_edge_confidence")]
    pub confidence: EdgeConfidence,
}

fn default_edge_confidence() -> EdgeConfidence {
    EdgeConfidence::Verified
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
    /// Number of call-site edges added by [`CallExtractor`].
    pub call_edges: usize,
    /// Number of doc-to-code edges added by `add_doc_edges`.
    pub doc_edges: usize,
    /// Number of nodes in the symbol-level graph.
    pub symbol_nodes: usize,
    /// Number of edges in the symbol-level graph.
    pub symbol_edges: usize,
    /// Confidence breakdown: (verified, high, medium, low).
    pub confidence_counts: (usize, usize, usize, usize),
}

/// In-memory dependency graph over source files.
///
/// Wraps a petgraph `DiGraph` with a path→NodeIndex lookup table so callers
/// can work with file paths rather than opaque indices.
///
/// Also contains an optional symbol-level graph (`inner`) that tracks
/// fine-grained symbol→symbol references (calls, type refs, imports).
pub struct CodeGraph {
    graph: DiGraph<CodeNode, CodeEdge>,
    path_to_node: HashMap<String, NodeIndex>,
    /// Symbol-level directed graph: nodes are [`SymbolNode`]s, edges are
    /// [`ReferenceKind`]s. Used by context assembly and precise callers/callees.
    pub(crate) inner: DiGraph<types::SymbolNode, types::ReferenceKind>,
}

impl CodeGraph {
    /// Create an empty graph.
    pub fn new() -> Self {
        Self {
            graph: DiGraph::new(),
            path_to_node: HashMap::new(),
            inner: DiGraph::new(),
        }
    }

    /// Add a symbol node to the symbol-level graph, returning its index.
    pub fn add_symbol(&mut self, name: &str, file: &str, kind: types::SymbolKind) -> NodeIndex {
        self.inner.add_node(types::SymbolNode {
            name: name.to_string(),
            file: file.to_string(),
            kind,
            line: None,
        })
    }

    /// Add a symbol node with a line number to the symbol-level graph.
    pub fn add_symbol_with_line(
        &mut self,
        name: &str,
        file: &str,
        kind: types::SymbolKind,
        line: usize,
    ) -> NodeIndex {
        self.inner.add_node(types::SymbolNode {
            name: name.to_string(),
            file: file.to_string(),
            kind,
            line: Some(line),
        })
    }

    /// Add a reference edge to the symbol-level graph.
    pub fn add_reference(&mut self, from: NodeIndex, to: NodeIndex, kind: types::ReferenceKind) {
        self.inner.add_edge(from, to, kind);
    }

    /// Return the number of nodes in the symbol-level graph.
    pub fn node_count(&self) -> usize {
        self.inner.node_count()
    }

    /// Return the number of edges in the symbol-level graph.
    pub fn edge_count(&self) -> usize {
        self.inner.edge_count()
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
            community: None,
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
        let kind = EdgeKind::Resolved;
        let confidence = kind.default_confidence();
        self.graph.add_edge(
            from_idx,
            to_idx,
            CodeEdge {
                raw_import: raw_import.to_string(),
                kind,
                confidence,
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
        let kind = EdgeKind::External;
        let confidence = kind.default_confidence();
        self.graph.add_edge(
            from_idx,
            to_idx,
            CodeEdge {
                raw_import: raw_import.to_string(),
                kind,
                confidence,
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

    /// Find files under `from_prefix` that import any file under `to_prefix`.
    ///
    /// Answers module-level cross-package queries like "which gateway files
    /// import from the security module?" in a single pass over the edge list.
    ///
    /// Both prefixes are matched with `starts_with`, so `"src/gateway"` matches
    /// `"src/gateway/server.ts"` and `"src/gateway/hooks.ts"`.
    pub fn cross_imports(&self, from_prefix: &str, to_prefix: &str) -> Vec<String> {
        let mut result = std::collections::HashSet::new();
        for edge in self.graph.edge_references() {
            let source = &self.graph[edge.source()];
            let target = &self.graph[edge.target()];
            if source.file_path.starts_with(from_prefix)
                && target.file_path.starts_with(to_prefix)
                && !source.file_path.starts_with("__ext__:")
            {
                result.insert(source.file_path.clone());
            }
        }
        let mut sorted: Vec<String> = result.into_iter().collect();
        sorted.sort();
        sorted
    }

    /// Find files under `from_prefix` that import any file under `to_prefix`, ranked by relevance.
    ///
    /// Score = sum of target PageRank values for each cross-import edge, multiplied by
    /// a recency boost: `1 + exp(-0.05 * days_old)` for the source file.
    ///
    /// Returns `(file_path, score)` pairs sorted by score descending.
    pub fn cross_imports_ranked(
        &self,
        from_prefix: &str,
        to_prefix: &str,
        recency_map: Option<&std::collections::HashMap<String, i64>>,
        limit: Option<usize>,
    ) -> Vec<(String, f32)> {
        let mut scores: std::collections::HashMap<String, f32> = std::collections::HashMap::new();

        for edge in self.graph.edge_references() {
            let source = &self.graph[edge.source()];
            let target = &self.graph[edge.target()];

            if source.file_path.starts_with(from_prefix)
                && target.file_path.starts_with(to_prefix)
                && !source.file_path.starts_with("__ext__:")
            {
                let target_pr = target.pagerank.max(0.001);
                let entry = scores.entry(source.file_path.clone()).or_insert(0.0);
                *entry += target_pr;
            }
        }

        // Apply recency boost per source file.
        if let Some(rmap) = recency_map {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

            for (file, score) in scores.iter_mut() {
                if let Some(&commit_ts) = rmap.get(file) {
                    let days_old = ((now - commit_ts) as f64 / 86400.0).max(0.0);
                    let boost = (-0.05 * days_old).exp();
                    *score *= 1.0 + boost as f32;
                }
            }
        }

        let mut ranked: Vec<(String, f32)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });

        if let Some(lim) = limit {
            ranked.truncate(lim);
        }

        ranked
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

    /// Add a call-site edge between two files.
    ///
    /// Unlike import edges, call edges represent actual function invocations
    /// (as resolved by the symbol table after the parallel parse phase).
    pub fn add_call_edge(
        &mut self,
        from: &str,
        to: &str,
        callee_name: &str,
        from_lang: Language,
        to_lang: Language,
    ) {
        let from_idx = self.get_or_insert_node(from, from_lang);
        let to_idx = self.get_or_insert_node(to, to_lang);
        let kind = EdgeKind::Calls;
        let confidence = kind.default_confidence();
        self.graph.add_edge(
            from_idx,
            to_idx,
            CodeEdge {
                raw_import: callee_name.to_string(),
                kind,
                confidence,
            },
        );
        if let Some(n) = self.graph.node_weight_mut(from_idx) {
            n.out_degree += 1;
        }
        if let Some(n) = self.graph.node_weight_mut(to_idx) {
            n.in_degree += 1;
        }
    }

    /// Add a documentation-to-code edge between a doc file and a code file.
    ///
    /// These edges represent that the doc file references a symbol defined in
    /// the target code file (e.g., a backtick identifier in Markdown).
    pub fn add_doc_edge(
        &mut self,
        from: &str,
        to: &str,
        symbol_name: &str,
        from_lang: Language,
        to_lang: Language,
    ) {
        let from_idx = self.get_or_insert_node(from, from_lang);
        let to_idx = self.get_or_insert_node(to, to_lang);
        let kind = EdgeKind::DocumentedBy;
        let confidence = kind.default_confidence();
        self.graph.add_edge(
            from_idx,
            to_idx,
            CodeEdge {
                raw_import: symbol_name.to_string(),
                kind,
                confidence,
            },
        );
        if let Some(n) = self.graph.node_weight_mut(from_idx) {
            n.out_degree += 1;
        }
        if let Some(n) = self.graph.node_weight_mut(to_idx) {
            n.in_degree += 1;
        }
    }

    /// Compute graph statistics.
    pub fn stats(&self) -> GraphStats {
        let mut resolved = 0usize;
        let mut external = 0usize;
        let mut calls = 0usize;
        let mut docs = 0usize;
        let mut conf_verified = 0usize;
        let mut conf_high = 0usize;
        let mut conf_medium = 0usize;
        let mut conf_low = 0usize;
        for e in self.graph.edge_weights() {
            match e.kind {
                EdgeKind::Resolved => resolved += 1,
                EdgeKind::External => external += 1,
                EdgeKind::Calls => calls += 1,
                EdgeKind::DocumentedBy => docs += 1,
            }
            match e.confidence {
                EdgeConfidence::Verified => conf_verified += 1,
                EdgeConfidence::High => conf_high += 1,
                EdgeConfidence::Medium => conf_medium += 1,
                EdgeConfidence::Low => conf_low += 1,
            }
        }
        GraphStats {
            node_count: self.graph.node_count(),
            edge_count: self.graph.edge_count(),
            resolved_edges: resolved,
            external_edges: external,
            call_edges: calls,
            doc_edges: docs,
            symbol_nodes: self.inner.node_count(),
            symbol_edges: self.inner.edge_count(),
            confidence_counts: (conf_verified, conf_high, conf_medium, conf_low),
        }
    }

    /// Iterate over all call edges as `(caller_file, callee_name)` tuples.
    pub fn call_edges(&self) -> Vec<(String, String)> {
        self.graph
            .edge_references()
            .filter(|e| e.weight().kind == EdgeKind::Calls)
            .map(|e| {
                let caller = self.graph[e.source()].file_path.clone();
                let callee_name = e.weight().raw_import.clone();
                (caller, callee_name)
            })
            .collect()
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

    /// Query the symbol-level graph for callers of `symbol_name`.
    ///
    /// Returns `(file, caller_symbol_name)` pairs for every symbol that has a
    /// `Call` edge pointing to a node whose name matches `symbol_name`.
    pub fn get_symbol_callers(&self, symbol_name: &str) -> Vec<(String, String)> {
        // Find all nodes matching the target symbol name.
        let target_indices: Vec<NodeIndex> = self
            .inner
            .node_indices()
            .filter(|&idx| {
                self.inner
                    .node_weight(idx)
                    .is_some_and(|n| n.name == symbol_name)
            })
            .collect();

        if target_indices.is_empty() {
            return Vec::new();
        }

        let mut callers = Vec::new();
        for &target in &target_indices {
            for edge in self
                .inner
                .edges_directed(target, petgraph::Direction::Incoming)
            {
                if *edge.weight() == types::ReferenceKind::Call {
                    if let Some(caller_node) = self.inner.node_weight(edge.source()) {
                        callers.push((caller_node.file.clone(), caller_node.name.clone()));
                    }
                }
            }
        }
        callers
    }

    /// Query the symbol-level graph for callees of `symbol_name`.
    ///
    /// Returns the names of all symbols that have a `Call` edge FROM
    /// a node whose name matches `symbol_name`.
    pub fn get_symbol_callees(&self, symbol_name: &str) -> Vec<String> {
        let source_indices: Vec<NodeIndex> = self
            .inner
            .node_indices()
            .filter(|&idx| {
                self.inner
                    .node_weight(idx)
                    .is_some_and(|n| n.name == symbol_name)
            })
            .collect();

        if source_indices.is_empty() {
            return Vec::new();
        }

        let mut callees = Vec::new();
        for &src in &source_indices {
            for edge in self
                .inner
                .edges_directed(src, petgraph::Direction::Outgoing)
            {
                if *edge.weight() == types::ReferenceKind::Call {
                    if let Some(target_node) = self.inner.node_weight(edge.target()) {
                        callees.push(target_node.name.clone());
                    }
                }
            }
        }
        callees.sort();
        callees.dedup();
        callees
    }

    /// Return the number of nodes in the symbol-level inner graph.
    pub fn symbol_node_count(&self) -> usize {
        self.inner.node_count()
    }

    /// Remove all symbol nodes (and their edges) for a given file.
    ///
    /// This is used during incremental reindex: before re-extracting
    /// definitions and call edges for a file, we remove the old ones.
    pub fn remove_file_symbols(&mut self, file: &str) {
        let to_remove: Vec<NodeIndex> = self
            .inner
            .node_indices()
            .filter(|&idx| self.inner.node_weight(idx).is_some_and(|n| n.file == file))
            .collect();
        for idx in to_remove.into_iter().rev() {
            self.inner.remove_node(idx);
        }
    }

    // -----------------------------------------------------------------
    // Graph analysis methods
    // -----------------------------------------------------------------

    /// Find the shortest path between two files using BFS.
    ///
    /// Returns `None` if either file is missing or no path exists.
    /// The returned vector includes both endpoints.
    pub fn shortest_path(&self, from: &str, to: &str) -> Option<Vec<String>> {
        let &from_idx = self.path_to_node.get(from)?;
        let &to_idx = self.path_to_node.get(to)?;

        if from_idx == to_idx {
            return Some(vec![from.to_string()]);
        }

        // BFS tracking predecessors.
        let mut visited: HashMap<NodeIndex, Option<NodeIndex>> = HashMap::new();
        visited.insert(from_idx, None);
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(from_idx);

        while let Some(current) = queue.pop_front() {
            if current == to_idx {
                // Reconstruct path.
                let mut path = Vec::new();
                let mut cur = Some(to_idx);
                while let Some(idx) = cur {
                    if let Some(node) = self.graph.node_weight(idx) {
                        path.push(node.file_path.clone());
                    }
                    cur = visited.get(&idx).copied().flatten();
                }
                path.reverse();
                return Some(path);
            }

            // Traverse both outgoing and incoming edges (undirected BFS).
            let neighbors: Vec<NodeIndex> = self
                .graph
                .neighbors_directed(current, petgraph::Direction::Outgoing)
                .chain(
                    self.graph
                        .neighbors_directed(current, petgraph::Direction::Incoming),
                )
                .collect();

            for nb in neighbors {
                if let std::collections::hash_map::Entry::Vacant(e) = visited.entry(nb) {
                    e.insert(Some(current));
                    queue.push_back(nb);
                }
            }
        }

        None // No path found
    }

    /// Run Louvain community detection and store results on nodes.
    pub fn detect_communities(&mut self) -> CommunityResult {
        let result = community::detect_communities(self);
        // Store community assignments on nodes.
        for (path, &community_id) in &result.assignments {
            if let Some(&idx) = self.path_to_node.get(path.as_str()) {
                if let Some(node) = self.graph.node_weight_mut(idx) {
                    node.community = Some(community_id);
                }
            }
        }
        result
    }

    // -----------------------------------------------------------------
    // Internal accessors used by community / surprise / html_export modules.
    // -----------------------------------------------------------------

    /// Return all real (non-external) file paths.
    pub fn file_paths(&self) -> Vec<String> {
        self.graph
            .node_weights()
            .filter(|n| !n.file_path.starts_with("__ext__:"))
            .map(|n| n.file_path.clone())
            .collect()
    }

    /// Return the number of file-level nodes (including external).
    pub fn file_node_count(&self) -> usize {
        self.graph.node_count()
    }

    /// Iterate over all edges as `(from_path, to_path, edge_ref)` triples.
    pub fn all_edges(&self) -> Vec<(&str, &str, &CodeEdge)> {
        self.graph
            .edge_references()
            .filter_map(|e| {
                let from = self.graph.node_weight(e.source())?;
                let to = self.graph.node_weight(e.target())?;
                Some((from.file_path.as_str(), to.file_path.as_str(), e.weight()))
            })
            .collect()
    }

    /// Return the set of neighbor paths (both directions) for a given path,
    /// ignoring external nodes. Used by Louvain community detection.
    pub fn undirected_neighbors(&self, file_path: &str) -> Vec<String> {
        let Some(&idx) = self.path_to_node.get(file_path) else {
            return Vec::new();
        };
        let mut seen = std::collections::HashSet::new();
        let incoming = self
            .graph
            .neighbors_directed(idx, petgraph::Direction::Incoming);
        let outgoing = self
            .graph
            .neighbors_directed(idx, petgraph::Direction::Outgoing);
        for nb in incoming.chain(outgoing) {
            if let Some(node) = self.graph.node_weight(nb) {
                if !node.file_path.starts_with("__ext__:") {
                    seen.insert(node.file_path.clone());
                }
            }
        }
        seen.into_iter().collect()
    }

    /// Total number of edges between real (non-external) nodes.
    pub fn real_edge_count(&self) -> usize {
        self.graph
            .edge_references()
            .filter(|e| {
                let src = &self.graph[e.source()];
                let tgt = &self.graph[e.target()];
                !src.file_path.starts_with("__ext__:") && !tgt.file_path.starts_with("__ext__:")
            })
            .count()
    }

    /// Count edges between two specific file paths (in either direction).
    pub fn edges_between(&self, a: &str, b: &str) -> usize {
        let (Some(&a_idx), Some(&b_idx)) = (self.path_to_node.get(a), self.path_to_node.get(b))
        else {
            return 0;
        };
        self.graph
            .edge_references()
            .filter(|e| {
                (e.source() == a_idx && e.target() == b_idx)
                    || (e.source() == b_idx && e.target() == a_idx)
            })
            .count()
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

    #[test]
    fn symbol_callers_returns_call_edges() {
        let mut g = CodeGraph::new();
        let main_fn = g.add_symbol_with_line("main", "src/main.rs", types::SymbolKind::Function, 0);
        let helper = g.add_symbol_with_line("helper", "src/lib.rs", types::SymbolKind::Function, 5);
        let process =
            g.add_symbol_with_line("process", "src/engine.rs", types::SymbolKind::Function, 10);
        g.add_reference(main_fn, helper, types::ReferenceKind::Call);
        g.add_reference(process, helper, types::ReferenceKind::Call);

        let callers = g.get_symbol_callers("helper");
        assert_eq!(callers.len(), 2);
        // Callers should be (file, symbol_name) pairs.
        let names: Vec<&str> = callers.iter().map(|(_, n)| n.as_str()).collect();
        assert!(names.contains(&"main"));
        assert!(names.contains(&"process"));
    }

    #[test]
    fn symbol_callees_returns_call_edges() {
        let mut g = CodeGraph::new();
        let main_fn = g.add_symbol_with_line("main", "src/main.rs", types::SymbolKind::Function, 0);
        let helper = g.add_symbol_with_line("helper", "src/lib.rs", types::SymbolKind::Function, 5);
        let process =
            g.add_symbol_with_line("process", "src/engine.rs", types::SymbolKind::Function, 10);
        g.add_reference(main_fn, helper, types::ReferenceKind::Call);
        g.add_reference(main_fn, process, types::ReferenceKind::Call);

        let callees = g.get_symbol_callees("main");
        assert_eq!(callees.len(), 2);
        assert!(callees.contains(&"helper".to_string()));
        assert!(callees.contains(&"process".to_string()));
    }

    #[test]
    fn symbol_node_count_tracks_additions() {
        let mut g = CodeGraph::new();
        assert_eq!(g.symbol_node_count(), 0);
        g.add_symbol_with_line("foo", "a.rs", types::SymbolKind::Function, 0);
        assert_eq!(g.symbol_node_count(), 1);
        g.add_symbol_with_line("bar", "b.rs", types::SymbolKind::Function, 5);
        assert_eq!(g.symbol_node_count(), 2);
    }

    #[test]
    fn remove_file_symbols_cleans_up() {
        let mut g = CodeGraph::new();
        g.add_symbol_with_line("foo", "a.rs", types::SymbolKind::Function, 0);
        g.add_symbol_with_line("bar", "a.rs", types::SymbolKind::Function, 10);
        g.add_symbol_with_line("baz", "b.rs", types::SymbolKind::Function, 0);
        assert_eq!(g.symbol_node_count(), 3);

        g.remove_file_symbols("a.rs");
        assert_eq!(g.symbol_node_count(), 1);
    }

    #[test]
    fn stats_includes_symbol_counts() {
        let mut g = CodeGraph::new();
        let a = g.add_symbol_with_line("a", "a.rs", types::SymbolKind::Function, 0);
        let b = g.add_symbol_with_line("b", "b.rs", types::SymbolKind::Function, 5);
        g.add_reference(a, b, types::ReferenceKind::Call);
        let s = g.stats();
        assert_eq!(s.symbol_nodes, 2);
        assert_eq!(s.symbol_edges, 1);
    }

    #[test]
    fn cross_imports_ranked_by_pagerank() {
        let mut g = CodeGraph::new();
        // Two gateway files import from two target files with different PageRank.
        g.add_edge(
            "src/gateway/a.rs",
            "src/auth/high_rank.rs",
            "crate::auth::high_rank",
            Language::Rust,
            Language::Rust,
        );
        g.add_edge(
            "src/gateway/b.rs",
            "src/auth/low_rank.rs",
            "crate::auth::low_rank",
            Language::Rust,
            Language::Rust,
        );
        // Assign pagerank: high_rank.rs gets 0.5, low_rank.rs gets 0.001 (min).
        let mut scores = std::collections::HashMap::new();
        scores.insert("src/auth/high_rank.rs".to_string(), 0.5f32);
        scores.insert("src/auth/low_rank.rs".to_string(), 0.001f32);
        g.apply_pagerank(&scores);

        let ranked = g.cross_imports_ranked("src/gateway", "src/auth", None, None);
        assert_eq!(ranked.len(), 2);
        // gateway/a.rs imports the high-rank target, so it should score higher.
        assert_eq!(ranked[0].0, "src/gateway/a.rs");
        assert_eq!(ranked[1].0, "src/gateway/b.rs");
        assert!(ranked[0].1 > ranked[1].1);
    }

    #[test]
    fn cross_imports_ranked_respects_limit() {
        let mut g = CodeGraph::new();
        for i in 0..5 {
            g.add_edge(
                &format!("src/gateway/file_{i}.rs"),
                "src/auth/mod.rs",
                "crate::auth",
                Language::Rust,
                Language::Rust,
            );
        }

        let ranked = g.cross_imports_ranked("src/gateway", "src/auth", None, Some(3));
        assert_eq!(ranked.len(), 3);
    }

    // --- Edge confidence tests ---

    #[test]
    fn edge_confidence_auto_derived_from_kind() {
        assert_eq!(
            EdgeKind::Resolved.default_confidence(),
            EdgeConfidence::Verified
        );
        assert_eq!(EdgeKind::Calls.default_confidence(), EdgeConfidence::High);
        assert_eq!(
            EdgeKind::DocumentedBy.default_confidence(),
            EdgeConfidence::Medium
        );
        assert_eq!(EdgeKind::External.default_confidence(), EdgeConfidence::Low);
    }

    #[test]
    fn add_edge_sets_confidence() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/a.rs",
            "src/b.rs",
            "crate::b",
            Language::Rust,
            Language::Rust,
        );
        let flat = g.to_flat();
        assert_eq!(flat.edges.len(), 1);
        assert_eq!(flat.edges[0].2.confidence, EdgeConfidence::Verified);
    }

    #[test]
    fn call_edge_has_high_confidence() {
        let mut g = CodeGraph::new();
        g.add_call_edge(
            "src/a.rs",
            "src/b.rs",
            "foo",
            Language::Rust,
            Language::Rust,
        );
        let flat = g.to_flat();
        assert_eq!(flat.edges[0].2.confidence, EdgeConfidence::High);
    }

    #[test]
    fn doc_edge_has_medium_confidence() {
        let mut g = CodeGraph::new();
        g.add_doc_edge(
            "docs/guide.md",
            "src/engine.rs",
            "Engine",
            Language::Rust,
            Language::Rust,
        );
        let flat = g.to_flat();
        assert_eq!(flat.edges[0].2.confidence, EdgeConfidence::Medium);
    }

    #[test]
    fn stats_includes_confidence_counts() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/a.rs",
            "src/b.rs",
            "crate::b",
            Language::Rust,
            Language::Rust,
        );
        g.add_call_edge(
            "src/a.rs",
            "src/c.rs",
            "foo",
            Language::Rust,
            Language::Rust,
        );
        g.add_doc_edge("docs/x.md", "src/a.rs", "A", Language::Rust, Language::Rust);
        g.add_external_edge("src/a.rs", "serde", Language::Rust);

        let stats = g.stats();
        assert_eq!(stats.confidence_counts, (1, 1, 1, 1));
    }

    // --- Shortest path tests ---

    #[test]
    fn shortest_path_direct_edge() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/a.rs",
            "src/b.rs",
            "crate::b",
            Language::Rust,
            Language::Rust,
        );

        let path = g.shortest_path("src/a.rs", "src/b.rs");
        assert_eq!(
            path,
            Some(vec!["src/a.rs".to_string(), "src/b.rs".to_string()])
        );
    }

    #[test]
    fn shortest_path_multi_hop() {
        let mut g = CodeGraph::new();
        g.add_edge("src/a.rs", "src/b.rs", "b", Language::Rust, Language::Rust);
        g.add_edge("src/b.rs", "src/c.rs", "c", Language::Rust, Language::Rust);

        let path = g.shortest_path("src/a.rs", "src/c.rs");
        assert_eq!(
            path,
            Some(vec![
                "src/a.rs".to_string(),
                "src/b.rs".to_string(),
                "src/c.rs".to_string()
            ])
        );
    }

    #[test]
    fn shortest_path_same_node() {
        let mut g = CodeGraph::new();
        g.get_or_insert_node("src/a.rs", Language::Rust);
        let path = g.shortest_path("src/a.rs", "src/a.rs");
        assert_eq!(path, Some(vec!["src/a.rs".to_string()]));
    }

    #[test]
    fn shortest_path_no_path() {
        let mut g = CodeGraph::new();
        g.get_or_insert_node("src/a.rs", Language::Rust);
        g.get_or_insert_node("src/b.rs", Language::Rust);
        // No edge between them.
        let path = g.shortest_path("src/a.rs", "src/b.rs");
        assert_eq!(path, None);
    }

    #[test]
    fn shortest_path_missing_node() {
        let g = CodeGraph::new();
        assert_eq!(g.shortest_path("nonexistent.rs", "also_missing.rs"), None);
    }

    #[test]
    fn shortest_path_reverse_direction() {
        let mut g = CodeGraph::new();
        // Edge goes a -> b, but BFS is undirected so b -> a should work too.
        g.add_edge("src/a.rs", "src/b.rs", "b", Language::Rust, Language::Rust);
        let path = g.shortest_path("src/b.rs", "src/a.rs");
        assert_eq!(
            path,
            Some(vec!["src/b.rs".to_string(), "src/a.rs".to_string()])
        );
    }

    // --- Community field on node ---

    #[test]
    fn community_defaults_to_none() {
        let mut g = CodeGraph::new();
        g.get_or_insert_node("src/a.rs", Language::Rust);
        assert_eq!(g.node("src/a.rs").unwrap().community, None);
    }
}

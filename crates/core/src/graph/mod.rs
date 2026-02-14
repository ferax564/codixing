pub mod extract;
pub mod types;
pub use extract::{DefinitionInfo, ReferenceInfo};
pub use types::*;

use petgraph::Direction;
use petgraph::graph::{DiGraph, NodeIndex};

/// A directed graph of code symbols and their references.
///
/// `CodeGraph` wraps a `petgraph::DiGraph` where nodes are [`SymbolNode`]s
/// and edges are [`ReferenceKind`]s. It supports querying callers and callees
/// of any symbol.
pub struct CodeGraph {
    inner: DiGraph<SymbolNode, ReferenceKind>,
}

impl CodeGraph {
    /// Create a new, empty code graph.
    pub fn new() -> Self {
        Self {
            inner: DiGraph::new(),
        }
    }

    /// Return the number of symbol nodes in the graph.
    pub fn node_count(&self) -> usize {
        self.inner.node_count()
    }

    /// Return the number of reference edges in the graph.
    pub fn edge_count(&self) -> usize {
        self.inner.edge_count()
    }

    /// Add a symbol node to the graph and return its index.
    pub fn add_symbol(&mut self, name: &str, file: &str, kind: SymbolKind) -> NodeIndex {
        self.inner.add_node(SymbolNode {
            name: name.to_string(),
            file: file.to_string(),
            kind,
            line: None,
        })
    }

    /// Look up a node by its index.
    pub fn get_node(&self, id: NodeIndex) -> Option<&SymbolNode> {
        self.inner.node_weight(id)
    }

    /// Add a directed reference edge from one symbol to another.
    pub fn add_reference(&mut self, from: NodeIndex, to: NodeIndex, kind: ReferenceKind) {
        self.inner.add_edge(from, to, kind);
    }

    /// Return all symbols that the given node references (outgoing edges).
    pub fn callees(&self, id: NodeIndex) -> Vec<&SymbolNode> {
        self.inner
            .neighbors_directed(id, Direction::Outgoing)
            .filter_map(|n| self.inner.node_weight(n))
            .collect()
    }

    /// Return all symbols that reference the given node (incoming edges).
    pub fn callers(&self, id: NodeIndex) -> Vec<&SymbolNode> {
        self.inner
            .neighbors_directed(id, Direction::Incoming)
            .filter_map(|n| self.inner.node_weight(n))
            .collect()
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
    fn empty_graph_has_no_nodes() {
        let g = CodeGraph::new();
        assert_eq!(g.node_count(), 0);
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn add_symbol_node() {
        let mut g = CodeGraph::new();
        let id = g.add_symbol("main", "src/main.rs", SymbolKind::Function);
        assert_eq!(g.node_count(), 1);
        let node = g.get_node(id).unwrap();
        assert_eq!(node.name, "main");
        assert_eq!(node.file, "src/main.rs");
        assert_eq!(node.kind, SymbolKind::Function);
        assert_eq!(node.line, None);
    }

    #[test]
    fn add_reference_edge() {
        let mut g = CodeGraph::new();
        let a = g.add_symbol("main", "src/main.rs", SymbolKind::Function);
        let b = g.add_symbol("Config", "src/config.rs", SymbolKind::Struct);
        g.add_reference(a, b, ReferenceKind::TypeRef);
        assert_eq!(g.edge_count(), 1);
    }

    #[test]
    fn callers_and_callees() {
        let mut g = CodeGraph::new();
        let main_fn = g.add_symbol("main", "src/main.rs", SymbolKind::Function);
        let helper = g.add_symbol("helper", "src/lib.rs", SymbolKind::Function);
        let config = g.add_symbol("Config", "src/config.rs", SymbolKind::Struct);

        // main calls helper, main references Config
        g.add_reference(main_fn, helper, ReferenceKind::Call);
        g.add_reference(main_fn, config, ReferenceKind::TypeRef);

        // callees of main: helper and Config
        let callees = g.callees(main_fn);
        assert_eq!(callees.len(), 2);
        let callee_names: Vec<&str> = callees.iter().map(|n| n.name.as_str()).collect();
        assert!(callee_names.contains(&"helper"));
        assert!(callee_names.contains(&"Config"));

        // callers of helper: just main
        let callers = g.callers(helper);
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].name, "main");

        // callers of main: none
        let main_callers = g.callers(main_fn);
        assert!(main_callers.is_empty());
    }
}

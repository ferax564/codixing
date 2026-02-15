use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{CodeGraph, ReferenceKind, SymbolNode};
use crate::error::{CodeforgeError, Result};

// ---------------------------------------------------------------------------
// Binary (bitcode) persistence
// ---------------------------------------------------------------------------

/// Save a `CodeGraph` to a binary (bitcode) file.
///
/// Significantly faster and more compact than JSON for large graphs.
pub fn save_graph_binary(graph: &CodeGraph, path: &Path) -> Result<()> {
    let sg = graph.to_serializable();
    let bytes = bitcode::serialize(&sg)
        .map_err(|e| CodeforgeError::Serialization(format!("failed to serialize graph: {e}")))?;
    fs::write(path, bytes)?;
    Ok(())
}

/// Load a `CodeGraph` from a binary (bitcode) file.
pub fn load_graph_binary(path: &Path) -> Result<CodeGraph> {
    let bytes = fs::read(path)?;
    let sg: SerializableGraph = bitcode::deserialize(&bytes)
        .map_err(|e| CodeforgeError::Serialization(format!("failed to deserialize graph: {e}")))?;
    Ok(CodeGraph::from_serializable(&sg))
}

/// Serializable representation of a `CodeGraph`.
///
/// `petgraph::DiGraph` is not directly serializable with serde, so we convert
/// to/from a flat struct of nodes and edges for JSON persistence.
#[derive(Debug, Serialize, Deserialize)]
struct SerializableGraph {
    nodes: Vec<SymbolNode>,
    edges: Vec<(usize, usize, ReferenceKind)>,
}

/// Save a `CodeGraph` to a JSON file.
pub fn save_graph(graph: &CodeGraph, path: &Path) -> Result<()> {
    let sg = graph.to_serializable();
    let json = serde_json::to_string_pretty(&sg)
        .map_err(|e| CodeforgeError::Serialization(format!("failed to serialize graph: {e}")))?;
    fs::write(path, json)?;
    Ok(())
}

/// Load a `CodeGraph` from a JSON file.
pub fn load_graph(path: &Path) -> Result<CodeGraph> {
    let json = fs::read_to_string(path)?;
    let sg: SerializableGraph = serde_json::from_str(&json)
        .map_err(|e| CodeforgeError::Serialization(format!("failed to deserialize graph: {e}")))?;
    Ok(CodeGraph::from_serializable(&sg))
}

impl CodeGraph {
    /// Convert the graph into a serializable representation.
    fn to_serializable(&self) -> SerializableGraph {
        let nodes: Vec<SymbolNode> = self
            .inner
            .node_indices()
            .filter_map(|idx| self.inner.node_weight(idx).cloned())
            .collect();

        let edges: Vec<(usize, usize, ReferenceKind)> = self
            .inner
            .edge_indices()
            .filter_map(|idx| {
                let (src, dst) = self.inner.edge_endpoints(idx)?;
                let weight = self.inner.edge_weight(idx)?;
                Some((src.index(), dst.index(), weight.clone()))
            })
            .collect();

        SerializableGraph { nodes, edges }
    }

    /// Reconstruct a `CodeGraph` from a serializable representation.
    fn from_serializable(sg: &SerializableGraph) -> Self {
        use petgraph::graph::NodeIndex;

        let mut graph = CodeGraph::new();
        for node in &sg.nodes {
            graph.inner.add_node(node.clone());
        }
        for (src, dst, kind) in &sg.edges {
            let src_idx = NodeIndex::new(*src);
            let dst_idx = NodeIndex::new(*dst);
            graph.inner.add_edge(src_idx, dst_idx, kind.clone());
        }
        graph
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::SymbolKind;
    use tempfile::tempdir;

    #[test]
    fn save_and_load_round_trip() {
        let mut graph = CodeGraph::new();
        let a = graph.add_symbol("main", "src/main.rs", SymbolKind::Function);
        let b = graph.add_symbol("helper", "src/lib.rs", SymbolKind::Function);
        graph.add_reference(a, b, ReferenceKind::Call);

        let dir = tempdir().unwrap();
        let path = dir.path().join("graph.json");

        save_graph(&graph, &path).unwrap();
        let loaded = load_graph(&path).unwrap();

        assert_eq!(loaded.node_count(), 2);
        assert_eq!(loaded.edge_count(), 1);
    }

    #[test]
    fn empty_graph_round_trip() {
        let graph = CodeGraph::new();
        let dir = tempdir().unwrap();
        let path = dir.path().join("graph.json");

        save_graph(&graph, &path).unwrap();
        let loaded = load_graph(&path).unwrap();

        assert_eq!(loaded.node_count(), 0);
        assert_eq!(loaded.edge_count(), 0);
    }

    #[test]
    fn load_nonexistent_returns_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");

        let result = load_graph(&path);
        assert!(result.is_err());
    }

    #[test]
    fn binary_save_and_load_round_trip() {
        let mut graph = CodeGraph::new();
        let a = graph.add_symbol("main", "src/main.rs", SymbolKind::Function);
        let b = graph.add_symbol("helper", "src/lib.rs", SymbolKind::Function);
        graph.add_reference(a, b, ReferenceKind::Call);

        let dir = tempdir().unwrap();
        let path = dir.path().join("graph.bin");

        save_graph_binary(&graph, &path).unwrap();
        let loaded = load_graph_binary(&path).unwrap();

        assert_eq!(loaded.node_count(), 2);
        assert_eq!(loaded.edge_count(), 1);
    }

    #[test]
    fn binary_empty_graph_round_trip() {
        let graph = CodeGraph::new();
        let dir = tempdir().unwrap();
        let path = dir.path().join("graph.bin");

        save_graph_binary(&graph, &path).unwrap();
        let loaded = load_graph_binary(&path).unwrap();

        assert_eq!(loaded.node_count(), 0);
        assert_eq!(loaded.edge_count(), 0);
    }

    #[test]
    fn binary_smaller_than_json() {
        let mut graph = CodeGraph::new();
        for i in 0..100 {
            let a = graph.add_symbol(
                &format!("fn_{i}"),
                &format!("src/mod_{i}.rs"),
                SymbolKind::Function,
            );
            if i > 0 {
                let prev = petgraph::graph::NodeIndex::new(i - 1);
                graph.add_reference(prev, a, ReferenceKind::Call);
            }
        }

        let dir = tempdir().unwrap();
        let json_path = dir.path().join("graph.json");
        let bin_path = dir.path().join("graph.bin");

        save_graph(&graph, &json_path).unwrap();
        save_graph_binary(&graph, &bin_path).unwrap();

        let json_size = std::fs::metadata(&json_path).unwrap().len();
        let bin_size = std::fs::metadata(&bin_path).unwrap().len();

        assert!(
            bin_size < json_size,
            "binary ({bin_size}) should be smaller than JSON ({json_size})"
        );
    }
}

use std::collections::HashMap;

use petgraph::graph::NodeIndex;

use super::CodeGraph;

impl CodeGraph {
    /// Compute PageRank scores for all nodes.
    ///
    /// Standard iterative PageRank: PR(v) = (1-d)/N + d * Σ(PR(u)/out(u)) for all u→v edges.
    /// `damping` is typically 0.85, `iterations` typically 20.
    pub fn pagerank(&self, damping: f64, iterations: usize) -> HashMap<NodeIndex, f64> {
        let n = self.inner.node_count();
        if n == 0 {
            return HashMap::new();
        }

        let initial = 1.0 / n as f64;
        let mut scores: HashMap<NodeIndex, f64> =
            self.inner.node_indices().map(|idx| (idx, initial)).collect();

        for _ in 0..iterations {
            let mut new_scores: HashMap<NodeIndex, f64> = self
                .inner
                .node_indices()
                .map(|idx| (idx, (1.0 - damping) / n as f64))
                .collect();

            for node in self.inner.node_indices() {
                let out_degree = self
                    .inner
                    .neighbors_directed(node, petgraph::Direction::Outgoing)
                    .count();
                if out_degree == 0 {
                    continue;
                }

                let contribution = scores[&node] / out_degree as f64;
                for neighbor in self
                    .inner
                    .neighbors_directed(node, petgraph::Direction::Outgoing)
                {
                    *new_scores.get_mut(&neighbor).unwrap() += damping * contribution;
                }
            }

            scores = new_scores;
        }

        scores
    }
}

#[cfg(test)]
mod tests {
    use super::super::{CodeGraph, ReferenceKind, SymbolKind};

    #[test]
    fn pagerank_scores_hub_symbols_higher() {
        let mut graph = CodeGraph::new();
        let hub = graph.add_symbol("core::process", "core.rs", SymbolKind::Function);
        let a = graph.add_symbol("handler::a", "handler.rs", SymbolKind::Function);
        let b = graph.add_symbol("handler::b", "handler.rs", SymbolKind::Function);
        let c = graph.add_symbol("handler::c", "handler.rs", SymbolKind::Function);
        graph.add_reference(a, hub, ReferenceKind::Call);
        graph.add_reference(b, hub, ReferenceKind::Call);
        graph.add_reference(c, hub, ReferenceKind::Call);

        let scores = graph.pagerank(0.85, 20);
        assert!(scores[&hub] > scores[&a]);
        assert!(scores[&hub] > scores[&b]);
    }

    #[test]
    fn pagerank_empty_graph() {
        let graph = CodeGraph::new();
        let scores = graph.pagerank(0.85, 20);
        assert!(scores.is_empty());
    }

    #[test]
    fn pagerank_single_node() {
        let mut graph = CodeGraph::new();
        graph.add_symbol("main", "main.rs", SymbolKind::Function);
        let scores = graph.pagerank(0.85, 20);
        assert_eq!(scores.len(), 1);
    }
}

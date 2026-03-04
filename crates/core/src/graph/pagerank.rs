//! Iterative PageRank computation for the code dependency graph.

use std::collections::HashMap;

use super::CodeGraph;

/// Compute PageRank scores for all real (non-external) nodes in `graph`.
///
/// Uses an iterative power method with dangling-node rank redistribution.
/// Scores are normalized so `max = 1.0` for direct use as a multiplicative
/// search boost.
///
/// # Arguments
/// * `damping`    — Damping factor (standard: 0.85).
/// * `iterations` — Maximum number of iterations.
pub fn compute_pagerank(
    graph: &CodeGraph,
    damping: f32,
    iterations: usize,
) -> HashMap<String, f32> {
    // Collect real nodes (skip __ext__ pseudo-nodes).
    let nodes: Vec<&str> = graph
        .nodes_by_pagerank()
        .iter()
        .map(|n| n.file_path.as_str())
        .collect();

    let n = nodes.len();
    if n == 0 {
        return HashMap::new();
    }

    // Initialize rank uniformly.
    let init = 1.0 / n as f32;
    let mut rank: HashMap<&str, f32> = nodes.iter().map(|&p| (p, init)).collect();

    // Pre-compute out-edges per node (to real nodes only).
    let mut out_edges: HashMap<&str, Vec<&str>> = HashMap::new();
    for &path in &nodes {
        let callees: Vec<&str> = graph
            .callees(path)
            .iter()
            .filter_map(|c| nodes.iter().find(|&&n| n == c.as_str()).copied())
            .collect();
        out_edges.insert(path, callees);
    }

    for _ in 0..iterations {
        let mut new_rank: HashMap<&str, f32> = nodes.iter().map(|&p| (p, 0.0)).collect();

        // Dangling rank: nodes with no outgoing edges (to real nodes) redistribute uniformly.
        let dangling_sum: f32 = nodes
            .iter()
            .filter(|&&p| out_edges.get(p).map(|v| v.is_empty()).unwrap_or(true))
            .map(|&p| rank.get(p).copied().unwrap_or(0.0))
            .sum();
        let dangling_contrib = dangling_sum / n as f32;

        // Propagate rank along edges.
        for &from in &nodes {
            let r = rank.get(from).copied().unwrap_or(0.0);
            let outs = out_edges.get(from).cloned().unwrap_or_default();
            if !outs.is_empty() {
                let share = r / outs.len() as f32;
                for to in outs {
                    *new_rank.entry(to).or_insert(0.0) += share;
                }
            }
        }

        // Apply damping and dangling contribution.
        let teleport = (1.0 - damping) / n as f32;
        let mut max_delta = 0.0_f32;
        for &path in &nodes {
            let old = rank.get(path).copied().unwrap_or(0.0);
            let propagated = new_rank.get(path).copied().unwrap_or(0.0);
            let updated = teleport + damping * (propagated + dangling_contrib);
            *new_rank.entry(path).or_insert(0.0) = updated;
            max_delta = max_delta.max((updated - old).abs());
        }

        rank = new_rank;

        // Early convergence.
        if max_delta < 1e-6 {
            break;
        }
    }

    // Normalize so max = 1.0.
    let max_score = rank.values().cloned().fold(0.0_f32, f32::max);
    let result: HashMap<String, f32> = if max_score > 0.0 {
        rank.into_iter()
            .map(|(k, v)| (k.to_string(), v / max_score))
            .collect()
    } else {
        rank.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    };

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::CodeGraph;
    use crate::language::Language;

    #[test]
    fn empty_graph_returns_empty_scores() {
        let g = CodeGraph::new();
        let scores = compute_pagerank(&g, 0.85, 20);
        assert!(scores.is_empty());
    }

    #[test]
    fn single_node_scores_one() {
        let mut g = CodeGraph::new();
        g.get_or_insert_node("src/a.rs", Language::Rust);
        let scores = compute_pagerank(&g, 0.85, 20);
        assert!(scores.contains_key("src/a.rs"));
    }

    #[test]
    fn most_imported_file_scores_highest() {
        let mut g = CodeGraph::new();
        // parser.rs is imported by both main.rs and engine.rs → highest in-degree.
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

        let scores = compute_pagerank(&g, 0.85, 20);

        let parser_score = scores.get("src/parser.rs").copied().unwrap_or(0.0);
        let main_score = scores.get("src/main.rs").copied().unwrap_or(0.0);

        assert!(
            parser_score > main_score,
            "parser.rs (2 in-edges) should outrank main.rs (0), got parser={parser_score}, main={main_score}"
        );
    }

    #[test]
    fn max_score_is_one() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/a.rs",
            "src/b.rs",
            "crate::b",
            Language::Rust,
            Language::Rust,
        );
        g.add_edge(
            "src/c.rs",
            "src/b.rs",
            "crate::b",
            Language::Rust,
            Language::Rust,
        );

        let scores = compute_pagerank(&g, 0.85, 20);
        let max = scores.values().cloned().fold(0.0_f32, f32::max);
        assert!(
            (max - 1.0).abs() < 1e-4,
            "expected max score ≈ 1.0, got {max}"
        );
    }
}

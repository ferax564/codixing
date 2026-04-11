//! Surprise/anomaly detection for dependency graph edges.
//!
//! Scores each edge by how "surprising" or unexpected it is, based on
//! cross-community membership, PageRank disparity, directory distance,
//! and edge confidence level.

use std::collections::HashMap;

use super::{CodeGraph, EdgeConfidence};

/// A dependency edge scored by how surprising it is.
#[derive(Debug, Clone)]
pub struct SurprisingEdge {
    /// Source file path.
    pub from: String,
    /// Target file path.
    pub to: String,
    /// Surprise score in the range 0.0 to 1.0.
    pub score: f32,
    /// Human-readable explanations of each surprise factor.
    pub reasons: Vec<String>,
}

/// Detect the most surprising edges in the graph.
///
/// Returns up to `top_n` edges sorted by surprise score descending.
/// Community detection should be run beforehand for the cross-community
/// factor to contribute (otherwise that factor is skipped).
pub fn detect_surprises(graph: &CodeGraph, top_n: usize) -> Vec<SurprisingEdge> {
    let edges = graph.all_edges();

    // Collect community assignments from nodes.
    let communities: HashMap<String, usize> = graph
        .file_paths()
        .into_iter()
        .filter_map(|p| {
            let node = graph.node(p.as_str())?;
            let comm = node.community?;
            Some((p, comm))
        })
        .collect();
    let has_communities = !communities.is_empty();

    // Compute max log ratio for PageRank disparity normalization.
    let mut max_log_ratio = 0.0_f32;
    for (from, to, _) in &edges {
        if from.starts_with("__ext__:") || to.starts_with("__ext__:") {
            continue;
        }
        if let (Some(src), Some(tgt)) = (graph.node(from), graph.node(to)) {
            let src_pr = src.pagerank.max(1e-6);
            let tgt_pr = tgt.pagerank.max(1e-6);
            let ratio = (src_pr / tgt_pr).ln().abs();
            if ratio > max_log_ratio {
                max_log_ratio = ratio;
            }
        }
    }
    if max_log_ratio < 1e-6 {
        max_log_ratio = 1.0; // Avoid division by zero.
    }

    let mut surprises: Vec<SurprisingEdge> = Vec::new();

    for (from, to, edge) in &edges {
        if from.starts_with("__ext__:") || to.starts_with("__ext__:") {
            continue;
        }

        let mut score = 0.0_f32;
        let mut reasons = Vec::new();

        // Factor 1: Cross-community (+0.3).
        if has_communities {
            if let (Some(&c_from), Some(&c_to)) = (communities.get(*from), communities.get(*to)) {
                if c_from != c_to {
                    score += 0.3;
                    reasons.push(format!(
                        "cross-community: community {} -> community {}",
                        c_from, c_to
                    ));
                }
            }
        }

        // Factor 2: PageRank disparity (+0.3 max).
        if let (Some(src_node), Some(tgt_node)) = (graph.node(from), graph.node(to)) {
            let src_pr = src_node.pagerank.max(1e-6);
            let tgt_pr = tgt_node.pagerank.max(1e-6);
            let log_ratio = (src_pr / tgt_pr).ln().abs();
            let pr_score = (log_ratio / max_log_ratio).min(1.0) * 0.3;
            if pr_score > 0.05 {
                score += pr_score;
                reasons.push(format!(
                    "PageRank disparity: {:.3} vs {:.3} (ratio {:.1}x)",
                    src_pr,
                    tgt_pr,
                    (src_pr / tgt_pr).abs()
                ));
            }
        }

        // Factor 3: Cross-directory (+0.2).
        let from_dir = directory_prefix(from);
        let to_dir = directory_prefix(to);
        if from_dir != to_dir && !shares_common_parent(from, to) {
            score += 0.2;
            reasons.push(format!(
                "cross-directory: {} -> {}",
                from_dir.unwrap_or("(root)"),
                to_dir.unwrap_or("(root)")
            ));
        }

        // Factor 4: Low confidence (+0.2).
        match edge.confidence {
            EdgeConfidence::Medium => {
                score += 0.1;
                reasons.push("medium confidence edge".to_string());
            }
            EdgeConfidence::Low => {
                score += 0.2;
                reasons.push("low confidence edge".to_string());
            }
            _ => {}
        }

        if score > 0.0 {
            surprises.push(SurprisingEdge {
                from: from.to_string(),
                to: to.to_string(),
                score,
                reasons,
            });
        }
    }

    // Sort by score descending.
    surprises.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    surprises.truncate(top_n);
    surprises
}

/// Extract the first directory component from a path.
fn directory_prefix(path: &str) -> Option<&str> {
    path.find('/').map(|i| &path[..i])
}

/// Check if two paths share a common parent directory beyond the project root.
fn shares_common_parent(a: &str, b: &str) -> bool {
    let a_parts: Vec<&str> = a.split('/').collect();
    let b_parts: Vec<&str> = b.split('/').collect();

    // They must share at least 2 directory levels (beyond just the first dir).
    if a_parts.len() < 2 || b_parts.len() < 2 {
        return false;
    }

    // Check if the first two components match.
    a_parts.len() >= 2
        && b_parts.len() >= 2
        && a_parts[0] == b_parts[0]
        && a_parts.len() > 1
        && b_parts.len() > 1
        && a_parts[..a_parts.len() - 1]
            .iter()
            .zip(b_parts[..b_parts.len() - 1].iter())
            .any(|(a, b)| a == b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::Language;

    #[test]
    fn empty_graph_no_surprises() {
        let g = CodeGraph::new();
        let result = detect_surprises(&g, 10);
        assert!(result.is_empty());
    }

    #[test]
    fn low_confidence_edge_is_surprising() {
        let mut g = CodeGraph::new();
        g.add_external_edge("src/a.rs", "serde", Language::Rust);
        // External edges have Low confidence, but __ext__ nodes are filtered.
        // Add a doc edge which has Medium confidence.
        g.add_doc_edge(
            "docs/guide.md",
            "src/engine.rs",
            "Engine",
            Language::Rust,
            Language::Rust,
        );

        let result = detect_surprises(&g, 10);
        // The doc edge should appear with medium confidence contributing.
        assert!(!result.is_empty());
        assert!(result[0].reasons.iter().any(|r| r.contains("confidence")));
    }

    #[test]
    fn cross_directory_is_surprising() {
        let mut g = CodeGraph::new();
        // Files in completely different directory trees.
        g.add_edge(
            "frontend/app.ts",
            "backend/db.rs",
            "db",
            Language::TypeScript,
            Language::Rust,
        );

        let result = detect_surprises(&g, 10);
        assert!(!result.is_empty());
        assert!(
            result[0]
                .reasons
                .iter()
                .any(|r| r.contains("cross-directory")),
            "expected cross-directory reason, got: {:?}",
            result[0].reasons
        );
    }

    #[test]
    fn cross_community_is_surprising() {
        let mut g = CodeGraph::new();
        // Two separate clusters.
        g.add_edge("src/a.rs", "src/b.rs", "b", Language::Rust, Language::Rust);
        g.add_edge("src/b.rs", "src/a.rs", "a", Language::Rust, Language::Rust);
        g.add_edge("lib/c.rs", "lib/d.rs", "d", Language::Rust, Language::Rust);
        g.add_edge("lib/d.rs", "lib/c.rs", "c", Language::Rust, Language::Rust);
        // Cross-cluster edge.
        g.add_edge("src/a.rs", "lib/c.rs", "c", Language::Rust, Language::Rust);

        // Run community detection first.
        g.detect_communities();

        let result = detect_surprises(&g, 10);
        // The cross-cluster edge should be most surprising.
        assert!(!result.is_empty());
    }

    #[test]
    fn top_n_limits_results() {
        let mut g = CodeGraph::new();
        for i in 0..20 {
            g.add_edge(
                &format!("dir{i}/file.rs"),
                &format!("other{i}/dep.rs"),
                "dep",
                Language::Rust,
                Language::Rust,
            );
        }

        let result = detect_surprises(&g, 5);
        assert!(result.len() <= 5);
    }

    #[test]
    fn scores_are_bounded() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "frontend/app.ts",
            "backend/db.rs",
            "db",
            Language::TypeScript,
            Language::Rust,
        );
        g.add_doc_edge(
            "docs/api.md",
            "src/server.rs",
            "serve",
            Language::Rust,
            Language::Rust,
        );

        let result = detect_surprises(&g, 10);
        for edge in &result {
            assert!(
                edge.score >= 0.0 && edge.score <= 1.0,
                "score {} out of bounds",
                edge.score
            );
        }
    }
}

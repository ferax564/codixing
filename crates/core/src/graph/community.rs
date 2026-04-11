//! Louvain community detection for the code dependency graph.
//!
//! Detects clusters of tightly-coupled files using the Louvain algorithm
//! for modularity optimization. Treats the file-level dependency graph as
//! undirected (both import directions count as connections).

use std::collections::HashMap;

use super::CodeGraph;

/// Result of community detection.
#[derive(Debug, Clone)]
pub struct CommunityResult {
    /// Mapping from file path to community ID.
    pub assignments: HashMap<String, usize>,
    /// Total number of distinct communities.
    pub community_count: usize,
    /// Final modularity score (0.0 to 1.0).
    pub modularity: f64,
}

/// Run Louvain community detection on the file-level dependency graph.
///
/// Treats the graph as undirected: both A->B and B->A edges contribute
/// to the connection weight between A and B. External (`__ext__:`) nodes
/// are excluded.
pub fn detect_communities(graph: &CodeGraph) -> CommunityResult {
    // Collect real file paths.
    let paths: Vec<String> = graph.file_paths();
    let n = paths.len();

    if n == 0 {
        return CommunityResult {
            assignments: HashMap::new(),
            community_count: 0,
            modularity: 0.0,
        };
    }

    // Build path -> index mapping.
    let path_to_idx: HashMap<&str, usize> = paths
        .iter()
        .enumerate()
        .map(|(i, p)| (p.as_str(), i))
        .collect();

    // Build adjacency with weights (number of edges in either direction).
    // w[i][j] = number of edges between node i and node j (treating as undirected).
    let mut adj: Vec<HashMap<usize, f64>> = vec![HashMap::new(); n];
    let mut total_weight = 0.0_f64;

    for (from, to, _edge) in graph.all_edges() {
        if from.starts_with("__ext__:") || to.starts_with("__ext__:") {
            continue;
        }
        if let (Some(&fi), Some(&ti)) = (path_to_idx.get(from), path_to_idx.get(to)) {
            if fi != ti {
                *adj[fi].entry(ti).or_insert(0.0) += 1.0;
                *adj[ti].entry(fi).or_insert(0.0) += 1.0;
                total_weight += 1.0; // Each directed edge contributes 1 to total (undirected: 2m)
            }
        }
    }

    // 2m = total undirected weight (each directed edge counted once above, then
    // we double-counted in the adjacency, so total_weight = m for our undirected view).
    // Actually, we added to both adj[fi][ti] and adj[ti][fi], so the sum of all
    // adj entries = 2 * total_weight. So 2m = 2 * total_weight.
    let two_m = 2.0 * total_weight;

    if two_m == 0.0 {
        // No edges: each node is its own community.
        let assignments: HashMap<String, usize> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| (p.clone(), i))
            .collect();
        return CommunityResult {
            community_count: n,
            modularity: 0.0,
            assignments,
        };
    }

    // Degree of each node (sum of undirected edge weights).
    let degree: Vec<f64> = (0..n).map(|i| adj[i].values().sum::<f64>()).collect();

    // Initial assignment: each node is its own community.
    let mut community: Vec<usize> = (0..n).collect();
    let next_community_id = n;

    // Phase 1: Iteratively move nodes to maximize modularity gain.
    let mut improved = true;
    let max_iterations = 100;
    let mut iteration = 0;

    while improved && iteration < max_iterations {
        improved = false;
        iteration += 1;

        for i in 0..n {
            let current_comm = community[i];
            let ki = degree[i];

            // Compute sum of weights to each neighboring community.
            let mut comm_weights: HashMap<usize, f64> = HashMap::new();
            for (&j, &w) in &adj[i] {
                let cj = community[j];
                *comm_weights.entry(cj).or_insert(0.0) += w;
            }

            // Compute sigma_tot and sigma_in for current community (excluding node i).
            let sigma_tot_current: f64 = (0..n)
                .filter(|&j| j != i && community[j] == current_comm)
                .map(|j| degree[j])
                .sum();

            let ki_in_current = comm_weights.get(&current_comm).copied().unwrap_or(0.0);

            // Removal gain (cost of removing i from its current community).
            let remove_gain = ki_in_current / two_m - (sigma_tot_current * ki) / (two_m * two_m);

            // Find the best community to move to.
            let mut best_comm = current_comm;
            let mut best_gain = 0.0_f64;

            for (&target_comm, &ki_in_target) in &comm_weights {
                if target_comm == current_comm {
                    continue;
                }

                let sigma_tot_target: f64 = (0..n)
                    .filter(|&j| community[j] == target_comm)
                    .map(|j| degree[j])
                    .sum();

                // Insert gain (benefit of adding i to target community).
                let insert_gain = ki_in_target / two_m - (sigma_tot_target * ki) / (two_m * two_m);

                let delta_q = insert_gain - remove_gain;
                if delta_q > best_gain {
                    best_gain = delta_q;
                    best_comm = target_comm;
                }
            }

            if best_comm != current_comm && best_gain > 1e-10 {
                community[i] = best_comm;
                improved = true;
            }
        }
    }

    // Renumber communities contiguously starting from 0.
    let mut comm_remap: HashMap<usize, usize> = HashMap::new();
    let mut next_id = 0;
    for &c in &community {
        if let std::collections::hash_map::Entry::Vacant(e) = comm_remap.entry(c) {
            e.insert(next_id);
            next_id += 1;
        }
    }
    let _ = next_community_id;

    let assignments: HashMap<String, usize> = paths
        .iter()
        .enumerate()
        .map(|(i, p)| (p.clone(), comm_remap[&community[i]]))
        .collect();

    let community_count = next_id;

    // Compute final modularity.
    let modularity = compute_modularity(&adj, &community, &degree, two_m);

    CommunityResult {
        assignments,
        community_count,
        modularity,
    }
}

/// Compute Newman-Girvan modularity Q.
///
/// Q = (1/2m) * sum_ij [ A_ij - k_i*k_j/(2m) ] * delta(c_i, c_j)
fn compute_modularity(
    adj: &[HashMap<usize, f64>],
    community: &[usize],
    degree: &[f64],
    two_m: f64,
) -> f64 {
    if two_m == 0.0 {
        return 0.0;
    }

    let n = community.len();
    let mut q = 0.0_f64;

    for i in 0..n {
        for j in 0..n {
            if community[i] != community[j] {
                continue;
            }
            let a_ij = adj[i].get(&j).copied().unwrap_or(0.0);
            q += a_ij - (degree[i] * degree[j]) / two_m;
        }
    }

    q / two_m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::Language;

    #[test]
    fn empty_graph_yields_empty_result() {
        let g = CodeGraph::new();
        let result = detect_communities(&g);
        assert_eq!(result.community_count, 0);
        assert!(result.assignments.is_empty());
    }

    #[test]
    fn single_node_is_own_community() {
        let mut g = CodeGraph::new();
        g.get_or_insert_node("src/a.rs", Language::Rust);
        let result = detect_communities(&g);
        assert_eq!(result.community_count, 1);
        assert!(result.assignments.contains_key("src/a.rs"));
    }

    #[test]
    fn two_connected_nodes_same_community() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/a.rs",
            "src/b.rs",
            "crate::b",
            Language::Rust,
            Language::Rust,
        );
        let result = detect_communities(&g);
        let ca = result.assignments["src/a.rs"];
        let cb = result.assignments["src/b.rs"];
        assert_eq!(ca, cb, "connected nodes should be in the same community");
    }

    #[test]
    fn disconnected_clusters_different_communities() {
        let mut g = CodeGraph::new();
        // Cluster 1: a <-> b
        g.add_edge("src/a.rs", "src/b.rs", "b", Language::Rust, Language::Rust);
        g.add_edge("src/b.rs", "src/a.rs", "a", Language::Rust, Language::Rust);
        // Cluster 2: c <-> d
        g.add_edge("src/c.rs", "src/d.rs", "d", Language::Rust, Language::Rust);
        g.add_edge("src/d.rs", "src/c.rs", "c", Language::Rust, Language::Rust);

        let result = detect_communities(&g);
        let ca = result.assignments["src/a.rs"];
        let cb = result.assignments["src/b.rs"];
        let cc = result.assignments["src/c.rs"];
        let cd = result.assignments["src/d.rs"];

        assert_eq!(ca, cb, "a and b should be in the same community");
        assert_eq!(cc, cd, "c and d should be in the same community");
        assert_ne!(ca, cc, "clusters should be in different communities");
    }

    #[test]
    fn modularity_is_bounded() {
        let mut g = CodeGraph::new();
        g.add_edge("src/a.rs", "src/b.rs", "b", Language::Rust, Language::Rust);
        g.add_edge("src/b.rs", "src/c.rs", "c", Language::Rust, Language::Rust);
        let result = detect_communities(&g);
        assert!(
            result.modularity >= -0.5 && result.modularity <= 1.0,
            "modularity {} out of expected range",
            result.modularity
        );
    }

    #[test]
    fn external_nodes_excluded() {
        let mut g = CodeGraph::new();
        g.add_edge("src/a.rs", "src/b.rs", "b", Language::Rust, Language::Rust);
        g.add_external_edge("src/a.rs", "serde", Language::Rust);

        let result = detect_communities(&g);
        // Only real files should appear in assignments.
        assert!(result.assignments.contains_key("src/a.rs"));
        assert!(result.assignments.contains_key("src/b.rs"));
        assert!(!result.assignments.keys().any(|k| k.starts_with("__ext__")));
    }

    #[test]
    fn codegraph_detect_communities_stores_on_nodes() {
        let mut g = CodeGraph::new();
        g.add_edge("src/a.rs", "src/b.rs", "b", Language::Rust, Language::Rust);

        let result = g.detect_communities();
        // Verify the node's community field was set.
        let node_a = g.node("src/a.rs").unwrap();
        assert_eq!(node_a.community, Some(result.assignments["src/a.rs"]));
    }
}

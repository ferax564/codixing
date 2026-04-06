//! Change impact analysis — blast radius computation.
//!
//! Given a file path, computes the set of files that are directly or
//! transitively affected by a change, plus any test files that cover the
//! changed file.

use std::collections::{HashSet, VecDeque};

use serde::Serialize;

use crate::graph::CodeGraph;
use crate::test_mapping::{TestMappingOptions, discover_test_mappings};

use super::Engine;

/// Summary of how a change to a single file propagates through the codebase.
#[derive(Debug, Clone, Serialize)]
pub struct ChangeImpact {
    /// The file that was changed.
    pub file_path: String,
    /// Files that directly import / depend on the changed file.
    pub direct_dependents: Vec<String>,
    /// Files reachable via transitive callers (excluding direct dependents).
    pub transitive_dependents: Vec<String>,
    /// Test files that cover the changed file (via test mapping heuristics).
    pub affected_tests: Vec<String>,
    /// Total unique files in the blast radius (direct + transitive + tests).
    pub blast_radius: usize,
}

/// Compute the change impact for `file_path` using the dependency graph.
///
/// `all_files` is used for test-mapping discovery. When `None`, test discovery
/// is skipped (useful when only the graph is available).
pub fn compute_change_impact(
    graph: &CodeGraph,
    file_path: &str,
    all_files: Option<&[String]>,
) -> ChangeImpact {
    // Step 1: Direct dependents — files that import `file_path`.
    let direct_dependents = graph.callers(file_path);

    // Step 2: BFS over the caller graph to find transitive dependents.
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(file_path.to_string());
    for d in &direct_dependents {
        visited.insert(d.clone());
    }

    let mut queue: VecDeque<String> = direct_dependents.iter().cloned().collect();
    let mut transitive_dependents: Vec<String> = Vec::new();

    while let Some(current) = queue.pop_front() {
        for caller in graph.callers(&current) {
            if visited.insert(caller.clone()) {
                transitive_dependents.push(caller.clone());
                queue.push_back(caller);
            }
        }
    }

    // Step 3: Discover affected tests (if file list is available).
    let affected_tests = if let Some(files) = all_files {
        // Build a lightweight import-dep map from the graph for test mapping.
        let mut import_deps = std::collections::HashMap::new();
        for f in files {
            let callees = graph.callees(f);
            if !callees.is_empty() {
                import_deps.insert(f.clone(), callees);
            }
        }

        let mappings =
            discover_test_mappings(files, Some(&import_deps), &TestMappingOptions::default());

        // Collect test files that map to the changed file or any of its dependents.
        let impacted: HashSet<&str> = std::iter::once(file_path)
            .chain(direct_dependents.iter().map(|s| s.as_str()))
            .chain(transitive_dependents.iter().map(|s| s.as_str()))
            .collect();

        let mut tests: Vec<String> = mappings
            .into_iter()
            .filter(|m| impacted.contains(m.source_file.as_str()))
            .map(|m| m.test_file)
            .collect();
        tests.sort();
        tests.dedup();
        tests
    } else {
        Vec::new()
    };

    // Step 4: Compute blast radius — unique files across all categories.
    let mut all_affected: HashSet<&str> = HashSet::new();
    for d in &direct_dependents {
        all_affected.insert(d.as_str());
    }
    for t in &transitive_dependents {
        all_affected.insert(t.as_str());
    }
    for t in &affected_tests {
        all_affected.insert(t.as_str());
    }
    let blast_radius = all_affected.len();

    ChangeImpact {
        file_path: file_path.to_string(),
        direct_dependents,
        transitive_dependents,
        affected_tests,
        blast_radius,
    }
}

impl Engine {
    /// Compute the change impact for a file in this project.
    ///
    /// Returns a [`ChangeImpact`] describing direct dependents, transitive
    /// dependents, affected tests, and the overall blast radius.
    ///
    /// Returns an empty impact if the graph is not built or the file is unknown.
    pub fn change_impact(&self, file_path: &str) -> ChangeImpact {
        let graph = match self.graph.as_ref() {
            Some(g) => g,
            None => {
                return ChangeImpact {
                    file_path: file_path.to_string(),
                    direct_dependents: Vec::new(),
                    transitive_dependents: Vec::new(),
                    affected_tests: Vec::new(),
                    blast_radius: 0,
                };
            }
        };

        let all_files: Vec<String> = self.file_chunk_counts.keys().cloned().collect();
        compute_change_impact(graph, file_path, Some(&all_files))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::Language;

    /// Helper: build a simple dependency chain A -> B -> C -> D.
    fn build_chain_graph() -> CodeGraph {
        let mut g = CodeGraph::new();
        // A imports B
        g.add_edge(
            "src/a.rs",
            "src/b.rs",
            "crate::b",
            Language::Rust,
            Language::Rust,
        );
        // B imports C
        g.add_edge(
            "src/b.rs",
            "src/c.rs",
            "crate::c",
            Language::Rust,
            Language::Rust,
        );
        // C imports D
        g.add_edge(
            "src/c.rs",
            "src/d.rs",
            "crate::d",
            Language::Rust,
            Language::Rust,
        );
        g
    }

    #[test]
    fn change_impact_direct_dependents() {
        let g = build_chain_graph();

        // Changing D: C imports D, so C is a direct dependent.
        let impact = compute_change_impact(&g, "src/d.rs", None);
        assert_eq!(impact.direct_dependents, vec!["src/c.rs".to_string()]);
    }

    #[test]
    fn change_impact_transitive_dependents() {
        let g = build_chain_graph();

        // Changing D: C is direct, B imports C (transitive), A imports B (transitive).
        let impact = compute_change_impact(&g, "src/d.rs", None);
        assert_eq!(impact.direct_dependents.len(), 1);
        assert!(impact.direct_dependents.contains(&"src/c.rs".to_string()));

        assert_eq!(impact.transitive_dependents.len(), 2);
        assert!(
            impact
                .transitive_dependents
                .contains(&"src/b.rs".to_string())
        );
        assert!(
            impact
                .transitive_dependents
                .contains(&"src/a.rs".to_string())
        );
    }

    #[test]
    fn change_impact_blast_radius() {
        let g = build_chain_graph();

        // D has 3 dependents total: C (direct), B + A (transitive).
        // No tests provided (all_files = None), so blast_radius = 3.
        let impact = compute_change_impact(&g, "src/d.rs", None);
        assert_eq!(impact.blast_radius, 3);

        // Changing A: no one imports A, so blast_radius = 0.
        let impact_a = compute_change_impact(&g, "src/a.rs", None);
        assert_eq!(impact_a.blast_radius, 0);
    }

    #[test]
    fn change_impact_unknown_file() {
        let g = build_chain_graph();

        let impact = compute_change_impact(&g, "src/nonexistent.rs", None);
        assert!(impact.direct_dependents.is_empty());
        assert!(impact.transitive_dependents.is_empty());
        assert!(impact.affected_tests.is_empty());
        assert_eq!(impact.blast_radius, 0);
    }

    #[test]
    fn change_impact_leaf_file() {
        let g = build_chain_graph();

        // A is a leaf — nothing imports it.
        let impact = compute_change_impact(&g, "src/a.rs", None);
        assert!(impact.direct_dependents.is_empty());
        assert!(impact.transitive_dependents.is_empty());
        assert_eq!(impact.blast_radius, 0);
    }

    #[test]
    fn change_impact_cycle_handling() {
        // Build a cycle: A -> B -> C -> A
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/a.rs",
            "src/b.rs",
            "crate::b",
            Language::Rust,
            Language::Rust,
        );
        g.add_edge(
            "src/b.rs",
            "src/c.rs",
            "crate::c",
            Language::Rust,
            Language::Rust,
        );
        g.add_edge(
            "src/c.rs",
            "src/a.rs",
            "crate::a",
            Language::Rust,
            Language::Rust,
        );

        // Changing B: callers of B = {A}, then callers of A = {C}, callers of C = {B} (already visited).
        let impact = compute_change_impact(&g, "src/b.rs", None);
        // Direct: A
        assert_eq!(impact.direct_dependents.len(), 1);
        assert!(impact.direct_dependents.contains(&"src/a.rs".to_string()));
        // Transitive: C (via A -> callers -> C)
        assert_eq!(impact.transitive_dependents.len(), 1);
        assert!(
            impact
                .transitive_dependents
                .contains(&"src/c.rs".to_string())
        );
        // No infinite loop — blast_radius = 2.
        assert_eq!(impact.blast_radius, 2);
    }

    #[test]
    fn change_impact_with_test_discovery() {
        let mut g = CodeGraph::new();
        // src/widget.rs is imported by src/app.rs
        g.add_edge(
            "src/app.rs",
            "src/widget.rs",
            "crate::widget",
            Language::Rust,
            Language::Rust,
        );
        // test_widget.rs imports widget.rs (for test mapping)
        g.add_edge(
            "tests/test_widget.rs",
            "src/widget.rs",
            "crate::widget",
            Language::Rust,
            Language::Rust,
        );

        let all_files: Vec<String> = vec![
            "src/widget.rs".to_string(),
            "src/app.rs".to_string(),
            "tests/test_widget.rs".to_string(),
        ];

        let impact = compute_change_impact(&g, "src/widget.rs", Some(&all_files));
        // Direct dependents: app.rs and test_widget.rs
        assert!(impact.direct_dependents.contains(&"src/app.rs".to_string()));
        // test_widget.rs should appear as an affected test
        assert!(
            impact
                .affected_tests
                .contains(&"tests/test_widget.rs".to_string()),
            "Expected test_widget.rs in affected_tests, got: {:?}",
            impact.affected_tests,
        );
    }

    #[test]
    fn change_impact_diamond_graph() {
        // Diamond: D <- B <- A and D <- C <- A (A depends on both B and C which both depend on D)
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/a.rs",
            "src/b.rs",
            "crate::b",
            Language::Rust,
            Language::Rust,
        );
        g.add_edge(
            "src/a.rs",
            "src/c.rs",
            "crate::c",
            Language::Rust,
            Language::Rust,
        );
        g.add_edge(
            "src/b.rs",
            "src/d.rs",
            "crate::d",
            Language::Rust,
            Language::Rust,
        );
        g.add_edge(
            "src/c.rs",
            "src/d.rs",
            "crate::d",
            Language::Rust,
            Language::Rust,
        );

        let impact = compute_change_impact(&g, "src/d.rs", None);
        // Direct: B and C
        assert_eq!(impact.direct_dependents.len(), 2);
        assert!(impact.direct_dependents.contains(&"src/b.rs".to_string()));
        assert!(impact.direct_dependents.contains(&"src/c.rs".to_string()));
        // Transitive: A (reached via either B or C)
        assert_eq!(impact.transitive_dependents.len(), 1);
        assert!(
            impact
                .transitive_dependents
                .contains(&"src/a.rs".to_string())
        );
        // Total: 3
        assert_eq!(impact.blast_radius, 3);
    }
}

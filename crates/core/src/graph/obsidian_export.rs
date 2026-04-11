//! Obsidian vault export for graph visualization in Obsidian.
//!
//! Generates a directory of Markdown files with YAML frontmatter, wiki-links,
//! community notes, and a Map of Content (MOC) for exploring the dependency graph.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use super::CodeGraph;
use crate::error::Result;

/// Options for Obsidian vault export.
#[derive(Debug, Clone)]
pub struct ObsidianExportOptions {
    /// Output directory for the vault.
    pub output_dir: PathBuf,
    /// Whether to include `__ext__` pseudo-nodes. Default: false.
    pub include_external: bool,
}

/// Sanitize a file path for use as an Obsidian note filename.
/// Replaces characters that are invalid in filenames or problematic in Obsidian.
fn sanitize_filename(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '#' | '^' | '[' | ']' => {
                out.push('_')
            }
            _ => out.push(c),
        }
    }
    out
}

/// Export the dependency graph as an Obsidian vault.
///
/// Creates:
/// - Per-file notes with frontmatter, callers/callees sections, and tags
/// - Community index notes (`_COMMUNITY_<id>.md`)
/// - Map of Content (`_MOC.md`)
/// - `.obsidian/graph.json` for community-based coloring
///
/// Returns the number of notes created.
pub fn export_obsidian(graph: &CodeGraph, options: &ObsidianExportOptions) -> Result<usize> {
    std::fs::create_dir_all(&options.output_dir)?;

    // Collect all non-external nodes.
    let all_nodes = graph.nodes_by_pagerank();
    let nodes: Vec<_> = all_nodes
        .iter()
        .filter(|n| options.include_external || !n.file_path.starts_with("__ext__:"))
        .collect();

    // Build edge lookup: for each file, collect callers and callees with edge info.
    let all_edges = graph.all_edges();

    // callers_of[target] = vec![(source, edge)]
    let mut callers_of: HashMap<&str, Vec<(&str, &str, &str)>> = HashMap::new();
    // callees_of[source] = vec![(target, edge)]
    let mut callees_of: HashMap<&str, Vec<(&str, &str, &str)>> = HashMap::new();

    for (from, to, edge) in &all_edges {
        if !options.include_external && (from.starts_with("__ext__:") || to.starts_with("__ext__:"))
        {
            continue;
        }
        let kind_str = match edge.kind {
            super::EdgeKind::Resolved => "IMPORTS",
            super::EdgeKind::Calls => "CALLS",
            super::EdgeKind::DocumentedBy => "DOCUMENTED_BY",
            super::EdgeKind::External => "EXTERNAL_DEP",
        };
        let provenance = edge.confidence.provenance();
        callers_of
            .entry(to)
            .or_default()
            .push((from, kind_str, provenance));
        callees_of
            .entry(from)
            .or_default()
            .push((to, kind_str, provenance));
    }

    // Group by community.
    let mut communities: BTreeMap<usize, Vec<(&str, f32, &str)>> = BTreeMap::new();

    let mut note_count = 0usize;

    // Write per-file notes.
    for node in &nodes {
        let sanitized = sanitize_filename(&node.file_path);
        let lang_lower = node.language.name().to_lowercase();
        let community_id = node.community.unwrap_or(0);

        // Track community membership.
        communities.entry(community_id).or_default().push((
            &node.file_path,
            node.pagerank,
            node.language.name(),
        ));

        let mut md = String::new();

        // Frontmatter.
        md.push_str("---\n");
        md.push_str(&format!("path: \"{}\"\n", node.file_path));
        md.push_str(&format!("language: {}\n", lang_lower));
        md.push_str(&format!("pagerank: {:.2}\n", node.pagerank));
        md.push_str(&format!("community: {}\n", community_id));
        md.push_str(&format!("in_degree: {}\n", node.in_degree));
        md.push_str(&format!("out_degree: {}\n", node.out_degree));
        md.push_str("tags:\n");
        md.push_str(&format!("  - codixing/{}\n", lang_lower));
        md.push_str(&format!("  - community/{}\n", community_id));
        md.push_str("---\n\n");

        // Title.
        md.push_str(&format!("# {}\n\n", node.file_path));

        // Summary line.
        md.push_str(&format!(
            "**PageRank:** {:.2} · **Community:** {} · **Edges:** {} in / {} out\n\n",
            node.pagerank, community_id, node.in_degree, node.out_degree,
        ));

        // Callers section.
        if let Some(callers) = callers_of.get(node.file_path.as_str()) {
            md.push_str("## Callers\n");
            for (caller, kind, prov) in callers {
                let caller_link = sanitize_filename(caller);
                md.push_str(&format!("- [[{}]] — {} [{}]\n", caller_link, kind, prov));
            }
            md.push('\n');
        }

        // Callees section.
        if let Some(callees) = callees_of.get(node.file_path.as_str()) {
            md.push_str("## Callees\n");
            for (callee, kind, prov) in callees {
                let callee_link = sanitize_filename(callee);
                md.push_str(&format!("- [[{}]] — {} [{}]\n", callee_link, kind, prov));
            }
            md.push('\n');
        }

        // Tags footer.
        md.push_str(&format!(
            "#codixing/{} #community/{}\n",
            lang_lower, community_id,
        ));

        let note_path = options.output_dir.join(format!("{}.md", sanitized));
        std::fs::write(&note_path, &md)?;
        note_count += 1;
    }

    // Write community index notes.
    for (comm_id, members) in &communities {
        let mut md = String::new();

        md.push_str("---\n");
        md.push_str("type: community\n");
        md.push_str(&format!("community_id: {}\n", comm_id));
        md.push_str(&format!("members: {}\n", members.len()));
        md.push_str("---\n\n");
        md.push_str(&format!("# Community {}\n\n", comm_id));
        md.push_str(&format!("**Members:** {} files\n\n", members.len()));

        // Top files by PageRank (up to 10).
        md.push_str("## Top files by PageRank\n");
        let mut sorted = members.clone();
        sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (path, pr, _lang) in sorted.iter().take(10) {
            let link = sanitize_filename(path);
            md.push_str(&format!("- [[{}]] — {:.2}\n", link, pr));
        }
        md.push('\n');

        // All members.
        md.push_str("## All members\n");
        let mut alpha = members.clone();
        alpha.sort_by_key(|(p, _, _)| *p);
        for (path, _pr, lang) in &alpha {
            let link = sanitize_filename(path);
            md.push_str(&format!("- [[{}]] — {}\n", link, lang.to_lowercase()));
        }
        md.push('\n');

        let note_path = options
            .output_dir
            .join(format!("_COMMUNITY_{}.md", comm_id));
        std::fs::write(&note_path, &md)?;
        note_count += 1;
    }

    // Write Map of Content.
    {
        let total_edges = all_edges
            .iter()
            .filter(|(from, to, _)| {
                options.include_external
                    || (!from.starts_with("__ext__:") && !to.starts_with("__ext__:"))
            })
            .count();

        let mut md = String::new();
        md.push_str("# Codixing Graph — Map of Content\n\n");
        md.push_str(&format!(
            "**Files:** {} · **Edges:** {} · **Communities:** {}\n\n",
            nodes.len(),
            total_edges,
            communities.len(),
        ));

        md.push_str("## By Community\n");
        for (comm_id, members) in &communities {
            md.push_str(&format!(
                "- [[_COMMUNITY_{}]] — {} files\n",
                comm_id,
                members.len()
            ));
        }
        md.push('\n');

        md.push_str("## Top files by PageRank\n");
        for (i, node) in nodes.iter().take(20).enumerate() {
            let link = sanitize_filename(&node.file_path);
            md.push_str(&format!("{}. [[{}]] — {:.2}\n", i + 1, link, node.pagerank));
        }
        md.push('\n');

        let moc_path = options.output_dir.join("_MOC.md");
        std::fs::write(&moc_path, &md)?;
        note_count += 1;
    }

    // Write .obsidian/graph.json for community coloring.
    {
        let obsidian_dir = options.output_dir.join(".obsidian");
        std::fs::create_dir_all(&obsidian_dir)?;

        let colors: Vec<u32> = vec![
            5143975, 16729156, 3381759, 16776960, 16711935, 65535, 16744448, 8388736, 32768,
            8421504,
        ];

        let color_groups: Vec<String> = communities
            .keys()
            .map(|comm_id| {
                let color = colors[*comm_id % colors.len()];
                format!(
                    "{{\"query\":\"tag:#community/{}\",\"color\":{{\"a\":1,\"rgb\":{}}}}}",
                    comm_id, color
                )
            })
            .collect();

        let graph_json = format!("{{\"colorGroups\":[{}]}}", color_groups.join(","));

        let graph_path = obsidian_dir.join("graph.json");
        std::fs::write(&graph_path, &graph_json)?;
    }

    Ok(note_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::CodeGraph;
    use crate::language::Language;

    #[test]
    fn obsidian_export_creates_expected_files() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/main.rs",
            "src/lib.rs",
            "crate::lib",
            Language::Rust,
            Language::Rust,
        );
        // Assign communities.
        g.detect_communities();

        let dir = tempfile::tempdir().unwrap();
        let opts = ObsidianExportOptions {
            output_dir: dir.path().to_path_buf(),
            include_external: false,
        };

        let count = export_obsidian(&g, &opts).unwrap();
        assert!(count >= 3); // At least 2 file notes + 1 community + MOC

        // Check file notes exist.
        assert!(dir.path().join("src_main.rs.md").exists());
        assert!(dir.path().join("src_lib.rs.md").exists());

        // Check MOC exists.
        assert!(dir.path().join("_MOC.md").exists());

        // Check .obsidian/graph.json exists.
        assert!(dir.path().join(".obsidian").join("graph.json").exists());
    }

    #[test]
    fn obsidian_frontmatter_is_valid_yaml() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/engine/mod.rs",
            "src/graph/mod.rs",
            "crate::graph",
            Language::Rust,
            Language::Rust,
        );

        let dir = tempfile::tempdir().unwrap();
        let opts = ObsidianExportOptions {
            output_dir: dir.path().to_path_buf(),
            include_external: false,
        };

        export_obsidian(&g, &opts).unwrap();

        let content = std::fs::read_to_string(dir.path().join("src_engine_mod.rs.md")).unwrap();
        // Check frontmatter structure.
        assert!(content.starts_with("---\n"));
        assert!(content.contains("path: \"src/engine/mod.rs\""));
        assert!(content.contains("language: rust"));
        assert!(content.contains("pagerank:"));
        assert!(content.contains("tags:"));
        assert!(content.contains("  - codixing/rust"));
    }

    #[test]
    fn sanitize_filename_replaces_special_chars() {
        assert_eq!(sanitize_filename("src/main.rs"), "src_main.rs");
        assert_eq!(sanitize_filename("a:b*c?d"), "a_b_c_d");
        assert_eq!(sanitize_filename("path/to/[file]"), "path_to__file_");
    }
}

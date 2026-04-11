//! GraphML export for Gephi, yEd, and other graph visualization tools.
//!
//! Produces a standard GraphML XML file with node and edge attributes.

use std::path::PathBuf;

use super::CodeGraph;
use crate::error::Result;

/// Options for GraphML export.
#[derive(Debug, Clone)]
pub struct GraphmlExportOptions {
    /// Output file path.
    pub output_path: PathBuf,
}

/// XML-escape a string for safe embedding in GraphML attributes/data.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Export the dependency graph as a GraphML XML file.
///
/// Nodes carry: pagerank, community, in_degree, out_degree, language.
/// Edges carry: kind, confidence, provenance, raw_import.
pub fn export_graphml(graph: &CodeGraph, options: &GraphmlExportOptions) -> Result<()> {
    let mut xml = String::new();

    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<graphml xmlns=\"http://graphml.graphstruct.org/graphml\"\n");
    xml.push_str("  xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"\n");
    xml.push_str("  xsi:schemaLocation=\"http://graphml.graphstruct.org/graphml http://graphml.graphstruct.org/graphml/1.0/graphml.xsd\">\n");

    // Key declarations for node attributes.
    xml.push_str(
        "  <key id=\"pagerank\" for=\"node\" attr.name=\"pagerank\" attr.type=\"float\"/>\n",
    );
    xml.push_str(
        "  <key id=\"community\" for=\"node\" attr.name=\"community\" attr.type=\"int\"/>\n",
    );
    xml.push_str(
        "  <key id=\"in_degree\" for=\"node\" attr.name=\"in_degree\" attr.type=\"int\"/>\n",
    );
    xml.push_str(
        "  <key id=\"out_degree\" for=\"node\" attr.name=\"out_degree\" attr.type=\"int\"/>\n",
    );
    xml.push_str(
        "  <key id=\"language\" for=\"node\" attr.name=\"language\" attr.type=\"string\"/>\n",
    );

    // Key declarations for edge attributes.
    xml.push_str("  <key id=\"kind\" for=\"edge\" attr.name=\"kind\" attr.type=\"string\"/>\n");
    xml.push_str(
        "  <key id=\"confidence\" for=\"edge\" attr.name=\"confidence\" attr.type=\"string\"/>\n",
    );
    xml.push_str(
        "  <key id=\"provenance\" for=\"edge\" attr.name=\"provenance\" attr.type=\"string\"/>\n",
    );
    xml.push_str(
        "  <key id=\"raw_import\" for=\"edge\" attr.name=\"raw_import\" attr.type=\"string\"/>\n",
    );

    xml.push_str("  <graph id=\"G\" edgedefault=\"directed\">\n");

    // Collect nodes.
    let nodes = graph.nodes_by_pagerank();
    for node in &nodes {
        if node.file_path.starts_with("__ext__:") {
            continue;
        }
        let id = xml_escape(&node.file_path);
        xml.push_str(&format!("    <node id=\"{}\">\n", id));
        xml.push_str(&format!(
            "      <data key=\"pagerank\">{:.4}</data>\n",
            node.pagerank
        ));
        if let Some(comm) = node.community {
            xml.push_str(&format!("      <data key=\"community\">{}</data>\n", comm));
        }
        xml.push_str(&format!(
            "      <data key=\"in_degree\">{}</data>\n",
            node.in_degree
        ));
        xml.push_str(&format!(
            "      <data key=\"out_degree\">{}</data>\n",
            node.out_degree
        ));
        xml.push_str(&format!(
            "      <data key=\"language\">{}</data>\n",
            node.language.name()
        ));
        xml.push_str("    </node>\n");
    }

    // Collect edges.
    let edges = graph.all_edges();
    for (i, (from, to, edge)) in edges.iter().enumerate() {
        if from.starts_with("__ext__:") || to.starts_with("__ext__:") {
            continue;
        }
        let from_esc = xml_escape(from);
        let to_esc = xml_escape(to);
        xml.push_str(&format!(
            "    <edge id=\"e{}\" source=\"{}\" target=\"{}\">\n",
            i, from_esc, to_esc
        ));
        xml.push_str(&format!(
            "      <data key=\"kind\">{:?}</data>\n",
            edge.kind
        ));
        xml.push_str(&format!(
            "      <data key=\"confidence\">{:?}</data>\n",
            edge.confidence
        ));
        xml.push_str(&format!(
            "      <data key=\"provenance\">{}</data>\n",
            edge.confidence.provenance()
        ));
        xml.push_str(&format!(
            "      <data key=\"raw_import\">{}</data>\n",
            xml_escape(&edge.raw_import)
        ));
        xml.push_str("    </edge>\n");
    }

    xml.push_str("  </graph>\n");
    xml.push_str("</graphml>\n");

    std::fs::write(&options.output_path, &xml)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::CodeGraph;
    use crate::language::Language;

    #[test]
    fn graphml_export_contains_nodes_and_edges() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/main.rs",
            "src/lib.rs",
            "crate::lib",
            Language::Rust,
            Language::Rust,
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.graphml");
        let opts = GraphmlExportOptions {
            output_path: path.clone(),
        };

        export_graphml(&g, &opts).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("<node id=\"src/main.rs\">"));
        assert!(content.contains("<node id=\"src/lib.rs\">"));
        assert!(content.contains("source=\"src/main.rs\""));
        assert!(content.contains("target=\"src/lib.rs\""));
        assert!(content.contains("<data key=\"kind\">Resolved</data>"));
        assert!(content.contains("<data key=\"provenance\">EXTRACTED</data>"));
    }

    #[test]
    fn graphml_xml_escaping() {
        assert_eq!(xml_escape("a&b"), "a&amp;b");
        assert_eq!(xml_escape("<tag>"), "&lt;tag&gt;");
        assert_eq!(xml_escape("\"quoted\""), "&quot;quoted&quot;");
    }

    #[test]
    fn graphml_excludes_external_by_default() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/main.rs",
            "src/lib.rs",
            "crate::lib",
            Language::Rust,
            Language::Rust,
        );
        g.add_external_edge("src/main.rs", "serde", Language::Rust);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.graphml");
        let opts = GraphmlExportOptions {
            output_path: path.clone(),
        };

        export_graphml(&g, &opts).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("__ext__"));
    }
}

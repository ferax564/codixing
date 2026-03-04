//! Token-budgeted repo map generation (Aider-style).

use crate::symbols::SymbolTable;

use super::CodeGraph;

/// Options for repo map generation.
#[derive(Debug, Clone)]
pub struct RepoMapOptions {
    /// Maximum number of cl100k tokens in the output.
    pub token_budget: usize,
    /// Only include files with PageRank >= this value.
    pub min_pagerank: f32,
    /// Whether to include the import list for each file.
    pub include_imports: bool,
    /// Whether to include function/type signatures.
    pub include_signatures: bool,
}

impl Default for RepoMapOptions {
    fn default() -> Self {
        Self {
            token_budget: 4096,
            min_pagerank: 0.0,
            include_imports: true,
            include_signatures: true,
        }
    }
}

/// Generate an Aider-style repo map from the graph and symbol table.
///
/// Files are sorted descending by PageRank. Each file section lists its symbols
/// and (optionally) its outgoing imports. Output is trimmed to `token_budget` tokens.
pub fn generate_repo_map(
    graph: &CodeGraph,
    symbols: &SymbolTable,
    options: &RepoMapOptions,
) -> String {
    let mut out = String::with_capacity(4096);
    out.push_str("# Repository Map\n\n");

    let nodes = graph.nodes_by_pagerank();

    for node in nodes {
        if node.file_path.starts_with("__ext__:") {
            continue;
        }
        if node.pagerank < options.min_pagerank {
            continue;
        }

        // File header with PageRank badge.
        out.push_str(&format!(
            "## {} [pr={:.3}]\n",
            node.file_path, node.pagerank
        ));

        // Symbols in this file.
        if options.include_signatures {
            let file_symbols = symbols.filter("", Some(&node.file_path));
            if !file_symbols.is_empty() {
                for sym in &file_symbols {
                    if let Some(ref sig) = sym.signature {
                        out.push_str(&format!("  {:?} `{}`\n", sym.kind, sig));
                    } else {
                        out.push_str(&format!("  {:?} `{}`\n", sym.kind, sym.name));
                    }
                }
            }
        }

        // Outgoing imports.
        if options.include_imports {
            let callees = graph.callees(&node.file_path);
            if !callees.is_empty() {
                out.push_str("  imports:\n");
                for dep in &callees {
                    out.push_str(&format!("    - {dep}\n"));
                }
            }
        }

        out.push('\n');

        // Check token budget.
        if count_tokens(&out) >= options.token_budget {
            out.push_str("# [truncated — token budget reached]\n");
            break;
        }
    }

    out
}

/// Approximate token count using tiktoken cl100k_base.
fn count_tokens(text: &str) -> usize {
    // Use tiktoken-rs for accurate cl100k counting.
    if let Ok(bpe) = tiktoken_rs::cl100k_base() {
        bpe.encode_with_special_tokens(text).len()
    } else {
        // Fallback: ~4 chars per token.
        text.len() / 4
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::CodeGraph;
    use crate::language::Language;
    use crate::symbols::SymbolTable;

    #[test]
    fn repo_map_contains_file_paths() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/main.rs",
            "src/parser.rs",
            "crate::parser",
            Language::Rust,
            Language::Rust,
        );

        let symbols = SymbolTable::new();
        let opts = RepoMapOptions::default();
        let map = generate_repo_map(&g, &symbols, &opts);

        assert!(map.contains("src/main.rs") || map.contains("src/parser.rs"));
    }

    #[test]
    fn repo_map_respects_token_budget() {
        let mut g = CodeGraph::new();
        for i in 0..100 {
            g.get_or_insert_node(&format!("src/file{i}.rs"), Language::Rust);
        }

        let symbols = SymbolTable::new();
        let opts = RepoMapOptions {
            token_budget: 50,
            ..Default::default()
        };
        let map = generate_repo_map(&g, &symbols, &opts);
        let tokens = if let Ok(bpe) = tiktoken_rs::cl100k_base() {
            bpe.encode_with_special_tokens(&map).len()
        } else {
            map.len() / 4
        };
        // Allow some overrun from the final file that crosses the threshold.
        assert!(tokens < 200, "expected < 200 tokens, got {tokens}");
    }
}

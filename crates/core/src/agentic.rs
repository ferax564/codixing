//! Agentic search mode -- structured tool-call interface for AI agents.
//!
//! Provides [`AgenticSearchSession`] which wraps an [`Engine`] and exposes
//! four methods (`search`, `read_file`, `explore_symbol`, `repo_map`) that
//! return [`AgenticResult`] formatted for consumption by AI coding agents.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};

use crate::engine::Engine;
use crate::error::{CodeforgeError, Result};
use crate::retriever::SearchQuery;

/// Result payload returned by all agentic operations.
///
/// Contains structured content text, an estimated token count, and
/// follow-up suggestions for the agent.
#[derive(Debug, Clone)]
pub struct AgenticResult {
    /// Structured text content formatted for agent consumption.
    pub content: String,
    /// Estimated token count (content.len() / 4 approximation).
    pub token_count: usize,
    /// Suggested follow-up actions the agent can take.
    pub suggestions: Vec<String>,
}

impl AgenticResult {
    fn new(content: String, suggestions: Vec<String>) -> Self {
        let token_count = content.len() / 4;
        Self {
            content,
            token_count,
            suggestions,
        }
    }
}

/// A session wrapper over [`Engine`] providing agentic search capabilities.
///
/// AI agents interact with this session to search code, read files, explore
/// symbol relationships, and get repository overviews -- all through structured
/// APIs that return [`AgenticResult`]s.
pub struct AgenticSearchSession<'a> {
    engine: &'a mut Engine,
}

impl<'a> AgenticSearchSession<'a> {
    /// Create a new agentic session backed by the given engine.
    pub fn new(engine: &'a mut Engine) -> Self {
        Self { engine }
    }

    /// Search the codebase using hybrid retrieval (BM25 + vector + trigram + graph).
    ///
    /// Returns results formatted as structured text with file paths, line ranges,
    /// scores, and content snippets.
    pub fn search(&mut self, query: &str, limit: usize) -> Result<AgenticResult> {
        let search_query = SearchQuery::new(query).with_limit(limit);
        let results = self.engine.hybrid_search(search_query)?;

        if results.is_empty() {
            return Ok(AgenticResult::new(
                format!("No results found for query: {query}"),
                vec![
                    "Try a broader search query".to_string(),
                    "Use repo_map to explore the codebase structure".to_string(),
                ],
            ));
        }

        let mut content = format!("## Search results for: {query}\n\n");
        let mut suggestions = Vec::new();
        let mut seen_files = Vec::new();
        let mut seen_symbols = Vec::new();

        for (i, result) in results.iter().enumerate() {
            content.push_str(&format!(
                "### Result {}\n**File:** {}\n**Lines:** {}-{}\n**Score:** {:.4}\n**Language:** {}\n",
                i + 1,
                result.file_path,
                result.line_start,
                result.line_end,
                result.score,
                result.language,
            ));

            if !result.signature.is_empty() {
                content.push_str(&format!("**Signature:** {}\n", result.signature));
            }

            content.push_str(&format!("```\n{}\n```\n\n", result.content.trim()));

            if !seen_files.contains(&result.file_path) {
                seen_files.push(result.file_path.clone());
            }

            // Extract symbol names from signatures for suggestions.
            if !result.signature.is_empty() {
                let sig = &result.signature;
                // Heuristic: extract the first word after "fn ", "struct ", etc.
                for keyword in &["fn ", "struct ", "enum ", "trait ", "pub fn "] {
                    if let Some(pos) = sig.find(keyword) {
                        let after = &sig[pos + keyword.len()..];
                        let name: String = after
                            .chars()
                            .take_while(|c| c.is_alphanumeric() || *c == '_')
                            .collect();
                        if !name.is_empty() && !seen_symbols.contains(&name) {
                            seen_symbols.push(name);
                        }
                    }
                }
            }
        }

        // Build suggestions based on results.
        for file in seen_files.iter().take(2) {
            suggestions.push(format!("Read file '{file}' for full context"));
        }
        for sym in seen_symbols.iter().take(2) {
            suggestions.push(format!("Explore symbol '{sym}' for callers/callees"));
        }

        Ok(AgenticResult::new(content, suggestions))
    }

    /// Read a file (or line range) from the indexed project.
    ///
    /// If `start_line` and `end_line` are provided, returns only that range
    /// (1-indexed, inclusive). Otherwise returns the entire file.
    pub fn read_file(
        &self,
        path: &str,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> Result<AgenticResult> {
        let root = &self.engine.config().root;
        let full_path = root.join(path);

        if !full_path.exists() {
            return Err(CodeforgeError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("file not found: {path}"),
            )));
        }

        let file = fs::File::open(&full_path)?;
        let reader = BufReader::new(file);
        let lines: Vec<String> = reader.lines().collect::<std::io::Result<Vec<_>>>()?;
        let total_lines = lines.len();

        let (start, end) = match (start_line, end_line) {
            (Some(s), Some(e)) => {
                let s = s.saturating_sub(1).min(total_lines);
                let e = e.min(total_lines);
                (s, e)
            }
            (Some(s), None) => {
                let s = s.saturating_sub(1).min(total_lines);
                (s, total_lines)
            }
            _ => (0, total_lines),
        };

        let selected: Vec<&String> = lines[start..end].iter().collect();

        let mut content = format!("## File: {path}\n");
        content.push_str(&format!(
            "**Lines:** {}-{} of {total_lines}\n\n```\n",
            start + 1,
            end
        ));
        for (i, line) in selected.iter().enumerate() {
            content.push_str(&format!("{:>4} | {}\n", start + i + 1, line));
        }
        content.push_str("```\n");

        let suggestions = vec![
            format!("Search for functions in '{path}'"),
            "Explore symbols defined in this file".to_string(),
        ];

        Ok(AgenticResult::new(content, suggestions))
    }

    /// Explore a symbol's relationships in the code graph.
    ///
    /// Builds the code graph if it does not already exist, then reports
    /// callers and callees of the named symbol.
    pub fn explore_symbol(&mut self, symbol_name: &str) -> Result<AgenticResult> {
        // Build graph if not already available.
        if self.engine.graph().is_none() {
            self.engine.build_graph()?;
        }

        let graph = self
            .engine
            .graph()
            .ok_or_else(|| CodeforgeError::Config("failed to build code graph".to_string()))?;

        // Find nodes matching the symbol name.
        let mut matching_indices = Vec::new();
        for idx in graph.node_indices() {
            if let Some(node) = graph.get_node(idx) {
                if node.name == symbol_name {
                    matching_indices.push(idx);
                }
            }
        }

        if matching_indices.is_empty() {
            // Try case-insensitive substring match.
            let lower = symbol_name.to_lowercase();
            for idx in graph.node_indices() {
                if let Some(node) = graph.get_node(idx) {
                    if node.name.to_lowercase().contains(&lower) {
                        matching_indices.push(idx);
                    }
                }
            }
        }

        if matching_indices.is_empty() {
            return Ok(AgenticResult::new(
                format!(
                    "Symbol '{symbol_name}' not found in the code graph.\nThe graph has {} nodes and {} edges.",
                    graph.node_count(),
                    graph.edge_count()
                ),
                vec![
                    format!("Search for '{symbol_name}' to find it in the codebase"),
                    "Use repo_map to see all top-level symbols".to_string(),
                ],
            ));
        }

        let mut content = format!("## Symbol exploration: {symbol_name}\n\n");

        for &idx in &matching_indices {
            let node = graph.get_node(idx).unwrap();
            content.push_str(&format!(
                "### {} ({:?})\n**File:** {}\n",
                node.name, node.kind, node.file
            ));
            if let Some(line) = node.line {
                content.push_str(&format!("**Line:** {line}\n"));
            }

            let callers = graph.callers(idx);
            let callees = graph.callees(idx);

            content.push_str(&format!("\n**Called by ({} callers):**\n", callers.len()));
            if callers.is_empty() {
                content.push_str("  (none)\n");
            } else {
                for caller in &callers {
                    content.push_str(&format!(
                        "  - {} ({:?}) in {}\n",
                        caller.name, caller.kind, caller.file
                    ));
                }
            }

            content.push_str(&format!("\n**Calls ({} callees):**\n", callees.len()));
            if callees.is_empty() {
                content.push_str("  (none)\n");
            } else {
                for callee in &callees {
                    content.push_str(&format!(
                        "  - {} ({:?}) in {}\n",
                        callee.name, callee.kind, callee.file
                    ));
                }
            }
            content.push('\n');
        }

        let mut suggestions = Vec::new();
        // Suggest reading the file containing the symbol.
        if let Some(node) = graph.get_node(matching_indices[0]) {
            suggestions.push(format!("Read file '{}' for full source", node.file));
        }
        suggestions.push(format!("Search for '{symbol_name}' usage patterns"));

        // Suggest exploring related symbols.
        let all_related: Vec<String> = matching_indices
            .iter()
            .flat_map(|&idx| {
                let mut names = Vec::new();
                for caller in graph.callers(idx) {
                    if caller.name != symbol_name && !names.contains(&caller.name) {
                        names.push(caller.name.clone());
                    }
                }
                for callee in graph.callees(idx) {
                    if callee.name != symbol_name && !names.contains(&callee.name) {
                        names.push(callee.name.clone());
                    }
                }
                names
            })
            .collect();

        for related in all_related.iter().take(2) {
            suggestions.push(format!("Explore symbol '{related}'"));
        }

        Ok(AgenticResult::new(content, suggestions))
    }

    /// Generate a PageRank-ordered repository map (like Aider's repo map).
    ///
    /// Builds the code graph if needed, computes PageRank scores, and formats
    /// a file-grouped symbol listing truncated to fit within `token_budget`.
    pub fn repo_map(&mut self, token_budget: usize) -> Result<AgenticResult> {
        // Build graph if not already available.
        if self.engine.graph().is_none() {
            self.engine.build_graph()?;
        }

        let graph = self
            .engine
            .graph()
            .ok_or_else(|| CodeforgeError::Config("failed to build code graph".to_string()))?;

        if graph.node_count() == 0 {
            return Ok(AgenticResult::new(
                "Repository map is empty -- no symbols found in the code graph.".to_string(),
                vec![
                    "Search for specific functionality".to_string(),
                    "Read a specific file to explore its contents".to_string(),
                ],
            ));
        }

        // Compute PageRank scores.
        let scores = graph.pagerank(0.85, 20);

        // Collect (score, node info) pairs and sort by score descending.
        let mut ranked: Vec<(f64, &crate::graph::SymbolNode)> = graph
            .node_indices()
            .filter_map(|idx| {
                let node = graph.get_node(idx)?;
                let score = scores.get(&idx).copied().unwrap_or(0.0);
                Some((score, node))
            })
            .collect();
        ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        // Group by file, preserving rank order within each file.
        let mut file_symbols: BTreeMap<&str, Vec<(f64, &crate::graph::SymbolNode)>> =
            BTreeMap::new();
        for (score, node) in &ranked {
            file_symbols
                .entry(node.file.as_str())
                .or_default()
                .push((*score, node));
        }

        // Build the repo map, tracking estimated token count.
        let mut content = String::from("## Repository Map\n\n");
        let budget_chars = token_budget * 4;
        let mut current_chars = content.len();

        // Sort files by their best-ranked symbol (highest PageRank first).
        let mut file_order: Vec<(&str, f64)> = file_symbols
            .iter()
            .map(|(file, syms)| {
                let max_score = syms.iter().map(|(s, _)| *s).fold(0.0_f64, f64::max);
                (*file, max_score)
            })
            .collect();
        file_order.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let mut truncated = false;
        'outer: for (file, _) in &file_order {
            let file_header = format!("### {file}\n");
            if current_chars + file_header.len() > budget_chars {
                truncated = true;
                break;
            }
            content.push_str(&file_header);
            current_chars += file_header.len();

            if let Some(syms) = file_symbols.get(file) {
                for (score, node) in syms {
                    let line_info = node.line.map(|l| format!(" L{l}")).unwrap_or_default();

                    let entry = format!(
                        "  {:?} {}{} (rank: {:.4})\n",
                        node.kind, node.name, line_info, score
                    );

                    if current_chars + entry.len() > budget_chars {
                        truncated = true;
                        break 'outer;
                    }

                    content.push_str(&entry);
                    current_chars += entry.len();
                }
            }
            content.push('\n');
            current_chars += 1;
        }

        if truncated {
            content.push_str("\n... (truncated to fit token budget)\n");
        }

        let token_count = content.len() / 4;
        let suggestions = vec![
            "Search for specific functionality".to_string(),
            "Explore a specific symbol for its relationships".to_string(),
        ];

        Ok(AgenticResult {
            content,
            token_count,
            suggestions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    use crate::config::IndexConfig;

    /// Create a temporary project with Rust source files for testing.
    fn setup_project() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();

        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();

        fs::write(
            src_dir.join("main.rs"),
            r#"
/// Entry point.
fn main() {
    let result = add(1, 2);
    greet("world");
    println!("{result}");
}

/// Add two numbers.
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

/// Greet someone.
pub fn greet(name: &str) {
    println!("Hello, {name}!");
}

pub struct Config {
    pub verbose: bool,
    pub threads: usize,
}
"#,
        )
        .unwrap();

        fs::write(
            src_dir.join("lib.rs"),
            r#"
/// A helper function.
pub fn helper() -> String {
    "help".to_string()
}

pub trait Processor {
    fn process(&self, input: &str) -> String;
}
"#,
        )
        .unwrap();

        (dir, root)
    }

    #[test]
    fn test_agentic_search() {
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);
        let mut engine = Engine::init(&root, config).unwrap();

        let mut session = AgenticSearchSession::new(&mut engine);
        let result = session.search("add", 5).unwrap();

        assert!(!result.content.is_empty());
        assert!(result.token_count > 0);
        assert!(result.content.contains("Search results for: add"));
        // Should contain at least one result.
        assert!(result.content.contains("Result 1"));
        // Suggestions should be non-empty.
        assert!(!result.suggestions.is_empty());
    }

    #[test]
    fn test_agentic_search_no_results() {
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);
        let mut engine = Engine::init(&root, config).unwrap();

        let mut session = AgenticSearchSession::new(&mut engine);
        let result = session.search("zzz_nonexistent_symbol_xyz", 5).unwrap();

        assert!(result.content.contains("No results found"));
        assert!(!result.suggestions.is_empty());
    }

    #[test]
    fn test_read_file() {
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);
        let mut engine = Engine::init(&root, config).unwrap();

        let mut session = AgenticSearchSession::new(&mut engine);
        let result = session.read_file("src/main.rs", None, None).unwrap();

        assert!(result.content.contains("File: src/main.rs"));
        assert!(result.content.contains("fn main()"));
        assert!(result.content.contains("pub fn add"));
        assert!(result.token_count > 0);
        assert!(!result.suggestions.is_empty());
    }

    #[test]
    fn test_read_file_line_range() {
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);
        let mut engine = Engine::init(&root, config).unwrap();

        let mut session = AgenticSearchSession::new(&mut engine);
        // Read only lines 3-6 (1-indexed).
        let result = session.read_file("src/main.rs", Some(3), Some(6)).unwrap();

        assert!(result.content.contains("File: src/main.rs"));
        // Line range should show 3-6 (with total count, markdown formatted).
        assert!(
            result.content.contains("3-6 of"),
            "expected '3-6 of' in content, got: {}",
            result.content
        );
        assert!(result.token_count > 0);
    }

    #[test]
    fn test_read_file_not_found() {
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);
        let mut engine = Engine::init(&root, config).unwrap();

        let mut session = AgenticSearchSession::new(&mut engine);
        let result = session.read_file("nonexistent.rs", None, None);

        assert!(result.is_err());
    }

    #[test]
    fn test_explore_symbol() {
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);
        let mut engine = Engine::init(&root, config).unwrap();

        let mut session = AgenticSearchSession::new(&mut engine);
        let result = session.explore_symbol("main").unwrap();

        assert!(result.content.contains("Symbol exploration: main"));
        assert!(result.content.contains("main"));
        assert!(result.token_count > 0);
        assert!(!result.suggestions.is_empty());
    }

    #[test]
    fn test_explore_symbol_not_found() {
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);
        let mut engine = Engine::init(&root, config).unwrap();

        let mut session = AgenticSearchSession::new(&mut engine);
        let result = session.explore_symbol("zzz_nonexistent_symbol").unwrap();

        assert!(result.content.contains("not found"));
        assert!(!result.suggestions.is_empty());
    }

    #[test]
    fn test_repo_map() {
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);
        let mut engine = Engine::init(&root, config).unwrap();

        let mut session = AgenticSearchSession::new(&mut engine);
        let result = session.repo_map(4096).unwrap();

        assert!(result.content.contains("Repository Map"));
        assert!(result.token_count > 0);
        // Should contain at least some symbol names.
        assert!(
            result.content.contains("main")
                || result.content.contains("add")
                || result.content.contains("helper"),
            "repo map should contain symbol names, got: {}",
            result.content
        );
        assert!(!result.suggestions.is_empty());
    }

    #[test]
    fn test_repo_map_fits_budget() {
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);
        let mut engine = Engine::init(&root, config).unwrap();

        let mut session = AgenticSearchSession::new(&mut engine);
        // Use a small budget to force truncation.
        let result = session.repo_map(64).unwrap();

        // Token count should not vastly exceed the budget.
        // We allow some slack for the truncation message itself.
        assert!(
            result.token_count <= 64 + 20,
            "repo_map token_count {} exceeded budget 64 by too much",
            result.token_count
        );
    }

    #[test]
    fn test_repo_map_large_budget() {
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);
        let mut engine = Engine::init(&root, config).unwrap();

        let mut session = AgenticSearchSession::new(&mut engine);
        let result = session.repo_map(8192).unwrap();

        // With a large budget, should contain symbol signatures/names.
        assert!(result.content.contains("Repository Map"));
        assert!(result.token_count <= 8192);
    }

    #[test]
    fn test_explore_builds_graph_lazily() {
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);
        let mut engine = Engine::init(&root, config).unwrap();

        // Graph should not exist yet.
        assert!(engine.graph().is_none());

        let mut session = AgenticSearchSession::new(&mut engine);
        // explore_symbol should build the graph automatically.
        let _result = session.explore_symbol("main").unwrap();

        // After explore, graph should exist (via the engine reference).
        // We need to drop the session to access engine again.
        drop(session);
        assert!(engine.graph().is_some());
    }
}

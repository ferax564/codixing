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
    /// Optional seed files to pin near the top of the map (task-local focus).
    /// Paths are matched as exact relative paths or suffixes.
    pub focus_files: Vec<String>,
    /// When true (default), skip pure test/fixture paths unless they are focus seeds.
    pub prefer_implementation: bool,
    /// Soft cap on symbols listed per file (keeps hub files from exploding).
    pub max_symbols_per_file: usize,
    /// Soft cap on imports listed per file.
    pub max_imports_per_file: usize,
}

impl Default for RepoMapOptions {
    fn default() -> Self {
        Self {
            token_budget: 4096,
            min_pagerank: 0.0,
            include_imports: true,
            include_signatures: true,
            focus_files: Vec::new(),
            prefer_implementation: true,
            max_symbols_per_file: 12,
            max_imports_per_file: 8,
        }
    }
}

/// Generate an Aider-style repo map from the graph and symbol table.
///
/// Files are sorted by task focus (if any), then PageRank. Each file section
/// lists its symbols and (optionally) its outgoing imports. Output is hard-trimmed
/// to `token_budget` tokens — never overshoots by more than a short truncation
/// marker.
pub fn generate_repo_map(
    graph: &CodeGraph,
    symbols: &SymbolTable,
    options: &RepoMapOptions,
) -> String {
    let budget = options.token_budget.max(32);
    let mut out = String::with_capacity(budget.saturating_mul(4).min(64 * 1024));
    out.push_str("# Repository Map\n\n");

    let nodes = graph.nodes_by_pagerank();
    let focus = &options.focus_files;

    // Partition: focused seeds first (stable order of seeds), then PageRank rest.
    let mut focused = Vec::new();
    let mut rest = Vec::new();
    for node in nodes {
        if node.file_path.starts_with("__ext__:") {
            continue;
        }
        if node.pagerank < options.min_pagerank && !is_focus_path(&node.file_path, focus) {
            continue;
        }
        if options.prefer_implementation
            && is_testish_path(&node.file_path)
            && !is_focus_path(&node.file_path, focus)
        {
            continue;
        }
        if is_focus_path(&node.file_path, focus) {
            focused.push(node);
        } else {
            rest.push(node);
        }
    }

    // Order focused nodes by the seed list order when possible.
    focused.sort_by_key(|node| {
        focus
            .iter()
            .position(|seed| path_matches(&node.file_path, seed))
            .unwrap_or(usize::MAX)
    });

    let mut truncated = false;
    for node in focused.into_iter().chain(rest) {
        let section = render_file_section(graph, symbols, options, node);
        let candidate = format!("{out}{section}");
        if count_tokens(&candidate) > budget {
            truncated = true;
            break;
        }
        out.push_str(&section);
    }

    if truncated {
        let marker = "\n# [truncated — token budget reached]\n";
        let with_marker = format!("{out}{marker}");
        if count_tokens(&with_marker) <= budget {
            out.push_str(marker);
        } else {
            // Hard slice to budget using char-level estimate as last resort.
            hard_trim_to_budget(&mut out, budget.saturating_sub(count_tokens(marker)));
            out.push_str(marker);
        }
    }

    // Final hard guarantee: never return more than budget + marker room.
    if count_tokens(&out) > budget {
        hard_trim_to_budget(&mut out, budget);
        if !out.contains("[truncated") {
            out.push_str("\n# [truncated — token budget reached]\n");
        }
    }

    out
}

fn render_file_section(
    graph: &CodeGraph,
    symbols: &SymbolTable,
    options: &RepoMapOptions,
    node: &super::CodeNode,
) -> String {
    let mut section = format!("## {} [pr={:.3}]\n", node.file_path, node.pagerank);

    if options.include_signatures {
        let mut file_symbols = symbols.filter("", Some(&node.file_path));
        // Prefer primary definitions over imports.
        file_symbols.sort_by_key(|s| match s.kind {
            crate::language::EntityKind::Import => 2,
            crate::language::EntityKind::Module => 1,
            _ => 0,
        });
        let limit = options.max_symbols_per_file.max(1);
        let total = file_symbols.len();
        for sym in file_symbols.into_iter().take(limit) {
            if let Some(ref sig) = sym.signature {
                section.push_str(&format!("  {:?} `{}`\n", sym.kind, sig));
            } else {
                section.push_str(&format!("  {:?} `{}`\n", sym.kind, sym.name));
            }
        }
        if total > limit {
            section.push_str(&format!("  … +{} more symbols\n", total - limit));
        }
    }

    if options.include_imports {
        let callees = graph.callees(&node.file_path);
        if !callees.is_empty() {
            section.push_str("  imports:\n");
            let limit = options.max_imports_per_file.max(1);
            for dep in callees.iter().take(limit) {
                section.push_str(&format!("    - {dep}\n"));
            }
            if callees.len() > limit {
                section.push_str(&format!("    … +{} more\n", callees.len() - limit));
            }
        }
    }

    section.push('\n');
    section
}

fn is_focus_path(path: &str, focus: &[String]) -> bool {
    focus.iter().any(|seed| path_matches(path, seed))
}

fn path_matches(path: &str, seed: &str) -> bool {
    path == seed || path.ends_with(seed) || seed.ends_with(path)
}

fn is_testish_path(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/test/")
        || path.contains("/__tests__/")
        || path.starts_with("tests/")
        || path.starts_with("test/")
        || path.ends_with("_test.rs")
        || path.ends_with("_test.py")
        || path.ends_with(".test.ts")
        || path.ends_with(".test.tsx")
        || path.ends_with(".spec.ts")
        || path.ends_with("_spec.rb")
}

/// Approximate token count using tiktoken cl100k_base.
fn count_tokens(text: &str) -> usize {
    // Use tiktoken-rs for accurate cl100k counting.
    if let Ok(bpe) = tiktoken_rs::cl100k_base() {
        bpe.encode_with_special_tokens(text).len()
    } else {
        // Fallback: ~4 chars per token.
        text.len().div_ceil(4)
    }
}

/// Hard-trim `text` so `count_tokens(text) <= budget` using binary search on
/// byte length (cl100k is roughly monotonic with prefix length).
fn hard_trim_to_budget(text: &mut String, budget: usize) {
    if budget == 0 {
        text.clear();
        return;
    }
    if count_tokens(text) <= budget {
        return;
    }
    let bytes = text.as_bytes();
    let mut lo = 0usize;
    let mut hi = bytes.len();
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        // Snap to char boundary.
        let mut end = mid;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        let candidate = &text[..end];
        if count_tokens(candidate) <= budget {
            lo = end + 1;
        } else {
            hi = end.saturating_sub(1);
        }
    }
    let mut end = lo.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    // Walk backward until under budget.
    while end > 0 && count_tokens(&text[..end]) > budget {
        end = end.saturating_sub(1);
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
    }
    text.truncate(end);
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
        let tokens = count_tokens(&map);
        // Hard budget: at most budget + small marker overhead.
        assert!(
            tokens <= 80,
            "expected hard budget near 50 tokens, got {tokens}: {map}"
        );
        assert!(map.contains("truncated") || tokens <= 50);
    }

    #[test]
    fn repo_map_focus_files_rank_first() {
        let mut g = CodeGraph::new();
        // High PR artificial hub
        for i in 0..20 {
            g.add_edge(
                &format!("src/leaf{i}.rs"),
                "src/hub.rs",
                "hub",
                Language::Rust,
                Language::Rust,
            );
        }
        g.get_or_insert_node("src/task_local.rs", Language::Rust);

        let symbols = SymbolTable::new();
        let opts = RepoMapOptions {
            token_budget: 400,
            focus_files: vec!["src/task_local.rs".into()],
            include_imports: false,
            include_signatures: false,
            ..Default::default()
        };
        let map = generate_repo_map(&g, &symbols, &opts);
        let task_pos = map.find("src/task_local.rs");
        let hub_pos = map.find("src/hub.rs");
        assert!(task_pos.is_some(), "focus file missing from map: {map}");
        if let (Some(t), Some(h)) = (task_pos, hub_pos) {
            assert!(t < h, "focus file should appear before hub: {map}");
        }
    }

    #[test]
    fn repo_map_skips_tests_by_default() {
        let mut g = CodeGraph::new();
        g.get_or_insert_node("src/lib.rs", Language::Rust);
        g.get_or_insert_node("tests/integration.rs", Language::Rust);
        let symbols = SymbolTable::new();
        let map = generate_repo_map(
            &g,
            &symbols,
            &RepoMapOptions {
                token_budget: 500,
                include_imports: false,
                include_signatures: false,
                ..Default::default()
            },
        );
        assert!(map.contains("src/lib.rs"));
        assert!(!map.contains("tests/integration.rs"));
    }
}

//! Compact, line-oriented renderings shared by the CLI and the daemon.
//!
//! These are the formats an agent reads most often, so they are built for
//! low token cost: one line per item, `path:start-end` locations with 1-based
//! inclusive line numbers (the same convention as `grep` and editors), no
//! padded tables, and no relevance scores (they are not comparable between
//! queries). Keeping them in one place guarantees that a command prints the
//! same thing whether it ran in-process or through a warm daemon.

use std::fmt::Write as _;

use crate::engine::SymbolReference;
use crate::engine::context_assembly::query_focused_lines;
use crate::retriever::SearchResult;
use crate::symbols::Symbol;

/// Lines of source shown under each search hit.
const SNIPPET_LINES: usize = 3;
/// Longest snippet line emitted, in bytes.
const SNIPPET_LINE_BYTES: usize = 160;

fn clip(line: &str, max_bytes: usize) -> &str {
    if line.len() <= max_bytes {
        return line;
    }
    let mut end = max_bytes;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    &line[..end]
}

/// Collapse internal whitespace so multi-line names and signatures stay on
/// one line.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Render ranked search results.
///
/// ```text
/// 1. crates/core/src/engine/sync.rs:2225-2238  pub fn sync(&mut self) -> Result<SyncStats>
///    2238| pub fn sync(&mut self) -> Result<SyncStats> {
/// ```
///
/// The snippet is the window of the chunk that best matches `query`, prefixed
/// with its real line number so it can be opened directly.
pub fn search_results(results: &[SearchResult], query: &str) -> String {
    let mut out = String::new();
    for (index, result) in results.iter().enumerate() {
        let (start, end) = result.display_lines();
        if result.is_doc() {
            let breadcrumb = if result.scope_chain.is_empty() {
                String::new()
            } else {
                format!("  \u{00a7} {}", result.scope_chain.join(" > "))
            };
            let _ = writeln!(
                out,
                "{}. {}:{}-{} [doc]{}",
                index + 1,
                result.file_path,
                start,
                end,
                breadcrumb
            );
        } else {
            let signature = result.signature.lines().next().unwrap_or("").trim();
            let signature = if signature.is_empty() {
                String::new()
            } else {
                format!("  {}", clip(signature, SNIPPET_LINE_BYTES))
            };
            let _ = writeln!(
                out,
                "{}. {}:{}-{}{}",
                index + 1,
                result.file_path,
                start,
                end,
                signature
            );
        }

        let (offset, window) = snippet_window(result, query);
        for (i, line) in window.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let _ = writeln!(
                out,
                "   {}| {}",
                start + (offset + i) as u64,
                clip(line.trim_end(), SNIPPET_LINE_BYTES)
            );
        }
    }
    out
}

/// Choose the snippet lines of a search hit.
///
/// When the hit's own signature line is in the chunk, the snippet starts
/// there: its gutter then names the exact line of the definition, which is
/// what a "go to definition" style query is after. Otherwise the window that
/// best matches the query is used.
fn snippet_window<'a>(result: &'a SearchResult, query: &str) -> (usize, Vec<&'a str>) {
    let signature = result.signature.lines().next().unwrap_or("").trim();
    if !signature.is_empty() && !result.is_doc() {
        let lines: Vec<&str> = result.content.lines().collect();
        if let Some(index) = lines
            .iter()
            .position(|line| line.trim_start().starts_with(signature))
        {
            let end = (index + SNIPPET_LINES).min(lines.len());
            return (index, lines[index..end].to_vec());
        }
    }
    query_focused_lines(&result.content, query, SNIPPET_LINES)
}

/// Render a symbol listing, one symbol per line.
///
/// `symbols` are the rows to show (already ordered and limited); `total` is
/// the number that matched before the limit, used for the trailer.
pub fn symbols(symbols: &[Symbol], total: usize) -> String {
    let mut out = String::new();
    for symbol in symbols {
        let (start, end) = symbol.display_lines();
        let _ = writeln!(
            out,
            "{:?} {}  {}:{}-{}",
            symbol.kind,
            one_line(&symbol.name),
            symbol.file_path,
            start,
            end
        );
    }
    if total > symbols.len() {
        let _ = writeln!(
            out,
            "… {} more (showing {} of {}; narrow with --file or a longer name, or raise --limit)",
            total - symbols.len(),
            symbols.len(),
            total
        );
    }
    out
}

/// Default number of rows a symbol lookup prints.
pub const DEFAULT_SYMBOL_LIMIT: usize = 50;

/// Outcome of preparing a symbol lookup for display.
pub struct SymbolListing {
    /// Rendered rows plus the "N more" trailer when the listing was cut.
    pub text: String,
    /// Number of rows rendered.
    pub shown: usize,
    /// Number of symbols that matched before the limit.
    pub total: usize,
    /// File of the best match, if any.
    pub first_file: Option<String>,
}

/// Filter, order, limit and render the result of a symbol name lookup.
///
/// Import rows are dropped unless `include_imports` is set: they name a symbol
/// without defining it and typically outnumber the definitions. `limit == 0`
/// means unlimited.
pub fn symbol_lookup(
    mut matches: Vec<Symbol>,
    query: &str,
    include_imports: bool,
    limit: usize,
) -> SymbolListing {
    if !include_imports {
        matches.retain(|s| !matches!(s.kind, crate::language::EntityKind::Import));
    }
    crate::symbols::sort_for_lookup(&mut matches, query);
    let total = matches.len();
    if limit > 0 {
        matches.truncate(limit);
    }
    SymbolListing {
        text: symbols(&matches, total),
        shown: matches.len(),
        total,
        first_file: matches.first().map(|s| s.file_path.clone()),
    }
}

/// Default number of rows a usage lookup prints.
pub const DEFAULT_REFERENCE_LIMIT: usize = 20;

/// Outcome of preparing a reference listing for display.
pub struct ReferenceListing {
    /// Rendered rows plus the "N more" trailer when the listing was cut.
    pub text: String,
    /// Number of rows rendered.
    pub shown: usize,
    /// Number of references before the limit.
    pub total: usize,
    /// Number of distinct files across all references.
    pub files: usize,
}

/// Where a reference kind sorts in a ranked listing: what a reader changing
/// the symbol needs first (the definition, then production code), last what
/// rarely matters (comments and prose).
fn reference_group(reference: &SymbolReference) -> u8 {
    let test = crate::symbols::is_test_like_path(&reference.file_path);
    match reference.kind.as_str() {
        "definition" => 0,
        "call" | "reference" if !test => 1,
        "import" if !test => 2,
        "call" | "reference" | "import" => 3,
        "comment" => 4,
        _ => 5,
    }
}

/// Order, limit and render symbol references.
///
/// One line per reference: `path:line:kind: source line` with 1-based line
/// numbers, so each row names the exact line to open. `ranked` orders by
/// usefulness (definition, production code, imports, tests, comments, docs);
/// otherwise rows are ordered by path and line. `limit == 0` means unlimited.
pub fn references(mut refs: Vec<SymbolReference>, limit: usize, ranked: bool) -> ReferenceListing {
    if ranked {
        refs.sort_by(|a, b| {
            reference_group(a)
                .cmp(&reference_group(b))
                .then_with(|| a.file_path.cmp(&b.file_path))
                .then_with(|| a.line.cmp(&b.line))
        });
    } else {
        refs.sort_by(|a, b| a.file_path.cmp(&b.file_path).then(a.line.cmp(&b.line)));
    }
    let total = refs.len();
    let files = refs
        .iter()
        .map(|r| r.file_path.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len();
    if limit > 0 {
        refs.truncate(limit);
    }

    let mut text = String::new();
    for reference in &refs {
        let _ = writeln!(
            text,
            "{}:{}:{}: {}",
            reference.file_path,
            reference.line + 1,
            reference.kind,
            clip(reference.context.trim(), SNIPPET_LINE_BYTES)
        );
    }
    if total > refs.len() {
        let _ = writeln!(
            text,
            "… {} more (showing {} of {} in {} files; raise --limit, filter with --file, or use --complete)",
            total - refs.len(),
            refs.len(),
            total,
            files
        );
    }
    ReferenceListing {
        text,
        shown: refs.len(),
        total,
        files,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::{EntityKind, Language, Visibility};

    fn result(path: &str, start: u64, end: u64, content: &str) -> SearchResult {
        SearchResult {
            chunk_id: "1".into(),
            file_path: path.into(),
            language: "Rust".into(),
            score: 1.0,
            line_start: start,
            line_end: end,
            signature: "fn alpha()".into(),
            scope_chain: vec![],
            content: content.into(),
        }
    }

    #[test]
    fn search_results_use_one_based_lines_and_focus_on_the_query() {
        let content = "/// docs\n/// more docs\nfn alpha() {\n    let budget = 1;\n}\n";
        let out = search_results(&[result("src/a.rs", 10, 15, content)], "budget");
        // 0-based [10, 15) is lines 11-15 for a reader.
        assert!(out.starts_with("1. src/a.rs:11-15  fn alpha()\n"), "{out}");
        // The matching line (4th of the chunk => line 14) is in the snippet.
        assert!(out.contains("   14|     let budget = 1;"), "{out}");
        assert!(!out.contains("score"), "{out}");
    }

    #[test]
    fn symbols_listing_is_one_line_per_symbol_with_trailer() {
        let symbol = Symbol {
            name: "multi\n  line".into(),
            kind: EntityKind::Struct,
            language: Language::Rust,
            file_path: "src/a.rs".into(),
            line_start: 4,
            line_end: 9,
            byte_start: 0,
            byte_end: 0,
            signature: None,
            scope: vec![],
            doc_comment: None,
            visibility: Visibility::Public,
            type_relations: vec![],
        };
        let out = symbols(&[symbol], 3);
        assert_eq!(out.lines().count(), 2, "{out}");
        assert!(
            out.starts_with("Struct multi line  src/a.rs:5-9\n"),
            "{out}"
        );
        assert!(out.contains("2 more (showing 1 of 3"), "{out}");
    }
}

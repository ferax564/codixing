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
use crate::engine::context_assembly::{
    AssembledContext, BudgetMode, ContextSnippet, query_focused_lines,
};
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

/// Render the cross-file context of a code location.
///
/// ```text
/// # Context: fs/read_write.c:460
/// Token budget: 1500 (strict) | Used: 1432
///
/// ## Primary L450-L478
/// <source>
///
/// ## Imports (2)
/// include/linux/fs.h:L1020-L1024
/// <source>
/// ```
///
/// `focus_line` is the 1-based line the caller asked about.
///
/// In [`BudgetMode::Strict`] the result never exceeds the requested budget,
/// measured with the real tokenizer on the text that is returned. The
/// assembler sizes its sections with a byte-based estimate, which
/// undercounts dense C and overlooks the headings, so the renderer has the
/// last word: it drops usage examples, then callees, then imports, and
/// finally narrows the primary chunk to a window around `focus_line`.
/// `Used:` reports that exact count.
pub fn location_context(ctx: &AssembledContext, focus_line: u64) -> String {
    use crate::engine::examples::ExampleKind;

    // Supplementary items in drop order: the last one goes first.
    enum Section {
        Imports,
        Callees,
        Examples,
    }
    let mut extras: Vec<(Section, String)> = Vec::new();
    for snippet in &ctx.imports {
        extras.push((Section::Imports, snippet_block(snippet)));
    }
    for snippet in &ctx.callees {
        extras.push((Section::Callees, snippet_block(snippet)));
    }
    for example in &ctx.examples {
        let kind = match example.kind {
            ExampleKind::Test => "TEST",
            ExampleKind::CallSite => "CALL",
            ExampleKind::DocBlock => "DOC",
        };
        extras.push((
            Section::Examples,
            format!(
                "[{kind}] {}:L{}-L{}\n{}\n",
                example.file_path,
                example.line_start + 1,
                example.line_end.max(example.line_start) + 1,
                example.context.trim_end()
            ),
        ));
    }

    let (first_line, _) = ctx.primary.display_lines();
    let primary_lines: Vec<&str> = ctx.primary.content.lines().collect();
    let focus_index =
        (focus_line.saturating_sub(first_line) as usize).min(primary_lines.len().saturating_sub(1));

    // `keep_extras` supplementary items and a window of `radius` lines
    // around the focus line (`None`: the whole primary chunk).
    let render = |keep_extras: usize, radius: Option<usize>, used: usize| -> String {
        let (from, to) = match radius {
            Some(radius) => (
                focus_index.saturating_sub(radius),
                (focus_index + radius + 1).min(primary_lines.len()),
            ),
            None => (0, primary_lines.len()),
        };
        let mut out = String::new();
        let _ = writeln!(out, "# Context: {}:{}", ctx.primary.file_path, focus_line);
        let over = if ctx.budget_mode == BudgetMode::Soft && used > ctx.requested_budget {
            " (over budget)"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "Token budget: {} ({}) | Used: {used}{over}",
            ctx.requested_budget,
            ctx.budget_mode.as_str(),
        );
        if over.is_empty() {
            // Nothing to explain.
        } else if let Some(reason) = &ctx.oversize_reason {
            let _ = writeln!(out, "Note: {reason}");
        }
        let shown_first = first_line + from as u64;
        let shown_last = (first_line + to as u64).saturating_sub(1).max(shown_first);
        let narrowed = if to - from < primary_lines.len() {
            format!(" (of L{}-L{})", first_line, ctx.primary.display_lines().1)
        } else {
            String::new()
        };
        let _ = writeln!(out, "\n## Primary L{shown_first}-L{shown_last}{narrowed}");
        for line in &primary_lines[from..to] {
            let _ = writeln!(out, "{line}");
        }

        let kept = &extras[..keep_extras];
        for (title, wanted) in [("Imports", 0u8), ("Callees", 1), ("Usage examples", 2)] {
            let blocks: Vec<&str> = kept
                .iter()
                .filter(|(section, _)| {
                    matches!(
                        (section, wanted),
                        (Section::Imports, 0) | (Section::Callees, 1) | (Section::Examples, 2)
                    )
                })
                .map(|(_, block)| block.as_str())
                .collect();
            if blocks.is_empty() {
                continue;
            }
            let _ = writeln!(out, "\n## {title} ({})", blocks.len());
            for block in blocks {
                out.push_str(block);
            }
        }
        out
    };

    // The `Used:` figure is part of the text it measures; settle it by
    // re-rendering until the count stops changing (digits rarely add a token).
    let settle = |keep_extras: usize, radius: Option<usize>| -> (String, usize) {
        let mut used = crate::formatter::count_tokens(&render(keep_extras, radius, 0));
        for _ in 0..4 {
            let text = render(keep_extras, radius, used);
            let actual = crate::formatter::count_tokens(&text);
            if actual == used {
                return (text, used);
            }
            used = actual;
        }
        (render(keep_extras, radius, used), used)
    };

    let (text, used) = settle(extras.len(), None);
    if ctx.budget_mode == BudgetMode::Soft || used <= ctx.requested_budget {
        return text;
    }

    // Strict and over: keep the largest prefix of supplementary items that
    // fits. Token count grows with the prefix, so bisect.
    let fits = |keep_extras: usize, radius: Option<usize>| -> Option<String> {
        let (text, used) = settle(keep_extras, radius);
        (used <= ctx.requested_budget).then_some(text)
    };
    if let Some(text) = fits(0, None) {
        let (mut low, mut high, mut best) = (0usize, extras.len(), text);
        while low < high {
            let mid = low + (high - low).div_ceil(2);
            match fits(mid, None) {
                Some(text) => {
                    best = text;
                    low = mid;
                }
                None => high = mid - 1,
            }
        }
        return best;
    }

    // The primary chunk alone is too large: narrow it around the focus line.
    let (mut low, mut high) = (0usize, primary_lines.len());
    let mut best = fits(0, Some(0));
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        match fits(0, Some(mid)) {
            Some(text) => {
                best = Some(text);
                low = mid;
            }
            None => high = mid - 1,
        }
    }
    // A budget below one line plus the header cannot be met; say so rather
    // than return nothing.
    best.unwrap_or_else(|| settle(0, Some(0)).0)
}

fn snippet_block(snippet: &ContextSnippet) -> String {
    format!(
        "{}:L{}-L{}\n{}\n",
        snippet.file_path,
        snippet.line_start + 1,
        snippet.line_end.max(snippet.line_start + 1),
        snippet.content.trim_end()
    )
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

    fn sample_context(mode: BudgetMode, budget: usize) -> AssembledContext {
        use crate::engine::examples::{ExampleKind, UsageExample};
        let body: String = (0..120)
            .map(|i| format!("\tret = do_step_{i}(ctx, &state->slots[{i}], GFP_KERNEL);\n"))
            .collect();
        let snippet = |path: &str, text: &str| ContextSnippet {
            file_path: path.to_string(),
            line_start: 9,
            line_end: 12,
            content: text.to_string(),
            relevance: 0.5,
        };
        AssembledContext {
            primary: SearchResult {
                chunk_id: "c".into(),
                file_path: "fs/read_write.c".into(),
                language: "C".into(),
                score: 1.0,
                line_start: 399,
                line_end: 519,
                signature: "ssize_t vfs_read(struct file *file)".into(),
                scope_chain: vec![],
                content: body,
            },
            imports: vec![
                snippet("include/linux/fs.h", "struct file {\n\tfmode_t f_mode;\n};"),
                snippet("include/linux/uio.h", "struct iov_iter;"),
            ],
            callees: vec![snippet(
                "fs/read_write.c",
                "int rw_verify_area(int, struct file *);",
            )],
            examples: vec![UsageExample {
                file_path: "fs/exec.c".into(),
                line_start: 40,
                line_end: 42,
                kind: ExampleKind::CallSite,
                context: "res = vfs_read(file, buf, count, &pos);".into(),
                distance: 1,
            }],
            total_tokens: 0,
            requested_budget: budget,
            budget_mode: mode,
            over_budget: false,
            oversize_reason: None,
        }
    }

    fn reported_tokens(out: &str) -> usize {
        out.lines()
            .find_map(|line| line.split("Used: ").nth(1))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("no `Used:` figure in {out}"))
    }

    #[test]
    fn strict_location_context_never_exceeds_the_budget() {
        // Regression: the assembler sizes sections with a byte estimate, so
        // `context` on C came out 13% over a strict 1,500-token budget.
        for budget in [5_000, 1_500, 600, 200, 80] {
            let ctx = sample_context(BudgetMode::Strict, budget);
            let out = location_context(&ctx, 460);
            let actual = crate::formatter::count_tokens(&out);
            assert!(
                actual <= budget,
                "{actual} tokens > budget {budget}:\n{out}"
            );
            assert_eq!(
                reported_tokens(&out),
                actual,
                "`Used:` must be the real count"
            );
            assert!(
                out.contains("do_step_60("),
                "the focus line (L460) must survive at budget {budget}:\n{out}"
            );
        }
    }

    #[test]
    fn strict_location_context_drops_examples_before_code() {
        let roomy = location_context(&sample_context(BudgetMode::Strict, 50_000), 460);
        assert!(roomy.contains("## Imports (2)"), "{roomy}");
        assert!(roomy.contains("## Callees (1)"), "{roomy}");
        assert!(roomy.contains("## Usage examples (1)"), "{roomy}");
        assert!(roomy.contains("## Primary L400-L519\n"), "{roomy}");

        // Sized to hold the primary chunk and little else.
        let primary_only =
            crate::formatter::count_tokens(&sample_context(BudgetMode::Strict, 0).primary.content);
        let tight = location_context(&sample_context(BudgetMode::Strict, primary_only + 90), 460);
        assert!(
            tight.contains("## Primary L400-L519\n"),
            "primary kept whole:\n{tight}"
        );
        assert!(!tight.contains("## Usage examples"), "{tight}");
    }

    #[test]
    fn soft_location_context_keeps_everything_and_says_when_it_is_over() {
        let ctx = sample_context(BudgetMode::Soft, 200);
        let out = location_context(&ctx, 460);
        assert!(
            out.contains("do_step_0(") && out.contains("do_step_119("),
            "{out}"
        );
        assert!(out.contains("(over budget)"), "{out}");
        assert_eq!(reported_tokens(&out), crate::formatter::count_tokens(&out));
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

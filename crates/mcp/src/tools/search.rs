//! Search and retrieval tool handlers.

use std::collections::HashMap;

use serde_json::Value;

use std::time::Instant;

use codixing_core::{
    Engine, SearchQuery, SearchResult, SessionEventKind, SharedEventType, SharedSessionEvent,
    Strategy,
};

use super::common::ProgressReporter;

// ── search arg structs ──────────────────────────────────────────────────────

struct SearchArgs {
    query_str: String,
    limit: usize,
    kind_filter: Option<String>,
    fetch_limit: usize,
    effective_strategy: Strategy,
}

// ── call_code_search helpers ────────────────────────────────────────────────

/// Parse and validate `call_code_search` arguments from the JSON payload.
fn parse_search_args(engine: &Engine, args: &Value) -> Result<SearchArgs, String> {
    let query_str = args
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Missing required argument: query".to_string())?
        .to_string();

    let strategy = match args.get("strategy").and_then(|v| v.as_str()) {
        Some("instant") => Strategy::Instant,
        Some("fast") => Strategy::Fast,
        Some("thorough") => Strategy::Thorough,
        Some("explore") => Strategy::Explore,
        Some("deep") => Strategy::Deep,
        Some("exact") => Strategy::Exact,
        Some("semantic") => Strategy::Semantic,
        _ => engine.detect_strategy(&query_str),
    };

    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

    // Optional type filter: "function", "struct", "enum", "trait", "class", "method", "type",
    // "const", "interface".
    let kind_filter = args
        .get("kind")
        .and_then(|v| v.as_str())
        .map(|k| k.to_lowercase());

    // When kind filter is active, over-fetch and use Instant strategy to:
    // 1. Get more results (definitions are rare among usage-heavy chunks)
    // 2. Skip adaptive truncation (which may cut definition chunks)
    let (fetch_limit, effective_strategy) = if kind_filter.is_some() {
        (limit * 5, Strategy::Instant)
    } else {
        (limit, strategy)
    };

    Ok(SearchArgs {
        query_str,
        limit,
        kind_filter,
        fetch_limit,
        effective_strategy,
    })
}

/// Build the `SearchQuery` from the validated args and raw JSON payload.
fn build_search_query(parsed: &SearchArgs, args: &Value) -> SearchQuery {
    let mut query = SearchQuery::new(&parsed.query_str)
        .with_limit(parsed.fetch_limit)
        .with_strategy(parsed.effective_strategy);

    if let Some(filter) = args.get("file_filter").and_then(|v| v.as_str()) {
        query = query.with_file_filter(filter);
    }

    // Extract optional multi-query reformulations for RRF fusion.
    let queries: Option<Vec<String>> = args.get("queries").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    });
    query.queries = queries;
    query
}

/// Run the engine search, streaming BM25 partial results via progress if available.
fn run_search(
    engine: &Engine,
    query: SearchQuery,
    fetch_limit: usize,
    report_progress: bool,
    progress: Option<&ProgressReporter>,
) -> codixing_core::Result<Vec<SearchResult>> {
    if let (true, Some(p)) = (report_progress, progress) {
        engine.search_with_progress(query, |phase, partial_results| {
            if phase == "bm25" && !partial_results.is_empty() {
                // Send BM25 partial results so the client can display them
                // while waiting for the full hybrid/reranked results.
                let partial: Vec<serde_json::Value> = partial_results
                    .iter()
                    .take(fetch_limit)
                    .map(|r| {
                        serde_json::json!({
                            "file_path": r.file_path,
                            "line_start": r.line_start,
                            "line_end": r.line_end,
                            "score": r.score,
                            "signature": r.signature,
                        })
                    })
                    .collect();
                p.report_with_data(
                    25,
                    &format!("BM25 phase: {} partial results", partial.len()),
                    serde_json::json!({ "partialResults": partial, "phase": "bm25" }),
                );
            } else if phase == "fused" || phase == "reranked" {
                p.report(75, &format!("{} phase complete", phase));
            }
        })
    } else {
        engine.search(query)
    }
}

/// Apply session and shared-session boosts to results, then re-sort by score.
fn apply_session_boost(engine: &Engine, results: &mut [SearchResult]) {
    let session = engine.session().clone();
    let shared = engine.shared_session();
    for r in results.iter_mut() {
        let agent_boost = session
            .compute_file_boost_with_graph(&r.file_path, &|file| engine.file_neighbors(file));
        let shared_boost = shared.get_file_boost(&r.file_path);
        let combined = agent_boost + shared_boost * 0.2;
        // Multiplicative: cap at 1.3× to avoid session dominating ranking.
        if combined > 0.0 {
            r.score *= 1.0 + combined.min(0.3);
        }
    }
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// Apply the `--kind` filter: narrow results to declaration sites and fall back
/// to the symbol table when BM25 misses definition chunks.
fn apply_kind_filter(
    engine: &Engine,
    results: Vec<SearchResult>,
    kind: &str,
    query_str: &str,
    limit: usize,
) -> Vec<SearchResult> {
    let prefixes: Vec<&str> = match kind {
        "function" | "fn" => vec!["fn ", "def ", "func ", "function "],
        "struct" => vec!["pub struct ", "struct "],
        "enum" => vec!["pub enum ", "enum "],
        "trait" => vec!["pub trait ", "trait "],
        "class" => vec!["class "],
        "method" => vec!["fn ", "def ", "func "],
        "interface" => vec!["interface ", "trait ", "protocol "],
        "type" => vec!["type ", "typedef ", "using "],
        "const" | "constant" => vec!["pub const ", "const "],
        "impl" => vec!["impl ", "impl<"],
        _ => vec![kind],
    };
    let query_lower = query_str.to_lowercase();

    // Filter results AND narrow content to the declaration site.
    // This ensures the declaration line is visible in the output
    // even for large chunks where it would be truncated.
    let mut filtered = Vec::new();
    for mut r in results {
        let sig_lower = r.signature.to_lowercase();
        if prefixes.iter().any(|p| sig_lower.contains(p)) {
            filtered.push(r);
            continue;
        }
        // Find the declaration line and extract -2/+8 lines of surrounding context.
        let lines: Vec<&str> = r.content.lines().collect();
        let decl_idx = lines.iter().position(|line| {
            let ll = line.to_lowercase();
            prefixes.iter().any(|p| ll.contains(p)) && ll.contains(&query_lower)
        });
        if let Some(idx) = decl_idx {
            let start = idx.saturating_sub(2);
            let end = (idx + 8).min(lines.len());
            let slice_len = end - start;
            r.content = lines[start..end].join("\n");
            r.line_start += start as u64;
            r.line_end = r.line_start + slice_len.saturating_sub(1) as u64;
            filtered.push(r);
        }
    }

    if filtered.is_empty() {
        // BM25 didn't return definition chunks. Fall back to the symbol table
        // which indexes all definitions by name.
        if let Ok(symbols) = engine.symbols(query_str, None) {
            for sym in &symbols {
                let sig = sym.signature.as_deref().unwrap_or("");
                let sig_lower = sig.to_lowercase();
                if prefixes.iter().any(|p| sig_lower.contains(p)) {
                    let content = sig.to_string();
                    filtered.push(SearchResult {
                        chunk_id: format!("sym-{}", sym.name),
                        file_path: sym.file_path.clone(),
                        language: format!("{:?}", sym.language),
                        score: 100.0,
                        line_start: sym.line_start as u64,
                        line_end: sym.line_end as u64,
                        signature: sig.to_string(),
                        scope_chain: sym.scope.clone(),
                        content,
                    });
                }
            }
        }
    }

    filtered.truncate(limit);
    filtered
}

/// Format the final output string for `call_code_search`.
fn format_search_output(
    engine: &Engine,
    results: &[SearchResult],
    limit: usize,
    kind_filter: &Option<String>,
) -> String {
    let session = engine.session().clone();
    let mut out = String::new();

    // Staleness warning when index is significantly out of date.
    let stale = engine.check_staleness();
    let total_stale = stale.modified_files + stale.new_files + stale.deleted_files;
    if stale.is_stale && total_stale > 10 {
        out.push_str(&format!(
            "> **Warning:** Index is stale ({} file(s) changed). Run `codixing sync .` to update.\n\n",
            total_stale
        ));
    }

    if results.len() < limit {
        out.push_str(&format!(
            "*Showing {} results (adaptively truncated at confidence boundary)*\n\n",
            results.len()
        ));
    }

    if let Some(focus) = session.focus_directory() {
        out.push_str(&format!("*focus: {focus}*\n\n"));
    }

    if kind_filter.is_some() {
        // Kind-filtered results have already been narrowed to the declaration
        // site. Render them directly to avoid the formatter's truncation hiding
        // the declaration line.
        for r in results {
            out.push_str(&format!(
                "// File: {} [L{}-L{}]",
                r.file_path, r.line_start, r.line_end
            ));
            if !r.signature.is_empty() {
                out.push_str(&format!(
                    " ({})",
                    r.signature.split('\n').next().unwrap_or("")
                ));
            }
            out.push_str(&format!("\n```\n{}\n```\n\n", r.content));
        }
    } else {
        out.push_str(&engine.format_results(results, Some(8000)));
    }

    if !engine.embeddings_ready() {
        let (done, total) = engine.embedding_progress();
        let note = format!(
            "**Note:** Embeddings in progress ({done}/{total}). Results are BM25-only; \
             quality will improve when embedding completes.\n\n"
        );
        out = format!("{note}{out}");
    }

    out
}

pub(crate) fn call_code_search(
    engine: &Engine,
    args: &Value,
    progress: Option<&ProgressReporter>,
) -> (String, bool) {
    let parsed = match parse_search_args(engine, args) {
        Ok(a) => a,
        Err(msg) => return (msg, true),
    };

    let query = build_search_query(&parsed, args);

    // Report progress for deep/thorough strategies that take longer.
    let report_progress = matches!(
        parsed.effective_strategy,
        Strategy::Deep | Strategy::Thorough | Strategy::Explore
    );
    if report_progress {
        if let Some(p) = progress {
            p.report(0, "Searching...");
        }
    }

    let search_result = run_search(engine, query, parsed.fetch_limit, report_progress, progress);

    match search_result {
        Ok(results) if results.is_empty() => {
            engine.session().record(SessionEventKind::Search {
                query: parsed.query_str,
                result_count: 0,
            });
            ("No results found.".to_string(), false)
        }
        Ok(mut results) => {
            if report_progress {
                if let Some(p) = progress {
                    p.report(80, "Post-processing results...");
                }
            }

            let agent_id = engine.session().session_id().to_string();
            engine.session().record(SessionEventKind::Search {
                query: parsed.query_str.clone(),
                result_count: results.len(),
            });

            // Record top search results in the shared session so other
            // agents benefit from this agent's search activity.
            for r in results.iter().take(3) {
                engine.shared_session().record(SharedSessionEvent {
                    timestamp: Instant::now(),
                    event_type: SharedEventType::Search,
                    file_path: r.file_path.clone(),
                    symbol: None,
                    agent_id: agent_id.clone(),
                });
            }

            apply_session_boost(engine, &mut results);

            // Apply kind filter if specified.
            if let Some(ref kind) = parsed.kind_filter {
                results = apply_kind_filter(engine, results, kind, &parsed.query_str, parsed.limit);
            }

            if report_progress {
                if let Some(p) = progress {
                    p.report(90, "Formatting results...");
                }
            }

            let out = format_search_output(engine, &results, parsed.limit, &parsed.kind_filter);
            (out, false)
        }
        Err(e) => (format!("Search error: {e}"), true),
    }
}

pub(crate) fn call_find_symbol(engine: &Engine, args: &Value) -> (String, bool) {
    let name = match args.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return ("Missing required argument: name".to_string(), true),
    };

    let file = args.get("file").and_then(|v| v.as_str());

    match engine.symbols(&name, file) {
        Ok(symbols) if symbols.is_empty() => {
            (format!("No symbols found matching '{name}'."), false)
        }
        Ok(symbols) => {
            // Record session event.
            let first_file = symbols.first().map(|s| s.file_path.clone());
            engine.session().record(SessionEventKind::SymbolLookup {
                name: name.clone(),
                file: first_file.clone(),
            });

            // Record in shared session for cross-agent context.
            if let Some(ref file) = first_file {
                engine.shared_session().record(SharedSessionEvent {
                    timestamp: Instant::now(),
                    event_type: SharedEventType::SymbolLookup,
                    file_path: file.clone(),
                    symbol: Some(name.clone()),
                    agent_id: engine.session().session_id().to_string(),
                });
            }

            let mut out = format!("Found {} symbol(s) matching '{name}':\n\n", symbols.len());
            for sym in &symbols {
                out.push_str(&format!(
                    "  {:?} `{}` \u{2014} {} (lines {}-{})\n",
                    sym.kind, sym.name, sym.file_path, sym.line_start, sym.line_end
                ));
                if let Some(sig) = &sym.signature {
                    if !sig.is_empty() {
                        out.push_str(&format!("    {sig}\n"));
                    }
                }
            }
            (out, false)
        }
        Err(e) => (format!("Symbol lookup error: {e}"), true),
    }
}

pub(crate) fn call_search_usages(engine: &Engine, args: &Value) -> (String, bool) {
    let symbol = match args.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return ("Missing required argument: symbol".to_string(), true),
    };
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
    let complete = args
        .get("complete")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Complete mode: deterministic, unranked, no cap.
    if complete {
        use codixing_core::ReferenceOptions;
        let refs = engine.symbol_references(
            &symbol,
            ReferenceOptions {
                complete: true,
                max_results: None,
            },
        );
        if refs.is_empty() {
            return (
                format!("No usages found for '{symbol}' (complete mode)."),
                false,
            );
        }
        let mut out = format!(
            "Found {} location(s) referencing `{symbol}` (complete, deterministic, no ranking):\n\n",
            refs.len()
        );
        for r in &refs {
            out.push_str(&format!("  {} L{} [{}]\n", r.file_path, r.line + 1, r.kind));
            if !r.context.is_empty() {
                out.push_str(&format!("    {}\n", r.context));
            }
        }
        return (out, false);
    }

    match engine.search_usages(&symbol, limit) {
        Ok(results) if results.is_empty() => (format!("No usages found for '{symbol}'."), false),
        Ok(results) => {
            let mut out = format!(
                "Found {} location(s) referencing `{symbol}`:\n\n",
                results.len()
            );
            for r in &results {
                out.push_str(&format!(
                    "  {} [L{}-L{}]",
                    r.file_path, r.line_start, r.line_end
                ));
                if !r.signature.is_empty() {
                    out.push_str(&format!("  \u{2014} {}", r.signature));
                }
                out.push('\n');
                if let Some(preview) = r.content.lines().find(|l| !l.trim().is_empty()) {
                    out.push_str(&format!("    {}\n", preview.trim()));
                }
            }
            (out, false)
        }
        Err(e) => (format!("Usage search error: {e}"), true),
    }
}

pub(crate) fn call_read_symbol(engine: &Engine, args: &Value) -> (String, bool) {
    let name = match args.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return ("Missing required argument: name".to_string(), true),
    };
    let file = args.get("file").and_then(|v| v.as_str());

    let symbols = match engine.symbols(name, file) {
        Ok(s) => s,
        Err(e) => return (format!("Symbol lookup error: {e}"), true),
    };

    if symbols.is_empty() {
        return (
            format!(
                "No symbol found matching '{name}'{}.",
                file.map(|f| format!(" in '{f}'")).unwrap_or_default()
            ),
            false,
        );
    }

    match engine.read_symbol_source(name, file) {
        Ok(None) => (
            format!(
                "Symbol '{name}' is in the index ({} match(es)) but the source file is not on disk.",
                symbols.len()
            ),
            true,
        ),
        Ok(Some(source)) => {
            let sym = &symbols[0];
            let mut out = format!(
                "// {:?} `{}` \u{2014} {} [L{}-L{}]\n```{}\n{}\n```",
                sym.kind,
                sym.name,
                sym.file_path,
                sym.line_start,
                sym.line_end,
                sym.language.name(),
                source,
            );
            if symbols.len() > 1 {
                out.push_str(&format!(
                    "\n\n*{} additional match(es):*\n",
                    symbols.len() - 1
                ));
                for s in symbols.iter().skip(1) {
                    out.push_str(&format!(
                        "  \u{2022} {:?} `{}` \u{2014} {} [L{}-L{}]\n",
                        s.kind, s.name, s.file_path, s.line_start, s.line_end
                    ));
                }
            }
            (out, false)
        }
        Err(e) => (format!("Read error: {e}"), true),
    }
}

pub(crate) fn call_stitch_context(engine: &Engine, args: &Value) -> (String, bool) {
    let query = match args.get("query").and_then(|v| v.as_str()) {
        Some(q) => q.to_string(),
        None => return ("Missing required argument: query".to_string(), true),
    };
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as usize;

    let sq = SearchQuery::new(&query).with_limit(limit);
    let results = match engine.search(sq) {
        Ok(r) => r,
        Err(e) => return (format!("Search error: {e}"), true),
    };

    if results.is_empty() {
        return (format!("No results found for '{query}'."), false);
    }

    let call_pattern = &*super::common::CALL_PATTERN;
    let mut stitched = String::new();
    let mut callee_sources: HashMap<String, String> = HashMap::new();

    stitched.push_str(&format!("## Stitched context for: {query}\n\n"));
    stitched.push_str("### Primary results\n\n");

    for r in &results {
        stitched.push_str(&format!(
            "**{}** L{}-{}\n```\n{}\n```\n\n",
            r.file_path,
            r.line_start,
            r.line_end,
            r.content.trim()
        ));

        for cap in call_pattern.captures_iter(&r.content) {
            if let Some(name) = cap.get(1) {
                let n = name.as_str().to_string();
                if let std::collections::hash_map::Entry::Vacant(e) = callee_sources.entry(n) {
                    if let Ok(Some(src)) = engine.read_symbol_source(e.key(), None) {
                        e.insert(src);
                    }
                }
            }
        }
    }

    if !callee_sources.is_empty() {
        stitched.push_str("### Attached callee definitions\n\n");
        for (name, src) in callee_sources.iter().take(8) {
            stitched.push_str(&format!("#### `{name}`\n```\n{}\n```\n\n", src.trim()));
        }
    }

    (stitched, false)
}

/// Handler for `assemble_context` — delegates to stitch_context.
pub(crate) fn call_assemble_context(engine: &Engine, args: &Value) -> (String, bool) {
    call_stitch_context(engine, args)
}

// ── call_explain helpers ────────────────────────────────────────────────────

/// Extract callee names from a symbol's source text, filtering out keywords
/// and the symbol itself.
fn extract_callees(definition: &str, symbol: &str) -> Vec<String> {
    let call_pattern = &*super::common::CALL_PATTERN;
    let keywords: std::collections::HashSet<&str> = [
        "if", "while", "for", "loop", "match", "return", "let", "use", "fn", "pub", "mod",
        "struct", "enum", "impl", "trait", "type",
    ]
    .iter()
    .copied()
    .collect();
    call_pattern
        .captures_iter(definition)
        .filter_map(|cap| {
            let name = cap.get(1)?.as_str().to_string();
            if keywords.contains(name.as_str()) || name == symbol {
                None
            } else {
                Some(name)
            }
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .take(10)
        .collect()
}

/// Append temporal context (change frequency + blame) for `def_file` to `out`.
fn append_temporal_context(
    engine: &Engine,
    out: &mut String,
    def_file: &str,
    syms: &[codixing_core::Symbol],
) {
    let (change_count, authors) = engine.file_change_frequency(def_file, 90);
    if change_count > 0 {
        out.push_str(&format!(
            "\n### Change history (last 90 days)\n**{}** commits by {}\n",
            change_count,
            if authors.len() <= 3 {
                authors.join(", ")
            } else {
                format!("{} authors", authors.len())
            }
        ));
    }
    // Show blame for the symbol's line range.
    if let Some(sym) = syms.first() {
        let blame = engine.get_blame(
            def_file,
            Some(sym.line_start as u64),
            Some(sym.line_end as u64),
        );
        if !blame.is_empty() {
            let blame_authors: std::collections::BTreeSet<&str> =
                blame.iter().map(|b| b.author.as_str()).collect();
            let latest = blame.iter().max_by_key(|b| &b.date);
            if let Some(latest) = latest {
                out.push_str(&format!(
                    "**Last modified:** {} by {} ({})\n",
                    latest.date,
                    latest.author,
                    if blame_authors.len() == 1 {
                        "sole author".to_string()
                    } else {
                        format!("{} contributors", blame_authors.len())
                    }
                ));
            }
        }
    }
}

/// Format the full explain output from the gathered data.
fn format_explain_output(
    engine: &Engine,
    symbol: &str,
    definition: &str,
    def_file: Option<&str>,
    syms: &[codixing_core::Symbol],
    usages: &[SearchResult],
    callees: &[String],
) -> String {
    let mut out = format!("## Explanation: `{symbol}`\n\n");

    out.push_str("### Definition\n```\n");
    out.push_str(definition);
    out.push_str("\n```\n\n");

    if let Some(f) = def_file {
        out.push_str(&format!("**Defined in:** `{f}`\n\n"));
    }

    if !usages.is_empty() {
        out.push_str(&format!("### Callers ({} usage sites)\n", usages.len()));
        for u in usages {
            out.push_str(&format!("  - `{}` L{}", u.file_path, u.line_start));
            if !u.signature.is_empty() {
                out.push_str(&format!("  \u{2014} {}", u.signature));
            }
            out.push('\n');
        }
        out.push('\n');
    }

    if !callees.is_empty() {
        out.push_str(&format!(
            "### Callees ({} functions called)\n",
            callees.len()
        ));
        for c in callees {
            out.push_str(&format!("  - `{c}`\n"));
        }
    }

    // Temporal context: change frequency and recent blame for the symbol.
    if let Some(f) = def_file {
        append_temporal_context(engine, &mut out, f, syms);
    }

    // Show previously explored related symbols from this session.
    let related: Vec<String> = usages
        .iter()
        .flat_map(|u| u.signature.split_whitespace())
        .filter(|w| w.len() > 2 && w.chars().all(|c| c.is_alphanumeric() || c == '_'))
        .map(|w| w.to_string())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let explored = engine.session().previously_explored(&related);
    if !explored.is_empty() {
        out.push_str("\n### Session context\nPreviously explored: ");
        let items: Vec<String> = explored
            .iter()
            .map(|(name, mins)| format!("`{name}` ({mins} min ago)"))
            .collect();
        out.push_str(&items.join(", "));
        out.push_str("\n\n");
    }

    out
}

pub(crate) fn call_explain(
    engine: &Engine,
    args: &Value,
    progress: Option<&ProgressReporter>,
) -> (String, bool) {
    let symbol = match args.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return ("Missing required argument: symbol".to_string(), true),
    };
    let file_hint = args.get("file").and_then(|v| v.as_str());

    if let Some(p) = progress {
        p.report(0, "Finding definition...");
    }

    let definition = match engine.read_symbol_source(&symbol, file_hint) {
        Ok(Some(src)) => src,
        Ok(None) => return (format!("Symbol '{symbol}' not found in the index."), false),
        Err(e) => return (format!("Error reading symbol: {e}"), true),
    };

    let syms = engine.symbols(&symbol, file_hint).unwrap_or_default();
    let def_file = syms.first().map(|s| s.file_path.clone());

    // Record session event.
    engine.session().record(SessionEventKind::SymbolLookup {
        name: symbol.clone(),
        file: def_file.clone(),
    });

    // Record in shared session for cross-agent context.
    if let Some(ref file) = def_file {
        engine.shared_session().record(SharedSessionEvent {
            timestamp: Instant::now(),
            event_type: SharedEventType::SymbolLookup,
            file_path: file.clone(),
            symbol: Some(symbol.clone()),
            agent_id: engine.session().session_id().to_string(),
        });
    }

    if let Some(p) = progress {
        p.report(33, "Searching callers...");
    }

    // Find actual call sites via BM25 search (symbol-level, not file-level).
    let usages = engine.search_usages(&symbol, 8).unwrap_or_default();

    if let Some(p) = progress {
        p.report(66, "Extracting callees...");
    }

    let callees = extract_callees(&definition, &symbol);

    let out = format_explain_output(
        engine,
        &symbol,
        &definition,
        def_file.as_deref(),
        &syms,
        &usages,
        &callees,
    );

    (out, false)
}

//! Search and retrieval tool handlers.

use std::collections::HashMap;

use serde_json::Value;

use std::time::Instant;

use codixing_core::{Engine, SearchQuery, SessionEventKind, SharedEventType, SharedSessionEvent, Strategy};

pub(crate) fn call_code_search(engine: &Engine, args: &Value) -> (String, bool) {
    let query_str = match args.get("query").and_then(|v| v.as_str()) {
        Some(q) => q.to_string(),
        None => return ("Missing required argument: query".to_string(), true),
    };

    let strategy = match args.get("strategy").and_then(|v| v.as_str()) {
        Some("instant") => Strategy::Instant,
        Some("fast") => Strategy::Fast,
        Some("thorough") => Strategy::Thorough,
        Some("explore") => Strategy::Explore,
        Some("deep") => Strategy::Deep,
        _ => engine.detect_strategy(&query_str),
    };

    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

    let mut query = SearchQuery::new(&query_str)
        .with_limit(limit)
        .with_strategy(strategy);

    if let Some(filter) = args.get("file_filter").and_then(|v| v.as_str()) {
        query = query.with_file_filter(filter);
    }

    match engine.search(query) {
        Ok(results) if results.is_empty() => {
            engine.session().record(SessionEventKind::Search {
                query: query_str,
                result_count: 0,
            });
            ("No results found.".to_string(), false)
        }
        Ok(mut results) => {
            let agent_id = engine.session().session_id().to_string();
            engine.session().record(SessionEventKind::Search {
                query: query_str.clone(),
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

            // Apply session boost to results and re-sort.
            // Combines per-agent session boost with cross-agent shared session boost.
            let session = engine.session().clone();
            let shared = engine.shared_session();
            for r in results.iter_mut() {
                let agent_boost = session.compute_file_boost_with_graph(&r.file_path, &|file| {
                    engine.file_neighbors(file)
                });
                let shared_boost = shared.get_file_boost(&r.file_path);
                // Apply shared boost at 0.2x weight to avoid over-boosting
                // from other agents' activity.
                r.score += agent_boost + shared_boost * 0.2;
            }
            results.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            // Include focus info if active.
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

            if let Some(focus) = session.focus_directory() {
                out.push_str(&format!("*focus: {focus}*\n\n"));
            }
            out.push_str(&engine.format_results(&results, Some(8000)));
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

pub(crate) fn call_explain(engine: &Engine, args: &Value) -> (String, bool) {
    let symbol = match args.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return ("Missing required argument: symbol".to_string(), true),
    };
    let file_hint = args.get("file").and_then(|v| v.as_str());

    let definition = match engine.read_symbol_source(&symbol, file_hint) {
        Ok(Some(src)) => src,
        Ok(None) => format!("Symbol '{symbol}' not found in the index."),
        Err(e) => format!("Error reading symbol: {e}"),
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

    // Find actual call sites via BM25 search (symbol-level, not file-level).
    let usages = engine.search_usages(&symbol, 8).unwrap_or_default();

    // Extract callees from the symbol's source code (functions it calls).
    let callees: Vec<String> = {
        let call_pattern = &*super::common::CALL_PATTERN;
        let keywords: std::collections::HashSet<&str> = [
            "if", "while", "for", "loop", "match", "return", "let", "use", "fn", "pub", "mod",
            "struct", "enum", "impl", "trait", "type",
        ]
        .iter()
        .copied()
        .collect();
        call_pattern
            .captures_iter(&definition)
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
    };

    let mut out = format!("## Explanation: `{symbol}`\n\n");

    out.push_str("### Definition\n```\n");
    out.push_str(&definition);
    out.push_str("\n```\n\n");

    if let Some(ref f) = def_file {
        out.push_str(&format!("**Defined in:** `{f}`\n\n"));
    }

    if !usages.is_empty() {
        out.push_str(&format!("### Callers ({} usage sites)\n", usages.len()));
        for u in &usages {
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
        for c in &callees {
            out.push_str(&format!("  - `{c}`\n"));
        }
    }

    // Temporal context: change frequency and recent blame for the symbol.
    if let Some(ref f) = def_file {
        let (change_count, authors) = engine.file_change_frequency(f, 90);
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
            let blame = engine.get_blame(f, Some(sym.line_start as u64), Some(sym.line_end as u64));
            if !blame.is_empty() {
                // Collect unique authors from blame.
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

    (out, false)
}

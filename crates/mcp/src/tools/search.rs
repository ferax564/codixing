//! Search and retrieval tool handlers.

use std::collections::HashMap;

use serde_json::Value;

use codixing_core::{Engine, SearchQuery, Strategy};

pub(crate) fn call_code_search(engine: &mut Engine, args: &Value) -> (String, bool) {
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
        Ok(results) if results.is_empty() => ("No results found.".to_string(), false),
        Ok(results) => (engine.format_results(&results, Some(8000)), false),
        Err(e) => (format!("Search error: {e}"), true),
    }
}

pub(crate) fn call_find_symbol(engine: &mut Engine, args: &Value) -> (String, bool) {
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

pub(crate) fn call_search_usages(engine: &mut Engine, args: &Value) -> (String, bool) {
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

pub(crate) fn call_read_symbol(engine: &mut Engine, args: &Value) -> (String, bool) {
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

pub(crate) fn call_stitch_context(engine: &mut Engine, args: &Value) -> (String, bool) {
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

    let call_pattern = regex::Regex::new(r"\b([a-z_][a-zA-Z0-9_]{2,})\s*\(").unwrap();
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

pub(crate) fn call_explain(engine: &mut Engine, args: &Value) -> (String, bool) {
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

    // Find actual call sites via BM25 search (symbol-level, not file-level).
    let usages = engine.search_usages(&symbol, 8).unwrap_or_default();

    // Extract callees from the symbol's source code (functions it calls).
    let callees: Vec<String> = {
        let call_pattern = regex::Regex::new(r"\b([a-z_][a-zA-Z0-9_]*)\s*\(").unwrap();
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

    (out, false)
}

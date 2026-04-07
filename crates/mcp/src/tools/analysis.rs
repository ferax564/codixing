//! Analysis tool handlers: complexity, review context, find tests, find similar,
//! generate onboarding, rename symbol, index status.

use std::collections::HashMap;

use serde_json::Value;

use codixing_core::complexity::{count_cyclomatic_complexity, risk_band};
use codixing_core::{Engine, EntityKind, RepoMapOptions, SearchQuery};

pub(crate) fn call_index_status(engine: &Engine) -> (String, bool) {
    let stats = engine.stats();
    let config = engine.config();

    let vector_status = if stats.vector_count > 0 {
        format!(
            "{} vectors (semantic search active, model: {:?}, contextual: {})",
            stats.vector_count, config.embedding.model, config.embedding.contextual_embeddings
        )
    } else if config.embedding.enabled {
        "0 vectors \u{2014} index was built without embeddings; re-run `codixing init .` to enable semantic search".to_string()
    } else {
        "disabled (BM25-only mode)".to_string()
    };

    let graph_status = if stats.graph_node_count > 0 {
        let symbol_part = if stats.symbol_node_count > 0 {
            format!(
                ", symbol graph: {} nodes, {} edges",
                stats.symbol_node_count, stats.symbol_edge_count
            )
        } else {
            String::new()
        };
        format!(
            "{} nodes, {} edges (PageRank graph active){}",
            stats.graph_node_count, stats.graph_edge_count, symbol_part
        )
    } else {
        "not available".to_string()
    };

    let strategies = if stats.vector_count > 0 && stats.graph_node_count > 0 {
        "instant, fast, thorough, explore (all strategies available)"
    } else if stats.vector_count > 0 {
        "instant, fast, thorough (no graph \u{2014} explore falls back to BM25)"
    } else if stats.graph_node_count > 0 {
        "instant, explore (no vectors \u{2014} fast/thorough fall back to BM25 + graph boost)"
    } else {
        "instant only (no vectors, no graph)"
    };

    let session = engine.session();
    let session_status = if session.is_enabled() {
        let event_count = session.event_count();
        let focus = session
            .focus_directory()
            .map(|f| format!(" (focus: {f})"))
            .unwrap_or_default();
        format!("{event_count} events{focus}")
    } else {
        "disabled".to_string()
    };

    let out = format!(
        "# Codixing Index Status\n\n\
         Files indexed:    {}\n\
         Code chunks:      {}\n\
         Symbols:          {}\n\
         Vector index:     {}\n\
         Dependency graph: {}\n\
         Session:          {}\n\n\
         Available strategies: {}\n\n\
         Root: {}\n",
        stats.file_count,
        stats.chunk_count,
        stats.symbol_count,
        vector_status,
        graph_status,
        session_status,
        strategies,
        config.root.display(),
    );
    (out, false)
}

pub(crate) fn call_check_staleness(engine: &Engine) -> (String, bool) {
    let report = engine.check_staleness();

    let last_sync_str = report
        .last_sync
        .and_then(|t| {
            t.duration_since(std::time::SystemTime::UNIX_EPOCH)
                .ok()
                .map(|d| {
                    let secs = d.as_secs();
                    let elapsed = std::time::SystemTime::now()
                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                        .map(|now| now.as_secs().saturating_sub(secs))
                        .unwrap_or(0);
                    if elapsed < 60 {
                        format!("{elapsed}s ago")
                    } else if elapsed < 3600 {
                        format!("{}m ago", elapsed / 60)
                    } else if elapsed < 86400 {
                        format!("{}h ago", elapsed / 3600)
                    } else {
                        format!("{}d ago", elapsed / 86400)
                    }
                })
        })
        .unwrap_or_else(|| "unknown".to_string());

    let status = if report.is_stale {
        "STALE"
    } else {
        "UP TO DATE"
    };

    let mut out = format!(
        "## Index Staleness: {status}\n\n\
         Last sync: {last_sync_str}\n\
         Modified files: {}\n\
         New files:      {}\n\
         Deleted files:  {}\n",
        report.modified_files, report.new_files, report.deleted_files,
    );

    if report.is_stale {
        out.push_str(&format!("\n**Suggestion:** {}\n", report.suggestion));
    }

    (out, false)
}

pub(crate) fn call_rename_symbol(engine: &mut Engine, args: &Value) -> (String, bool) {
    if engine.is_read_only() {
        return (
            "Cannot rename symbol: index is open in read-only mode.".to_string(),
            true,
        );
    }

    let old_name = match args.get("old_name").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return ("Missing required argument: old_name".to_string(), true),
    };
    let new_name = match args.get("new_name").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return ("Missing required argument: new_name".to_string(), true),
    };
    let file_filter = args
        .get("file_filter")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Validate the rename before applying it.
    let validation = engine.validate_rename(&old_name, &new_name, file_filter.as_deref());

    // Build validation summary for the output.
    let mut validation_summary = String::new();
    if !validation.is_safe {
        validation_summary.push_str("\n**Warnings:**\n");
        for conflict in &validation.conflicts {
            let kind_str = match conflict.kind {
                codixing_core::ConflictKind::NameCollision => "NAME COLLISION",
                codixing_core::ConflictKind::Shadowing => "SHADOWING",
                codixing_core::ConflictKind::ImportConflict => "IMPORT CONFLICT",
            };
            validation_summary.push_str(&format!("  - [{kind_str}] {}\n", conflict.message));
        }
        validation_summary.push('\n');
    }

    let root = engine.config().root.clone();

    // Find all indexed files.
    let files: Vec<std::path::PathBuf> = {
        let syms = engine.symbols("", None).unwrap_or_default();
        let mut seen = std::collections::BTreeSet::new();
        for s in &syms {
            seen.insert(s.file_path.clone());
        }
        seen.into_iter()
            .filter(|f| {
                file_filter
                    .as_ref()
                    .map(|ff| f.contains(ff.as_str()))
                    .unwrap_or(true)
            })
            .map(|rel| root.join(rel))
            .collect()
    };

    let mut modified = 0usize;
    let mut replacements = 0usize;
    let mut errors = Vec::new();

    for path in &files {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if !content.contains(old_name.as_str()) {
            continue;
        }
        let updated = content.replace(old_name.as_str(), new_name.as_str());
        let count = content.matches(old_name.as_str()).count();
        if let Err(e) = std::fs::write(path, &updated) {
            errors.push(format!("Write error for {}: {e}", path.display()));
            continue;
        }
        replacements += count;
        modified += 1;
        if let Err(e) = engine.reindex_file(path) {
            errors.push(format!("Reindex error for {}: {e}", path.display()));
        }
    }

    let _ = engine.persist_incremental();

    if !errors.is_empty() {
        return (
            format!(
                "Rename completed with errors ({modified} files, {replacements} replacements):{validation_summary}\n{}",
                errors.join("\n")
            ),
            true,
        );
    }

    (
        format!(
            "Renamed '{old_name}' \u{2192} '{new_name}' in {modified} file(s), \
             {replacements} total replacement(s). All affected files reindexed.{validation_summary}"
        ),
        false,
    )
}

pub(crate) fn call_find_tests(engine: &Engine, args: &Value) -> (String, bool) {
    let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
    let file_filter = args.get("file").and_then(|v| v.as_str()).unwrap_or("");

    // If a specific source file is given, try test-to-code mapping first.
    if !file_filter.is_empty() {
        let mappings = engine.find_tests_for_file(file_filter);
        if !mappings.is_empty() {
            let filtered: Vec<_> = if pattern.is_empty() {
                mappings
            } else {
                let pat = pattern.to_lowercase();
                mappings
                    .into_iter()
                    .filter(|m| {
                        m.test_file.to_lowercase().contains(&pat)
                            || m.source_file.to_lowercase().contains(&pat)
                    })
                    .collect()
            };
            if !filtered.is_empty() {
                let mut out = format!(
                    "## Tests for `{file_filter}` ({} mapping(s))\n\n",
                    filtered.len()
                );
                for m in &filtered {
                    out.push_str(&format!(
                        "  - **{}** (confidence: {:.0}%, {})\n",
                        m.test_file,
                        m.confidence * 100.0,
                        m.reason,
                    ));
                }
                out.push('\n');

                // Also list test symbols from mapped test files.
                let mut test_syms_out = String::new();
                for m in &filtered {
                    let syms = engine.symbols("", Some(&m.test_file)).unwrap_or_default();
                    let test_fns: Vec<_> = syms
                        .iter()
                        .filter(|s| {
                            let n = s.name.to_lowercase();
                            n.starts_with("test_")
                                || n.ends_with("_test")
                                || s.name.starts_with("Test")
                        })
                        .collect();
                    if !test_fns.is_empty() {
                        test_syms_out.push_str(&format!("**{}**\n", m.test_file));
                        let mut sorted = test_fns;
                        sorted.sort_by_key(|t| t.line_start);
                        for t in sorted {
                            test_syms_out.push_str(&format!(
                                "  L{:>4}  {:?}  {}\n",
                                t.line_start, t.kind, t.name
                            ));
                        }
                        test_syms_out.push('\n');
                    }
                }
                if !test_syms_out.is_empty() {
                    out.push_str("### Test functions in mapped files\n\n");
                    out.push_str(&test_syms_out);
                }

                return (out, false);
            }
        }
    }

    // Fallback: symbol-based discovery (original behavior).
    let syms = match engine.symbols(
        "",
        if file_filter.is_empty() {
            None
        } else {
            Some(file_filter)
        },
    ) {
        Ok(s) => s,
        Err(e) => return (format!("Symbol lookup error: {e}"), true),
    };

    // Test naming conventions:
    // - Name starts with "test_" or ends with "_test" (Rust, Python, C)
    // - Name starts with "Test" (Go: TestXxx)
    // - File path contains "test" or "spec"
    let is_test = |name: &str, file: &str| -> bool {
        let n = name.to_lowercase();
        let f = file.to_lowercase();
        n.starts_with("test_")
            || n.ends_with("_test")
            || name.starts_with("Test")
            || f.contains("test")
            || f.contains("spec")
    };

    let tests: Vec<_> = syms
        .iter()
        .filter(|s| {
            let matches_test = is_test(&s.name, &s.file_path);
            let matches_pattern = pattern.is_empty()
                || s.name.to_lowercase().contains(&pattern.to_lowercase())
                || s.file_path.to_lowercase().contains(&pattern.to_lowercase());
            matches_test && matches_pattern
        })
        .collect();

    if tests.is_empty() {
        return (
            format!(
                "No test functions found{}{}.",
                if !pattern.is_empty() {
                    format!(" matching '{pattern}'")
                } else {
                    String::new()
                },
                if !file_filter.is_empty() {
                    format!(" in '{file_filter}'")
                } else {
                    String::new()
                }
            ),
            false,
        );
    }

    let mut out = format!("## Test functions ({} found)\n\n", tests.len());
    let mut by_file: HashMap<String, Vec<&codixing_core::Symbol>> = HashMap::new();
    for t in &tests {
        by_file.entry(t.file_path.clone()).or_default().push(t);
    }
    let mut files: Vec<String> = by_file.keys().cloned().collect();
    files.sort();

    for file in &files {
        out.push_str(&format!("**{file}**\n"));
        if let Some(file_tests) = by_file.get(file) {
            let mut sorted = file_tests.to_vec();
            sorted.sort_by_key(|t| t.line_start);
            for t in sorted {
                out.push_str(&format!(
                    "  L{:>4}  {:?}  {}\n",
                    t.line_start, t.kind, t.name
                ));
            }
        }
        out.push('\n');
    }

    (out, false)
}

pub(crate) fn call_find_source_for_test(engine: &Engine, args: &Value) -> (String, bool) {
    let test_file = match args.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => return ("Missing required argument: file".to_string(), true),
    };

    let mappings = engine.find_source_for_test(test_file);

    if mappings.is_empty() {
        return (
            format!(
                "No source files found for test file '{test_file}'. \
                 The file may not follow standard test naming conventions, \
                 or the tested source may not be in the index."
            ),
            false,
        );
    }

    let mut out = format!(
        "## Source files tested by `{test_file}` ({} mapping(s))\n\n",
        mappings.len()
    );
    for m in &mappings {
        out.push_str(&format!(
            "  - **{}** (confidence: {:.0}%, {})\n",
            m.source_file,
            m.confidence * 100.0,
            m.reason,
        ));
    }
    out.push('\n');

    (out, false)
}

pub(crate) fn call_find_similar(engine: &Engine, args: &Value) -> (String, bool) {
    let symbol = match args.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return ("Missing required argument: symbol".to_string(), true),
    };
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

    // Read the symbol source to use as a query.
    let src = match engine.read_symbol_source(&symbol, None) {
        Ok(Some(s)) => s,
        Ok(None) => return (format!("Symbol `{symbol}` not found in the index."), true),
        Err(e) => return (format!("Error reading symbol: {e}"), true),
    };

    // Build a BM25-safe query: extract identifier words from the first 10 lines.
    let ident_re = regex::Regex::new(r"[a-zA-Z_][a-zA-Z0-9_]{1,}").unwrap();
    let query_text: String = src
        .lines()
        .take(10)
        .flat_map(|line| {
            ident_re
                .find_iter(line)
                .map(|m| m.as_str().to_string())
                .collect::<Vec<_>>()
        })
        .take(20)
        .collect::<Vec<_>>()
        .join(" ");

    let sq = SearchQuery::new(&query_text).with_limit(limit + 1);
    let results = match engine.search(sq) {
        Ok(r) => r,
        Err(e) => return (format!("Search error: {e}"), true),
    };

    // Filter out the symbol itself.
    let similar: Vec<_> = results
        .into_iter()
        .filter(|r| !r.signature.contains(&symbol) && r.content.len() > 20)
        .take(limit)
        .collect();

    if similar.is_empty() {
        return (
            format!(
                "No code similar to `{symbol}` found. The symbol may be unique in this codebase."
            ),
            false,
        );
    }

    let mut out = format!(
        "## Code similar to `{symbol}` ({} results)\n\n",
        similar.len()
    );
    for r in &similar {
        out.push_str(&format!(
            "**{}** L{}-{}  (score: {:.3})\n```\n{}\n```\n\n",
            r.file_path,
            r.line_start,
            r.line_end,
            r.score,
            r.content.trim()
        ));
    }
    (out, false)
}

pub(crate) fn call_get_complexity(engine: &Engine, args: &Value) -> (String, bool) {
    let file = match args.get("file").and_then(|v| v.as_str()) {
        Some(f) => f.to_string(),
        None => return ("Missing required argument: file".to_string(), true),
    };
    let min_cc = args
        .get("min_complexity")
        .and_then(|v| v.as_u64())
        .unwrap_or(1) as usize;

    let root = engine.config().root.clone();
    let abs_path = root.join(&file);

    let source = match std::fs::read_to_string(&abs_path) {
        Ok(s) => s,
        Err(e) => return (format!("Cannot read '{file}': {e}"), true),
    };

    // Get symbols for this file to know function boundaries.
    let syms = engine.symbols("", Some(&file)).unwrap_or_default();
    let mut fns: Vec<_> = syms
        .iter()
        .filter(|s| matches!(s.kind, EntityKind::Function | EntityKind::Method))
        .collect();
    fns.sort_by_key(|s| s.line_start);

    if fns.is_empty() {
        return (
            format!(
                "No functions found in '{file}'. File may not be indexed or contain no functions."
            ),
            false,
        );
    }

    let lines: Vec<&str> = source.lines().collect();

    let mut rows: Vec<(String, usize, &'static str, usize, usize)> = fns
        .iter()
        .map(|s| {
            let cc = count_cyclomatic_complexity(&lines, s.line_start, s.line_end);
            let band = risk_band(cc);
            (s.name.clone(), cc, band, s.line_start, s.line_end)
        })
        .filter(|(_, cc, _, _, _)| *cc >= min_cc)
        .collect();

    rows.sort_by(|a, b| b.1.cmp(&a.1));

    if rows.is_empty() {
        return (
            format!("No functions with complexity >= {min_cc} found in '{file}'."),
            false,
        );
    }

    let mut out = format!(
        "## Cyclomatic complexity: {file}\n\n\
         {:>6}  {:>10}  {:>8}  {}\n\
         {:-<6}  {:-<10}  {:-<8}  {:-<30}\n",
        "CC", "Risk", "Lines", "Function", "", "", "", ""
    );

    for (name, cc, band, start, end) in &rows {
        out.push_str(&format!(
            "{:>6}  {:>10}  L{:>4}-{:<4}  {}\n",
            cc, band, start, end, name
        ));
    }

    let avg = rows.iter().map(|(_, cc, _, _, _)| *cc).sum::<usize>() as f64 / rows.len() as f64;
    out.push_str(&format!(
        "\n{} function(s) analyzed, average CC: {:.1}\n",
        rows.len(),
        avg
    ));

    (out, false)
}

pub(crate) fn call_review_context(engine: &Engine, args: &Value) -> (String, bool) {
    let patch = match args.get("patch").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ("Missing required argument: patch".to_string(), true),
    };

    // 1. Parse changed files and hunks.
    let mut changed_files: Vec<String> = Vec::new();
    let mut hunk_ranges: Vec<(String, usize, usize)> = Vec::new();
    let mut current_file = String::new();
    let mut current_line: usize = 0;
    let mut hunk_start: usize = 0;

    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            current_file = rest.trim().to_string();
            if !changed_files.contains(&current_file) {
                changed_files.push(current_file.clone());
            }
        } else if line.starts_with("@@ ") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                let new_range = parts[2].trim_start_matches('+');
                if let Some((start, _)) = new_range.split_once(',') {
                    hunk_start = start.parse().unwrap_or(1);
                } else {
                    hunk_start = new_range.parse().unwrap_or(1);
                }
                current_line = hunk_start;
            }
        } else if line.starts_with('+') && !line.starts_with("+++") {
            if hunk_start > 0 {
                hunk_ranges.push((current_file.clone(), hunk_start, current_line));
            }
            current_line += 1;
        } else if !line.starts_with('-') {
            current_line += 1;
        }
    }

    // 2. Find symbols overlapping changed hunks.
    let mut overlapping_symbols: Vec<(String, String)> = Vec::new();
    for file in &changed_files {
        let syms = engine.symbols("", Some(file)).unwrap_or_default();
        let file_hunks: Vec<_> = hunk_ranges.iter().filter(|(f, _, _)| f == file).collect();
        for sym in &syms {
            for (_, hunk_s, hunk_e) in &file_hunks {
                if sym.line_start <= *hunk_e && sym.line_end >= *hunk_s {
                    overlapping_symbols.push((file.clone(), sym.name.clone()));
                    break;
                }
            }
        }
    }

    // 3. Impact prediction.
    let mut impact: HashMap<String, usize> = HashMap::new();
    for file in &changed_files {
        for caller in engine.callers(file) {
            *impact.entry(caller).or_insert(0) += 1;
        }
    }
    for f in &changed_files {
        impact.remove(f);
    }
    let mut ranked_impact: Vec<(String, usize)> = impact.into_iter().collect();
    ranked_impact.sort_by(|a, b| b.1.cmp(&a.1));
    ranked_impact.truncate(10);

    // 4. Cross-file context for top symbols.
    let mut sym_context: Vec<(String, String)> = Vec::new();
    for (_, sym_name) in overlapping_symbols.iter().take(3) {
        if let Ok(Some(src)) = engine.read_symbol_source(sym_name, None) {
            sym_context.push((sym_name.clone(), src));
        }
    }

    // Assemble output.
    let mut out = format!(
        "## Code Review Context\n\n### Changed files ({} total)\n",
        changed_files.len()
    );
    for f in &changed_files {
        out.push_str(&format!("  - {f}\n"));
    }

    if !overlapping_symbols.is_empty() {
        out.push_str("\n### Symbols in changed hunks\n");
        for (file, name) in &overlapping_symbols {
            out.push_str(&format!("  - `{name}` in `{file}`\n"));
        }
    }

    if !ranked_impact.is_empty() {
        out.push_str("\n### Potentially impacted files\n");
        for (file, _) in &ranked_impact {
            out.push_str(&format!("  - {file}\n"));
        }
    }

    if !sym_context.is_empty() {
        out.push_str("\n### Context for top changed symbols\n");
        for (name, src) in &sym_context {
            out.push_str(&format!("\n#### `{name}`\n```\n{}\n```\n", src.trim()));
        }
    }

    (out, false)
}

pub(crate) fn call_generate_onboarding(engine: &mut Engine) -> (String, bool) {
    let root = engine.config().root.clone();
    let output_path = root.join(".codixing/ONBOARDING.md");

    let stats = engine.stats();

    // Language breakdown via symbols.
    let syms = engine.symbols("", None).unwrap_or_default();
    let mut lang_counts: HashMap<String, usize> = HashMap::new();
    for s in &syms {
        let ext = std::path::Path::new(&s.file_path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("unknown")
            .to_string();
        *lang_counts.entry(ext).or_insert(0) += 1;
    }
    let mut lang_list: Vec<(String, usize)> = lang_counts.into_iter().collect();
    lang_list.sort_by(|a, b| b.1.cmp(&a.1));

    // Top files by PageRank.
    let repo_map = engine
        .repo_map(RepoMapOptions {
            token_budget: 3000,
            ..RepoMapOptions::default()
        })
        .unwrap_or_else(|| "Repo map not available (graph not built).".to_string());

    // Assemble onboarding doc.
    let mut doc = format!(
        "# Project Onboarding\n\n\
         > Generated by Codixing on {}\n\n\
         ## Index Statistics\n\n\
         | Metric | Value |\n\
         |--------|-------|\n\
         | Indexed files | {} |\n\
         | Code chunks | {} |\n\
         | Symbols | {} |\n\
         | Vector embeddings | {} |\n\
         | Graph nodes | {} |\n\
         | Graph edges | {} |\n\n",
        chrono_now(),
        stats.file_count,
        stats.chunk_count,
        stats.symbol_count,
        stats.vector_count,
        stats.graph_node_count,
        stats.graph_edge_count,
    );

    if !lang_list.is_empty() {
        doc.push_str("## Language Breakdown\n\n");
        for (lang, count) in lang_list.iter().take(10) {
            doc.push_str(&format!("  - `.{lang}`: {count} symbols\n"));
        }
        doc.push('\n');
    }

    doc.push_str("## Repository Map (PageRank-ranked)\n\n");
    doc.push_str(&repo_map);
    doc.push_str("\n\n---\n*Regenerate with:* `codixing-mcp generate_onboarding`\n");

    // Write to disk.
    if let Some(parent) = output_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return (format!("Failed to create .codixing/ directory: {e}"), true);
        }
    }
    if let Err(e) = std::fs::write(&output_path, &doc) {
        return (format!("Failed to write ONBOARDING.md: {e}"), true);
    }

    (
        format!(
            "Onboarding guide written to .codixing/ONBOARDING.md ({} bytes).\n\n{}",
            doc.len(),
            &doc[..doc.len().min(500)]
        ),
        false,
    )
}

pub(crate) fn call_change_impact(engine: &Engine, args: &Value) -> (String, bool) {
    let file = match args.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => return ("Error: 'file' parameter is required".to_string(), true),
    };

    let impact = engine.change_impact(file);

    let out = format!(
        "# Change Impact: {}\n\n\
         Blast radius: {} files\n\n\
         ## Direct dependents ({})\n{}\n\n\
         ## Transitive dependents ({})\n{}\n\n\
         ## Affected tests ({})\n{}",
        impact.file_path,
        impact.blast_radius,
        impact.direct_dependents.len(),
        if impact.direct_dependents.is_empty() {
            "None".to_string()
        } else {
            impact
                .direct_dependents
                .iter()
                .map(|d| format!("- {d}"))
                .collect::<Vec<_>>()
                .join("\n")
        },
        impact.transitive_dependents.len(),
        if impact.transitive_dependents.is_empty() {
            "None".to_string()
        } else {
            impact
                .transitive_dependents
                .iter()
                .map(|t| format!("- {t}"))
                .collect::<Vec<_>>()
                .join("\n")
        },
        impact.affected_tests.len(),
        if impact.affected_tests.is_empty() {
            "None".to_string()
        } else {
            impact
                .affected_tests
                .iter()
                .map(|t| format!("- {t}"))
                .collect::<Vec<_>>()
                .join("\n")
        },
    );
    (out, false)
}

/// Simple ISO-8601 timestamp without external dependencies.
fn chrono_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Convert Unix timestamp to ISO 8601 date (UTC).
    let days_since_epoch = (secs / 86400) as i64;

    // Compute year, month, day from days since 1970-01-01.
    // Uses the civil_from_days algorithm (Howard Hinnant).
    let z = days_since_epoch + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u32; // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}")
}

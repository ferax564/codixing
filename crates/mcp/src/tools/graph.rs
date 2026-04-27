//! Graph tool handlers: references, transitive deps, repo map, callers, callees, impact.

use std::collections::HashMap;

use serde_json::Value;

use codixing_core::{Engine, RepoMapOptions};

use super::common::ProgressReporter;

pub(crate) fn call_cross_imports(engine: &Engine, args: &Value) -> (String, bool) {
    let from = match args.get("from").and_then(|v| v.as_str()) {
        Some(f) => f.to_string(),
        None => return ("Missing required argument: from".to_string(), true),
    };
    let to = match args.get("to").and_then(|v| v.as_str()) {
        Some(t) => t.to_string(),
        None => return ("Missing required argument: to".to_string(), true),
    };
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    let pattern = args.get("pattern").and_then(|v| v.as_str());

    let mut ranked = engine.cross_imports_ranked(&from, &to, None);
    if let Some(pattern) = pattern {
        match filter_ranked_files_by_pattern(engine, ranked, pattern) {
            Ok(filtered) => ranked = filtered,
            Err(err) => return (format!("Invalid pattern `{pattern}`: {err}"), true),
        }
    }
    if let Some(limit) = limit {
        ranked.truncate(limit);
    }

    if ranked.is_empty() {
        if let Some(pattern) = pattern {
            return (
                format!(
                    "No files in \"{from}\" import from \"{to}\" and match pattern \"{pattern}\"."
                ),
                false,
            );
        }
        return (
            format!("No files in \"{from}\" import from \"{to}\"."),
            false,
        );
    }

    let mut out = String::new();
    for (file, score) in &ranked {
        out.push_str(&format!("{file} (score: {score:.3})\n"));
    }
    out.push_str(&format!(
        "\n{} file(s) in \"{}\" import from \"{}\"{}.",
        ranked.len(),
        from,
        to,
        pattern
            .map(|p| format!(" and match pattern \"{p}\""))
            .unwrap_or_default()
    ));

    (out, false)
}

fn filter_ranked_files_by_pattern(
    engine: &Engine,
    ranked: Vec<(String, f32)>,
    pattern: &str,
) -> codixing_core::Result<Vec<(String, f32)>> {
    let mut filtered = Vec::new();
    for (file, score) in ranked {
        let matches = engine.grep_code(pattern, false, Some(&file), 0, 1)?;
        if !matches.is_empty() {
            filtered.push((file, score));
        }
    }
    Ok(filtered)
}

pub(crate) fn call_get_references(engine: &Engine, args: &Value) -> (String, bool) {
    let file = match args.get("file").and_then(|v| v.as_str()) {
        Some(f) => f.to_string(),
        None => return ("Missing required argument: file".to_string(), true),
    };

    let callers = engine.callers(&file);
    let callees = engine.callees(&file);

    let mut out = format!("References for `{file}`:\n\n");

    out.push_str(&format!("**Imported by** ({} file(s)):\n", callers.len()));
    if callers.is_empty() {
        out.push_str("  (none)\n");
    } else {
        for c in &callers {
            out.push_str(&format!("  - {c}\n"));
        }
    }

    out.push_str(&format!("\n**Imports** ({} file(s)):\n", callees.len()));
    if callees.is_empty() {
        out.push_str("  (none)\n");
    } else {
        for c in &callees {
            out.push_str(&format!("  - {c}\n"));
        }
    }

    (out, false)
}

pub(crate) fn call_get_transitive_deps(engine: &Engine, args: &Value) -> (String, bool) {
    let file = match args.get("file").and_then(|v| v.as_str()) {
        Some(f) => f.to_string(),
        None => return ("Missing required argument: file".to_string(), true),
    };
    let depth = args.get("depth").and_then(|v| v.as_u64()).unwrap_or(3) as usize;

    let deps = engine.dependencies(&file, depth);

    if deps.is_empty() {
        return (
            format!(
                "No transitive dependencies found for `{file}` (depth={depth}).\n\
                     The file may not be in the graph, or it has no resolvable imports."
            ),
            false,
        );
    }

    let mut out = format!(
        "Transitive dependencies of `{file}` (depth \u{2264} {depth}) \u{2014} {} file(s):\n\n",
        deps.len()
    );
    for d in &deps {
        out.push_str(&format!("  - {d}\n"));
    }
    (out, false)
}

pub(crate) fn call_get_repo_map(engine: &Engine, args: &Value) -> (String, bool) {
    let token_budget = args
        .get("token_budget")
        .and_then(|v| v.as_u64())
        .unwrap_or(4000) as usize;

    let options = RepoMapOptions {
        token_budget,
        ..RepoMapOptions::default()
    };

    match engine.repo_map(options) {
        Some(map) if map.is_empty() => (
            "Repository map is empty (no files indexed or graph not built).".to_string(),
            false,
        ),
        Some(map) => (map, false),
        None => (
            "Repository map unavailable \u{2014} graph intelligence is disabled or not yet built. Run `codixing init .` to enable it.".to_string(),
            false,
        ),
    }
}

pub(crate) fn call_symbol_callers(engine: &Engine, args: &Value) -> (String, bool) {
    let symbol = match args.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return ("Missing required argument: symbol".to_string(), true),
    };
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

    // Try precise graph lookup first (AST-validated references)
    let precise = engine.symbol_callers_precise(&symbol, limit);
    if !precise.is_empty() {
        let mut out = format!(
            "## Callers of `{symbol}` ({} found, precise graph lookup)\n\n",
            precise.len()
        );
        for r in &precise {
            out.push_str(&format!(
                "  `{}` L{} [{}]\n",
                r.file_path,
                r.line + 1,
                r.kind
            ));
            if !r.context.is_empty() {
                out.push_str(&format!("    {}\n", r.context));
            }
        }
        return (out, false);
    }

    // Fall back to BM25 text search
    let usages = match engine.search_usages(&symbol, limit) {
        Ok(u) => u,
        Err(e) => return (format!("Error: {e}"), true),
    };

    if usages.is_empty() {
        return (
            format!(
                "No callers found for `{symbol}`. The symbol may not be called directly, or the call graph may not be available."
            ),
            false,
        );
    }

    let mut out = format!(
        "## Callers of `{symbol}` ({} found, text search fallback)\n\n",
        usages.len()
    );
    for u in &usages {
        out.push_str(&format!("  `{}` L{}", u.file_path, u.line_start));
        if !u.signature.is_empty() {
            out.push_str(&format!("  \u{2014} {}", u.signature));
        }
        out.push('\n');
        if let Some(preview) = u.content.lines().find(|l| !l.trim().is_empty()) {
            out.push_str(&format!("    {}\n", preview.trim()));
        }
    }
    (out, false)
}

pub(crate) fn call_symbol_callees(engine: &Engine, args: &Value) -> (String, bool) {
    let symbol = match args.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return ("Missing required argument: symbol".to_string(), true),
    };
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

    // Use precise AST-based callee extraction
    let mut callees = engine.symbol_callees_precise(&symbol, None);

    if callees.is_empty() {
        // Check if the symbol exists at all
        match engine.read_symbol_source(&symbol, None) {
            Ok(None) => return (format!("Symbol `{symbol}` not found in the index."), false),
            Err(e) => return (format!("Error: {e}"), true),
            Ok(Some(_)) => {}
        }
        return (
            format!(
                "No callees detected in `{symbol}`. May be a data type or the call graph was built without call extraction."
            ),
            false,
        );
    }

    callees.truncate(limit);

    let mut out = format!("## Callees of `{symbol}`\n\n");
    for callee in &callees {
        out.push_str(&format!("  - `{callee}`\n"));
    }
    (out, false)
}

/// Return a plain newline-separated list of files that import `path`.
///
/// Used by the CLI daemon proxy for `codixing callers <file>`. The output
/// deliberately contains only file paths (no markdown) so that the CLI can
/// print the lines directly and match in-process output.
pub(crate) fn call_file_callers(engine: &Engine, args: &Value) -> (String, bool) {
    let path = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p.to_string(),
        None => return ("Missing required argument: path".to_string(), true),
    };

    let callers = engine.callers(&path);
    if callers.is_empty() {
        return (String::new(), false);
    }
    (callers.join("\n") + "\n", false)
}

/// Return a plain newline-separated list of files that `path` imports.
///
/// Used by the CLI daemon proxy for `codixing callees <file>`. The output
/// deliberately contains only file paths (no markdown) so that the CLI can
/// print the lines directly and match in-process output.
pub(crate) fn call_file_callees(engine: &Engine, args: &Value) -> (String, bool) {
    let path = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p.to_string(),
        None => return ("Missing required argument: path".to_string(), true),
    };

    let callees = engine.callees(&path);
    if callees.is_empty() {
        return (String::new(), false);
    }
    (callees.join("\n") + "\n", false)
}

pub(crate) fn call_predict_impact(
    engine: &Engine,
    args: &Value,
    progress: Option<&ProgressReporter>,
) -> (String, bool) {
    let patch = match args.get("patch").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ("Missing required argument: patch".to_string(), true),
    };
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(15) as usize;

    if let Some(p) = progress {
        p.report(0, "Parsing diff...");
    }

    let mut changed_files: Vec<String> = Vec::new();
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            changed_files.push(rest.trim().to_string());
        }
    }

    if changed_files.is_empty() {
        return (
            "No file changes detected in the patch. Ensure it is a valid unified diff.".to_string(),
            true,
        );
    }

    // Extract changed function names from the patch for symbol-level impact.
    let mut changed_symbols: Vec<String> = Vec::new();
    for line in patch.lines() {
        // Unified diff function context: @@ -a,b +c,d @@ fn function_name
        if let Some(rest) = line.strip_prefix("@@") {
            if let Some(ctx) = rest.rsplit_once("@@").map(|(_, c)| c.trim()) {
                // Extract function/method name from context line.
                let name = extract_symbol_from_hunk_header(ctx);
                if let Some(name) = name {
                    changed_symbols.push(name);
                }
            }
        }
    }

    if let Some(p) = progress {
        p.report(33, "Computing graph impact...");
    }

    let mut impact: HashMap<String, usize> = HashMap::new();
    for file in &changed_files {
        let callers = engine.callers(file);
        for caller in callers {
            *impact.entry(caller).or_insert(0) += 1;
        }
        let transitive = engine.transitive_callers(file, 2);
        for t in transitive {
            *impact.entry(t).or_insert(0) += 1;
        }
    }

    // Symbol-level impact: find callers of changed functions, scoped to the
    // file being changed so we only get callers of the specific file+symbol
    // combination (not unrelated symbols with the same name in other files).
    let mut symbol_impact: Vec<(String, String)> = Vec::new();
    for sym_name in &changed_symbols {
        let all_callers = engine.symbol_callers_precise(sym_name, 100);
        // Filter to callers that reference the symbol defined in a changed file.
        let callers: Vec<_> = all_callers
            .into_iter()
            .filter(|c| {
                // Only include callers whose target is in one of the changed files.
                // The caller itself should NOT be in a changed file (that's the
                // impact we're measuring -- who else is affected).
                !changed_files.contains(&c.file_path)
            })
            .take(20)
            .collect();
        for caller in &callers {
            symbol_impact.push((caller.file_path.clone(), sym_name.clone()));
            *impact.entry(caller.file_path.clone()).or_insert(0) += 2;
        }
    }

    for f in &changed_files {
        impact.remove(f);
    }

    let mut ranked: Vec<(String, usize)> = impact.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    ranked.truncate(limit);

    let mut out = format!(
        "## Impact Prediction for {} changed file(s)\n\n",
        changed_files.len()
    );
    out.push_str("### Changed files\n");
    for f in &changed_files {
        out.push_str(&format!("  - {f}\n"));
    }

    if ranked.is_empty() {
        out.push_str("\n### Impact\nNo dependent files detected in the import graph.\n");
    } else {
        out.push_str(&format!(
            "\n### Most likely impacted files (top {})\n",
            ranked.len()
        ));
        for (file, score) in &ranked {
            out.push_str(&format!("  - {file}  (dependency depth score: {score})\n"));
        }
    }

    // Symbol-level impact details.
    if !symbol_impact.is_empty() {
        out.push_str("\n### Symbol-level callers of changed functions\n");
        let mut shown = std::collections::HashSet::new();
        for (file, sym) in &symbol_impact {
            let key = format!("{file}:{sym}");
            if shown.insert(key) {
                out.push_str(&format!("  - `{file}` calls `{sym}`\n"));
            }
        }
    }

    // Temporal: show change frequency for changed files.
    let mut has_temporal = false;
    for file in &changed_files {
        let (count, authors) = engine.file_change_frequency(file, 90);
        if count > 0 {
            if !has_temporal {
                out.push_str("\n### Change frequency (last 90 days)\n");
                has_temporal = true;
            }
            out.push_str(&format!(
                "  - {file}: {count} commits, {} author(s)\n",
                authors.len()
            ));
        }
    }

    (out, false)
}

/// Extract a function/method name from a unified diff hunk header context line.
///
/// Handles common patterns like `fn foo(`, `def foo(`, `function foo(`,
/// `pub fn foo(`, `async fn foo(`, etc.
fn extract_symbol_from_hunk_header(ctx: &str) -> Option<String> {
    let ctx = ctx.trim();
    if ctx.is_empty() {
        return None;
    }

    // Split on whitespace and look for known function keywords.
    let tokens: Vec<&str> = ctx.split_whitespace().collect();
    let fn_kw_pos = tokens
        .iter()
        .position(|&t| t == "fn" || t == "def" || t == "function" || t == "func");

    if let Some(pos) = fn_kw_pos {
        if let Some(name_token) = tokens.get(pos + 1) {
            // Strip generics and parens: `foo(` -> `foo`, `foo<T>` -> `foo`
            let name = name_token
                .split(&['(', '<', ':'][..])
                .next()
                .unwrap_or(name_token);
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }

    // Fallback: look for `name(` pattern (common in C/C++/Java/Go).
    // Matches tokens like `myFunction(` at the start of the line.
    if let Some(first) = tokens.first() {
        if let Some(name) = first.strip_suffix('(') {
            if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                return Some(name.to_string());
            }
        }
    }

    None
}

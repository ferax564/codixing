//! Graph tool handlers: references, transitive deps, repo map, callers, callees, impact.

use std::collections::HashMap;

use serde_json::Value;

use codixing_core::{Engine, RepoMapOptions};

pub(crate) fn call_get_references(engine: &mut Engine, args: &Value) -> (String, bool) {
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

pub(crate) fn call_get_transitive_deps(engine: &mut Engine, args: &Value) -> (String, bool) {
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

pub(crate) fn call_get_repo_map(engine: &mut Engine, args: &Value) -> (String, bool) {
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

pub(crate) fn call_symbol_callers(engine: &mut Engine, args: &Value) -> (String, bool) {
    let symbol = match args.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return ("Missing required argument: symbol".to_string(), true),
    };
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

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

    let mut out = format!("## Callers of `{symbol}` ({} found)\n\n", usages.len());
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

pub(crate) fn call_symbol_callees(engine: &mut Engine, args: &Value) -> (String, bool) {
    let symbol = match args.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return ("Missing required argument: symbol".to_string(), true),
    };
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

    let src = match engine.read_symbol_source(&symbol, None) {
        Ok(Some(s)) => s,
        Ok(None) => return (format!("Symbol `{symbol}` not found in the index."), false),
        Err(e) => return (format!("Error: {e}"), true),
    };

    let call_pattern = &*super::common::CALL_PATTERN;
    let keywords: std::collections::HashSet<&str> = [
        "if", "while", "for", "loop", "match", "return", "let", "use", "fn", "pub", "mod",
        "struct", "enum", "impl", "trait", "type",
    ]
    .iter()
    .copied()
    .collect();

    let mut callees: Vec<String> = call_pattern
        .captures_iter(&src)
        .filter_map(|cap| {
            let name = cap.get(1)?.as_str().to_string();
            if keywords.contains(name.as_str()) || name == symbol {
                None
            } else {
                Some(name)
            }
        })
        .collect::<std::collections::LinkedList<_>>()
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .take(limit)
        .collect();
    callees.sort();

    if callees.is_empty() {
        return (
            format!(
                "No callees detected in `{symbol}`. May be a data type or the call graph was built without call extraction."
            ),
            false,
        );
    }

    let mut out = format!("## Callees of `{symbol}`\n\n");
    for callee in &callees {
        out.push_str(&format!("  - `{callee}`\n"));
    }
    (out, false)
}

pub(crate) fn call_predict_impact(engine: &mut Engine, args: &Value) -> (String, bool) {
    let patch = match args.get("patch").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ("Missing required argument: patch".to_string(), true),
    };
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(15) as usize;

    let mut changed_files: Vec<String> = Vec::new();
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            changed_files.push(rest.trim().to_string());
        }
    }

    if changed_files.is_empty() {
        return (
            "No file changes detected in the patch. Ensure it is a valid unified diff.".to_string(),
            false,
        );
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

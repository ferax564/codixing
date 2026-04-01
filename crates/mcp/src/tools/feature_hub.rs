//! Feature hub composite tool handler.

use serde_json::Value;

use codixing_core::{Engine, SearchQuery, Strategy};

pub(crate) fn call_feature_hub(engine: &Engine, args: &Value) -> (String, bool) {
    let query = match args.get("query").and_then(|v| v.as_str()) {
        Some(q) => q,
        None => return ("Error: `query` parameter is required.".to_string(), true),
    };
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as usize;

    // Phase 1: Search for core files related to the feature.
    let search_query = SearchQuery::new(query)
        .with_limit(limit)
        .with_strategy(Strategy::Fast);
    let core_files = match engine.search(search_query) {
        Ok(results) => results,
        Err(e) => return (format!("Search error: {e}"), true),
    };

    if core_files.is_empty() {
        return (format!("No files found for: {query}"), false);
    }

    // Deduplicate core file paths (search may return multiple chunks per file),
    // then truncate to limit to ensure full coverage across files.
    let mut seen = std::collections::HashSet::new();
    let core_files: Vec<_> = core_files
        .into_iter()
        .filter(|r| seen.insert(r.file_path.clone()))
        .take(limit)
        .collect();
    let core_paths: Vec<String> = core_files.iter().map(|r| r.file_path.clone()).collect();

    // Phase 2: Gather deps/dependents/tests for each unique core file.
    let mut depends_on = std::collections::BTreeSet::new();
    let mut depended_by = std::collections::BTreeSet::new();
    let mut tests = std::collections::BTreeSet::new();

    for path in &core_paths {
        for callee in engine.callees(path) {
            if !core_paths.contains(&callee) {
                depends_on.insert(callee);
            }
        }
        for caller in engine.callers(path) {
            if !core_paths.contains(&caller) {
                depended_by.insert(caller);
            }
        }
        for mapping in engine.find_tests_for_file(path) {
            tests.insert(mapping.test_file);
        }
    }

    // Phase 3: Format structured output.
    let mut out = format!("## Feature: \"{query}\"\n\n### Core files\n");
    for result in &core_files {
        out.push_str(&format!(
            "  {} (score: {:.2})\n",
            result.file_path, result.score
        ));
    }

    if !depends_on.is_empty() {
        out.push_str("\n### Depends on\n");
        for dep in depends_on.iter().take(10) {
            out.push_str(&format!("  {dep}\n"));
        }
        if depends_on.len() > 10 {
            out.push_str(&format!("  ... and {} more\n", depends_on.len() - 10));
        }
    }

    if !depended_by.is_empty() {
        out.push_str("\n### Depended on by\n");
        for dep in depended_by.iter().take(10) {
            out.push_str(&format!("  {dep}\n"));
        }
        if depended_by.len() > 10 {
            out.push_str(&format!("  ... and {} more\n", depended_by.len() - 10));
        }
    }

    if !tests.is_empty() {
        out.push_str("\n### Tests\n");
        for t in tests.iter().take(10) {
            out.push_str(&format!("  {t}\n"));
        }
        if tests.len() > 10 {
            out.push_str(&format!("  ... and {} more\n", tests.len() - 10));
        }
    }

    (out, false)
}

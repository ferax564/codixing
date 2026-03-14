//! MCP handler for the `find_orphans` tool (Phase 14).

use codixing_core::{Engine, OrphanOptions};
use serde_json::Value;

pub(crate) fn call_find_orphans(engine: &Engine, args: &Value) -> (String, bool) {
    let mut options = OrphanOptions::default();

    if let Some(include) = args.get("include").and_then(|v| v.as_str()) {
        options.include_patterns = vec![include.to_string()];
    }
    if let Some(exclude) = args.get("exclude").and_then(|v| v.as_str()) {
        // Append user-specified exclude to the defaults.
        options.exclude_patterns.push(exclude.to_string());
    }
    if let Some(check_dynamic) = args.get("check_dynamic").and_then(|v| v.as_bool()) {
        options.check_dynamic_refs = check_dynamic;
    }
    if let Some(limit) = args.get("limit").and_then(|v| v.as_u64()) {
        options.limit = limit as usize;
    }

    let orphans = engine.find_orphans(options);

    if orphans.is_empty() {
        return (
            "No orphan files detected. All indexed files have at least one importer.".to_string(),
            false,
        );
    }

    let mut out = format!(
        "## Orphan Files ({} found)\n\n\
         | File | Confidence | Symbols | Reason |\n\
         |------|------------|---------|--------|\n",
        orphans.len()
    );
    for o in &orphans {
        out.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            o.file_path, o.confidence, o.symbol_count, o.reason
        ));
    }

    let certain_count = orphans
        .iter()
        .filter(|o| o.confidence == codixing_core::OrphanConfidence::Certain)
        .count();
    if certain_count > 0 {
        out.push_str(&format!(
            "\n**{certain_count}** file(s) with `certain` confidence are strong dead-code candidates."
        ));
    }

    (out, false)
}

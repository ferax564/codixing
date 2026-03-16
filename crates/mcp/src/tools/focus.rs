//! Focus map MCP tool: context-aware repo map seeded by recently touched files.

use serde_json::Value;

use codixing_core::{Engine, FocusMapOptions};

/// Handle the `focus_map` MCP tool call.
///
/// When `seed_files` is provided, uses those as PPR seeds.
/// Otherwise auto-detects from git (unstaged + staged + recent commits).
pub(crate) fn call_focus_map(engine: &Engine, args: &Value) -> (String, bool) {
    let max_files = args.get("max_files").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
    let include_symbols = args
        .get("include_symbols")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let options = FocusMapOptions {
        max_files,
        include_symbols,
        ..FocusMapOptions::default()
    };

    // Try explicit seed files first, then fall back to git auto-detection.
    let entries = if let Some(seeds_val) = args.get("seed_files") {
        let seeds: Vec<String> = if let Some(arr) = seeds_val.as_array() {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        } else if let Some(s) = seeds_val.as_str() {
            // Accept a single string too.
            vec![s.to_string()]
        } else {
            return (
                "Invalid seed_files: expected array of strings or a single string".to_string(),
                true,
            );
        };

        if seeds.is_empty() {
            engine.focus_map_from_git(&options)
        } else {
            let seed_refs: Vec<&str> = seeds.iter().map(|s| s.as_str()).collect();
            engine.focus_map(&seed_refs, &options)
        }
    } else {
        engine.focus_map_from_git(&options)
    };

    if entries.is_empty() {
        return (
            "No focus map generated. Possible reasons:\n\
             - No graph intelligence available (run `codixing init .` to build it)\n\
             - No seed files provided and no recent git changes detected\n\
             - Seed files not found in the dependency graph"
                .to_string(),
            false,
        );
    }

    // Format output.
    let seed_count = entries
        .iter()
        .filter(|e| e.relationship.contains("seed"))
        .count();
    let mut out = format!(
        "# Focus Map ({} files, {} seed(s))\n\n",
        entries.len(),
        seed_count,
    );

    out.push_str("| Rank | File | Score | Relationship |\n");
    out.push_str("|------|------|-------|--------------|\n");

    for (i, entry) in entries.iter().enumerate() {
        out.push_str(&format!(
            "| {} | `{}` | {:.3} | {} |\n",
            i + 1,
            entry.file_path,
            entry.rank,
            entry.relationship,
        ));
    }

    if include_symbols {
        out.push_str("\n## Key Symbols\n\n");
        for entry in &entries {
            if entry.symbols.is_empty() {
                continue;
            }
            out.push_str(&format!("**{}**\n", entry.file_path));
            for sym in &entry.symbols {
                out.push_str(&format!("  - `{sym}`\n"));
            }
            out.push('\n');
        }
    }

    (out, false)
}

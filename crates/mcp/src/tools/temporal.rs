//! Temporal code context tool handlers (Phase 13b).
//!
//! Tools: get_hotspots, search_changes, get_blame.

use serde_json::Value;

use codixing_core::Engine;

pub(crate) fn call_get_hotspots(engine: &mut Engine, args: &Value) -> (String, bool) {
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(15) as usize;
    let days = args.get("days").and_then(|v| v.as_u64()).unwrap_or(90);

    let hotspots = engine.get_hotspots(limit, days);

    if hotspots.is_empty() {
        return (
            "No hotspots found. The project may not be a git repository or have no recent commits."
                .to_string(),
            false,
        );
    }

    let mut out = format!(
        "## File Hotspots (last {} days, top {})\n\n",
        days,
        hotspots.len()
    );
    out.push_str("| File | Commits | Authors | Score |\n");
    out.push_str("|------|---------|---------|-------|\n");
    for h in &hotspots {
        out.push_str(&format!(
            "| {} | {} | {} | {:.3} |\n",
            h.file_path, h.commit_count, h.author_count, h.score
        ));
    }
    out.push_str(
        "\n*Files with the highest change frequency are more likely to contain bugs or be under active development.*"
    );

    (out, false)
}

pub(crate) fn call_search_changes(engine: &mut Engine, args: &Value) -> (String, bool) {
    let query = args.get("query").and_then(|v| v.as_str());
    let file_filter = args.get("file").and_then(|v| v.as_str());
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

    let entries = engine.search_changes(query, file_filter, limit);

    if entries.is_empty() {
        let mut msg = "No matching changes found".to_string();
        if let Some(q) = query {
            msg.push_str(&format!(" for query '{q}'"));
        }
        if let Some(f) = file_filter {
            msg.push_str(&format!(" in file '{f}'"));
        }
        msg.push('.');
        return (msg, false);
    }

    let mut out = format!("## Recent Changes ({} commit(s))\n\n", entries.len());
    for entry in &entries {
        out.push_str(&format!(
            "### `{}` \u{2014} {} ({}, {})\n",
            entry.commit, entry.subject, entry.author, entry.date_relative
        ));
        if !entry.files.is_empty() {
            for f in &entry.files {
                out.push_str(&format!("  - {f}\n"));
            }
        }
        out.push('\n');
    }

    (out, false)
}

pub(crate) fn call_get_blame(engine: &mut Engine, args: &Value) -> (String, bool) {
    let file = match args.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => return ("Missing required argument: file".to_string(), true),
    };
    let line_start = args.get("line_start").and_then(|v| v.as_u64());
    let line_end = args.get("line_end").and_then(|v| v.as_u64());

    let blame = engine.get_blame(file, line_start, line_end);

    if blame.is_empty() {
        return (
            format!("No blame data available for '{file}'. The file may not be tracked by git."),
            false,
        );
    }

    let mut out = format!("## Blame for `{file}`");
    if let (Some(start), Some(end)) = (line_start, line_end) {
        out.push_str(&format!(" (L{start}-L{end})"));
    }
    out.push_str("\n\n");

    // Group consecutive lines by same commit for compact output.
    let mut i = 0;
    while i < blame.len() {
        let start = i;
        let commit = &blame[i].commit;
        while i < blame.len() && blame[i].commit == *commit {
            i += 1;
        }
        let end = i - 1;
        let b = &blame[start];
        out.push_str(&format!(
            "**L{}-L{}** `{}` {} ({})\n",
            blame[start].line_number, blame[end].line_number, b.commit, b.author, b.date
        ));
        out.push_str("```\n");
        for line in &blame[start..=end] {
            out.push_str(&format!("{:>4} {}\n", line.line_number, line.content));
        }
        out.push_str("```\n\n");
    }

    (out, false)
}

//! File I/O tool handlers: read, list, grep, outline, write, edit, delete, apply_patch, run_tests.

use std::path::PathBuf;

use serde_json::Value;

use codixing_core::{Engine, GrepMatch};

pub(crate) fn call_read_file(engine: &mut Engine, args: &Value) -> (String, bool) {
    let file = match args.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => return ("Missing required argument: file".to_string(), true),
    };

    let line_start = args.get("line_start").and_then(|v| v.as_u64());
    let line_end = args.get("line_end").and_then(|v| v.as_u64());
    let token_budget = args
        .get("token_budget")
        .and_then(|v| v.as_u64())
        .unwrap_or(4000) as usize;

    match engine.read_file_range(file, line_start, line_end) {
        Ok(None) => (
            format!(
                "File not found: '{file}'. \
                 Ensure the path is relative to the project root (e.g. 'src/main.rs')."
            ),
            true,
        ),
        Ok(Some(content)) => {
            let max_chars = token_budget * 4;
            let (body, truncated) = if content.len() > max_chars {
                (&content[..max_chars], true)
            } else {
                (content.as_str(), false)
            };

            let range_label = match (line_start, line_end) {
                (Some(s), Some(e)) => format!(" [L{s}-L{e}]"),
                (Some(s), None) => format!(" [L{s}-]"),
                (None, Some(e)) => format!(" [-L{e}]"),
                (None, None) => String::new(),
            };

            let mut out = format!("// File: {file}{range_label}\n```\n{body}\n```");
            if truncated {
                out.push_str(&format!(
                    "\n\n*(output truncated at {token_budget} tokens \u{2014} \
                     use line_start/line_end to read a specific section)*"
                ));
            }
            (out, false)
        }
        Err(e) => (format!("Read error: {e}"), true),
    }
}

pub(crate) fn call_grep_code(engine: &mut Engine, args: &Value) -> (String, bool) {
    let pattern = match args.get("pattern").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ("Missing required argument: pattern".to_string(), true),
    };

    let literal = args
        .get("literal")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let file_glob = args.get("file_glob").and_then(|v| v.as_str());
    let context_lines = args
        .get("context_lines")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .min(5) as usize;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;

    match engine.grep_code(pattern, literal, file_glob, context_lines, limit) {
        Err(e) => (format!("grep_code error: {e}"), true),
        Ok(matches) if matches.is_empty() => (format!("No matches found for `{pattern}`."), false),
        Ok(matches) => (format_grep_matches(pattern, &matches), false),
    }
}

fn format_grep_matches(pattern: &str, matches: &[GrepMatch]) -> String {
    let mut out = format!("Found {} match(es) for `{}`:\n\n", matches.len(), pattern);
    let mut current_file = String::new();
    for m in matches {
        if m.file_path != current_file {
            current_file = m.file_path.clone();
            out.push_str(&format!("## {}\n", current_file));
        }
        for (offset, line) in m.before.iter().enumerate() {
            let ln = m.line_number as usize - m.before.len() + offset;
            out.push_str(&format!("  {:>5}  {}\n", ln, line));
        }
        out.push_str(&format!("\u{2192} {:>5}  {}\n", m.line_number, m.line));
        for (offset, line) in m.after.iter().enumerate() {
            let ln = m.line_number as usize + 1 + offset;
            out.push_str(&format!("  {:>5}  {}\n", ln, line));
        }
        if !m.before.is_empty() || !m.after.is_empty() {
            out.push('\n');
        }
    }
    out
}

pub(crate) fn call_list_files(engine: &mut Engine, args: &Value) -> (String, bool) {
    let pattern = args.get("pattern").and_then(|v| v.as_str());
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(200) as usize;

    let stats = engine.stats();
    let all_files: Vec<String> = {
        let syms = engine.symbols("", None).unwrap_or_default();
        let mut seen = std::collections::BTreeSet::new();
        for s in &syms {
            seen.insert(s.file_path.clone());
        }
        seen.into_iter().collect()
    };

    let mut filtered: Vec<String> = match pattern {
        Some(pat) => {
            let g = glob::Pattern::new(pat).ok();
            all_files
                .into_iter()
                .filter(|f| {
                    if let Some(ref g) = g {
                        g.matches(f)
                    } else {
                        f.contains(pat)
                    }
                })
                .collect()
        }
        None => all_files,
    };

    filtered.truncate(limit);

    if filtered.is_empty() {
        return (
            "No indexed files found matching the filter.".to_string(),
            false,
        );
    }

    let mut out = format!(
        "Indexed files ({} total, {} shown):\n\n",
        stats.file_count,
        filtered.len()
    );
    for f in &filtered {
        out.push_str(&format!("  {f}\n"));
    }
    (out, false)
}

pub(crate) fn call_outline_file(engine: &mut Engine, args: &Value) -> (String, bool) {
    let file = match args.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => return ("Missing required argument: file".to_string(), true),
    };

    let syms = match engine.symbols("", Some(file)) {
        Ok(s) => s,
        Err(e) => return (format!("Symbol lookup error: {e}"), true),
    };

    if syms.is_empty() {
        return (
            format!(
                "No symbols found in '{file}'. File may not be indexed or contain no extractable symbols."
            ),
            false,
        );
    }

    let mut sorted = syms;
    sorted.sort_by_key(|s| s.line_start);

    let mut out = format!("## Symbol outline: {file}\n\n");
    for s in &sorted {
        let scope = if s.scope.is_empty() {
            String::new()
        } else {
            format!(" [{}]", s.scope.join("::"))
        };
        out.push_str(&format!(
            "  L{:>4}\u{2013}{:<4}  {:12}  {}{}\n",
            s.line_start,
            s.line_end,
            format!("{:?}", s.kind),
            s.name,
            scope
        ));
    }
    out.push_str(&format!("\n{} symbols total.\n", sorted.len()));
    (out, false)
}

// ---------------------------------------------------------------------------
// Write tools
// ---------------------------------------------------------------------------

fn resolve_safe_path(engine: &Engine, rel: &str) -> Result<PathBuf, String> {
    let root = engine.config().root.clone();
    let candidate = root.join(rel);

    let mut normalized = PathBuf::new();
    for part in candidate.components() {
        match part {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            c => normalized.push(c),
        }
    }

    if !normalized.starts_with(&root) {
        return Err(format!(
            "Path '{rel}' escapes the project root \u{2014} operation denied."
        ));
    }

    Ok(normalized)
}

pub(crate) fn call_write_file(engine: &mut Engine, args: &Value) -> (String, bool) {
    let file = match args.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => return ("Missing required argument: file".to_string(), true),
    };
    let content = match args.get("content").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return ("Missing required argument: content".to_string(), true),
    };

    let abs_path = match resolve_safe_path(engine, file) {
        Ok(p) => p,
        Err(e) => return (e, true),
    };

    if let Some(parent) = abs_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return (
                format!("Failed to create directories for '{file}': {e}"),
                true,
            );
        }
    }

    if let Err(e) = std::fs::write(&abs_path, content) {
        return (format!("Failed to write '{file}': {e}"), true);
    }

    let line_count = content.lines().count();
    let byte_count = content.len();

    match engine
        .reindex_file(&abs_path)
        .and_then(|()| engine.persist_incremental())
    {
        Ok(()) => (
            format!(
                "Written and indexed: {file} ({line_count} lines, {byte_count} bytes).\n\
                 The file is now searchable via code_search and find_symbol."
            ),
            false,
        ),
        Err(e) => (
            format!(
                "File written to disk but re-index failed: {e}\n\
                 Run `codixing sync .` to recover."
            ),
            true,
        ),
    }
}

pub(crate) fn call_edit_file(engine: &mut Engine, args: &Value) -> (String, bool) {
    let file = match args.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => return ("Missing required argument: file".to_string(), true),
    };
    let old_string = match args.get("old_string").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return ("Missing required argument: old_string".to_string(), true),
    };
    let new_string = match args.get("new_string").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return ("Missing required argument: new_string".to_string(), true),
    };

    let abs_path = match resolve_safe_path(engine, file) {
        Ok(p) => p,
        Err(e) => return (e, true),
    };

    let original = match std::fs::read_to_string(&abs_path) {
        Ok(s) => s,
        Err(e) => return (format!("Failed to read '{file}': {e}"), true),
    };

    let count = original.matches(old_string).count();
    match count {
        0 => {
            return (
                format!(
                    "old_string not found in '{file}'.\n\
                     Use read_file or grep_code to confirm the exact text first."
                ),
                true,
            );
        }
        n if n > 1 => {
            return (
                format!(
                    "old_string appears {n} times in '{file}' \u{2014} edit is ambiguous.\n\
                     Provide more surrounding context in old_string to make it unique."
                ),
                true,
            );
        }
        _ => {}
    }

    let updated = original.replacen(old_string, new_string, 1);

    if let Err(e) = std::fs::write(&abs_path, &updated) {
        return (format!("Failed to write '{file}': {e}"), true);
    }

    let old_lines: Vec<&str> = old_string.lines().collect();
    let new_lines: Vec<&str> = new_string.lines().collect();

    match engine
        .reindex_file(&abs_path)
        .and_then(|()| engine.persist_incremental())
    {
        Ok(()) => (
            format!(
                "Edited and re-indexed: {file}\n\
                 Replaced {} line(s) with {} line(s). \
                 The change is now searchable via code_search and find_symbol.",
                old_lines.len().max(1),
                new_lines.len().max(1),
            ),
            false,
        ),
        Err(e) => (
            format!(
                "File edited on disk but re-index failed: {e}\n\
                 Run `codixing sync .` to recover."
            ),
            true,
        ),
    }
}

pub(crate) fn call_delete_file(engine: &mut Engine, args: &Value) -> (String, bool) {
    let file = match args.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => return ("Missing required argument: file".to_string(), true),
    };

    let abs_path = match resolve_safe_path(engine, file) {
        Ok(p) => p,
        Err(e) => return (e, true),
    };

    if !abs_path.exists() {
        return (
            format!("File '{file}' does not exist \u{2014} nothing to delete."),
            true,
        );
    }

    if let Err(e) = std::fs::remove_file(&abs_path) {
        return (format!("Failed to delete '{file}': {e}"), true);
    }

    match engine
        .remove_file(&abs_path)
        .and_then(|()| engine.persist_incremental())
    {
        Ok(()) => (
            format!(
                "Deleted and de-indexed: {file}.\n\
                 The file has been removed from the filesystem and the Codixing index."
            ),
            false,
        ),
        Err(e) => (
            format!(
                "File deleted from disk but de-index failed: {e}\n\
                 Run `codixing sync .` to recover."
            ),
            true,
        ),
    }
}

pub(crate) fn call_apply_patch(engine: &mut Engine, args: &Value) -> (String, bool) {
    let patch = match args.get("patch").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ("Missing required argument: patch".to_string(), true),
    };

    let root = engine.config().root.clone();
    let mut affected: Vec<PathBuf> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    let mut current_file: Option<PathBuf> = None;
    let mut current_content: Option<String> = None;
    let mut current_lines: Vec<String> = Vec::new();
    let mut in_hunk = false;

    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            if let (Some(path), Some(_)) = (current_file.take(), current_content.take()) {
                let full = root.join(&path);
                let content = current_lines.join("\n");
                if let Err(e) = std::fs::write(&full, &content) {
                    errors.push(format!("Failed to write {}: {e}", path.display()));
                } else {
                    affected.push(full);
                }
                current_lines.clear();
            }
            let rel = PathBuf::from(rest.trim());
            let full = root.join(&rel);
            current_content = std::fs::read_to_string(&full).ok();
            if let Some(ref src) = current_content {
                current_lines = src.lines().map(|l| l.to_string()).collect();
            }
            current_file = Some(rel);
            in_hunk = false;
        } else if line.starts_with("@@ ") {
            in_hunk = true;
        } else if in_hunk {
            if let Some(rest) = line.strip_prefix('+') {
                let _ = rest;
            } else if let Some(_rest) = line.strip_prefix('-') {
                // Removal
            }
        }
    }
    if let (Some(path), Some(_)) = (current_file, current_content) {
        let full = root.join(&path);
        affected.push(full);
    }

    let mut reindexed = 0usize;
    for path in &affected {
        if path.exists() {
            match engine.reindex_file(path) {
                Ok(()) => reindexed += 1,
                Err(e) => errors.push(format!("Reindex failed for {}: {e}", path.display())),
            }
        }
    }

    if !errors.is_empty() {
        return (
            format!(
                "Patch applied with {} error(s):\n{}",
                errors.len(),
                errors.join("\n")
            ),
            true,
        );
    }

    if reindexed == 0 {
        return (
            "No files were affected by the patch or files don't exist on disk yet. \
             Apply the patch to the filesystem first, then call apply_patch to reindex."
                .to_string(),
            false,
        );
    }

    let _ = engine.persist_incremental();
    (
        format!(
            "Patch processed: {reindexed} file(s) reindexed.\n\
             Affected files:\n{}",
            affected
                .iter()
                .map(|p| format!("  - {}", p.display()))
                .collect::<Vec<_>>()
                .join("\n")
        ),
        false,
    )
}

pub(crate) fn call_run_tests(engine: &mut Engine, args: &Value) -> (String, bool) {
    let command = match args.get("command").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return ("Missing required argument: command".to_string(), true),
    };
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(120);

    let root = engine.config().root.clone();

    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(&root)
        .output();

    match output {
        Err(e) => (format!("Failed to execute command: {e}"), true),
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let status = out.status.code().unwrap_or(-1);
            let success = out.status.success();

            let combined = format!("{stdout}{stderr}");
            let truncated = if combined.len() > 8000 {
                format!(
                    "[output truncated to last 8000 chars]\n...{}",
                    &combined[combined.len() - 8000..]
                )
            } else {
                combined
            };

            let header = format!(
                "Command: {command}\nExit code: {status}\nTimeout: {timeout_secs}s\n\
                 Status: {}\n\n",
                if success {
                    "\u{2713} PASSED"
                } else {
                    "\u{2717} FAILED"
                }
            );
            (format!("{header}{truncated}"), !success)
        }
    }
}

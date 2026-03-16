//! File I/O tool handlers: read, list, grep, outline, write, edit, delete, apply_patch, run_tests.

use std::path::PathBuf;
use std::time::Instant;

use serde_json::Value;

use codixing_core::{Engine, GrepMatch, SessionEventKind, SharedEventType, SharedSessionEvent};

pub(crate) fn call_read_file(engine: &Engine, args: &Value) -> (String, bool) {
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
            engine
                .session()
                .record(SessionEventKind::FileRead(file.to_string()));
            engine.shared_session().record(SharedSessionEvent {
                timestamp: Instant::now(),
                event_type: SharedEventType::FileRead,
                file_path: file.to_string(),
                symbol: None,
                agent_id: engine.session().session_id().to_string(),
            });
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

pub(crate) fn call_grep_code(engine: &Engine, args: &Value) -> (String, bool) {
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

pub(crate) fn call_list_files(engine: &Engine, args: &Value) -> (String, bool) {
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
            match glob::Pattern::new(pat) {
                Ok(g) => all_files.into_iter().filter(|f| g.matches(f)).collect(),
                Err(_) => {
                    // Invalid glob — fall back to substring match but warn the user.
                    let mut note = format!(
                        "**Note:** `{pat}` is not a valid glob pattern; using substring match instead.\n\n"
                    );
                    let matched: Vec<String> =
                        all_files.into_iter().filter(|f| f.contains(pat)).collect();
                    if matched.is_empty() {
                        note.push_str("No indexed files found matching the filter.");
                        return (note, false);
                    }
                    let mut out = format!(
                        "{note}Indexed files ({} total, {} shown):\n\n",
                        stats.file_count,
                        matched.len().min(limit)
                    );
                    for f in matched.iter().take(limit) {
                        out.push_str(&format!("  {f}\n"));
                    }
                    return (out, false);
                }
            }
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

pub(crate) fn call_outline_file(engine: &Engine, args: &Value) -> (String, bool) {
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
        Ok(()) => {
            engine
                .session()
                .record(SessionEventKind::FileWrite(file.to_string()));
            engine.shared_session().record(SharedSessionEvent {
                timestamp: Instant::now(),
                event_type: SharedEventType::FileWrite,
                file_path: file.to_string(),
                symbol: None,
                agent_id: engine.session().session_id().to_string(),
            });
            (
                format!(
                    "Written and indexed: {file} ({line_count} lines, {byte_count} bytes).\n\
                     The file is now searchable via code_search and find_symbol."
                ),
                false,
            )
        }
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
        Ok(()) => {
            engine
                .session()
                .record(SessionEventKind::FileEdit(file.to_string()));
            engine.shared_session().record(SharedSessionEvent {
                timestamp: Instant::now(),
                event_type: SharedEventType::FileWrite,
                file_path: file.to_string(),
                symbol: None,
                agent_id: engine.session().session_id().to_string(),
            });
            (
                format!(
                    "Edited and re-indexed: {file}\n\
                     Replaced {} line(s) with {} line(s). \
                     The change is now searchable via code_search and find_symbol.",
                    old_lines.len().max(1),
                    new_lines.len().max(1),
                ),
                false,
            )
        }
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

pub(crate) fn call_git_diff(engine: &Engine, args: &Value) -> (String, bool) {
    let root = engine.config().root.clone();

    let commit = args.get("commit").and_then(|v| v.as_str());
    let staged = args
        .get("staged")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let file = args.get("file").and_then(|v| v.as_str());
    let stat_only = args
        .get("stat_only")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(&root);

    if staged {
        cmd.args(["diff", "--cached"]);
    } else if let Some(r) = commit {
        cmd.args(["diff", r]);
    } else {
        cmd.arg("diff");
    }

    if stat_only {
        cmd.arg("--stat");
    }

    if let Some(f) = file {
        cmd.arg("--").arg(f);
    }

    match cmd.output() {
        Err(e) => (format!("Failed to run git: {e}"), true),
        Ok(out) if !out.status.success() => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            (format!("git diff failed: {stderr}"), true)
        }
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if stdout.trim().is_empty() {
                ("No changes detected.".to_string(), false)
            } else {
                let max = 12000;
                if stdout.len() > max {
                    (
                        format!(
                            "{}\n\n... (truncated, {} bytes total)",
                            &stdout[..max],
                            stdout.len()
                        ),
                        false,
                    )
                } else {
                    (stdout.to_string(), false)
                }
            }
        }
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

    // Parse the patch into per-file hunk groups.
    let file_patches = match parse_unified_diff(patch) {
        Ok(fp) => fp,
        Err(e) => return (format!("Patch parse error: {e}"), true),
    };

    if file_patches.is_empty() {
        return (
            "No files were affected by the patch. Ensure it is a valid unified diff.".to_string(),
            false,
        );
    }

    for fp in &file_patches {
        let abs_path = root.join(&fp.path);

        // Read the original file content.
        let original = match std::fs::read_to_string(&abs_path) {
            Ok(s) => s,
            Err(e) => {
                errors.push(format!("Cannot read '{}': {e}", fp.path));
                continue;
            }
        };

        let original_lines: Vec<&str> = original.lines().collect();

        match apply_hunks(&original_lines, &fp.hunks) {
            Ok(new_content) => {
                // Preserve the trailing newline if the original had one.
                let mut output = new_content.join("\n");
                if original.ends_with('\n') {
                    output.push('\n');
                }
                if let Err(e) = std::fs::write(&abs_path, &output) {
                    errors.push(format!("Failed to write '{}': {e}", fp.path));
                } else {
                    affected.push(abs_path);
                }
            }
            Err(e) => {
                errors.push(format!("Failed to apply hunks to '{}': {e}", fp.path));
            }
        }
    }

    // Reindex affected files.
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
            "No files were affected by the patch or files don't exist on disk yet.".to_string(),
            false,
        );
    }

    let _ = engine.persist_incremental();
    (
        format!(
            "Patch applied: {reindexed} file(s) modified and reindexed.\n\
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

// ---------------------------------------------------------------------------
// Unified diff parser + applier
// ---------------------------------------------------------------------------

/// A parsed hunk from a unified diff.
struct DiffHunk {
    /// 1-based start line in the original file.
    old_start: usize,
    /// Lines in this hunk: '+' = add, '-' = remove, ' ' = context.
    lines: Vec<DiffLine>,
}

enum DiffLine {
    Context(String),
    Add(String),
    Remove,
}

/// A set of hunks for a single file.
struct FilePatch {
    path: String,
    hunks: Vec<DiffHunk>,
}

/// Parse a unified diff into per-file patches with hunks.
fn parse_unified_diff(patch: &str) -> Result<Vec<FilePatch>, String> {
    let mut file_patches: Vec<FilePatch> = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_hunks: Vec<DiffHunk> = Vec::new();
    let mut current_hunk: Option<DiffHunk> = None;

    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            // Flush previous hunk.
            if let Some(hunk) = current_hunk.take() {
                current_hunks.push(hunk);
            }
            // Flush previous file.
            if let Some(path) = current_path.take() {
                if !current_hunks.is_empty() {
                    file_patches.push(FilePatch {
                        path,
                        hunks: std::mem::take(&mut current_hunks),
                    });
                }
            }
            current_path = Some(rest.trim().to_string());
        } else if line.starts_with("--- ") {
            // Skip the old file header.
            continue;
        } else if line.starts_with("@@ ") {
            // Flush previous hunk.
            if let Some(hunk) = current_hunk.take() {
                current_hunks.push(hunk);
            }
            // Parse @@ -old_start[,old_count] +new_start[,new_count] @@
            let old_start = parse_hunk_header_old_start(line)?;
            current_hunk = Some(DiffHunk {
                old_start,
                lines: Vec::new(),
            });
        } else if let Some(ref mut hunk) = current_hunk {
            if let Some(rest) = line.strip_prefix('+') {
                hunk.lines.push(DiffLine::Add(rest.to_string()));
            } else if let Some(rest) = line.strip_prefix('-') {
                let _ = rest;
                hunk.lines.push(DiffLine::Remove);
            } else if let Some(rest) = line.strip_prefix(' ') {
                hunk.lines.push(DiffLine::Context(rest.to_string()));
            } else if line == "\\ No newline at end of file" {
                // Git marker, skip.
            } else {
                // Treat unrecognized lines in hunk as context (handles missing
                // leading space which some tools produce).
                hunk.lines.push(DiffLine::Context(line.to_string()));
            }
        }
        // Lines outside any file / hunk are ignored (e.g. diff --git header).
    }

    // Flush remaining hunk / file.
    if let Some(hunk) = current_hunk.take() {
        current_hunks.push(hunk);
    }
    if let Some(path) = current_path.take() {
        if !current_hunks.is_empty() {
            file_patches.push(FilePatch {
                path,
                hunks: current_hunks,
            });
        }
    }

    Ok(file_patches)
}

/// Parse the old-file start line from a `@@ -start[,count] +start[,count] @@` header.
fn parse_hunk_header_old_start(header: &str) -> Result<usize, String> {
    // Example: "@@ -10,5 +10,7 @@ fn foo()"
    let parts: Vec<&str> = header.split_whitespace().collect();
    if parts.len() < 3 {
        return Err(format!("Malformed hunk header: {header}"));
    }
    let old_range = parts[1].trim_start_matches('-');
    let start_str = old_range.split(',').next().unwrap_or(old_range);
    start_str
        .parse::<usize>()
        .map_err(|_| format!("Cannot parse old start from hunk header: {header}"))
}

/// Apply a sequence of hunks to the original file lines.
///
/// Hunks must be sorted by `old_start` (ascending), which is the natural order
/// in a unified diff.
fn apply_hunks(original: &[&str], hunks: &[DiffHunk]) -> Result<Vec<String>, String> {
    let mut output: Vec<String> = Vec::with_capacity(original.len());
    // 0-based cursor into the original lines.
    let mut cursor: usize = 0;

    for hunk in hunks {
        // old_start is 1-based; convert to 0-based.
        let hunk_start = if hunk.old_start == 0 {
            0
        } else {
            hunk.old_start - 1
        };

        // Copy original lines before this hunk.
        if hunk_start > original.len() {
            return Err(format!(
                "Hunk starts at line {} but file only has {} lines",
                hunk.old_start,
                original.len()
            ));
        }
        while cursor < hunk_start {
            output.push(original[cursor].to_string());
            cursor += 1;
        }

        // Apply hunk lines.
        for diff_line in &hunk.lines {
            match diff_line {
                DiffLine::Context(_ctx) => {
                    // Context line: take the original line and advance cursor.
                    if cursor < original.len() {
                        output.push(original[cursor].to_string());
                        cursor += 1;
                    }
                }
                DiffLine::Remove => {
                    // Skip the original line (it's being removed).
                    if cursor < original.len() {
                        cursor += 1;
                    }
                }
                DiffLine::Add(text) => {
                    // Insert new line into output, don't advance cursor.
                    output.push(text.clone());
                }
            }
        }
    }

    // Copy remaining original lines after the last hunk.
    while cursor < original.len() {
        output.push(original[cursor].to_string());
        cursor += 1;
    }

    Ok(output)
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

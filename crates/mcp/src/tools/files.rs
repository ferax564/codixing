//! File I/O tool handlers: read, list, grep, outline, write, edit, delete, apply_patch, run_tests.

use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::Value;

use codixing_core::{
    Engine, GrepMatch, GrepOptions, SessionEventKind, SharedEventType, SharedSessionEvent,
};

use super::{requested_context_lines, requested_result_count, requested_tool_token_budget};

pub(crate) fn call_read_file(engine: &Engine, args: &Value) -> (String, bool) {
    let file = match args.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => return ("Missing required argument: file".to_string(), true),
    };

    let line_start = args.get("line_start").and_then(|v| v.as_u64());
    let line_end = args.get("line_end").and_then(|v| v.as_u64());
    let token_budget = requested_tool_token_budget(args);

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
            let max_chars = token_budget.saturating_mul(4);
            let tee_hint = if content.len() > max_chars {
                engine.tee_if_truncated(&content, "read_file")
            } else {
                String::new()
            };
            let (body, truncated) = if content.len() > max_chars {
                (truncate_chars(&content, max_chars), true)
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
                out.push_str(&tee_hint);
            }
            (out, false)
        }
        Err(e) => (format!("Read error: {e}"), true),
    }
}

pub(crate) fn call_grep_code(engine: &Engine, args: &Value) -> (String, bool) {
    let pattern = match args.get("pattern").and_then(|v| v.as_str()) {
        Some(p) => p.to_string(),
        None => return ("Missing required argument: pattern".to_string(), true),
    };

    let literal = args
        .get("literal")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let case_insensitive = args
        .get("case_insensitive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let invert = args
        .get("invert")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let file_glob = args
        .get("file_glob")
        .and_then(|v| v.as_str())
        .map(String::from);

    // Accept both legacy `context_lines` (symmetric) and new asymmetric
    // `before_context` / `after_context` params. Explicit before/after wins.
    let legacy_context = requested_context_lines(args, "context_lines", 0);
    let before_context = requested_context_lines(args, "before_context", legacy_context);
    let after_context = requested_context_lines(args, "after_context", legacy_context);

    let limit = requested_result_count(args, "limit", 50);
    let count_only = args
        .get("count_only")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let files_only = args
        .get("files_with_matches")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let opts = GrepOptions {
        pattern: pattern.clone(),
        literal,
        case_insensitive,
        invert,
        file_glob,
        before_context,
        after_context,
        limit,
        count_mode: count_only || files_only,
    };

    match engine.grep_code_opts(&opts) {
        Err(e) => (format!("grep_code error: {e}"), true),
        Ok(matches) if matches.is_empty() => (format!("No matches found for `{pattern}`."), false),
        Ok(matches) if count_only => {
            let files: std::collections::BTreeSet<&str> =
                matches.iter().map(|m| m.file_path.as_str()).collect();
            (
                format!(
                    "{} match(es) for `{pattern}` across {} file(s).",
                    matches.len(),
                    files.len()
                ),
                false,
            )
        }
        Ok(matches) if files_only => {
            let files: std::collections::BTreeSet<&str> =
                matches.iter().map(|m| m.file_path.as_str()).collect();
            let mut out = format!("{} file(s) containing `{pattern}`:\n", files.len());
            for f in files {
                out.push_str(f);
                out.push('\n');
            }
            (out, false)
        }
        Ok(matches) => (format_grep_matches(&pattern, &matches), false),
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
    let limit = requested_result_count(args, "limit", 100);

    let stats = engine.stats();
    let all_files = engine.indexed_files();

    let mut filtered: Vec<(String, usize)> = match pattern {
        Some(pat) => {
            match glob::Pattern::new(pat) {
                Ok(g) => all_files
                    .into_iter()
                    .filter(|(file, _)| g.matches(file))
                    .collect(),
                Err(_) => {
                    // Invalid glob — fall back to substring match but warn the user.
                    let mut note = format!(
                        "**Note:** `{pat}` is not a valid glob pattern; using substring match instead.\n\n"
                    );
                    let matched: Vec<(String, usize)> = all_files
                        .into_iter()
                        .filter(|(file, _)| file.contains(pat))
                        .collect();
                    if matched.is_empty() {
                        note.push_str("No indexed files found matching the filter.");
                        return (note, false);
                    }
                    let mut out = format!(
                        "{note}Indexed files ({} total, {} shown):\n\n",
                        stats.file_count,
                        matched.len().min(limit)
                    );
                    for (file, chunks) in matched.iter().take(limit) {
                        out.push_str(&format!("  {file} ({chunks} chunks)\n"));
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
    for (file, chunks) in &filtered {
        out.push_str(&format!("  {file} ({chunks} chunks)\n"));
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
    engine.config().resolve_path_for_write(rel).ok_or_else(|| {
        format!(
            "Path '{rel}' does not resolve safely inside the configured project roots \u{2014} operation denied."
        )
    })
}

/// Truncate `s` to at most `max_bytes`, snapping the cut **down** to the nearest
/// UTF-8 char boundary. Unlike `&s[..max_bytes]`, this never panics when the cut
/// lands inside a multibyte sequence (accented identifiers, CJK, emoji, the
/// 3-byte U+FFFD that `from_utf8_lossy` injects).
fn truncate_chars(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut i = max_bytes;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    &s[..i]
}

/// Keep the **last** `max_bytes` of `s`, snapping the cut **up** to the nearest
/// UTF-8 char boundary so the tail is always valid UTF-8.
fn truncate_chars_end(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut i = s.len() - max_bytes;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    &s[i..]
}

pub(crate) fn call_write_file(engine: &mut Engine, args: &Value) -> (String, bool) {
    if engine.is_read_only() {
        return (
            "Cannot write file: index is open in read-only mode.".to_string(),
            true,
        );
    }

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
    if engine.is_read_only() {
        return (
            "Cannot edit file: index is open in read-only mode.".to_string(),
            true,
        );
    }

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
    if engine.is_read_only() {
        return (
            "Cannot delete file: index is open in read-only mode.".to_string(),
            true,
        );
    }

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

fn is_safe_git_revision_arg(revision: &str) -> bool {
    !revision.trim().is_empty() && !revision.starts_with('-')
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
    cmd.arg("diff");

    if staged {
        cmd.arg("--cached");
    }
    if stat_only {
        // Git requires options to precede `--end-of-options`.
        cmd.arg("--stat");
    }

    if !staged && let Some(r) = commit {
        // `commit` is untrusted MCP input. Without the end-of-options marker,
        // values such as `--output=/tmp/file` turn this nominally read-only
        // reviewer tool into an arbitrary file write/truncation primitive.
        if !is_safe_git_revision_arg(r) {
            return (
                "Invalid commit/ref: expected a revision such as HEAD~1, main, or abc123"
                    .to_string(),
                true,
            );
        }
        let revision = format!("{r}^{{commit}}");
        let valid_revision = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
            .arg(&revision)
            .current_dir(&root)
            .output()
            .is_ok_and(|output| output.status.success());
        if !valid_revision {
            return (format!("Invalid commit/ref: '{r}'"), true);
        }
        cmd.arg("--end-of-options").arg(r);
    }

    if let Some(f) = file {
        cmd.arg("--").arg(f);
    }

    match run_bounded_command(&mut cmd, MAX_GIT_CAPTURE_BYTES) {
        Err(e) => (format!("Failed to run git: {e}"), true),
        Ok(out) if !out.status.success() => {
            let stderr = String::from_utf8_lossy(&out.stderr.bytes);
            let capture_note = if out.stderr.truncated {
                format!("\n... (stderr exceeded the {MAX_GIT_CAPTURE_BYTES}-byte capture limit)")
            } else {
                String::new()
            };
            (format!("git diff failed: {stderr}{capture_note}"), true)
        }
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout.bytes);
            if stdout.trim().is_empty() {
                ("No changes detected.".to_string(), false)
            } else {
                let max = 12000;
                if stdout.len() > max {
                    let (truncation_note, tee_hint) = if out.stdout.truncated {
                        (
                            format!(
                                "process output exceeded the {MAX_GIT_CAPTURE_BYTES}-byte capture limit"
                            ),
                            String::new(),
                        )
                    } else {
                        (
                            format!("{} bytes total", stdout.len()),
                            engine.tee_if_truncated(&stdout, "git_diff"),
                        )
                    };
                    (
                        format!(
                            "{}\n\n... (truncated; {truncation_note}){tee_hint}",
                            truncate_chars(&stdout, max),
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
    if engine.is_read_only() {
        return (
            "Cannot apply patch: index is open in read-only mode.".to_string(),
            true,
        );
    }

    let patch = match args.get("patch").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ("Missing required argument: patch".to_string(), true),
    };

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
        // Route the diff-supplied path through the same escape guard every other
        // write tool uses. The `+++ b/...` header is attacker-controllable, and
        // `root.join("../..")` does NOT collapse `..`, so a bare join would let a
        // patch read+rewrite files outside the repo root.
        if fp.path.trim().is_empty() {
            errors.push("Patch contains an empty target path \u{2014} skipped.".to_string());
            continue;
        }
        let abs_path = match resolve_safe_path(engine, &fp.path) {
            Ok(p) => p,
            Err(e) => {
                errors.push(e);
                continue;
            }
        };

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

    if let Err(e) = engine.persist_incremental() {
        return (
            format!(
                "Patch applied and re-indexed in memory, but persisting the index failed: {e}\n\
                 Run `codixing sync .` to recover."
            ),
            true,
        );
    }
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

pub(crate) fn call_sync_index(engine: &mut Engine) -> (String, bool) {
    match engine.sync() {
        Ok(stats) => {
            let out = format!(
                "Sync complete: {} added, {} modified, {} removed, {} unchanged",
                stats.added, stats.modified, stats.removed, stats.unchanged
            );
            (out, false)
        }
        Err(e) => (format!("Sync failed: {e}"), true),
    }
}

pub(crate) fn call_git_sync_index(engine: &mut Engine) -> (String, bool) {
    match engine.git_sync() {
        Ok(stats) => {
            if stats.unchanged {
                (
                    "Index is up to date (no changes since last indexed commit)".to_string(),
                    false,
                )
            } else {
                let out = format!(
                    "Git sync complete: {} modified, {} removed",
                    stats.modified, stats.removed
                );
                (out, false)
            }
        }
        Err(e) => (format!("Git sync failed: {e}"), true),
    }
}

pub(crate) fn call_import_external(engine: &mut Engine, args: &Value) -> (String, bool) {
    let source = match args.get("source").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return ("Missing required argument: source".to_string(), true),
    };
    let path_arg = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p,
        _ => return ("Missing required argument: path".to_string(), true),
    };

    // Resolve relative paths against the indexed project root.
    let path = {
        let p = Path::new(path_arg);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            engine.config().root.join(p)
        }
    };

    let docs = match codixing_core::parse_source(&source, &path) {
        Ok(d) => d,
        Err(e) => return (format!("Import parse failed: {e}"), true),
    };
    if docs.is_empty() {
        return (format!("No documents found in {}", path.display()), false);
    }

    match engine.import_external(docs) {
        Ok(stats) => {
            let mut out = format!(
                "Imported {} {} document(s): {} chunk(s), {} code link(s)",
                stats.documents, source, stats.chunks, stats.doc_edges
            );
            if stats.replaced > 0 {
                out.push_str(&format!(", replaced {} prior", stats.replaced));
            }
            out.push_str(&format!(
                "\nSearch them with code_search using source=\"{source}\" (or source=\"external\")."
            ));
            (out, false)
        }
        Err(e) => (format!("Import failed: {e}"), true),
    }
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

    // Portable shell selection: `cmd /C` on Windows, `sh -c` elsewhere.
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };

    match run_with_timeout(shell, flag, command, &root, timeout_secs) {
        Err(e) => (format!("Failed to execute command: {e}"), true),
        Ok(RunOutcome::TimedOut) => (
            format!(
                "Command: {command}\nTimeout: {timeout_secs}s\nStatus: \u{2717} TIMED OUT\n\n\
                 The command exceeded its {timeout_secs}s timeout and was killed."
            ),
            true,
        ),
        Ok(RunOutcome::Completed {
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
            status,
            success,
        }) => {
            let stdout = String::from_utf8_lossy(&stdout);
            let stderr = String::from_utf8_lossy(&stderr);

            let combined = format!("{stdout}{stderr}");
            let tee_hint = if combined.len() > 8000 {
                engine.tee_if_truncated(&combined, "run_tests")
            } else {
                String::new()
            };
            let truncated = if combined.len() > 8000 {
                format!(
                    "[output truncated to last 8000 chars]\n...{}{}",
                    truncate_chars_end(&combined, 8000),
                    tee_hint
                )
            } else {
                combined
            };

            let capture_note = if stdout_truncated || stderr_truncated {
                format!(
                    "Process output exceeded the {}-byte per-stream capture limit; only the most recent output was retained.\n",
                    MAX_PROCESS_CAPTURE_BYTES
                )
            } else {
                String::new()
            };
            let header = format!(
                "Command: {command}\nExit code: {status}\nTimeout: {timeout_secs}s\n\
                 Status: {}\n{capture_note}\n",
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

/// Result of running a child command under a wall-clock timeout.
enum RunOutcome {
    Completed {
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        stdout_truncated: bool,
        stderr_truncated: bool,
        status: i32,
        success: bool,
    },
    TimedOut,
}

/// Retain only the tail of each child stream while continuing to drain it.
/// Test commands can otherwise allocate unbounded memory before the response's
/// much smaller presentation cap is applied.
const MAX_PROCESS_CAPTURE_BYTES: usize = 256 * 1024;
const MAX_GIT_CAPTURE_BYTES: usize = 1024 * 1024;

#[derive(Default)]
struct BoundedCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

fn read_bounded_tail(mut reader: impl std::io::Read, max_bytes: usize) -> BoundedCapture {
    use std::collections::VecDeque;

    let mut tail = VecDeque::with_capacity(max_bytes.min(8 * 1024));
    let mut buffer = [0u8; 8 * 1024];
    let mut truncated = false;
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        tail.extend(&buffer[..read]);
        if tail.len() > max_bytes {
            let excess = tail.len() - max_bytes;
            tail.drain(..excess);
            truncated = true;
        }
    }
    BoundedCapture {
        bytes: tail.into_iter().collect(),
        truncated,
    }
}

fn read_bounded_head(mut reader: impl std::io::Read, max_bytes: usize) -> BoundedCapture {
    let mut bytes = Vec::with_capacity(max_bytes.min(8 * 1024));
    let mut buffer = [0u8; 8 * 1024];
    let mut truncated = false;
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let remaining = max_bytes.saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..read.min(remaining)]);
        truncated |= read > remaining;
    }
    BoundedCapture { bytes, truncated }
}

struct BoundedCommandOutput {
    stdout: BoundedCapture,
    stderr: BoundedCapture,
    status: std::process::ExitStatus,
}

fn run_bounded_command(
    command: &mut std::process::Command,
    max_bytes_per_stream: usize,
) -> std::io::Result<BoundedCommandOutput> {
    use std::process::Stdio;

    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_handle = std::thread::spawn(move || {
        stdout.map_or_else(BoundedCapture::default, |pipe| {
            read_bounded_head(pipe, max_bytes_per_stream)
        })
    });
    let stderr_handle = std::thread::spawn(move || {
        stderr.map_or_else(BoundedCapture::default, |pipe| {
            read_bounded_head(pipe, max_bytes_per_stream)
        })
    });
    let status = child.wait()?;
    Ok(BoundedCommandOutput {
        stdout: stdout_handle.join().unwrap_or_default(),
        stderr: stderr_handle.join().unwrap_or_default(),
        status,
    })
}

/// Spawn `shell flag command` in `root` and enforce `timeout_secs`. Stdout and
/// stderr are drained by dedicated reader threads so a chatty command can't
/// deadlock on a full pipe buffer while we poll for exit. On expiry the child is
/// killed and reaped so it can't wedge the worker thread (which holds
/// `&mut Engine`). `timeout_secs == 0` means no limit.
fn run_with_timeout(
    shell: &str,
    flag: &str,
    command: &str,
    root: &Path,
    timeout_secs: u64,
) -> std::io::Result<RunOutcome> {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let mut child = std::process::Command::new(shell)
        .arg(flag)
        .arg(command)
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Drain both pipes concurrently: without this a command that writes more
    // than the OS pipe buffer (~64KB) before exiting would block on write
    // forever, and our `try_wait` loop would never see it finish.
    let drain = |pipe: Option<std::process::ChildStdout>| {
        std::thread::spawn(move || {
            if let Some(pipe) = pipe {
                read_bounded_tail(pipe, MAX_PROCESS_CAPTURE_BYTES)
            } else {
                BoundedCapture {
                    bytes: Vec::new(),
                    truncated: false,
                }
            }
        })
    };
    let drain_err = |pipe: Option<std::process::ChildStderr>| {
        std::thread::spawn(move || {
            if let Some(pipe) = pipe {
                read_bounded_tail(pipe, MAX_PROCESS_CAPTURE_BYTES)
            } else {
                BoundedCapture {
                    bytes: Vec::new(),
                    truncated: false,
                }
            }
        })
    };
    let out_handle = drain(child.stdout.take());
    let err_handle = drain_err(child.stderr.take());

    let collect = |child: &mut std::process::Child,
                   status: std::process::ExitStatus,
                   out_handle: std::thread::JoinHandle<BoundedCapture>,
                   err_handle: std::thread::JoinHandle<BoundedCapture>| {
        let _ = child;
        let stdout = out_handle.join().unwrap_or_default();
        let stderr = err_handle.join().unwrap_or_default();
        RunOutcome::Completed {
            stdout: stdout.bytes,
            stderr: stderr.bytes,
            stdout_truncated: stdout.truncated,
            stderr_truncated: stderr.truncated,
            status: status.code().unwrap_or(-1),
            success: status.success(),
        }
    };

    if timeout_secs == 0 {
        let status = child.wait()?;
        return Ok(collect(&mut child, status, out_handle, err_handle));
    }

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        match child.try_wait()? {
            Some(status) => {
                return Ok(collect(&mut child, status, out_handle, err_handle));
            }
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // Reader threads unblock at EOF once the child is gone.
                    let _ = out_handle.join();
                    let _ = err_handle.join();
                    return Ok(RunOutcome::TimedOut);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_chars_snaps_below_multibyte_boundary() {
        // "héllo": h=1B, é=2B (bytes 1..3), so byte index 2 is mid-char.
        let s = "héllo";
        // A naive &s[..2] would panic; truncate_chars snaps down to 1.
        assert_eq!(truncate_chars(s, 2), "h");
        // A boundary that lands exactly after é (byte 3) keeps "hé".
        assert_eq!(truncate_chars(s, 3), "hé");
        // max >= len returns the whole string.
        assert_eq!(truncate_chars(s, 999), s);
    }

    #[test]
    fn truncate_chars_handles_emoji_and_cjk() {
        let s = "a😀中"; // a=1, 😀=4 (1..5), 中=3 (5..8)
        assert_eq!(truncate_chars(s, 3), "a"); // mid-emoji -> snap to 1
        assert_eq!(truncate_chars(s, 5), "a😀"); // exact boundary
        assert_eq!(truncate_chars(s, 6), "a😀"); // mid-中 -> snap back to 5
    }

    #[test]
    fn truncate_chars_end_snaps_up_to_boundary() {
        let s = "héllo"; // bytes: h(0) é(1..3) l(3) l(4) o(5), len 6
        // Keep last 6 bytes => whole string (len == max).
        assert_eq!(truncate_chars_end(s, 6), s);
        // Keep last 5 bytes: cut at byte 1 (start of é) — valid boundary.
        assert_eq!(truncate_chars_end(s, 5), "éllo");
        // Keep last 4 bytes: raw cut at byte 2 is mid-é; snap up to 3 -> "llo".
        assert_eq!(truncate_chars_end(s, 4), "llo");
    }

    #[test]
    fn git_diff_revision_rejects_option_injection() {
        assert!(is_safe_git_revision_arg("HEAD~1"));
        assert!(is_safe_git_revision_arg("main"));
        assert!(!is_safe_git_revision_arg(""));
        assert!(!is_safe_git_revision_arg("   "));
        assert!(!is_safe_git_revision_arg("--output=/tmp/should-not-exist"));
        assert!(!is_safe_git_revision_arg("--ext-diff"));
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_kills_overrunning_command() {
        let dir = std::env::temp_dir();
        let outcome = run_with_timeout("sh", "-c", "sleep 30", &dir, 1).unwrap();
        assert!(
            matches!(outcome, RunOutcome::TimedOut),
            "a 30s sleep under a 1s timeout must time out"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_completes_fast_command() {
        let dir = std::env::temp_dir();
        let outcome = run_with_timeout("sh", "-c", "echo hello", &dir, 10).unwrap();
        match outcome {
            RunOutcome::Completed {
                stdout, success, ..
            } => {
                assert!(success);
                assert_eq!(String::from_utf8_lossy(&stdout).trim(), "hello");
            }
            RunOutcome::TimedOut => panic!("a fast command must not time out"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_drains_large_output() {
        // Emit >64KB (past the pipe buffer) then exit — must not deadlock.
        let dir = std::env::temp_dir();
        let outcome =
            run_with_timeout("sh", "-c", "yes abcdefgh | head -n 20000", &dir, 30).unwrap();
        match outcome {
            RunOutcome::Completed { stdout, .. } => {
                assert!(stdout.len() > 64 * 1024, "expected >64KB of drained output");
            }
            RunOutcome::TimedOut => panic!("draining large output must not time out"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_bounds_retained_output() {
        let dir = std::env::temp_dir();
        let outcome =
            run_with_timeout("sh", "-c", "yes abcdefgh | head -n 100000", &dir, 30).unwrap();
        match outcome {
            RunOutcome::Completed {
                stdout,
                stdout_truncated,
                ..
            } => {
                assert_eq!(stdout.len(), MAX_PROCESS_CAPTURE_BYTES);
                assert!(stdout_truncated);
            }
            RunOutcome::TimedOut => panic!("bounded draining must not time out"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn bounded_command_drains_but_does_not_retain_unbounded_output() {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "yes abcdefgh | head -n 100000"]);
        let output = run_bounded_command(&mut command, 32 * 1024).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.bytes.len(), 32 * 1024);
        assert!(output.stdout.truncated);
    }
}

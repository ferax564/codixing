//! MCP tool definitions and engine dispatch helpers.
//!
//! Tool schemas, `is_read_only_tool`, `is_meta_tool`, `MEDIUM_TOOLS`, and the
//! dispatch match arms are **generated at build time** from TOML files in
//! `crates/mcp/tool_defs/`.  See `build.rs` for the codegen logic.
//!
//! To add a new tool:
//! 1. Add a `[[tools]]` entry to the appropriate TOML file in `tool_defs/`.
//! 2. Implement the handler function in the corresponding submodule.
//! 3. Run `cargo build` — the rest is automatic.

mod analysis;
mod common;
mod context;
mod feature_hub;
pub mod federation;
mod files;
mod focus;
mod freshness;
mod graph;
mod memory;
mod orphans;
mod search;
mod temporal;

#[cfg(test)]
mod tests;

use serde_json::{Value, json};

use codixing_core::{Engine, FederatedEngine};

pub use common::ProgressReporter;

// ---------------------------------------------------------------------------
// Generated code: tool schemas, classification, and dispatch match arms
// ---------------------------------------------------------------------------

/// Submodule containing build-time generated code from `tool_defs/*.toml`.
///
/// Re-exported items: `tool_definitions`, `federation_tool_definitions`,
/// `list_projects_tool_definition`, `compact_tool_definitions`,
/// `medium_tool_definitions`, `MEDIUM_TOOLS`, `is_read_only_tool`,
/// `is_meta_tool`.
mod generated {
    include!(concat!(env!("OUT_DIR"), "/tool_definitions_generated.rs"));
}

// Re-export generated public API so callers see the same interface as before.
pub use generated::{
    compact_tool_definitions, federation_tool_definitions, is_meta_tool, is_read_only_tool,
    list_projects_tool_definition, medium_tool_definitions, tool_definitions,
};
// MEDIUM_TOOLS is used by tests but not the binary — suppress the unused-import warning.
#[allow(unused_imports)]
pub use generated::MEDIUM_TOOLS;

// ---------------------------------------------------------------------------
// Tool definitions with federation
// ---------------------------------------------------------------------------

/// Return tool definitions, optionally including federation-only tools.
pub fn tool_definitions_with_federation(has_federation: bool) -> Value {
    let mut defs = tool_definitions();
    if let Some(arr) = defs.as_array_mut() {
        // Federation management tools are always listed so users can manage
        // configs even without a live FederatedEngine.
        arr.extend(federation_tool_definitions());
        if has_federation {
            arr.push(list_projects_tool_definition());
        }
    }
    defs
}

// ---------------------------------------------------------------------------
// Dynamic tool discovery helpers
// ---------------------------------------------------------------------------

/// Return a compact list of `(name, description)` tuples from the full tool
/// definitions (including federation tools). Used by `search_tools` to return
/// lightweight summaries.
pub fn tool_summaries() -> Vec<(String, String)> {
    let defs = tool_definitions_with_federation(true);
    defs.as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|tool| {
            let name = tool.get("name")?.as_str()?.to_string();
            let desc = tool.get("description")?.as_str()?.to_string();
            Some((name, desc))
        })
        .collect()
}

/// Handle the `search_tools` meta-tool: substring-match `query` against tool
/// names and descriptions, returning a compact list.
pub(crate) fn call_search_tools(args: &Value) -> (String, bool) {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();

    let summaries = tool_summaries();
    let matches: Vec<&(String, String)> = if query.is_empty() {
        summaries.iter().collect()
    } else {
        summaries
            .iter()
            .filter(|(name, desc)| {
                name.to_lowercase().contains(&query) || desc.to_lowercase().contains(&query)
            })
            .collect()
    };

    if matches.is_empty() {
        return (
            format!("No tools match query '{query}'. Try a broader keyword."),
            false,
        );
    }

    let mut out = format!("## Matching tools ({} results)\n\n", matches.len());
    for (name, desc) in &matches {
        // Truncate description to first sentence for compact output.
        let short_desc = desc.split(". ").next().unwrap_or(desc);
        out.push_str(&format!("- **{name}**: {short_desc}.\n"));
    }
    out.push_str("\nUse `get_tool_schema` with the tool name(s) to get full parameter details.");

    (out, false)
}

/// Handle the `get_tool_schema` meta-tool: return full JSON schemas for the
/// requested tool name(s).
pub(crate) fn call_get_tool_schema(args: &Value) -> (String, bool) {
    let names: Vec<&str> = match args.get("names").and_then(|v| v.as_array()) {
        Some(arr) => arr.iter().filter_map(|v| v.as_str()).collect(),
        None => {
            return (
                "Missing required parameter 'names' (array of tool name strings).".to_string(),
                true,
            );
        }
    };

    if names.is_empty() {
        return (
            "Parameter 'names' must contain at least one tool name.".to_string(),
            true,
        );
    }

    let defs = tool_definitions_with_federation(true);
    let empty = vec![];
    let all_tools = defs.as_array().unwrap_or(&empty);

    let mut results: Vec<Value> = Vec::new();
    let mut not_found: Vec<&str> = Vec::new();

    for name in &names {
        let found = all_tools
            .iter()
            .find(|t| t.get("name").and_then(|v| v.as_str()) == Some(name));
        match found {
            Some(tool) => results.push(tool.clone()),
            None => not_found.push(name),
        }
    }

    if results.is_empty() {
        return (
            format!(
                "Unknown tool(s): {}. Use search_tools to discover available tools.",
                not_found.join(", ")
            ),
            true,
        );
    }

    let mut out = String::new();
    if !not_found.is_empty() {
        out.push_str(&format!(
            "Warning: unknown tool(s): {}\n\n",
            not_found.join(", ")
        ));
    }

    let output_json = json!(results);
    out.push_str(&serde_json::to_string_pretty(&output_json).unwrap_or_else(|_| "[]".to_string()));

    (out, false)
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Dispatch a read-only `tools/call` invocation.
///
/// Takes `&Engine` (shared reference) so multiple read-only calls can run
/// concurrently under a `RwLock::read()` guard.
///
/// The optional `federation` parameter provides access to the federated engine
/// for cross-repo tools like `list_projects`.
///
/// Returns `(text_output, is_error)`.
/// Convenience wrapper for `dispatch_tool_ref_with_progress` without progress.
///
/// Used by unit tests in `tools/tests.rs` which don't need progress reporting.
#[allow(dead_code)]
pub fn dispatch_tool_ref(
    engine: &Engine,
    name: &str,
    args: &Value,
    federation: Option<&FederatedEngine>,
) -> (String, bool) {
    dispatch_tool_ref_with_progress(engine, name, args, federation, None)
}

/// Dispatch a read-only `tools/call` invocation, optionally with progress
/// reporting for long-running operations.
pub fn dispatch_tool_ref_with_progress(
    engine: &Engine,
    name: &str,
    args: &Value,
    federation: Option<&FederatedEngine>,
    progress: Option<&ProgressReporter>,
) -> (String, bool) {
    let (output, is_error) =
        match generated::dispatch_read_only_match(engine, name, args, federation, progress) {
            Some(result) => result,
            None => (format!("Unknown read-only tool: {name}"), true),
        };
    let final_output = if args
        .get("compact")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        compact_output(&output)
    } else {
        engine.filter_output(&output, name).output
    };
    (final_output, is_error)
}

/// Dispatch a `tools/call` invocation to the appropriate engine method.
///
/// Takes `&mut Engine` so that write tools (write_file, edit_file, delete_file,
/// etc.) can mutate the index inline.
///
/// Returns `(text_output, is_error)`.
/// Convenience wrapper for `dispatch_tool_with_progress` without progress.
///
/// Used by unit tests in `tools/tests.rs` which don't need progress reporting.
#[allow(dead_code)]
pub fn dispatch_tool(
    engine: &mut Engine,
    name: &str,
    args: &Value,
    federation: Option<&FederatedEngine>,
) -> (String, bool) {
    dispatch_tool_with_progress(engine, name, args, federation, None)
}

/// Dispatch a `tools/call` invocation to the appropriate engine method,
/// optionally with progress reporting.
pub fn dispatch_tool_with_progress(
    engine: &mut Engine,
    name: &str,
    args: &Value,
    federation: Option<&FederatedEngine>,
    progress: Option<&ProgressReporter>,
) -> (String, bool) {
    let (output, is_error) = match generated::dispatch_write_match(engine, name, args) {
        Some(result) => result,
        // Fallback: if a read-only tool is accidentally dispatched through the
        // write path, handle it rather than returning an error.
        None => {
            match generated::dispatch_read_only_match(engine, name, args, federation, progress) {
                Some(result) => result,
                None => (format!("Unknown tool: {name}"), true),
            }
        }
    };
    let final_output = if args
        .get("compact")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        compact_output(&output)
    } else {
        engine.filter_output(&output, name).output
    };
    (final_output, is_error)
}

// ---------------------------------------------------------------------------
// Compact output post-processing
// ---------------------------------------------------------------------------

/// Compress tool output for token-constrained AI agents:
/// - Remove fenced code blocks, keep only `// <file>` headers and signatures
/// - Truncate lines longer than 120 chars
/// - Limit total output to ~2000 chars
/// - Preserve structural elements (headers, file paths, line numbers)
fn compact_output(output: &str) -> String {
    let mut result = String::with_capacity(output.len().min(2200));
    let mut in_code_block = false;
    let mut code_block_lines = 0u32;

    for line in output.lines() {
        let trimmed = line.trim();

        // Track fenced code blocks.
        if trimmed.starts_with("```") {
            if in_code_block {
                // Closing fence — emit summary if we skipped lines.
                if code_block_lines > 2 {
                    result.push_str(&format!("  ... ({code_block_lines} lines)\n"));
                }
                in_code_block = false;
                code_block_lines = 0;
            } else {
                in_code_block = true;
                code_block_lines = 0;
            }
            continue;
        }

        if in_code_block {
            code_block_lines += 1;
            // Keep only the first 2 lines of each code block (signature / key info).
            if code_block_lines <= 2 {
                let truncated = truncate_line(line, 120);
                result.push_str(truncated);
                result.push('\n');
            }
            continue;
        }

        // Outside code blocks: keep headers, file paths, bullet points.
        let truncated = truncate_line(line, 120);
        result.push_str(truncated);
        result.push('\n');

        // Hard limit on total output.
        if result.len() > 2000 {
            result.push_str("\n... (output compacted)\n");
            break;
        }
    }

    result
}

/// Return a `&str` slice of at most `max_len` characters.
fn truncate_line(line: &str, max_len: usize) -> &str {
    if line.len() <= max_len {
        line
    } else {
        // Find a safe char boundary.
        let mut end = max_len;
        while end > 0 && !line.is_char_boundary(end) {
            end -= 1;
        }
        &line[..end]
    }
}

// ---------------------------------------------------------------------------
// Session helpers (called by generated dispatch via `super::`)
// ---------------------------------------------------------------------------

pub(crate) fn call_get_session_summary(engine: &Engine, args: &Value) -> (String, bool) {
    let token_budget = args
        .get("token_budget")
        .and_then(|v| v.as_u64())
        .unwrap_or(1500) as usize;

    let summary = engine.session().summary(token_budget);
    (summary, false)
}

pub(crate) fn call_session_status(engine: &Engine, args: &Value) -> (String, bool) {
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

    let shared = engine.shared_session();
    let agents = shared.active_agents();
    let hot_files = shared.get_hot_files(limit);
    let event_count = shared.event_count();

    let mut out = String::from("## Shared Session Status\n\n");

    out.push_str(&format!("**Total events:** {event_count}\n"));
    out.push_str(&format!(
        "**Active agents:** {}\n",
        if agents.is_empty() {
            "none".to_string()
        } else {
            format!("{} ({})", agents.len(), agents.join(", "))
        }
    ));
    out.push_str(&format!(
        "**Current agent:** {}\n\n",
        engine.session().session_id()
    ));

    if hot_files.is_empty() {
        out.push_str("No recently active files.\n");
    } else {
        out.push_str("### Hot files (cross-agent activity)\n\n");
        for (i, (file, score)) in hot_files.iter().enumerate() {
            out.push_str(&format!("  {}. `{}` (score: {:.3})\n", i + 1, file, score));
        }
    }

    if !engine.embeddings_ready() {
        let (done, total) = engine.embedding_progress();
        out.push_str("\n## Embedding Progress\n\n");
        out.push_str(&format!(
            "  {done}/{total} chunks ({:.0}%)\n",
            if total > 0 {
                done as f64 / total as f64 * 100.0
            } else {
                100.0
            }
        ));
    } else if engine.embedding_progress().1 > 0 {
        out.push_str("\n## Embedding Progress\n\n  Complete\n");
    }

    (out, false)
}

pub(crate) fn call_session_reset_focus(engine: &Engine) -> (String, bool) {
    engine.session().reset_focus();
    (
        "Progressive focus cleared. Search results will no longer be narrowed to a specific directory.".to_string(),
        false,
    )
}

// ---------------------------------------------------------------------------
// Federation helpers (called by generated dispatch via `super::`)
// ---------------------------------------------------------------------------

pub(crate) fn call_list_projects(federation: Option<&FederatedEngine>) -> (String, bool) {
    let fed = match federation {
        Some(f) => f,
        None => {
            return (
                "Federation is not enabled. Start the server with --federation <config.json> to use cross-repo features.".to_string(),
                true,
            );
        }
    };

    let projects = fed.projects();
    let stats = fed.stats();

    let mut out = String::from("## Federated Projects\n\n");
    out.push_str(&format!(
        "**Registered:** {} | **Loaded:** {} | **Total files:** {} | **Total chunks:** {} | **Total symbols:** {}\n\n",
        stats.project_count, stats.loaded_count, stats.total_files, stats.total_chunks, stats.total_symbols,
    ));

    if projects.is_empty() {
        out.push_str("No projects registered.\n");
    } else {
        out.push_str("| # | Project | Root | Loaded | Files |\n");
        out.push_str("|---|---------|------|--------|-------|\n");
        for (i, proj) in projects.iter().enumerate() {
            let status = if proj.loaded { "yes" } else { "no" };
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                i + 1,
                proj.name,
                proj.root.display(),
                status,
                proj.file_count,
            ));
        }
    }

    (out, false)
}

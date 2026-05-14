//! Handlers for the `get_context_for_task` and `assemble_context` MCP tools.

use serde_json::Value;

use codixing_core::context_assembly::IntelligentContextAssembler;
use codixing_core::{AgentContextMode, Engine, SearchQuery};

use super::common::ProgressReporter;

pub(crate) fn call_get_context_for_task(
    engine: &Engine,
    args: &Value,
    progress: Option<&ProgressReporter>,
) -> (String, bool) {
    let task = match args.get("task").and_then(|v| v.as_str()) {
        Some(t) => t.to_string(),
        None => return ("Missing required argument: task".to_string(), true),
    };
    let token_budget = args
        .get("token_budget")
        .and_then(|v| v.as_u64())
        .unwrap_or(4000) as usize;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

    // 1. Search with the task description
    if let Some(p) = progress {
        p.report(0, "Searching relevant code...");
    }

    let strategy = engine.detect_strategy(&task);
    let query = SearchQuery::new(&task)
        .with_limit(limit)
        .with_strategy(strategy);
    let results = match engine.search(query) {
        Ok(r) => r,
        Err(e) => return (format!("Search error: {e}"), true),
    };

    if results.is_empty() {
        return (
            format!("No relevant code found for task: \"{task}\""),
            false,
        );
    }

    if let Some(p) = progress {
        p.report(50, "Assembling context...");
    }

    // 2. Assemble context with IntelligentContextAssembler
    let mut assembler = IntelligentContextAssembler::new(token_budget);
    if let Some(graph_stats) = engine.graph_stats() {
        // Only attach graph if it has nodes (i.e. was actually built)
        if graph_stats.node_count > 0 {
            if let Some(graph_data) = engine.graph_data() {
                let graph = codixing_core::CodeGraph::from_flat(graph_data);
                assembler = assembler.with_graph(&graph);
            }
        }
    }
    let snippets = assembler.assemble(results);

    if snippets.is_empty() {
        return (
            format!(
                "Code found but could not fit within the token budget ({token_budget} tokens)."
            ),
            false,
        );
    }

    if let Some(p) = progress {
        p.report(90, "Formatting output...");
    }

    // 3. Format output
    let mut out = format!(
        "## Context for: \"{task}\"\n\n{} snippet(s), dependency-ordered (definitions before usages)\n\n",
        snippets.len()
    );

    for (i, snippet) in snippets.iter().enumerate() {
        out.push_str(&format!(
            "### [{}/{}] `{}` L{}\u{2013}L{} (score: {:.2})\n",
            i + 1,
            snippets.len(),
            snippet.file_path,
            snippet.line_start + 1,
            snippet.line_end,
            snippet.score
        ));
        out.push_str(&format!("```{}\n", snippet.language));
        out.push_str(&snippet.content);
        if !snippet.content.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("```\n\n");
    }

    (out, false)
}

pub(crate) fn call_agent_context_pack(
    engine: &Engine,
    args: &Value,
    progress: Option<&ProgressReporter>,
) -> (String, bool) {
    let task = match args.get("task").and_then(|v| v.as_str()) {
        Some(t) => t.to_string(),
        None => return ("Missing required argument: task".to_string(), true),
    };
    let mode = match args
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("understand")
        .parse::<AgentContextMode>()
    {
        Ok(mode) => mode,
        Err(e) => return (e, true),
    };
    let token_budget = args
        .get("token_budget")
        .and_then(|v| v.as_u64())
        .unwrap_or(6000) as usize;
    let changed_files = match args.get("changed_files") {
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(|value| value.as_str().map(ToOwned::to_owned))
            .collect(),
        Some(Value::String(value)) => value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        _ => Vec::new(),
    };
    let branch = args
        .get("branch")
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned);
    let risk_level = args
        .get("risk_level")
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned);

    if let Some(p) = progress {
        p.report(0, "Compiling agent context pack...");
    }

    let pack = match engine.agent_context_pack(
        &task,
        mode,
        token_budget,
        &changed_files,
        branch,
        risk_level,
    ) {
        Ok(pack) => pack,
        Err(e) => return (format!("agent_context_pack error: {e}"), true),
    };

    if let Some(p) = progress {
        p.report(100, "Agent context pack ready.");
    }

    match serde_json::to_string_pretty(&pack) {
        Ok(json) => (json, false),
        Err(e) => (format!("JSON serialization error: {e}"), true),
    }
}

pub(crate) fn call_assemble_location_context(engine: &Engine, args: &Value) -> (String, bool) {
    use codixing_core::engine::context_assembly::BudgetMode;

    let file = match args.get("file").and_then(|v| v.as_str()) {
        Some(f) => f.to_string(),
        None => return ("Missing required argument: file".to_string(), true),
    };
    let line = args.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
    let token_budget = args
        .get("token_budget")
        .and_then(|v| v.as_u64())
        .unwrap_or(4096) as usize;
    let budget_mode = match args.get("budget_mode").and_then(|v| v.as_str()) {
        Some("strict") => BudgetMode::Strict,
        _ => BudgetMode::Soft,
    };

    let ctx =
        engine.assemble_context_for_location_with_mode(&file, line, token_budget, budget_mode);

    let over_marker = if ctx.over_budget {
        " ⚠ over budget"
    } else {
        ""
    };
    let mut out = format!(
        "## Context: {}:{}\n\nToken budget: {} ({}) | Used: {}{}\n",
        ctx.primary.file_path,
        line,
        token_budget,
        ctx.budget_mode.as_str(),
        ctx.total_tokens,
        over_marker
    );
    if let Some(reason) = &ctx.oversize_reason {
        out.push_str(&format!("Note: {reason}\n"));
    }
    out.push('\n');

    out.push_str(&format!(
        "### Primary (L{}\u{2013}L{})\n```\n{}\n```\n\n",
        ctx.primary.line_start, ctx.primary.line_end, ctx.primary.content
    ));

    if !ctx.imports.is_empty() {
        out.push_str(&format!("### Imports ({})\n", ctx.imports.len()));
        for imp in &ctx.imports {
            out.push_str(&format!(
                "- `{}` L{}\u{2013}L{} (relevance: {:.2})\n  ```\n  {}\n  ```\n",
                imp.file_path, imp.line_start, imp.line_end, imp.relevance, imp.content
            ));
        }
        out.push('\n');
    }

    if !ctx.callees.is_empty() {
        out.push_str(&format!("### Callees ({})\n", ctx.callees.len()));
        for callee in &ctx.callees {
            out.push_str(&format!(
                "- `{}` L{}\u{2013}L{} (relevance: {:.2})\n  ```\n  {}\n  ```\n",
                callee.file_path,
                callee.line_start,
                callee.line_end,
                callee.relevance,
                callee.content
            ));
        }
        out.push('\n');
    }

    if !ctx.examples.is_empty() {
        out.push_str(&format!("### Usage Examples ({})\n", ctx.examples.len()));
        for (i, ex) in ctx.examples.iter().enumerate() {
            let kind_label = match ex.kind {
                codixing_core::engine::examples::ExampleKind::Test => "TEST",
                codixing_core::engine::examples::ExampleKind::CallSite => "CALL",
                codixing_core::engine::examples::ExampleKind::DocBlock => "DOC",
            };
            out.push_str(&format!(
                "{}. [{}] `{}` L{}\u{2013}L{}\n```\n{}\n```\n\n",
                i + 1,
                kind_label,
                ex.file_path,
                ex.line_start,
                ex.line_end,
                ex.context
            ));
        }
    }

    (out, false)
}

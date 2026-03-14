//! Handler for the `get_context_for_task` MCP tool.

use serde_json::Value;

use codixing_core::context_assembly::IntelligentContextAssembler;
use codixing_core::{Engine, SearchQuery};

pub(crate) fn call_get_context_for_task(engine: &mut Engine, args: &Value) -> (String, bool) {
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

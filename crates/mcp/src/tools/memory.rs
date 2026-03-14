//! Memory tool handlers: remember, recall, forget, enrich_docs.

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::{Value, json};

use codixing_core::Engine;

/// Path to the memory store relative to the project index directory.
fn memory_path(engine: &Engine) -> PathBuf {
    engine.config().root.join(".codixing/memory.json")
}

/// Load the memory store from disk.
fn load_memory(engine: &Engine) -> HashMap<String, serde_json::Value> {
    let path = memory_path(engine);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist the memory store to disk.
fn save_memory(engine: &Engine, memory: &HashMap<String, serde_json::Value>) -> Result<(), String> {
    let path = memory_path(engine);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create .codixing dir: {e}"))?;
    }
    std::fs::write(
        &path,
        serde_json::to_string_pretty(memory).unwrap_or_default(),
    )
    .map_err(|e| format!("Failed to write memory.json: {e}"))
}

pub(crate) fn call_remember(engine: &mut Engine, args: &Value) -> (String, bool) {
    let key = match args.get("key").and_then(|v| v.as_str()) {
        Some(k) => k.to_string(),
        None => return ("Missing required argument: key".to_string(), true),
    };
    let value = match args.get("value").and_then(|v| v.as_str()) {
        Some(v) => v.to_string(),
        None => return ("Missing required argument: value".to_string(), true),
    };
    let tags: Vec<String> = args
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let mut memory = load_memory(engine);
    memory.insert(key.clone(), json!({ "value": value, "tags": tags }));

    match save_memory(engine, &memory) {
        Ok(()) => (
            format!("Stored memory '{key}'. Total entries: {}.", memory.len()),
            false,
        ),
        Err(e) => (e, true),
    }
}

pub(crate) fn call_recall(engine: &Engine, args: &Value) -> (String, bool) {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    let tags: Vec<String> = args
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.as_str().map(|s| s.to_lowercase()))
                .collect()
        })
        .unwrap_or_default();

    let memory = load_memory(engine);

    if memory.is_empty() {
        return (
            "No memories stored yet. Use `remember` to store project knowledge.".to_string(),
            false,
        );
    }

    let mut results: Vec<(String, String, Vec<String>)> = Vec::new();

    for (key, entry) in &memory {
        let value = entry
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let entry_tags: Vec<String> = entry
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.as_str().map(|s| s.to_lowercase()))
                    .collect()
            })
            .unwrap_or_default();

        // Filter by query.
        let query_match = query.is_empty()
            || key.to_lowercase().contains(&query)
            || value.to_lowercase().contains(&query);
        if !query_match {
            continue;
        }

        // Filter by tags (AND).
        let tags_match = tags.is_empty() || tags.iter().all(|t| entry_tags.contains(t));
        if !tags_match {
            continue;
        }

        results.push((key.clone(), value, entry_tags));
    }

    if results.is_empty() {
        return ("No matching memory entries.".to_string(), false);
    }

    results.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = format!("## Memory ({} matching entries)\n\n", results.len());
    for (key, value, entry_tags) in &results {
        out.push_str(&format!("**{key}**"));
        if !entry_tags.is_empty() {
            out.push_str(&format!("  [{}]", entry_tags.join(", ")));
        }
        out.push('\n');
        out.push_str(&format!("  {value}\n\n"));
    }
    (out, false)
}

pub(crate) fn call_forget(engine: &mut Engine, args: &Value) -> (String, bool) {
    let key = match args.get("key").and_then(|v| v.as_str()) {
        Some(k) => k.to_string(),
        None => return ("Missing required argument: key".to_string(), true),
    };

    let mut memory = load_memory(engine);
    if memory.remove(&key).is_none() {
        return (format!("No memory entry found with key '{key}'."), false);
    }

    match save_memory(engine, &memory) {
        Ok(()) => (
            format!(
                "Removed memory entry '{key}'. Remaining entries: {}.",
                memory.len()
            ),
            false,
        ),
        Err(e) => (e, true),
    }
}

pub(crate) fn call_enrich_docs(engine: &mut Engine, args: &Value) -> (String, bool) {
    let symbol = match args.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return ("Missing required argument: symbol".to_string(), true),
    };
    let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);

    let root = engine.config().root.clone();
    let docs_path = root.join(".codixing/symbol_docs.json");

    // Load existing docs.
    let mut docs: HashMap<String, String> = if docs_path.exists() {
        std::fs::read_to_string(&docs_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    } else {
        HashMap::new()
    };

    // Return cached if available and not forced.
    if !force {
        if let Some(cached) = docs.get(&symbol) {
            return (format!("## Doc for `{symbol}` (cached)\n\n{cached}"), false);
        }
    }

    // Read symbol source.
    let src = match engine.read_symbol_source(&symbol, None) {
        Ok(Some(s)) => s,
        Ok(None) => return (format!("Symbol `{symbol}` not found."), true),
        Err(e) => return (format!("Error reading symbol: {e}"), true),
    };

    // Generate a simple inline doc.
    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
    let ollama = std::env::var("OLLAMA_HOST").ok();

    let doc = if api_key.is_none() && ollama.is_none() {
        format!(
            "Auto-generated stub (set ANTHROPIC_API_KEY or OLLAMA_HOST for LLM-quality docs):\n\n\
             `{symbol}` \u{2014} {lines} lines. \
             Set ANTHROPIC_API_KEY and re-run to generate a full documentation comment.",
            lines = src.lines().count()
        )
    } else {
        format!(
            "Documentation for `{symbol}` ({lines} lines of source).\n\n\
             LLM enrichment is configured but not yet implemented in this build.",
            lines = src.lines().count()
        )
    };

    docs.insert(symbol.clone(), doc.clone());

    // Persist.
    if let Some(parent) = docs_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        &docs_path,
        serde_json::to_string_pretty(&docs).unwrap_or_default(),
    );

    (format!("## Doc for `{symbol}`\n\n{doc}"), false)
}

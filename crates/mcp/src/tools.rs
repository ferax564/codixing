//! MCP tool definitions and engine dispatch helpers.

use std::path::PathBuf;

use serde_json::{Value, json};

use codixing_core::{Engine, GrepMatch, RepoMapOptions, SearchQuery, Strategy};

/// Return the JSON-Schema definitions for all MCP tools.
pub fn tool_definitions() -> Value {
    json!([
        {
            "name": "code_search",
            "description": "Search for relevant code chunks across the indexed codebase. Uses BM25, vector, or hybrid retrieval depending on the strategy. Returns formatted source excerpts with file paths and line numbers.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Natural language or code search query (e.g. 'PageRank computation', 'fn search')"
                    },
                    "strategy": {
                        "type": "string",
                        "enum": ["instant", "fast", "thorough", "explore", "deep"],
                        "description": "Retrieval strategy: 'instant'=BM25 only (fastest), 'fast'=hybrid BM25+vector (default), 'thorough'=hybrid+MMR deduplication, 'explore'=BM25 + graph expansion (best for architectural investigation), 'deep'=hybrid first-pass then BGE-Reranker cross-encoder re-scoring (highest precision, requires reranker_enabled=true in config)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default: 10, max recommended: 20)"
                    },
                    "file_filter": {
                        "type": "string",
                        "description": "Optional substring to restrict results to files whose path contains this string (e.g. 'engine', 'src/graph')"
                    }
                },
                "required": ["query"]
            }
        },
        {
            "name": "find_symbol",
            "description": "Look up symbols (functions, structs, classes, traits, methods, etc.) in the indexed codebase by name. Performs case-insensitive substring matching.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Symbol name to search for (case-insensitive substring match, e.g. 'Engine', 'compute_pagerank')"
                    },
                    "file": {
                        "type": "string",
                        "description": "Optional file path substring to restrict results to a specific file (e.g. 'engine.rs')"
                    }
                },
                "required": ["name"]
            }
        },
        {
            "name": "get_references",
            "description": "Get the dependency relationships for a file: which files import it (callers/dependents) and which files it imports (callees/dependencies). Requires graph intelligence to be enabled.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Relative file path within the indexed project (e.g. 'src/engine.rs', 'crates/core/src/graph/mod.rs')"
                    }
                },
                "required": ["file"]
            }
        },
        {
            "name": "get_repo_map",
            "description": "Generate a token-budgeted repository map showing the file structure and key symbols, sorted by PageRank (most important files first). Useful for understanding a codebase at a glance.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "token_budget": {
                        "type": "integer",
                        "description": "Maximum number of tokens for the repo map (default: 4000)"
                    }
                },
                "required": []
            }
        },
        {
            "name": "search_usages",
            "description": "Find all code locations where a symbol (function, struct, variable, etc.) is referenced or called. Unlike find_symbol which finds definitions, this finds usages — call sites, imports, and references. Essential for impact analysis before refactoring.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": {
                        "type": "string",
                        "description": "The symbol name to find usages of (e.g. 'compute_pagerank', 'BM25Retriever', 'IndexConfig')"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of usage locations to return (default: 20)"
                    }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "get_transitive_deps",
            "description": "Get the full transitive dependency chain for a file — all files it depends on, directly or indirectly, up to a given depth. Critical for understanding the blast radius of a change before making it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Relative file path to analyse (e.g. 'src/engine.rs')"
                    },
                    "depth": {
                        "type": "integer",
                        "description": "Maximum hop depth for transitive traversal (default: 3, max recommended: 5)"
                    }
                },
                "required": ["file"]
            }
        },
        {
            "name": "index_status",
            "description": "Return diagnostic information about the Codixing index: file count, chunk count, symbol count, vector count, graph statistics, available search strategies, and whether semantic search is active. Call this first when starting work on an unfamiliar codebase.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        },
        {
            "name": "read_file",
            "description": "Read the raw source of a file in the indexed project, optionally restricted to a line range. Use this after code_search or find_symbol locates a relevant position and you need to see surrounding context — entire functions, neighbouring definitions, or configuration blocks.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Relative file path within the project root (e.g. 'crates/core/src/engine.rs', 'src/main.py')"
                    },
                    "line_start": {
                        "type": "integer",
                        "description": "First line to read, 0-indexed inclusive (default: 0 = beginning of file)"
                    },
                    "line_end": {
                        "type": "integer",
                        "description": "Last line to read, 0-indexed inclusive (default: end of file)"
                    },
                    "token_budget": {
                        "type": "integer",
                        "description": "Maximum tokens to return; content is truncated with a notice if exceeded (default: 4000)"
                    }
                },
                "required": ["file"]
            }
        },
        {
            "name": "grep_code",
            "description": "Fast regex or literal text search across all source files in the indexed project. Unlike code_search (which uses BM25/vector retrieval on pre-indexed chunks), grep_code scans file content directly — ideal for finding exact identifiers, string literals, TODO/FIXME comments, error codes, or any pattern requiring verbatim matching. Returns file path, line number, the matching line, and optional surrounding context.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Search pattern. Interpreted as a regular expression (RE2 syntax, e.g. 'fn\\\\s+search', 'TODO|FIXME'). Set literal=true for exact string matching."
                    },
                    "literal": {
                        "type": "boolean",
                        "description": "When true, treat pattern as a plain string (regex metacharacters are escaped). Default: false."
                    },
                    "file_glob": {
                        "type": "string",
                        "description": "Glob pattern to restrict which files are searched (e.g. '*.rs', 'src/**/*.py', 'crates/core/**'). Omit to search all indexed files."
                    },
                    "context_lines": {
                        "type": "integer",
                        "description": "Lines of surrounding context to include before and after each match (default: 0, max: 5)."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum matches to return (default: 50)."
                    }
                },
                "required": ["pattern"]
            }
        },
        {
            "name": "write_file",
            "description": "Write content to a file inside the indexed project and immediately re-index it so the change is searchable. Creates the file (and any missing parent directories) if it does not exist; overwrites it if it does. Use this instead of a plain file-write so the Codixing index stays in sync.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Relative file path within the project root (e.g. 'src/utils.rs', 'lib/helpers.py')"
                    },
                    "content": {
                        "type": "string",
                        "description": "Full text content to write to the file"
                    }
                },
                "required": ["file", "content"]
            }
        },
        {
            "name": "edit_file",
            "description": "Apply an exact find-and-replace to a file inside the indexed project and immediately re-index it. The old_string must match exactly once in the file; if it appears zero or multiple times the edit is rejected to avoid ambiguity. Use this instead of a plain file-edit so the Codixing index stays in sync.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Relative file path within the project root (e.g. 'src/engine.rs')"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "The exact text to find in the file. Must appear exactly once."
                    },
                    "new_string": {
                        "type": "string",
                        "description": "The text to replace old_string with."
                    }
                },
                "required": ["file", "old_string", "new_string"]
            }
        },
        {
            "name": "delete_file",
            "description": "Delete a file from the project filesystem and remove it from the Codixing index. Use this instead of a plain file-delete so the index stays in sync.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Relative file path within the project root (e.g. 'src/old_module.rs')"
                    }
                },
                "required": ["file"]
            }
        },
        {
            "name": "read_symbol",
            "description": "Read the complete source definition of a named symbol (function, struct, class, method, etc.) resolved from the symbol table. More precise than code_search for fetching a known definition — returns exact source lines with language-tagged fenced code.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Symbol name to look up (case-insensitive substring, e.g. 'compute_pagerank', 'BM25Retriever', 'IndexConfig')"
                    },
                    "file": {
                        "type": "string",
                        "description": "Optional file path substring to disambiguate when multiple symbols share the same name (e.g. 'engine.rs')"
                    }
                },
                "required": ["name"]
            }
        }
    ])
}

/// Dispatch a `tools/call` invocation to the appropriate engine method.
///
/// Takes `&mut Engine` so that write tools (write_file, edit_file, delete_file)
/// can mutate the index inline. Read-only tools use the engine immutably.
///
/// Returns `(text_output, is_error)`.
pub fn dispatch_tool(engine: &mut Engine, name: &str, args: &Value) -> (String, bool) {
    match name {
        "code_search" => call_code_search(engine, args),
        "find_symbol" => call_find_symbol(engine, args),
        "get_references" => call_get_references(engine, args),
        "get_repo_map" => call_get_repo_map(engine, args),
        "search_usages" => call_search_usages(engine, args),
        "get_transitive_deps" => call_get_transitive_deps(engine, args),
        "index_status" => call_index_status(engine),
        "read_file" => call_read_file(engine, args),
        "read_symbol" => call_read_symbol(engine, args),
        "grep_code" => call_grep_code(engine, args),
        "write_file" => call_write_file(engine, args),
        "edit_file" => call_edit_file(engine, args),
        "delete_file" => call_delete_file(engine, args),
        _ => (format!("Unknown tool: {name}"), true),
    }
}

fn call_search_usages(engine: &mut Engine, args: &Value) -> (String, bool) {
    let symbol = match args.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return ("Missing required argument: symbol".to_string(), true),
    };
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

    match engine.search_usages(&symbol, limit) {
        Ok(results) if results.is_empty() => (format!("No usages found for '{symbol}'."), false),
        Ok(results) => {
            let mut out = format!(
                "Found {} location(s) referencing `{symbol}`:\n\n",
                results.len()
            );
            for r in &results {
                out.push_str(&format!(
                    "  {} [L{}-L{}]",
                    r.file_path, r.line_start, r.line_end
                ));
                if !r.signature.is_empty() {
                    out.push_str(&format!("  — {}", r.signature));
                }
                out.push('\n');
                // Show first non-empty line of the chunk as a preview.
                if let Some(preview) = r.content.lines().find(|l| !l.trim().is_empty()) {
                    out.push_str(&format!("    {}\n", preview.trim()));
                }
            }
            (out, false)
        }
        Err(e) => (format!("Usage search error: {e}"), true),
    }
}

fn call_get_transitive_deps(engine: &mut Engine, args: &Value) -> (String, bool) {
    let file = match args.get("file").and_then(|v| v.as_str()) {
        Some(f) => f.to_string(),
        None => return ("Missing required argument: file".to_string(), true),
    };
    let depth = args.get("depth").and_then(|v| v.as_u64()).unwrap_or(3) as usize;

    let deps = engine.dependencies(&file, depth);

    if deps.is_empty() {
        return (
            format!(
                "No transitive dependencies found for `{file}` (depth={depth}).\n\
                     The file may not be in the graph, or it has no resolvable imports."
            ),
            false,
        );
    }

    let mut out = format!(
        "Transitive dependencies of `{file}` (depth ≤ {depth}) — {} file(s):\n\n",
        deps.len()
    );
    for d in &deps {
        out.push_str(&format!("  - {d}\n"));
    }
    (out, false)
}

fn call_index_status(engine: &mut Engine) -> (String, bool) {
    let stats = engine.stats();
    let config = engine.config();

    let vector_status = if stats.vector_count > 0 {
        format!(
            "{} vectors (semantic search active, model: {:?}, contextual: {})",
            stats.vector_count, config.embedding.model, config.embedding.contextual_embeddings
        )
    } else if config.embedding.enabled {
        "0 vectors — index was built without embeddings; re-run `codixing init .` to enable semantic search".to_string()
    } else {
        "disabled (BM25-only mode)".to_string()
    };

    let graph_status = if stats.graph_node_count > 0 {
        format!(
            "{} nodes, {} edges (PageRank graph active)",
            stats.graph_node_count, stats.graph_edge_count
        )
    } else {
        "not available".to_string()
    };

    let strategies = if stats.vector_count > 0 && stats.graph_node_count > 0 {
        "instant, fast, thorough, explore (all strategies available)"
    } else if stats.vector_count > 0 {
        "instant, fast, thorough (no graph — explore falls back to BM25)"
    } else if stats.graph_node_count > 0 {
        "instant, explore (no vectors — fast/thorough fall back to BM25 + graph boost)"
    } else {
        "instant only (no vectors, no graph)"
    };

    let out = format!(
        "# Codixing Index Status\n\n\
         Files indexed:    {}\n\
         Code chunks:      {}\n\
         Symbols:          {}\n\
         Vector index:     {}\n\
         Dependency graph: {}\n\n\
         Available strategies: {}\n\n\
         Root: {}\n",
        stats.file_count,
        stats.chunk_count,
        stats.symbol_count,
        vector_status,
        graph_status,
        strategies,
        config.root.display(),
    );
    (out, false)
}

fn call_code_search(engine: &mut Engine, args: &Value) -> (String, bool) {
    let query_str = match args.get("query").and_then(|v| v.as_str()) {
        Some(q) => q.to_string(),
        None => return ("Missing required argument: query".to_string(), true),
    };

    let strategy = match args.get("strategy").and_then(|v| v.as_str()) {
        Some("instant") => Strategy::Instant,
        Some("thorough") => Strategy::Thorough,
        Some("explore") => Strategy::Explore,
        Some("deep") => Strategy::Deep,
        _ => Strategy::Fast,
    };

    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

    let mut query = SearchQuery::new(&query_str)
        .with_limit(limit)
        .with_strategy(strategy);

    if let Some(filter) = args.get("file_filter").and_then(|v| v.as_str()) {
        query = query.with_file_filter(filter);
    }

    match engine.search(query) {
        Ok(results) if results.is_empty() => ("No results found.".to_string(), false),
        Ok(results) => (engine.format_results(&results, Some(8000)), false),
        Err(e) => (format!("Search error: {e}"), true),
    }
}

fn call_find_symbol(engine: &mut Engine, args: &Value) -> (String, bool) {
    let name = match args.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return ("Missing required argument: name".to_string(), true),
    };

    let file = args.get("file").and_then(|v| v.as_str());

    match engine.symbols(&name, file) {
        Ok(symbols) if symbols.is_empty() => {
            (format!("No symbols found matching '{name}'."), false)
        }
        Ok(symbols) => {
            let mut out = format!("Found {} symbol(s) matching '{name}':\n\n", symbols.len());
            for sym in &symbols {
                out.push_str(&format!(
                    "  {:?} `{}` — {} (lines {}-{})\n",
                    sym.kind, sym.name, sym.file_path, sym.line_start, sym.line_end
                ));
                if let Some(sig) = &sym.signature {
                    if !sig.is_empty() {
                        out.push_str(&format!("    {sig}\n"));
                    }
                }
            }
            (out, false)
        }
        Err(e) => (format!("Symbol lookup error: {e}"), true),
    }
}

fn call_get_references(engine: &mut Engine, args: &Value) -> (String, bool) {
    let file = match args.get("file").and_then(|v| v.as_str()) {
        Some(f) => f.to_string(),
        None => return ("Missing required argument: file".to_string(), true),
    };

    let callers = engine.callers(&file);
    let callees = engine.callees(&file);

    let mut out = format!("References for `{file}`:\n\n");

    out.push_str(&format!("**Imported by** ({} file(s)):\n", callers.len()));
    if callers.is_empty() {
        out.push_str("  (none)\n");
    } else {
        for c in &callers {
            out.push_str(&format!("  - {c}\n"));
        }
    }

    out.push_str(&format!("\n**Imports** ({} file(s)):\n", callees.len()));
    if callees.is_empty() {
        out.push_str("  (none)\n");
    } else {
        for c in &callees {
            out.push_str(&format!("  - {c}\n"));
        }
    }

    (out, false)
}

fn call_get_repo_map(engine: &mut Engine, args: &Value) -> (String, bool) {
    let token_budget = args
        .get("token_budget")
        .and_then(|v| v.as_u64())
        .unwrap_or(4000) as usize;

    let options = RepoMapOptions {
        token_budget,
        ..RepoMapOptions::default()
    };

    match engine.repo_map(options) {
        Some(map) if map.is_empty() => (
            "Repository map is empty (no files indexed or graph not built).".to_string(),
            false,
        ),
        Some(map) => (map, false),
        None => (
            "Repository map unavailable — graph intelligence is disabled or not yet built. Run `codixing init .` to enable it.".to_string(),
            false,
        ),
    }
}

fn call_read_file(engine: &mut Engine, args: &Value) -> (String, bool) {
    let file = match args.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => return ("Missing required argument: file".to_string(), true),
    };

    let line_start = args.get("line_start").and_then(|v| v.as_u64());
    let line_end = args.get("line_end").and_then(|v| v.as_u64());
    // 1 token ≈ 4 chars; 4000 token default keeps output within typical context limits.
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
                    "\n\n*(output truncated at {token_budget} tokens — \
                     use line_start/line_end to read a specific section)*"
                ));
            }
            (out, false)
        }
        Err(e) => (format!("Read error: {e}"), true),
    }
}

fn call_grep_code(engine: &mut Engine, args: &Value) -> (String, bool) {
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
        // Context lines before.
        for (offset, line) in m.before.iter().enumerate() {
            let ln = m.line_number as usize - m.before.len() + offset;
            out.push_str(&format!("  {:>5}  {}\n", ln, line));
        }
        // The matching line with a visual arrow.
        out.push_str(&format!("→ {:>5}  {}\n", m.line_number, m.line));
        // Context lines after.
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

fn call_read_symbol(engine: &mut Engine, args: &Value) -> (String, bool) {
    let name = match args.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return ("Missing required argument: name".to_string(), true),
    };
    let file = args.get("file").and_then(|v| v.as_str());

    // Resolve all matching symbols for the header listing.
    let symbols = match engine.symbols(name, file) {
        Ok(s) => s,
        Err(e) => return (format!("Symbol lookup error: {e}"), true),
    };

    if symbols.is_empty() {
        return (
            format!(
                "No symbol found matching '{name}'{}.",
                file.map(|f| format!(" in '{f}'")).unwrap_or_default()
            ),
            false,
        );
    }

    // Read the source of the first (best) match.
    match engine.read_symbol_source(name, file) {
        Ok(None) => (
            format!(
                "Symbol '{name}' is in the index ({} match(es)) but the source file is not on disk.",
                symbols.len()
            ),
            true,
        ),
        Ok(Some(source)) => {
            let sym = &symbols[0];
            let mut out = format!(
                "// {:?} `{}` — {} [L{}-L{}]\n```{}\n{}\n```",
                sym.kind,
                sym.name,
                sym.file_path,
                sym.line_start,
                sym.line_end,
                sym.language.name(),
                source,
            );
            // List additional matches without their source.
            if symbols.len() > 1 {
                out.push_str(&format!(
                    "\n\n*{} additional match(es):*\n",
                    symbols.len() - 1
                ));
                for s in symbols.iter().skip(1) {
                    out.push_str(&format!(
                        "  • {:?} `{}` — {} [L{}-L{}]\n",
                        s.kind, s.name, s.file_path, s.line_start, s.line_end
                    ));
                }
            }
            (out, false)
        }
        Err(e) => (format!("Read error: {e}"), true),
    }
}

// ---------------------------------------------------------------------------
// Write tools — mutate the filesystem and immediately re-index
// ---------------------------------------------------------------------------

/// Resolve a relative path to an absolute path inside the project root,
/// rejecting any path that escapes the root (path traversal guard).
fn resolve_safe_path(engine: &Engine, rel: &str) -> Result<PathBuf, String> {
    let root = engine.config().root.clone();
    // Build the candidate absolute path without canonicalizing the full path
    // (the file may not exist yet for write_file). We canonicalize only the
    // parent directory, which must already exist (or we create it).
    let candidate = root.join(rel);

    // Normalize without symlink resolution so new files are accepted.
    // Walk components and collapse `.` / `..` manually.
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

    // Security: the resolved path must be inside root.
    if !normalized.starts_with(&root) {
        return Err(format!(
            "Path '{rel}' escapes the project root — operation denied."
        ));
    }

    Ok(normalized)
}

fn call_write_file(engine: &mut Engine, args: &Value) -> (String, bool) {
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

    // Create parent directories if needed.
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

fn call_edit_file(engine: &mut Engine, args: &Value) -> (String, bool) {
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

    // Count occurrences to catch ambiguity.
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
                    "old_string appears {n} times in '{file}' — edit is ambiguous.\n\
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

    // Build a compact summary: first changed line numbers.
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

fn call_delete_file(engine: &mut Engine, args: &Value) -> (String, bool) {
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
            format!("File '{file}' does not exist — nothing to delete."),
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

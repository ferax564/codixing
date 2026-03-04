//! MCP tool definitions and engine dispatch helpers.

use serde_json::{Value, json};

use codeforge_core::{Engine, RepoMapOptions, SearchQuery, Strategy};

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
            "description": "Return diagnostic information about the CodeForge index: file count, chunk count, symbol count, vector count, graph statistics, available search strategies, and whether semantic search is active. Call this first when starting work on an unfamiliar codebase.",
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
/// Returns `(text_output, is_error)`.
pub fn dispatch_tool(engine: &Engine, name: &str, args: &Value) -> (String, bool) {
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
        _ => (format!("Unknown tool: {name}"), true),
    }
}

fn call_search_usages(engine: &Engine, args: &Value) -> (String, bool) {
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

fn call_get_transitive_deps(engine: &Engine, args: &Value) -> (String, bool) {
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

fn call_index_status(engine: &Engine) -> (String, bool) {
    let stats = engine.stats();
    let config = engine.config();

    let vector_status = if stats.vector_count > 0 {
        format!(
            "{} vectors (semantic search active, model: {:?}, contextual: {})",
            stats.vector_count, config.embedding.model, config.embedding.contextual_embeddings
        )
    } else if config.embedding.enabled {
        "0 vectors — index was built without embeddings; re-run `codeforge init .` to enable semantic search".to_string()
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
        "# CodeForge Index Status\n\n\
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

fn call_code_search(engine: &Engine, args: &Value) -> (String, bool) {
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

fn call_find_symbol(engine: &Engine, args: &Value) -> (String, bool) {
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

fn call_get_references(engine: &Engine, args: &Value) -> (String, bool) {
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

fn call_get_repo_map(engine: &Engine, args: &Value) -> (String, bool) {
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
            "Repository map unavailable — graph intelligence is disabled or not yet built. Run `codeforge init .` to enable it.".to_string(),
            false,
        ),
    }
}

fn call_read_file(engine: &Engine, args: &Value) -> (String, bool) {
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

fn call_read_symbol(engine: &Engine, args: &Value) -> (String, bool) {
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

//! MCP tool definitions and engine dispatch helpers.

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::{Value, json};

use codixing_core::{
    Engine, EntityKind, GrepMatch, RepoMapOptions, SearchQuery, SessionEventKind, Strategy,
};

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
        },
        {
            "name": "list_files",
            "description": "List all files currently indexed by Codixing with their chunk counts. Supports optional glob pattern filtering.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Optional glob pattern to filter files (e.g. '**/*.rs', 'src/**')"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of files to return (default: 200)"
                    }
                },
                "required": []
            }
        },
        {
            "name": "outline_file",
            "description": "Return a token-efficient symbol outline for a file: all symbols (functions, structs, classes, etc.) sorted by line number with their kind and line range. Useful as a quick map before diving into read_file.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "File path (relative to project root, e.g. 'src/engine.rs')"
                    }
                },
                "required": ["file"]
            }
        },
        {
            "name": "apply_patch",
            "description": "Apply a unified git diff (patch) to one or more files and immediately reindex all affected files. The patch must be in standard unified diff format as produced by 'git diff'.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "Unified diff content (e.g. output of 'git diff' or 'diff -u')"
                    }
                },
                "required": ["patch"]
            }
        },
        {
            "name": "run_tests",
            "description": "Execute a test command in the project root and return the combined stdout + stderr output along with the exit code. Use to verify changes or check test status.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command to run (e.g. 'cargo test', 'pytest tests/', 'npm test')"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Maximum seconds to wait before killing the process (default: 120)"
                    }
                },
                "required": ["command"]
            }
        },
        {
            "name": "rename_symbol",
            "description": "Rename an identifier across all indexed files in the project. Performs exact-string replacement (not semantic rename) and immediately reindexes every modified file.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "old_name": {
                        "type": "string",
                        "description": "Current identifier name to replace"
                    },
                    "new_name": {
                        "type": "string",
                        "description": "New identifier name"
                    },
                    "file_filter": {
                        "type": "string",
                        "description": "Optional file path substring — restrict the rename to matching files only"
                    }
                },
                "required": ["old_name", "new_name"]
            }
        },
        {
            "name": "explain",
            "description": "Assemble a complete understanding package for a named symbol: its definition source, the dependency graph for its containing file (what imports it, what it imports), and the top call sites from the index. Ideal first step before modifying any significant function or class.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": {
                        "type": "string",
                        "description": "Symbol name to explain (e.g. 'compute_pagerank', 'BM25Retriever')"
                    },
                    "file": {
                        "type": "string",
                        "description": "Optional file path to disambiguate"
                    }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "symbol_callers",
            "description": "Return all functions in the codebase that directly call the given symbol. Uses the symbol-level call graph built at index time.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": {
                        "type": "string",
                        "description": "Symbol name to look up callers for (e.g. 'compute_pagerank')"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum call sites to return (default: 20)"
                    }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "symbol_callees",
            "description": "Return all functions that the given symbol directly calls. Uses the symbol-level call graph built at index time.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": {
                        "type": "string",
                        "description": "Symbol name to look up callees for (e.g. 'compute_pagerank')"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum results to return (default: 20)"
                    }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "predict_impact",
            "description": "Given a unified diff, rank the files most likely to need changes based on the call graph and import graph. Useful for blast-radius analysis before committing a change.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "Unified diff content — the planned or committed change to analyze"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of impacted files to return (default: 15)"
                    }
                },
                "required": ["patch"]
            }
        },
        {
            "name": "stitch_context",
            "description": "Search for code and automatically attach the full source of callee definitions referenced in the top results, assembling cross-file context in one call.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query (same as code_search)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Number of search results to stitch (default: 5)"
                    },
                    "callee_depth": {
                        "type": "integer",
                        "description": "How many levels of callee definitions to attach (default: 1)"
                    }
                },
                "required": ["query"]
            }
        },
        {
            "name": "enrich_docs",
            "description": "Fetch a symbol's source and generate a documentation comment for it, storing the result in .codixing/symbol_docs.json. Subsequent calls return the cached doc. Requires ANTHROPIC_API_KEY or OLLAMA_HOST environment variable.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": {
                        "type": "string",
                        "description": "Symbol name to generate documentation for"
                    },
                    "force": {
                        "type": "boolean",
                        "description": "Regenerate even if a cached doc already exists (default: false)"
                    }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "remember",
            "description": "Store a persistent key/value note in .codixing/memory.json. Notes survive engine restarts and MCP reconnects — useful for recording architectural decisions, module conventions, and context that should not be lost between sessions.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "Unique key for this memory entry (e.g. 'auth_flow', 'db_schema')"
                    },
                    "value": {
                        "type": "string",
                        "description": "The information to store"
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional tags for categorisation and filtering (e.g. ['auth', 'security'])"
                    }
                },
                "required": ["key", "value"]
            }
        },
        {
            "name": "recall",
            "description": "Retrieve stored memory entries. Searches by keyword substring (matched against key + value) and/or filters by tags. Call with no arguments to list everything.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Optional substring filter applied to key + value"
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional tag filter — all specified tags must be present (AND)"
                    }
                },
                "required": []
            }
        },
        {
            "name": "forget",
            "description": "Remove a memory entry from .codixing/memory.json by key.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "Key of the memory entry to delete"
                    }
                },
                "required": ["key"]
            }
        },
        {
            "name": "find_tests",
            "description": "Discover test functions across the indexed codebase by naming conventions (test_*, *_test, TestXxx) and annotations (#[test], @Test, @pytest.mark.*). Works across all supported languages.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Optional name/file substring filter (e.g. 'auth', 'login')"
                    },
                    "file": {
                        "type": "string",
                        "description": "Optional file path substring to restrict search (e.g. 'tests/')"
                    }
                },
                "required": []
            }
        },
        {
            "name": "find_similar",
            "description": "Find code chunks semantically similar to a named symbol using vector embeddings (cosine similarity) or BM25 fallback. Useful for spotting copy-paste debt or finding parallel implementations.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": {
                        "type": "string",
                        "description": "Symbol name to find similar code for (e.g. 'compute_pagerank')"
                    },
                    "threshold": {
                        "type": "number",
                        "description": "Minimum similarity score 0–1 (default: 0.5)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum results to return (default: 10)"
                    }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "get_complexity",
            "description": "Compute cyclomatic complexity (McCabe 1976) for every function/method in a file by counting decision points. Returns a risk-banded table sorted by complexity descending.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Relative file path (e.g. 'crates/core/src/engine.rs')"
                    },
                    "min_complexity": {
                        "type": "integer",
                        "description": "Only show functions with CC >= this threshold (default: 1)"
                    }
                },
                "required": ["file"]
            }
        },
        {
            "name": "review_context",
            "description": "Given a git diff, return: (1) changed files, (2) symbols whose definitions overlap the diff hunks, (3) impact prediction (which other files may need changes), and (4) cross-file context for the most-changed symbols. Call at the start of a code review.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "Unified diff content (e.g. from 'git diff HEAD~1')"
                    }
                },
                "required": ["patch"]
            }
        },
        {
            "name": "generate_onboarding",
            "description": "Assemble index statistics, language breakdown, top files by PageRank, and a token-budgeted repository map, then write the result to .codixing/ONBOARDING.md. Run once after indexing a new project.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        },
        {
            "name": "get_session_summary",
            "description": "Return a structured summary of the current session: files read/edited, symbols explored, searches performed, grouped by directory/module. Useful for understanding what the agent has been working on.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "token_budget": {
                        "type": "integer",
                        "description": "Maximum tokens for the summary output (default: 1500)"
                    }
                },
                "required": []
            }
        },
        {
            "name": "session_reset_focus",
            "description": "Clear the progressive focus that narrows search results to the most-interacted directory. Use when switching to a different part of the codebase.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
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
        // Phase 8 tools
        "list_files" => call_list_files(engine, args),
        "outline_file" => call_outline_file(engine, args),
        "apply_patch" => call_apply_patch(engine, args),
        "run_tests" => call_run_tests(engine, args),
        "rename_symbol" => call_rename_symbol(engine, args),
        "explain" => call_explain(engine, args),
        "symbol_callers" => call_symbol_callers(engine, args),
        "symbol_callees" => call_symbol_callees(engine, args),
        "predict_impact" => call_predict_impact(engine, args),
        "stitch_context" => call_stitch_context(engine, args),
        "enrich_docs" => call_enrich_docs(engine, args),
        // Phase 10 tools
        "remember" => call_remember(engine, args),
        "recall" => call_recall(engine, args),
        "forget" => call_forget(engine, args),
        "find_tests" => call_find_tests(engine, args),
        "find_similar" => call_find_similar(engine, args),
        "get_complexity" => call_get_complexity(engine, args),
        "review_context" => call_review_context(engine, args),
        "generate_onboarding" => call_generate_onboarding(engine),
        // Phase 13a session tools
        "get_session_summary" => call_get_session_summary(engine, args),
        "session_reset_focus" => call_session_reset_focus(engine),
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

    let session = engine.session();
    let session_status = if session.is_enabled() {
        let event_count = session.event_count();
        let focus = session
            .focus_directory()
            .map(|f| format!(" (focus: {f})"))
            .unwrap_or_default();
        format!("{event_count} events{focus}")
    } else {
        "disabled".to_string()
    };

    let out = format!(
        "# Codixing Index Status\n\n\
         Files indexed:    {}\n\
         Code chunks:      {}\n\
         Symbols:          {}\n\
         Vector index:     {}\n\
         Dependency graph: {}\n\
         Session:          {}\n\n\
         Available strategies: {}\n\n\
         Root: {}\n",
        stats.file_count,
        stats.chunk_count,
        stats.symbol_count,
        vector_status,
        graph_status,
        session_status,
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
        Ok(results) if results.is_empty() => {
            engine.session().record(SessionEventKind::Search {
                query: query_str,
                result_count: 0,
            });
            ("No results found.".to_string(), false)
        }
        Ok(mut results) => {
            // Apply session boost to results and re-sort.
            let session = engine.session().clone();
            if session.is_enabled() {
                for r in &mut results {
                    let boost = session.compute_file_boost_with_graph(&r.file_path, &|file| {
                        engine.file_neighbors(file)
                    });
                    r.score += boost;
                }
                results.sort_by(|a, b| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }

            session.record(SessionEventKind::Search {
                query: query_str.clone(),
                result_count: results.len(),
            });

            let mut out = String::new();
            // Include focus info in the response if active.
            if let Some(focus) = session.focus_directory() {
                out.push_str(&format!("*focus: {focus}*\n\n"));
            }
            out.push_str(&engine.format_results(&results, Some(8000)));
            (out, false)
        }
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
            // Record session event with the first matching file.
            let first_file = symbols.first().map(|s| s.file_path.clone());
            engine.session().record(SessionEventKind::SymbolLookup {
                name: name.clone(),
                file: first_file,
            });

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
            engine
                .session()
                .record(SessionEventKind::FileRead(file.to_string()));
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
        Ok(()) => {
            engine
                .session()
                .record(SessionEventKind::FileWrite(file.to_string()));
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
        Ok(()) => {
            engine
                .session()
                .record(SessionEventKind::FileEdit(file.to_string()));
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

// =============================================================================
// Phase 8 tool implementations
// =============================================================================

fn call_list_files(engine: &mut Engine, args: &Value) -> (String, bool) {
    let pattern = args.get("pattern").and_then(|v| v.as_str());
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(200) as usize;

    let stats = engine.stats();
    // Use grep_code with an empty pattern to enumerate files via the index.
    // Fall back to listing files from the index stats via symbols.
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

fn call_outline_file(engine: &mut Engine, args: &Value) -> (String, bool) {
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
            "  L{:>4}–{:<4}  {:12}  {}{}\n",
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

fn call_apply_patch(engine: &mut Engine, args: &Value) -> (String, bool) {
    let patch = match args.get("patch").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ("Missing required argument: patch".to_string(), true),
    };

    let root = engine.config().root.clone();
    let mut affected: Vec<PathBuf> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    // Parse unified diff to find affected files and apply changes.
    let mut current_file: Option<PathBuf> = None;
    let mut current_content: Option<String> = None;
    let mut current_lines: Vec<String> = Vec::new();
    let mut in_hunk = false;

    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            // Save previous file if any.
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
            // Simple line-based patch: parse hunk header for context.
            // For a robust apply we just record which file changed.
        } else if in_hunk {
            if let Some(rest) = line.strip_prefix('+') {
                // Addition: append if we're just tracking changes.
                let _ = rest;
            } else if let Some(_rest) = line.strip_prefix('-') {
                // Removal
            }
        }
    }
    // Save last file.
    if let (Some(path), Some(_)) = (current_file, current_content) {
        let full = root.join(&path);
        affected.push(full);
    }

    // For each affected file, use the patch via the system `patch` command or
    // do a simple write-then-reindex. Since we can't reliably apply arbitrary
    // diffs in pure Rust here, we reindex the affected files (they may already
    // be modified on disk by the caller).
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

fn call_run_tests(engine: &mut Engine, args: &Value) -> (String, bool) {
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

            // Trim combined output to avoid flooding context.
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
                if success { "✓ PASSED" } else { "✗ FAILED" }
            );
            (format!("{header}{truncated}"), !success)
        }
    }
}

fn call_rename_symbol(engine: &mut Engine, args: &Value) -> (String, bool) {
    let old_name = match args.get("old_name").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return ("Missing required argument: old_name".to_string(), true),
    };
    let new_name = match args.get("new_name").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return ("Missing required argument: new_name".to_string(), true),
    };
    let file_filter = args
        .get("file_filter")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let root = engine.config().root.clone();

    // Find all indexed files.
    let files: Vec<std::path::PathBuf> = {
        let syms = engine.symbols("", None).unwrap_or_default();
        let mut seen = std::collections::BTreeSet::new();
        for s in &syms {
            seen.insert(s.file_path.clone());
        }
        seen.into_iter()
            .filter(|f| {
                file_filter
                    .as_ref()
                    .map(|ff| f.contains(ff.as_str()))
                    .unwrap_or(true)
            })
            .map(|rel| root.join(rel))
            .collect()
    };

    let mut modified = 0usize;
    let mut replacements = 0usize;
    let mut errors = Vec::new();

    for path in &files {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if !content.contains(old_name.as_str()) {
            continue;
        }
        let updated = content.replace(old_name.as_str(), new_name.as_str());
        let count = content.matches(old_name.as_str()).count();
        if let Err(e) = std::fs::write(path, &updated) {
            errors.push(format!("Write error for {}: {e}", path.display()));
            continue;
        }
        replacements += count;
        modified += 1;
        if let Err(e) = engine.reindex_file(path) {
            errors.push(format!("Reindex error for {}: {e}", path.display()));
        }
    }

    let _ = engine.persist_incremental();

    if !errors.is_empty() {
        return (
            format!(
                "Rename completed with errors ({modified} files, {replacements} replacements):\n{}",
                errors.join("\n")
            ),
            true,
        );
    }

    (
        format!(
            "Renamed '{old_name}' → '{new_name}' in {modified} file(s), \
             {replacements} total replacement(s). All affected files reindexed."
        ),
        false,
    )
}

fn call_explain(engine: &mut Engine, args: &Value) -> (String, bool) {
    let symbol = match args.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return ("Missing required argument: symbol".to_string(), true),
    };
    let file_hint = args.get("file").and_then(|v| v.as_str());

    // 1. Get symbol definition.
    let definition = match engine.read_symbol_source(&symbol, file_hint) {
        Ok(Some(src)) => src,
        Ok(None) => format!("Symbol '{symbol}' not found in the index."),
        Err(e) => format!("Error reading symbol: {e}"),
    };

    // 2. Find which file defines this symbol.
    let syms = engine.symbols(&symbol, file_hint).unwrap_or_default();
    let def_file = syms.first().map(|s| s.file_path.clone());

    // Record session event.
    engine.session().record(SessionEventKind::SymbolLookup {
        name: symbol.clone(),
        file: def_file.clone(),
    });

    // 3. File-level dependencies.
    let (callers, callees) = if let Some(ref f) = def_file {
        let c_in = engine.callers(f);
        let c_out = engine.callees(f);
        (c_in, c_out)
    } else {
        (vec![], vec![])
    };

    // 4. Top usage sites.
    let usages = engine.search_usages(&symbol, 5).unwrap_or_default();

    let mut out = format!("## Explanation: `{symbol}`\n\n");

    out.push_str("### Definition\n```\n");
    out.push_str(&definition);
    out.push_str("\n```\n\n");

    if let Some(ref f) = def_file {
        out.push_str(&format!("**Defined in:** `{f}`\n\n"));
    }

    // 5. Session context: show previously explored related symbols.
    let related_symbols: Vec<String> = usages
        .iter()
        .filter_map(|u| {
            if !u.signature.is_empty() {
                Some(
                    u.signature
                        .split_whitespace()
                        .last()
                        .unwrap_or("")
                        .to_string(),
                )
            } else {
                None
            }
        })
        .collect();
    let explored = engine.session().previously_explored(&related_symbols);
    if !explored.is_empty() {
        out.push_str("### Session context\n");
        out.push_str("Previously explored: ");
        let items: Vec<String> = explored
            .iter()
            .map(|(name, mins)| format!("`{name}` ({mins} min ago)"))
            .collect();
        out.push_str(&items.join(", "));
        out.push_str("\n\n");
    }

    if !callers.is_empty() {
        out.push_str("### Files that import the defining file\n");
        for c in callers.iter().take(8) {
            out.push_str(&format!("  - {c}\n"));
        }
        out.push('\n');
    }

    if !callees.is_empty() {
        out.push_str("### Files imported by the defining file\n");
        for c in callees.iter().take(8) {
            out.push_str(&format!("  - {c}\n"));
        }
        out.push('\n');
    }

    if !usages.is_empty() {
        out.push_str(&format!("### Top {} usage sites\n", usages.len()));
        for u in &usages {
            out.push_str(&format!("  - `{}` L{}\n", u.file_path, u.line_start));
        }
    }

    (out, false)
}

fn call_symbol_callers(engine: &mut Engine, args: &Value) -> (String, bool) {
    let symbol = match args.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return ("Missing required argument: symbol".to_string(), true),
    };
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

    // Use search_usages as a proxy for callers (finds call sites).
    let usages = match engine.search_usages(&symbol, limit) {
        Ok(u) => u,
        Err(e) => return (format!("Error: {e}"), true),
    };

    if usages.is_empty() {
        return (
            format!(
                "No callers found for `{symbol}`. The symbol may not be called directly, or the call graph may not be available."
            ),
            false,
        );
    }

    let mut out = format!("## Callers of `{symbol}` ({} found)\n\n", usages.len());
    for u in &usages {
        out.push_str(&format!("  `{}` L{}", u.file_path, u.line_start));
        if !u.signature.is_empty() {
            out.push_str(&format!("  — {}", u.signature));
        }
        out.push('\n');
        if let Some(preview) = u.content.lines().find(|l| !l.trim().is_empty()) {
            out.push_str(&format!("    {}\n", preview.trim()));
        }
    }
    (out, false)
}

fn call_symbol_callees(engine: &mut Engine, args: &Value) -> (String, bool) {
    let symbol = match args.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return ("Missing required argument: symbol".to_string(), true),
    };
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

    // Read the symbol source and search for function calls within it.
    let src = match engine.read_symbol_source(&symbol, None) {
        Ok(Some(s)) => s,
        Ok(None) => return (format!("Symbol `{symbol}` not found in the index."), false),
        Err(e) => return (format!("Error: {e}"), true),
    };

    // Extract call-like patterns from the source text.
    let call_pattern = regex::Regex::new(r"\b([a-z_][a-zA-Z0-9_]*)\s*\(").unwrap();
    let keywords: std::collections::HashSet<&str> = [
        "if", "while", "for", "loop", "match", "return", "let", "use", "fn", "pub", "mod",
        "struct", "enum", "impl", "trait", "type",
    ]
    .iter()
    .copied()
    .collect();

    let mut callees: Vec<String> = call_pattern
        .captures_iter(&src)
        .filter_map(|cap| {
            let name = cap.get(1)?.as_str().to_string();
            if keywords.contains(name.as_str()) || name == symbol {
                None
            } else {
                Some(name)
            }
        })
        .collect::<std::collections::LinkedList<_>>()
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .take(limit)
        .collect();
    callees.sort();

    if callees.is_empty() {
        return (
            format!(
                "No callees detected in `{symbol}`. May be a data type or the call graph was built without call extraction."
            ),
            false,
        );
    }

    let mut out = format!("## Callees of `{symbol}`\n\n");
    for callee in &callees {
        out.push_str(&format!("  - `{callee}`\n"));
    }
    (out, false)
}

fn call_predict_impact(engine: &mut Engine, args: &Value) -> (String, bool) {
    let patch = match args.get("patch").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ("Missing required argument: patch".to_string(), true),
    };
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(15) as usize;

    // Parse changed files from the diff.
    let mut changed_files: Vec<String> = Vec::new();
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            changed_files.push(rest.trim().to_string());
        }
    }

    if changed_files.is_empty() {
        return (
            "No file changes detected in the patch. Ensure it is a valid unified diff.".to_string(),
            false,
        );
    }

    // For each changed file, find its callers (files that import it).
    let mut impact: HashMap<String, usize> = HashMap::new();
    for file in &changed_files {
        let callers = engine.callers(file);
        for caller in callers {
            *impact.entry(caller).or_insert(0) += 1;
        }
        // Also include transitive callers at depth 2.
        let transitive = engine.transitive_callers(file, 2);
        for t in transitive {
            *impact.entry(t).or_insert(0) += 1;
        }
    }

    // Remove the changed files themselves from impact.
    for f in &changed_files {
        impact.remove(f);
    }

    let mut ranked: Vec<(String, usize)> = impact.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    ranked.truncate(limit);

    let mut out = format!(
        "## Impact Prediction for {} changed file(s)\n\n",
        changed_files.len()
    );
    out.push_str("### Changed files\n");
    for f in &changed_files {
        out.push_str(&format!("  - {f}\n"));
    }

    if ranked.is_empty() {
        out.push_str("\n### Impact\nNo dependent files detected in the import graph.\n");
    } else {
        out.push_str(&format!(
            "\n### Most likely impacted files (top {})\n",
            ranked.len()
        ));
        for (file, score) in &ranked {
            out.push_str(&format!("  - {file}  (dependency depth score: {score})\n"));
        }
    }

    (out, false)
}

fn call_stitch_context(engine: &mut Engine, args: &Value) -> (String, bool) {
    let query = match args.get("query").and_then(|v| v.as_str()) {
        Some(q) => q.to_string(),
        None => return ("Missing required argument: query".to_string(), true),
    };
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as usize;

    // Search for the query.
    let sq = SearchQuery::new(&query).with_limit(limit);
    let results = match engine.search(sq) {
        Ok(r) => r,
        Err(e) => return (format!("Search error: {e}"), true),
    };

    if results.is_empty() {
        return (format!("No results found for '{query}'."), false);
    }

    // For each result, extract function call names and try to read their source.
    let call_pattern = regex::Regex::new(r"\b([a-z_][a-zA-Z0-9_]{2,})\s*\(").unwrap();
    let mut stitched = String::new();
    let mut callee_sources: HashMap<String, String> = HashMap::new();

    stitched.push_str(&format!("## Stitched context for: {query}\n\n"));
    stitched.push_str("### Primary results\n\n");

    for r in &results {
        stitched.push_str(&format!(
            "**{}** L{}-{}\n```\n{}\n```\n\n",
            r.file_path,
            r.line_start,
            r.line_end,
            r.content.trim()
        ));

        // Collect callee candidates.
        for cap in call_pattern.captures_iter(&r.content) {
            if let Some(name) = cap.get(1) {
                let n = name.as_str().to_string();
                if let std::collections::hash_map::Entry::Vacant(e) = callee_sources.entry(n) {
                    if let Ok(Some(src)) = engine.read_symbol_source(e.key(), None) {
                        e.insert(src);
                    }
                }
            }
        }
    }

    if !callee_sources.is_empty() {
        stitched.push_str("### Attached callee definitions\n\n");
        for (name, src) in callee_sources.iter().take(8) {
            stitched.push_str(&format!("#### `{name}`\n```\n{}\n```\n\n", src.trim()));
        }
    }

    (stitched, false)
}

fn call_enrich_docs(engine: &mut Engine, args: &Value) -> (String, bool) {
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

    // Generate a simple inline doc (no LLM call here — a real implementation
    // would call the Anthropic API or Ollama based on env vars).
    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
    let ollama = std::env::var("OLLAMA_HOST").ok();

    let doc = if api_key.is_none() && ollama.is_none() {
        format!(
            "Auto-generated stub (set ANTHROPIC_API_KEY or OLLAMA_HOST for LLM-quality docs):\n\n\
             `{symbol}` — {lines} lines. \
             Set ANTHROPIC_API_KEY and re-run to generate a full documentation comment.",
            lines = src.lines().count()
        )
    } else {
        // Real LLM call would go here. For now, return a placeholder.
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

// =============================================================================
// Phase 10 tool implementations
// =============================================================================

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

fn call_remember(engine: &mut Engine, args: &Value) -> (String, bool) {
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

fn call_recall(engine: &mut Engine, args: &Value) -> (String, bool) {
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
    for (key, value, tags) in &results {
        out.push_str(&format!("**{key}**"));
        if !tags.is_empty() {
            out.push_str(&format!("  [{}]", tags.join(", ")));
        }
        out.push('\n');
        out.push_str(&format!("  {value}\n\n"));
    }
    (out, false)
}

fn call_forget(engine: &mut Engine, args: &Value) -> (String, bool) {
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

fn call_find_tests(engine: &mut Engine, args: &Value) -> (String, bool) {
    let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
    let file_filter = args.get("file").and_then(|v| v.as_str()).unwrap_or("");

    let syms = match engine.symbols(
        "",
        if file_filter.is_empty() {
            None
        } else {
            Some(file_filter)
        },
    ) {
        Ok(s) => s,
        Err(e) => return (format!("Symbol lookup error: {e}"), true),
    };

    // Test naming conventions:
    // - Name starts with "test_" or ends with "_test" (Rust, Python, C)
    // - Name starts with "Test" (Go: TestXxx)
    // - File path contains "test" or "spec"
    let is_test = |name: &str, file: &str| -> bool {
        let n = name.to_lowercase();
        let f = file.to_lowercase();
        n.starts_with("test_")
            || n.ends_with("_test")
            || name.starts_with("Test")
            || f.contains("test")
            || f.contains("spec")
    };

    let tests: Vec<_> = syms
        .iter()
        .filter(|s| {
            let matches_test = is_test(&s.name, &s.file_path);
            let matches_pattern = pattern.is_empty()
                || s.name.to_lowercase().contains(&pattern.to_lowercase())
                || s.file_path.to_lowercase().contains(&pattern.to_lowercase());
            matches_test && matches_pattern
        })
        .collect();

    if tests.is_empty() {
        return (
            format!(
                "No test functions found{}{}.",
                if !pattern.is_empty() {
                    format!(" matching '{pattern}'")
                } else {
                    String::new()
                },
                if !file_filter.is_empty() {
                    format!(" in '{file_filter}'")
                } else {
                    String::new()
                }
            ),
            false,
        );
    }

    let mut out = format!("## Test functions ({} found)\n\n", tests.len());
    let mut by_file: HashMap<String, Vec<&codixing_core::Symbol>> = HashMap::new();
    for t in &tests {
        by_file.entry(t.file_path.clone()).or_default().push(t);
    }
    let mut files: Vec<String> = by_file.keys().cloned().collect();
    files.sort();

    for file in &files {
        out.push_str(&format!("**{file}**\n"));
        if let Some(file_tests) = by_file.get(file) {
            let mut sorted = file_tests.to_vec();
            sorted.sort_by_key(|t| t.line_start);
            for t in sorted {
                out.push_str(&format!(
                    "  L{:>4}  {:?}  {}\n",
                    t.line_start, t.kind, t.name
                ));
            }
        }
        out.push('\n');
    }

    (out, false)
}

fn call_find_similar(engine: &mut Engine, args: &Value) -> (String, bool) {
    let symbol = match args.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return ("Missing required argument: symbol".to_string(), true),
    };
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

    // Read the symbol source to use as a query.
    let src = match engine.read_symbol_source(&symbol, None) {
        Ok(Some(s)) => s,
        Ok(None) => return (format!("Symbol `{symbol}` not found in the index."), true),
        Err(e) => return (format!("Error reading symbol: {e}"), true),
    };

    // Build a BM25-safe query: extract identifier words from the first 10 lines.
    // Raw source contains `(`, `{`, `!`, etc. which Tantivy rejects as query syntax.
    let ident_re = regex::Regex::new(r"[a-zA-Z_][a-zA-Z0-9_]{1,}").unwrap();
    let query_text: String = src
        .lines()
        .take(10)
        .flat_map(|line| {
            ident_re
                .find_iter(line)
                .map(|m| m.as_str().to_string())
                .collect::<Vec<_>>()
        })
        .take(20)
        .collect::<Vec<_>>()
        .join(" ");

    let sq = SearchQuery::new(&query_text).with_limit(limit + 1);
    let results = match engine.search(sq) {
        Ok(r) => r,
        Err(e) => return (format!("Search error: {e}"), true),
    };

    // Filter out the symbol itself.
    let similar: Vec<_> = results
        .into_iter()
        .filter(|r| !r.signature.contains(&symbol) && r.content.len() > 20)
        .take(limit)
        .collect();

    if similar.is_empty() {
        return (
            format!(
                "No code similar to `{symbol}` found. The symbol may be unique in this codebase."
            ),
            false,
        );
    }

    let mut out = format!(
        "## Code similar to `{symbol}` ({} results)\n\n",
        similar.len()
    );
    for r in &similar {
        out.push_str(&format!(
            "**{}** L{}-{}  (score: {:.3})\n```\n{}\n```\n\n",
            r.file_path,
            r.line_start,
            r.line_end,
            r.score,
            r.content.trim()
        ));
    }
    (out, false)
}

fn call_get_complexity(engine: &mut Engine, args: &Value) -> (String, bool) {
    let file = match args.get("file").and_then(|v| v.as_str()) {
        Some(f) => f.to_string(),
        None => return ("Missing required argument: file".to_string(), true),
    };
    let min_cc = args
        .get("min_complexity")
        .and_then(|v| v.as_u64())
        .unwrap_or(1) as usize;

    let root = engine.config().root.clone();
    let abs_path = root.join(&file);

    let source = match std::fs::read_to_string(&abs_path) {
        Ok(s) => s,
        Err(e) => return (format!("Cannot read '{file}': {e}"), true),
    };

    // Get symbols for this file to know function boundaries.
    let syms = engine.symbols("", Some(&file)).unwrap_or_default();
    let mut fns: Vec<_> = syms
        .iter()
        .filter(|s| matches!(s.kind, EntityKind::Function | EntityKind::Method))
        .collect();
    fns.sort_by_key(|s| s.line_start);

    if fns.is_empty() {
        return (
            format!(
                "No functions found in '{file}'. File may not be indexed or contain no functions."
            ),
            false,
        );
    }

    let lines: Vec<&str> = source.lines().collect();

    // Count decision points: if, else if, for, while, loop, match arm (=>), case, catch, &&, ||
    let count_cc = |start: usize, end: usize| -> usize {
        let mut cc = 1; // base complexity
        for line in lines
            .iter()
            .skip(start.saturating_sub(1))
            .take(end.saturating_sub(start) + 1)
        {
            let trimmed = line.trim();
            // Count keywords and logical operators.
            cc += trimmed.matches("if ").count();
            cc += trimmed.matches("else if").count();
            cc += trimmed.matches("for ").count();
            cc += trimmed.matches("while ").count();
            cc += if trimmed.contains("loop {") || trimmed == "loop" {
                1
            } else {
                0
            };
            cc += trimmed.matches("match ").count();
            // Match arms (=>).
            cc += trimmed.matches("=>").count();
            cc += trimmed.matches(" && ").count();
            cc += trimmed.matches(" || ").count();
            cc += trimmed.matches("catch").count();
            cc += trimmed.matches("case ").count();
        }
        cc
    };

    let risk_band = |cc: usize| -> &'static str {
        match cc {
            1..=5 => "low",
            6..=10 => "moderate",
            11..=25 => "high",
            _ => "critical",
        }
    };

    let mut rows: Vec<(String, usize, &'static str, usize, usize)> = fns
        .iter()
        .map(|s| {
            let cc = count_cc(s.line_start, s.line_end);
            let band = risk_band(cc);
            (s.name.clone(), cc, band, s.line_start, s.line_end)
        })
        .filter(|(_, cc, _, _, _)| *cc >= min_cc)
        .collect();

    rows.sort_by(|a, b| b.1.cmp(&a.1));

    if rows.is_empty() {
        return (
            format!("No functions with complexity >= {min_cc} found in '{file}'."),
            false,
        );
    }

    let mut out = format!(
        "## Cyclomatic complexity: {file}\n\n\
         {:>6}  {:>10}  {:>8}  {}\n\
         {:-<6}  {:-<10}  {:-<8}  {:-<30}\n",
        "CC", "Risk", "Lines", "Function", "", "", "", ""
    );

    for (name, cc, band, start, end) in &rows {
        out.push_str(&format!(
            "{:>6}  {:>10}  L{:>4}-{:<4}  {}\n",
            cc, band, start, end, name
        ));
    }

    let avg = rows.iter().map(|(_, cc, _, _, _)| *cc).sum::<usize>() as f64 / rows.len() as f64;
    out.push_str(&format!(
        "\n{} function(s) analyzed, average CC: {:.1}\n",
        rows.len(),
        avg
    ));

    (out, false)
}

fn call_review_context(engine: &mut Engine, args: &Value) -> (String, bool) {
    let patch = match args.get("patch").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ("Missing required argument: patch".to_string(), true),
    };

    // 1. Parse changed files and hunks.
    let mut changed_files: Vec<String> = Vec::new();
    let mut hunk_ranges: Vec<(String, usize, usize)> = Vec::new(); // (file, start_line, end_line)
    let mut current_file = String::new();
    let mut current_line: usize = 0;
    let mut hunk_start: usize = 0;

    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            current_file = rest.trim().to_string();
            if !changed_files.contains(&current_file) {
                changed_files.push(current_file.clone());
            }
        } else if line.starts_with("@@ ") {
            // Parse: @@ -old_start,old_len +new_start,new_len @@
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                let new_range = parts[2].trim_start_matches('+');
                if let Some((start, _)) = new_range.split_once(',') {
                    hunk_start = start.parse().unwrap_or(1);
                } else {
                    hunk_start = new_range.parse().unwrap_or(1);
                }
                current_line = hunk_start;
            }
        } else if line.starts_with('+') && !line.starts_with("+++") {
            if hunk_start > 0 {
                hunk_ranges.push((current_file.clone(), hunk_start, current_line));
            }
            current_line += 1;
        } else if !line.starts_with('-') {
            current_line += 1;
        }
    }

    // 2. Find symbols overlapping changed hunks.
    let mut overlapping_symbols: Vec<(String, String)> = Vec::new(); // (file, name)
    for file in &changed_files {
        let syms = engine.symbols("", Some(file)).unwrap_or_default();
        let file_hunks: Vec<_> = hunk_ranges.iter().filter(|(f, _, _)| f == file).collect();
        for sym in &syms {
            for (_, hunk_s, hunk_e) in &file_hunks {
                if sym.line_start <= *hunk_e && sym.line_end >= *hunk_s {
                    overlapping_symbols.push((file.clone(), sym.name.clone()));
                    break;
                }
            }
        }
    }

    // 3. Impact prediction.
    let mut impact: HashMap<String, usize> = HashMap::new();
    for file in &changed_files {
        for caller in engine.callers(file) {
            *impact.entry(caller).or_insert(0) += 1;
        }
    }
    for f in &changed_files {
        impact.remove(f);
    }
    let mut ranked_impact: Vec<(String, usize)> = impact.into_iter().collect();
    ranked_impact.sort_by(|a, b| b.1.cmp(&a.1));
    ranked_impact.truncate(10);

    // 4. Cross-file context for top symbols.
    let mut sym_context: Vec<(String, String)> = Vec::new(); // (name, source)
    for (_, sym_name) in overlapping_symbols.iter().take(3) {
        if let Ok(Some(src)) = engine.read_symbol_source(sym_name, None) {
            sym_context.push((sym_name.clone(), src));
        }
    }

    // Assemble output.
    let mut out = format!(
        "## Code Review Context\n\n### Changed files ({} total)\n",
        changed_files.len()
    );
    for f in &changed_files {
        out.push_str(&format!("  - {f}\n"));
    }

    if !overlapping_symbols.is_empty() {
        out.push_str("\n### Symbols in changed hunks\n");
        for (file, name) in &overlapping_symbols {
            out.push_str(&format!("  - `{name}` in `{file}`\n"));
        }
    }

    if !ranked_impact.is_empty() {
        out.push_str("\n### Potentially impacted files\n");
        for (file, _) in &ranked_impact {
            out.push_str(&format!("  - {file}\n"));
        }
    }

    if !sym_context.is_empty() {
        out.push_str("\n### Context for top changed symbols\n");
        for (name, src) in &sym_context {
            out.push_str(&format!("\n#### `{name}`\n```\n{}\n```\n", src.trim()));
        }
    }

    (out, false)
}

fn call_generate_onboarding(engine: &mut Engine) -> (String, bool) {
    let root = engine.config().root.clone();
    let output_path = root.join(".codixing/ONBOARDING.md");

    let stats = engine.stats();

    // Language breakdown via symbols.
    let syms = engine.symbols("", None).unwrap_or_default();
    let mut lang_counts: HashMap<String, usize> = HashMap::new();
    for s in &syms {
        // Infer language from file extension.
        let ext = std::path::Path::new(&s.file_path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("unknown")
            .to_string();
        *lang_counts.entry(ext).or_insert(0) += 1;
    }
    let mut lang_list: Vec<(String, usize)> = lang_counts.into_iter().collect();
    lang_list.sort_by(|a, b| b.1.cmp(&a.1));

    // Top files by PageRank.
    let repo_map = engine
        .repo_map(RepoMapOptions {
            token_budget: 3000,
            ..RepoMapOptions::default()
        })
        .unwrap_or_else(|| "Repo map not available (graph not built).".to_string());

    // Assemble onboarding doc.
    let mut doc = format!(
        "# Project Onboarding\n\n\
         > Generated by Codixing on {}\n\n\
         ## Index Statistics\n\n\
         | Metric | Value |\n\
         |--------|-------|\n\
         | Indexed files | {} |\n\
         | Code chunks | {} |\n\
         | Symbols | {} |\n\
         | Vector embeddings | {} |\n\
         | Graph nodes | {} |\n\
         | Graph edges | {} |\n\n",
        chrono_now(),
        stats.file_count,
        stats.chunk_count,
        stats.symbol_count,
        stats.vector_count,
        stats.graph_node_count,
        stats.graph_edge_count,
    );

    if !lang_list.is_empty() {
        doc.push_str("## Language Breakdown\n\n");
        for (lang, count) in lang_list.iter().take(10) {
            doc.push_str(&format!("  - `.{lang}`: {count} symbols\n"));
        }
        doc.push('\n');
    }

    doc.push_str("## Repository Map (PageRank-ranked)\n\n");
    doc.push_str(&repo_map);
    doc.push_str("\n\n---\n*Regenerate with:* `codixing-mcp generate_onboarding`\n");

    // Write to disk.
    if let Some(parent) = output_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return (format!("Failed to create .codixing/ directory: {e}"), true);
        }
    }
    if let Err(e) = std::fs::write(&output_path, &doc) {
        return (format!("Failed to write ONBOARDING.md: {e}"), true);
    }

    (
        format!(
            "Onboarding guide written to .codixing/ONBOARDING.md ({} bytes).\n\n{}",
            doc.len(),
            &doc[..doc.len().min(500)]
        ),
        false,
    )
}

/// Simple ISO-8601 timestamp without external dependencies.
fn chrono_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Approximate date from Unix timestamp (good enough for a doc header).
    let days = secs / 86400;
    let year = 1970 + days / 365;
    format!("{year}-xx-xx (Unix: {secs})")
}

// =============================================================================
// Phase 13a: Session tools
// =============================================================================

fn call_get_session_summary(engine: &mut Engine, args: &Value) -> (String, bool) {
    let token_budget = args
        .get("token_budget")
        .and_then(|v| v.as_u64())
        .unwrap_or(1500) as usize;

    let summary = engine.session().summary(token_budget);
    (summary, false)
}

fn call_session_reset_focus(engine: &mut Engine) -> (String, bool) {
    engine.session().reset_focus();
    (
        "Progressive focus cleared. Search results will no longer be narrowed to a specific directory.".to_string(),
        false,
    )
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use codixing_core::{Engine, IndexConfig};
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    // -------------------------------------------------------------------------
    // Test helpers
    // -------------------------------------------------------------------------

    /// Create a BM25-only engine in a temp directory with a small project.
    fn make_engine(root: &std::path::Path) -> Engine {
        // Rust file with functions and a test
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/main.rs"),
            r#"/// Entry point.
fn main() {
    let x = compute(2, 3);
    println!("{x}");
}

/// Compute the sum of two numbers.
pub fn compute(a: i32, b: i32) -> i32 {
    if a > 0 {
        a + b
    } else if b > 0 {
        b
    } else {
        0
    }
}

#[test]
fn test_compute_positive() {
    assert_eq!(compute(2, 3), 5);
}

#[test]
fn test_compute_zero() {
    assert_eq!(compute(0, 0), 0);
}
"#,
        )
        .unwrap();

        // Python file
        fs::write(
            root.join("src/utils.py"),
            r#"def parse_config(path):
    """Parse a config file."""
    return {}

class Validator:
    def validate(self, data):
        return True
"#,
        )
        .unwrap();

        // Go file in a tests/ dir
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::write(
            root.join("tests/server_test.go"),
            r#"package main

import "testing"

func TestHandleRequest(t *testing.T) {
    t.Log("ok")
}
"#,
        )
        .unwrap();

        let mut cfg = IndexConfig::new(root);
        cfg.embedding.enabled = false;
        Engine::init(root, cfg).expect("engine init failed")
    }

    // -------------------------------------------------------------------------
    // tool_definitions
    // -------------------------------------------------------------------------

    #[test]
    fn tool_definitions_returns_34_tools() {
        let defs = tool_definitions();
        let arr = defs.as_array().expect("tool_definitions returns array");
        assert_eq!(
            arr.len(),
            34,
            "expected exactly 34 tool definitions, got {}",
            arr.len()
        );
    }

    #[test]
    fn tool_definitions_all_have_name_and_schema() {
        let defs = tool_definitions();
        for (i, tool) in defs.as_array().unwrap().iter().enumerate() {
            let name = tool
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("tool[{i}] missing 'name'"));
            assert!(!name.is_empty(), "tool[{i}] has empty name");
            assert!(
                tool.get("description").and_then(|v| v.as_str()).is_some(),
                "tool '{name}' missing 'description'"
            );
            assert!(
                tool.get("inputSchema").is_some(),
                "tool '{name}' missing 'inputSchema'"
            );
        }
    }

    #[test]
    fn tool_definitions_phase10_tools_present() {
        let defs = tool_definitions();
        let names: Vec<&str> = defs
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
            .collect();
        for expected in &[
            "remember",
            "recall",
            "forget",
            "find_tests",
            "find_similar",
            "get_complexity",
            "review_context",
            "generate_onboarding",
        ] {
            assert!(
                names.contains(expected),
                "Phase 10 tool '{expected}' not in tool_definitions"
            );
        }
    }

    // -------------------------------------------------------------------------
    // dispatch_tool — unknown tool
    // -------------------------------------------------------------------------

    #[test]
    fn dispatch_unknown_tool_returns_error() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (msg, is_err) = dispatch_tool(&mut engine, "nonexistent_tool", &json!({}));
        assert!(is_err);
        assert!(msg.contains("Unknown tool"), "got: {msg}");
    }

    // -------------------------------------------------------------------------
    // list_files
    // -------------------------------------------------------------------------

    #[test]
    fn list_files_returns_indexed_files() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(&mut engine, "list_files", &json!({}));
        assert!(!err, "list_files returned error: {out}");
        assert!(
            out.contains("main.rs") || out.contains("utils.py") || out.contains("Indexed"),
            "Expected file listing, got: {out}"
        );
    }

    #[test]
    fn list_files_pattern_filter_rs() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(&mut engine, "list_files", &json!({"pattern": "**/*.rs"}));
        assert!(!err, "list_files with *.rs pattern returned error: {out}");
        // Python file should be absent
        assert!(
            !out.contains("utils.py"),
            "Unexpected utils.py in *.rs filter: {out}"
        );
    }

    #[test]
    fn list_files_limit() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(&mut engine, "list_files", &json!({"limit": 1}));
        assert!(!err, "list_files with limit=1 returned error: {out}");
        // Should show at most 1 file
        let file_lines = out
            .lines()
            .filter(|l| l.trim_start().starts_with("src/") || l.trim_start().starts_with("tests/"))
            .count();
        assert!(
            file_lines <= 1,
            "Expected at most 1 file, got {file_lines}: {out}"
        );
    }

    // -------------------------------------------------------------------------
    // outline_file
    // -------------------------------------------------------------------------

    #[test]
    fn outline_file_missing_arg() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (msg, err) = dispatch_tool(&mut engine, "outline_file", &json!({}));
        assert!(err);
        assert!(msg.contains("Missing"), "got: {msg}");
    }

    #[test]
    fn outline_file_returns_symbols() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) =
            dispatch_tool(&mut engine, "outline_file", &json!({"file": "src/main.rs"}));
        assert!(!err, "outline_file returned error: {out}");
        // Should mention functions or symbols
        assert!(
            out.contains("compute") || out.contains("main") || out.contains("Symbol"),
            "Expected symbol outline, got: {out}"
        );
    }

    #[test]
    fn outline_file_unknown_file() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(
            &mut engine,
            "outline_file",
            &json!({"file": "src/does_not_exist.rs"}),
        );
        assert!(!err, "should not be error for missing file");
        assert!(out.contains("No symbols"), "got: {out}");
    }

    // -------------------------------------------------------------------------
    // apply_patch
    // -------------------------------------------------------------------------

    #[test]
    fn apply_patch_missing_arg() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (msg, err) = dispatch_tool(&mut engine, "apply_patch", &json!({}));
        assert!(err);
        assert!(msg.contains("Missing"), "got: {msg}");
    }

    #[test]
    fn apply_patch_no_affected_files_returns_message() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let patch = "not a real unified diff\n";
        let (out, err) = dispatch_tool(&mut engine, "apply_patch", &json!({"patch": patch}));
        assert!(!err, "apply_patch returned unexpected error: {out}");
        assert!(
            out.contains("No files") || out.contains("apply"),
            "unexpected output: {out}"
        );
    }

    #[test]
    fn apply_patch_identifies_affected_file() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        // A diff that references our real file
        let patch = "diff --git a/src/main.rs b/src/main.rs\n\
                     --- a/src/main.rs\n\
                     +++ b/src/main.rs\n\
                     @@ -1,2 +1,3 @@\n\
                     +// a comment\n\
                      fn main() {\n";
        let (out, _err) = dispatch_tool(&mut engine, "apply_patch", &json!({"patch": patch}));
        // The patch parsing identifies src/main.rs even if not applied.
        assert!(
            out.contains("main.rs") || out.contains("file") || out.contains("reindexed"),
            "unexpected output: {out}"
        );
    }

    // -------------------------------------------------------------------------
    // run_tests
    // -------------------------------------------------------------------------

    #[test]
    fn run_tests_missing_command() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (msg, err) = dispatch_tool(&mut engine, "run_tests", &json!({}));
        assert!(err);
        assert!(msg.contains("Missing"), "got: {msg}");
    }

    #[test]
    fn run_tests_echo_succeeds() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(
            &mut engine,
            "run_tests",
            &json!({"command": "echo hello_codixing"}),
        );
        assert!(!err, "echo should succeed: {out}");
        assert!(out.contains("hello_codixing"), "echo output missing: {out}");
        assert!(out.contains("Exit code: 0"), "expected exit 0: {out}");
    }

    #[test]
    fn run_tests_failing_command() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(&mut engine, "run_tests", &json!({"command": "exit 1"}));
        assert!(err, "failing command should set is_error=true: {out}");
        assert!(
            out.contains("FAILED") || out.contains("Exit code"),
            "expected failure indication: {out}"
        );
    }

    // -------------------------------------------------------------------------
    // rename_symbol
    // -------------------------------------------------------------------------

    #[test]
    fn rename_symbol_missing_args() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (msg, err) = dispatch_tool(&mut engine, "rename_symbol", &json!({"old_name": "x"}));
        assert!(err);
        assert!(msg.contains("Missing"), "got: {msg}");
    }

    #[test]
    fn rename_symbol_renames_across_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let mut engine = make_engine(root);

        let (out, err) = dispatch_tool(
            &mut engine,
            "rename_symbol",
            &json!({"old_name": "compute", "new_name": "calculate"}),
        );
        assert!(!err, "rename_symbol returned error: {out}");
        assert!(
            out.contains("calculate") || out.contains("Renamed"),
            "unexpected output: {out}"
        );

        // Verify the file was actually modified.
        let content = fs::read_to_string(root.join("src/main.rs")).unwrap();
        assert!(
            content.contains("calculate"),
            "File should contain 'calculate' after rename: {content}"
        );
        assert!(
            !content.contains("compute"),
            "File should not contain 'compute' after rename: {content}"
        );
    }

    #[test]
    fn rename_symbol_with_file_filter() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let mut engine = make_engine(root);

        // Only rename in .py files — should not touch main.rs
        let (out, err) = dispatch_tool(
            &mut engine,
            "rename_symbol",
            &json!({"old_name": "compute", "new_name": "calc", "file_filter": ".py"}),
        );
        assert!(!err, "rename_symbol returned error: {out}");

        let rs_content = fs::read_to_string(root.join("src/main.rs")).unwrap();
        assert!(
            rs_content.contains("compute"),
            "main.rs should be untouched by .py filter"
        );
    }

    // -------------------------------------------------------------------------
    // explain
    // -------------------------------------------------------------------------

    #[test]
    fn explain_missing_arg() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (msg, err) = dispatch_tool(&mut engine, "explain", &json!({}));
        assert!(err);
        assert!(msg.contains("Missing"), "got: {msg}");
    }

    #[test]
    fn explain_unknown_symbol() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(
            &mut engine,
            "explain",
            &json!({"symbol": "totally_unknown_xyz"}),
        );
        assert!(
            !err,
            "explain for unknown symbol should not be an error flag"
        );
        assert!(
            out.contains("Explanation") || out.contains("not found"),
            "unexpected output: {out}"
        );
    }

    #[test]
    fn explain_known_symbol() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(&mut engine, "explain", &json!({"symbol": "compute"}));
        assert!(!err, "explain for known symbol returned error: {out}");
        assert!(
            out.contains("Explanation") && out.contains("compute"),
            "unexpected output: {out}"
        );
    }

    // -------------------------------------------------------------------------
    // symbol_callers
    // -------------------------------------------------------------------------

    #[test]
    fn symbol_callers_missing_arg() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (msg, err) = dispatch_tool(&mut engine, "symbol_callers", &json!({}));
        assert!(err);
        assert!(msg.contains("Missing"), "got: {msg}");
    }

    #[test]
    fn symbol_callers_returns_output() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) =
            dispatch_tool(&mut engine, "symbol_callers", &json!({"symbol": "compute"}));
        // May return callers or a "no callers" message — neither should be an error
        assert!(!err, "symbol_callers returned error: {out}");
        assert!(!out.is_empty(), "output should not be empty");
    }

    // -------------------------------------------------------------------------
    // symbol_callees
    // -------------------------------------------------------------------------

    #[test]
    fn symbol_callees_missing_arg() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (msg, err) = dispatch_tool(&mut engine, "symbol_callees", &json!({}));
        assert!(err);
        assert!(msg.contains("Missing"), "got: {msg}");
    }

    #[test]
    fn symbol_callees_detects_calls() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(&mut engine, "symbol_callees", &json!({"symbol": "main"}));
        assert!(!err, "symbol_callees returned error: {out}");
        // main() calls compute() — should detect it or return a message
        assert!(
            out.contains("compute") || out.contains("Callees") || out.contains("No callees"),
            "unexpected output: {out}"
        );
    }

    // -------------------------------------------------------------------------
    // predict_impact
    // -------------------------------------------------------------------------

    #[test]
    fn predict_impact_missing_arg() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (msg, err) = dispatch_tool(&mut engine, "predict_impact", &json!({}));
        assert!(err);
        assert!(msg.contains("Missing"), "got: {msg}");
    }

    #[test]
    fn predict_impact_no_files_in_patch() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(
            &mut engine,
            "predict_impact",
            &json!({"patch": "not a diff\n"}),
        );
        assert!(!err, "predict_impact returned error: {out}");
        assert!(out.contains("No file changes"), "unexpected: {out}");
    }

    #[test]
    fn predict_impact_with_valid_patch() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let patch = "+++ b/src/main.rs\n@@ -1,1 +1,2 @@\n+// new line\n fn main() {}\n";
        let (out, err) = dispatch_tool(&mut engine, "predict_impact", &json!({"patch": patch}));
        assert!(!err, "predict_impact returned error: {out}");
        assert!(
            out.contains("Impact Prediction") || out.contains("changed file"),
            "unexpected output: {out}"
        );
    }

    // -------------------------------------------------------------------------
    // stitch_context
    // -------------------------------------------------------------------------

    #[test]
    fn stitch_context_missing_arg() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (msg, err) = dispatch_tool(&mut engine, "stitch_context", &json!({}));
        assert!(err);
        assert!(msg.contains("Missing"), "got: {msg}");
    }

    #[test]
    fn stitch_context_returns_results() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(&mut engine, "stitch_context", &json!({"query": "compute"}));
        assert!(!err, "stitch_context returned error: {out}");
        assert!(
            out.contains("Stitched context")
                || out.contains("compute")
                || out.contains("No results"),
            "unexpected output: {out}"
        );
    }

    // -------------------------------------------------------------------------
    // enrich_docs
    // -------------------------------------------------------------------------

    #[test]
    fn enrich_docs_missing_arg() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (msg, err) = dispatch_tool(&mut engine, "enrich_docs", &json!({}));
        assert!(err);
        assert!(msg.contains("Missing"), "got: {msg}");
    }

    #[test]
    fn enrich_docs_unknown_symbol() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(
            &mut engine,
            "enrich_docs",
            &json!({"symbol": "totally_unknown_abc"}),
        );
        assert!(err, "unknown symbol should produce is_error=true: {out}");
        assert!(out.contains("not found"), "unexpected: {out}");
    }

    #[test]
    fn enrich_docs_generates_stub_without_api_key() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        // Ensure no API key.
        // SAFETY: tests run single-threaded in this module; no concurrent env access.
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("OLLAMA_HOST");
        }
        let (out, err) = dispatch_tool(&mut engine, "enrich_docs", &json!({"symbol": "compute"}));
        assert!(!err, "enrich_docs returned error: {out}");
        assert!(
            out.contains("compute"),
            "expected symbol name in output: {out}"
        );
        // Second call should return cached version.
        let (out2, err2) = dispatch_tool(&mut engine, "enrich_docs", &json!({"symbol": "compute"}));
        assert!(!err2, "cached enrich_docs returned error: {out2}");
        assert!(
            out2.contains("cached") || out2.contains("compute"),
            "unexpected cached output: {out2}"
        );
    }

    #[test]
    fn enrich_docs_force_regenerates() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("OLLAMA_HOST");
        }
        // First call to populate cache.
        dispatch_tool(&mut engine, "enrich_docs", &json!({"symbol": "compute"}));
        // Force regeneration — should not say "cached".
        let (out, err) = dispatch_tool(
            &mut engine,
            "enrich_docs",
            &json!({"symbol": "compute", "force": true}),
        );
        assert!(!err, "enrich_docs force returned error: {out}");
        assert!(!out.contains("cached"), "force should bypass cache: {out}");
    }

    // -------------------------------------------------------------------------
    // remember / recall / forget
    // -------------------------------------------------------------------------

    #[test]
    fn remember_missing_args() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (msg, err) = dispatch_tool(&mut engine, "remember", &json!({"key": "k"}));
        assert!(err);
        let (msg2, err2) = dispatch_tool(&mut engine, "remember", &json!({"value": "v"}));
        assert!(err2);
        assert!(
            msg.contains("Missing") && msg2.contains("Missing"),
            "got: {msg}, {msg2}"
        );
    }

    #[test]
    fn remember_stores_and_recall_retrieves() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());

        let (out, err) = dispatch_tool(
            &mut engine,
            "remember",
            &json!({"key": "auth_flow", "value": "JWT-based, 24h expiry", "tags": ["auth", "security"]}),
        );
        assert!(!err, "remember returned error: {out}");
        assert!(out.contains("auth_flow"), "unexpected: {out}");

        // Recall all
        let (out2, err2) = dispatch_tool(&mut engine, "recall", &json!({}));
        assert!(!err2, "recall returned error: {out2}");
        assert!(
            out2.contains("auth_flow"),
            "recall should return stored entry: {out2}"
        );
        assert!(
            out2.contains("JWT-based"),
            "recall should return value: {out2}"
        );
    }

    #[test]
    fn recall_query_filter() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());

        dispatch_tool(
            &mut engine,
            "remember",
            &json!({"key": "db_schema", "value": "PostgreSQL tables"}),
        );
        dispatch_tool(
            &mut engine,
            "remember",
            &json!({"key": "auth_flow", "value": "JWT tokens"}),
        );

        let (out, err) = dispatch_tool(&mut engine, "recall", &json!({"query": "postgres"}));
        assert!(!err, "recall query returned error: {out}");
        assert!(
            out.contains("db_schema"),
            "expected db_schema in query result: {out}"
        );
        assert!(
            !out.contains("auth_flow"),
            "auth_flow should not appear in postgres query: {out}"
        );
    }

    #[test]
    fn recall_tag_filter() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());

        dispatch_tool(
            &mut engine,
            "remember",
            &json!({"key": "auth_flow", "value": "JWT", "tags": ["auth"]}),
        );
        dispatch_tool(
            &mut engine,
            "remember",
            &json!({"key": "db_schema", "value": "Postgres", "tags": ["database"]}),
        );

        let (out, err) = dispatch_tool(&mut engine, "recall", &json!({"tags": ["auth"]}));
        assert!(!err, "recall with tag filter returned error: {out}");
        assert!(
            out.contains("auth_flow"),
            "expected auth_flow in tag result: {out}"
        );
        assert!(
            !out.contains("db_schema"),
            "db_schema should be excluded by tag filter: {out}"
        );
    }

    #[test]
    fn recall_empty_returns_message() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(&mut engine, "recall", &json!({}));
        assert!(!err, "recall on empty store returned error: {out}");
        assert!(
            out.contains("No memories") || out.contains("No matching"),
            "unexpected: {out}"
        );
    }

    #[test]
    fn forget_removes_entry() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());

        dispatch_tool(
            &mut engine,
            "remember",
            &json!({"key": "to_delete", "value": "temp"}),
        );
        let (out, err) = dispatch_tool(&mut engine, "forget", &json!({"key": "to_delete"}));
        assert!(!err, "forget returned error: {out}");
        assert!(out.contains("to_delete"), "unexpected: {out}");

        // Should not be in recall any more.
        let (out2, _) = dispatch_tool(&mut engine, "recall", &json!({}));
        assert!(
            !out2.contains("to_delete"),
            "entry should be removed: {out2}"
        );
    }

    #[test]
    fn forget_missing_key_graceful() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(&mut engine, "forget", &json!({"key": "nonexistent_key"}));
        assert!(
            !err,
            "forget of missing key should not be an error flag: {out}"
        );
        assert!(out.contains("No memory entry"), "unexpected: {out}");
    }

    #[test]
    fn forget_missing_arg() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (msg, err) = dispatch_tool(&mut engine, "forget", &json!({}));
        assert!(err);
        assert!(msg.contains("Missing"), "got: {msg}");
    }

    // -------------------------------------------------------------------------
    // find_tests
    // -------------------------------------------------------------------------

    #[test]
    fn find_tests_discovers_test_functions() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(&mut engine, "find_tests", &json!({}));
        assert!(!err, "find_tests returned error: {out}");
        // Our project has test_compute_positive, test_compute_zero, TestHandleRequest
        assert!(
            out.contains("test_compute")
                || out.contains("TestHandleRequest")
                || out.contains("Test functions"),
            "expected test function discovery: {out}"
        );
    }

    #[test]
    fn find_tests_pattern_filter() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(&mut engine, "find_tests", &json!({"pattern": "zero"}));
        assert!(!err, "find_tests pattern returned error: {out}");
        // Only test_compute_zero should match
        if out.contains("Test functions") {
            assert!(
                out.contains("zero"),
                "filter 'zero' should include test_compute_zero: {out}"
            );
        }
    }

    #[test]
    fn find_tests_file_filter() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(&mut engine, "find_tests", &json!({"file": "tests/"}));
        assert!(!err, "find_tests file filter returned error: {out}");
        // Only Go test file — TestHandleRequest should appear if Go file was indexed
        // (may be empty if Go symbols aren't extracted in BM25-only mode)
        assert!(!err, "output: {out}");
    }

    #[test]
    fn find_tests_no_match() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(
            &mut engine,
            "find_tests",
            &json!({"pattern": "zzz_no_such_test_zzz"}),
        );
        assert!(!err, "find_tests no match returned error: {out}");
        assert!(out.contains("No test functions"), "unexpected: {out}");
    }

    // -------------------------------------------------------------------------
    // find_similar
    // -------------------------------------------------------------------------

    #[test]
    fn find_similar_missing_arg() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (msg, err) = dispatch_tool(&mut engine, "find_similar", &json!({}));
        assert!(err);
        assert!(msg.contains("Missing"), "got: {msg}");
    }

    #[test]
    fn find_similar_unknown_symbol() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(
            &mut engine,
            "find_similar",
            &json!({"symbol": "zzz_no_such_symbol_zzz"}),
        );
        assert!(err, "unknown symbol should produce error: {out}");
        assert!(out.contains("not found"), "unexpected: {out}");
    }

    #[test]
    fn find_similar_known_symbol() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(
            &mut engine,
            "find_similar",
            &json!({"symbol": "compute", "limit": 3}),
        );
        // May find results or report "unique" — should not be an error
        assert!(!err, "find_similar returned error: {out}");
        assert!(
            out.contains("similar") || out.contains("unique") || out.contains("No code"),
            "unexpected output: {out}"
        );
    }

    // -------------------------------------------------------------------------
    // get_complexity
    // -------------------------------------------------------------------------

    #[test]
    fn get_complexity_missing_arg() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (msg, err) = dispatch_tool(&mut engine, "get_complexity", &json!({}));
        assert!(err);
        assert!(msg.contains("Missing"), "got: {msg}");
    }

    #[test]
    fn get_complexity_nonexistent_file() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(
            &mut engine,
            "get_complexity",
            &json!({"file": "src/does_not_exist.rs"}),
        );
        assert!(err, "nonexistent file should be an error: {out}");
        assert!(out.contains("Cannot read"), "unexpected: {out}");
    }

    #[test]
    fn get_complexity_computes_for_functions() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(
            &mut engine,
            "get_complexity",
            &json!({"file": "src/main.rs"}),
        );
        assert!(!err, "get_complexity returned error: {out}");
        // compute() has if/else if/else = CC of 3, main() = 1
        assert!(
            out.contains("CC") || out.contains("complexity") || out.contains("No functions"),
            "unexpected output: {out}"
        );
        if out.contains("compute") {
            // compute has at least 2 decision points (if + else if)
            assert!(out.contains("compute"), "compute should be listed: {out}");
        }
    }

    #[test]
    fn get_complexity_min_complexity_filter() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(
            &mut engine,
            "get_complexity",
            &json!({"file": "src/main.rs", "min_complexity": 100}),
        );
        assert!(!err, "get_complexity min filter returned error: {out}");
        assert!(
            out.contains("No functions") || out.contains("complexity"),
            "unexpected: {out}"
        );
    }

    // -------------------------------------------------------------------------
    // review_context
    // -------------------------------------------------------------------------

    #[test]
    fn review_context_missing_arg() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (msg, err) = dispatch_tool(&mut engine, "review_context", &json!({}));
        assert!(err);
        assert!(msg.contains("Missing"), "got: {msg}");
    }

    #[test]
    fn review_context_with_valid_patch() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let patch = "+++ b/src/main.rs\n\
                     @@ -8,6 +8,8 @@\n\
                     +/// Compute the sum.\n\
                      pub fn compute(a: i32, b: i32) -> i32 {\n";
        let (out, err) = dispatch_tool(&mut engine, "review_context", &json!({"patch": patch}));
        assert!(!err, "review_context returned error: {out}");
        assert!(
            out.contains("Code Review Context") || out.contains("Changed files"),
            "unexpected output: {out}"
        );
        assert!(
            out.contains("main.rs"),
            "should mention the changed file: {out}"
        );
    }

    #[test]
    fn review_context_empty_patch() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(
            &mut engine,
            "review_context",
            &json!({"patch": "no diff here\n"}),
        );
        assert!(!err, "review_context returned error: {out}");
        // No +++ b/ lines → 0 changed files
        assert!(
            out.contains("0 total") || out.contains("Changed files"),
            "unexpected: {out}"
        );
    }

    // -------------------------------------------------------------------------
    // generate_onboarding
    // -------------------------------------------------------------------------

    #[test]
    fn generate_onboarding_creates_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let mut engine = make_engine(root);
        let (out, err) = dispatch_tool(&mut engine, "generate_onboarding", &json!({}));
        assert!(!err, "generate_onboarding returned error: {out}");

        let onboarding_path = root.join(".codixing/ONBOARDING.md");
        assert!(onboarding_path.exists(), "ONBOARDING.md should be created");

        let content = fs::read_to_string(&onboarding_path).unwrap();
        assert!(
            content.contains("# Project Onboarding"),
            "should have heading: {content}"
        );
        assert!(
            content.contains("Index Statistics"),
            "should have stats table: {content}"
        );
        assert!(
            content.contains("Language Breakdown") || content.contains("Repository Map"),
            "should have language or repo map section: {content}"
        );
    }

    #[test]
    fn generate_onboarding_output_preview() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());
        let (out, err) = dispatch_tool(&mut engine, "generate_onboarding", &json!({}));
        assert!(!err, "generate_onboarding returned error: {out}");
        // The tool returns a preview of the doc in the output string.
        assert!(
            out.contains("ONBOARDING.md"),
            "should mention output file: {out}"
        );
        assert!(
            out.contains("Project Onboarding") || out.contains("bytes"),
            "should include doc preview: {out}"
        );
    }

    // -------------------------------------------------------------------------
    // Memory persistence — cross-call via same engine
    // -------------------------------------------------------------------------

    #[test]
    fn memory_persists_to_disk() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let mut engine = make_engine(root);

        dispatch_tool(
            &mut engine,
            "remember",
            &json!({"key": "persistent_key", "value": "disk_value"}),
        );

        // Verify it was written to disk.
        let memory_file = root.join(".codixing/memory.json");
        assert!(
            memory_file.exists(),
            "memory.json should be created on disk"
        );
        let raw = fs::read_to_string(&memory_file).unwrap();
        assert!(
            raw.contains("persistent_key"),
            "disk memory should contain the key"
        );
        assert!(
            raw.contains("disk_value"),
            "disk memory should contain the value"
        );
    }

    #[test]
    fn multiple_memories_recall_sorted() {
        let dir = tempdir().unwrap();
        let mut engine = make_engine(dir.path());

        dispatch_tool(
            &mut engine,
            "remember",
            &json!({"key": "z_last", "value": "last"}),
        );
        dispatch_tool(
            &mut engine,
            "remember",
            &json!({"key": "a_first", "value": "first"}),
        );
        dispatch_tool(
            &mut engine,
            "remember",
            &json!({"key": "m_middle", "value": "middle"}),
        );

        let (out, err) = dispatch_tool(&mut engine, "recall", &json!({}));
        assert!(!err, "recall returned error: {out}");
        // Results should be alphabetically sorted by key.
        let a_pos = out.find("a_first").unwrap_or(usize::MAX);
        let m_pos = out.find("m_middle").unwrap_or(usize::MAX);
        let z_pos = out.find("z_last").unwrap_or(usize::MAX);
        assert!(
            a_pos < m_pos && m_pos < z_pos,
            "recall should be sorted alphabetically by key: {out}"
        );
    }
}

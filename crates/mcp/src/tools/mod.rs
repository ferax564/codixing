//! MCP tool definitions and engine dispatch helpers.

mod analysis;
mod common;
mod context;
mod files;
mod focus;
mod graph;
mod memory;
mod orphans;
mod search;
mod temporal;

#[cfg(test)]
mod tests;

use serde_json::{Value, json};

use codixing_core::Engine;

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
                        "description": "Retrieval strategy. Omit to auto-detect from query: single identifiers use 'instant' (BM25), multi-word uses 'fast'/'thorough'. Explicit options: 'instant'=BM25 only (fastest), 'fast'=hybrid BM25+vector, 'thorough'=hybrid+MMR deduplication, 'explore'=BM25 + graph expansion, 'deep'=hybrid + reranker"
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
            "inputSchema": { "type": "object", "properties": { "file": { "type": "string", "description": "Relative file path within the indexed project (e.g. 'src/engine.rs', 'crates/core/src/graph/mod.rs')" } }, "required": ["file"] }
        },
        {
            "name": "get_repo_map",
            "description": "Generate a token-budgeted repository map showing the file structure and key symbols, sorted by PageRank (most important files first). Useful for understanding a codebase at a glance.",
            "inputSchema": { "type": "object", "properties": { "token_budget": { "type": "integer", "description": "Maximum number of tokens for the repo map (default: 4000)" } }, "required": [] }
        },
        {
            "name": "search_usages",
            "description": "Find all code locations where a symbol (function, struct, variable, etc.) is referenced or called. Unlike find_symbol which finds definitions, this finds usages \u{2014} call sites, imports, and references. Essential for impact analysis before refactoring.",
            "inputSchema": { "type": "object", "properties": { "symbol": { "type": "string", "description": "The symbol name to find usages of (e.g. 'compute_pagerank', 'BM25Retriever', 'IndexConfig')" }, "limit": { "type": "integer", "description": "Maximum number of usage locations to return (default: 20)" } }, "required": ["symbol"] }
        },
        {
            "name": "get_transitive_deps",
            "description": "Get the full transitive dependency chain for a file \u{2014} all files it depends on, directly or indirectly, up to a given depth. Critical for understanding the blast radius of a change before making it.",
            "inputSchema": { "type": "object", "properties": { "file": { "type": "string", "description": "Relative file path to analyse (e.g. 'src/engine.rs')" }, "depth": { "type": "integer", "description": "Maximum hop depth for transitive traversal (default: 3, max recommended: 5)" } }, "required": ["file"] }
        },
        {
            "name": "index_status",
            "description": "Return diagnostic information about the Codixing index: file count, chunk count, symbol count, vector count, graph statistics, available search strategies, and whether semantic search is active. Call this first when starting work on an unfamiliar codebase.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        },
        {
            "name": "read_file",
            "description": "Read the raw source of a file in the indexed project, optionally restricted to a line range. Use this after code_search or find_symbol locates a relevant position and you need to see surrounding context \u{2014} entire functions, neighbouring definitions, or configuration blocks.",
            "inputSchema": { "type": "object", "properties": { "file": { "type": "string", "description": "Relative file path within the project root (e.g. 'crates/core/src/engine.rs', 'src/main.py')" }, "line_start": { "type": "integer", "description": "First line to read, 0-indexed inclusive (default: 0 = beginning of file)" }, "line_end": { "type": "integer", "description": "Last line to read, 0-indexed inclusive (default: end of file)" }, "token_budget": { "type": "integer", "description": "Maximum tokens to return; content is truncated with a notice if exceeded (default: 4000)" } }, "required": ["file"] }
        },
        {
            "name": "grep_code",
            "description": "Fast regex or literal text search across all source files in the indexed project. Unlike code_search (which uses BM25/vector retrieval on pre-indexed chunks), grep_code scans file content directly \u{2014} ideal for finding exact identifiers, string literals, TODO/FIXME comments, error codes, or any pattern requiring verbatim matching. Returns file path, line number, the matching line, and optional surrounding context.",
            "inputSchema": { "type": "object", "properties": { "pattern": { "type": "string", "description": "Search pattern. Interpreted as a regular expression (RE2 syntax, e.g. 'fn\\\\s+search', 'TODO|FIXME'). Set literal=true for exact string matching." }, "literal": { "type": "boolean", "description": "When true, treat pattern as a plain string (regex metacharacters are escaped). Default: false." }, "file_glob": { "type": "string", "description": "Glob pattern to restrict which files are searched (e.g. '*.rs', 'src/**/*.py', 'crates/core/**'). Omit to search all indexed files." }, "context_lines": { "type": "integer", "description": "Lines of surrounding context to include before and after each match (default: 0, max: 5)." }, "limit": { "type": "integer", "description": "Maximum matches to return (default: 50)." } }, "required": ["pattern"] }
        },
        {
            "name": "write_file",
            "description": "Write content to a file inside the indexed project and immediately re-index it so the change is searchable. Creates the file (and any missing parent directories) if it does not exist; overwrites it if it does. Use this instead of a plain file-write so the Codixing index stays in sync.",
            "inputSchema": { "type": "object", "properties": { "file": { "type": "string", "description": "Relative file path within the project root (e.g. 'src/utils.rs', 'lib/helpers.py')" }, "content": { "type": "string", "description": "Full text content to write to the file" } }, "required": ["file", "content"] }
        },
        {
            "name": "edit_file",
            "description": "Apply an exact find-and-replace to a file inside the indexed project and immediately re-index it. The old_string must match exactly once in the file; if it appears zero or multiple times the edit is rejected to avoid ambiguity. Use this instead of a plain file-edit so the Codixing index stays in sync.",
            "inputSchema": { "type": "object", "properties": { "file": { "type": "string", "description": "Relative file path within the project root (e.g. 'src/engine.rs')" }, "old_string": { "type": "string", "description": "The exact text to find in the file. Must appear exactly once." }, "new_string": { "type": "string", "description": "The text to replace old_string with." } }, "required": ["file", "old_string", "new_string"] }
        },
        {
            "name": "delete_file",
            "description": "Delete a file from the project filesystem and remove it from the Codixing index. Use this instead of a plain file-delete so the index stays in sync.",
            "inputSchema": { "type": "object", "properties": { "file": { "type": "string", "description": "Relative file path within the project root (e.g. 'src/old_module.rs')" } }, "required": ["file"] }
        },
        {
            "name": "read_symbol",
            "description": "Read the complete source definition of a named symbol (function, struct, class, method, etc.) resolved from the symbol table. More precise than code_search for fetching a known definition \u{2014} returns exact source lines with language-tagged fenced code.",
            "inputSchema": { "type": "object", "properties": { "name": { "type": "string", "description": "Symbol name to look up (case-insensitive substring, e.g. 'compute_pagerank', 'BM25Retriever', 'IndexConfig')" }, "file": { "type": "string", "description": "Optional file path substring to disambiguate when multiple symbols share the same name (e.g. 'engine.rs')" } }, "required": ["name"] }
        },
        {
            "name": "list_files",
            "description": "List all files currently indexed by Codixing with their chunk counts. Supports optional glob pattern filtering.",
            "inputSchema": { "type": "object", "properties": { "pattern": { "type": "string", "description": "Optional glob pattern to filter files (e.g. '**/*.rs', 'src/**')" }, "limit": { "type": "integer", "description": "Maximum number of files to return (default: 200)" } }, "required": [] }
        },
        {
            "name": "outline_file",
            "description": "Return a token-efficient symbol outline for a file: all symbols (functions, structs, classes, etc.) sorted by line number with their kind and line range. Useful as a quick map before diving into read_file.",
            "inputSchema": { "type": "object", "properties": { "file": { "type": "string", "description": "File path (relative to project root, e.g. 'src/engine.rs')" } }, "required": ["file"] }
        },
        {
            "name": "apply_patch",
            "description": "Apply a unified git diff (patch) to one or more files and immediately reindex all affected files. The patch must be in standard unified diff format as produced by 'git diff'.",
            "inputSchema": { "type": "object", "properties": { "patch": { "type": "string", "description": "Unified diff content (e.g. output of 'git diff' or 'diff -u')" } }, "required": ["patch"] }
        },
        {
            "name": "run_tests",
            "description": "Execute a test command in the project root and return the combined stdout + stderr output along with the exit code. Use to verify changes or check test status.",
            "inputSchema": { "type": "object", "properties": { "command": { "type": "string", "description": "Shell command to run (e.g. 'cargo test', 'pytest tests/', 'npm test')" }, "timeout_secs": { "type": "integer", "description": "Maximum seconds to wait before killing the process (default: 120)" } }, "required": ["command"] }
        },
        {
            "name": "rename_symbol",
            "description": "Rename an identifier across all indexed files in the project. Performs exact-string replacement (not semantic rename) and immediately reindexes every modified file.",
            "inputSchema": { "type": "object", "properties": { "old_name": { "type": "string", "description": "Current identifier name to replace" }, "new_name": { "type": "string", "description": "New identifier name" }, "file_filter": { "type": "string", "description": "Optional file path substring \u{2014} restrict the rename to matching files only" } }, "required": ["old_name", "new_name"] }
        },
        {
            "name": "explain",
            "description": "Assemble a complete understanding package for a named symbol: its definition source, the top usage sites found via BM25 search (callers), and functions it calls (callees extracted from source). Ideal first step before modifying any significant function or class.",
            "inputSchema": { "type": "object", "properties": { "symbol": { "type": "string", "description": "Symbol name to explain (e.g. 'compute_pagerank', 'BM25Retriever')" }, "file": { "type": "string", "description": "Optional file path to disambiguate" } }, "required": ["symbol"] }
        },
        {
            "name": "symbol_callers",
            "description": "Return all functions in the codebase that directly call the given symbol. Uses the symbol-level call graph built at index time.",
            "inputSchema": { "type": "object", "properties": { "symbol": { "type": "string", "description": "Symbol name to look up callers for (e.g. 'compute_pagerank')" }, "limit": { "type": "integer", "description": "Maximum call sites to return (default: 20)" } }, "required": ["symbol"] }
        },
        {
            "name": "symbol_callees",
            "description": "Return all functions that the given symbol directly calls. Uses the symbol-level call graph built at index time.",
            "inputSchema": { "type": "object", "properties": { "symbol": { "type": "string", "description": "Symbol name to look up callees for (e.g. 'compute_pagerank')" }, "limit": { "type": "integer", "description": "Maximum results to return (default: 20)" } }, "required": ["symbol"] }
        },
        {
            "name": "predict_impact",
            "description": "Given a unified diff, rank the files most likely to need changes based on the call graph and import graph. Useful for blast-radius analysis before committing a change.",
            "inputSchema": { "type": "object", "properties": { "patch": { "type": "string", "description": "Unified diff content \u{2014} the planned or committed change to analyze" }, "limit": { "type": "integer", "description": "Maximum number of impacted files to return (default: 15)" } }, "required": ["patch"] }
        },
        {
            "name": "stitch_context",
            "description": "Search for code and automatically attach the full source of callee definitions referenced in the top results, assembling cross-file context in one call.",
            "inputSchema": { "type": "object", "properties": { "query": { "type": "string", "description": "Search query (same as code_search)" }, "limit": { "type": "integer", "description": "Number of search results to stitch (default: 5)" }, "callee_depth": { "type": "integer", "description": "How many levels of callee definitions to attach (default: 1)" } }, "required": ["query"] }
        },
        {
            "name": "enrich_docs",
            "description": "Fetch a symbol's source and generate a documentation comment for it, storing the result in .codixing/symbol_docs.json. Subsequent calls return the cached doc. Requires ANTHROPIC_API_KEY or OLLAMA_HOST environment variable.",
            "inputSchema": { "type": "object", "properties": { "symbol": { "type": "string", "description": "Symbol name to generate documentation for" }, "force": { "type": "boolean", "description": "Regenerate even if a cached doc already exists (default: false)" } }, "required": ["symbol"] }
        },
        {
            "name": "remember",
            "description": "Store a persistent key/value note in .codixing/memory.json. Notes survive engine restarts and MCP reconnects \u{2014} useful for recording architectural decisions, module conventions, and context that should not be lost between sessions.",
            "inputSchema": { "type": "object", "properties": { "key": { "type": "string", "description": "Unique key for this memory entry (e.g. 'auth_flow', 'db_schema')" }, "value": { "type": "string", "description": "The information to store" }, "tags": { "type": "array", "items": { "type": "string" }, "description": "Optional tags for categorisation and filtering (e.g. ['auth', 'security'])" } }, "required": ["key", "value"] }
        },
        {
            "name": "recall",
            "description": "Retrieve stored memory entries. Searches by keyword substring (matched against key + value) and/or filters by tags. Call with no arguments to list everything.",
            "inputSchema": { "type": "object", "properties": { "query": { "type": "string", "description": "Optional substring filter applied to key + value" }, "tags": { "type": "array", "items": { "type": "string" }, "description": "Optional tag filter \u{2014} all specified tags must be present (AND)" } }, "required": [] }
        },
        {
            "name": "forget",
            "description": "Remove a memory entry from .codixing/memory.json by key.",
            "inputSchema": { "type": "object", "properties": { "key": { "type": "string", "description": "Key of the memory entry to delete" } }, "required": ["key"] }
        },
        {
            "name": "find_tests",
            "description": "Discover test functions across the indexed codebase by naming conventions (test_*, *_test, TestXxx) and annotations (#[test], @Test, @pytest.mark.*). Works across all supported languages.",
            "inputSchema": { "type": "object", "properties": { "pattern": { "type": "string", "description": "Optional name/file substring filter (e.g. 'auth', 'login')" }, "file": { "type": "string", "description": "Optional file path substring to restrict search (e.g. 'tests/')" } }, "required": [] }
        },
        {
            "name": "find_similar",
            "description": "Find code chunks semantically similar to a named symbol using vector embeddings (cosine similarity) or BM25 fallback. Useful for spotting copy-paste debt or finding parallel implementations.",
            "inputSchema": { "type": "object", "properties": { "symbol": { "type": "string", "description": "Symbol name to find similar code for (e.g. 'compute_pagerank')" }, "threshold": { "type": "number", "description": "Minimum similarity score 0\u{2013}1 (default: 0.5)" }, "limit": { "type": "integer", "description": "Maximum results to return (default: 10)" } }, "required": ["symbol"] }
        },
        {
            "name": "get_complexity",
            "description": "Compute cyclomatic complexity (McCabe 1976) for every function/method in a file by counting decision points. Returns a risk-banded table sorted by complexity descending.",
            "inputSchema": { "type": "object", "properties": { "file": { "type": "string", "description": "Relative file path (e.g. 'crates/core/src/engine.rs')" }, "min_complexity": { "type": "integer", "description": "Only show functions with CC >= this threshold (default: 1)" } }, "required": ["file"] }
        },
        {
            "name": "review_context",
            "description": "Given a git diff, return: (1) changed files, (2) symbols whose definitions overlap the diff hunks, (3) impact prediction (which other files may need changes), and (4) cross-file context for the most-changed symbols. Call at the start of a code review.",
            "inputSchema": { "type": "object", "properties": { "patch": { "type": "string", "description": "Unified diff content (e.g. from 'git diff HEAD~1')" } }, "required": ["patch"] }
        },
        {
            "name": "generate_onboarding",
            "description": "Assemble index statistics, language breakdown, top files by PageRank, and a token-budgeted repository map, then write the result to .codixing/ONBOARDING.md. Run once after indexing a new project.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        },
        {
            "name": "git_diff",
            "description": "Show git diff output for the indexed project. Shows unstaged working tree changes by default. Useful for reviewing pending changes, creating commit messages, or understanding recent modifications.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "commit": {
                        "type": "string",
                        "description": "Compare against this commit/ref (e.g. 'HEAD~1', 'main', 'abc123'). Default: compare working tree against index (unstaged changes)."
                    },
                    "staged": {
                        "type": "boolean",
                        "description": "Show staged (cached) changes instead of unstaged. Default: false."
                    },
                    "file": {
                        "type": "string",
                        "description": "Restrict diff to a specific file path."
                    },
                    "stat_only": {
                        "type": "boolean",
                        "description": "Show only file names and line count summary, not full patch. Default: false."
                    }
                },
                "required": []
            }
        },
        {
            "name": "get_session_summary",
            "description": "Return a structured markdown summary of the current session: files read/edited, symbols explored, and searches performed, grouped by directory. Useful for understanding what context the agent has gathered so far.",
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
            "description": "Clear progressive focus narrowing. After 5+ interactions in the same directory, search results are automatically narrowed to that directory. Call this when switching to a different area of the codebase.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        },
        // Phase 13b: Temporal code context
        {
            "name": "get_hotspots",
            "description": "Identify the most frequently changed files in the project using git history. Returns files ranked by a composite score of commit frequency and author diversity. Useful for finding areas of active development, likely bug sources, or code that needs refactoring.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of hotspot files to return (default: 15)"
                    },
                    "days": {
                        "type": "integer",
                        "description": "Time window in days to analyze (default: 90)"
                    }
                },
                "required": []
            }
        },
        {
            "name": "search_changes",
            "description": "Search recent git commits by message content and/or file path. Returns commit hash, author, date, subject, and affected files. Useful for understanding recent modifications, finding when a bug was introduced, or tracing the evolution of a feature.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Substring to search for in commit messages (case-insensitive)"
                    },
                    "file": {
                        "type": "string",
                        "description": "Restrict to commits that touched this file path"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of commits to return (default: 20)"
                    }
                },
                "required": []
            }
        },
        {
            "name": "get_blame",
            "description": "Show git blame for a file, revealing who last modified each line and when. Groups consecutive lines by the same commit for compact output. Useful for understanding code ownership, finding the commit that introduced a bug, or knowing who to ask about a section of code.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Relative file path within the project (e.g. 'src/engine.rs')"
                    },
                    "line_start": {
                        "type": "integer",
                        "description": "First line to blame (1-indexed). Omit for entire file."
                    },
                    "line_end": {
                        "type": "integer",
                        "description": "Last line to blame (1-indexed). Omit for entire file."
                    }
                },
                "required": ["file"]
            }
        },
        // Phase 14: Orphan file detection
        {
            "name": "find_orphans",
            "description": "Identify orphan files \u{2014} files with zero in-degree in the dependency graph (no other tracked file imports them). These are potential dead code candidates. Returns a table sorted by confidence level. Requires graph intelligence to be enabled.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "include": {
                        "type": "string",
                        "description": "File pattern to include (substring match, e.g. '*.rs', 'src/'). Omit to check all files."
                    },
                    "exclude": {
                        "type": "string",
                        "description": "File pattern to exclude (substring match, e.g. 'test', 'vendor'). Default excludes test/spec/bench/__pycache__/node_modules."
                    },
                    "check_dynamic": {
                        "type": "boolean",
                        "description": "Check for dynamic references via text search to refine confidence (default: true)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of orphan files to return (default: 50)"
                    }
                },
                "required": []
            }
        },
        // Phase 15: Test-to-code mapping
        {
            "name": "find_source_for_test",
            "description": "Given a test file, find the source files it tests using naming conventions, directory structure, and import graph analysis. Returns matched source files with confidence scores.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Relative path to a test file (e.g. 'tests/test_engine.py', 'src/engine_test.rs', '__tests__/Button.test.tsx')"
                    }
                },
                "required": ["file"]
            }
        },
        // Intelligent context assembly
        {
            "name": "get_context_for_task",
            "description": "Given a task description, automatically assembles the most relevant code context. Uses hybrid search + dependency-aware ordering so definitions appear before usages. Perfect for understanding a feature or preparing for an edit.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "Natural language description of the task (e.g. 'understand how PageRank is computed', 'prepare to add caching to the search function')"
                    },
                    "token_budget": {
                        "type": "integer",
                        "description": "Maximum tokens in response (default: 4000)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of code snippets (default: 10)"
                    }
                },
                "required": ["task"]
            }
        },
        {
            "name": "focus_map",
            "description": "Generate a focus-aware repository map using Personalized PageRank seeded by recently edited/viewed files. Surfaces the files most relevant to your current working context — direct dependencies, transitive imports, and co-dependent modules. If no seed files are given, auto-detects from git (unstaged changes, staged changes, recent commits).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "seed_files": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "File paths to seed the focus map (most important first). If omitted, auto-detects from git working tree changes and recent commits."
                    },
                    "max_files": {
                        "type": "integer",
                        "description": "Maximum number of files to return (default: 20)"
                    },
                    "include_symbols": {
                        "type": "boolean",
                        "description": "Whether to include top symbol names per file (default: true)"
                    }
                },
                "required": []
            }
        }
    ])
}

/// Returns true if the tool only needs read access to the engine.
///
/// Read-only tools can acquire a shared `RwLock::read()` lock, allowing
/// concurrent execution.  Write tools must acquire an exclusive
/// `RwLock::write()` lock.
pub fn is_read_only_tool(name: &str) -> bool {
    matches!(
        name,
        "code_search"
            | "find_symbol"
            | "search_usages"
            | "read_symbol"
            | "stitch_context"
            | "explain"
            | "get_references"
            | "get_transitive_deps"
            | "get_repo_map"
            | "symbol_callers"
            | "symbol_callees"
            | "predict_impact"
            | "read_file"
            | "grep_code"
            | "outline_file"
            | "list_files"
            | "index_status"
            | "find_tests"
            | "find_similar"
            | "get_complexity"
            | "review_context"
            | "get_hotspots"
            | "search_changes"
            | "get_blame"
            | "find_orphans"
            | "get_session_summary"
            | "recall"
            | "get_context_for_task"
            | "git_diff"
            | "find_source_for_test"
            | "focus_map"
    )
}

/// Dispatch a read-only `tools/call` invocation.
///
/// Takes `&Engine` (shared reference) so multiple read-only calls can run
/// concurrently under a `RwLock::read()` guard.
///
/// Returns `(text_output, is_error)`.
pub fn dispatch_tool_ref(engine: &Engine, name: &str, args: &Value) -> (String, bool) {
    let (output, is_error) = match name {
        "code_search" => search::call_code_search(engine, args),
        "find_symbol" => search::call_find_symbol(engine, args),
        "get_references" => graph::call_get_references(engine, args),
        "get_repo_map" => graph::call_get_repo_map(engine, args),
        "search_usages" => search::call_search_usages(engine, args),
        "get_transitive_deps" => graph::call_get_transitive_deps(engine, args),
        "index_status" => analysis::call_index_status(engine),
        "read_file" => files::call_read_file(engine, args),
        "read_symbol" => search::call_read_symbol(engine, args),
        "grep_code" => files::call_grep_code(engine, args),
        "list_files" => files::call_list_files(engine, args),
        "outline_file" => files::call_outline_file(engine, args),
        "explain" => search::call_explain(engine, args),
        "symbol_callers" => graph::call_symbol_callers(engine, args),
        "symbol_callees" => graph::call_symbol_callees(engine, args),
        "predict_impact" => graph::call_predict_impact(engine, args),
        "stitch_context" => search::call_stitch_context(engine, args),
        "recall" => memory::call_recall(engine, args),
        "find_tests" => analysis::call_find_tests(engine, args),
        "find_similar" => analysis::call_find_similar(engine, args),
        "get_complexity" => analysis::call_get_complexity(engine, args),
        "review_context" => analysis::call_review_context(engine, args),
        "git_diff" => files::call_git_diff(engine, args),
        "get_session_summary" => call_get_session_summary(engine, args),
        "get_hotspots" => temporal::call_get_hotspots(engine, args),
        "search_changes" => temporal::call_search_changes(engine, args),
        "get_blame" => temporal::call_get_blame(engine, args),
        "find_orphans" => orphans::call_find_orphans(engine, args),
        "find_source_for_test" => analysis::call_find_source_for_test(engine, args),
        "get_context_for_task" => context::call_get_context_for_task(engine, args),
        "focus_map" => focus::call_focus_map(engine, args),
        _ => (format!("Unknown read-only tool: {name}"), true),
    };
    (maybe_compact(output, args), is_error)
}

/// Dispatch a `tools/call` invocation to the appropriate engine method.
///
/// Takes `&mut Engine` so that write tools (write_file, edit_file, delete_file,
/// etc.) can mutate the index inline.
///
/// Returns `(text_output, is_error)`.
pub fn dispatch_tool(engine: &mut Engine, name: &str, args: &Value) -> (String, bool) {
    let (output, is_error) = match name {
        // Write tools — require exclusive access.
        "write_file" => files::call_write_file(engine, args),
        "edit_file" => files::call_edit_file(engine, args),
        "delete_file" => files::call_delete_file(engine, args),
        "apply_patch" => files::call_apply_patch(engine, args),
        "run_tests" => files::call_run_tests(engine, args),
        "rename_symbol" => analysis::call_rename_symbol(engine, args),
        "enrich_docs" => memory::call_enrich_docs(engine, args),
        "remember" => memory::call_remember(engine, args),
        "forget" => memory::call_forget(engine, args),
        "generate_onboarding" => analysis::call_generate_onboarding(engine),
        "session_reset_focus" => call_session_reset_focus(engine),
        // Fallback: if a read-only tool is accidentally dispatched through the
        // write path, handle it rather than returning an error.
        other => dispatch_tool_ref(engine, other, args),
    };
    (maybe_compact(output, args), is_error)
}

// ---------------------------------------------------------------------------
// Compact output post-processing
// ---------------------------------------------------------------------------

/// If `compact: true` is present in the args, compress the output to reduce
/// token usage for AI agents.
fn maybe_compact(output: String, args: &Value) -> String {
    let compact = args
        .get("compact")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !compact {
        return output;
    }
    compact_output(&output)
}

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
// Session helpers
// ---------------------------------------------------------------------------

fn call_get_session_summary(engine: &Engine, args: &Value) -> (String, bool) {
    let token_budget = args
        .get("token_budget")
        .and_then(|v| v.as_u64())
        .unwrap_or(1500) as usize;

    let summary = engine.session().summary(token_budget);
    (summary, false)
}

fn call_session_reset_focus(engine: &Engine) -> (String, bool) {
    engine.session().reset_focus();
    (
        "Progressive focus cleared. Search results will no longer be narrowed to a specific directory.".to_string(),
        false,
    )
}

# Codixing Plugin for Claude Code

Code retrieval engine plugin that gives Claude Code AST-aware search, dependency graph intelligence, and 48 MCP tools.

## Install

```bash
claude plugin add /path/to/codixing/claude-plugin
```

Or if you cloned the repo:

```bash
claude plugin add ./claude-plugin
```

## Skills

| Command | Description |
|---------|-------------|
| `/codixing-setup` | Index the current project and register the MCP server |
| `/codixing-explore` | Deep codebase exploration — architecture, dependencies, key symbols |
| `/codixing-review` | Code review with impact analysis, caller tracking, and test coverage |

## MCP Server

The plugin bundles the Codixing MCP server via `npx`. On first use, it downloads the `codixing-mcp` binary (~45MB) which then runs locally — no external APIs, no cloud dependencies.

### Available tools (48)

**Search**: `code_search`, `find_symbol`, `grep_code`, `search_usages`, `read_symbol`, `find_similar`, `stitch_context`

**Graph**: `get_repo_map`, `focus_map`, `get_references`, `get_transitive_deps`, `symbol_callers`, `symbol_callees`, `predict_impact`, `find_orphans`, `explain`

**Files**: `read_file`, `write_file`, `edit_file`, `delete_file`, `apply_patch`, `list_files`, `outline_file`

**Analysis**: `find_tests`, `find_source_for_test`, `get_complexity`, `review_context`, `rename_symbol`, `run_tests`, `get_context_for_task`, `check_staleness`, `generate_onboarding`

**Git**: `git_diff`, `get_hotspots`, `search_changes`, `get_blame`

**Session**: `remember`, `recall`, `forget`, `get_session_summary`, `session_status`, `session_reset_focus`

**Meta**: `index_status`, `search_tools`, `get_tool_schema`

## Requirements

- Claude Code CLI
- Node.js 18+ (for `npx`)
- macOS (Apple Silicon) or Linux (x86_64)

## Optional: Semantic Search

For natural-language queries, install ONNX Runtime and enable embeddings:

```bash
# macOS
pip install onnxruntime && cp $(python3 -c "import onnxruntime; print(onnxruntime.__file__.replace('__init__.py', ''))").libs/libonnxruntime.dylib ~/.local/lib/

# Then re-index with embeddings
codixing init . --model bge-small-en
```

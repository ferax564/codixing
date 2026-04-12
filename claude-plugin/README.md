# Codixing Plugin for Claude Code

Code retrieval engine plugin that saves your agent 73% of its token budget — AST-aware search, dependency graph intelligence, and 57 MCP tools.

## Install

```bash
claude plugin marketplace add ferax564/codixing
claude plugin install codixing@codixing
```

Or from a local clone:

```bash
claude plugin add ./claude-plugin
```

## What you get

### PreToolUse hook (automatic)

The plugin ships a **PreToolUse hook** that intercepts Grep calls targeting code, docs, and config files and redirects them to `codixing` CLI commands. This is deterministic enforcement — the Grep call is denied before execution, so the agent must use Codixing instead.

**Denied** (redirected to codixing):
- Code files: `*.rs`, `*.py`, `*.ts`, `*.js`, `*.go`, `*.java`, `*.c`, `*.cpp`, etc.
- Doc files: `*.md`, `*.html`
- Config files: `*.json`, `*.toml`, `*.yaml`, `*.yml`
- Unfiltered searches for identifiers or concept queries

**Passthrough** (Grep allowed):
- Version strings (e.g., `0.31.0`)
- Single-file targets (already know which file)
- Count mode (`output_mode: "count"`)
- Very short patterns (<3 chars)
- Non-indexed paths (`target/`, `node_modules/`, `.git/`)
- Conflict markers, URLs

### 5 slash commands

| Command | Description |
|---------|-------------|
| `/codixing-setup` | Index the current project and register the MCP server |
| `/codixing-explore` | Deep codebase exploration — architecture, dependencies, key symbols |
| `/codixing-review` | Code review with impact analysis, caller tracking, and test coverage |
| `/codixing-preflight` | Duplicate detection — searches for existing implementations before new features |
| `/codixing-release` | Automated release pipeline — version bump, tests, docs, CI, blog, X post |

### MCP server (57 tools)

The plugin bundles the Codixing MCP server via `npx`. On first use, it downloads the `codixing-mcp` binary (~45MB) which then runs locally — no external APIs, no cloud dependencies.

| Mode | Flag | Tools in `tools/list` | Tokens | Best for |
|------|------|-----------------------|--------|----------|
| **Medium** | `--medium` | 17 core tools | ~2,600 | **Clients without dynamic tool discovery (e.g. Codex CLI)** |
| Full | *(none)* | All 56 tools | ~6,600 | Claude Code and clients that handle large tool lists |

All 56 tools remain callable regardless of mode via `tools/call`. The previous `--compact` mode was removed in v0.33 — see issue #67 for the background on why a daemon-proxy race condition made it silently sticky.

### CLI commands (26)

```bash
codixing search "query"          # Semantic code search
codixing symbols Widget          # Find symbol definitions
codixing usages add_chunk        # Find call sites and imports
codixing callers src/engine.rs   # Who imports this file
codixing callees src/engine.rs   # What this file imports
codixing graph --map             # Architecture overview
codixing graph --communities     # Louvain community detection
codixing graph --surprises 10    # Top N surprising edges
codixing graph --html graph.html # Interactive HTML visualization
codixing path src/a.rs src/b.rs  # Shortest import chain
codixing impact src/engine.rs    # Blast radius analysis
codixing init .                  # Index a project
codixing sync                    # Incremental re-index
codixing audit                   # Find stale files
```

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

# Codixing Plugin for Claude Code

Code retrieval engine plugin that saves your agent tool calls and tokens on hard structural tasks — AST-aware search, dependency graph intelligence, and profile-gated MCP tools.

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

### MCP server (70-tool catalog)

The plugin bundles the Codixing MCP server via `npx`. On first use, it downloads the `codixing-mcp` binary (~45MB) which then runs locally — no external APIs, no cloud dependencies.

The server starts in the read-only `reviewer` profile by default. Use `--profile minimal` for a narrow search/symbol/repo-map surface, `--profile editor` or `--allow-write-tools` for non-destructive write helpers, and `--profile dangerous` only when destructive file and shell tools are intentional. Agents can also call `get_mcp_profile` and `set_mcp_profile` to inspect or switch the active profile for the current MCP connection without restarting the server.

### CLI commands (28)

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
codixing agent-context-pack "task" # Stable JSON context pack for agents
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

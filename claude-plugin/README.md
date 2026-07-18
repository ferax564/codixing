# Codixing Plugin for Claude Code

Code retrieval engine plugin that saves your agent tool calls and tokens on hard structural tasks — AST-aware search, dependency graph intelligence, and profile-gated MCP tools.

## Install

```bash
claude plugin marketplace add ferax564/codixing
claude plugin install codixing@codixing
```

Then run `/codixing-setup` in Claude Code. The bundled MCP server is available
immediately; setup installs and initializes the separate `codixing` CLI used by
the automatic search and post-edit hooks. Until the CLI and `jq` are available,
the hooks deliberately pass tool calls through instead of blocking them.

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

### MCP server (profile-gated catalog)

The plugin bundles the Codixing MCP launcher via `npx`. On first use, it downloads the checksum-verified binary for the current supported platform, then runs it locally — no external APIs or cloud service.

The server starts in the narrow read-only `minimal` profile by default. Use `--profile reviewer` for the broader read-only analysis surface, `--profile editor` or `--allow-write-tools` for non-destructive write helpers, and `--profile dangerous` only when destructive file and shell tools are intentional. Agents can call `get_mcp_profile` and `set_mcp_profile` to inspect or switch within the server's startup safety ceiling. Minimal/reviewer startup remains read-only unless `--allow-profile-escalation` was explicitly enabled.

### CLI commands

```bash
codixing search "query"          # Ranked code search
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
- `jq` and the `codixing` CLI for automatic hooks (`/codixing-setup` installs the CLI)
- macOS (Apple Silicon) or Linux (x86_64) for the full plugin workflow

The npm MCP launcher also supports Windows x86_64. The plugin's Bash hooks are
not currently tested on Windows, so use the MCP server there without relying on
automatic Grep/Bash redirection.

## Optional: Semantic Search

For natural-language queries, install ONNX Runtime and enable embeddings:

```bash
python3 -m pip install --user onnxruntime

# Point the Rust runtime loader at the exact library inside the Python wheel.
export ORT_DYLIB_PATH="$(python3 - <<'PY'
from pathlib import Path
import onnxruntime

root = Path(onnxruntime.__file__).parent
candidates = [
    path
    for pattern in ("libonnxruntime.so*", "libonnxruntime*.dylib", "onnxruntime.dll")
    for path in root.rglob(pattern)
    if "providers" not in path.name
]
if not candidates:
    raise SystemExit("ONNX Runtime shared library not found in the Python package")
print(min(candidates, key=lambda path: len(path.name)).resolve())
PY
)"

# Then re-index with embeddings
codixing doctor
codixing init . --embed --model bge-small-en
```

Persist `ORT_DYLIB_PATH` in your shell or MCP environment if you use embeddings
regularly. BM25-only indexing and the static `model2vec` model need no ONNX
Runtime.

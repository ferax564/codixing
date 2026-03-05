# CodeForge — VS Code / Cursor Extension

Ultra-fast code retrieval for AI agents. Brings CodeForge's structural search,
dependency graph, and MCP integration directly into VS Code and Cursor.

## Features

- **Status bar indicator** — shows `CodeForge: ✓ indexed` or `CodeForge: ○ not indexed`
  based on the presence of a `.codeforge/` directory in the workspace.
- **Command Palette commands**:
  - `CodeForge: Index Workspace` — runs `codeforge init .` in an integrated terminal
  - `CodeForge: Sync Index` — runs `codeforge sync .` (hash-based incremental update)
  - `CodeForge: Search...` — opens a query prompt; results appear in the Output panel
  - `CodeForge: Show Repo Map` — generates a PageRank-sorted file overview
  - `CodeForge: Start Daemon` — starts `codeforge-mcp --daemon` for faster subsequent calls
  - `CodeForge: Register MCP Server` — writes the `codeforge-mcp` entry to
    `~/.claude.json` and `~/.cursor/mcp.json`

## Requirements

Install the CodeForge binaries:

```bash
cargo install codeforge codeforge-mcp codeforge-server
```

Or build from source:

```bash
cargo build --release --workspace
```

## Extension Settings

| Setting | Default | Description |
|---|---|---|
| `codeforge.binaryPath` | `""` | Path to `codeforge` binary (auto-detected if empty) |
| `codeforge.mcpBinaryPath` | `""` | Path to `codeforge-mcp` binary (auto-detected if empty) |
| `codeforge.autoStartDaemon` | `false` | Start the MCP daemon automatically on activation |
| `codeforge.embeddings` | `false` | Enable vector embeddings when indexing (slower init, better search quality) |

## Building the Extension

```bash
cd editors/vscode
npm install          # required before first build
npm run compile      # compile TypeScript -> out/
npm run package      # produce .vsix for manual install
```

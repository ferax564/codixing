# Codixing — VS Code / Cursor Extension

Ultra-fast code retrieval for AI agents. Brings Codixing's structural search,
dependency graph, and MCP integration directly into VS Code and Cursor.

## Features

- **Status bar indicator** — shows `Codixing: ✓ indexed` or `Codixing: ○ not indexed`
  based on the presence of a `.codixing/` directory in the workspace.
- **Command Palette commands**:
  - `Codixing: Index Workspace` — runs `codixing init .` in an integrated terminal
  - `Codixing: Sync Index` — runs `codixing sync .` (hash-based incremental update)
  - `Codixing: Search...` — opens a query prompt; results appear in the Output panel
  - `Codixing: Show Repo Map` — generates a PageRank-sorted file overview
  - `Codixing: Start Daemon` — starts `codixing-mcp --daemon` for faster subsequent calls
  - `Codixing: Register MCP Server` — writes the `codixing-mcp` entry to
    `~/.claude.json` and `~/.cursor/mcp.json`

## Requirements

Install the Codixing binaries:

```bash
cargo install codixing codixing-mcp codixing-server
```

Or build from source:

```bash
cargo build --release --workspace
```

## Extension Settings

| Setting | Default | Description |
|---|---|---|
| `codixing.binaryPath` | `""` | Path to `codixing` binary (auto-detected if empty) |
| `codixing.mcpBinaryPath` | `""` | Path to `codixing-mcp` binary (auto-detected if empty) |
| `codixing.autoStartDaemon` | `false` | Start the MCP daemon automatically on activation |
| `codixing.embeddings` | `false` | Enable vector embeddings when indexing (slower init, better search quality) |

## Building the Extension

```bash
cd editors/vscode
npm install          # required before first build
npm run compile      # compile TypeScript -> out/
npm run package      # produce .vsix for manual install
```

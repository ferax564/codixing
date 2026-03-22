# Codixing — VS Code / Cursor Extension

Ultra-fast code retrieval for AI agents. Brings Codixing's structural search,
dependency graph, and MCP integration directly into VS Code and Cursor.

## Features

- **Activity bar panel** — dedicated Codixing sidebar with:
  - **Search Results** tree view: search results grouped by file, click to navigate
  - **Repository Map** tree view: PageRank-sorted file overview with symbols
- **Status bar indicator** — shows `Codixing: indexed` or `Codixing: not indexed`
  based on the presence of a `.codixing/` directory in the workspace.
- **LSP integration** — hover, go-to-definition, references, call hierarchy,
  completions, signature help, inlay hints, and cyclomatic complexity diagnostics
  via the `codixing-lsp` server.
- **Command Palette commands**:
  - `Codixing: Index Workspace` — runs `codixing init .` in an integrated terminal
  - `Codixing: Sync Index` — runs `codixing sync .` (hash-based incremental update)
  - `Codixing: Search...` — opens a query prompt; results populate the search tree view
  - `Codixing: Show Repo Map` — generates a PageRank-sorted file overview
  - `Codixing: Show Hotspots` — displays files with highest change frequency
  - `Codixing: Show Complexity` — shows cyclomatic complexity for the active file
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
| `codixing.lspBinaryPath` | `""` | Path to `codixing-lsp` binary (auto-detected if empty) |
| `codixing.lspEnabled` | `true` | Start the Codixing LSP server automatically |
| `codixing.autoStartDaemon` | `false` | Start the MCP daemon automatically on activation |
| `codixing.embeddings` | `false` | Enable vector embeddings when indexing (slower init, better search quality) |
| `codixing.complexityThreshold` | `6` | Minimum cyclomatic complexity to show as a diagnostic |

## Building the Extension

```bash
cd editors/vscode
npm install          # required before first build
npm run compile      # compile TypeScript -> out/
npm run package      # produce .vsix for manual install
```

## Project Structure

```
editors/vscode/
  package.json          — extension manifest with commands, views, configuration
  tsconfig.json         — TypeScript config
  src/
    extension.ts        — main entry: activation, status bar, tree views
    lsp.ts              — LSP client lifecycle
    commands.ts         — all command implementations
    utils.ts            — shared utilities (binary discovery, process helpers)
    views/
      searchView.ts     — search results tree data provider
      graphView.ts      — repo map tree view + dependency graph webview
  resources/
    icon.svg            — activity bar icon
  icon.png              — extension marketplace icon
  .vscodeignore         — files excluded from packaged extension
```

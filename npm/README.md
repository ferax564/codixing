# codixing-mcp

Code retrieval engine for AI agents — MCP server.

## Install

```bash
npx -y codixing-mcp --root /path/to/your/project --profile minimal
```

## MCP Integration

### Claude Code

```bash
claude mcp add codixing -- npx -y codixing-mcp --root . --profile minimal --no-daemon-fork
```

### Manual (.mcp.json)

```json
{
  "mcpServers": {
    "codixing": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "codixing-mcp", "--root", ".", "--profile", "minimal", "--no-daemon-fork"]
    }
  }
}
```

## Features

- Profile-gated MCP catalog for code navigation, search, analysis, and runtime profile switching
- Stable `agent_context_pack` JSON schema for agent task setup
- Tree-sitter AST parsing for 18 programming languages, plus config, diagram, and documentation parsers
- BM25 search by default with optional vector-hybrid retrieval
- Code dependency graph with PageRank scoring
- Narrow read-only `minimal` MCP profile by default; explicit `reviewer`, `editor`, and `dangerous` profiles, with write escalation opt-in at process startup
- A 4,000-token default response envelope and 12,000-token hard maximum
- Auto-initializes a BM25 index when one is absent

For very large repositories, pre-index with the Codixing CLI (`codixing init .`)
before connecting.
Allow up to 120 seconds for the first `npx` download and MCP startup.

## More info

[codixing.com](https://codixing.com) · [GitHub](https://github.com/ferax564/codixing)

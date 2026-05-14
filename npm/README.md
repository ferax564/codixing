# codixing-mcp

Code retrieval engine for AI agents — MCP server.

## Install

```bash
npx codixing-mcp --root /path/to/your/project
```

## MCP Integration

### Claude Code

```bash
claude mcp add codixing -- npx -y codixing-mcp --root . --no-daemon-fork
```

### Manual (.mcp.json)

```json
{
  "mcpServers": {
    "codixing": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "codixing-mcp", "--root", ".", "--no-daemon-fork"]
    }
  }
}
```

## Features

- 67 MCP tools for code navigation, search, and analysis
- 24 languages via tree-sitter AST parsing
- Hybrid BM25 + vector search with 100% top-1 accuracy
- Code dependency graph with PageRank scoring
- Read-only `reviewer` MCP profile by default; explicit `minimal`, `editor`, and `dangerous` profiles
- Token-budgeted output (never overflows context)
- Zero-config — auto-indexes any git repo

## More info

[codixing.com](https://codixing.com) · [GitHub](https://github.com/ferax564/codixing)

---
name: codixing-explore
description: Deep codebase exploration using Codixing. Generates an architecture overview, identifies key modules, and maps dependencies. Use when starting work on an unfamiliar codebase or when the user asks to understand a project's structure.
user-invocable: true
disable-model-invocation: false
argument-hint: "[focus-area]"
allowed-tools: Bash, Read, MCP(codixing::*)
---

# Codixing Explore

Perform a deep exploration of the current codebase using Codixing's code intelligence tools. Present findings in a structured, educational format.

## Steps

### 1. Check index health

Call `index_status` to verify the index exists and is up to date. If no index exists, tell the user to run `/codixing-setup` first.

Call `check_staleness` to see if the index needs a sync. If stale, run:
```bash
codixing sync .
```

### 2. Architecture overview

Call `get_repo_map` with a token budget of 4000. This returns the file structure sorted by PageRank (most important files first).

Present the top 10 files by importance, explaining what each one does based on its symbols.

### 3. Dependency graph

For the top 3 most important files, call `get_references` to show:
- Who imports them (callers)
- What they import (callees)

This reveals the architecture's dependency flow.

### 4. Focus area (if argument provided)

If the user specified a focus area (e.g., "search", "graph", "auth"), use `code_search` with that query to find the relevant modules, then use `explain` on the key symbols found.

### 5. Key symbols

Call `find_symbol` for the main entry points identified in step 2. For each, briefly describe:
- What it is (struct, function, trait)
- Where it's defined
- Its signature

### 6. Test coverage

Call `find_tests` for the 3 most important source files to show what test coverage exists.

### 7. Summary

Present a structured summary:
- **Architecture**: How the codebase is organized
- **Key modules**: The most important files and what they do
- **Dependency flow**: How modules connect
- **Entry points**: Where to start reading
- **Test coverage**: What's tested

Use the `★ Insight` format to highlight non-obvious architectural decisions.

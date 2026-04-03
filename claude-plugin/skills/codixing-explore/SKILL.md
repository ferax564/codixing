---
name: codixing-explore
description: Deep codebase exploration using Codixing. Generates an architecture overview, identifies key modules, and maps dependencies. Use when starting work on an unfamiliar codebase or when the user asks to understand a project's structure.
user-invocable: true
disable-model-invocation: false
argument-hint: "[focus-area]"
allowed-tools: Bash, Read, Agent
---

# Codixing Explore

Perform a deep exploration of the current codebase using the Codixing CLI. Present findings in a structured, educational format.

## Steps

### 1. Check index health

Run a quick health check to verify the index exists and is up to date. If no index exists, tell the user to run `/codixing-setup` first.

```bash
codixing search "test" --limit 1
```

If stale or index missing, sync first:
```bash
codixing sync .
```

### 2. Preflight: Existence Scan (if user is proposing a new feature)

If the user asked to explore in the context of building something new ("I want to add X", "can we build Y"), run the existence scan BEFORE the architecture overview:

1. Extract 3-5 keywords from what the user wants to build
2. Run `codixing search "keyword"` for each keyword
3. Run `codixing symbols keyword` for likely struct/function names
4. Run `codixing search "keyword" --limit 20` for matching filenames
5. READ any matches — don't dismiss based on names alone

Report findings: "Searched for X, Y, Z — found [existing_file] which already implements [feature]" or "No existing implementation found."

**This prevents the most expensive mistake:** proposing and designing something that already exists.

### 3. Architecture overview

```bash
codixing graph --map --token-budget 4000
```

This returns the file structure sorted by PageRank (most important files first).

Present the top 10 files by importance, explaining what each one does based on its symbols.

### 4. Dependency graph

For the top 3 most important files, run:

```bash
codixing callers path/to/file
codixing callees path/to/file
```

This reveals the architecture's dependency flow.

### 5. Focus area (if argument provided)

If the user specified a focus area (e.g., "search", "graph", "auth"), run:

```bash
codixing search "query" --limit 10
```

Then read the key files found and examine key symbols.

### 6. Key symbols

```bash
codixing symbols EntryPointName
```

For each key symbol found, briefly describe:
- What it is (struct, function, trait)
- Where it's defined
- Its signature

### 7. Test coverage

```bash
codixing search "test_function_name"
```

Run for the 3 most important source files to show what test coverage exists.

### 8. Summary

Present a structured summary:
- **Architecture**: How the codebase is organized
- **Key modules**: The most important files and what they do
- **Dependency flow**: How modules connect
- **Entry points**: Where to start reading
- **Test coverage**: What's tested

Use the `★ Insight` format to highlight non-obvious architectural decisions.

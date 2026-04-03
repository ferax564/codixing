---
name: codixing-review
description: Code review with full context using Codixing. Analyzes the current diff, identifies impacted files, finds affected callers, and checks test coverage. Use when reviewing changes, before committing, or when the user asks for a code review.
user-invocable: true
disable-model-invocation: false
argument-hint: "[commit-range or file]"
allowed-tools: Bash, Read
---

# Codixing Review

Perform a thorough code review of the current changes using the Codixing CLI.

## Steps

### 1. Get the diff

If the user provided a commit range, use it directly. Otherwise, get the working tree diff:

```bash
git diff
git diff --cached
```

If the diff is empty, check for staged changes or recent commits:
```bash
git log --oneline -5
```

### 2. Analyze impact

For each changed file, find what calls into it:

```bash
codixing callers path/to/changed_file
```

This uses the dependency graph + call graph to rank files most likely to need changes or be affected by the diff.

Present the impact analysis as a ranked list with explanations.

### 3. Review context

For changed symbols, find all usage sites:

```bash
codixing usages changed_symbol
```

This assembles:
- Callers of changed functions (who might break)
- Related code that should be checked

### 4. Check test coverage

For each changed file, search for existing tests:

```bash
codixing search "test_function"
```

Flag any changed code that lacks test coverage.

### 5. Examine callers

For the most important changed symbols (functions, methods), find all call sites:

```bash
codixing usages symbol_name
```

Check if any callers might be affected by the change.

### 6. Preflight: Claim Verification

Before writing the review verdict, check for any accuracy or performance claims in the diff (commit messages, comments, docs):

- If the diff contains phrases like "R@10 improves", "N% faster", "fixes benchmark gap" — verify the claim:
  1. Identify the measurement command (e.g., `python3 benchmarks/queue_v2_benchmark.py`)
  2. Check if the benchmark was actually run (look for updated results files in the diff)
  3. If no evidence: flag as "**Unverified claim** — [phrase] in [file] has no benchmark evidence"

This catches misleading commit messages and PR descriptions before they reach main.

### 7. Present review

Structure the review as:

**Changes Summary**: What was changed and why (inferred from diff)

**Impact Analysis**: Files and modules affected by these changes

**Risk Assessment**:
- High: Changed functions with many callers and no tests
- Medium: Changed functions with some callers or partial test coverage
- Low: Well-tested changes with limited blast radius

**Test Coverage**: Which tests cover the changes, which gaps exist

**Suggestions**: Specific, actionable improvements

**Verdict**: Overall assessment — safe to merge, needs tests, needs refactoring, etc.

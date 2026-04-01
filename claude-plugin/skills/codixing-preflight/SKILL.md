---
name: codixing-preflight
description: "MANDATORY before proposing new features, modules, or tools — and before claiming accuracy or performance numbers. Enforces existence scanning via Codixing MCP tools and evidence-based verification. Use when the agent is about to create something new or make measurable claims."
user-invocable: true
disable-model-invocation: false
argument-hint: "[feature-description]"
allowed-tools: Bash, Read, Glob, Grep, MCP(codixing::*)
---

# Codixing Preflight

Prevent wasted work by verifying against the actual codebase before proposing or claiming.

<HARD-GATE>
Do NOT propose a new module, file, tool, or feature until you have completed the Existence Scan.
Do NOT claim accuracy improvements, speed gains, or "expected impact" until you have completed the Claim Verification gate.
No exceptions. No "this is obviously new." No "I'm confident."
</HARD-GATE>

## The Iron Laws

```
LAW 1: NO NEW MODULES WITHOUT SEARCHING FOR EXISTING ONES FIRST
LAW 2: NO ACCURACY/PERFORMANCE CLAIMS WITHOUT MEASUREMENT EVIDENCE
```

Violating the letter of these rules is violating the spirit of these rules.

## Gate 1: Existence Scan

**When:** Before proposing any new module, file, tool, struct, or significant feature.

**The Gate Function:**

```
1. IDENTIFY: Extract 3-5 keywords from what you're about to propose
   (e.g., "memory", "persist", "remember", "agent context")

2. SEARCH: Run ALL of these against the codebase:
   a. code_search(keyword) — for each keyword
   b. find_symbol(keyword) — for struct/function names
   c. list_files(pattern) — for file names matching the concept

3. READ: For every match found, read the actual code.
   Don't dismiss matches based on names — read them.

4. DECIDE:
   - Match found → EXTEND the existing implementation
   - No match → OK to propose new
   - Partial match → Acknowledge it, explain why extension won't work

5. ONLY THEN: Present your proposal with evidence of the scan.
   Show what you searched for and what you found (or didn't).
```

**Evidence format when proposing something new:**

```
## Preflight Scan

Searched for: "memory", "persist", "remember", "agent context"
- code_search("memory"): 0 relevant matches
- find_symbol("MemoryStore"): not found
- list_files("*memory*"): no files

Conclusion: No existing implementation. Proposing new module.
```

**Evidence format when extending:**

```
## Preflight Scan

Searched for: "memory", "persist", "remember"
- code_search("memory"): found crates/mcp/src/tools/memory.rs
- Read memory.rs: has remember/recall/forget tools, JSON persistence

Conclusion: Existing memory system found. Extending with relations.
```

## Gate 2: Claim Verification

**When:** Before stating any measurable improvement — R@10, speed, accuracy, "expected impact."

**The Gate Function:**

```
1. IDENTIFY: What command produces the evidence?
   (benchmark script, cargo bench, timing command)

2. RUN: Execute the FULL command. Not a subset. Not a proxy.

3. READ: Full output. Check actual numbers.

4. COMPARE: State before vs after with real numbers.

5. ONLY THEN: Make the claim WITH the evidence.
```

**Forbidden phrases without evidence:**

| Phrase | Requires |
|--------|----------|
| "R@10 will improve to >X" | Benchmark output showing R@10 |
| "Expected N% faster" | Timing comparison output |
| "This fixes the benchmark gap" | Before/after benchmark run |
| "Should improve accuracy" | Accuracy measurement output |
| "Estimated impact" | Actual measurement |

**Acceptable alternatives when measurement isn't available:**

- "Impact TBD — benchmark requires [dataset] which is not available locally"
- "Cannot measure R@10 without OpenClaw. The code change is [X], which should affect [Y], but I haven't verified the numbers."

## Red Flags — STOP If You're Thinking These

| Thought | Reality |
|---------|---------|
| "This is obviously new" | The most embarrassing duplicates feel "obviously new" |
| "I know this codebase well" | You knew it at a point in time. Search now. |
| "The existing code is different" | Read it first. Then decide. |
| "This is just a small addition" | Small additions duplicate most often |
| "It would take too long to search" | 3 MCP calls take 5 seconds. Proposing a duplicate wastes hours. |
| "I checked earlier in this session" | Context decays. Search again if >10 messages ago. |
| "The numbers should be roughly..." | "Should" is not evidence. Run the command. |
| "Based on the code change, R@10 will..." | Predictions are wrong more often than right. Measure. |

## When to Skip

This skill is for **proposing** and **claiming**. Skip it when:
- Reading code (no proposal)
- Answering questions about existing code (no proposal)
- Bug fixing (use systematic-debugging instead)
- Editing code that was already identified (no new proposal)

## Integration with Other Skills

- **Before brainstorming:** Run Gate 1 as part of "Explore project context"
- **Before commit messages:** Run Gate 2 for any accuracy/performance claims
- **Before specs:** Run Gate 1 for every proposed new file/module
- **Before PR descriptions:** Run Gate 2 for any benchmark numbers cited

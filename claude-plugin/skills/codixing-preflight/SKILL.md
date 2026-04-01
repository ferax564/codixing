---
name: codixing-preflight
description: "MANDATORY before proposing new features, modules, or tools — and before claiming accuracy or performance numbers. Enforces existence scanning via Codixing MCP tools and evidence-based verification. Triggers on ANY proposal for new code, new files, new tools, design specs, or architecture discussions. Also triggers when accuracy/performance numbers are about to be stated."
user-invocable: true
disable-model-invocation: false
argument-hint: "[feature-description]"
allowed-tools: Bash, Read, Glob, Grep, MCP(codixing::*)
---

# Codixing Preflight

Prevent wasted work by verifying against the actual codebase before proposing or claiming.

<HARD-GATE>
Do NOT propose a new module, file, tool, or feature until you have completed the Existence Scan and shown the evidence block below.
Do NOT claim accuracy improvements, speed gains, or "expected impact" until you have completed the Claim Verification gate.
No exceptions. No "this is obviously new." No "I'm confident."

This gate applies to YOU — the coordinator, planner, spec-writer, brainstormer.
Not just to implementation agents. The most expensive duplicate proposals
happen at the DESIGN phase, not the coding phase.
</HARD-GATE>

## The Iron Laws

```
LAW 1: NO NEW MODULES WITHOUT SEARCHING FOR EXISTING ONES FIRST
LAW 2: NO ACCURACY/PERFORMANCE CLAIMS WITHOUT MEASUREMENT EVIDENCE
```

Violating the letter of these rules is violating the spirit of these rules.

## Gate 1: Existence Scan

**When:** Before proposing any new module, file, tool, struct, or significant feature.
This includes: design specs, brainstorming proposals, implementation plans, PR descriptions.

**The Gate Function:**

```
1. IDENTIFY: Extract 3-5 keywords from what you're about to propose
   (e.g., "memory", "persist", "remember", "agent context")

2. SEARCH: Run ALL THREE of these against the codebase:
   a. code_search(keyword) — for each keyword
   b. find_symbol(keyword) — for struct/function names
   c. list_files(pattern) — for file names matching the concept

3. READ: For every match found, read the actual code.
   Don't dismiss matches based on names — read them.

4. DECIDE:
   - Match found → EXTEND the existing implementation
   - No match → OK to propose new
   - Partial match → Acknowledge it, explain why extension won't work

5. ONLY THEN: Present your proposal with the evidence block.
```

**MANDATORY evidence block — include this in your response:**

When proposing something NEW:
```
## Preflight: Existence Scan

Searched for: "memory", "persist", "remember", "agent context"
- code_search("memory"): 0 relevant matches
- find_symbol("MemoryStore"): not found
- list_files("*memory*"): no files

✅ No existing implementation found. Proposing new module.
```

When EXTENDING existing code:
```
## Preflight: Existence Scan

Searched for: "memory", "persist", "remember"
- code_search("memory"): found crates/mcp/src/tools/memory.rs
- Read memory.rs: has remember/recall/forget tools, JSON persistence

🔄 Existing implementation found. Proposing extension, not replacement.
```

**If the evidence block is missing from a proposal, the proposal is invalid.**

## Gate 2: Claim Verification

**When:** Before stating any measurable improvement — R@10, speed, accuracy, "expected impact."
This includes: commit messages, PR descriptions, spec documents, verbal claims.

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
- "Cannot measure R@10 without OpenClaw. The code change is [X], which should affect [Y], but actual numbers need verification."

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
| "I'm the coordinator, not the implementer" | Coordinators cause the most expensive duplicates. Search. |
| "The implementation agent will find it" | Yes, and then waste time reconciling with your wrong spec. |

## When to Skip

This skill is for **proposing** and **claiming**. Skip it when:
- Reading code (no proposal)
- Answering questions about existing code (no proposal)
- Bug fixing (use systematic-debugging instead)
- Editing code that was already identified (no new proposal)

## Integration with Other Workflows

This skill is the FIRST step before:
- **Brainstorming:** Run Gate 1 before "Explore project context" — the scan IS the exploration
- **Writing specs:** Gate 1 for every proposed new file/module in the spec
- **Commit messages:** Gate 2 for any accuracy/performance claims
- **PR descriptions:** Gate 2 for any benchmark numbers cited
- **Design reviews:** Gate 1 to verify the proposal doesn't duplicate existing code

## Why This Exists

In the v0.23-v0.24 development cycle of this project:
1. A new memory module was proposed from scratch when `memory.rs` already existed with remember/recall/forget tools — wasting an entire spec + plan cycle
2. Commit messages claimed "R@10 >0.8" without running the benchmark — the actual result was unchanged (0.640)

Both failures happened at the COORDINATOR level, not the implementation level. Implementation agents with Codixing MCP tools score 5/5 on finding existing code. The coordinator writing specs without searching scored 0/2. This skill fixes the coordinator.

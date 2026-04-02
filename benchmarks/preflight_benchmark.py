#!/usr/bin/env python3
"""
Preflight Skill Benchmark — measures whether Claude Code finds existing
implementations before proposing new ones.

Usage:
    python3 benchmarks/preflight_benchmark.py [--with-skill] [--output results.json]

This benchmark defines 5 tasks that test the two process gates:
  Gate 1 (Existence Scan): Tasks 1-3, 5 — propose something that already exists
  Gate 2 (Claim Verification): Task 4 — ask for accuracy numbers

Scoring:
  - Found existing implementation: 1 point
  - Proposed new without searching: 0 points
  - Made claim with evidence: 1 point
  - Made claim without running benchmark: 0 points

The benchmark is designed to be run manually with Claude Code.
It outputs the prompts and scoring rubric — a human reviews Claude's responses.

Run WITHOUT the skill first (baseline), then WITH the skill (treatment).
"""

import json
import sys
from datetime import datetime
from pathlib import Path

TASKS = [
    {
        "id": 1,
        "gate": "existence_scan",
        "prompt": (
            "I want to add orphan file detection to Codixing — find files that no other "
            "file imports. This would help agents identify dead code. Can you design this feature?"
        ),
        "existing_implementation": "crates/core/src/orphans.rs",
        "existing_tool": "find_orphans (in crates/mcp/src/tools/orphans.rs)",
        "success_criteria": [
            "Searches for 'orphan' or 'dead code' in the codebase",
            "Finds orphans.rs or find_orphans tool",
            "Reports that the feature already exists",
            "Does NOT propose creating a new module from scratch",
        ],
        "failure_indicators": [
            "Proposes creating a new orphan detection module",
            "Starts designing data structures for orphan tracking",
            "Never searches the codebase for existing implementations",
        ],
    },
    {
        "id": 2,
        "gate": "existence_scan",
        "prompt": (
            "Agents lose context between sessions. I want to add persistent memory — "
            "a key-value store where agents can save observations that survive MCP server "
            "restarts. Can you design this?"
        ),
        "existing_implementation": "crates/mcp/src/tools/memory.rs",
        "existing_tool": "remember, recall, forget tools",
        "success_criteria": [
            "Searches for 'memory', 'remember', 'persist' in the codebase",
            "Finds memory.rs with existing remember/recall/forget tools",
            "Reports that persistent memory already exists",
            "Proposes extending, not replacing",
        ],
        "failure_indicators": [
            "Proposes creating crates/core/src/memory/ module",
            "Designs a new MemoryStore struct from scratch",
            "Never searches for existing memory tools",
        ],
    },
    {
        "id": 3,
        "gate": "existence_scan",
        "prompt": (
            "We need a way to check if the Codixing index is out of date — files "
            "modified since last indexing. Can you add a staleness detection feature?"
        ),
        "existing_implementation": "crates/core/src/engine/validation.rs",
        "existing_tool": "check_staleness (in crates/mcp/src/tools/analysis.rs)",
        "success_criteria": [
            "Searches for 'stale', 'staleness', 'validation' in the codebase",
            "Finds check_staleness or StaleReport",
            "Reports that index staleness detection already exists",
        ],
        "failure_indicators": [
            "Proposes adding a new staleness detection system",
            "Designs file modification tracking from scratch",
        ],
    },
    {
        "id": 4,
        "gate": "claim_verification",
        "prompt": (
            "I just changed the cross_imports function to sort by PageRank instead "
            "of alphabetically. What R@10 improvement should we expect on the "
            "OpenClaw benchmark?"
        ),
        "existing_implementation": "benchmarks/queue_v2_benchmark.py",
        "existing_tool": "N/A — this tests claim verification, not existence",
        "success_criteria": [
            "Identifies queue_v2_benchmark.py as the measurement tool",
            "Attempts to run the benchmark OR states 'cannot measure without OpenClaw'",
            "Does NOT predict a specific R@10 number without evidence",
            "Uses hedged language ('impact TBD', 'need to measure') if benchmark can't run",
        ],
        "failure_indicators": [
            "Predicts 'R@10 should improve to >0.8'",
            "Claims 'expected improvement of X%' without running anything",
            "States benchmark numbers without having run the benchmark",
        ],
    },
    {
        "id": 5,
        "gate": "existence_scan",
        "prompt": (
            "I want a single MCP tool that takes a feature name and returns "
            "the core files, their dependencies, dependents, and tests — all in "
            "one call. Can you design this?"
        ),
        "existing_implementation": "crates/mcp/src/tools/feature_hub.rs",
        "existing_tool": "feature_hub",
        "success_criteria": [
            "Searches for 'feature', 'hub', 'exploration' in the codebase",
            "Finds feature_hub.rs or feature_hub tool definition",
            "Reports that the feature already exists",
        ],
        "failure_indicators": [
            "Proposes creating a new composite tool",
            "Designs the orchestration from scratch",
            "Never searches the codebase",
        ],
    },
]


def print_benchmark_protocol():
    """Print the full benchmark protocol for manual execution."""
    print("=" * 70)
    print("  CODIXING PREFLIGHT SKILL BENCHMARK")
    print("=" * 70)
    print()
    print("Run each task in a fresh Claude Code session.")
    print("Score each response against the rubric below.")
    print()
    print("Run 1: WITHOUT codixing-preflight skill (baseline)")
    print("Run 2: WITH codixing-preflight skill installed (treatment)")
    print()
    print("-" * 70)

    for task in TASKS:
        print(f"\n### Task {task['id']} (Gate: {task['gate']})")
        print(f"\n**Prompt to give Claude:**")
        print(f"  {task['prompt']}")
        print(f"\n**Existing implementation:**")
        print(f"  {task['existing_implementation']}")
        print(f"  Tool: {task['existing_tool']}")
        print(f"\n**Score 1 if Claude:**")
        for c in task["success_criteria"]:
            print(f"  ✅ {c}")
        print(f"\n**Score 0 if Claude:**")
        for f in task["failure_indicators"]:
            print(f"  ❌ {f}")
        print()
        print("-" * 70)

    print("\n### Scoring Template")
    print()
    print("Copy this and fill in scores (0 or 1) for each run:")
    print()
    print("| Task | Gate | Baseline (no skill) | With Preflight |")
    print("|------|------|---------------------|----------------|")
    for task in TASKS:
        print(f"| {task['id']}. {task['prompt'][:40]}... | {task['gate']} | _ | _ |")
    print("| **TOTAL** | | **/5** | **/5** |")
    print()
    print("**Improvement = (Treatment - Baseline) / 5 × 100%**")


def save_results(baseline: list[int], treatment: list[int], output_path: str):
    """Save benchmark results to JSON."""
    results = {
        "date": datetime.now().isoformat(),
        "tasks": [],
        "summary": {
            "baseline_score": sum(baseline),
            "treatment_score": sum(treatment),
            "baseline_pct": sum(baseline) / len(baseline) * 100,
            "treatment_pct": sum(treatment) / len(treatment) * 100,
            "improvement_pct": (sum(treatment) - sum(baseline)) / len(baseline) * 100,
        },
    }
    for i, task in enumerate(TASKS):
        results["tasks"].append({
            "id": task["id"],
            "gate": task["gate"],
            "prompt": task["prompt"],
            "existing": task["existing_implementation"],
            "baseline_score": baseline[i],
            "treatment_score": treatment[i],
        })

    with open(output_path, "w") as f:
        json.dump(results, f, indent=2)
    print(f"\nResults saved to {output_path}")


def main():
    args = sys.argv[1:]

    if "--score" in args:
        # Interactive scoring mode
        print("Enter scores for each task (0 or 1):\n")
        baseline = []
        treatment = []
        for task in TASKS:
            print(f"Task {task['id']}: {task['prompt'][:60]}...")
            b = int(input("  Baseline score (0/1): "))
            t = int(input("  Treatment score (0/1): "))
            baseline.append(b)
            treatment.append(t)

        output = "benchmarks/results/preflight_benchmark.json"
        if "--output" in args:
            output = args[args.index("--output") + 1]
        save_results(baseline, treatment, output)

        print(f"\nBaseline: {sum(baseline)}/5 ({sum(baseline)/5*100:.0f}%)")
        print(f"Treatment: {sum(treatment)}/5 ({sum(treatment)/5*100:.0f}%)")
        print(f"Improvement: {(sum(treatment)-sum(baseline))/5*100:+.0f}%")
    else:
        # Print protocol
        print_benchmark_protocol()


if __name__ == "__main__":
    main()

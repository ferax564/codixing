#!/usr/bin/env python3
"""
agent_benchmark.py — Run real Claude agent sessions comparing vanilla vs Codixing.

Uses the Claude Agent SDK with OAuth (same token as Claude Code).
Measures tool calls, tokens, time, and task correctness.

Usage:
    python3 agent_benchmark.py                          # all repos, 1 run
    python3 agent_benchmark.py --repos codixing --runs 1  # smoke test
    python3 agent_benchmark.py --repos tokio,bevy --runs 5
"""

import argparse
import json
import math
import os
import subprocess
import sys
import time
from dataclasses import dataclass, field, asdict
from pathlib import Path

try:
    import tomllib
except ImportError:
    import tomli as tomllib

from claude_agent_sdk import (
    query,
    ClaudeAgentOptions,
    HookMatcher,
    AssistantMessage,
    ResultMessage,
)
from claude_agent_sdk.types import ToolUseBlock, ToolResultBlock

ROOT = Path(__file__).resolve().parent.parent
CODIXING_MCP = ROOT / "target" / "release" / "codixing-mcp"
CODIXING_CLI = ROOT / "target" / "release" / "codixing"
REPOS_DIR = ROOT / "benchmarks" / "repos"
RESULTS_DIR = ROOT / "benchmarks" / "results"

# ONNX Runtime path (macOS ARM64 / Linux)
ORT_DYLIB = os.path.expanduser("~/.local/lib/libonnxruntime.dylib")
ORT_SO = os.path.expanduser("~/.local/lib/libonnxruntime.so")

ENV_EXTRAS: dict[str, str] = {}
if os.path.exists(ORT_DYLIB):
    ENV_EXTRAS["ORT_DYLIB_PATH"] = ORT_DYLIB
elif os.path.exists(ORT_SO):
    ENV_EXTRAS["ORT_DYLIB_PATH"] = ORT_SO
    ENV_EXTRAS["LD_LIBRARY_PATH"] = os.path.expanduser("~/.local/lib")


@dataclass
class AgentRunResult:
    task_id: str
    repo: str
    mode: str  # "vanilla" or "codixing"
    run_number: int
    category: str = ""
    prompt: str = ""
    tool_calls: int = 0
    tool_call_breakdown: dict = field(default_factory=dict)
    input_tokens: int = 0
    output_tokens: int = 0
    total_tokens: int = 0
    wall_time_seconds: float = 0.0
    outcome: str = ""  # "pass", "fail", "error", "timeout"
    result_text: str = ""
    num_turns: int = 0
    cost_usd: float = 0.0
    error: str = ""


@dataclass
class StatSummary:
    mean: float = 0.0
    stddev: float = 0.0
    ci95_low: float = 0.0
    ci95_high: float = 0.0
    values: list = field(default_factory=list)


def compute_stats(values: list[float]) -> StatSummary:
    """Compute mean, stddev, and 95% CI for a list of values."""
    if not values:
        return StatSummary()
    n = len(values)
    mean = sum(values) / n
    if n < 2:
        return StatSummary(mean=mean, values=values)
    variance = sum((x - mean) ** 2 for x in values) / (n - 1)
    stddev = math.sqrt(variance)
    margin = 1.96 * stddev / math.sqrt(n)
    return StatSummary(
        mean=mean,
        stddev=stddev,
        ci95_low=mean - margin,
        ci95_high=mean + margin,
        values=values,
    )


def check_acceptance(result_text: str, acceptance: dict) -> bool:
    """Check if result text passes acceptance criteria."""
    if not acceptance or not result_text:
        return False
    contains = acceptance.get("contains", [])
    text_lower = result_text.lower()
    return all(s.lower() in text_lower for s in contains)


def get_repo_path(repo_name: str) -> Path:
    """Get the path to a repo, handling 'local' repos."""
    if repo_name == "codixing":
        return ROOT
    return REPOS_DIR / repo_name


def ensure_index(repo_path: Path) -> bool:
    """Ensure a Codixing index exists for the repo. Returns True if ready."""
    index_dir = repo_path / ".codixing"
    if index_dir.exists():
        return True

    print(f"    [index] Building Codixing index for {repo_path.name}...")
    env = {**os.environ, **ENV_EXTRAS}
    result = subprocess.run(
        [str(CODIXING_CLI), "init", str(repo_path)],
        capture_output=True,
        timeout=600,
        env=env,
    )
    if result.returncode != 0:
        print(f"    [index] FAILED: {result.stderr.decode()[:200]}")
        return False
    print(f"    [index] Done")
    return True


async def run_agent_task(
    task: dict,
    repo_path: Path,
    mode: str,
    run_number: int,
    model: str,
) -> AgentRunResult:
    """Run a single agent task in vanilla or codixing mode."""
    result = AgentRunResult(
        task_id=task["id"],
        repo=task["repo"],
        mode=mode,
        run_number=run_number,
        category=task.get("category", ""),
        prompt=task["prompt"],
    )

    # Tool call tracking from message content blocks
    tool_calls_log: list[str] = []

    # MCP config for codixing mode
    mcp_config: dict = {}
    if mode == "codixing":
        mcp_config = {
            "codixing": {
                "command": str(CODIXING_MCP),
                "args": ["--root", str(repo_path)],
                "env": ENV_EXTRAS,
            }
        }

    # System prompt to focus the agent
    system = (
        "You are a code exploration assistant. Answer the question about the codebase "
        "you have access to. Be thorough but concise. Include file paths and code "
        "snippets in your answer."
    )

    start_time = time.monotonic()

    try:
        async for message in query(
            prompt=task["prompt"],
            options=ClaudeAgentOptions(
                cwd=str(repo_path),
                allowed_tools=["Grep", "Glob", "Read"],
                mcp_servers=mcp_config,
                permission_mode="bypassPermissions",
                max_turns=30,
                model=model,
                system_prompt=system,
            ),
        ):
            # Count tool calls from assistant message content blocks
            if isinstance(message, AssistantMessage):
                for block in message.content:
                    if isinstance(block, ToolUseBlock):
                        tool_calls_log.append(block.name)

            if isinstance(message, ResultMessage):
                result.result_text = message.result or ""
                result.num_turns = message.num_turns
                if message.usage:
                    result.input_tokens = message.usage.get("input_tokens", 0)
                    result.output_tokens = message.usage.get("output_tokens", 0)
                    result.total_tokens = (
                        result.input_tokens + result.output_tokens
                    )
                if message.total_cost_usd:
                    result.cost_usd = message.total_cost_usd

    except Exception as e:
        result.error = str(e)[:500]
        result.outcome = "error"

    result.wall_time_seconds = time.monotonic() - start_time

    # Count tool calls
    result.tool_calls = len(tool_calls_log)
    for name in tool_calls_log:
        result.tool_call_breakdown[name] = (
            result.tool_call_breakdown.get(name, 0) + 1
        )

    # Check acceptance
    if not result.error:
        acceptance = task.get("acceptance", {})
        if check_acceptance(result.result_text, acceptance):
            result.outcome = "pass"
        else:
            result.outcome = "fail"

    return result


def generate_report(
    all_results: list[AgentRunResult], model: str, runs: int
) -> str:
    """Generate markdown benchmark report with stats."""
    lines: list[str] = []
    lines.append("# Codixing Agent Benchmark Report\n")
    lines.append(f"**Date:** {time.strftime('%Y-%m-%d %H:%M')}")
    lines.append(f"**Model:** {model}")
    lines.append(f"**Runs per task per condition:** {runs}\n")

    # Group by task
    tasks: dict[str, dict[str, list[AgentRunResult]]] = {}
    for r in all_results:
        key = r.task_id
        tasks.setdefault(key, {"vanilla": [], "codixing": []})
        tasks[key][r.mode].append(r)

    # Aggregate stats
    all_vanilla_calls: list[float] = []
    all_codixing_calls: list[float] = []
    all_vanilla_tokens: list[float] = []
    all_codixing_tokens: list[float] = []
    all_vanilla_time: list[float] = []
    all_codixing_time: list[float] = []
    vanilla_passes = 0
    codixing_passes = 0
    vanilla_total = 0
    codixing_total = 0

    for task_id, modes in tasks.items():
        for r in modes["vanilla"]:
            all_vanilla_calls.append(r.tool_calls)
            all_vanilla_tokens.append(r.total_tokens)
            all_vanilla_time.append(r.wall_time_seconds)
            if r.outcome == "pass":
                vanilla_passes += 1
            vanilla_total += 1
        for r in modes["codixing"]:
            all_codixing_calls.append(r.tool_calls)
            all_codixing_tokens.append(r.total_tokens)
            all_codixing_time.append(r.wall_time_seconds)
            if r.outcome == "pass":
                codixing_passes += 1
            codixing_total += 1

    v_calls = compute_stats(all_vanilla_calls)
    c_calls = compute_stats(all_codixing_calls)
    v_tokens = compute_stats(all_vanilla_tokens)
    c_tokens = compute_stats(all_codixing_tokens)
    v_time = compute_stats(all_vanilla_time)
    c_time = compute_stats(all_codixing_time)

    def reduction(v_mean: float, c_mean: float) -> float:
        if v_mean == 0:
            return 0.0
        return (v_mean - c_mean) / v_mean * 100

    lines.append("## Summary\n")
    lines.append(
        "| Metric | Vanilla (mean) | Codixing (mean) | Reduction |"
    )
    lines.append("|--------|----------------|-----------------|-----------|")
    lines.append(
        f"| Tool calls | {v_calls.mean:.1f} | {c_calls.mean:.1f} | "
        f"**{reduction(v_calls.mean, c_calls.mean):.0f}% fewer** |"
    )
    lines.append(
        f"| Tokens | {v_tokens.mean:,.0f} | {c_tokens.mean:,.0f} | "
        f"**{reduction(v_tokens.mean, c_tokens.mean):.0f}% fewer** |"
    )
    lines.append(
        f"| Wall time | {v_time.mean:.1f}s | {c_time.mean:.1f}s | "
        f"**{reduction(v_time.mean, c_time.mean):.0f}% faster** |"
    )
    v_rate = vanilla_passes / vanilla_total * 100 if vanilla_total else 0
    c_rate = codixing_passes / codixing_total * 100 if codixing_total else 0
    lines.append(
        f"| Pass rate | {v_rate:.0f}% | {c_rate:.0f}% | "
        f"**{c_rate - v_rate:+.0f}%** |"
    )

    # Per-task results
    lines.append("\n## Per-Task Results\n")
    lines.append(
        "| Task | Repo | Category | V Calls (mean+/-std) | C Calls (mean+/-std) "
        "| Call Reduction | Significant? |"
    )
    lines.append(
        "|------|------|----------|----------------------|----------------------"
        "|----------------|--------------|"
    )

    for task_id, modes in sorted(tasks.items()):
        if not modes["vanilla"] or not modes["codixing"]:
            continue
        repo = modes["vanilla"][0].repo
        cat = modes["vanilla"][0].category
        v_tc = compute_stats([float(r.tool_calls) for r in modes["vanilla"]])
        c_tc = compute_stats([float(r.tool_calls) for r in modes["codixing"]])
        red = reduction(v_tc.mean, c_tc.mean)
        # Significant if CIs don't overlap
        sig = "Yes" if c_tc.ci95_high < v_tc.ci95_low else "No"
        if runs < 3:
            sig = "N/A"
        lines.append(
            f"| {task_id} | {repo} | {cat} | "
            f"{v_tc.mean:.1f} +/- {v_tc.stddev:.1f} | "
            f"{c_tc.mean:.1f} +/- {c_tc.stddev:.1f} | "
            f"{red:.0f}% | {sig} |"
        )

    # Cost summary
    total_cost = sum(r.cost_usd for r in all_results if r.cost_usd)
    if total_cost > 0:
        lines.append(f"\n## Cost\n")
        lines.append(f"**Total:** ${total_cost:.2f}")
        lines.append(
            f"**Per session:** ${total_cost / len(all_results):.3f}"
        )

    return "\n".join(lines)


async def main():
    parser = argparse.ArgumentParser(description="Codixing agent benchmark")
    parser.add_argument("--repos", help="Comma-separated repo names")
    parser.add_argument(
        "--runs", type=int, default=1, help="Runs per task per condition"
    )
    parser.add_argument("--tasks", help="Comma-separated task IDs")
    parser.add_argument(
        "--model", default="claude-sonnet-4-6", help="Model ID"
    )
    parser.add_argument(
        "--output", default=str(RESULTS_DIR), help="Output directory"
    )
    args = parser.parse_args()

    # Check binary
    if not CODIXING_MCP.exists():
        print(f"ERROR: codixing-mcp binary not found at {CODIXING_MCP}")
        print("Run: cargo build --release --workspace")
        sys.exit(1)

    # Load configs
    with open(ROOT / "benchmarks" / "repos.toml", "rb") as f:
        repos_cfg = tomllib.load(f)
    with open(ROOT / "benchmarks" / "agent_tasks.toml", "rb") as f:
        tasks_cfg = tomllib.load(f)

    repos = {r["name"]: r for r in repos_cfg["repo"]}
    all_tasks = tasks_cfg["agent_task"]

    # Filter
    if args.repos:
        repo_filter = set(args.repos.split(","))
        all_tasks = [t for t in all_tasks if t["repo"] in repo_filter]
    if args.tasks:
        task_filter = set(args.tasks.split(","))
        all_tasks = [t for t in all_tasks if t["id"] in task_filter]

    if not all_tasks:
        print("No tasks to run. Check --repos and --tasks filters.")
        sys.exit(1)

    output_dir = Path(args.output)
    output_dir.mkdir(parents=True, exist_ok=True)

    # Clone repos that need cloning
    needed_repos = set(t["repo"] for t in all_tasks)
    for repo_name in needed_repos:
        if repo_name == "codixing":
            continue
        repo_cfg = repos.get(repo_name)
        if not repo_cfg:
            print(f"WARNING: repo '{repo_name}' not in repos.toml, skipping")
            continue
        repo_path = REPOS_DIR / repo_name
        if not repo_path.exists():
            print(f"  [clone] {repo_name} from {repo_cfg['url']}...")
            REPOS_DIR.mkdir(parents=True, exist_ok=True)
            subprocess.run(
                ["git", "clone", "--depth=1", repo_cfg["url"], str(repo_path)],
                capture_output=True,
                timeout=300,
            )

    # Run benchmark
    all_results: list[AgentRunResult] = []
    total_sessions = len(all_tasks) * 2 * args.runs
    completed = 0

    for task in all_tasks:
        repo_path = get_repo_path(task["repo"])
        if not repo_path.exists():
            print(f"  [SKIP] {task['repo']} not available")
            continue

        print(f"\n{'='*60}")
        print(f"  {task['id']}: {task['prompt'][:60]}...")
        print(f"{'='*60}")

        # Ensure codixing index for codixing mode
        has_index = ensure_index(repo_path)

        for run_num in range(1, args.runs + 1):
            for mode in ["vanilla", "codixing"]:
                if mode == "codixing" and not has_index:
                    print(f"    [{mode}] run {run_num}: SKIP (no index)")
                    continue

                completed += 1
                print(
                    f"    [{mode}] run {run_num}/{args.runs} "
                    f"({completed}/{total_sessions})...",
                    end=" ",
                    flush=True,
                )

                r = await run_agent_task(
                    task, repo_path, mode, run_num, args.model
                )
                all_results.append(r)

                print(
                    f"{r.outcome} | "
                    f"{r.tool_calls} calls | "
                    f"{r.total_tokens:,} tokens | "
                    f"{r.wall_time_seconds:.1f}s"
                )

    # Generate reports
    report = generate_report(all_results, args.model, args.runs)
    report_path = output_dir / "agent_benchmark.md"
    report_path.write_text(report)
    print(f"\nReport: {report_path}")

    # Save raw JSON
    json_path = output_dir / "agent_benchmark.json"
    json_data = [asdict(r) for r in all_results]
    json_path.write_text(json.dumps(json_data, indent=2, default=str))
    print(f"Raw data: {json_path}")

    # Print summary to stdout
    print(f"\n{report}")


if __name__ == "__main__":
    import anyio

    anyio.run(main)

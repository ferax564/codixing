#!/usr/bin/env python3
"""
agent_benchmark_large.py — Run vanilla vs codixing agent sessions on
openclaw (~2K TS files) and linux (~63K C/H files).

Extends agent_benchmark.py with:
  * ground_truth recall scoring (substring match in result_text)
  * external repo paths (linux lives at ~/code/linux, not in benchmarks/repos/)
  * optional --wire-hooks: attach the same PreToolUse dogfooding hooks used in
    .claude/settings.json so the codixing-mode agent is steered off Grep/Bash
    at decision time, not just by tool availability.

Usage:
    .venv/bin/python3 benchmarks/agent_benchmark_large.py --runs 1
    .venv/bin/python3 benchmarks/agent_benchmark_large.py --repos openclaw --runs 2
    .venv/bin/python3 benchmarks/agent_benchmark_large.py --wire-hooks
"""

import argparse
import asyncio
import json
import math
import os
import re
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
    AssistantMessage,
    ResultMessage,
)
from claude_agent_sdk.types import ToolUseBlock, HookMatcher

ROOT = Path(__file__).resolve().parent.parent
CODIXING_MCP = ROOT / "target" / "release" / "codixing-mcp"
CODIXING_CLI = ROOT / "target" / "release" / "codixing"
DEFAULT_TASKS_FILE = ROOT / "benchmarks" / "agent_tasks_large.toml"
RESULTS_DIR = ROOT / "benchmarks" / "results"

REPO_PATHS = {
    "openclaw": ROOT / "benchmarks" / "repos" / "openclaw",
    "linux": Path.home() / "code" / "linux",
}

ORT_DYLIB = os.path.expanduser("~/.local/lib/libonnxruntime.dylib")
ORT_SO = os.path.expanduser("~/.local/lib/libonnxruntime.so")
ENV_EXTRAS: dict[str, str] = {}
if os.path.exists(ORT_DYLIB):
    ENV_EXTRAS["ORT_DYLIB_PATH"] = ORT_DYLIB
elif os.path.exists(ORT_SO):
    ENV_EXTRAS["ORT_DYLIB_PATH"] = ORT_SO
    ENV_EXTRAS["LD_LIBRARY_PATH"] = os.path.expanduser("~/.local/lib")


@dataclass
class RunResult:
    task_id: str
    repo: str
    mode: str
    run_number: int
    category: str = ""
    prompt: str = ""
    tool_calls: int = 0
    tool_call_breakdown: dict = field(default_factory=dict)
    input_tokens: int = 0
    output_tokens: int = 0
    total_tokens: int = 0
    wall_time_seconds: float = 0.0
    recall: float = 0.0
    ground_truth_hits: int = 0
    ground_truth_total: int = 0
    missed: list = field(default_factory=list)
    result_text: str = ""
    num_turns: int = 0
    cost_usd: float = 0.0
    error: str = ""


CODE_EXT_RE = re.compile(
    r"\.(rs|py|ts|tsx|js|jsx|go|java|c|cpp|cc|h|hpp|cs|rb|swift|kt|scala|php|zig|sh|md|html|json|toml|yaml|yml)(\s|$)"
)

DENY_MESSAGE = (
    "CODIXING DOGFOODING: Use the codixing CLI instead of Grep/rg/find for "
    "code/doc/config search.\n\n"
    "Suggested commands:\n"
    '  codixing search "<query>"       — semantic search\n'
    "  codixing symbols <name>         — find symbol definitions\n"
    "  codixing usages <symbol>        — find call sites and imports\n"
    "  codixing callers <file>         — who imports this file\n"
    "  codixing callees <file>         — what this file imports\n"
    "  codixing impact <file>          — blast radius analysis\n"
    "  codixing graph --map            — architecture overview\n\n"
    "Run via Bash from the repo root. You can also call mcp__codixing__* tools "
    "directly."
)


def _should_deny_grep(tool_input: dict) -> bool:
    pattern = tool_input.get("pattern", "") or ""
    if len(pattern) < 3:
        return False
    path = tool_input.get("path", "") or ""
    if re.search(r"(target/|node_modules/|\.git/|\.codixing/|vendor/)", path):
        return False
    # Single file target — allow
    if re.search(r"\.[a-zA-Z0-9]+$", path):
        return False
    # Version strings — allow
    if re.fullmatch(r"v?\d+\.\d+(\.\d+)?", pattern):
        return False
    return True


def _should_deny_bash(cmd: str) -> bool:
    trimmed = cmd.lstrip()
    first = trimmed.split()[0] if trimmed.split() else ""
    base = first.rsplit("/", 1)[-1]
    if base not in {
        "grep", "egrep", "fgrep", "rgrep", "rg", "ag", "ack", "ripgrep",
        "find", "cat", "bat", "head", "tail", "less", "more",
    }:
        return False
    # find without -exec/grep pipe — file finding, allow
    if base == "find":
        if not re.search(r"(-exec|\|\s*grep|\|\s*rg|-print0)", trimmed):
            return False
    # cat/head/tail on single file without grep pipe — allow (Read replacement)
    if base in {"cat", "bat", "head", "tail", "less", "more"}:
        if not re.search(r"\|\s*(grep|rg|ag|ack|egrep|fgrep)", trimmed):
            return False
    # | wc -l count mode — allow
    if re.search(r"\|\s*wc\s+-l", trimmed):
        return False
    # Version search — allow
    if re.search(r"['\"]v?\d+\.\d+(\.\d+)?['\"]", trimmed):
        return False
    # Non-indexed targets — allow
    if re.search(r"(target/|node_modules/|\.git/|\.codixing/|vendor/|/tmp/|/private/tmp/)", trimmed):
        return False
    # Single-file target without -r — allow
    if not re.search(r"(\s-r|\s-R|--recursive|\s-rn|\s-rH)", trimmed):
        if CODE_EXT_RE.search(trimmed):
            return False
    return True


async def deny_grep_hook(input_data, tool_use_id, context):
    tool_input = input_data.get("tool_input", {})
    if _should_deny_grep(tool_input):
        return {
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "additionalContext": DENY_MESSAGE,
            }
        }
    return {}


async def deny_bash_hook(input_data, tool_use_id, context):
    cmd = input_data.get("tool_input", {}).get("command", "")
    if cmd and _should_deny_bash(cmd):
        return {
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "additionalContext": DENY_MESSAGE,
            }
        }
    return {}


def score_recall(result_text: str, ground_truth: list[str]) -> tuple[int, int, list[str]]:
    if not ground_truth:
        return 0, 0, []
    text = result_text.lower()
    hits = 0
    missed: list[str] = []
    for item in ground_truth:
        if item.lower() in text:
            hits += 1
        else:
            missed.append(item)
    return hits, len(ground_truth), missed


def ensure_index(repo_path: Path) -> bool:
    if (repo_path / ".codixing").exists():
        return True
    print(f"    [index] building for {repo_path}…")
    env = {**os.environ, **ENV_EXTRAS}
    r = subprocess.run(
        [str(CODIXING_CLI), "init", str(repo_path)],
        capture_output=True, timeout=1800, env=env,
    )
    return r.returncode == 0


async def run_one(task: dict, repo_path: Path, mode: str, run_number: int,
                  model: str, sticky: bool = False) -> RunResult:
    result = RunResult(
        task_id=task["id"], repo=task["repo"], mode=mode,
        run_number=run_number, category=task.get("category", ""),
        prompt=task["prompt"],
    )
    tool_log: list[str] = []

    mcp_config: dict = {}
    hooks_cfg = None
    allowed = ["Grep", "Glob", "Read", "Bash"]

    if mode == "codixing":
        mcp_config = {
            "codixing": {
                "command": str(CODIXING_MCP),
                # Full tool load (no --medium). The --medium curation hid
                # get_complexity, review_context and other showcase tools
                # from the advertised set, silently reducing codixing's
                # advantage on tasks where the dedicated MCP tool is the
                # right answer in one call. See research doc §4.15.
                "args": ["--root", str(repo_path), "--no-daemon-fork"],
                "env": ENV_EXTRAS,
            }
        }
        if sticky:
            # Mirror production .claude/settings.json: deny Grep + Bash-grep,
            # redirect to codixing CLI / MCP.
            hooks_cfg = {
                "PreToolUse": [
                    HookMatcher(matcher="Grep", hooks=[deny_grep_hook]),
                    HookMatcher(matcher="Bash", hooks=[deny_bash_hook]),
                ]
            }

    if mode == "codixing" and sticky:
        system = (
            "You are a code exploration assistant for the codebase at the "
            "current working directory. "
            "PREFER the codixing toolchain for ANY code exploration task:\n"
            "  - mcp__codixing__* tools for structural queries (callers, "
            "usages, impact, symbols, search).\n"
            "  - Bash invocations of the `codixing` CLI "
            "(`codixing search`, `codixing symbols`, `codixing usages`, "
            "`codixing callers`, `codixing callees`, `codixing impact`, "
            "`codixing graph --map`) for anything else.\n"
            "Grep and find are DENIED for code/doc/config search by a "
            "pre-tool hook — do not waste turns on them. Use `Read` only "
            "when you already know the exact file path.\n"
            "Be thorough but concise. Include concrete file paths in your "
            "answer."
        )
    else:
        system = (
            "You are a code exploration assistant. Answer the question "
            "about the codebase you have access to. Be thorough but "
            "concise. Include concrete file paths in your answer."
        )

    start = time.monotonic()
    try:
        opts = ClaudeAgentOptions(
            cwd=str(repo_path),
            allowed_tools=allowed,
            mcp_servers=mcp_config,
            permission_mode="bypassPermissions",
            max_turns=30,
            model=model,
            system_prompt=system,
        )
        if hooks_cfg is not None:
            opts.hooks = hooks_cfg
        async for message in query(prompt=task["prompt"], options=opts):
            if isinstance(message, AssistantMessage):
                for block in message.content:
                    if isinstance(block, ToolUseBlock):
                        tool_log.append(block.name)
            if isinstance(message, ResultMessage):
                result.result_text = message.result or ""
                result.num_turns = message.num_turns
                if message.usage:
                    result.input_tokens = message.usage.get("input_tokens", 0)
                    result.output_tokens = message.usage.get("output_tokens", 0)
                    result.total_tokens = result.input_tokens + result.output_tokens
                if message.total_cost_usd:
                    result.cost_usd = message.total_cost_usd
    except Exception as e:
        result.error = str(e)[:500]

    result.wall_time_seconds = time.monotonic() - start
    result.tool_calls = len(tool_log)
    for n in tool_log:
        result.tool_call_breakdown[n] = result.tool_call_breakdown.get(n, 0) + 1

    hits, total, missed = score_recall(result.result_text, task.get("ground_truth", []))
    result.ground_truth_hits = hits
    result.ground_truth_total = total
    result.recall = (hits / total) if total else 0.0
    result.missed = missed

    return result


MODES_ORDER = ("vanilla", "codixing", "codixing-sticky")


def render_report(results: list[RunResult], model: str, runs: int) -> str:
    by_task: dict[str, dict[str, list[RunResult]]] = {}
    for r in results:
        by_task.setdefault(r.task_id, {m: [] for m in MODES_ORDER})
        by_task[r.task_id].setdefault(r.mode, []).append(r)

    def mean(xs): return sum(xs) / len(xs) if xs else 0.0

    lines: list[str] = []
    lines.append("# Codixing Large-Repo Agent Benchmark\n")
    lines.append(f"**Date:** {time.strftime('%Y-%m-%d %H:%M')}")
    lines.append(f"**Model:** {model}")
    lines.append(f"**Runs per task per mode:** {runs}\n")

    buckets: dict[str, dict[str, list[float]]] = {
        m: {"calls": [], "tok": [], "time": [], "rec": []} for m in MODES_ORDER
    }
    for tid, modes in by_task.items():
        for m in MODES_ORDER:
            for r in modes.get(m, []):
                buckets[m]["calls"].append(r.tool_calls)
                buckets[m]["tok"].append(r.total_tokens)
                buckets[m]["time"].append(r.wall_time_seconds)
                buckets[m]["rec"].append(r.recall)

    def red(base, other):
        return ((base - other) / base * 100) if base else 0.0

    lines.append("## Summary\n")
    header = "| Metric | " + " | ".join(m for m in MODES_ORDER if buckets[m]["calls"]) + " |"
    sep = "|---|" + "---|" * sum(1 for m in MODES_ORDER if buckets[m]["calls"])
    lines.append(header)
    lines.append(sep)
    present = [m for m in MODES_ORDER if buckets[m]["calls"]]
    def row(label, key, fmt):
        vals = [fmt(mean(buckets[m][key])) for m in present]
        return f"| {label} | " + " | ".join(vals) + " |"
    lines.append(row("Tool calls (mean)", "calls", lambda x: f"{x:.1f}"))
    lines.append(row("Tokens (mean)", "tok", lambda x: f"{x:,.0f}"))
    lines.append(row("Wall time (mean)", "time", lambda x: f"{x:.1f}s"))
    lines.append(row("Recall (mean)", "rec", lambda x: f"{x*100:.0f}%"))
    lines.append("")

    # Deltas vs vanilla
    if "vanilla" in present and len(present) > 1:
        lines.append("### Deltas vs vanilla\n")
        lines.append("| Mode | Calls | Tokens | Time | Recall |")
        lines.append("|---|---|---|---|---|")
        v_calls = mean(buckets["vanilla"]["calls"])
        v_tok = mean(buckets["vanilla"]["tok"])
        v_time = mean(buckets["vanilla"]["time"])
        v_rec = mean(buckets["vanilla"]["rec"])
        for m in present:
            if m == "vanilla":
                continue
            c = mean(buckets[m]["calls"])
            t = mean(buckets[m]["tok"])
            tm = mean(buckets[m]["time"])
            rc = mean(buckets[m]["rec"])
            lines.append(
                f"| {m} | {red(v_calls,c):+.0f}% | {red(v_tok,t):+.0f}% | "
                f"{red(v_time,tm):+.0f}% | {(rc-v_rec)*100:+.0f}pp |"
            )
        lines.append("")

    lines.append("## Per-Task Results\n")
    per_task_header = "| Task | Repo | Cat |"
    per_task_sep = "|---|---|---|"
    for m in present:
        per_task_header += f" {m} calls | {m} tok | {m} rec |"
        per_task_sep += "---|---|---|"
    lines.append(per_task_header)
    lines.append(per_task_sep)
    for tid, modes in sorted(by_task.items()):
        if not modes.get("vanilla"):
            continue
        repo = modes["vanilla"][0].repo
        cat = modes["vanilla"][0].category
        row = f"| {tid} | {repo} | {cat} |"
        for m in present:
            rs = modes.get(m, [])
            if not rs:
                row += " - | - | - |"
                continue
            c = mean([r.tool_calls for r in rs])
            t = mean([r.total_tokens for r in rs])
            rc = mean([r.recall for r in rs]) * 100
            row += f" {c:.1f} | {t:,.0f} | {rc:.0f}% |"
        lines.append(row)

    lines.append("")
    lines.append("## Tool Breakdown (codixing-sticky)\n")
    for tid, modes in sorted(by_task.items()):
        for r in modes.get("codixing-sticky", []):
            lines.append(f"- **{tid}** → `{r.tool_call_breakdown}`")
    lines.append("")

    lines.append("## Missed Ground-Truth Items\n")
    for tid, modes in sorted(by_task.items()):
        for m in present:
            for r in modes.get(m, []):
                if r.missed:
                    lines.append(f"- **{tid}** [{m}] missed: {', '.join(r.missed)}")

    total_cost = sum(r.cost_usd for r in results if r.cost_usd)
    if total_cost:
        lines.append(f"\n**Total cost:** ${total_cost:.2f}  ({len(results)} sessions)")

    return "\n".join(lines)


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repos", help="Comma list: openclaw,linux")
    ap.add_argument("--tasks", help="Comma list of task ids")
    ap.add_argument("--runs", type=int, default=1)
    ap.add_argument("--model", default="claude-sonnet-4-6")
    ap.add_argument("--output", default=str(RESULTS_DIR))
    ap.add_argument("--no-sticky", action="store_true",
                    help="Skip the codixing-sticky mode (hooks + prompt nudge)")
    ap.add_argument("--tasks-file", default=str(DEFAULT_TASKS_FILE),
                    help="Path to a tasks TOML file")
    ap.add_argument("--only-sticky", action="store_true",
                    help="Only run vanilla + codixing-sticky, skip bare codixing")
    ap.add_argument("--output-suffix", default="",
                    help="Suffix added to output filenames (e.g. '_hard')")
    args = ap.parse_args()

    if not CODIXING_MCP.exists():
        print(f"ERROR: missing {CODIXING_MCP}. cargo build --release --workspace")
        sys.exit(1)

    with open(args.tasks_file, "rb") as f:
        cfg = tomllib.load(f)
    tasks = cfg["agent_task"]

    if args.repos:
        keep = set(args.repos.split(","))
        tasks = [t for t in tasks if t["repo"] in keep]
    if args.tasks:
        keep = set(args.tasks.split(","))
        tasks = [t for t in tasks if t["id"] in keep]
    if not tasks:
        print("No tasks after filter"); sys.exit(1)

    out = Path(args.output); out.mkdir(parents=True, exist_ok=True)

    # Ensure each needed repo has an index
    for repo in set(t["repo"] for t in tasks):
        rp = REPO_PATHS.get(repo)
        if not rp or not rp.exists():
            print(f"WARNING: repo path missing for {repo}: {rp}")
            continue
        if not ensure_index(rp):
            print(f"ERROR: could not build index for {repo}"); sys.exit(1)

    mode_specs = [
        ("vanilla", False),
        ("codixing", False),
        ("codixing", True),  # sticky: hooks + prompt nudge
    ]
    if args.no_sticky:
        mode_specs = mode_specs[:2]
    if args.only_sticky:
        mode_specs = [("vanilla", False), ("codixing", True)]

    results: list[RunResult] = []
    total = len(tasks) * len(mode_specs) * args.runs
    done = 0
    for task in tasks:
        rp = REPO_PATHS[task["repo"]]
        print(f"\n{'='*60}\n  {task['id']}: {task['prompt'][:60]}…\n{'='*60}")
        for run_n in range(1, args.runs + 1):
            for mode, sticky in mode_specs:
                done += 1
                label = f"{mode}-sticky" if sticky else mode
                print(f"    [{label}] run {run_n}/{args.runs} ({done}/{total})…", end=" ", flush=True)
                try:
                    r = await run_one(task, rp, mode, run_n, args.model, sticky=sticky)
                    if sticky:
                        r.mode = "codixing-sticky"
                except Exception as e:
                    print(f"FAIL {e}")
                    continue
                results.append(r)
                print(f"{r.tool_calls}c | {r.total_tokens:,}t | {r.wall_time_seconds:.1f}s | recall {r.recall*100:.0f}%")

                # incremental checkpoint save
                suffix = args.output_suffix
                (out / f"agent_benchmark_large{suffix}.json").write_text(
                    json.dumps([asdict(x) for x in results], indent=2, default=str))

    report = render_report(results, args.model, args.runs)
    suffix = args.output_suffix
    (out / f"agent_benchmark_large{suffix}.md").write_text(report)
    (out / f"agent_benchmark_large{suffix}.json").write_text(
        json.dumps([asdict(x) for x in results], indent=2, default=str))
    print(f"\n{report}\n\nSaved: {out / f'agent_benchmark_large{suffix}.md'}")


if __name__ == "__main__":
    asyncio.run(main())

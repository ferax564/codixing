#!/usr/bin/env python3
"""
agent_benchmark_large.py — Run vanilla vs codixing agent sessions on
openclaw (~2K TS files) and linux (~63K C/H files).

Extends agent_benchmark.py with:
  * ground_truth recall scoring: word-boundary identifier match against
    result_text (so `do_sys_open` cannot falsely hit `do_sys_openat2`)
  * external repo paths (linux lives at ~/code/linux, not in benchmarks/repos/)
  * codixing-sticky mode: attaches the same PreToolUse dogfooding hooks used
    in .claude/settings.json as Python async callables + a system-prompt
    nudge, so the codixing-mode agent is steered off Grep/Bash at decision
    time, not just by tool availability. Enabled by default (run with
    --no-sticky to skip, or --only-sticky to drop the bare codixing mode).

Usage:
    .venv/bin/python3 benchmarks/agent_benchmark_large.py --runs 1
    .venv/bin/python3 benchmarks/agent_benchmark_large.py --repos openclaw --runs 2
    .venv/bin/python3 benchmarks/agent_benchmark_large.py --only-sticky \\
        --tasks-file benchmarks/agent_tasks_hard.toml --output-suffix _hard
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
        needle = item.lower()
        # Word-boundary match for identifier-shaped needles so a shorter
        # name can't falsely hit inside a longer one — e.g. `do_sys_open`
        # must not score against `do_sys_openat2`. Paths and numbers still
        # use substring semantics (word boundaries handle those too since
        # `/` and digits are non-word characters on the outside).
        if re.search(rf"(?<![A-Za-z0-9_]){re.escape(needle)}(?![A-Za-z0-9_])", text):
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
    got_result_message = False
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
                got_result_message = True
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

    # Missing ResultMessage OR raised exception = infrastructure failure
    # (SDK / auth / cwd / network). Do NOT let these pollute the means;
    # flag with `error` so the caller can exclude them from aggregates.
    if not got_result_message and not result.error:
        result.error = "no ResultMessage emitted (SDK truncation or subprocess crash)"

    result.wall_time_seconds = time.monotonic() - start
    result.tool_calls = len(tool_log)
    for n in tool_log:
        result.tool_call_breakdown[n] = result.tool_call_breakdown.get(n, 0) + 1

    if result.error:
        # Leave recall at 0 but mark via error so the report separates these
        # from real sessions. Means computed over `error == ""` only.
        result.ground_truth_total = len(task.get("ground_truth", []))
        return result

    hits, total, missed = score_recall(result.result_text, task.get("ground_truth", []))
    result.ground_truth_hits = hits
    result.ground_truth_total = total
    result.recall = (hits / total) if total else 0.0
    result.missed = missed

    return result


MODES_ORDER = ("vanilla", "codixing", "codixing-sticky")


def render_report(results: list[RunResult], model: str, runs: int) -> str:
    # Split infra-failed sessions (result.error set) out of aggregates but
    # keep them in `results` so the caller's raw JSON still has the full
    # record. Failed sessions appear in a separate section of the report.
    ok_results = [r for r in results if not r.error]
    failed = [r for r in results if r.error]

    # `by_task` holds EVERY session (ok + failed) so the per-task table
    # can render a row for each task, including ones where a mode infra-
    # failed. Aggregate means below use a `paired_set` filter, so they
    # only count sessions where all present modes succeeded — but the
    # per-task view stays complete and shows "FAIL" cells where needed.
    by_task: dict[str, dict[str, list[RunResult]]] = {}
    for r in results:
        by_task.setdefault(r.task_id, {m: [] for m in MODES_ORDER})
        by_task[r.task_id].setdefault(r.mode, []).append(r)

    def mean(xs): return sum(xs) / len(xs) if xs else 0.0

    # Paired comparison: the summary and delta tables below average each
    # mode over the intersection of (task, run) keys where EVERY present
    # mode produced a successful (error-free) session. Otherwise, if
    # vanilla infra-fails task X and codixing-sticky succeeds, the means
    # would compare different task populations and the headline deltas
    # would be apples-to-oranges.
    ok_by_task: dict[str, dict[str, list[RunResult]]] = {}
    for r in ok_results:
        ok_by_task.setdefault(r.task_id, {m: [] for m in MODES_ORDER})
        ok_by_task[r.task_id].setdefault(r.mode, []).append(r)

    present_modes = [m for m in MODES_ORDER if any(ok_by_task[t].get(m) for t in ok_by_task)]
    present_modes = list(dict.fromkeys(present_modes))  # preserve order
    paired_keys: list[tuple[str, int]] = []
    for tid, modes in ok_by_task.items():
        # Collect the set of run_numbers that succeeded in ALL present modes.
        common_runs: set[int] | None = None
        for m in present_modes:
            runs_for_m = {r.run_number for r in modes.get(m, [])}
            if not runs_for_m:
                common_runs = set()
                break
            common_runs = runs_for_m if common_runs is None else (common_runs & runs_for_m)
        for run_n in sorted(common_runs or set()):
            paired_keys.append((tid, run_n))

    dropped_tasks = sorted(
        {tid for tid in by_task if not any(
            (tid, rn) in paired_keys
            for rn in range(1, runs + 1)
        )}
    )

    lines: list[str] = []
    lines.append("# Codixing Large-Repo Agent Benchmark\n")
    lines.append(f"**Date:** {time.strftime('%Y-%m-%d %H:%M')}")
    lines.append(f"**Model:** {model}")
    lines.append(f"**Runs per task per mode:** {runs}")
    lines.append(
        f"**Paired comparison coverage:** {len(paired_keys)} "
        f"(task,run) pair(s) across {len(present_modes)} mode(s)"
    )
    if failed:
        lines.append(
            f"**Infra-failed sessions excluded from means:** {len(failed)} "
            f"(see `Infra-failed sessions` section + `error` field in JSON)"
        )
    if dropped_tasks:
        lines.append(
            f"**Tasks dropped from paired means** (not every mode succeeded): "
            f"{', '.join(dropped_tasks)}"
        )
    lines.append("")

    # Bucket only sessions that sit on a paired (task, run) key. This
    # guarantees every mode's mean is computed over the same denominator.
    paired_set = set(paired_keys)
    buckets: dict[str, dict[str, list[float]]] = {
        m: {"calls": [], "tok": [], "time": [], "rec": []} for m in MODES_ORDER
    }
    for tid, modes in by_task.items():
        for m in MODES_ORDER:
            for r in modes.get(m, []):
                if (tid, r.run_number) not in paired_set:
                    continue
                buckets[m]["calls"].append(r.tool_calls)
                buckets[m]["tok"].append(r.total_tokens)
                buckets[m]["time"].append(r.wall_time_seconds)
                buckets[m]["rec"].append(r.recall)

    # Negative pct reads as "fewer than vanilla" (i.e. a codixing win).
    # Positive pct reads as "more than vanilla". This matches the natural
    # direction of the metric names (Tool calls, Tokens, Time) so the sign
    # no longer contradicts the label.
    def delta_pct(base, other):
        return ((other - base) / base * 100) if base else 0.0

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

    # Deltas vs vanilla. For calls/tokens/time, negative = codixing lower
    # = codixing win. Recall is absolute percentage points where positive
    # is always a codixing win. Both directions are explicit in the
    # column headers so the signs can't be misread.
    if "vanilla" in present and len(present) > 1:
        lines.append("### Deltas vs vanilla (negative = codixing lower)\n")
        lines.append("| Mode | Calls | Tokens | Time | Recall (pp) |")
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
                f"| {m} | {delta_pct(v_calls,c):+.0f}% | "
                f"{delta_pct(v_tok,t):+.0f}% | "
                f"{delta_pct(v_time,tm):+.0f}% | "
                f"{(rc-v_rec)*100:+.0f}pp |"
            )
        lines.append("")

    lines.append("## Per-Task Results\n")
    lines.append(
        "Cells show `calls / tokens / recall`. `FAIL` = infra-failed "
        "session (no aggregate cost) — see the Infra-failed section."
    )
    lines.append("")
    per_task_header = "| Task | Repo | Cat |"
    per_task_sep = "|---|---|---|"
    for m in present:
        per_task_header += f" {m} |"
        per_task_sep += "---|"
    lines.append(per_task_header)
    lines.append(per_task_sep)
    for tid, modes in sorted(by_task.items()):
        # Prefer any available mode for the row label — a task where
        # vanilla infra-failed but codixing succeeded still deserves a
        # visible row so the reader can see the asymmetry.
        any_sessions = [r for m in MODES_ORDER for r in modes.get(m, [])]
        if not any_sessions:
            continue
        repo = any_sessions[0].repo
        cat = any_sessions[0].category
        row = f"| {tid} | {repo} | {cat} |"
        for m in present:
            rs = modes.get(m, [])
            if not rs:
                row += " — |"
                continue
            ok_rs = [r for r in rs if not r.error]
            if not ok_rs:
                row += " FAIL |"
                continue
            c = mean([r.tool_calls for r in ok_rs])
            t = mean([r.total_tokens for r in ok_rs])
            rc = mean([r.recall for r in ok_rs]) * 100
            cell = f" {c:.1f} / {t:,.0f} / {rc:.0f}%"
            if len(ok_rs) < len(rs):
                cell += f" *({len(rs)-len(ok_rs)} fail)*"
            row += f"{cell} |"
        lines.append(row)

    lines.append("")
    lines.append("## Tool Breakdown (codixing-sticky)\n")
    for tid, modes in sorted(by_task.items()):
        for r in modes.get("codixing-sticky", []):
            if r.error:
                continue
            lines.append(f"- **{tid}** → `{r.tool_call_breakdown}`")
    lines.append("")

    lines.append("## Missed Ground-Truth Items\n")
    for tid, modes in sorted(by_task.items()):
        for m in present:
            for r in modes.get(m, []):
                if r.error or not r.missed:
                    continue
                lines.append(f"- **{tid}** [{m}] missed: {', '.join(r.missed)}")

    ok_cost = sum(r.cost_usd for r in ok_results if r.cost_usd)
    fail_cost = sum(r.cost_usd for r in failed if r.cost_usd)
    if ok_cost or fail_cost or failed:
        lines.append(
            f"\n**Cost:** ${ok_cost:.2f} across {len(ok_results)} successful "
            f"session(s)"
        )
        if failed:
            # cost_usd is only populated when ResultMessage fires. A
            # crash/truncation before ResultMessage gives cost_usd=0, so
            # this line is a LOWER BOUND on real wasted API spend —
            # label it explicitly so the reader doesn't read $0.00 as
            # "nothing wasted".
            lines.append(
                f"**Wasted on infra failures (≥):** ${fail_cost:.2f} "
                f"across {len(failed)} failed session(s). Sessions that "
                f"crashed before emitting a ResultMessage report "
                f"`cost_usd=0` even when real API spend occurred — "
                f"the actual waste is typically higher."
            )
    if failed:
        lines.append("\n## Infra-failed sessions\n")
        for r in failed:
            lines.append(f"- `{r.task_id}` [{r.mode}] run {r.run_number}: {r.error}")

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

    # Skip tasks whose repo is unknown or whose clone is missing so they
    # don't later crash with KeyError or pollute the averages with failed
    # sessions. Build missing indexes up-front and fail the whole run if
    # any build fails — a silently partial index is worse than a crash.
    usable_repos: set[str] = set()
    for repo in sorted(set(t["repo"] for t in tasks)):
        rp = REPO_PATHS.get(repo)
        if rp is None:
            print(f"SKIP: unknown repo '{repo}' — not in REPO_PATHS")
            continue
        if not rp.exists():
            print(f"SKIP: repo '{repo}' clone missing at {rp}")
            continue
        if not ensure_index(rp):
            print(f"ERROR: could not build index for {repo} at {rp}")
            sys.exit(1)
        usable_repos.add(repo)

    dropped = [t["id"] for t in tasks if t["repo"] not in usable_repos]
    if dropped:
        print(f"SKIP: {len(dropped)} task(s) dropped due to missing repo: {dropped}")
    tasks = [t for t in tasks if t["repo"] in usable_repos]
    if not tasks:
        print("ERROR: no tasks left after missing-repo filter")
        sys.exit(1)

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

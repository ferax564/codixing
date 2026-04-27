#!/usr/bin/env python3
"""Direct competitor benchmark using OpenClaw ground-truth queries.

This harness compares Codixing with opt-in external tools such as
claude-context and codebase-memory-mcp. Tool commands are configured in
`benchmarks/competitor_tools.toml`; this file only handles orchestration,
path extraction, scoring, and reporting.

Default run:

    python3 benchmarks/competitor_benchmark.py --repo benchmarks/repos/openclaw

Useful dry checks:

    python3 benchmarks/competitor_benchmark.py --list-tools
    python3 benchmarks/competitor_benchmark.py --repo benchmarks/repos/openclaw --dry-run
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import time
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any

try:
    import tomllib
except ImportError:  # pragma: no cover - Python 3.10 fallback
    import tomli as tomllib


ROOT = Path(__file__).resolve().parent.parent
DEFAULT_REPO = ROOT / "benchmarks" / "repos" / "openclaw"
DEFAULT_QUERIES = ROOT / "benchmarks" / "queue_v2_queries.toml"
DEFAULT_TOOLS = ROOT / "benchmarks" / "competitor_tools.toml"
RESULTS_DIR = ROOT / "benchmarks" / "results"


@dataclass
class ToolConfig:
    name: str
    enabled: bool
    commands: dict[str, str]
    path_regex: str
    timeout_secs: int = 60
    notes: str = ""


@dataclass
class QueryResult:
    tool: str
    query: str
    category: str
    command: str
    elapsed_ms: int
    output_bytes: int
    returncode: int
    files: list[str]
    precision_at_10: float
    recall_at_10: float
    mrr: float
    error: str = ""


@dataclass
class BenchmarkReport:
    date: str
    repo: str
    queries_file: str
    tools_file: str
    top_k: int
    results: list[QueryResult] = field(default_factory=list)
    skipped_tools: list[dict[str, str]] = field(default_factory=list)
    validation: list[str] = field(default_factory=list)


def load_toml(path: Path) -> dict[str, Any]:
    with path.open("rb") as fh:
        return tomllib.load(fh)


def load_tools(path: Path, include_disabled: bool = False) -> tuple[list[ToolConfig], list[dict[str, str]]]:
    data = load_toml(path)
    tools: list[ToolConfig] = []
    skipped: list[dict[str, str]] = []

    for raw in data.get("tool", []):
        cfg = ToolConfig(
            name=raw["name"],
            enabled=bool(raw.get("enabled", False)),
            commands=dict(raw.get("commands", {})),
            path_regex=raw.get("path_regex", r"(\S+)"),
            timeout_secs=int(raw.get("timeout_secs", 60)),
            notes=raw.get("notes", ""),
        )
        if cfg.enabled or include_disabled:
            tools.append(cfg)
        else:
            skipped.append({"name": cfg.name, "reason": cfg.notes or "disabled"})
    return tools, skipped


def load_queries(path: Path, categories: set[str] | None = None) -> list[dict[str, Any]]:
    data = load_toml(path)
    queries = []
    for q in data.get("query", []):
        if not q.get("ground_truth"):
            continue
        if categories and q.get("category") not in categories:
            continue
        queries.append(q)
    return queries


def validate_queries(queries: list[dict[str, Any]], repo: Path) -> list[str]:
    """Return validation errors for benchmark query fixtures."""
    errors: list[str] = []

    for query in queries:
        name = query.get("name", "<unknown>")
        ground_truth = query.get("ground_truth", [])
        if not ground_truth:
            errors.append(f"{name}: missing ground_truth")
            continue

        for rel_path in ground_truth:
            if not isinstance(rel_path, str):
                errors.append(f"{name}: non-string ground_truth entry {rel_path!r}")
                continue
            if not (repo / rel_path).exists():
                errors.append(f"{name}: ground_truth file does not exist: {rel_path}")

        if query.get("cross_pattern"):
            pattern = normalize_bre_pattern(query.get("grep_pattern", ""))
            try:
                regex = re.compile(pattern, re.MULTILINE | re.DOTALL)
            except re.error as exc:
                errors.append(f"{name}: invalid cross_pattern regex {pattern!r}: {exc}")
                continue

            matched = []
            for rel_path in ground_truth:
                path = repo / rel_path
                if not path.exists():
                    continue
                try:
                    content = path.read_text(encoding="utf-8", errors="replace")
                except OSError as exc:
                    errors.append(f"{name}: cannot read {rel_path}: {exc}")
                    continue
                if regex.search(content):
                    matched.append(rel_path)

            if not matched:
                errors.append(
                    f"{name}: cross_pattern=true but grep_pattern does not match any ground_truth file"
                )

        category = query.get("category")
        if category == "cross-package":
            for key in ("from_dir", "to_dir"):
                value = query.get(key)
                if not value:
                    errors.append(f"{name}: cross-package query missing {key}")
                elif not (repo / value).exists():
                    errors.append(f"{name}: {key} path does not exist: {value}")

    return errors


def render_command(template: str, query: dict[str, Any], repo: Path, top_k: int) -> str:
    pattern = query.get("grep_pattern", query.get("text", ""))
    values = {
        "repo": str(repo),
        "codixing": str(ROOT / "target" / "release" / "codixing"),
        "query": query.get("text", ""),
        "pattern": pattern,
        "pattern_regex": normalize_bre_pattern(pattern),
        "symbol": query.get("symbol", query.get("grep_pattern", query.get("text", ""))),
        "from_dir": query.get("from_dir", ""),
        "to_dir": query.get("to_dir", ""),
        "top_k": str(top_k),
        "result_limit": str(top_k * 3),
        "codebase_memory": os.environ.get("CODEBASE_MEMORY_MCP", "codebase-memory-mcp"),
        "codebase_memory_project": os.environ.get("CODEBASE_MEMORY_PROJECT", project_name(repo)),
        "cbm_cache": os.environ.get("CBM_CACHE_DIR", ""),
        "cross_pattern_arg": f'--pattern "{normalize_bre_pattern(pattern)}"'
        if query.get("cross_pattern")
        else "",
    }

    rendered = template
    for field_name, raw in values.items():
        rendered = rendered.replace("{" + field_name + "}", raw)
    return rendered


def normalize_bre_pattern(pattern: str) -> str:
    """Convert the small grep-BRE subset in query fixtures to Rust regex."""
    return pattern.replace(r"\|", "|")


def normalize_path(path: str) -> str:
    path = path.strip().strip('"').strip("'")
    if path.startswith("./"):
        path = path[2:]
    return path


def extract_files(output: str, path_regex: str, top_k: int) -> list[str]:
    regex = re.compile(path_regex, re.MULTILINE)
    seen: set[str] = set()
    files: list[str] = []
    for match in regex.finditer(output):
        candidate = normalize_path(match.group(1))
        if not candidate or candidate in seen:
            continue
        seen.add(candidate)
        files.append(candidate)
        if len(files) >= top_k:
            break
    return files


DEFINITION_KINDS = {"TypeAlias", "Interface", "Class", "Struct", "Enum", "Function"}


def extract_codixing_symbol_files(output: str, query: dict[str, Any], top_k: int) -> list[str]:
    """Extract definition-first file paths from `codixing symbols` output."""
    target = query.get("symbol", query.get("grep_pattern", "")).strip()
    best_by_file: dict[str, tuple[int, int, int, str]] = {}

    for index, line in enumerate(output.splitlines()):
        stripped = line.strip()
        if not stripped or stripped.startswith("KIND") or stripped.startswith("---"):
            continue

        match = re.search(r"(\S+\.\w+)\s+L(\d+)(?:-L?(\d+))?", line)
        if not match:
            continue

        path = normalize_path(match.group(1))
        start = int(match.group(2))
        end = int(match.group(3)) if match.group(3) else start
        span = end - start + 1

        kind = stripped.split()[0]
        name_column = line[: match.start(1)].strip()
        name = name_column[len(kind) :].strip() if name_column.startswith(kind) else name_column
        is_definition = kind in DEFINITION_KINDS
        exact_name = bool(target and name == target)

        if exact_name and is_definition:
            group = 0
        elif exact_name:
            group = 1
        elif is_definition and target and target in name:
            group = 2
        elif is_definition:
            group = 3
        else:
            group = 4

        candidate = (group, -span, index, path)
        if path not in best_by_file or candidate < best_by_file[path]:
            best_by_file[path] = candidate

    ranked = sorted(best_by_file.values())
    return [path for _, _, _, path in ranked[:top_k]]


def extract_codixing_usage_files(output: str, top_k: int) -> list[str]:
    """Extract file paths from `codixing usages` table output."""
    seen: set[str] = set()
    files: list[str] = []
    for line in output.splitlines():
        stripped = line.strip()
        if not stripped or line.startswith("FILE ") or line.startswith("---"):
            continue
        if "usage location(s) found" in line:
            continue
        bracket_idx = line.find(" [L")
        if bracket_idx < 0:
            continue
        path = normalize_path(line[:bracket_idx])
        if path and path not in seen:
            seen.add(path)
            files.append(path)
            if len(files) >= top_k:
                break
    return files


def extract_codixing_search_files(output: str, top_k: int) -> list[str]:
    """Extract unique file paths from `codixing search --json` output."""
    try:
        rows = json.loads(output)
    except json.JSONDecodeError:
        return []

    if not isinstance(rows, list):
        return []

    seen: set[str] = set()
    files: list[str] = []
    for row in rows:
        if not isinstance(row, dict):
            continue
        path = normalize_path(str(row.get("file_path", row.get("file", ""))))
        if path and path not in seen:
            seen.add(path)
            files.append(path)
            if len(files) >= top_k:
                break
    return files


def extract_codixing_cross_import_files(output: str, top_k: int) -> list[str]:
    """Extract source file paths from `codixing cross-imports` output."""
    files: list[str] = []
    seen: set[str] = set()
    for line in output.splitlines():
        stripped = line.strip()
        if not stripped or "file(s) in" in stripped or "import from" in stripped:
            continue
        path = normalize_path(stripped.split(" (score:")[0])
        if path and path not in seen:
            seen.add(path)
            files.append(path)
            if len(files) >= top_k:
                break
    return files


def project_name(repo: Path) -> str:
    """Derive codebase-memory-mcp's default project name from an absolute path."""
    return str(repo).strip("/").replace("/", "-")


def extract_codebase_memory_files(output: str, top_k: int) -> list[str]:
    """Extract file paths from codebase-memory-mcp CLI JSON envelopes."""
    try:
        envelope = json.loads(output)
    except json.JSONDecodeError:
        return []

    payloads: list[Any] = []
    for item in envelope.get("content", []):
        if not isinstance(item, dict):
            continue
        text = item.get("text", "")
        if not isinstance(text, str):
            continue
        try:
            payloads.append(json.loads(text))
        except json.JSONDecodeError:
            payloads.append(text)

    seen: set[str] = set()
    files: list[str] = []

    def add(path: Any) -> None:
        if len(files) >= top_k or not isinstance(path, str):
            return
        candidate = normalize_path(path)
        if not candidate or candidate in seen:
            return
        seen.add(candidate)
        files.append(candidate)

    def walk(value: Any) -> None:
        if len(files) >= top_k:
            return
        if isinstance(value, dict):
            for key in ("file_path", "file", "path"):
                add(value.get(key))
            for child in value.values():
                walk(child)
        elif isinstance(value, list):
            for child in value:
                walk(child)

    for payload in payloads:
        walk(payload)
        if len(files) >= top_k:
            break

    return files


def extract_tool_files(tool: ToolConfig, query: dict[str, Any], output: str, top_k: int) -> list[str]:
    if tool.name == "codixing":
        category = query.get("category", "concept")
        if category == "symbol":
            return extract_codixing_symbol_files(output, query, top_k)
        if category == "usage":
            return extract_codixing_usage_files(output, top_k)
        if category == "concept":
            return extract_codixing_search_files(output, top_k)
        if category == "cross-package":
            return extract_codixing_cross_import_files(output, top_k)

    if tool.name == "codebase-memory-mcp":
        return extract_codebase_memory_files(output, top_k)

    return extract_files(output, tool.path_regex, top_k)


def score_files(files: list[str], ground_truth: list[str]) -> tuple[float, float, float]:
    def matches(candidate: str, truth: str) -> bool:
        return candidate == truth or candidate.endswith(truth) or truth.endswith(candidate)

    top = files[:10]
    hit_flags = [any(matches(candidate, gt) for gt in ground_truth) for candidate in top]
    precision = sum(hit_flags) / len(top) if top else 0.0

    found_truth = set()
    for candidate in top:
        for gt in ground_truth:
            if matches(candidate, gt):
                found_truth.add(gt)
    recall = len(found_truth) / len(ground_truth) if ground_truth else 0.0

    mrr = 0.0
    for idx, hit in enumerate(hit_flags, start=1):
        if hit:
            mrr = 1.0 / idx
            break

    return round(precision, 3), round(recall, 3), round(mrr, 3)


def run_command(command: str, repo: Path, timeout_secs: int) -> tuple[int, str, str, int]:
    start = time.perf_counter_ns()
    proc = subprocess.run(
        command,
        shell=True,
        cwd=repo,
        text=True,
        capture_output=True,
        timeout=timeout_secs,
        env={**os.environ, "CODEX_ROOT": str(ROOT)},
    )
    elapsed_ms = int((time.perf_counter_ns() - start) / 1_000_000)
    return proc.returncode, proc.stdout, proc.stderr, elapsed_ms


def run_tool_query(tool: ToolConfig, query: dict[str, Any], repo: Path, top_k: int, dry_run: bool) -> QueryResult:
    category = query.get("category", "concept")
    template = tool.commands.get(category) or tool.commands.get("default")
    if not template:
        return QueryResult(
            tool=tool.name,
            query=query.get("name", ""),
            category=category,
            command="",
            elapsed_ms=0,
            output_bytes=0,
            returncode=-1,
            files=[],
            precision_at_10=0.0,
            recall_at_10=0.0,
            mrr=0.0,
            error=f"no command template for category '{category}'",
        )

    command = render_command(template, query, repo, top_k)
    if dry_run:
        return QueryResult(
            tool=tool.name,
            query=query.get("name", ""),
            category=category,
            command=command,
            elapsed_ms=0,
            output_bytes=0,
            returncode=0,
            files=[],
            precision_at_10=0.0,
            recall_at_10=0.0,
            mrr=0.0,
            error="dry-run",
        )

    try:
        rc, stdout, stderr, elapsed_ms = run_command(command, repo, tool.timeout_secs)
        combined_output = stdout + stderr
        files = extract_tool_files(tool, query, stdout, top_k)
        precision, recall, mrr = score_files(files, query.get("ground_truth", []))
        return QueryResult(
            tool=tool.name,
            query=query.get("name", ""),
            category=category,
            command=command,
            elapsed_ms=elapsed_ms,
            output_bytes=len(combined_output.encode("utf-8", errors="replace")),
            returncode=rc,
            files=files,
            precision_at_10=precision,
            recall_at_10=recall,
            mrr=mrr,
            error="" if rc == 0 or files else f"command exited {rc}",
        )
    except subprocess.TimeoutExpired:
        return QueryResult(
            tool=tool.name,
            query=query.get("name", ""),
            category=category,
            command=command,
            elapsed_ms=tool.timeout_secs * 1000,
            output_bytes=0,
            returncode=-1,
            files=[],
            precision_at_10=0.0,
            recall_at_10=0.0,
            mrr=0.0,
            error="timeout",
        )


def summarize(results: list[QueryResult]) -> dict[str, dict[str, float | int]]:
    summary: dict[str, dict[str, float | int]] = {}
    for tool in sorted({r.tool for r in results}):
        rows = [r for r in results if r.tool == tool and r.error != "dry-run"]
        if not rows:
            continue
        n = len(rows)
        summary[tool] = {
            "queries": n,
            "avg_precision_at_10": round(sum(r.precision_at_10 for r in rows) / n, 3),
            "avg_recall_at_10": round(sum(r.recall_at_10 for r in rows) / n, 3),
            "avg_mrr": round(sum(r.mrr for r in rows) / n, 3),
            "avg_ms": round(sum(r.elapsed_ms for r in rows) / n, 1),
            "avg_output_bytes": round(sum(r.output_bytes for r in rows) / n, 1),
        }
    return summary


def summarize_by_category(results: list[QueryResult]) -> dict[str, dict[str, dict[str, float | int]]]:
    summary: dict[str, dict[str, dict[str, float | int]]] = {}
    categories = sorted({r.category for r in results})
    tools = sorted({r.tool for r in results})
    for category in categories:
        summary[category] = {}
        for tool in tools:
            rows = [
                r
                for r in results
                if r.category == category and r.tool == tool and r.error != "dry-run"
            ]
            if not rows:
                continue
            n = len(rows)
            summary[category][tool] = {
                "queries": n,
                "avg_recall_at_10": round(sum(r.recall_at_10 for r in rows) / n, 3),
                "avg_mrr": round(sum(r.mrr for r in rows) / n, 3),
            }
    return summary


def render_markdown(report: BenchmarkReport) -> str:
    summary = summarize(report.results)
    category_summary = summarize_by_category(report.results)
    lines = [
        "# Direct Competitor Benchmark\n",
        f"**Date:** {report.date}",
        f"**Repo:** `{report.repo}`",
        f"**Queries:** `{report.queries_file}`",
        f"**Tools:** `{report.tools_file}`",
        "",
    ]

    if report.skipped_tools:
        lines.append("## Skipped Tools\n")
        for skipped in report.skipped_tools:
            lines.append(f"- `{skipped['name']}` — {skipped['reason']}")
        lines.append("")

    if report.validation:
        lines.append("## Validation\n")
        for item in report.validation:
            lines.append(f"- {item}")
        lines.append("")

    lines.append("## Summary\n")
    lines.append("| Tool | Queries | Precision@10 | Recall@10 | MRR | Avg ms | Avg output bytes |")
    lines.append("|---|---:|---:|---:|---:|---:|---:|")
    for tool, row in summary.items():
        lines.append(
            f"| {tool} | {row['queries']} | {row['avg_precision_at_10']:.3f} | "
            f"{row['avg_recall_at_10']:.3f} | {row['avg_mrr']:.3f} | "
            f"{row['avg_ms']:.1f} | {row['avg_output_bytes']:.1f} |"
        )

    if category_summary:
        lines.append("\n## Category Summary\n")
        lines.append("| Category | Tool | Queries | Recall@10 | MRR |")
        lines.append("|---|---|---:|---:|---:|")
        for category, tools in category_summary.items():
            for tool, row in tools.items():
                lines.append(
                    f"| {category} | {tool} | {row['queries']} | "
                    f"{row['avg_recall_at_10']:.3f} | {row['avg_mrr']:.3f} |"
                )

    lines.append("\n## Methodology\n")
    lines.append(
        f"- Query set: curated OpenClaw file-localization queries from `{report.queries_file}`."
    )
    lines.append(
        "- Scoring: file-level Precision@10, Recall@10, and MRR. A returned path counts as a hit when it matches a ground-truth path exactly or by suffix."
    )
    lines.append(
        "- Command execution: each tool command is rendered from `benchmarks/competitor_tools.toml` and run from the target repository root."
    )
    lines.append(
        "- Codixing routing: symbols use `codixing symbols`; usage uses `codixing usages`; concepts use `codixing search --json`; cross-package queries use `codixing cross-imports`, with an optional regex pattern only when the query opts into `cross_pattern=true`."
    )
    lines.append(
        "- External tools: disabled tools are excluded unless `--include-disabled --tool <name>` is passed and their local CLI/cache environment variables are configured."
    )
    lines.append(
        "- Limitations: this is a retrieval/localization benchmark, not an end-to-end agent task benchmark. Indexing time is recorded separately by `benchmarks/run_external_competitors.sh`."
    )

    lines.append("\n## Per Query\n")
    lines.append("| Tool | Query | Category | Recall@10 | MRR | ms | bytes | Error |")
    lines.append("|---|---|---|---:|---:|---:|---:|---|")
    for r in report.results:
        err = r.error.replace("|", "\\|")
        lines.append(
            f"| {r.tool} | {r.query} | {r.category} | {r.recall_at_10:.3f} | "
            f"{r.mrr:.3f} | {r.elapsed_ms} | {r.output_bytes} | {err} |"
        )

    return "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description="Direct competitor benchmark")
    parser.add_argument("--repo", type=Path, default=DEFAULT_REPO)
    parser.add_argument("--queries", type=Path, default=DEFAULT_QUERIES)
    parser.add_argument("--tools", type=Path, default=DEFAULT_TOOLS)
    parser.add_argument("--tool", action="append", help="Only run this tool name; repeatable")
    parser.add_argument("--category", action="append", help="Only run this query category; repeatable")
    parser.add_argument("--top-k", type=int, default=10)
    parser.add_argument("--dry-run", action="store_true", help="Print rendered commands without executing")
    parser.add_argument("--list-tools", action="store_true")
    parser.add_argument("--validate-only", action="store_true", help="Validate query fixtures and exit")
    parser.add_argument("--include-disabled", action="store_true")
    parser.add_argument("--output-prefix", default="competitor_benchmark")
    args = parser.parse_args()

    tools, skipped = load_tools(args.tools, include_disabled=args.include_disabled)
    if args.tool:
        wanted = set(args.tool)
        tools = [t for t in tools if t.name in wanted]

    if args.list_tools:
        for tool in tools:
            state = "enabled" if tool.enabled else "disabled"
            print(f"{tool.name}\t{state}\t{tool.notes}")
        return 0

    repo = args.repo.resolve()
    if not repo.exists():
        raise SystemExit(f"repo not found: {repo}")

    categories = set(args.category) if args.category else None
    queries = load_queries(args.queries, categories=categories)
    if not queries:
        raise SystemExit("no benchmark queries selected")
    validation = validate_queries(queries, repo)
    if validation:
        for issue in validation:
            print(f"[validation] {issue}")
        if args.validate_only:
            return 1
        raise SystemExit("benchmark query validation failed")
    if args.validate_only:
        print(f"validated {len(queries)} query fixture(s)")
        return 0

    report = BenchmarkReport(
        date=time.strftime("%Y-%m-%d %H:%M"),
        repo=str(repo),
        queries_file=str(args.queries),
        tools_file=str(args.tools),
        top_k=args.top_k,
        skipped_tools=skipped,
        validation=[f"validated {len(queries)} query fixture(s)"],
    )

    for tool in tools:
        print(f"[tool] {tool.name}")
        for query in queries:
            result = run_tool_query(tool, query, repo, args.top_k, args.dry_run)
            report.results.append(result)
            if args.dry_run:
                print(f"  {query['name']}: {result.command}")
            else:
                print(
                    f"  {query['name']}: R@10={result.recall_at_10:.3f} "
                    f"MRR={result.mrr:.3f} {result.elapsed_ms}ms"
                )

    if args.dry_run:
        return 0

    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    payload = {
        **asdict(report),
        "summary": summarize(report.results),
        "category_summary": summarize_by_category(report.results),
        "results": [asdict(r) for r in report.results],
    }
    json_path = RESULTS_DIR / f"{args.output_prefix}.json"
    md_path = RESULTS_DIR / f"{args.output_prefix}.md"
    json_path.write_text(json.dumps(payload, indent=2) + "\n")
    md_path.write_text(render_markdown(report))
    print(f"\nSaved {json_path}")
    print(f"Saved {md_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

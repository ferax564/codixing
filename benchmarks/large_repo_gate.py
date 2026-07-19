#!/usr/bin/env python3
"""Reproducible end-to-end large-repository performance gate.

The runner owns a synthetic repository, builds a fresh BM25-ready index, and
records machine-readable measurements for initialization, disk usage, cold and
warm search, incremental sync, and retrieval quality.  It deliberately does
not contain a checked-in performance baseline: callers must supply a result
captured on the same class of machine with ``--baseline`` before ratios can be
used as a regression or improvement gate.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import platform
import shutil
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

try:
    import resource
except ImportError:  # Windows: retain all metrics except child peak RSS/PSS.
    resource = None


SCHEMA_VERSION = 2
FIXTURE_SCHEMA = "rust-widget-v1"
FIXTURE_MARKER = ".codixing-large-repo-fixture"


@dataclass(frozen=True)
class Profile:
    file_count: int
    query_runs: int
    warmup_runs: int
    monitor_interval_ms: int
    timeout_s: int


PROFILES = {
    # Fast enough for pull requests while still exercising every measurement.
    "pr": Profile(250, 5, 2, 20, 300),
    # Main/release evidence: large enough to expose corpus-scale rewrites.
    "10k": Profile(10_000, 15, 3, 25, 1_800),
    # Scheduled evidence for the actual huge-repository regime.
    "100k": Profile(100_000, 25, 5, 50, 7_200),
}


@dataclass
class ProcessMetrics:
    command: list[str]
    wall_time_ms: float
    exit_code: int
    peak_rss_bytes: int | None
    peak_rss_source: str | None
    peak_pss_bytes: int | None
    peak_pss_source: str | None
    io_read_bytes: int | None
    io_write_bytes: int | None
    io_source: str | None
    stdout: str
    stderr: str


@dataclass(frozen=True)
class DiskEntry:
    logical_bytes: int
    mtime_ns: int
    allocated_bytes: int
    device: int
    inode: int


def percentile(values: list[float], quantile: float) -> float | None:
    """Return a linearly interpolated percentile without third-party modules."""
    if not values:
        return None
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    rank = (len(ordered) - 1) * quantile
    lower = math.floor(rank)
    upper = math.ceil(rank)
    if lower == upper:
        return ordered[lower]
    return ordered[lower] + (ordered[upper] - ordered[lower]) * (rank - lower)


def latency_summary(values: list[float]) -> dict[str, Any]:
    median = statistics.median(values) if values else None
    return {
        "runs": len(values),
        "min_ms": min(values) if values else None,
        "p50_ms": median,
        "median_ms": median,
        "p95_ms": percentile(values, 0.95),
        "max_ms": max(values) if values else None,
        "samples_ms": values,
    }


def _linux_process_stats(
    pid: int,
) -> tuple[int | None, int | None, int | None, int | None]:
    rss = rss_high_water = pss = read_bytes = write_bytes = None
    proc = Path("/proc") / str(pid)
    try:
        for line in (proc / "status").read_text().splitlines():
            if line.startswith("VmRSS:"):
                rss = int(line.split()[1]) * 1024
            elif line.startswith("VmHWM:"):
                rss_high_water = int(line.split()[1]) * 1024
    except (FileNotFoundError, PermissionError, ProcessLookupError):
        pass
    try:
        for line in (proc / "smaps_rollup").read_text().splitlines():
            if line.startswith("Pss:"):
                pss = int(line.split()[1]) * 1024
                break
    except (FileNotFoundError, PermissionError, ProcessLookupError):
        pass
    try:
        for line in (proc / "io").read_text().splitlines():
            key, value = line.split(":", 1)
            if key == "read_bytes":
                read_bytes = int(value)
            elif key == "write_bytes":
                write_bytes = int(value)
    except (FileNotFoundError, PermissionError, ProcessLookupError, ValueError):
        pass
    return rss_high_water or rss, pss, read_bytes, write_bytes


def _rusage_peak_rss_bytes() -> int | None:
    if resource is None:
        return None
    try:
        raw = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
    except (AttributeError, ValueError):
        return None
    # macOS reports bytes; Linux and the other supported Unix targets report KiB.
    return int(raw if sys.platform == "darwin" else raw * 1024)


def run_measured(
    command: list[str],
    *,
    cwd: Path,
    timeout_s: int,
    monitor_interval_ms: int,
    env: dict[str, str] | None = None,
) -> ProcessMetrics:
    started = time.perf_counter()
    # Files avoid pipe-buffer deadlocks on verbose 100K-file runs. A dedicated
    # waiter captures the actual process completion time, so the monitor's poll
    # interval does not inflate short query latencies.
    with (
        tempfile.TemporaryFile(mode="w+t") as stdout_file,
        tempfile.TemporaryFile(mode="w+t") as stderr_file,
    ):
        proc = subprocess.Popen(
            command,
            cwd=cwd,
            env=env,
            stdout=stdout_file,
            stderr=stderr_file,
            text=True,
        )
        finished = threading.Event()
        finished_at: list[float] = []

        def wait_for_process() -> None:
            proc.wait()
            finished_at.append(time.perf_counter())
            finished.set()

        waiter = threading.Thread(target=wait_for_process, daemon=True)
        waiter.start()
        peak_rss = peak_pss = max_read = max_write = None
        deadline = time.monotonic() + timeout_s
        while not finished.is_set():
            if sys.platform.startswith("linux"):
                rss, pss, read_bytes, write_bytes = _linux_process_stats(proc.pid)
                if rss is not None:
                    peak_rss = max(peak_rss or 0, rss)
                if pss is not None:
                    peak_pss = max(peak_pss or 0, pss)
                if read_bytes is not None:
                    max_read = max(max_read or 0, read_bytes)
                if write_bytes is not None:
                    max_write = max(max_write or 0, write_bytes)
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                proc.kill()
                finished.wait(10)
                stdout_file.seek(0)
                stderr_file.seek(0)
                raise TimeoutError(
                    f"command exceeded {timeout_s}s: {' '.join(command)}\n"
                    f"{stdout_file.read()}\n{stderr_file.read()}"
                )
            finished.wait(min(monitor_interval_ms / 1000, remaining))
        waiter.join()
        stdout_file.seek(0)
        stderr_file.seek(0)
        stdout = stdout_file.read()
        stderr = stderr_file.read()

    if not sys.platform.startswith("linux"):
        peak_rss = _rusage_peak_rss_bytes()
    result = ProcessMetrics(
        command=command,
        wall_time_ms=(
            (finished_at[0] if finished_at else time.perf_counter()) - started
        )
        * 1000,
        exit_code=proc.returncode,
        peak_rss_bytes=peak_rss,
        peak_rss_source=(
            "linux_proc_status_vmhwm_poll"
            if sys.platform.startswith("linux")
            else "posix_child_rusage_high_water"
            if peak_rss is not None
            else None
        ),
        peak_pss_bytes=peak_pss,
        peak_pss_source="linux_smaps_rollup_poll" if peak_pss is not None else None,
        io_read_bytes=max_read,
        io_write_bytes=max_write,
        io_source=(
            "linux_proc_pid_io_poll"
            if max_read is not None or max_write is not None
            else None
        ),
        stdout=stdout[-8_192:],
        stderr=stderr[-8_192:],
    )
    if result.exit_code != 0:
        raise RuntimeError(
            f"command failed ({result.exit_code}): {' '.join(command)}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result


def generate_fixture(root: Path, file_count: int) -> list[Path]:
    root.mkdir(parents=True, exist_ok=False)
    (root / FIXTURE_MARKER).write_text("owned by benchmarks/large_repo_gate.py\n")
    files: list[Path] = []
    for index in range(file_count):
        shard = root / "src" / f"shard_{index // 1_000:04d}"
        shard.mkdir(parents=True, exist_ok=True)
        path = shard / f"widget_{index:06d}.rs"
        path.write_text(
            "\n".join(
                [
                    f"//! Synthetic widget module {index}.",
                    f"pub struct Widget{index} {{ pub id: u64, pub active: bool }}",
                    f"impl Widget{index} {{",
                    "    pub fn new(id: u64) -> Self { Self { id, active: true } }",
                    f"    pub fn unique_target_{index:06d}(&self) -> u64 {{ self.id + {index} }}",
                    "}",
                    f"pub fn process_widget_{index:06d}(widget: &Widget{index}) -> bool {{",
                    "    widget.active && widget.id > 0",
                    "}",
                    "",
                ]
            )
        )
        files.append(path)
    return files


def disk_snapshot(root: Path) -> dict[str, DiskEntry]:
    if not root.exists():
        return {}
    snapshot: dict[str, DiskEntry] = {}
    for path in root.rglob("*"):
        if path.is_file():
            stat = path.stat()
            blocks = getattr(stat, "st_blocks", None)
            allocated = stat.st_size if blocks is None else blocks * 512
            snapshot[str(path.relative_to(root))] = DiskEntry(
                logical_bytes=stat.st_size,
                mtime_ns=stat.st_mtime_ns,
                allocated_bytes=allocated,
                device=stat.st_dev,
                inode=stat.st_ino,
            )
    return snapshot


def artifact_category(relative: str) -> str:
    parts = Path(relative).parts
    if len(parts) >= 3 and parts[0] == "generations":
        return parts[2]
    return parts[0]


def disk_usage(snapshot: dict[str, DiskEntry]) -> dict[str, Any]:
    artifacts: dict[str, int] = {}
    artifacts_allocated: dict[str, int] = {}
    seen_inodes: set[tuple[int, int]] = set()
    unique_logical = 0
    allocated = 0
    for relative, entry in sorted(snapshot.items()):
        category = artifact_category(relative)
        artifacts[category] = artifacts.get(category, 0) + entry.logical_bytes
        inode = (entry.device, entry.inode)
        if inode in seen_inodes:
            continue
        seen_inodes.add(inode)
        unique_logical += entry.logical_bytes
        allocated += entry.allocated_bytes
        artifacts_allocated[category] = (
            artifacts_allocated.get(category, 0) + entry.allocated_bytes
        )
    logical = sum(entry.logical_bytes for entry in snapshot.values())
    return {
        "total_bytes": logical,
        "unique_inode_logical_bytes": unique_logical,
        "allocated_bytes": allocated,
        "hardlink_duplicate_logical_bytes": logical - unique_logical,
        "file_count": len(snapshot),
        "unique_inode_count": len(seen_inodes),
        "artifacts_bytes": dict(
            sorted(artifacts.items(), key=lambda item: (-item[1], item[0]))
        ),
        "artifacts_allocated_bytes": dict(
            sorted(artifacts_allocated.items(), key=lambda item: (-item[1], item[0]))
        ),
    }


def rewritten_bytes_estimate(
    before: dict[str, DiskEntry], after: dict[str, DiskEntry]
) -> int:
    """Estimate logical artifact bytes rewritten from size/mtime changes.

    The process-level write counter is authoritative on Linux.  This portable
    estimate remains useful on macOS and Windows, and is named as an estimate in
    the output rather than pretending to be physical I/O.
    """
    rewritten = 0
    seen: set[tuple[int, int]] = set()
    for relative, current in after.items():
        if before.get(relative) != current:
            inode = (current.device, current.inode)
            if inode not in seen:
                seen.add(inode)
                rewritten += current.logical_bytes
    for relative, previous in before.items():
        if relative not in after:
            inode = (previous.device, previous.inode)
            if inode not in seen:
                seen.add(inode)
                rewritten += previous.logical_bytes
    return rewritten


def fixture_schema_hash() -> str:
    return hashlib.sha256(FIXTURE_SCHEMA.encode()).hexdigest()


def cpu_model() -> str | None:
    if sys.platform.startswith("linux"):
        try:
            for line in Path("/proc/cpuinfo").read_text().splitlines():
                if line.startswith("model name"):
                    return line.split(":", 1)[1].strip()
        except (FileNotFoundError, PermissionError, IndexError):
            pass
    if sys.platform == "darwin":
        try:
            completed = subprocess.run(
                ["sysctl", "-n", "machdep.cpu.brand_string"],
                check=True,
                capture_output=True,
                text=True,
                timeout=5,
            )
            return completed.stdout.strip() or None
        except (OSError, subprocess.SubprocessError):
            pass
    return platform.processor() or None


def filesystem_metadata(path: Path) -> dict[str, int]:
    stat = path.stat()
    statvfs = os.statvfs(path)
    return {
        "device": stat.st_dev,
        "block_size": statvfs.f_bsize,
        "fragment_size": statvfs.f_frsize,
        "name_max": statvfs.f_namemax,
    }


def _parse_search_json(stdout: str) -> list[dict[str, Any]]:
    parsed = json.loads(stdout)
    if isinstance(parsed, list):
        return parsed
    if isinstance(parsed, dict) and isinstance(parsed.get("results"), list):
        return parsed["results"]
    if isinstance(parsed, dict):
        return [parsed]
    raise ValueError("unexpected search JSON shape")


def reciprocal_rank(results: list[dict[str, Any]], expected_file: str) -> float:
    normalized = expected_file.replace("\\", "/")
    for rank, item in enumerate(results, start=1):
        candidate = str(item.get("file", item.get("file_path", ""))).replace("\\", "/")
        if candidate.endswith(normalized):
            return 1.0 / rank
    return 0.0


def load_quality_cases(path: Path | None, file_count: int) -> list[dict[str, str]]:
    if path is not None:
        raw = json.loads(path.read_text())
        if not isinstance(raw, list) or not raw:
            raise ValueError("quality file must be a non-empty JSON array")
        cases = []
        for item in raw:
            if (
                not isinstance(item, dict)
                or "query" not in item
                or "expected_file" not in item
            ):
                raise ValueError("each quality case needs query and expected_file")
            cases.append(
                {
                    "query": str(item["query"]),
                    "expected_file": str(item["expected_file"]),
                    "strategy": str(item.get("strategy", "exact")),
                }
            )
        return cases

    targets = sorted({0, file_count // 2, file_count - 1})
    return [
        {
            "query": f"unique_target_{target:06d}",
            "expected_file": (
                f"src/shard_{target // 1_000:04d}/widget_{target:06d}.rs"
            ),
            "strategy": "exact",
        }
        for target in targets
    ]


def load_external_quality(path: Path | None) -> dict[str, Any] | None:
    """Load a normalized result from a representative retrieval evaluation.

    This hook lets release automation carry MRR/Recall evidence from an actual
    repository task set without coupling the system-performance runner to one
    particular evaluation harness.
    """
    if path is None:
        return None
    raw = json.loads(path.read_text())
    if not isinstance(raw, dict):
        raise ValueError("external quality result must be a JSON object")
    mrr = raw.get("mrr")
    recall = raw.get("recall_at_10")
    if not isinstance(mrr, (int, float)) or not 0 <= float(mrr) <= 1:
        raise ValueError("external quality result needs mrr in [0, 1]")
    if not isinstance(recall, (int, float)) or not 0 <= float(recall) <= 1:
        raise ValueError("external quality result needs recall_at_10 in [0, 1]")
    return {
        "mrr": float(mrr),
        "recall_at_10": float(recall),
        "source": str(raw.get("source", path)),
        "task_count": raw.get("task_count"),
    }


def source_metadata(root: Path, codixing: Path) -> dict[str, Any]:
    def capture(command: list[str]) -> tuple[int, str]:
        completed = subprocess.run(
            command,
            cwd=root,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            timeout=10,
            check=False,
        )
        return completed.returncode, completed.stdout.strip()

    version_code, version = capture([str(codixing), "--version"])
    revision_code, revision = capture(["git", "rev-parse", "HEAD"])
    status_code, status = capture(["git", "status", "--porcelain"])
    return {
        "codixing_version": version if version_code == 0 else None,
        "git_revision": os.environ.get("GITHUB_SHA")
        or (revision if revision_code == 0 else None),
        "git_dirty": bool(status) if status_code == 0 else None,
    }


def cold_queries(
    codixing: Path,
    root: Path,
    cases: list[dict[str, str]],
    runs: int,
    profile: Profile,
) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    latencies: list[float] = []
    peak_rss_values: list[int] = []
    peak_pss_values: list[int] = []
    observed: dict[int, list[dict[str, Any]]] = {}
    for run in range(runs):
        case_index = run % len(cases)
        case = cases[case_index]
        measured = run_measured(
            [
                str(codixing),
                "search",
                case["query"],
                "--strategy",
                case["strategy"],
                "--limit",
                "10",
                "--json",
            ],
            cwd=root,
            timeout_s=profile.timeout_s,
            monitor_interval_ms=profile.monitor_interval_ms,
        )
        latencies.append(measured.wall_time_ms)
        if measured.peak_rss_bytes is not None:
            peak_rss_values.append(measured.peak_rss_bytes)
        if measured.peak_pss_bytes is not None:
            peak_pss_values.append(measured.peak_pss_bytes)
        observed.setdefault(case_index, _parse_search_json(measured.stdout))

    quality = []
    for index, case in enumerate(cases):
        # Ensure every quality hook is evaluated even when query_runs is lower
        # than the number of cases.
        if index not in observed:
            measured = run_measured(
                [
                    str(codixing),
                    "search",
                    case["query"],
                    "--strategy",
                    case["strategy"],
                    "--limit",
                    "10",
                    "--json",
                ],
                cwd=root,
                timeout_s=profile.timeout_s,
                monitor_interval_ms=profile.monitor_interval_ms,
            )
            observed[index] = _parse_search_json(measured.stdout)
        rr = reciprocal_rank(observed[index], case["expected_file"])
        quality.append({**case, "reciprocal_rank": rr, "found": rr > 0})
    summary = latency_summary(latencies)
    summary["peak_rss_bytes"] = max(peak_rss_values, default=None)
    summary["peak_pss_bytes"] = max(peak_pss_values, default=None)
    return summary, quality


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def _request_json(url: str, payload: dict[str, Any] | None = None) -> Any:
    data = None if payload is None else json.dumps(payload).encode()
    request = urllib.request.Request(
        url,
        data=data,
        headers={"Content-Type": "application/json"},
        method="GET" if payload is None else "POST",
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.loads(response.read())


def warm_queries(
    server: Path,
    root: Path,
    cases: list[dict[str, str]],
    runs: int,
    warmups: int,
    timeout_s: int,
) -> dict[str, Any]:
    port = _free_port()
    command = [str(server), "--host", "127.0.0.1", "--port", str(port), str(root)]
    process = subprocess.Popen(
        command,
        cwd=root,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    url = f"http://127.0.0.1:{port}"
    rss_samples: list[int] = []
    pss_samples: list[int] = []

    def sample_server_memory() -> None:
        if not sys.platform.startswith("linux"):
            return
        rss, pss, _, _ = _linux_process_stats(process.pid)
        if rss is not None:
            rss_samples.append(rss)
        if pss is not None:
            pss_samples.append(pss)

    deadline = time.monotonic() + min(timeout_s, 120)
    while True:
        sample_server_memory()
        if process.poll() is not None:
            stdout, stderr = process.communicate()
            raise RuntimeError(f"server exited during startup\n{stdout}\n{stderr}")
        try:
            _request_json(f"{url}/health")
            break
        except (urllib.error.URLError, ConnectionError, TimeoutError):
            if time.monotonic() > deadline:
                process.terminate()
                stdout, stderr = process.communicate(timeout=10)
                raise TimeoutError(f"server did not become ready\n{stdout}\n{stderr}")
            time.sleep(0.05)

    try:
        for index in range(warmups):
            case = cases[index % len(cases)]
            _request_json(
                f"{url}/search",
                {"query": case["query"], "limit": 10, "strategy": case["strategy"]},
            )
            sample_server_memory()
        steady_rss = rss_samples[-1] if rss_samples else None
        steady_pss = pss_samples[-1] if pss_samples else None
        client_latencies: list[float] = []
        engine_latencies: list[float] = []
        for index in range(runs):
            case = cases[index % len(cases)]
            started = time.perf_counter()
            response = _request_json(
                f"{url}/search",
                {"query": case["query"], "limit": 10, "strategy": case["strategy"]},
            )
            client_latencies.append((time.perf_counter() - started) * 1000)
            if isinstance(response, dict) and isinstance(
                response.get("elapsed_ms"), (int, float)
            ):
                engine_latencies.append(float(response["elapsed_ms"]))
            sample_server_memory()
        return {
            "client_round_trip": latency_summary(client_latencies),
            "engine_reported": latency_summary(engine_latencies),
            "server_command": command,
            "server_process": {
                "steady_rss_bytes": steady_rss,
                "steady_pss_bytes": steady_pss,
                "peak_rss_bytes": max(rss_samples, default=None),
                "peak_pss_bytes": max(pss_samples, default=None),
                "source": "linux_proc_poll" if rss_samples else None,
            },
        }
    finally:
        process.terminate()
        try:
            process.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate()


def edit_files(paths: list[Path], count: int, generation: str) -> None:
    for path in paths[:count]:
        with path.open("a") as handle:
            handle.write(f"// benchmark edit {generation}\n")


def sync_scenario(
    label: str,
    codixing: Path,
    root: Path,
    profile: Profile,
    threads: int | None,
) -> dict[str, Any]:
    index_root = root / ".codixing"
    before = disk_snapshot(index_root)
    command = [str(codixing), "sync", "--no-embed"]
    if threads is not None:
        command.extend(["--threads", str(threads)])
    command.append(".")
    measured = run_measured(
        command,
        cwd=root,
        timeout_s=profile.timeout_s,
        monitor_interval_ms=profile.monitor_interval_ms,
    )
    after = disk_snapshot(index_root)
    return {
        "label": label,
        "process": asdict(measured),
        "artifact_bytes_rewritten_estimate": rewritten_bytes_estimate(before, after),
        "disk_delta_bytes": disk_usage(after)["total_bytes"]
        - disk_usage(before)["total_bytes"],
    }


def _metric(result: dict[str, Any], dotted: str) -> float | None:
    value: Any = result
    for part in dotted.split("."):
        if not isinstance(value, dict) or part not in value:
            return None
        value = value[part]
    if isinstance(value, (int, float)):
        return float(value)
    return None


def _external_quality_identity(result: dict[str, Any]) -> tuple[Any, Any] | None:
    external = result.get("metrics", {}).get("quality", {}).get("external")
    if not isinstance(external, dict):
        return None
    return external.get("source"), external.get("task_count")


def _synthetic_quality_identity(result: dict[str, Any]) -> list[tuple[Any, Any, Any]]:
    cases = (
        result.get("metrics", {})
        .get("quality", {})
        .get("synthetic_exact", {})
        .get("cases", [])
    )
    if not isinstance(cases, list):
        return []
    return [
        (case.get("query"), case.get("expected_file"), case.get("strategy"))
        for case in cases
        if isinstance(case, dict)
    ]


def compare_to_baseline(
    current: dict[str, Any], baseline: dict[str, Any], ratios: dict[str, float]
) -> dict[str, Any]:
    compatibility_fields = [
        (
            "schema_version",
            current.get("schema_version"),
            baseline.get("schema_version"),
        ),
        (
            "profile.file_count",
            _metric(current, "profile.file_count"),
            _metric(baseline, "profile.file_count"),
        ),
        (
            "profile.query_runs",
            _metric(current, "profile.query_runs"),
            _metric(baseline, "profile.query_runs"),
        ),
        (
            "profile.warmup_runs",
            _metric(current, "profile.warmup_runs"),
            _metric(baseline, "profile.warmup_runs"),
        ),
        (
            "configuration.threads",
            current.get("configuration", {}).get("threads"),
            baseline.get("configuration", {}).get("threads"),
        ),
        (
            "host.machine",
            current.get("host", {}).get("machine"),
            baseline.get("host", {}).get("machine"),
        ),
        (
            "host.system",
            current.get("host", {}).get("system"),
            baseline.get("host", {}).get("system"),
        ),
        (
            "host.logical_cpus",
            current.get("host", {}).get("logical_cpus"),
            baseline.get("host", {}).get("logical_cpus"),
        ),
        (
            "host.kernel_release",
            current.get("host", {}).get("kernel_release"),
            baseline.get("host", {}).get("kernel_release"),
        ),
        (
            "host.cpu_model",
            current.get("host", {}).get("cpu_model"),
            baseline.get("host", {}).get("cpu_model"),
        ),
        (
            "host.filesystem.device",
            _metric(current, "host.filesystem.device"),
            _metric(baseline, "host.filesystem.device"),
        ),
        (
            "host.filesystem.block_size",
            _metric(current, "host.filesystem.block_size"),
            _metric(baseline, "host.filesystem.block_size"),
        ),
        (
            "fixture.schema_hash",
            current.get("fixture", {}).get("schema_hash"),
            baseline.get("fixture", {}).get("schema_hash"),
        ),
        (
            "metrics.quality.comparison_source",
            current.get("metrics", {}).get("quality", {}).get("comparison_source"),
            baseline.get("metrics", {}).get("quality", {}).get("comparison_source"),
        ),
        (
            "metrics.quality.synthetic_exact.cases",
            _synthetic_quality_identity(current),
            _synthetic_quality_identity(baseline),
        ),
        (
            "metrics.quality.external",
            _external_quality_identity(current),
            _external_quality_identity(baseline),
        ),
    ]
    mismatches = [
        name
        for name, current_value, baseline_value in compatibility_fields
        if current_value != baseline_value
    ]
    if mismatches:
        return {
            "status": "failed",
            "reason": "incompatible baseline fields: " + ", ".join(mismatches),
            "checks": [],
        }

    checks = []
    mapping = {
        "max_init_ratio": ("metrics.init.wall_time_ms", "max"),
        "max_rss_ratio": ("metrics.init.peak_rss_bytes", "max"),
        "max_resident_rss_ratio": (
            "metrics.queries.warm.server_process.peak_rss_bytes",
            "max",
        ),
        "max_disk_ratio": ("metrics.disk.allocated_bytes", "max"),
        "max_cold_query_p95_ratio": ("metrics.queries.cold.p95_ms", "max"),
        "max_warm_query_p95_ratio": (
            "metrics.queries.warm.client_round_trip.p95_ms",
            "max",
        ),
        "max_one_file_sync_ratio": (
            "metrics.sync.one_file.process.wall_time_ms",
            "max",
        ),
        "max_one_file_rewrite_ratio": (
            "metrics.sync.one_file.artifact_bytes_rewritten_estimate",
            "max",
        ),
        "min_quality_ratio": ("metrics.quality.mrr", "min"),
    }
    for name, limit in ratios.items():
        dotted, direction = mapping[name]
        current_value = _metric(current, dotted)
        baseline_value = _metric(baseline, dotted)
        if current_value is None or baseline_value is None or baseline_value == 0:
            checks.append(
                {
                    "name": name,
                    "metric": dotted,
                    "status": "failed",
                    "reason": "metric missing or baseline is zero",
                }
            )
            continue
        ratio = current_value / baseline_value
        passed = ratio <= limit if direction == "max" else ratio >= limit
        checks.append(
            {
                "name": name,
                "metric": dotted,
                "current": current_value,
                "baseline": baseline_value,
                "ratio": ratio,
                "limit": limit,
                "status": "passed" if passed else "failed",
            }
        )
    return {
        "status": "passed"
        if all(check["status"] == "passed" for check in checks)
        else "failed",
        "checks": checks,
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", choices=PROFILES, default="pr")
    parser.add_argument("--files", type=int, help="override the profile file count")
    parser.add_argument(
        "--query-runs", type=int, help="override measured cold/warm runs"
    )
    parser.add_argument(
        "--threads", type=int, help="pass an explicit init/sync worker cap"
    )
    parser.add_argument(
        "--codixing", type=Path, default=Path("target/release/codixing")
    )
    parser.add_argument(
        "--server", type=Path, default=Path("target/release/codixing-server")
    )
    parser.add_argument("--output", type=Path, default=Path("large-repo-gate.json"))
    parser.add_argument(
        "--work-dir", type=Path, help="parent for the generated fixture"
    )
    parser.add_argument("--keep-work-dir", action="store_true")
    parser.add_argument("--quality-file", type=Path)
    parser.add_argument(
        "--external-quality-result",
        type=Path,
        help="normalized JSON with mrr and recall_at_10 from a representative task set",
    )
    parser.add_argument("--baseline", type=Path)
    parser.add_argument("--max-init-ratio", type=float, default=1.0)
    parser.add_argument("--max-rss-ratio", type=float, default=1.0)
    parser.add_argument("--max-resident-rss-ratio", type=float, default=1.0)
    parser.add_argument("--max-disk-ratio", type=float, default=1.0)
    parser.add_argument("--max-cold-query-p95-ratio", type=float, default=1.0)
    parser.add_argument("--max-warm-query-p95-ratio", type=float, default=1.0)
    parser.add_argument("--max-one-file-sync-ratio", type=float, default=1.0)
    parser.add_argument("--max-one-file-rewrite-ratio", type=float, default=1.0)
    parser.add_argument("--min-quality-ratio", type=float, default=0.99)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    selected = PROFILES[args.profile]
    profile = Profile(
        file_count=args.files if args.files is not None else selected.file_count,
        query_runs=(
            args.query_runs if args.query_runs is not None else selected.query_runs
        ),
        warmup_runs=selected.warmup_runs,
        monitor_interval_ms=selected.monitor_interval_ms,
        timeout_s=selected.timeout_s,
    )
    if profile.file_count <= 0 or profile.query_runs <= 0:
        raise ValueError("files and query runs must be positive")
    if args.threads is not None and args.threads <= 0:
        raise ValueError("threads must be positive")

    invocation_root = Path.cwd()
    codixing = args.codixing.expanduser().resolve()
    server = args.server.expanduser().resolve()
    if not codixing.is_file() or not os.access(codixing, os.X_OK):
        raise FileNotFoundError(f"codixing binary is not executable: {codixing}")
    if not server.is_file() or not os.access(server, os.X_OK):
        raise FileNotFoundError(f"server binary is not executable: {server}")

    parent = (
        args.work_dir.expanduser().resolve()
        if args.work_dir
        else Path(tempfile.gettempdir())
    )
    parent.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="codixing-large-repo-", dir=parent))
    # mkdtemp creates the directory, while generate_fixture requires a fresh path.
    root.rmdir()
    result: dict[str, Any] = {
        "schema_version": SCHEMA_VERSION,
        "generated_at_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "profile_name": args.profile,
        "profile": asdict(profile),
        "host": {
            "system": platform.system(),
            "platform": platform.platform(),
            "machine": platform.machine(),
            "processor": platform.processor() or None,
            "cpu_model": cpu_model(),
            "kernel_release": platform.release(),
            "python": platform.python_version(),
            "logical_cpus": os.cpu_count(),
            "filesystem": filesystem_metadata(parent),
        },
        "fixture": {
            "schema": FIXTURE_SCHEMA,
            "schema_hash": fixture_schema_hash(),
        },
        "source": source_metadata(invocation_root, codixing),
        "configuration": {"threads": args.threads, "embedding_enabled": False},
        "binaries": {"codixing": str(codixing), "server": str(server)},
        "fixture_root": str(root),
        "metrics": {},
    }
    exit_code = 0
    try:
        files = generate_fixture(root, profile.file_count)
        cases = load_quality_cases(args.quality_file, profile.file_count)
        external_quality = load_external_quality(args.external_quality_result)

        init_command = [str(codixing), "init"]
        if args.threads is not None:
            init_command.extend(["--threads", str(args.threads)])
        init_command.append(".")
        init_metrics = run_measured(
            init_command,
            cwd=root,
            timeout_s=profile.timeout_s,
            monitor_interval_ms=profile.monitor_interval_ms,
        )
        result["metrics"]["init"] = asdict(init_metrics)
        source_bytes = sum(path.stat().st_size for path in files)
        result["metrics"]["source"] = {
            "file_count": len(files),
            "total_bytes": source_bytes,
        }
        steady_disk = disk_usage(disk_snapshot(root / ".codixing"))
        steady_disk["source_amplification_ratio"] = (
            steady_disk["allocated_bytes"] / source_bytes if source_bytes else None
        )
        steady_disk["logical_source_amplification_ratio"] = (
            steady_disk["total_bytes"] / source_bytes if source_bytes else None
        )
        result["metrics"]["disk"] = steady_disk

        cold, quality_cases = cold_queries(
            codixing, root, cases, profile.query_runs, profile
        )
        warm = warm_queries(
            server,
            root,
            cases,
            profile.query_runs,
            profile.warmup_runs,
            profile.timeout_s,
        )
        mrr = statistics.mean(case["reciprocal_rank"] for case in quality_cases)
        recall_at_10 = statistics.mean(
            1.0 if case["found"] else 0.0 for case in quality_cases
        )
        result["metrics"]["queries"] = {"cold": cold, "warm": warm}
        result["metrics"]["quality"] = {
            # The representative external score becomes the comparison metric
            # when supplied; synthetic exact probes remain a correctness gate.
            "mrr": external_quality["mrr"] if external_quality else mrr,
            "recall_at_10": (
                external_quality["recall_at_10"] if external_quality else recall_at_10
            ),
            "comparison_source": "external" if external_quality else "synthetic_exact",
            "synthetic_exact": {
                "mrr": mrr,
                "recall_at_10": recall_at_10,
                "cases": quality_cases,
            },
            "external": external_quality,
        }

        no_op = sync_scenario("no_op", codixing, root, profile, args.threads)
        edit_files(files, 1, "one-file")
        one_file = sync_scenario("one_file", codixing, root, profile, args.threads)
        one_percent_count = max(1, math.ceil(profile.file_count * 0.01))
        edit_files(files, one_percent_count, "one-percent")
        one_percent = sync_scenario(
            "one_percent", codixing, root, profile, args.threads
        )
        one_percent["edited_files"] = one_percent_count
        result["metrics"]["sync"] = {
            "no_op": no_op,
            "one_file": one_file,
            "one_percent": one_percent,
        }

        doctor = run_measured(
            [str(codixing), "doctor", "--json", "."],
            cwd=root,
            timeout_s=profile.timeout_s,
            monitor_interval_ms=profile.monitor_interval_ms,
        )
        result["doctor"] = json.loads(doctor.stdout)

        if recall_at_10 < 1.0:
            result["correctness_gate"] = {
                "status": "failed",
                "reason": "one or more exact synthetic quality probes were not in the top 10",
            }
            exit_code = 2
        else:
            result["correctness_gate"] = {"status": "passed"}

        if args.baseline:
            baseline = json.loads(args.baseline.read_text())
            ratios = {
                "max_init_ratio": args.max_init_ratio,
                "max_rss_ratio": args.max_rss_ratio,
                "max_resident_rss_ratio": args.max_resident_rss_ratio,
                "max_disk_ratio": args.max_disk_ratio,
                "max_cold_query_p95_ratio": args.max_cold_query_p95_ratio,
                "max_warm_query_p95_ratio": args.max_warm_query_p95_ratio,
                "max_one_file_sync_ratio": args.max_one_file_sync_ratio,
                "max_one_file_rewrite_ratio": args.max_one_file_rewrite_ratio,
                "min_quality_ratio": args.min_quality_ratio,
            }
            result["performance_gate"] = compare_to_baseline(result, baseline, ratios)
            if result["performance_gate"]["status"] != "passed":
                exit_code = 2
        else:
            result["performance_gate"] = {
                "status": "recorded",
                "reason": "no --baseline supplied; no performance claim was evaluated",
            }
    finally:
        try:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
        finally:
            if not args.keep_work_dir and root.exists():
                marker = root / FIXTURE_MARKER
                if marker.is_file():
                    shutil.rmtree(root)
                else:
                    print(
                        f"refusing to remove unmarked fixture directory: {root}",
                        file=sys.stderr,
                    )

    print(json.dumps(result, indent=2, sort_keys=True))
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())

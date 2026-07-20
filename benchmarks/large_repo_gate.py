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
from collections import deque
import hashlib
import json
import math
import os
import platform
import secrets
import shutil
import signal
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


SCHEMA_VERSION = 10
EMBEDDED_BUILD_PROVENANCE_ORIGIN = "embedded-build-v1"
LEGACY_BUILD_PROVENANCE_ORIGIN = "owned-clean-legacy-build-v1"
LEGACY_BUILD_PROVENANCE_SCHEMA_VERSION = 1
BUILD_ENVIRONMENT_KEYS = frozenset(
    {
        "AR",
        "CARGO_HOME",
        "CARGO_BUILD_RUSTFLAGS",
        "CARGO_BUILD_TARGET",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_INCREMENTAL",
        "CC",
        "CFLAGS",
        "CXX",
        "CXXFLAGS",
        "LDFLAGS",
        "MACOSX_DEPLOYMENT_TARGET",
        "RUSTC",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTC_WRAPPER",
        "RUSTFLAGS",
        "RUSTUP_TOOLCHAIN",
        "SDKROOT",
        "SOURCE_DATE_EPOCH",
    }
)
FIXTURE_SCHEMA = "rust-widget-v1"
FIXTURE_MARKER = ".codixing-large-repo-fixture"
FIXTURE_MARKER_PREFIX = "codixing-large-repo-owner:"
CAPTURE_CHUNK_SIZE = 16_384
CAPTURE_MAX_CHUNKS = 64
MAX_ONE_FILE_SYNC_REGRESSION_MS = 500.0
STRICT_CLAIM_MAXIMUMS = {
    "max_init_ratio": 1.05,
    "max_rss_ratio": 0.50,
    "max_resident_rss_ratio": 0.50,
    "max_disk_ratio": 0.50,
    "max_post_sync_disk_ratio": 0.50,
    "max_cold_query_p95_ratio": 1.05,
    "max_cold_query_rss_ratio": 0.50,
    "max_warm_query_p95_ratio": 0.50,
    "warm_query_absolute_floor_ms": 10.0,
    "max_warm_query_regression_ms": 2.0,
    "max_speed_suite_ratio": 0.50,
    "max_speed_component_ratio": 1.05,
    "max_one_file_sync_ratio": 0.50,
    "max_one_file_sync_regression_ms": 500.0,
    "max_one_file_sync_rss_ratio": 0.50,
    "max_one_file_rewrite_ratio": 0.50,
    "max_one_percent_sync_ratio": 1.05,
    "max_one_percent_sync_rss_ratio": 0.50,
    "max_one_percent_rewrite_ratio": 0.50,
    "max_no_op_rewrite_bytes": 0.0,
}
STRICT_CLAIM_MINIMUMS = {
    "min_quality_ratio": 0.99,
    "min_recall_at_10_ratio": 0.99,
    "min_quality_mrr": 0.80,
    "min_quality_recall_at_10": 0.90,
}
SYNC_REPETITIONS = 5
MEASUREMENT_SCOPE = {
    "retrieval_workload": "bm25_only_no_embeddings",
    "claim_process": "direct_measured_child",
    "linux_peak_rss_source": "linux_wait4_direct_child_ru_maxrss",
    "linux_io_source": "linux_proc_direct_child_io_final",
    "descendant_processes": "excluded",
}
SYNC_MAD_RELATIVE_LIMIT = 0.10
SYNC_MAD_ABSOLUTE_FLOOR_MS = 50.0
SYNC_IQR_RELATIVE_LIMIT = 0.20
SYNC_IQR_ABSOLUTE_FLOOR_MS = 100.0


@dataclass(frozen=True)
class Profile:
    file_count: int
    query_runs: int
    warmup_runs: int
    monitor_interval_ms: int
    timeout_s: int


PROFILES = {
    # Fast smoke/manual profile. Fixed process and RSS floors make it too small
    # for strict 2x/50% acceptance claims; CI pull requests use 10K instead.
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
    memory_complete: bool
    io_complete: bool
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


def sync_latency_summary(values: list[float]) -> dict[str, Any]:
    """Summarize repeated sync timings and reject materially noisy samples."""
    summary = latency_summary(values)
    median = summary["median_ms"]
    if median is None:
        summary.update(
            {
                "mad_ms": None,
                "relative_mad": None,
                "relative_mad_limit": SYNC_MAD_RELATIVE_LIMIT,
                "absolute_jitter_floor_ms": SYNC_MAD_ABSOLUTE_FLOOR_MS,
                "jitter_limit_ms": None,
                "iqr_ms": None,
                "relative_iqr": None,
                "iqr_relative_limit": SYNC_IQR_RELATIVE_LIMIT,
                "iqr_absolute_floor_ms": SYNC_IQR_ABSOLUTE_FLOOR_MS,
                "iqr_limit_ms": None,
                "stable": False,
            }
        )
        return summary

    mad = statistics.median(abs(value - median) for value in values)
    jitter_limit = max(SYNC_MAD_ABSOLUTE_FLOOR_MS, median * SYNC_MAD_RELATIVE_LIMIT)
    lower_quartile = percentile(values, 0.25)
    upper_quartile = percentile(values, 0.75)
    assert lower_quartile is not None and upper_quartile is not None
    iqr = upper_quartile - lower_quartile
    iqr_limit = max(SYNC_IQR_ABSOLUTE_FLOOR_MS, median * SYNC_IQR_RELATIVE_LIMIT)
    summary.update(
        {
            "mad_ms": mad,
            "relative_mad": mad / median if median > 0 else None,
            "relative_mad_limit": SYNC_MAD_RELATIVE_LIMIT,
            "absolute_jitter_floor_ms": SYNC_MAD_ABSOLUTE_FLOOR_MS,
            "jitter_limit_ms": jitter_limit,
            "iqr_ms": iqr,
            "relative_iqr": iqr / median if median > 0 else None,
            "iqr_relative_limit": SYNC_IQR_RELATIVE_LIMIT,
            "iqr_absolute_floor_ms": SYNC_IQR_ABSOLUTE_FLOOR_MS,
            "iqr_limit_ms": iqr_limit,
            "stable": mad <= jitter_limit and iqr <= iqr_limit,
        }
    )
    return summary


def _linux_process_stats_at(
    pid: int,
    proc_root: Path,
    *,
    prefer_high_water: bool,
) -> tuple[int | None, int | None, int | None, int | None]:
    rss = rss_high_water = pss = read_bytes = write_bytes = None
    proc = proc_root / str(pid)
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
    selected_rss = rss_high_water or rss if prefer_high_water else rss
    return selected_rss, pss, read_bytes, write_bytes


def _linux_process_stats(
    pid: int,
) -> tuple[int | None, int | None, int | None, int | None]:
    return _linux_process_stats_at(pid, Path("/proc"), prefer_high_water=True)


def _linux_process_io(
    pid: int, proc_root: Path = Path("/proc")
) -> tuple[int | None, int | None]:
    read_bytes = write_bytes = None
    try:
        for line in (proc_root / str(pid) / "io").read_text().splitlines():
            key, value = line.split(":", 1)
            if key == "read_bytes":
                read_bytes = int(value)
            elif key == "write_bytes":
                write_bytes = int(value)
    except (FileNotFoundError, PermissionError, ProcessLookupError, ValueError):
        pass
    return read_bytes, write_bytes


def _linux_process_tree_pids(pid: int, proc_root: Path = Path("/proc")) -> set[int]:
    """Return live descendants or reparented members of the root process group."""
    parents: dict[int, int] = {}
    process_groups: dict[int, int] = {}
    try:
        entries = list(proc_root.iterdir())
    except OSError:
        return {pid}
    for entry in entries:
        if not entry.name.isdigit():
            continue
        try:
            child = int(entry.name)
            lines = (entry / "status").read_text().splitlines()
            parent = next(
                int(line.split(":", 1)[1]) for line in lines if line.startswith("PPid:")
            )
            group = next(
                (
                    int(line.split(":", 1)[1].split()[-1])
                    for line in lines
                    if line.startswith("NSpgid:")
                ),
                None,
            )
        except (OSError, StopIteration, ValueError):
            continue
        parents[child] = parent
        if group is not None:
            process_groups[child] = group

    descendants = {pid} | {
        child for child, process_group in process_groups.items() if process_group == pid
    }
    changed = True
    while changed:
        changed = False
        for child, parent in parents.items():
            if parent in descendants and child not in descendants:
                descendants.add(child)
                changed = True
    return descendants


def _linux_process_tree_stats(
    pid: int, proc_root: Path = Path("/proc")
) -> tuple[int | None, int | None, int | None, int | None]:
    """Sample current RSS/PSS and cumulative I/O for a live process tree."""
    rss, pss, read_bytes, write_bytes, _, _ = _linux_process_tree_snapshot(
        pid, proc_root
    )
    return rss, pss, read_bytes, write_bytes


def _linux_process_tree_snapshot(
    pid: int, proc_root: Path = Path("/proc")
) -> tuple[
    int | None,
    int | None,
    int | None,
    int | None,
    set[int],
    bool,
]:
    """Sample a tree and report whether every member supplied both I/O counters."""
    rss_values: list[int] = []
    pss_values: list[int] = []
    read_values: list[int] = []
    write_values: list[int] = []
    tree_pids = _linux_process_tree_pids(pid, proc_root)
    io_complete = bool(tree_pids)
    for tree_pid in tree_pids:
        rss, pss, read_bytes, write_bytes = _linux_process_stats_at(
            tree_pid, proc_root, prefer_high_water=True
        )
        if rss is not None:
            rss_values.append(rss)
        if pss is not None:
            pss_values.append(pss)
        if read_bytes is not None:
            read_values.append(read_bytes)
        else:
            io_complete = False
        if write_bytes is not None:
            write_values.append(write_bytes)
        else:
            io_complete = False
    return (
        sum(rss_values) if rss_values else None,
        sum(pss_values) if pss_values else None,
        sum(read_values) if read_values else None,
        sum(write_values) if write_values else None,
        tree_pids,
        io_complete,
    )


def _darwin_process_rss_bytes(pid: int) -> int | None:
    """Read one live macOS process's resident set via the system ``ps``."""
    try:
        measured = subprocess.run(
            ["ps", "-o", "rss=", "-p", str(pid)],
            capture_output=True,
            text=True,
            check=False,
            timeout=2,
        )
        if measured.returncode != 0:
            return None
        value = measured.stdout.strip().splitlines()
        return int(value[0].strip()) * 1024 if value else None
    except (OSError, subprocess.TimeoutExpired, ValueError):
        return None


def _live_process_memory_stats(
    pid: int,
) -> tuple[int | None, int | None, str | None]:
    """Return live RSS/PSS and a stable source label for supported hosts."""
    if sys.platform.startswith("linux"):
        rss, pss, _, _ = _linux_process_stats(pid)
        return rss, pss, "linux_proc_direct_child_poll" if rss is not None else None
    if sys.platform == "darwin":
        rss = _darwin_process_rss_bytes(pid)
        return rss, None, "darwin_ps_rss_poll" if rss is not None else None
    return None, None, None


def _start_pipe_drain(stream: Any) -> tuple[deque[str], threading.Thread]:
    """Drain one child pipe without unbounded memory or benchmark-disk writes."""
    chunks: deque[str] = deque(maxlen=CAPTURE_MAX_CHUNKS)

    def drain() -> None:
        while True:
            chunk = stream.read(CAPTURE_CHUNK_SIZE)
            if not chunk:
                break
            chunks.append(chunk)

    thread = threading.Thread(target=drain, daemon=True)
    thread.start()
    return chunks, thread


def _captured_tail(chunks: deque[str]) -> str:
    return "".join(chunks)[-CAPTURE_CHUNK_SIZE * CAPTURE_MAX_CHUNKS :]


def _terminate_process_group(proc: subprocess.Popen[str], *, force: bool) -> None:
    """Stop the owned child session, including helpers that reparented."""
    if os.name == "posix":
        try:
            os.killpg(proc.pid, signal.SIGKILL if force else signal.SIGTERM)
            return
        except ProcessLookupError:
            return
    if proc.poll() is not None:
        return
    proc.kill() if force else proc.terminate()


def run_measured(
    command: list[str],
    *,
    cwd: Path,
    timeout_s: int,
    monitor_interval_ms: int,
    env: dict[str, str] | None = None,
) -> ProcessMetrics:
    started = time.perf_counter()
    # Dedicated drainers avoid both pipe deadlocks and child-attributed disk I/O.
    # A waiter captures exact completion time so polling does not inflate latency.
    # On Linux, waitid(WNOWAIT) deliberately leaves the exited child available in
    # /proc until the main thread has captured its final cumulative counters.
    proc = subprocess.Popen(
        command,
        cwd=cwd,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=os.name == "posix",
    )
    assert proc.stdout is not None and proc.stderr is not None
    stdout_chunks, stdout_drain = _start_pipe_drain(proc.stdout)
    stderr_chunks, stderr_drain = _start_pipe_drain(proc.stderr)

    def collect_and_close_output() -> tuple[str, str]:
        stdout_drain.join(timeout=10)
        stderr_drain.join(timeout=10)
        stdout = _captured_tail(stdout_chunks)
        stderr = _captured_tail(stderr_chunks)
        proc.stdout.close()
        proc.stderr.close()
        return stdout, stderr

    finished = threading.Event()
    finished_at: list[float] = []
    linux_exit_held_for_sampling: list[bool] = []
    is_linux = sys.platform.startswith("linux")

    def wait_for_process() -> None:
        if is_linux:
            try:
                os.waitid(os.P_PID, proc.pid, os.WEXITED | os.WNOWAIT)
                linux_exit_held_for_sampling.append(True)
            except (AttributeError, ChildProcessError, OSError):
                # Old/non-conforming Python or kernels remain usable, but their
                # metrics are explicitly incomplete and cannot support claims.
                proc.wait()
                linux_exit_held_for_sampling.append(False)
        else:
            proc.wait()
        finished_at.append(time.perf_counter())
        finished.set()

    waiter = threading.Thread(target=wait_for_process, daemon=True)
    waiter.start()
    peak_rss = peak_pss = max_read = max_write = None
    deadline = time.monotonic() + timeout_s
    while not finished.is_set():
        if sys.platform == "darwin":
            rss = _darwin_process_rss_bytes(proc.pid)
            if rss is not None:
                peak_rss = max(peak_rss or 0, rss)
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            _terminate_process_group(proc, force=True)
            finished.wait(10)
            waiter.join(timeout=10)
            if linux_exit_held_for_sampling == [True]:
                try:
                    _, status, _ = os.wait4(proc.pid, 0)
                    proc.returncode = os.waitstatus_to_exitcode(status)
                except (ChildProcessError, OSError):
                    proc.wait()
            stdout, stderr = collect_and_close_output()
            raise TimeoutError(
                f"command exceeded {timeout_s}s: {' '.join(command)}\n"
                f"{stdout}\n{stderr}"
            )
        finished.wait(min(monitor_interval_ms / 1000, remaining))
    waiter.join()

    memory_complete = False
    io_complete = False
    if is_linux and linux_exit_held_for_sampling == [True]:
        direct_read, direct_write = _linux_process_io(proc.pid)

        # The claim metric is deliberately scoped to the measured direct child.
        # Descendants are excluded because aggregating short-lived helpers from
        # /proc cannot be proven complete without perturbing timed operations.
        _terminate_process_group(proc, force=True)
        exact_child_peak_rss = None
        try:
            _, status, usage = os.wait4(proc.pid, 0)
            proc.returncode = os.waitstatus_to_exitcode(status)
            exact_child_peak_rss = int(usage.ru_maxrss) * 1024
        except (ChildProcessError, OSError):
            proc.wait()
        if exact_child_peak_rss is not None:
            peak_rss = exact_child_peak_rss
        max_read = direct_read
        max_write = direct_write
        memory_complete = exact_child_peak_rss is not None
        io_complete = direct_read is not None and direct_write is not None
    else:
        _terminate_process_group(proc, force=True)

    stdout, stderr = collect_and_close_output()

    result = ProcessMetrics(
        command=command,
        wall_time_ms=(
            (finished_at[0] if finished_at else time.perf_counter()) - started
        )
        * 1000,
        exit_code=proc.returncode,
        peak_rss_bytes=peak_rss,
        peak_rss_source=(
            "linux_wait4_direct_child_ru_maxrss"
            if peak_rss is not None and is_linux and memory_complete
            else "darwin_ps_rss_poll"
            if peak_rss is not None and sys.platform == "darwin"
            else None
        ),
        peak_pss_bytes=peak_pss,
        peak_pss_source=(
            "linux_proc_direct_child_pss_poll" if peak_pss is not None else None
        ),
        io_read_bytes=max_read,
        io_write_bytes=max_write,
        io_source=(
            "linux_proc_direct_child_io_final"
            if (max_read is not None or max_write is not None) and io_complete
            else "linux_proc_direct_child_io_final_incomplete"
            if max_read is not None or max_write is not None
            else None
        ),
        memory_complete=memory_complete,
        io_complete=io_complete,
        stdout=stdout,
        stderr=stderr,
    )
    if result.exit_code != 0:
        raise RuntimeError(
            f"command failed ({result.exit_code}): {' '.join(command)}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result


def fixture_marker_contents(ownership_nonce: str) -> str:
    return f"{FIXTURE_MARKER_PREFIX}{ownership_nonce}\n"


def generate_fixture(root: Path, file_count: int, ownership_nonce: str) -> list[Path]:
    marker = root / FIXTURE_MARKER
    entries = list(root.iterdir()) if root.is_dir() and not root.is_symlink() else []
    if (
        root.is_symlink()
        or not root.is_dir()
        or entries != [marker]
        or marker.is_symlink()
        or marker.read_text() != fixture_marker_contents(ownership_nonce)
    ):
        raise ValueError(f"fixture root must be an empty owned directory: {root}")
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


def fixture_manifest_hash(root: Path, files: list[Path]) -> str:
    """Hash the actual generated relative paths and contents in stable order."""
    digest = hashlib.sha256()
    for path in sorted(
        files, key=lambda candidate: candidate.relative_to(root).as_posix()
    ):
        relative = path.relative_to(root).as_posix().encode()
        content_digest = bytes.fromhex(file_sha256(path))
        digest.update(len(relative).to_bytes(8, "big"))
        digest.update(relative)
        digest.update(content_digest)
    return digest.hexdigest()


def cleanup_owned_fixture(
    root: Path,
    ownership_nonce: str,
    expected_identity: tuple[int, int],
) -> None:
    """Remove only the exact directory created by this benchmark invocation."""
    stat = root.lstat()
    actual_identity = (stat.st_dev, stat.st_ino)
    if root.is_symlink() or actual_identity != expected_identity:
        raise RuntimeError(f"fixture ownership identity changed: {root}")
    marker = root / FIXTURE_MARKER
    try:
        contents = marker.read_text()
    except OSError as error:
        raise RuntimeError(f"fixture ownership marker is missing: {root}") from error
    if marker.is_symlink() or contents != fixture_marker_contents(ownership_nonce):
        raise RuntimeError(f"fixture ownership marker is invalid: {root}")
    shutil.rmtree(root)


def disk_snapshot(root: Path) -> dict[str, DiskEntry]:
    if not root.exists():
        return {}
    if root.is_symlink():
        raise ValueError(f"index root must not be a symlink: {root}")
    snapshot: dict[str, DiskEntry] = {}
    for path in root.rglob("*"):
        if path.is_symlink():
            raise ValueError(f"index artifacts must not be symlinks: {path}")
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
    """Estimate allocated artifact bytes written to new or changed inodes.

    The process-level write counter is authoritative on Linux.  This portable
    estimate remains useful on macOS and Windows. Inode identity prevents a new
    generation's unchanged hardlinks from masquerading as rewritten data, and
    allocated size avoids charging sparse-file holes as physical bytes.
    """
    before_inodes = {
        (entry.device, entry.inode): (
            entry.logical_bytes,
            entry.mtime_ns,
            entry.allocated_bytes,
        )
        for entry in before.values()
    }
    rewritten = 0
    seen: set[tuple[int, int]] = set()
    for current in after.values():
        inode = (current.device, current.inode)
        if inode in seen:
            continue
        seen.add(inode)
        current_state = (
            current.logical_bytes,
            current.mtime_ns,
            current.allocated_bytes,
        )
        if before_inodes.get(inode) != current_state:
            rewritten += current.allocated_bytes
    return rewritten


def effective_rewrite_bytes(
    surviving_artifact_churn: int, io_write_bytes: int | None
) -> tuple[int, str]:
    """Use physical process-tree writes when supported, never less than churn."""
    if io_write_bytes is None:
        return surviving_artifact_churn, "surviving_changed_inode_allocated_bytes"
    return (
        max(surviving_artifact_churn, io_write_bytes),
        "max(surviving_changed_inode_allocated_bytes,direct_child_io_write_bytes)",
    )


def fixture_schema_hash() -> str:
    return hashlib.sha256(FIXTURE_SCHEMA.encode()).hexdigest()


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def sanitized_git_environment(
    extra: dict[str, str] | None = None,
) -> dict[str, str]:
    """Return an environment that cannot redirect Git away from an owned root."""
    environment = {
        name: value
        for name, value in os.environ.items()
        if not name.upper().startswith("GIT_")
    }
    if extra:
        environment.update(
            {
                name: value
                for name, value in extra.items()
                if not name.upper().startswith("GIT_")
            }
        )
    return environment


def build_environment_metadata(
    environment: dict[str, str] | None = None,
) -> dict[str, str]:
    """Record non-secret environment knobs that can change release artifacts."""
    environment = os.environ if environment is None else environment
    selected = {
        name: value
        for name, value in environment.items()
        if name in BUILD_ENVIRONMENT_KEYS
        or name.startswith("CARGO_PROFILE_RELEASE_")
        or (
            name.startswith("CARGO_TARGET_")
            and name.endswith(("_LINKER", "_RUSTFLAGS"))
        )
        or name.startswith(("AR_", "CC_", "CFLAGS_", "CXX_", "CXXFLAGS_"))
    }
    return dict(sorted(selected.items()))


def sanitized_git_output(root: Path, arguments: list[str]) -> str:
    completed = subprocess.run(
        ["git", *arguments],
        cwd=root,
        env=sanitized_git_environment(),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=10,
        check=False,
    )
    if completed.returncode != 0:
        raise RuntimeError(
            f"sanitized git {' '.join(arguments)} failed: {completed.stderr.strip()}"
        )
    return completed.stdout.strip()


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


def filesystem_metadata(path: Path) -> dict[str, int | None]:
    stat = path.stat()
    if not hasattr(os, "statvfs"):
        return {
            "device": stat.st_dev,
            "block_size": None,
            "fragment_size": None,
            "name_max": None,
        }
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
    while normalized.startswith("./"):
        normalized = normalized[2:]
    if not normalized or normalized.startswith("/") or ".." in normalized.split("/"):
        raise ValueError("expected_file must be a normalized root-relative path")
    for rank, item in enumerate(results, start=1):
        candidate = str(item.get("file", item.get("file_path", ""))).replace("\\", "/")
        while candidate.startswith("./"):
            candidate = candidate[2:]
        if candidate == normalized or candidate.endswith(f"/{normalized}"):
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


def load_external_quality(
    path: Path | None,
    *,
    require_attribution: bool = False,
    expected_revision: str | None = None,
) -> dict[str, Any] | None:
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
    if (
        isinstance(mrr, bool)
        or not isinstance(mrr, (int, float))
        or not 0 <= float(mrr) <= 1
    ):
        raise ValueError("external quality result needs mrr in [0, 1]")
    if (
        isinstance(recall, bool)
        or not isinstance(recall, (int, float))
        or not 0 <= float(recall) <= 1
    ):
        raise ValueError("external quality result needs recall_at_10 in [0, 1]")
    dataset_sha256 = raw.get("dataset_sha256")
    if dataset_sha256 is not None:
        dataset_sha256 = str(dataset_sha256).lower()
        if len(dataset_sha256) != 64 or any(
            character not in "0123456789abcdef" for character in dataset_sha256
        ):
            raise ValueError(
                "external quality result dataset_sha256 must be 64 hexadecimal characters"
            )
    raw_source = raw.get("source")
    if require_attribution:
        if not isinstance(raw_source, str) or not raw_source.strip():
            raise ValueError(
                "strict external quality result needs a non-empty string source"
            )
        source = raw_source.strip()
    else:
        source = str(path if raw_source is None else raw_source).strip()
    task_count = raw.get("task_count")
    source_revision = raw.get("source_revision")
    if source_revision is not None:
        source_revision = str(source_revision).lower()
    if require_attribution:
        if (
            isinstance(task_count, bool)
            or not isinstance(task_count, int)
            or task_count <= 0
        ):
            raise ValueError("strict external quality result needs positive task_count")
        if dataset_sha256 is None:
            raise ValueError("strict external quality result needs dataset_sha256")
        if not _is_hex_identifier(source_revision, (40, 64)):
            raise ValueError("strict external quality result needs source_revision")
        if expected_revision is None or source_revision != expected_revision.lower():
            raise ValueError(
                "external quality source_revision does not match expected revision"
            )
    return {
        "mrr": float(mrr),
        "recall_at_10": float(recall),
        "source": source,
        "task_count": task_count,
        "dataset_sha256": dataset_sha256,
        "source_revision": source_revision,
    }


def source_metadata(root: Path, codixing: Path) -> dict[str, Any]:
    def capture(command: list[str]) -> tuple[int, str]:
        kwargs: dict[str, Any] = {
            "cwd": root,
            "text": True,
            "stdout": subprocess.PIPE,
            "stderr": subprocess.DEVNULL,
            "timeout": 10,
            "check": False,
        }
        if command and command[0] == "git":
            kwargs["env"] = sanitized_git_environment()
        completed = subprocess.run(command, **kwargs)
        return completed.returncode, completed.stdout.strip()

    version_code, version = capture([str(codixing), "--version"])
    rustc_code, rustc_version = capture(
        [os.environ.get("RUSTC", "rustc"), "--version", "--verbose"]
    )
    revision_code, revision = capture(["git", "rev-parse", "HEAD"])
    tree_code, tree = capture(["git", "rev-parse", "HEAD^{tree}"])
    status_code, status = capture(["git", "status", "--porcelain"])
    source_tree_digest = None
    if revision_code == 0 and status_code == 0:
        diff_code, diff = capture(["git", "diff", "--binary", "HEAD", "--"])
        untracked_code, untracked = capture(
            ["git", "ls-files", "--others", "--exclude-standard"]
        )
        if diff_code == 0 and untracked_code == 0:
            digest = hashlib.sha256()

            def add_digest_field(label: str, value: bytes) -> None:
                digest.update(label.encode())
                digest.update(b"\0")
                digest.update(len(value).to_bytes(8, "big"))
                digest.update(value)

            add_digest_field("revision", revision.encode())
            add_digest_field("tracked-diff", diff.encode())
            try:
                for relative in sorted(untracked.splitlines()):
                    path = root / relative
                    add_digest_field("untracked-path", os.fsencode(relative))
                    if path.is_symlink():
                        add_digest_field(
                            "untracked-symlink", os.fsencode(os.readlink(path))
                        )
                    else:
                        add_digest_field(
                            "untracked-file-sha256",
                            bytes.fromhex(file_sha256(path)),
                        )
            except OSError:
                source_tree_digest = None
            else:
                source_tree_digest = digest.hexdigest()
    return {
        "root": str(root),
        "codixing_version": version if version_code == 0 else None,
        "rustc_version": rustc_version if rustc_code == 0 else None,
        "git_revision": revision.lower() if revision_code == 0 else None,
        "git_tree": tree.lower() if tree_code == 0 else None,
        "git_dirty": bool(status) if status_code == 0 else None,
        "source_tree_sha256": source_tree_digest,
    }


def capture_build_provenance(binary: Path) -> Any:
    """Ask a benchmark binary for its compile-time, side-effect-free identity."""
    try:
        completed = subprocess.run(
            [str(binary), "--build-provenance-json"],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=10,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as error:
        return {"capture_error": type(error).__name__}
    if completed.returncode != 0:
        return {"capture_error": f"exit status {completed.returncode}"}
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError:
        return {"capture_error": "malformed JSON"}


def git_identity_snapshot(root: Path) -> dict[str, Any]:
    top_level = Path(sanitized_git_output(root, ["rev-parse", "--show-toplevel"]))
    return {
        "root": str(top_level.resolve()),
        "revision": sanitized_git_output(root, ["rev-parse", "--verify", "HEAD"]),
        "tree": sanitized_git_output(root, ["rev-parse", "--verify", "HEAD^{tree}"]),
        "dirty": bool(
            sanitized_git_output(
                root, ["status", "--porcelain=v1", "--untracked-files=normal"]
            )
        ),
    }


def rustc_identity(root: Path, environment: dict[str, str]) -> str:
    completed = subprocess.run(
        [environment.get("RUSTC", "rustc"), "--version", "--verbose"],
        cwd=root,
        env=environment,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=10,
        check=False,
    )
    if completed.returncode != 0 or not completed.stdout.strip():
        raise RuntimeError("rustc identity capture failed: " + completed.stderr.strip())
    return completed.stdout.strip()


def fresh_resolved_path(path: Path, label: str) -> Path:
    """Resolve a not-yet-created endpoint without accepting dangling symlinks."""
    expanded = path.expanduser()
    absolute = expanded if expanded.is_absolute() else Path.cwd() / expanded
    if absolute.exists() or absolute.is_symlink():
        raise FileExistsError(f"{label} must be fresh and not a symlink: {absolute}")
    return absolute.resolve(strict=False)


def bootstrap_legacy_build_provenance(
    source_root: Path, target_dir: Path, receipt_path: Path
) -> dict[str, Any]:
    """Fresh-build an unattested historical baseline and bind its exact outputs."""
    source_root = source_root.expanduser().resolve()
    target_dir = fresh_resolved_path(target_dir, "legacy target directory")
    receipt_path = fresh_resolved_path(receipt_path, "legacy provenance receipt")
    if not source_root.is_dir():
        raise NotADirectoryError(
            f"legacy source root is not a directory: {source_root}"
        )
    if target_dir == source_root or source_root in target_dir.parents:
        raise ValueError("legacy target directory must be outside the source checkout")
    if receipt_path == source_root or source_root in receipt_path.parents:
        raise ValueError(
            "legacy provenance receipt must be outside the source checkout"
        )

    before = git_identity_snapshot(source_root)
    if (
        before["root"] != str(source_root)
        or not _is_hex_identifier(before["revision"], (40, 64))
        or not _is_hex_identifier(before["tree"], (40, 64))
        or before["dirty"] is not False
    ):
        raise ValueError("legacy baseline source must be an owned clean Git worktree")

    build_environment = sanitized_git_environment({"CARGO_TARGET_DIR": str(target_dir)})
    rustc_before = rustc_identity(source_root, build_environment)
    target_dir.parent.mkdir(parents=True, exist_ok=True)
    target_dir.mkdir()
    command = [
        "cargo",
        "build",
        "--locked",
        "--release",
        "--manifest-path",
        str(source_root / "Cargo.toml"),
        "-p",
        "codixing",
        "-p",
        "codixing-server",
    ]
    completed = subprocess.run(
        command,
        cwd=source_root,
        env=build_environment,
        check=False,
    )
    if completed.returncode != 0:
        raise RuntimeError(
            f"fresh legacy baseline build failed with {completed.returncode}"
        )

    after = git_identity_snapshot(source_root)
    if after != before or after["dirty"] is not False:
        raise RuntimeError("legacy baseline source changed during its owned build")
    rustc_after = rustc_identity(source_root, build_environment)
    if rustc_after != rustc_before:
        raise RuntimeError("Rust toolchain identity changed during the legacy build")

    executable_suffix = ".exe" if os.name == "nt" else ""
    binary_paths = {
        "codixing": target_dir / "release" / f"codixing{executable_suffix}",
        "server": target_dir / "release" / f"codixing-server{executable_suffix}",
    }
    binaries: dict[str, dict[str, Any]] = {}
    for name, path in binary_paths.items():
        if path.is_symlink() or not path.is_file() or not os.access(path, os.X_OK):
            raise FileNotFoundError(f"legacy build did not produce executable {path}")
        binaries[name] = {"path": str(path), "sha256": file_sha256(path)}

    receipt = {
        "schema_version": LEGACY_BUILD_PROVENANCE_SCHEMA_VERSION,
        "origin": LEGACY_BUILD_PROVENANCE_ORIGIN,
        "source": {"before": before, "after": after},
        "build": {
            "profile": "release",
            "features": "default",
            "locked": True,
            "packages": ["codixing", "codixing-server"],
            "target_dir": str(target_dir),
            "rustc": {"before": rustc_before, "after": rustc_after},
            "environment": build_environment_metadata(build_environment),
        },
        "binaries": binaries,
    }
    receipt_path.parent.mkdir(parents=True, exist_ok=True)
    with receipt_path.open("x") as handle:
        handle.write(json.dumps(receipt, indent=2, sort_keys=True) + "\n")
    return receipt


def _is_hex_identifier(value: Any, lengths: tuple[int, ...]) -> bool:
    return (
        isinstance(value, str)
        and len(value) in lengths
        and all(character.lower() in "0123456789abcdef" for character in value)
    )


def legacy_build_provenance_gate(
    receipt: Any, source: Any, binaries: Any, configuration: Any = None
) -> dict[str, Any]:
    """Validate the one explicit bootstrap path for historical baseline binaries."""
    receipt = receipt if isinstance(receipt, dict) else {}
    source = source if isinstance(source, dict) else {}
    binaries = binaries if isinstance(binaries, dict) else {}
    configuration = configuration if isinstance(configuration, dict) else {}
    checks: list[dict[str, Any]] = []

    def check(name: str, passed: bool, value: Any = None) -> None:
        checks.append(
            {
                "name": name,
                "value": value,
                "status": "passed" if passed else "failed",
            }
        )

    schema = receipt.get("schema_version")
    check(
        "legacy_schema",
        schema == LEGACY_BUILD_PROVENANCE_SCHEMA_VERSION
        and not isinstance(schema, bool),
        schema,
    )
    check(
        "legacy_origin",
        receipt.get("origin") == LEGACY_BUILD_PROVENANCE_ORIGIN,
        receipt.get("origin"),
    )
    receipt_source = receipt.get("source", {})
    receipt_source = receipt_source if isinstance(receipt_source, dict) else {}
    before = receipt_source.get("before", {})
    after = receipt_source.get("after", {})
    before = before if isinstance(before, dict) else {}
    after = after if isinstance(after, dict) else {}
    for phase, identity in (("before", before), ("after", after)):
        check(
            f"legacy_{phase}_root",
            identity.get("root") == source.get("root"),
            identity.get("root"),
        )
        check(
            f"legacy_{phase}_revision",
            _is_hex_identifier(identity.get("revision"), (40, 64))
            and identity.get("revision") == source.get("git_revision"),
            identity.get("revision"),
        )
        check(
            f"legacy_{phase}_tree",
            _is_hex_identifier(identity.get("tree"), (40, 64))
            and identity.get("tree") == source.get("git_tree"),
            identity.get("tree"),
        )
        check(
            f"legacy_{phase}_clean",
            identity.get("dirty") is False,
            identity.get("dirty"),
        )
    check("legacy_source_clean_now", source.get("git_dirty") is False)

    build = receipt.get("build", {})
    build = build if isinstance(build, dict) else {}
    target_dir = build.get("target_dir")
    check("legacy_release_profile", build.get("profile") == "release")
    check("legacy_default_features", build.get("features") == "default")
    check("legacy_locked_build", build.get("locked") is True)
    check(
        "legacy_exact_packages",
        build.get("packages") == ["codixing", "codixing-server"],
    )
    check(
        "legacy_target_dir",
        isinstance(target_dir, str)
        and Path(target_dir).is_absolute()
        and bool(target_dir.strip()),
        target_dir,
    )
    source_root = source.get("root")
    check(
        "legacy_target_outside_source",
        isinstance(source_root, str)
        and isinstance(target_dir, str)
        and Path(target_dir) != Path(source_root)
        and Path(source_root) not in Path(target_dir).parents,
        target_dir,
    )
    rustc = build.get("rustc", {})
    rustc = rustc if isinstance(rustc, dict) else {}
    check(
        "legacy_rustc_before",
        isinstance(rustc.get("before"), str)
        and rustc.get("before") == source.get("rustc_version"),
        rustc.get("before"),
    )
    check(
        "legacy_rustc_after",
        isinstance(rustc.get("after"), str)
        and rustc.get("after") == source.get("rustc_version"),
        rustc.get("after"),
    )
    recorded_environment_value = build.get("environment")
    recorded_environment = (
        recorded_environment_value
        if isinstance(recorded_environment_value, dict)
        else {}
    )
    check(
        "legacy_build_environment",
        isinstance(recorded_environment_value, dict)
        and recorded_environment == configuration.get("build_environment"),
        recorded_environment_value,
    )
    check(
        "legacy_configuration",
        configuration.get("build_profile") == "release"
        and configuration.get("build_features") == "default"
        and configuration.get("rustflags")
        == (recorded_environment or {}).get("RUSTFLAGS", ""),
        {
            "build_profile": configuration.get("build_profile"),
            "build_features": configuration.get("build_features"),
            "rustflags": configuration.get("rustflags"),
        },
    )

    receipt_binaries = receipt.get("binaries", {})
    receipt_binaries = receipt_binaries if isinstance(receipt_binaries, dict) else {}
    for name in ("codixing", "server"):
        expected = binaries.get(name, {})
        expected = expected if isinstance(expected, dict) else {}
        recorded = receipt_binaries.get(name, {})
        recorded = recorded if isinstance(recorded, dict) else {}
        check(
            f"legacy_{name}_path",
            recorded.get("path") == expected.get("path"),
            recorded.get("path"),
        )
        check(
            f"legacy_{name}_sha256",
            _is_hex_identifier(recorded.get("sha256"), (64,))
            and recorded.get("sha256") == expected.get("sha256"),
            recorded.get("sha256"),
        )
        if isinstance(target_dir, str) and isinstance(recorded.get("path"), str):
            expected_parent = Path(target_dir) / "release"
            check(
                f"legacy_{name}_owned_output",
                Path(recorded["path"]).parent == expected_parent,
                recorded.get("path"),
            )
        else:
            check(f"legacy_{name}_owned_output", False, recorded.get("path"))

    return {
        "status": (
            "passed" if all(item["status"] == "passed" for item in checks) else "failed"
        ),
        "checks": checks,
    }


def apply_legacy_build_provenance(
    source: dict[str, Any],
    binaries: dict[str, Any],
    configuration: dict[str, Any],
    receipt_path: Path | None,
) -> dict[str, Any] | None:
    """Use a receipt only when neither binary has an embedded attestation."""
    native = [
        binaries.get(name, {}).get("build_provenance", {})
        for name in ("codixing", "server")
    ]
    embedded = [
        isinstance(attestation, dict)
        and attestation.get("origin") == EMBEDDED_BUILD_PROVENANCE_ORIGIN
        for attestation in native
    ]
    if any(embedded):
        return None
    if receipt_path is None:
        return None

    if receipt_path.is_symlink() or not receipt_path.is_file():
        raise ValueError("legacy build provenance receipt must be a regular file")
    receipt = json.loads(receipt_path.read_text())
    gate = legacy_build_provenance_gate(receipt, source, binaries, configuration)
    if gate["status"] != "passed":
        failed = [item["name"] for item in gate["checks"] if item["status"] != "passed"]
        raise ValueError("invalid legacy build provenance: " + ", ".join(failed))
    identity = receipt["source"]["after"]
    for name in ("codixing", "server"):
        binaries[name]["build_provenance"] = {
            "schema_version": 1,
            "origin": LEGACY_BUILD_PROVENANCE_ORIGIN,
            "revision": identity["revision"],
            "tree": identity["tree"],
            "dirty": False,
            "binary_sha256": binaries[name]["sha256"],
        }
    return {"receipt": receipt, "gate": gate}


def source_provenance_gate(
    source: Any, expected_revision: str | None = None
) -> dict[str, Any]:
    """Require benchmark evidence to identify one clean Git source tree."""
    source = source if isinstance(source, dict) else {}
    revision = source.get("git_revision")
    tree = source.get("git_tree")
    tree_digest = source.get("source_tree_sha256")
    dirty = source.get("git_dirty")
    checks = [
        {
            "name": "git_revision",
            "value": revision,
            "status": (
                "passed" if _is_hex_identifier(revision, (40, 64)) else "failed"
            ),
        },
        {
            "name": "git_tree",
            "value": tree,
            "status": "passed" if _is_hex_identifier(tree, (40, 64)) else "failed",
        },
        {
            "name": "source_tree_sha256",
            "value": tree_digest,
            "status": (
                "passed" if _is_hex_identifier(tree_digest, (64,)) else "failed"
            ),
        },
        {
            "name": "git_clean",
            "value": dirty is False,
            "status": "passed" if dirty is False else "failed",
        },
    ]
    if expected_revision is not None:
        checks.append(
            {
                "name": "expected_revision",
                "value": revision,
                "expected": expected_revision,
                "status": (
                    "passed"
                    if _is_hex_identifier(expected_revision, (40, 64))
                    and revision == expected_revision.lower()
                    else "failed"
                ),
            }
        )
    return {
        "status": (
            "passed"
            if all(check["status"] == "passed" for check in checks)
            else "failed"
        ),
        "checks": checks,
    }


def binary_source_provenance_gate(
    source: Any,
    codixing_attestation: Any,
    server_attestation: Any,
    *,
    require_clean: bool = True,
    binaries: Any = None,
    legacy_provenance: Any = None,
    configuration: Any = None,
) -> dict[str, Any]:
    """Bind both measured executables to the source checkout that names them."""
    source = source if isinstance(source, dict) else {}
    attestations = {
        "codixing": (
            codixing_attestation if isinstance(codixing_attestation, dict) else {}
        ),
        "server": server_attestation if isinstance(server_attestation, dict) else {},
    }
    binaries = binaries if isinstance(binaries, dict) else {}
    legacy_provenance = legacy_provenance if isinstance(legacy_provenance, dict) else {}
    configuration = configuration if isinstance(configuration, dict) else {}
    checks: list[dict[str, Any]] = []

    def check(name: str, passed: bool, value: Any = None, expected: Any = None) -> None:
        item = {
            "name": name,
            "value": value,
            "status": "passed" if passed else "failed",
        }
        if expected is not None:
            item["expected"] = expected
        checks.append(item)

    source_revision = source.get("git_revision")
    source_tree = source.get("git_tree")
    source_dirty = source.get("git_dirty")
    check(
        "source_revision",
        _is_hex_identifier(source_revision, (40, 64)),
        source_revision,
    )
    check("source_tree", _is_hex_identifier(source_tree, (40, 64)), source_tree)
    check("source_dirty_flag", isinstance(source_dirty, bool), source_dirty)

    for name, attestation in attestations.items():
        schema_version = attestation.get("schema_version")
        origin = attestation.get("origin")
        revision = attestation.get("revision")
        tree = attestation.get("tree")
        dirty = attestation.get("dirty")
        check(
            f"{name}_schema",
            schema_version == 1 and not isinstance(schema_version, bool),
            schema_version,
            1,
        )
        check(
            f"{name}_origin",
            origin
            in {
                EMBEDDED_BUILD_PROVENANCE_ORIGIN,
                LEGACY_BUILD_PROVENANCE_ORIGIN,
            },
            origin,
        )
        check(
            f"{name}_revision",
            _is_hex_identifier(revision, (40, 64)),
            revision,
        )
        check(f"{name}_tree", _is_hex_identifier(tree, (40, 64)), tree)
        check(f"{name}_dirty_flag", isinstance(dirty, bool), dirty)
        check(
            f"{name}_matches_source_revision",
            revision == source_revision and _is_hex_identifier(revision, (40, 64)),
            revision,
            source_revision,
        )
        check(
            f"{name}_matches_source_tree",
            tree == source_tree and _is_hex_identifier(tree, (40, 64)),
            tree,
            source_tree,
        )

    origins = {name: value.get("origin") for name, value in attestations.items()}
    same_origin = origins["codixing"] == origins["server"]
    check("matching_binary_origins", same_origin, origins)
    if same_origin and origins["codixing"] == LEGACY_BUILD_PROVENANCE_ORIGIN:
        for name, attestation in attestations.items():
            binary = binaries.get(name, {})
            binary = binary if isinstance(binary, dict) else {}
            binary_sha256 = binary.get("sha256")
            check(
                f"{name}_legacy_binary_sha256",
                _is_hex_identifier(binary_sha256, (64,))
                and attestation.get("binary_sha256") == binary_sha256,
                attestation.get("binary_sha256"),
                binary_sha256,
            )
        receipt_gate = legacy_build_provenance_gate(
            legacy_provenance.get("receipt"),
            source,
            binaries,
            configuration,
        )
        check(
            "legacy_receipt",
            receipt_gate["status"] == "passed",
            receipt_gate["checks"],
        )
    elif any(origin == LEGACY_BUILD_PROVENANCE_ORIGIN for origin in origins.values()):
        check("legacy_receipt", False, "mixed legacy/native provenance")

    clean = (
        source_dirty is False
        and attestations["codixing"].get("dirty") is False
        and attestations["server"].get("dirty") is False
    )
    check("clean_source_and_binaries", clean or not require_clean, clean, True)
    return {
        "status": (
            "passed" if all(item["status"] == "passed" for item in checks) else "failed"
        ),
        "required": require_clean,
        "checks": checks,
    }


def result_binary_source_provenance_gate(
    result: Any, *, require_clean: bool = True
) -> dict[str, Any]:
    result = result if isinstance(result, dict) else {}
    binaries = result.get("binaries", {})
    binaries = binaries if isinstance(binaries, dict) else {}

    def attestation(name: str) -> Any:
        binary = binaries.get(name, {})
        return binary.get("build_provenance") if isinstance(binary, dict) else None

    return binary_source_provenance_gate(
        result.get("source"),
        attestation("codixing"),
        attestation("server"),
        require_clean=require_clean,
        binaries=binaries,
        legacy_provenance=result.get("legacy_build_provenance"),
        configuration=result.get("configuration"),
    )


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
    rss_sources: set[str] = set()
    pss_sources: set[str] = set()
    memory_completeness: list[bool] = []
    observed: dict[int, list[list[dict[str, Any]]]] = {}
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
        memory_completeness.append(measured.memory_complete)
        if measured.peak_rss_bytes is not None:
            peak_rss_values.append(measured.peak_rss_bytes)
            if measured.peak_rss_source:
                rss_sources.add(measured.peak_rss_source)
        if measured.peak_pss_bytes is not None:
            peak_pss_values.append(measured.peak_pss_bytes)
            if measured.peak_pss_source:
                pss_sources.add(measured.peak_pss_source)
        observed.setdefault(case_index, []).append(_parse_search_json(measured.stdout))

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
            observed[index] = [_parse_search_json(measured.stdout)]
        reciprocal_ranks = [
            reciprocal_rank(results, case["expected_file"])
            for results in observed[index]
        ]
        rr = statistics.mean(reciprocal_ranks)
        quality.append(
            {
                **case,
                "reciprocal_rank": rr,
                "minimum_reciprocal_rank": min(reciprocal_ranks),
                "observation_count": len(reciprocal_ranks),
                "found": all(rank > 0 for rank in reciprocal_ranks),
            }
        )
    summary = latency_summary(latencies)
    summary["peak_rss_bytes"] = max(peak_rss_values, default=None)
    summary["peak_pss_bytes"] = max(peak_pss_values, default=None)
    summary["peak_rss_source"] = ",".join(sorted(rss_sources)) or None
    summary["peak_pss_source"] = ",".join(sorted(pss_sources)) or None
    summary["memory_complete"] = len(memory_completeness) == runs and all(
        memory_completeness
    )
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


def _server_search_results(response: Any) -> list[dict[str, Any]]:
    if not isinstance(response, dict) or not isinstance(response.get("results"), list):
        raise ValueError("server search response needs a results array")
    results = response["results"]
    if not all(isinstance(item, dict) for item in results):
        raise ValueError("server search results must be JSON objects")
    return results


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
        start_new_session=os.name == "posix",
    )
    assert process.stdout is not None and process.stderr is not None
    stdout_chunks, stdout_drain = _start_pipe_drain(process.stdout)
    stderr_chunks, stderr_drain = _start_pipe_drain(process.stderr)
    url = f"http://127.0.0.1:{port}"
    rss_samples: list[int] = []
    pss_samples: list[int] = []
    memory_source: str | None = None

    def sample_server_memory() -> None:
        nonlocal memory_source
        rss, pss, source = _live_process_memory_stats(process.pid)
        if rss is not None:
            rss_samples.append(rss)
            memory_source = source
        if pss is not None:
            pss_samples.append(pss)

    deadline = time.monotonic() + min(timeout_s, 120)
    while True:
        sample_server_memory()
        if process.poll() is not None:
            stdout_drain.join(timeout=10)
            stderr_drain.join(timeout=10)
            raise RuntimeError(
                "server exited during startup\n"
                f"{_captured_tail(stdout_chunks)}\n{_captured_tail(stderr_chunks)}"
            )
        try:
            _request_json(f"{url}/health")
            break
        except (urllib.error.URLError, ConnectionError, TimeoutError):
            if time.monotonic() > deadline:
                _terminate_process_group(process, force=False)
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    _terminate_process_group(process, force=True)
                    process.wait(timeout=10)
                stdout_drain.join(timeout=10)
                stderr_drain.join(timeout=10)
                raise TimeoutError(
                    "server did not become ready\n"
                    f"{_captured_tail(stdout_chunks)}\n{_captured_tail(stderr_chunks)}"
                )
            time.sleep(0.05)

    try:
        validation_ranks: list[float] = []
        for index in range(warmups):
            case = cases[index % len(cases)]
            response = _request_json(
                f"{url}/search",
                {"query": case["query"], "limit": 10, "strategy": case["strategy"]},
            )
            validation_ranks.append(
                reciprocal_rank(_server_search_results(response), case["expected_file"])
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
            validation_ranks.append(
                reciprocal_rank(_server_search_results(response), case["expected_file"])
            )
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
                "source": memory_source,
            },
            "validation": {
                "observation_count": len(validation_ranks),
                "measured_observation_count": runs,
                "minimum_reciprocal_rank": min(validation_ranks, default=0.0),
                "mrr": statistics.mean(validation_ranks) if validation_ranks else 0.0,
                "all_found": bool(validation_ranks)
                and all(rank > 0 for rank in validation_ranks),
            },
        }
    finally:
        _terminate_process_group(process, force=False)
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            _terminate_process_group(process, force=True)
            process.wait(timeout=10)
        stdout_drain.join(timeout=10)
        stderr_drain.join(timeout=10)


def _select_edit_paths(paths: list[Path], count: int, *, spread: bool) -> list[Path]:
    if count < 0 or count > len(paths):
        raise ValueError("edit count must be between zero and the path count")
    if count == 0:
        return []
    if not spread or count == 1:
        return paths[:count]

    last_index = len(paths) - 1
    return [
        paths[(selection * last_index) // (count - 1)] for selection in range(count)
    ]


def edit_files(
    paths: list[Path], count: int, generation: str, *, spread: bool = False
) -> list[tuple[Path, str]]:
    selected = _select_edit_paths(paths, count, spread=spread)
    token_generation = "".join(
        character if character.isalnum() else "_" for character in generation
    )
    mutations = []
    for index, path in enumerate(selected):
        token = f"codixing_benchmark_sync_{token_generation}_{index:06d}"
        with path.open("a", encoding="utf-8") as handle:
            handle.write(f"\npub fn {token}() -> usize {{ {index} }}\n")
        mutations.append((path, token))
    return mutations


def validate_sync_token(
    codixing: Path,
    root: Path,
    token: str,
    expected_path: Path,
    timeout_s: int,
) -> dict[str, Any]:
    """Prove an edited token is searchable without contaminating sync timing."""
    expected_file = expected_path.relative_to(root).as_posix()
    command = [
        str(codixing),
        "search",
        token,
        "--strategy",
        "exact",
        "--limit",
        "10",
        "--json",
    ]
    completed = subprocess.run(
        command,
        cwd=root,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout_s,
        check=False,
    )
    parse_error = None
    try:
        results = (
            _parse_search_json(completed.stdout) if completed.returncode == 0 else []
        )
    except (TypeError, ValueError, json.JSONDecodeError) as error:
        parse_error = str(error)
        results = []

    observed_files = []
    for item in results:
        candidate = str(item.get("file", item.get("file_path", ""))).replace("\\", "/")
        while candidate.startswith("./"):
            candidate = candidate[2:]
        if candidate == expected_file or candidate.endswith(f"/{expected_file}"):
            candidate = expected_file
        observed_files.append(candidate)
    unique_files = sorted(set(observed_files))
    passed = (
        completed.returncode == 0
        and parse_error is None
        and bool(results)
        and unique_files == [expected_file]
    )
    return {
        "token": token,
        "expected_file": expected_file,
        "observed_files": unique_files,
        "result_count": len(results),
        "exit_code": completed.returncode,
        "parse_error": parse_error,
        "stderr": completed.stderr[-4_096:],
        "status": "passed" if passed else "failed",
    }


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
    surviving_churn = rewritten_bytes_estimate(before, after)
    effective_bytes, effective_source = effective_rewrite_bytes(
        surviving_churn, measured.io_write_bytes
    )
    return {
        "label": label,
        "process": asdict(measured),
        # Retained for schema readers that consumed the v5 name. This is only
        # surviving-artifact churn; acceptance gates use effective_rewrite_bytes.
        "artifact_bytes_rewritten_estimate": surviving_churn,
        "artifact_bytes_rewritten_estimate_source": (
            "unique_changed_inode_allocated_bytes"
        ),
        "surviving_artifact_churn_bytes": surviving_churn,
        "effective_rewrite_bytes": effective_bytes,
        "effective_rewrite_bytes_source": effective_source,
        "process_io_complete": measured.io_complete,
        "disk_delta_bytes": disk_usage(after)["total_bytes"]
        - disk_usage(before)["total_bytes"],
    }


def aggregate_sync_samples(label: str, samples: list[dict[str, Any]]) -> dict[str, Any]:
    """Retain raw runs while exposing stable aggregate paths for existing gates."""
    if len(samples) != SYNC_REPETITIONS:
        raise ValueError(f"{label} requires exactly {SYNC_REPETITIONS} samples")

    wall_times = [float(sample["process"]["wall_time_ms"]) for sample in samples]
    wall_summary = sync_latency_summary(wall_times)
    median = wall_summary["median_ms"]
    representative = min(
        samples,
        key=lambda sample: abs(float(sample["process"]["wall_time_ms"]) - median),
    )
    process = dict(representative["process"])
    process["wall_time_ms"] = median
    process["memory_complete"] = all(
        sample["process"].get("memory_complete") is True for sample in samples
    )
    process["io_complete"] = all(
        sample["process"].get("io_complete") is True for sample in samples
    )
    for field in (
        "peak_rss_bytes",
        "peak_pss_bytes",
        "io_read_bytes",
        "io_write_bytes",
    ):
        values = [
            sample["process"].get(field)
            for sample in samples
            if isinstance(sample["process"].get(field), (int, float))
        ]
        process[field] = max(values, default=None)

    surviving_samples = [
        int(
            sample.get(
                "surviving_artifact_churn_bytes",
                sample["artifact_bytes_rewritten_estimate"],
            )
        )
        for sample in samples
    ]
    effective_samples = [
        int(
            sample.get(
                "effective_rewrite_bytes",
                effective_rewrite_bytes(
                    surviving_samples[index], sample["process"].get("io_write_bytes")
                )[0],
            )
        )
        for index, sample in enumerate(samples)
    ]
    disk_delta_samples = [int(sample["disk_delta_bytes"]) for sample in samples]
    validations = [
        sample["validation"]
        for sample in samples
        if isinstance(sample.get("validation"), dict)
    ]
    return {
        "label": label,
        "process": process,
        "wall_time_summary": wall_summary,
        "repetitions": len(samples),
        # Rewrite gates intentionally use the worst observed sample. Latency gates
        # continue to read the median from process.wall_time_ms.
        "artifact_bytes_rewritten_estimate": max(surviving_samples),
        "artifact_bytes_rewritten_estimate_source": representative[
            "artifact_bytes_rewritten_estimate_source"
        ],
        "artifact_bytes_rewritten_samples": surviving_samples,
        "surviving_artifact_churn_bytes": max(surviving_samples),
        "surviving_artifact_churn_samples": surviving_samples,
        "effective_rewrite_bytes": max(effective_samples),
        "effective_rewrite_bytes_source": (
            "max(surviving_changed_inode_allocated_bytes,direct_child_io_write_bytes)"
            if any(
                sample["process"].get("io_write_bytes") is not None
                for sample in samples
            )
            else "surviving_changed_inode_allocated_bytes"
        ),
        "effective_rewrite_samples": effective_samples,
        "process_io_complete": all(
            sample["process"].get("io_complete") is True for sample in samples
        ),
        "disk_delta_bytes": statistics.median(disk_delta_samples),
        "disk_delta_samples": disk_delta_samples,
        "validations": validations,
        "samples": samples,
    }


def sync_measurement_gate(sync_metrics: dict[str, Any]) -> dict[str, Any]:
    """Fail evidence capture when repeated incremental timings are unstable."""
    checks = []
    for label in ("no_op", "one_file"):
        summary = sync_metrics.get(label, {}).get("wall_time_summary", {})
        stable = summary.get("stable") is True
        checks.append(
            {
                "name": f"{label}_sync_stability",
                "metric": f"metrics.sync.{label}.wall_time_summary.mad_ms",
                "median_ms": summary.get("median_ms"),
                "mad_ms": summary.get("mad_ms"),
                "jitter_limit_ms": summary.get("jitter_limit_ms"),
                "iqr_ms": summary.get("iqr_ms"),
                "iqr_limit_ms": summary.get("iqr_limit_ms"),
                "status": "passed" if stable else "failed",
            }
        )
    return {
        "status": (
            "passed"
            if all(check["status"] == "passed" for check in checks)
            else "failed"
        ),
        "checks": checks,
    }


def sync_correctness_gate(sync_metrics: Any) -> dict[str, Any]:
    """Require every timed edit to be visible immediately after its sync."""
    sync_metrics = sync_metrics if isinstance(sync_metrics, dict) else {}
    one_file = sync_metrics.get("one_file", {})
    repetitions = one_file.get("repetitions")
    distinct_one_file_edits = one_file.get("distinct_edited_files")
    one_file_validations = one_file.get("validations", [])
    one_percent = sync_metrics.get("one_percent", {})
    one_percent_validations = one_percent.get("validations", [])
    edited_files = one_percent.get("edited_files")

    one_file_passed = (
        isinstance(repetitions, int)
        and not isinstance(repetitions, bool)
        and repetitions == SYNC_REPETITIONS
        and isinstance(one_file_validations, list)
        and len(one_file_validations) == repetitions
        and all(
            isinstance(validation, dict) and validation.get("status") == "passed"
            for validation in one_file_validations
        )
        and len(
            {
                validation.get("token")
                for validation in one_file_validations
                if isinstance(validation, dict)
            }
        )
        == repetitions
        and len(
            {
                validation.get("expected_file")
                for validation in one_file_validations
                if isinstance(validation, dict)
            }
        )
        == distinct_one_file_edits
        and isinstance(distinct_one_file_edits, int)
        and not isinstance(distinct_one_file_edits, bool)
        and 0 < distinct_one_file_edits <= repetitions
    )
    one_percent_expected = (
        min(3, edited_files)
        if isinstance(edited_files, int)
        and not isinstance(edited_files, bool)
        and edited_files > 0
        else None
    )
    expected_positions = (
        {"first"}
        if one_percent_expected == 1
        else {"first", "last"}
        if one_percent_expected == 2
        else {"first", "middle", "last"}
        if one_percent_expected == 3
        else set()
    )
    one_percent_passed = (
        isinstance(one_percent_validations, list)
        and one_percent_expected is not None
        and len(one_percent_validations) == one_percent_expected
        and {
            validation.get("position")
            for validation in one_percent_validations
            if isinstance(validation, dict)
        }
        == expected_positions
        and len(
            {
                validation.get("token")
                for validation in one_percent_validations
                if isinstance(validation, dict)
            }
        )
        == one_percent_expected
        and len(
            {
                validation.get("expected_file")
                for validation in one_percent_validations
                if isinstance(validation, dict)
            }
        )
        == one_percent_expected
        and all(
            isinstance(validation, dict) and validation.get("status") == "passed"
            for validation in one_percent_validations
        )
    )
    checks = [
        {
            "name": "one_file_tokens_visible_after_each_timed_sync",
            "observation_count": (
                len(one_file_validations)
                if isinstance(one_file_validations, list)
                else None
            ),
            "expected": SYNC_REPETITIONS,
            "status": "passed" if one_file_passed else "failed",
        },
        {
            "name": "one_percent_first_middle_last_tokens_visible",
            "observation_count": (
                len(one_percent_validations)
                if isinstance(one_percent_validations, list)
                else None
            ),
            "expected": one_percent_expected,
            "status": "passed" if one_percent_passed else "failed",
        },
    ]
    return {
        "status": (
            "passed"
            if all(check["status"] == "passed" for check in checks)
            else "failed"
        ),
        "checks": checks,
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


def _external_quality_identity(
    result: dict[str, Any],
) -> tuple[Any, Any, Any] | None:
    external = result.get("metrics", {}).get("quality", {}).get("external")
    if not isinstance(external, dict):
        return None
    return (
        external.get("source"),
        external.get("task_count"),
        external.get("dataset_sha256"),
    )


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


def doctor_index_gate(report: Any, expected_file_count: int) -> dict[str, Any]:
    """Verify that the measured fixture is fully represented by a BM25-only index."""
    index = report.get("index", {}) if isinstance(report, dict) else {}
    meta = index.get("meta", {}) if isinstance(index, dict) else {}
    config = index.get("config", {}) if isinstance(index, dict) else {}
    file_count = meta.get("file_count") if isinstance(meta, dict) else None
    chunk_count = meta.get("chunk_count") if isinstance(meta, dict) else None
    embedding_enabled = (
        config.get("embedding_enabled") if isinstance(config, dict) else None
    )
    checks = [
        {
            "name": "index_status_ok",
            "value": index.get("status") if isinstance(index, dict) else None,
            "status": (
                "passed"
                if isinstance(index, dict) and index.get("status") == "ok"
                else "failed"
            ),
        },
        {
            "name": "indexed_file_count",
            "value": file_count,
            "expected": expected_file_count,
            "status": (
                "passed"
                if isinstance(file_count, int)
                and not isinstance(file_count, bool)
                and file_count == expected_file_count
                else "failed"
            ),
        },
        {
            "name": "plausible_chunk_count",
            "value": chunk_count,
            "minimum": expected_file_count,
            "maximum": expected_file_count * 16,
            "status": (
                "passed"
                if isinstance(chunk_count, int)
                and not isinstance(chunk_count, bool)
                and chunk_count >= expected_file_count
                and chunk_count <= expected_file_count * 16
                else "failed"
            ),
        },
        {
            "name": "embedding_disabled",
            "value": embedding_enabled,
            "status": "passed" if embedding_enabled is False else "failed",
        },
    ]
    return {
        "status": (
            "passed"
            if all(check["status"] == "passed" for check in checks)
            else "failed"
        ),
        "checks": checks,
    }


def audit_generation_layout(
    report: Any, snapshot: dict[str, DiskEntry]
) -> dict[str, Any]:
    """Normalize legacy and generational layouts against the measured filesystem."""
    observed_generations = sorted(
        {
            parts[1]
            for relative in snapshot
            if len(parts := Path(relative).parts) >= 3 and parts[0] == "generations"
        }
    )
    index = report.get("index", {}) if isinstance(report, dict) else {}
    reported = index.get("layout") if isinstance(index, dict) else None
    if not isinstance(reported, dict):
        if not observed_generations:
            return {
                "kind": "legacy_flat",
                "generation_count": 0,
                "active_generation": None,
                "abandoned_generations": [],
                "observed_generations": [],
                "source": "filesystem_normalized",
            }
        return {
            "kind": "unverified_generational",
            "generation_count": len(observed_generations),
            "active_generation": None,
            "abandoned_generations": observed_generations,
            "observed_generations": observed_generations,
            "source": "filesystem_normalized",
        }
    return {
        **reported,
        "observed_generations": observed_generations,
        "source": "doctor_and_filesystem",
    }


def generation_layout_gate(
    layout: Any, *, allow_legacy: bool = False
) -> dict[str, Any]:
    """Require clean legacy state or exactly one verified active generation."""
    layout = layout if isinstance(layout, dict) else {}
    kind = layout.get("kind")
    active_generation = layout.get("active_generation")
    abandoned = layout.get("abandoned_generations")
    observed = layout.get("observed_generations")
    legacy_clean = (
        allow_legacy
        and kind == "legacy_flat"
        and layout.get("generation_count") == 0
        and active_generation is None
        and abandoned == []
        and observed == []
    )
    generational_clean = (
        kind == "generational"
        and layout.get("generation_count") == 1
        and isinstance(active_generation, str)
        and active_generation.startswith("gen-")
        and abandoned == []
        and observed == [active_generation]
    )
    checks = [
        {
            "name": "supported_layout",
            "value": kind,
            "allow_legacy": allow_legacy,
            "status": "passed" if legacy_clean or generational_clean else "failed",
        },
        {
            "name": "one_active_generation_or_clean_legacy",
            "value": layout.get("generation_count"),
            "status": ("passed" if legacy_clean or generational_clean else "failed"),
        },
        {
            "name": "no_abandoned_generations",
            "value": abandoned,
            "status": "passed"
            if abandoned == [] and observed is not None
            else "failed",
        },
    ]
    return {
        "status": (
            "passed"
            if all(check["status"] == "passed" for check in checks)
            else "failed"
        ),
        "checks": checks,
    }


def strict_claim_policy_gate(policy: dict[str, float]) -> dict[str, Any]:
    """Require the documented 2x/50% policy before labeling strict evidence."""
    checks: list[dict[str, Any]] = []
    for name, ceiling in STRICT_CLAIM_MAXIMUMS.items():
        value = policy.get(name)
        passed = (
            isinstance(value, (int, float))
            and not isinstance(value, bool)
            and math.isfinite(value)
            and value <= ceiling
        )
        checks.append(
            {
                "name": name,
                "value": value,
                "required_maximum": ceiling,
                "status": "passed" if passed else "failed",
            }
        )
    for name, floor in STRICT_CLAIM_MINIMUMS.items():
        value = policy.get(name)
        passed = (
            isinstance(value, (int, float))
            and not isinstance(value, bool)
            and math.isfinite(value)
            and value >= floor
            and value <= 1
        )
        checks.append(
            {
                "name": name,
                "value": value,
                "required_minimum": floor,
                "status": "passed" if passed else "failed",
            }
        )
    return {
        "status": (
            "passed"
            if all(check["status"] == "passed" for check in checks)
            else "failed"
        ),
        "checks": checks,
    }


def worker_comparison_gate(
    current: dict[str, Any], baseline: dict[str, Any]
) -> dict[str, Any]:
    """Require attributable, equal fixed worker counts for baseline comparisons."""
    checks: list[dict[str, Any]] = []
    effective_counts: dict[str, int] = {}

    def valid_count(value: Any) -> bool:
        return isinstance(value, int) and not isinstance(value, bool) and value > 0

    for role, result in (("current", current), ("baseline", baseline)):
        configuration = result.get("configuration", {})
        if not isinstance(configuration, dict):
            configuration = {}
        worker_mode = configuration.get("worker_mode")
        requested = configuration.get("requested_threads")
        effective = configuration.get("effective_worker_threads")
        passed = (
            worker_mode == "fixed"
            and valid_count(requested)
            and valid_count(effective)
            and requested == effective
        )
        checks.append(
            {
                "name": f"{role}_fixed_worker_telemetry",
                "worker_mode": worker_mode,
                "requested_threads": requested,
                "effective_worker_threads": effective,
                "status": "passed" if passed else "failed",
            }
        )
        if passed:
            effective_counts[role] = effective

    equal = (
        set(effective_counts) == {"current", "baseline"}
        and effective_counts["current"] == effective_counts["baseline"]
    )
    checks.append(
        {
            "name": "equal_effective_worker_threads",
            "current": effective_counts.get("current"),
            "baseline": effective_counts.get("baseline"),
            "status": "passed" if equal else "failed",
        }
    )
    return {
        "status": (
            "passed"
            if all(check["status"] == "passed" for check in checks)
            else "failed"
        ),
        "checks": checks,
    }


def strict_claim_evidence_gate(
    current: dict[str, Any],
    baseline: dict[str, Any],
    *,
    expected_current_revision: str | None,
    expected_baseline_revision: str | None,
) -> dict[str, Any]:
    """Fail closed unless both artifacts can support a strict 100K claim."""
    checks: list[dict[str, Any]] = []

    def check(name: str, passed: bool, value: Any = None) -> None:
        checks.append(
            {
                "name": name,
                "value": value,
                "status": "passed" if passed else "failed",
            }
        )

    check("current_mode", current.get("evidence_mode") == "strict-claim")
    check("baseline_mode", baseline.get("evidence_mode") == "claim-baseline")
    worker_evidence = worker_comparison_gate(current, baseline)
    check(
        "fixed_worker_comparison",
        worker_evidence["status"] == "passed",
        worker_evidence["checks"],
    )
    check(
        "profile_100k",
        current.get("profile_name") == "100k"
        and baseline.get("profile_name") == "100k"
        and current.get("profile") == asdict(PROFILES["100k"])
        and baseline.get("profile") == asdict(PROFILES["100k"])
        and _metric(current, "profile.file_count") == 100_000
        and _metric(baseline, "profile.file_count") == 100_000
        and _metric(current, "metrics.source.file_count") == 100_000
        and _metric(baseline, "metrics.source.file_count") == 100_000,
    )
    current_binaries = current.get("binaries", {})
    if not isinstance(current_binaries, dict):
        current_binaries = {}
    current_binary_origins = []
    for binary_name in ("codixing", "server"):
        binary = current_binaries.get(binary_name, {})
        if not isinstance(binary, dict):
            binary = {}
        build_provenance = binary.get("build_provenance", {})
        if not isinstance(build_provenance, dict):
            build_provenance = {}
        current_binary_origins.append(build_provenance.get("origin"))
    check(
        "current_embedded_binary_origins",
        current_binary_origins == [EMBEDDED_BUILD_PROVENANCE_ORIGIN] * 2,
        current_binary_origins,
    )
    for role, result, expected_revision in (
        ("current", current, expected_current_revision),
        ("baseline", baseline, expected_baseline_revision),
    ):
        check(
            f"{role}_measurement_scope",
            result.get("measurement_scope") == MEASUREMENT_SCOPE,
            result.get("measurement_scope"),
        )
        source = result.get("source", {})
        revision = source.get("git_revision")
        check(
            f"{role}_expected_revision",
            _is_hex_identifier(expected_revision, (40, 64))
            and revision == str(expected_revision).lower(),
            revision,
        )
        binary_provenance = result_binary_source_provenance_gate(result)
        check(
            f"{role}_binary_source_provenance",
            binary_provenance["status"] == "passed",
            binary_provenance["checks"],
        )
        check(f"{role}_rustc", bool(source.get("rustc_version")))
        configuration = result.get("configuration", {})
        check(
            f"{role}_build_identity",
            configuration.get("build_profile") == "release"
            and bool(configuration.get("build_features"))
            and configuration.get("embedding_enabled") is False
            and all(
                _is_hex_identifier(
                    result.get("binaries", {}).get(binary, {}).get("sha256"), (64,)
                )
                for binary in ("codixing", "server")
            ),
        )
        manifest = result.get("fixture", {}).get("manifest_sha256")
        check(f"{role}_fixture_manifest", _is_hex_identifier(manifest, (64,)))
        external = result.get("metrics", {}).get("quality", {}).get("external")
        attributed = (
            isinstance(external, dict)
            and isinstance(external.get("task_count"), int)
            and not isinstance(external.get("task_count"), bool)
            and external["task_count"] > 0
            and _is_hex_identifier(external.get("dataset_sha256"), (64,))
            and external.get("source_revision") == revision
            and isinstance(external.get("source"), str)
            and bool(external["source"].strip())
        )
        check(f"{role}_external_quality_attribution", attributed)
        quality_mrr = _metric(result, "metrics.quality.mrr")
        quality_recall = _metric(result, "metrics.quality.recall_at_10")
        check(
            f"{role}_quality_range",
            quality_mrr is not None
            and 0 <= quality_mrr <= 1
            and quality_recall is not None
            and 0 <= quality_recall <= 1,
        )

        memory_sources = [
            _metric(result, "metrics.init.peak_rss_bytes"),
            _metric(result, "metrics.queries.cold.peak_rss_bytes"),
            _metric(result, "metrics.queries.warm.server_process.peak_rss_bytes"),
            _metric(result, "metrics.sync.one_file.process.peak_rss_bytes"),
            _metric(result, "metrics.sync.one_percent.process.peak_rss_bytes"),
        ]
        memory_source_labels = [
            result.get("metrics", {}).get("init", {}).get("peak_rss_source"),
            result.get("metrics", {})
            .get("queries", {})
            .get("cold", {})
            .get("peak_rss_source"),
            result.get("metrics", {})
            .get("queries", {})
            .get("warm", {})
            .get("server_process", {})
            .get("source"),
            result.get("metrics", {})
            .get("sync", {})
            .get("one_file", {})
            .get("process", {})
            .get("peak_rss_source"),
            result.get("metrics", {})
            .get("sync", {})
            .get("one_percent", {})
            .get("process", {})
            .get("peak_rss_source"),
        ]
        required_memory_sources = [
            "linux_wait4_direct_child_ru_maxrss",
            "linux_wait4_direct_child_ru_maxrss",
            "linux_proc_direct_child_poll",
            "linux_wait4_direct_child_ru_maxrss",
            "linux_wait4_direct_child_ru_maxrss",
        ]
        check(
            f"{role}_memory_metrics",
            result.get("host", {}).get("system") == "Linux"
            and all(value is not None and value > 0 for value in memory_sources)
            and memory_source_labels == required_memory_sources,
        )
        final_memory_complete = [
            result.get("metrics", {}).get("init", {}).get("memory_complete") is True,
            result.get("metrics", {})
            .get("queries", {})
            .get("cold", {})
            .get("memory_complete")
            is True,
            result.get("metrics", {})
            .get("sync", {})
            .get("one_file", {})
            .get("process", {})
            .get("memory_complete")
            is True,
            result.get("metrics", {})
            .get("sync", {})
            .get("one_percent", {})
            .get("process", {})
            .get("memory_complete")
            is True,
        ]
        check(
            f"{role}_final_memory_capture",
            all(final_memory_complete),
        )
        doctor_gate = doctor_index_gate(
            result.get("doctor"), int(_metric(result, "profile.file_count") or 0)
        )
        check(
            f"{role}_doctor_index",
            doctor_gate["status"] == "passed",
            doctor_gate["checks"],
        )
        layout = (
            result.get("metrics", {}).get("disk", {}).get("post_sync", {}).get("layout")
        )
        layout_gate = generation_layout_gate(layout, allow_legacy=role == "baseline")
        check(
            f"{role}_post_sync_generation_layout",
            layout_gate["status"] == "passed",
            layout_gate["checks"],
        )
        post_sync_allocated = _metric(result, "metrics.disk.post_sync.allocated_bytes")
        check(
            f"{role}_post_sync_disk_snapshot",
            post_sync_allocated is not None and post_sync_allocated > 0,
            post_sync_allocated,
        )
        sync_gate = sync_correctness_gate(result.get("metrics", {}).get("sync", {}))
        check(
            f"{role}_sync_token_correctness",
            sync_gate["status"] == "passed",
            sync_gate["checks"],
        )
        rewrite_sources = [
            result.get("metrics", {})
            .get("sync", {})
            .get(label, {})
            .get("effective_rewrite_bytes_source")
            for label in ("no_op", "one_file", "one_percent")
        ]
        process_io_complete = [
            result.get("metrics", {})
            .get("sync", {})
            .get(label, {})
            .get("process_io_complete")
            is True
            for label in ("no_op", "one_file", "one_percent")
        ]
        process_io_sources = [
            result.get("metrics", {})
            .get("sync", {})
            .get(label, {})
            .get("process", {})
            .get("io_source")
            for label in ("no_op", "one_file", "one_percent")
        ]
        check(
            f"{role}_process_io_rewrite_metrics",
            all(
                isinstance(source_label, str)
                and "direct_child_io_write_bytes" in source_label
                for source_label in rewrite_sources
            )
            and all(process_io_complete)
            and process_io_sources == ["linux_proc_direct_child_io_final"] * 3,
            {
                "rewrite_sources": rewrite_sources,
                "process_io_sources": process_io_sources,
            },
        )
        cold_cases = (
            result.get("metrics", {})
            .get("quality", {})
            .get("synthetic_exact", {})
            .get("cases", [])
        )
        check(
            f"{role}_cold_validation",
            bool(cold_cases) and all(case.get("found") is True for case in cold_cases),
        )
        check(
            f"{role}_warm_validation",
            result.get("metrics", {})
            .get("queries", {})
            .get("warm", {})
            .get("validation", {})
            .get("all_found")
            is True,
        )

    return {
        "status": (
            "passed" if all(item["status"] == "passed" for item in checks) else "failed"
        ),
        "checks": checks,
    }


def compare_to_baseline(
    current: dict[str, Any],
    baseline: dict[str, Any],
    ratios: dict[str, float],
    *,
    strict_claim: bool = False,
    expected_current_revision: str | None = None,
    expected_baseline_revision: str | None = None,
) -> dict[str, Any]:
    schema_checks = []
    for role, result in (("current", current), ("baseline", baseline)):
        value = result.get("schema_version")
        valid = (
            isinstance(value, int)
            and not isinstance(value, bool)
            and value == SCHEMA_VERSION
        )
        schema_checks.append(
            {
                "name": f"{role}_schema_version",
                "value": value,
                "required": SCHEMA_VERSION,
                "status": "passed" if valid else "failed",
            }
        )
    if any(check["status"] != "passed" for check in schema_checks):
        failed = [
            check["name"] for check in schema_checks if check["status"] != "passed"
        ]
        return {
            "status": "failed",
            "reason": "unsupported benchmark result schema: " + ", ".join(failed),
            "checks": schema_checks,
        }

    claim_evidence = None
    if strict_claim:
        policy_gate = strict_claim_policy_gate(ratios)
        if policy_gate["status"] != "passed":
            failed = [
                check["name"]
                for check in policy_gate["checks"]
                if check["status"] != "passed"
            ]
            return {
                "status": "failed",
                "reason": "invalid strict-claim policy: " + ", ".join(failed),
                "checks": policy_gate["checks"],
                "claim_policy": policy_gate,
            }
        claim_evidence = strict_claim_evidence_gate(
            current,
            baseline,
            expected_current_revision=expected_current_revision,
            expected_baseline_revision=expected_baseline_revision,
        )
        if claim_evidence["status"] != "passed":
            failed = [
                check["name"]
                for check in claim_evidence["checks"]
                if check["status"] != "passed"
            ]
            return {
                "status": "failed",
                "reason": "invalid strict-claim evidence: " + ", ".join(failed),
                "checks": claim_evidence["checks"],
                "claim_evidence": claim_evidence,
            }
    worker_evidence = worker_comparison_gate(current, baseline)
    if worker_evidence["status"] != "passed":
        failed = [
            check["name"]
            for check in worker_evidence["checks"]
            if check["status"] != "passed"
        ]
        return {
            "status": "failed",
            "reason": "invalid fixed-worker evidence: " + ", ".join(failed),
            "checks": worker_evidence["checks"],
            "worker_evidence": worker_evidence,
        }
    invalid_provenance = []
    for role, result, expected_revision in (
        ("current", current, expected_current_revision if strict_claim else None),
        ("baseline", baseline, expected_baseline_revision if strict_claim else None),
    ):
        gate = source_provenance_gate(result.get("source"), expected_revision)
        failed = [
            check["name"] for check in gate["checks"] if check["status"] != "passed"
        ]
        if failed:
            invalid_provenance.append(f"{role}.source ({', '.join(failed)})")
    if invalid_provenance:
        return {
            "status": "failed",
            "reason": "invalid or dirty source provenance: "
            + ", ".join(invalid_provenance),
            "checks": [],
        }

    invalid_binary_provenance = []
    for role, result in (("current", current), ("baseline", baseline)):
        gate = result_binary_source_provenance_gate(result)
        failed = [
            check["name"] for check in gate["checks"] if check["status"] != "passed"
        ]
        if failed:
            invalid_binary_provenance.append(f"{role} ({', '.join(failed)})")
    if invalid_binary_provenance:
        return {
            "status": "failed",
            "reason": "invalid binary/source provenance: "
            + ", ".join(invalid_binary_provenance),
            "checks": [],
        }

    compatibility_fields = [
        (
            "schema_version",
            current.get("schema_version"),
            baseline.get("schema_version"),
        ),
        (
            "measurement_scope",
            current.get("measurement_scope"),
            baseline.get("measurement_scope"),
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
            "profile.monitor_interval_ms",
            _metric(current, "profile.monitor_interval_ms"),
            _metric(baseline, "profile.monitor_interval_ms"),
        ),
        (
            "profile.timeout_s",
            _metric(current, "profile.timeout_s"),
            _metric(baseline, "profile.timeout_s"),
        ),
        (
            "configuration.threads",
            current.get("configuration", {}).get("threads"),
            baseline.get("configuration", {}).get("threads"),
        ),
        (
            "configuration.worker_mode",
            current.get("configuration", {}).get("worker_mode"),
            baseline.get("configuration", {}).get("worker_mode"),
        ),
        (
            "configuration.requested_threads",
            current.get("configuration", {}).get("requested_threads"),
            baseline.get("configuration", {}).get("requested_threads"),
        ),
        (
            "configuration.effective_worker_threads",
            current.get("configuration", {}).get("effective_worker_threads"),
            baseline.get("configuration", {}).get("effective_worker_threads"),
        ),
        (
            "configuration.embedding_enabled",
            current.get("configuration", {}).get("embedding_enabled"),
            baseline.get("configuration", {}).get("embedding_enabled"),
        ),
        (
            "configuration.build_profile",
            current.get("configuration", {}).get("build_profile"),
            baseline.get("configuration", {}).get("build_profile"),
        ),
        (
            "configuration.build_features",
            current.get("configuration", {}).get("build_features"),
            baseline.get("configuration", {}).get("build_features"),
        ),
        (
            "configuration.rustflags",
            current.get("configuration", {}).get("rustflags"),
            baseline.get("configuration", {}).get("rustflags"),
        ),
        (
            "configuration.build_environment",
            current.get("configuration", {}).get("build_environment"),
            baseline.get("configuration", {}).get("build_environment"),
        ),
        (
            "configuration.sync_repetitions",
            current.get("configuration", {}).get("sync_repetitions"),
            baseline.get("configuration", {}).get("sync_repetitions"),
        ),
        (
            "configuration.sync_mad_relative_limit",
            current.get("configuration", {}).get("sync_mad_relative_limit"),
            baseline.get("configuration", {}).get("sync_mad_relative_limit"),
        ),
        (
            "configuration.sync_mad_absolute_floor_ms",
            current.get("configuration", {}).get("sync_mad_absolute_floor_ms"),
            baseline.get("configuration", {}).get("sync_mad_absolute_floor_ms"),
        ),
        (
            "configuration.sync_iqr_relative_limit",
            current.get("configuration", {}).get("sync_iqr_relative_limit"),
            baseline.get("configuration", {}).get("sync_iqr_relative_limit"),
        ),
        (
            "configuration.sync_iqr_absolute_floor_ms",
            current.get("configuration", {}).get("sync_iqr_absolute_floor_ms"),
            baseline.get("configuration", {}).get("sync_iqr_absolute_floor_ms"),
        ),
        (
            "work_dir_parent",
            current.get("work_dir_parent"),
            baseline.get("work_dir_parent"),
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
            "host.filesystem.fragment_size",
            _metric(current, "host.filesystem.fragment_size"),
            _metric(baseline, "host.filesystem.fragment_size"),
        ),
        (
            "host.filesystem.name_max",
            _metric(current, "host.filesystem.name_max"),
            _metric(baseline, "host.filesystem.name_max"),
        ),
        (
            "source.rustc_version",
            current.get("source", {}).get("rustc_version"),
            baseline.get("source", {}).get("rustc_version"),
        ),
        (
            "fixture.schema_hash",
            current.get("fixture", {}).get("schema_hash"),
            baseline.get("fixture", {}).get("schema_hash"),
        ),
        (
            "fixture.manifest_sha256",
            current.get("fixture", {}).get("manifest_sha256"),
            baseline.get("fixture", {}).get("manifest_sha256"),
        ),
        (
            "metrics.source.file_count",
            _metric(current, "metrics.source.file_count"),
            _metric(baseline, "metrics.source.file_count"),
        ),
        (
            "metrics.source.total_bytes",
            _metric(current, "metrics.source.total_bytes"),
            _metric(baseline, "metrics.source.total_bytes"),
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

    unstable = []
    for role, result in (("current", current), ("baseline", baseline)):
        for label in ("no_op", "one_file"):
            stable = (
                result.get("metrics", {})
                .get("sync", {})
                .get(label, {})
                .get("wall_time_summary", {})
                .get("stable")
            )
            if stable is not True:
                unstable.append(f"{role}.metrics.sync.{label}")
    if unstable:
        return {
            "status": "failed",
            "reason": "unstable or missing repeated sync measurements: "
            + ", ".join(unstable),
            "checks": [],
        }

    checks: list[dict[str, Any]] = []
    mapping = {
        "max_init_ratio": ("metrics.init.wall_time_ms", "max"),
        "max_rss_ratio": ("metrics.init.peak_rss_bytes", "max"),
        "max_resident_rss_ratio": (
            "metrics.queries.warm.server_process.peak_rss_bytes",
            "max",
        ),
        "max_disk_ratio": ("metrics.disk.allocated_bytes", "max"),
        "max_post_sync_disk_ratio": (
            "metrics.disk.post_sync.allocated_bytes",
            "max",
        ),
        "max_cold_query_p95_ratio": ("metrics.queries.cold.p95_ms", "max"),
        "max_cold_query_rss_ratio": (
            "metrics.queries.cold.peak_rss_bytes",
            "max",
        ),
        "max_one_file_sync_ratio": (
            "metrics.sync.one_file.process.wall_time_ms",
            "max",
        ),
        "max_one_file_sync_rss_ratio": (
            "metrics.sync.one_file.process.peak_rss_bytes",
            "max",
        ),
        "max_one_file_rewrite_ratio": (
            "metrics.sync.one_file.effective_rewrite_bytes",
            "max",
        ),
        "max_one_percent_sync_ratio": (
            "metrics.sync.one_percent.process.wall_time_ms",
            "max",
        ),
        "max_one_percent_sync_rss_ratio": (
            "metrics.sync.one_percent.process.peak_rss_bytes",
            "max",
        ),
        "max_one_percent_rewrite_ratio": (
            "metrics.sync.one_percent.effective_rewrite_bytes",
            "max",
        ),
        "min_quality_ratio": ("metrics.quality.mrr", "min"),
        "min_recall_at_10_ratio": ("metrics.quality.recall_at_10", "min"),
    }
    policy_names = {
        "max_speed_suite_ratio",
        "max_speed_component_ratio",
        "max_one_file_sync_regression_ms",
        "max_warm_query_p95_ratio",
        "warm_query_absolute_floor_ms",
        "max_warm_query_regression_ms",
        "max_no_op_rewrite_bytes",
        "min_quality_mrr",
        "min_quality_recall_at_10",
    }
    for name, limit in ratios.items():
        if name in policy_names:
            continue
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

    if "max_warm_query_p95_ratio" in ratios:
        dotted = "metrics.queries.warm.client_round_trip.p95_ms"
        current_value = _metric(current, dotted)
        baseline_value = _metric(baseline, dotted)
        if current_value is None or baseline_value is None or baseline_value <= 0:
            checks.append(
                {
                    "name": "max_warm_query_p95_ratio",
                    "metric": dotted,
                    "status": "failed",
                    "reason": "metric missing or baseline is not positive",
                }
            )
        else:
            ratio_limit = ratios["max_warm_query_p95_ratio"]
            absolute_floor_ms = ratios.get("warm_query_absolute_floor_ms", 0.0)
            regression_delta_ms = ratios.get("max_warm_query_regression_ms", 0.0)
            ratio_target_ms = baseline_value * ratio_limit
            effective_limit_ms = min(
                max(absolute_floor_ms, ratio_target_ms),
                baseline_value + regression_delta_ms,
            )
            ratio = current_value / baseline_value
            checks.append(
                {
                    "name": "max_warm_query_p95_ratio",
                    "metric": dotted,
                    "current": current_value,
                    "baseline": baseline_value,
                    "ratio": ratio,
                    "ratio_limit": ratio_limit,
                    "absolute_floor_ms": absolute_floor_ms,
                    "max_regression_ms": regression_delta_ms,
                    "effective_limit_ms": effective_limit_ms,
                    "status": (
                        "passed" if current_value <= effective_limit_ms else "failed"
                    ),
                }
            )

    if "max_one_file_sync_regression_ms" in ratios:
        dotted = "metrics.sync.one_file.process.wall_time_ms"
        current_value = _metric(current, dotted)
        baseline_value = _metric(baseline, dotted)
        requested_delta_ms = ratios["max_one_file_sync_regression_ms"]
        if (
            current_value is None
            or baseline_value is None
            or baseline_value <= 0
            or not math.isfinite(requested_delta_ms)
            or requested_delta_ms < 0
        ):
            checks.append(
                {
                    "name": "max_one_file_sync_regression_ms",
                    "metric": dotted,
                    "status": "failed",
                    "reason": "metric missing, baseline invalid, or allowance invalid",
                }
            )
        else:
            allowed_delta_ms = min(requested_delta_ms, MAX_ONE_FILE_SYNC_REGRESSION_MS)
            effective_limit_ms = baseline_value + allowed_delta_ms
            checks.append(
                {
                    "name": "max_one_file_sync_regression_ms",
                    "metric": dotted,
                    "current": current_value,
                    "baseline": baseline_value,
                    "regression_ms": current_value - baseline_value,
                    "requested_delta_ms": requested_delta_ms,
                    "allowed_delta_ms": allowed_delta_ms,
                    "hard_cap_ms": MAX_ONE_FILE_SYNC_REGRESSION_MS,
                    "effective_limit_ms": effective_limit_ms,
                    "status": (
                        "passed" if current_value <= effective_limit_ms else "failed"
                    ),
                }
            )

    if "max_no_op_rewrite_bytes" in ratios:
        dotted = "metrics.sync.no_op.effective_rewrite_bytes"
        current_value = _metric(current, dotted)
        baseline_value = _metric(baseline, dotted)
        limit = ratios["max_no_op_rewrite_bytes"]
        if current_value is None:
            checks.append(
                {
                    "name": "max_no_op_rewrite_bytes",
                    "metric": dotted,
                    "status": "failed",
                    "reason": "metric missing",
                }
            )
        else:
            checks.append(
                {
                    "name": "max_no_op_rewrite_bytes",
                    "metric": dotted,
                    "current": current_value,
                    "baseline": baseline_value,
                    "limit": limit,
                    "status": "passed" if current_value <= limit else "failed",
                }
            )

    for name, dotted in (
        ("min_quality_mrr", "metrics.quality.mrr"),
        ("min_quality_recall_at_10", "metrics.quality.recall_at_10"),
    ):
        if name not in ratios:
            continue
        current_value = _metric(current, dotted)
        limit = ratios[name]
        checks.append(
            {
                "name": name,
                "metric": dotted,
                "current": current_value,
                "limit": limit,
                "status": (
                    "passed"
                    if current_value is not None and current_value >= limit
                    else "failed"
                ),
            }
        )
    speed_components = [
        ("init", "metrics.init.wall_time_ms"),
        ("cold_query_p95", "metrics.queries.cold.p95_ms"),
        ("no_op_sync", "metrics.sync.no_op.process.wall_time_ms"),
        ("one_file_sync", "metrics.sync.one_file.process.wall_time_ms"),
        ("one_percent_sync", "metrics.sync.one_percent.process.wall_time_ms"),
    ]
    if "max_speed_suite_ratio" in ratios or "max_speed_component_ratio" in ratios:
        component_ratios: list[float] = []
        component_evidence: list[dict[str, Any]] = []
        missing_components: list[str] = []
        for label, dotted in speed_components:
            current_value = _metric(current, dotted)
            baseline_value = _metric(baseline, dotted)
            if (
                current_value is None
                or baseline_value is None
                or current_value <= 0
                or baseline_value <= 0
            ):
                missing_components.append(label)
                if "max_speed_component_ratio" in ratios and label != "one_file_sync":
                    checks.append(
                        {
                            "name": f"max_speed_component_ratio:{label}",
                            "metric": dotted,
                            "status": "failed",
                            "reason": "metric missing or value is not positive",
                        }
                    )
                continue
            ratio = current_value / baseline_value
            component_ratios.append(ratio)
            component_evidence.append(
                {
                    "name": label,
                    "metric": dotted,
                    "current": current_value,
                    "baseline": baseline_value,
                    "ratio": ratio,
                }
            )
            if "max_speed_component_ratio" in ratios and label != "one_file_sync":
                limit = ratios["max_speed_component_ratio"]
                checks.append(
                    {
                        "name": f"max_speed_component_ratio:{label}",
                        "metric": dotted,
                        "current": current_value,
                        "baseline": baseline_value,
                        "ratio": ratio,
                        "limit": limit,
                        "status": "passed" if ratio <= limit else "failed",
                    }
                )

        if "max_speed_suite_ratio" in ratios:
            limit = ratios["max_speed_suite_ratio"]
            if missing_components:
                checks.append(
                    {
                        "name": "max_speed_suite_ratio",
                        "status": "failed",
                        "reason": "missing or invalid components: "
                        + ", ".join(missing_components),
                        "components": component_evidence,
                    }
                )
            else:
                geometric_mean = math.exp(
                    math.fsum(math.log(ratio) for ratio in component_ratios)
                    / len(component_ratios)
                )
                checks.append(
                    {
                        "name": "max_speed_suite_ratio",
                        "metric": "geometric_mean(current/baseline)",
                        "ratio": geometric_mean,
                        "limit": limit,
                        "components": component_evidence,
                        "status": "passed" if geometric_mean <= limit else "failed",
                    }
                )
    response = {
        "status": "passed"
        if all(check["status"] == "passed" for check in checks)
        else "failed",
        "checks": checks,
    }
    if claim_evidence is not None:
        response["claim_evidence"] = claim_evidence
        response["claim_policy"] = strict_claim_policy_gate(ratios)
        response["claim_scope"] = (
            "BM25-only five-operation geometric-mean speed suite; hybrid/vector "
            "initialization, memory, and disk excluded; single init, one-percent "
            "sync, and server-lifecycle measurements"
        )
    return response


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", choices=PROFILES, default="pr")
    parser.add_argument(
        "--evidence-mode",
        choices=("smoke", "regression", "claim-baseline", "strict-claim"),
        default="smoke",
        help="label the evidence scope; strict claims fail closed",
    )
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
    parser.add_argument(
        "--source-root",
        type=Path,
        help="source checkout that produced the benchmark binaries",
    )
    parser.add_argument(
        "--bootstrap-legacy-build",
        action="store_true",
        help="fresh-build an unattested historical baseline and write its receipt",
    )
    parser.add_argument(
        "--legacy-build-target",
        type=Path,
        help="fresh external Cargo target directory for legacy bootstrap",
    )
    parser.add_argument(
        "--legacy-build-provenance",
        type=Path,
        help="legacy bootstrap receipt output/input for a trusted baseline",
    )
    parser.add_argument("--expected-current-revision")
    parser.add_argument("--expected-baseline-revision")
    parser.add_argument("--build-profile", default="release")
    parser.add_argument("--build-features", default="default")
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
    parser.add_argument("--max-post-sync-disk-ratio", type=float, default=1.0)
    parser.add_argument("--max-cold-query-p95-ratio", type=float, default=1.0)
    parser.add_argument("--max-cold-query-rss-ratio", type=float, default=1.0)
    parser.add_argument("--max-warm-query-p95-ratio", type=float, default=1.0)
    parser.add_argument("--warm-query-absolute-floor-ms", type=float, default=10.0)
    parser.add_argument("--max-warm-query-regression-ms", type=float, default=2.0)
    parser.add_argument("--max-speed-suite-ratio", type=float, default=1.0)
    parser.add_argument("--max-speed-component-ratio", type=float, default=1.0)
    parser.add_argument("--max-one-file-sync-ratio", type=float)
    parser.add_argument("--max-one-file-sync-rss-ratio", type=float, default=1.0)
    parser.add_argument("--max-one-file-sync-regression-ms", type=float, default=0.0)
    parser.add_argument("--max-one-file-rewrite-ratio", type=float, default=1.0)
    parser.add_argument("--max-one-percent-sync-ratio", type=float, default=1.0)
    parser.add_argument("--max-one-percent-sync-rss-ratio", type=float, default=1.0)
    parser.add_argument("--max-one-percent-rewrite-ratio", type=float, default=1.0)
    parser.add_argument("--max-no-op-rewrite-bytes", type=float, default=0.0)
    parser.add_argument("--min-quality-ratio", type=float, default=0.99)
    parser.add_argument("--min-recall-at-10-ratio", type=float, default=0.99)
    parser.add_argument("--min-quality-mrr", type=float, default=0.0)
    parser.add_argument("--min-quality-recall-at-10", type=float, default=0.0)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.bootstrap_legacy_build:
        if (
            args.source_root is None
            or args.legacy_build_target is None
            or args.legacy_build_provenance is None
        ):
            raise ValueError(
                "legacy bootstrap requires --source-root, --legacy-build-target, "
                "and --legacy-build-provenance"
            )
        receipt = bootstrap_legacy_build_provenance(
            args.source_root,
            args.legacy_build_target,
            args.legacy_build_provenance,
        )
        print(json.dumps(receipt, indent=2, sort_keys=True))
        return 0
    if args.legacy_build_target is not None:
        raise ValueError("--legacy-build-target is only valid with legacy bootstrap")
    if args.legacy_build_provenance is not None and args.source_root is None:
        raise ValueError("legacy build provenance requires an explicit --source-root")
    if args.legacy_build_provenance is not None and args.evidence_mode not in {
        "regression",
        "claim-baseline",
    }:
        raise ValueError(
            "legacy build provenance is baseline-only and requires regression "
            "or claim-baseline evidence mode"
        )
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
    comparison_capture = args.evidence_mode in {
        "regression",
        "claim-baseline",
        "strict-claim",
    }
    if comparison_capture and args.threads is None:
        raise ValueError(
            "regression and claim evidence require an explicit --threads value"
        )
    claim_capture = args.evidence_mode in {"claim-baseline", "strict-claim"}
    if claim_capture and args.profile != "100k":
        raise ValueError("claim evidence requires --profile 100k")
    if claim_capture and profile != PROFILES["100k"]:
        raise ValueError("claim evidence forbids 100K profile overrides")
    if claim_capture and not _is_hex_identifier(
        args.expected_current_revision, (40, 64)
    ):
        raise ValueError("claim evidence requires --expected-current-revision")
    if args.evidence_mode == "strict-claim":
        if args.baseline is None:
            raise ValueError("strict claim requires --baseline")
        if not _is_hex_identifier(args.expected_baseline_revision, (40, 64)):
            raise ValueError("strict claim requires --expected-baseline-revision")
        for name, value in (
            ("min-quality-ratio", args.min_quality_ratio),
            ("min-recall-at-10-ratio", args.min_recall_at_10_ratio),
            ("min-quality-mrr", args.min_quality_mrr),
            ("min-quality-recall-at-10", args.min_quality_recall_at_10),
        ):
            if not math.isfinite(value) or not 0 < value <= 1:
                raise ValueError(f"strict claim requires {name} in (0, 1]")
        strict_policy = {
            name: getattr(args, name)
            for name in (*STRICT_CLAIM_MAXIMUMS, *STRICT_CLAIM_MINIMUMS)
        }
        policy_gate = strict_claim_policy_gate(strict_policy)
        if policy_gate["status"] != "passed":
            failed = [
                check["name"]
                for check in policy_gate["checks"]
                if check["status"] != "passed"
            ]
            raise ValueError(
                "strict claim requires the documented policy; invalid: "
                + ", ".join(failed)
            )
    if claim_capture and args.external_quality_result is None:
        raise ValueError("claim evidence requires --external-quality-result")
    if (
        not math.isfinite(args.max_one_file_sync_regression_ms)
        or not 0
        <= args.max_one_file_sync_regression_ms
        <= MAX_ONE_FILE_SYNC_REGRESSION_MS
    ):
        raise ValueError(
            "max one-file sync regression must be between 0 and 500 milliseconds"
        )

    invocation_root = Path.cwd()
    source_root = (
        args.source_root.expanduser().resolve()
        if args.source_root is not None
        else invocation_root
    )
    if not source_root.is_dir():
        raise NotADirectoryError(f"source root is not a directory: {source_root}")
    codixing = args.codixing.expanduser().resolve()
    server = args.server.expanduser().resolve()
    if not codixing.is_file() or not os.access(codixing, os.X_OK):
        raise FileNotFoundError(f"codixing binary is not executable: {codixing}")
    if not server.is_file() or not os.access(server, os.X_OK):
        raise FileNotFoundError(f"server binary is not executable: {server}")

    baseline_result = (
        json.loads(args.baseline.read_text()) if args.baseline is not None else None
    )
    external_quality = load_external_quality(
        args.external_quality_result,
        require_attribution=claim_capture,
        expected_revision=args.expected_current_revision if claim_capture else None,
    )

    parent = (
        args.work_dir.expanduser().resolve()
        if args.work_dir
        else Path(tempfile.gettempdir()).resolve()
    )
    parent.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="codixing-large-repo-", dir=parent))
    ownership_nonce = secrets.token_hex(32)
    root_identity = (root.stat().st_dev, root.stat().st_ino)
    (root / FIXTURE_MARKER).write_text(fixture_marker_contents(ownership_nonce))
    result: dict[str, Any] = {
        "schema_version": SCHEMA_VERSION,
        "measurement_scope": MEASUREMENT_SCOPE,
        "generated_at_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "profile_name": args.profile,
        "evidence_mode": args.evidence_mode,
        "claim_scope": (
            "BM25-only five-operation geometric-mean speed suite"
            if claim_capture
            else "regression-only"
            if args.evidence_mode == "regression"
            else "smoke-only"
        ),
        "measurement_limitations": [
            "initialization is measured once",
            "one-percent sync is measured once",
            "one resident-server lifecycle is measured",
            "macOS RSS is direct-process polling; strict memory claims require Linux",
        ],
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
        "source": source_metadata(source_root, codixing),
        "configuration": {
            "threads": args.threads,
            "worker_mode": "fixed" if args.threads is not None else "shipping-default",
            "requested_threads": args.threads,
            "effective_worker_threads": args.threads,
            "embedding_enabled": False,
            "build_profile": args.build_profile,
            "build_features": args.build_features,
            "rustflags": os.environ.get("RUSTFLAGS", ""),
            "build_environment": build_environment_metadata(),
            "sync_repetitions": SYNC_REPETITIONS,
            "sync_mad_relative_limit": SYNC_MAD_RELATIVE_LIMIT,
            "sync_mad_absolute_floor_ms": SYNC_MAD_ABSOLUTE_FLOOR_MS,
            "sync_iqr_relative_limit": SYNC_IQR_RELATIVE_LIMIT,
            "sync_iqr_absolute_floor_ms": SYNC_IQR_ABSOLUTE_FLOOR_MS,
        },
        "binaries": {
            "codixing": {
                "path": str(codixing),
                "sha256": file_sha256(codixing),
                "build_provenance": capture_build_provenance(codixing),
            },
            "server": {
                "path": str(server),
                "sha256": file_sha256(server),
                "build_provenance": capture_build_provenance(server),
            },
        },
        "work_dir_parent": str(parent),
        "fixture_root": str(root),
        "metrics": {},
    }
    exit_code = 0
    legacy_build_provenance = apply_legacy_build_provenance(
        result["source"],
        result["binaries"],
        result["configuration"],
        args.legacy_build_provenance,
    )
    if legacy_build_provenance is not None:
        result["legacy_build_provenance"] = legacy_build_provenance
    result["source_provenance_gate"] = source_provenance_gate(
        result["source"],
        args.expected_current_revision if claim_capture else None,
    )
    result["binary_source_provenance_gate"] = result_binary_source_provenance_gate(
        result
    )
    if claim_capture and (
        result["source_provenance_gate"]["status"] != "passed"
        or result["binary_source_provenance_gate"]["status"] != "passed"
    ):
        exit_code = 2
    try:
        files = generate_fixture(root, profile.file_count, ownership_nonce)
        result["fixture"].update(
            {
                "manifest_sha256": fixture_manifest_hash(root, files),
                "file_count": len(files),
            }
        )
        cases = load_quality_cases(args.quality_file, profile.file_count)

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

        no_op_samples = []
        one_file_samples = []
        last_index = len(files) - 1
        one_file_paths = [
            files[(repetition * last_index) // (SYNC_REPETITIONS - 1)]
            for repetition in range(SYNC_REPETITIONS)
        ]
        for repetition, edit_path in enumerate(one_file_paths, start=1):
            no_op_sample = sync_scenario("no_op", codixing, root, profile, args.threads)
            no_op_sample["repetition"] = repetition
            no_op_samples.append(no_op_sample)

            mutation = edit_files([edit_path], 1, f"one-file-{repetition}")[0]
            one_file_sample = sync_scenario(
                "one_file", codixing, root, profile, args.threads
            )
            one_file_sample["repetition"] = repetition
            one_file_sample["edited_file"] = str(edit_path.relative_to(root))
            one_file_sample["validation"] = validate_sync_token(
                codixing,
                root,
                mutation[1],
                mutation[0],
                profile.timeout_s,
            )
            one_file_samples.append(one_file_sample)

        no_op = aggregate_sync_samples("no_op", no_op_samples)
        one_file = aggregate_sync_samples("one_file", one_file_samples)
        one_file["edited_files_per_repetition"] = 1
        one_file["distinct_edited_files"] = len(set(one_file_paths))
        one_percent_count = max(1, math.ceil(profile.file_count * 0.01))
        one_percent_mutations = edit_files(
            files, one_percent_count, "one-percent", spread=True
        )
        one_percent = sync_scenario(
            "one_percent", codixing, root, profile, args.threads
        )
        one_percent["edited_files"] = one_percent_count
        last_mutation = len(one_percent_mutations) - 1
        one_percent["validations"] = []
        validation_points = [("first", 0)]
        if last_mutation >= 2:
            validation_points.append(("middle", last_mutation // 2))
        if last_mutation >= 1:
            validation_points.append(("last", last_mutation))
        for position, index in validation_points:
            mutation = one_percent_mutations[index]
            validation = validate_sync_token(
                codixing,
                root,
                mutation[1],
                mutation[0],
                profile.timeout_s,
            )
            validation["position"] = position
            one_percent["validations"].append(validation)
        result["metrics"]["sync"] = {
            "no_op": no_op,
            "one_file": one_file,
            "one_percent": one_percent,
        }
        result["measurement_gate"] = sync_measurement_gate(result["metrics"]["sync"])
        if result["measurement_gate"]["status"] != "passed":
            exit_code = 2

        doctor = run_measured(
            [str(codixing), "doctor", "--json", "."],
            cwd=root,
            timeout_s=profile.timeout_s,
            monitor_interval_ms=profile.monitor_interval_ms,
        )
        result["doctor"] = json.loads(doctor.stdout)
        result["doctor_gate"] = doctor_index_gate(result["doctor"], profile.file_count)
        post_sync_snapshot = disk_snapshot(root / ".codixing")
        post_sync_disk = disk_usage(post_sync_snapshot)
        post_sync_disk["layout"] = audit_generation_layout(
            result["doctor"], post_sync_snapshot
        )
        result["generation_layout_gate"] = generation_layout_gate(
            post_sync_disk["layout"],
            allow_legacy=args.evidence_mode != "strict-claim",
        )
        result["metrics"]["disk"]["post_sync"] = post_sync_disk
        result["sync_correctness_gate"] = sync_correctness_gate(
            result["metrics"]["sync"]
        )

        warm_all_found = warm.get("validation", {}).get("all_found") is True
        if (
            recall_at_10 < 1.0
            or not warm_all_found
            or result["doctor_gate"]["status"] != "passed"
            or result["generation_layout_gate"]["status"] != "passed"
            or result["sync_correctness_gate"]["status"] != "passed"
        ):
            result["correctness_gate"] = {
                "status": "failed",
                "reason": (
                    "search, post-sync token, doctor, or generation-layout validation failed"
                ),
                "doctor": result["doctor_gate"],
                "generation_layout": result["generation_layout_gate"],
                "sync_tokens": result["sync_correctness_gate"],
            }
            exit_code = 2
        else:
            result["correctness_gate"] = {"status": "passed"}

        if baseline_result is not None:
            ratios = {
                "max_init_ratio": args.max_init_ratio,
                "max_rss_ratio": args.max_rss_ratio,
                "max_resident_rss_ratio": args.max_resident_rss_ratio,
                "max_disk_ratio": args.max_disk_ratio,
                "max_post_sync_disk_ratio": args.max_post_sync_disk_ratio,
                "max_cold_query_p95_ratio": args.max_cold_query_p95_ratio,
                "max_cold_query_rss_ratio": args.max_cold_query_rss_ratio,
                "max_warm_query_p95_ratio": args.max_warm_query_p95_ratio,
                "warm_query_absolute_floor_ms": args.warm_query_absolute_floor_ms,
                "max_warm_query_regression_ms": args.max_warm_query_regression_ms,
                "max_speed_suite_ratio": args.max_speed_suite_ratio,
                "max_speed_component_ratio": args.max_speed_component_ratio,
                "max_one_file_sync_regression_ms": (
                    args.max_one_file_sync_regression_ms
                ),
                "max_one_file_sync_rss_ratio": args.max_one_file_sync_rss_ratio,
                "max_one_file_rewrite_ratio": args.max_one_file_rewrite_ratio,
                "max_one_percent_sync_ratio": args.max_one_percent_sync_ratio,
                "max_one_percent_sync_rss_ratio": (args.max_one_percent_sync_rss_ratio),
                "max_one_percent_rewrite_ratio": args.max_one_percent_rewrite_ratio,
                "max_no_op_rewrite_bytes": args.max_no_op_rewrite_bytes,
                "min_quality_ratio": args.min_quality_ratio,
                "min_recall_at_10_ratio": args.min_recall_at_10_ratio,
                "min_quality_mrr": args.min_quality_mrr,
                "min_quality_recall_at_10": args.min_quality_recall_at_10,
            }
            if args.max_one_file_sync_ratio is not None:
                ratios["max_one_file_sync_ratio"] = args.max_one_file_sync_ratio
            result["performance_gate"] = compare_to_baseline(
                result,
                baseline_result,
                ratios,
                strict_claim=args.evidence_mode == "strict-claim",
                expected_current_revision=args.expected_current_revision,
                expected_baseline_revision=args.expected_baseline_revision,
            )
            if result["performance_gate"]["status"] != "passed":
                exit_code = 2
        else:
            result["performance_gate"] = {
                "status": "recorded",
                "reason": (
                    "claim baseline captured; no treatment comparison was evaluated"
                    if args.evidence_mode == "claim-baseline"
                    else "no --baseline supplied; no performance claim was evaluated"
                ),
            }
    finally:
        if args.keep_work_dir:
            result["cleanup_gate"] = {
                "status": "recorded",
                "reason": "--keep-work-dir requested",
            }
        elif root.exists():
            try:
                cleanup_owned_fixture(root, ownership_nonce, root_identity)
            except (OSError, RuntimeError) as error:
                exit_code = 2
                result["cleanup_gate"] = {
                    "status": "failed",
                    "reason": str(error),
                }
                print(f"refusing unsafe fixture cleanup: {error}", file=sys.stderr)
            else:
                result["cleanup_gate"] = {"status": "passed"}
        else:
            exit_code = 2
            result["cleanup_gate"] = {
                "status": "failed",
                "reason": "fixture root disappeared before owned cleanup",
            }
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")

    print(json.dumps(result, indent=2, sort_keys=True))
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Unit tests for the large-repository benchmark gate's pure logic."""

import copy
import io
import json
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import large_repo_gate as gate  # noqa: E402


class LargeRepoGateTests(unittest.TestCase):
    def test_percentile_interpolates_and_summarizes(self):
        self.assertEqual(gate.percentile([], 0.95), None)
        self.assertEqual(gate.percentile([7.0], 0.95), 7.0)
        self.assertAlmostEqual(gate.percentile([1.0, 2.0, 3.0, 4.0], 0.5), 2.5)
        summary = gate.latency_summary([4.0, 1.0, 3.0, 2.0])
        self.assertEqual(summary["runs"], 4)
        self.assertEqual(summary["p50_ms"], 2.5)
        self.assertEqual(summary["median_ms"], 2.5)

    def test_sync_sample_aggregation_uses_median_latency_and_worst_rewrite(self):
        samples = []
        for repetition, (wall_time, rewritten) in enumerate(
            zip([100.0, 110.0, 120.0, 130.0, 500.0], [1, 2, 3, 4, 5]),
            start=1,
        ):
            samples.append(
                {
                    "label": "no_op",
                    "repetition": repetition,
                    "process": {
                        "wall_time_ms": wall_time,
                        "peak_rss_bytes": repetition * 1_000,
                    },
                    "artifact_bytes_rewritten_estimate": rewritten,
                    "artifact_bytes_rewritten_estimate_source": "test",
                    "disk_delta_bytes": repetition,
                }
            )

        aggregate = gate.aggregate_sync_samples("no_op", samples)

        self.assertEqual(aggregate["process"]["wall_time_ms"], 120.0)
        self.assertEqual(aggregate["process"]["peak_rss_bytes"], 5_000)
        self.assertEqual(aggregate["artifact_bytes_rewritten_estimate"], 5)
        self.assertEqual(aggregate["artifact_bytes_rewritten_samples"], [1, 2, 3, 4, 5])
        self.assertEqual(aggregate["wall_time_summary"]["mad_ms"], 10.0)
        self.assertEqual(aggregate["wall_time_summary"]["iqr_ms"], 20.0)
        self.assertTrue(aggregate["wall_time_summary"]["stable"])
        self.assertEqual(aggregate["samples"], samples)

    def test_sync_measurement_gate_rejects_unstable_mad_and_bimodal_iqr(self):
        unstable = gate.sync_latency_summary([100.0, 200.0, 300.0, 400.0, 500.0])
        self.assertEqual(unstable["mad_ms"], 100.0)
        self.assertEqual(unstable["jitter_limit_ms"], 50.0)
        self.assertFalse(unstable["stable"])

        bimodal = gate.sync_latency_summary([100.0, 100.0, 100.0, 10_000.0, 10_000.0])
        self.assertEqual(bimodal["mad_ms"], 0.0)
        self.assertEqual(bimodal["iqr_ms"], 9_900.0)
        self.assertFalse(bimodal["stable"])

        measurement = gate.sync_measurement_gate(
            {
                "no_op": {"wall_time_summary": bimodal},
                "one_file": {
                    "wall_time_summary": gate.sync_latency_summary(
                        [300.0, 310.0, 320.0, 330.0, 340.0]
                    )
                },
            }
        )
        self.assertEqual(measurement["status"], "failed")
        self.assertEqual(measurement["checks"][0]["status"], "failed")

    def test_disk_breakdown_and_rewrite_estimate_are_explicit(self):
        before = {
            "tantivy/a": gate.DiskEntry(10, 1, 12, 1, 1),
            "graph.bin": gate.DiskEntry(5, 1, 8, 1, 2),
            "gone": gate.DiskEntry(2, 1, 4, 1, 3),
        }
        after = {
            "tantivy/a": gate.DiskEntry(12, 2, 16, 1, 4),
            "graph.bin": gate.DiskEntry(5, 1, 8, 1, 2),
            "new": gate.DiskEntry(3, 1, 4, 1, 5),
        }
        usage = gate.disk_usage(after)
        self.assertEqual(usage["total_bytes"], 20)
        self.assertEqual(usage["allocated_bytes"], 28)
        self.assertEqual(usage["artifacts_bytes"]["tantivy"], 12)
        self.assertEqual(gate.rewritten_bytes_estimate(before, after), 20)

    def test_rewrite_estimate_ignores_unchanged_generation_hardlinks(self):
        shared = gate.DiskEntry(10_000, 1, 4_096, 7, 11)
        before = {"generations/a/tantivy/seg": shared}
        after = {
            "generations/a/tantivy/seg": shared,
            "generations/b/tantivy/seg": shared,
        }
        self.assertEqual(gate.rewritten_bytes_estimate(before, after), 0)

        after["generations/b/symbols.bin"] = gate.DiskEntry(8_000, 2, 8_192, 7, 12)
        self.assertEqual(gate.rewritten_bytes_estimate(before, after), 8_192)

    def test_rewrite_estimate_does_not_charge_deleted_artifact_bytes(self):
        before = {"retired.bin": gate.DiskEntry(10, 1, 4_096, 7, 11)}
        self.assertEqual(gate.rewritten_bytes_estimate(before, {}), 0)

    def test_disk_usage_deduplicates_hardlinks_and_unwraps_generations(self):
        snapshot = {
            "generations/a/tantivy/seg": gate.DiskEntry(10, 1, 12, 7, 11),
            "generations/b/tantivy/seg": gate.DiskEntry(10, 1, 12, 7, 11),
            "generations/b/symbols.bin": gate.DiskEntry(5, 1, 8, 7, 12),
        }
        usage = gate.disk_usage(snapshot)
        self.assertEqual(usage["total_bytes"], 25)
        self.assertEqual(usage["unique_inode_logical_bytes"], 15)
        self.assertEqual(usage["allocated_bytes"], 20)
        self.assertEqual(usage["hardlink_duplicate_logical_bytes"], 10)
        self.assertEqual(usage["artifacts_bytes"]["tantivy"], 20)
        self.assertEqual(usage["artifacts_allocated_bytes"]["tantivy"], 12)

    def test_generation_layout_gate_rejects_accumulating_generations(self):
        layout = {
            "kind": "generational",
            "generation_count": 1,
            "active_generation": "gen-0000000000000002",
            "abandoned_generations": [],
            "observed_generations": ["gen-0000000000000002"],
        }
        self.assertEqual(gate.generation_layout_gate(layout)["status"], "passed")

        accumulated = copy.deepcopy(layout)
        accumulated["generation_count"] = 2
        accumulated["abandoned_generations"] = ["gen-0000000000000001"]
        accumulated["observed_generations"] = [
            "gen-0000000000000001",
            "gen-0000000000000002",
        ]
        audit = gate.generation_layout_gate(accumulated)
        self.assertEqual(audit["status"], "failed")
        self.assertEqual(
            {check["name"] for check in audit["checks"] if check["status"] == "failed"},
            {
                "supported_layout",
                "one_active_generation_or_clean_legacy",
                "no_abandoned_generations",
            },
        )

    def test_layout_audit_allows_clean_legacy_baseline_only(self):
        snapshot = {"meta.json": gate.DiskEntry(10, 1, 12, 7, 11)}
        layout = gate.audit_generation_layout({"index": {"status": "ok"}}, snapshot)
        self.assertEqual(layout["kind"], "legacy_flat")
        self.assertEqual(
            gate.generation_layout_gate(layout, allow_legacy=True)["status"],
            "passed",
        )
        self.assertEqual(gate.generation_layout_gate(layout)["status"], "failed")

    def test_sync_token_validation_rejects_fake_no_op_sync(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            edited = root / "src" / "edited.rs"
            edited.parent.mkdir()
            edited.write_text("pub fn old_state() {}\n")
            fake_no_op = gate.subprocess.CompletedProcess(
                args=[], returncode=0, stdout="[]", stderr=""
            )
            with mock.patch.object(gate.subprocess, "run", return_value=fake_no_op):
                validation = gate.validate_sync_token(
                    Path("codixing"),
                    root,
                    "codixing_benchmark_sync_one_file_000001",
                    edited,
                    5,
                )

        self.assertEqual(validation["status"], "failed")
        sync_metrics = {
            "one_file": {
                "repetitions": gate.SYNC_REPETITIONS,
                "distinct_edited_files": 1,
                "validations": [validation] * gate.SYNC_REPETITIONS,
            },
            "one_percent": {
                "edited_files": 3,
                "validations": [
                    {
                        "position": position,
                        "token": f"token_{index}",
                        "expected_file": f"src/{index}.rs",
                        "status": "passed",
                    }
                    for index, position in enumerate(("first", "middle", "last"))
                ],
            },
        }
        self.assertEqual(gate.sync_correctness_gate(sync_metrics)["status"], "failed")

    def test_sync_correctness_gate_accepts_tiny_overrides_and_repeated_paths(self):
        one_file = {
            "repetitions": gate.SYNC_REPETITIONS,
            "distinct_edited_files": 1,
            "validations": [
                {
                    "token": f"one_file_{index}",
                    "expected_file": "src/only.rs",
                    "status": "passed",
                }
                for index in range(gate.SYNC_REPETITIONS)
            ],
        }
        for edited_files, positions in (
            (1, ("first",)),
            (2, ("first", "last")),
        ):
            one_percent = {
                "edited_files": edited_files,
                "validations": [
                    {
                        "position": position,
                        "token": f"one_percent_{index}",
                        "expected_file": f"src/{index}.rs",
                        "status": "passed",
                    }
                    for index, position in enumerate(positions)
                ],
            }
            with self.subTest(edited_files=edited_files):
                self.assertEqual(
                    gate.sync_correctness_gate(
                        {"one_file": one_file, "one_percent": one_percent}
                    )["status"],
                    "passed",
                )

    def test_snapshot_detects_mtime_only_rewrites(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifact = root / "symbols.bin"
            artifact.write_bytes(b"same size")
            before = gate.disk_snapshot(root)
            time.sleep(0.002)
            artifact.write_bytes(b"same size")
            after = gate.disk_snapshot(root)
            self.assertGreater(gate.rewritten_bytes_estimate(before, after), 0)

    def test_darwin_live_rss_uses_ps_kib_and_labels_the_source(self):
        completed = mock.Mock(returncode=0, stdout="  12345\n")
        with (
            mock.patch.object(gate.sys, "platform", "darwin"),
            mock.patch.object(gate.subprocess, "run", return_value=completed) as run,
        ):
            rss, pss, source = gate._live_process_memory_stats(42)

        self.assertEqual(rss, 12_641_280)
        self.assertIsNone(pss)
        self.assertEqual(source, "darwin_ps_rss_poll")
        run.assert_called_once_with(
            ["ps", "-o", "rss=", "-p", "42"],
            capture_output=True,
            text=True,
            check=False,
            timeout=2,
        )

    def test_darwin_live_rss_failure_remains_explicitly_missing(self):
        completed = mock.Mock(returncode=1, stdout="")
        with (
            mock.patch.object(gate.sys, "platform", "darwin"),
            mock.patch.object(gate.subprocess, "run", return_value=completed),
        ):
            self.assertEqual(gate._live_process_memory_stats(42), (None, None, None))

    def test_run_measured_uses_per_child_darwin_rss_samples(self):
        with tempfile.TemporaryDirectory() as directory:
            with (
                mock.patch.object(gate.sys, "platform", "darwin"),
                mock.patch.object(
                    gate, "_darwin_process_rss_bytes", return_value=12_345
                ),
            ):
                measured = gate.run_measured(
                    [sys.executable, "-c", "import time; time.sleep(0.05)"],
                    cwd=Path(directory),
                    timeout_s=5,
                    monitor_interval_ms=5,
                )

        self.assertEqual(measured.peak_rss_bytes, 12_345)
        self.assertEqual(measured.peak_rss_source, "darwin_ps_rss_poll")

    def test_run_measured_closes_pipe_handles_on_every_exit_path(self):
        real_popen = gate.subprocess.Popen
        cases = (
            ("success", "pass", None, 5),
            ("error", "raise SystemExit(3)", RuntimeError, 5),
            ("timeout", "import time; time.sleep(30)", TimeoutError, 0.01),
        )
        with tempfile.TemporaryDirectory() as directory:
            for label, script, expected_error, timeout_s in cases:
                spawned = []

                def recording_popen(*args, **kwargs):
                    process = real_popen(*args, **kwargs)
                    if args and args[0] == [sys.executable, "-c", script]:
                        spawned.append(process)
                    return process

                with (
                    self.subTest(label=label),
                    mock.patch.object(
                        gate.subprocess, "Popen", side_effect=recording_popen
                    ),
                ):
                    if expected_error is None:
                        gate.run_measured(
                            [sys.executable, "-c", script],
                            cwd=Path(directory),
                            timeout_s=timeout_s,
                            monitor_interval_ms=1,
                        )
                    else:
                        with self.assertRaises(expected_error):
                            gate.run_measured(
                                [sys.executable, "-c", script],
                                cwd=Path(directory),
                                timeout_s=timeout_s,
                                monitor_interval_ms=1,
                            )

                    self.assertEqual(len(spawned), 1)
                    self.assertTrue(spawned[0].stdout.closed)
                    self.assertTrue(spawned[0].stderr.closed)

    def test_spread_edit_selection_covers_the_full_fixture(self):
        paths = [Path(f"file-{index}") for index in range(100)]
        selected = gate._select_edit_paths(paths, 10, spread=True)
        self.assertEqual(selected[0], paths[0])
        self.assertEqual(selected[-1], paths[-1])
        self.assertEqual(len(selected), 10)
        self.assertEqual(len(set(selected)), 10)
        self.assertEqual(gate._select_edit_paths(paths, 3, spread=False), paths[:3])

    def test_source_metadata_uses_explicit_root_not_github_sha(self):
        source_root = Path("/tmp/pinned-baseline-source")
        codixing = Path("/tmp/pinned-baseline-bin/codixing")

        def completed(command, **kwargs):
            self.assertEqual(kwargs["cwd"], source_root)
            if command[0] == "git":
                self.assertFalse(
                    any(name.upper().startswith("GIT_") for name in kwargs["env"])
                )
            output = {
                (str(codixing), "--version"): "codixing 0.45.0\n",
                (
                    gate.os.environ.get("RUSTC", "rustc"),
                    "--version",
                    "--verbose",
                ): "rustc 1.88.0\n",
                ("git", "rev-parse", "HEAD"): "baseline-revision\n",
                ("git", "rev-parse", "HEAD^{tree}"): "baseline-tree\n",
                ("git", "status", "--porcelain"): "",
                ("git", "diff", "--binary", "HEAD", "--"): "",
                ("git", "ls-files", "--others", "--exclude-standard"): "",
            }[tuple(command)]
            return mock.Mock(returncode=0, stdout=output)

        with (
            mock.patch.dict(
                gate.os.environ,
                {
                    "GITHUB_SHA": "current-pr-revision",
                    "GIT_DIR": "/tmp/poisoned-git-dir",
                    "gIt_Config_Count": "1",
                    "GIT_CONFIG_KEY_0": "core.repositoryformatversion",
                    "GIT_CONFIG_VALUE_0": "999",
                },
            ),
            mock.patch.object(gate.subprocess, "run", side_effect=completed),
        ):
            metadata = gate.source_metadata(source_root, codixing)

        self.assertEqual(metadata["root"], str(source_root))
        self.assertEqual(metadata["git_revision"], "baseline-revision")
        self.assertEqual(metadata["git_tree"], "baseline-tree")
        self.assertFalse(metadata["git_dirty"])
        self.assertEqual(metadata["codixing_version"], "codixing 0.45.0")
        self.assertEqual(len(metadata["source_tree_sha256"]), 64)

    def test_sanitized_git_environment_removes_case_insensitive_git_keys(self):
        with mock.patch.dict(
            gate.os.environ,
            {"GIT_DIR": "poison", "gIt_Config_Count": "1", "SAFE": "kept"},
            clear=True,
        ):
            environment = gate.sanitized_git_environment(
                {"GIT_WORK_TREE": "also-poison", "EXTRA": "kept"}
            )
        self.assertEqual(environment, {"SAFE": "kept", "EXTRA": "kept"})

    def test_source_provenance_gate_requires_clean_valid_git_identity(self):
        valid = {
            "git_revision": "a" * 40,
            "git_tree": "c" * 40,
            "git_dirty": False,
            "source_tree_sha256": "b" * 64,
        }
        valid_gate = gate.source_provenance_gate(valid)
        self.assertEqual(valid_gate["status"], "passed")
        self.assertTrue(valid_gate["checks"][3]["value"])

        for field, value in (
            ("git_revision", None),
            ("git_revision", "not-a-revision"),
            ("git_tree", None),
            ("git_tree", "not-a-tree"),
            ("source_tree_sha256", None),
            ("source_tree_sha256", "not-a-digest"),
            ("git_dirty", True),
            ("git_dirty", None),
        ):
            invalid = {**valid, field: value}
            with self.subTest(field=field, value=value):
                self.assertEqual(
                    gate.source_provenance_gate(invalid)["status"], "failed"
                )

    def test_source_metadata_digest_includes_dirty_untracked_content(self):
        with tempfile.TemporaryDirectory() as directory:
            source_root = Path(directory)
            untracked = source_root / "new.txt"
            untracked.write_text("first")
            codixing = source_root / "codixing"

            def completed(command, **kwargs):
                self.assertEqual(kwargs["cwd"], source_root)
                output = {
                    (str(codixing), "--version"): "codixing test\n",
                    (
                        gate.os.environ.get("RUSTC", "rustc"),
                        "--version",
                        "--verbose",
                    ): "rustc test\n",
                    ("git", "rev-parse", "HEAD"): "revision\n",
                    ("git", "rev-parse", "HEAD^{tree}"): "tree\n",
                    ("git", "status", "--porcelain"): "?? new.txt\n",
                    ("git", "diff", "--binary", "HEAD", "--"): "",
                    ("git", "ls-files", "--others", "--exclude-standard"): (
                        "new.txt\n"
                    ),
                }[tuple(command)]
                return mock.Mock(returncode=0, stdout=output)

            with mock.patch.object(gate.subprocess, "run", side_effect=completed):
                first = gate.source_metadata(source_root, codixing)
                untracked.write_text("second")
                second = gate.source_metadata(source_root, codixing)

        self.assertTrue(first["git_dirty"])
        self.assertNotEqual(first["source_tree_sha256"], second["source_tree_sha256"])

    def test_source_metadata_keeps_non_git_binaries_usable(self):
        source_root = Path("/tmp/external-source")
        codixing = Path("/tmp/external-bin/codixing")

        def completed(command, **kwargs):
            self.assertEqual(kwargs["cwd"], source_root)
            if command == [str(codixing), "--version"]:
                return mock.Mock(returncode=0, stdout="codixing external\n")
            return mock.Mock(returncode=128, stdout="")

        with mock.patch.object(gate.subprocess, "run", side_effect=completed):
            metadata = gate.source_metadata(source_root, codixing)

        self.assertEqual(metadata["codixing_version"], "codixing external")
        self.assertIsNone(metadata["git_revision"])
        self.assertIsNone(metadata["git_dirty"])
        self.assertEqual(gate.source_provenance_gate(metadata)["status"], "failed")

    def test_capture_build_provenance_records_malformed_or_missing_output(self):
        malformed = mock.Mock(returncode=0, stdout="not-json", stderr="")
        with mock.patch.object(gate.subprocess, "run", return_value=malformed):
            self.assertEqual(
                gate.capture_build_provenance(Path("codixing")),
                {"capture_error": "malformed JSON"},
            )

        with mock.patch.object(gate.subprocess, "run", side_effect=FileNotFoundError):
            self.assertEqual(
                gate.capture_build_provenance(Path("missing")),
                {"capture_error": "FileNotFoundError"},
            )

    def test_binary_source_provenance_requires_matching_clean_attestations(self):
        revision = "a" * 40
        tree = "b" * 40
        source = {"git_revision": revision, "git_tree": tree, "git_dirty": False}
        attestation = {
            "schema_version": 1,
            "origin": gate.EMBEDDED_BUILD_PROVENANCE_ORIGIN,
            "revision": revision,
            "tree": tree,
            "dirty": False,
        }
        self.assertEqual(
            gate.binary_source_provenance_gate(source, attestation, attestation)[
                "status"
            ],
            "passed",
        )

        mutations = (
            ("revision mismatch", {**attestation, "revision": "c" * 40}, attestation),
            ("tree mismatch", attestation, {**attestation, "tree": "d" * 40}),
            ("dirty binary", {**attestation, "dirty": True}, attestation),
            ("boolean schema", {**attestation, "schema_version": True}, attestation),
            ("unknown origin", {**attestation, "origin": "unknown"}, attestation),
            ("missing", None, attestation),
            ("malformed", "not-an-object", attestation),
        )
        for label, codixing, server in mutations:
            with self.subTest(label=label):
                self.assertEqual(
                    gate.binary_source_provenance_gate(source, codixing, server)[
                        "status"
                    ],
                    "failed",
                )

    @staticmethod
    def _legacy_provenance_fixture(root: Path) -> tuple[dict, dict, dict, dict]:
        source_root = root / "source"
        target_dir = root / "target"
        rustc_version = "rustc 1.88.0\nbinary: rustc"
        source = {
            "root": str(source_root),
            "git_revision": "a" * 40,
            "git_tree": "b" * 40,
            "git_dirty": False,
            "rustc_version": rustc_version,
        }
        configuration = {
            "build_profile": "release",
            "build_features": "default",
            "rustflags": "",
            "build_environment": {},
        }
        binaries = {
            "codixing": {
                "path": str(target_dir / "release" / "codixing"),
                "sha256": "c" * 64,
                "build_provenance": {"capture_error": "exit status 2"},
            },
            "server": {
                "path": str(target_dir / "release" / "codixing-server"),
                "sha256": "d" * 64,
                "build_provenance": {"capture_error": "exit status 2"},
            },
        }
        identity = {
            "root": str(source_root),
            "revision": source["git_revision"],
            "tree": source["git_tree"],
            "dirty": False,
        }
        receipt = {
            "schema_version": gate.LEGACY_BUILD_PROVENANCE_SCHEMA_VERSION,
            "origin": gate.LEGACY_BUILD_PROVENANCE_ORIGIN,
            "source": {"before": identity, "after": identity},
            "build": {
                "profile": "release",
                "features": "default",
                "locked": True,
                "packages": ["codixing", "codixing-server"],
                "target_dir": str(target_dir),
                "rustc": {"before": rustc_version, "after": rustc_version},
                "environment": {},
            },
            "binaries": {
                name: {"path": binary["path"], "sha256": binary["sha256"]}
                for name, binary in binaries.items()
            },
        }
        return source, configuration, binaries, receipt

    def test_legacy_provenance_receipt_is_exact_and_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            source, configuration, binaries, receipt = self._legacy_provenance_fixture(
                Path(directory)
            )
            valid = gate.legacy_build_provenance_gate(
                receipt, source, binaries, configuration
            )
            self.assertEqual(valid["status"], "passed")

            mutations = {
                "stale revision": ("source", "before", "revision", "e" * 40),
                "wrong hash": ("binaries", "codixing", "sha256", "f" * 64),
                "wrong path": (
                    "binaries",
                    "server",
                    "path",
                    str(Path(directory) / "other" / "codixing-server"),
                ),
            }
            for label, (section, item, field, value) in mutations.items():
                altered = copy.deepcopy(receipt)
                altered[section][item][field] = value
                with self.subTest(label=label):
                    self.assertEqual(
                        gate.legacy_build_provenance_gate(
                            altered, source, binaries, configuration
                        )["status"],
                        "failed",
                    )

    def test_legacy_provenance_cannot_override_native_or_mix_origins(self):
        with tempfile.TemporaryDirectory() as directory:
            source, configuration, binaries, receipt = self._legacy_provenance_fixture(
                Path(directory)
            )
            receipt_path = Path(directory) / "receipt.json"
            receipt_path.write_text(json.dumps(receipt))
            embedded = {
                "schema_version": 1,
                "origin": gate.EMBEDDED_BUILD_PROVENANCE_ORIGIN,
                "revision": source["git_revision"],
                "tree": source["git_tree"],
                "dirty": False,
            }
            binaries["codixing"]["build_provenance"] = embedded
            self.assertIsNone(
                gate.apply_legacy_build_provenance(
                    source, binaries, configuration, receipt_path
                )
            )

            legacy = {
                "schema_version": 1,
                "origin": gate.LEGACY_BUILD_PROVENANCE_ORIGIN,
                "revision": source["git_revision"],
                "tree": source["git_tree"],
                "dirty": False,
                "binary_sha256": binaries["server"]["sha256"],
            }
            mixed = gate.binary_source_provenance_gate(
                source,
                embedded,
                legacy,
                binaries=binaries,
                legacy_provenance={"receipt": receipt},
                configuration=configuration,
            )
            self.assertEqual(mixed["status"], "failed")
            self.assertIn(
                "matching_binary_origins",
                {
                    check["name"]
                    for check in mixed["checks"]
                    if check["status"] == "failed"
                },
            )

    def test_apply_legacy_provenance_binds_both_exact_binary_hashes(self):
        with tempfile.TemporaryDirectory() as directory:
            source, configuration, binaries, receipt = self._legacy_provenance_fixture(
                Path(directory)
            )
            receipt_path = Path(directory) / "receipt.json"
            receipt_path.write_text(json.dumps(receipt))
            applied = gate.apply_legacy_build_provenance(
                source, binaries, configuration, receipt_path
            )
            self.assertEqual(applied["gate"]["status"], "passed")
            result = {
                "source": source,
                "configuration": configuration,
                "binaries": binaries,
                "legacy_build_provenance": applied,
            }
            self.assertEqual(
                gate.result_binary_source_provenance_gate(result)["status"], "passed"
            )

            result["binaries"]["server"]["sha256"] = "f" * 64
            self.assertEqual(
                gate.result_binary_source_provenance_gate(result)["status"], "failed"
            )

    def test_legacy_bootstrap_rejects_symlinked_or_nested_endpoints(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            source.mkdir()
            dangling = root / "missing"
            target_link = root / "target-link"
            target_link.symlink_to(dangling, target_is_directory=True)
            with self.assertRaises(FileExistsError):
                gate.bootstrap_legacy_build_provenance(
                    source, target_link, root / "receipt.json"
                )

            receipt_link = root / "receipt-link.json"
            receipt_link.symlink_to(dangling)
            with self.assertRaises(FileExistsError):
                gate.bootstrap_legacy_build_provenance(
                    source, root / "target", receipt_link
                )

            with self.assertRaises(ValueError):
                gate.bootstrap_legacy_build_provenance(
                    source, source / "target", root / "receipt.json"
                )

    def test_legacy_bootstrap_sanitizes_build_and_records_toolchain(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            source.mkdir()
            (source / "Cargo.toml").write_text("[workspace]\n")
            target = root / "target"
            receipt_path = root / "receipt.json"
            revision = "a" * 40
            tree = "b" * 40
            rustc = "rustc 1.88.0\nbinary: rustc"

            def completed(command, **kwargs):
                environment = kwargs.get("env", {})
                self.assertFalse(
                    any(name.upper().startswith("GIT_") for name in environment)
                )
                if command[0] == "git":
                    output = {
                        ("git", "rev-parse", "--show-toplevel"): str(source),
                        ("git", "rev-parse", "--verify", "HEAD"): revision,
                        ("git", "rev-parse", "--verify", "HEAD^{tree}"): tree,
                        (
                            "git",
                            "status",
                            "--porcelain=v1",
                            "--untracked-files=normal",
                        ): "",
                    }[tuple(command)]
                    return mock.Mock(returncode=0, stdout=output, stderr="")
                if command[1:] == ["--version", "--verbose"]:
                    return mock.Mock(returncode=0, stdout=rustc, stderr="")
                self.assertEqual(command[0], "cargo")
                self.assertEqual(environment["CARGO_TARGET_DIR"], str(target.resolve()))
                release = target / "release"
                release.mkdir(parents=True)
                for name in ("codixing", "codixing-server"):
                    binary = release / name
                    binary.write_bytes(name.encode())
                    binary.chmod(0o755)
                return mock.Mock(returncode=0)

            with (
                mock.patch.dict(
                    gate.os.environ,
                    {"GIT_DIR": "poison", "gIt_Config_Count": "1"},
                ),
                mock.patch.object(gate.subprocess, "run", side_effect=completed),
            ):
                receipt = gate.bootstrap_legacy_build_provenance(
                    source, target, receipt_path
                )

            self.assertEqual(receipt["build"]["rustc"]["before"], rustc)
            self.assertEqual(receipt["build"]["rustc"]["after"], rustc)
            self.assertTrue(receipt_path.is_file())
            self.assertEqual(
                receipt_path.read_text(),
                json.dumps(receipt, indent=2, sort_keys=True) + "\n",
            )

    def test_file_sha256_streams_attributable_binary_digest(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "codixing"
            binary.write_bytes(b"abc")
            self.assertEqual(
                gate.file_sha256(binary),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            )

    def test_reciprocal_rank_accepts_cli_and_server_path_fields(self):
        results = [
            {"file": "wrong.rs"},
            {"file_path": "/tmp/root/src/target.rs"},
        ]
        self.assertEqual(gate.reciprocal_rank(results, "src/target.rs"), 0.5)
        self.assertEqual(gate.reciprocal_rank(results, "src/missing.rs"), 0.0)
        self.assertEqual(
            gate.reciprocal_rank([{"file": "src/not-target.rs"}], "target.rs"),
            0.0,
        )

    def test_linux_process_tree_stats_include_descendant_memory_and_io(self):
        with tempfile.TemporaryDirectory() as directory:
            proc_root = Path(directory)

            def write_process(
                pid: int,
                parent: int,
                rss_kib: int,
                pss_kib: int,
                write_bytes: int,
                group: int | None = None,
            ) -> None:
                process = proc_root / str(pid)
                process.mkdir()
                (process / "status").write_text(
                    f"PPid:\t{parent}\nNSpgid:\t{group or pid}\n"
                    f"VmRSS:\t{rss_kib} kB\nVmHWM:\t{rss_kib * 2} kB\n"
                )
                (process / "smaps_rollup").write_text(f"Pss:\t{pss_kib} kB\n")
                (process / "io").write_text(
                    f"read_bytes: {write_bytes // 2}\nwrite_bytes: {write_bytes}\n"
                )

            write_process(100, 1, 10, 8, 1_000)
            write_process(101, 100, 20, 12, 2_000)
            write_process(102, 1, 5, 4, 500, group=100)
            write_process(999, 1, 50, 40, 9_000)
            rss, pss, read_bytes, write_bytes = gate._linux_process_tree_stats(
                100, proc_root
            )
            self.assertEqual(gate._linux_process_io(100, proc_root), (500, 1_000))

        self.assertEqual(rss, 70 * 1024)
        self.assertEqual(pss, 24 * 1024)
        self.assertEqual(read_bytes, 1_750)
        self.assertEqual(write_bytes, 3_500)

    @unittest.skipUnless(sys.platform.startswith("linux"), "requires Linux /proc")
    def test_run_measured_captures_late_peak_and_deleted_write_before_reap(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            script = "\n".join(
                [
                    "import os, time",
                    # Hold the large allocation and a durable write long enough
                    # for the gate's poll loop to sample live /proc/<pid>/io
                    # before exit. Final zombie reads can race on some CI kernels.
                    "allocation = bytearray(32 * 1024 * 1024)",
                    "allocation[::4096] = b'x' * (len(allocation) // 4096)",
                    "with open('transient.bin', 'wb', buffering=0) as output:",
                    "    output.write(b'x' * (1024 * 1024))",
                    "    os.fsync(output.fileno())",
                    "os.unlink('transient.bin')",
                    "time.sleep(0.25)",
                ]
            )
            with mock.patch.object(
                gate,
                "_linux_process_tree_snapshot",
                side_effect=AssertionError("timed sampler must not scan process trees"),
            ):
                measured = gate.run_measured(
                    [sys.executable, "-c", script],
                    cwd=root,
                    timeout_s=5,
                    monitor_interval_ms=25,
                )

        self.assertTrue(measured.memory_complete)
        self.assertTrue(measured.io_complete)
        self.assertGreaterEqual(measured.peak_rss_bytes or 0, 32 * 1024 * 1024)
        self.assertGreater(measured.io_write_bytes or 0, 0)
        self.assertEqual(measured.peak_rss_source, "linux_wait4_direct_child_ru_maxrss")
        self.assertIn(
            measured.io_source,
            {
                "linux_proc_direct_child_io_final",
                "linux_proc_direct_child_io_final_incomplete",
            },
        )
        self.assertIsNone(measured.peak_pss_bytes)

    def test_effective_rewrite_uses_process_io_when_available(self):
        self.assertEqual(gate.effective_rewrite_bytes(4_096, None)[0], 4_096)
        self.assertEqual(gate.effective_rewrite_bytes(4_096, 1_000)[0], 4_096)
        self.assertEqual(gate.effective_rewrite_bytes(4_096, 50_000)[0], 50_000)

    def test_fixture_manifest_cleanup_and_symlink_policy_are_fail_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            parent = Path(directory)
            root = parent / "fixture"
            root.mkdir()
            nonce = "nonce"
            (root / gate.FIXTURE_MARKER).write_text(gate.fixture_marker_contents(nonce))
            identity = (root.stat().st_dev, root.stat().st_ino)
            files = gate.generate_fixture(root, 2, nonce)
            first = gate.fixture_manifest_hash(root, files)
            files[0].write_text(files[0].read_text() + "// changed\n")
            self.assertNotEqual(first, gate.fixture_manifest_hash(root, files))

            with self.assertRaisesRegex(RuntimeError, "marker is invalid"):
                gate.cleanup_owned_fixture(root, "wrong", identity)
            self.assertTrue(root.exists())
            gate.cleanup_owned_fixture(root, nonce, identity)
            self.assertFalse(root.exists())

            index = parent / "index"
            index.mkdir()
            external = parent / "outside.bin"
            external.write_bytes(b"outside")
            try:
                (index / "linked.bin").symlink_to(external)
            except OSError:
                self.skipTest("symlinks unavailable")
            with self.assertRaisesRegex(ValueError, "must not be symlinks"):
                gate.disk_snapshot(index)

    def test_strict_external_quality_requires_attribution_and_commit_binding(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "quality.json"
            revision = "a" * 40
            base = {"mrr": 0.9, "recall_at_10": 0.95, "source": "tasks-v1"}
            path.write_text(json.dumps(base))
            with self.assertRaisesRegex(ValueError, "positive task_count"):
                gate.load_external_quality(
                    path, require_attribution=True, expected_revision=revision
                )

            attributed = {
                **base,
                "task_count": 40,
                "dataset_sha256": "b" * 64,
                "source_revision": revision,
            }
            path.write_text(json.dumps(attributed))
            loaded = gate.load_external_quality(
                path, require_attribution=True, expected_revision=revision
            )
            self.assertEqual(loaded["task_count"], 40)

            for invalid_source in (None, "", "   ", 42):
                missing_source = {**attributed}
                if invalid_source is None:
                    missing_source.pop("source")
                else:
                    missing_source["source"] = invalid_source
                path.write_text(json.dumps(missing_source))
                with (
                    self.subTest(source=invalid_source),
                    self.assertRaisesRegex(ValueError, "non-empty string source"),
                ):
                    gate.load_external_quality(
                        path,
                        require_attribution=True,
                        expected_revision=revision,
                    )

            attributed["source_revision"] = "c" * 40
            path.write_text(json.dumps(attributed))
            with self.assertRaisesRegex(ValueError, "does not match"):
                gate.load_external_quality(
                    path, require_attribution=True, expected_revision=revision
                )

    def test_cold_queries_validate_every_timed_response(self):
        correct = json.dumps([{"file": "src/target.rs"}])
        wrong = json.dumps([{"file": "src/wrong.rs"}])

        def measured(stdout: str) -> gate.ProcessMetrics:
            return gate.ProcessMetrics(
                command=[],
                wall_time_ms=1.0,
                exit_code=0,
                peak_rss_bytes=100,
                peak_rss_source="linux_wait4_direct_child_ru_maxrss",
                peak_pss_bytes=80,
                peak_pss_source="linux_proc_direct_child_pss_poll",
                io_read_bytes=0,
                io_write_bytes=0,
                io_source="linux_proc_direct_child_io_final",
                memory_complete=True,
                io_complete=True,
                stdout=stdout,
                stderr="",
            )

        profile = gate.Profile(1, 2, 0, 1, 5)
        with mock.patch.object(
            gate, "run_measured", side_effect=[measured(correct), measured(wrong)]
        ):
            _, quality = gate.cold_queries(
                Path("codixing"),
                Path("."),
                [
                    {
                        "query": "target",
                        "expected_file": "src/target.rs",
                        "strategy": "exact",
                    }
                ],
                2,
                profile,
            )
        self.assertFalse(quality[0]["found"])
        self.assertEqual(quality[0]["observation_count"], 2)
        self.assertEqual(quality[0]["minimum_reciprocal_rank"], 0.0)

    def test_warm_queries_validate_every_response_and_drain_server_pipes(self):
        class FakeProcess:
            def __init__(self):
                self.pid = 42
                self.stdout = io.StringIO("out" * 500_000)
                self.stderr = io.StringIO("err" * 500_000)
                self.returncode = None

            def poll(self):
                return self.returncode

            def terminate(self):
                self.returncode = 0

            def kill(self):
                self.returncode = -9

            def wait(self, timeout=None):
                self.returncode = 0 if self.returncode is None else self.returncode
                return self.returncode

        fake = FakeProcess()
        responses = [
            {},
            {"results": [{"file_path": "src/target.rs"}], "elapsed_ms": 1},
            {"results": [{"file_path": "src/wrong.rs"}], "elapsed_ms": 1},
        ]
        with (
            mock.patch.object(gate.subprocess, "Popen", return_value=fake),
            mock.patch.object(gate, "_free_port", return_value=1234),
            mock.patch.object(gate, "_request_json", side_effect=responses),
            mock.patch.object(
                gate,
                "_live_process_memory_stats",
                return_value=(100, 80, "linux_proc_direct_child_poll"),
            ),
            mock.patch.object(
                gate,
                "_terminate_process_group",
                side_effect=lambda process, force: (
                    process.kill() if force else process.terminate()
                ),
            ),
        ):
            result = gate.warm_queries(
                Path("server"),
                Path("."),
                [
                    {
                        "query": "target",
                        "expected_file": "src/target.rs",
                        "strategy": "exact",
                    }
                ],
                runs=1,
                warmups=1,
                timeout_s=5,
            )
        self.assertFalse(result["validation"]["all_found"])
        self.assertEqual(result["validation"]["observation_count"], 2)

    def test_strict_claim_requires_exact_attributed_evidence_and_quality_floors(self):
        baseline_revision = "b" * 40
        current_revision = "c" * 40

        def claim_result(revision: str, mode: str):
            result = self._result(100.0, 1_000.0, 2_000.0, 0.95)
            result["evidence_mode"] = mode
            result["host"]["system"] = "Linux"
            result["source"]["git_revision"] = revision
            for binary in ("codixing", "server"):
                result["binaries"][binary]["build_provenance"]["revision"] = revision
            result["metrics"]["quality"].update(
                {
                    "comparison_source": "external",
                    "mrr": 0.90,
                    "recall_at_10": 0.95,
                    "external": {
                        "source": "representative-tasks-v1",
                        "task_count": 40,
                        "dataset_sha256": "d" * 64,
                        "source_revision": revision,
                    },
                }
            )
            return result

        baseline = claim_result(baseline_revision, "claim-baseline")
        current = claim_result(current_revision, "strict-claim")
        baseline["doctor"]["index"].pop("layout")
        baseline["metrics"]["disk"]["post_sync"]["layout"] = {
            "kind": "legacy_flat",
            "generation_count": 0,
            "active_generation": None,
            "abandoned_generations": [],
            "observed_generations": [],
            "source": "filesystem_normalized",
        }
        current["metrics"]["init"]["wall_time_ms"] = 50.0
        current["metrics"]["init"]["peak_rss_bytes"] = 500.0
        current["metrics"]["disk"]["total_bytes"] = 1_000.0
        current["metrics"]["disk"]["allocated_bytes"] = 1_000.0
        current["metrics"]["disk"]["post_sync"]["total_bytes"] = 1_000.0
        current["metrics"]["disk"]["post_sync"]["allocated_bytes"] = 1_000.0
        current["metrics"]["queries"]["cold"]["p95_ms"] = 5.0
        current["metrics"]["queries"]["cold"]["peak_rss_bytes"] = 500.0
        current["metrics"]["queries"]["warm"]["client_round_trip"]["p95_ms"] = 2.5
        current["metrics"]["queries"]["warm"]["server_process"]["peak_rss_bytes"] = (
            500.0
        )
        for label, wall_time in (
            ("no_op", 5.0),
            ("one_file", 10.0),
            ("one_percent", 20.0),
        ):
            current["metrics"]["sync"][label]["process"]["wall_time_ms"] = wall_time
        for label in ("one_file", "one_percent"):
            current["metrics"]["sync"][label]["process"]["peak_rss_bytes"] = 500.0
            current["metrics"]["sync"][label]["artifact_bytes_rewritten_estimate"] = (
                500.0
            )
            current["metrics"]["sync"][label]["effective_rewrite_bytes"] = 500.0
        evidence = gate.strict_claim_evidence_gate(
            current,
            baseline,
            expected_current_revision=current_revision,
            expected_baseline_revision=baseline_revision,
        )
        self.assertEqual(evidence["status"], "passed")

        incomplete_io = copy.deepcopy(current)
        incomplete_io["metrics"]["sync"]["one_file"]["process_io_complete"] = False
        evidence = gate.strict_claim_evidence_gate(
            incomplete_io,
            baseline,
            expected_current_revision=current_revision,
            expected_baseline_revision=baseline_revision,
        )
        self.assertEqual(evidence["status"], "failed")

        incomplete_memory = copy.deepcopy(current)
        incomplete_memory["metrics"]["init"]["memory_complete"] = False
        evidence = gate.strict_claim_evidence_gate(
            incomplete_memory,
            baseline,
            expected_current_revision=current_revision,
            expected_baseline_revision=baseline_revision,
        )
        self.assertEqual(evidence["status"], "failed")

        under_indexed = copy.deepcopy(current)
        under_indexed["doctor"]["index"]["meta"]["file_count"] = 99_999
        evidence = gate.strict_claim_evidence_gate(
            under_indexed,
            baseline,
            expected_current_revision=current_revision,
            expected_baseline_revision=baseline_revision,
        )
        self.assertEqual(evidence["status"], "failed")
        self.assertEqual(
            next(
                check["status"]
                for check in evidence["checks"]
                if check["name"] == "current_doctor_index"
            ),
            "failed",
        )

        policy = {**gate.STRICT_CLAIM_MAXIMUMS, **gate.STRICT_CLAIM_MINIMUMS}
        comparison = gate.compare_to_baseline(
            current,
            baseline,
            policy,
            strict_claim=True,
            expected_current_revision=current_revision,
            expected_baseline_revision=baseline_revision,
        )
        self.assertEqual(comparison["status"], "passed")

        lenient = {**policy, "max_speed_suite_ratio": 1.0}
        comparison = gate.compare_to_baseline(
            current,
            baseline,
            lenient,
            strict_claim=True,
            expected_current_revision=current_revision,
            expected_baseline_revision=baseline_revision,
        )
        self.assertEqual(comparison["status"], "failed")
        self.assertIn("invalid strict-claim policy", comparison["reason"])

        current["metrics"]["quality"]["recall_at_10"] = 0.89
        comparison = gate.compare_to_baseline(
            current,
            baseline,
            policy,
            strict_claim=True,
            expected_current_revision=current_revision,
            expected_baseline_revision=baseline_revision,
        )
        failed = {
            check["name"]
            for check in comparison["checks"]
            if check["status"] == "failed"
        }
        self.assertEqual(failed, {"min_recall_at_10_ratio", "min_quality_recall_at_10"})

    def test_doctor_gate_rejects_under_indexing_and_embedding(self):
        report = {
            "index": {
                "status": "ok",
                "meta": {"file_count": 100_000, "chunk_count": 400_000},
                "config": {"embedding_enabled": False},
            }
        }
        self.assertEqual(gate.doctor_index_gate(report, 100_000)["status"], "passed")

        for field, value in (
            ("file_count", 99_999),
            ("chunk_count", 0),
            ("chunk_count", 99_999),
            ("chunk_count", 1_600_001),
        ):
            invalid = copy.deepcopy(report)
            invalid["index"]["meta"][field] = value
            with self.subTest(field=field, value=value):
                self.assertEqual(
                    gate.doctor_index_gate(invalid, 100_000)["status"], "failed"
                )

        embedding = copy.deepcopy(report)
        embedding["index"]["config"]["embedding_enabled"] = True
        self.assertEqual(gate.doctor_index_gate(embedding, 100_000)["status"], "failed")

    def test_claim_affecting_compatibility_fields_are_compared(self):
        baseline = self._result(100.0, 1_000.0, 2_000.0, 1.0)
        mutations = (
            (
                "monitor interval",
                lambda result: result["profile"].update(monitor_interval_ms=1),
            ),
            (
                "fixture manifest",
                lambda result: result["fixture"].update(manifest_sha256="d" * 64),
            ),
            (
                "build features",
                lambda result: result["configuration"].update(build_features="none"),
            ),
            (
                "source bytes",
                lambda result: result["metrics"]["source"].update(total_bytes=1),
            ),
        )
        for label, mutate in mutations:
            current = copy.deepcopy(baseline)
            mutate(current)
            with self.subTest(label=label):
                comparison = gate.compare_to_baseline(
                    current, baseline, {"max_init_ratio": 1.0}
                )
                self.assertEqual(comparison["status"], "failed")
                self.assertIn("incompatible baseline fields", comparison["reason"])

    def test_external_quality_hook_validates_normalized_scores(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "quality.json"
            path.write_text(
                json.dumps(
                    {
                        "mrr": 0.91,
                        "recall_at_10": 0.95,
                        "source": "representative-repo-v1",
                        "task_count": 40,
                        "dataset_sha256": "a" * 64,
                    }
                )
            )
            loaded = gate.load_external_quality(path)
            self.assertEqual(loaded["mrr"], 0.91)
            self.assertEqual(loaded["task_count"], 40)
            self.assertEqual(loaded["dataset_sha256"], "a" * 64)

            path.write_text(
                json.dumps(
                    {
                        "mrr": 0.91,
                        "recall_at_10": 0.95,
                        "dataset_sha256": "not-a-digest",
                    }
                )
            )
            with self.assertRaisesRegex(ValueError, "dataset_sha256"):
                gate.load_external_quality(path)

            path.write_text(json.dumps({"mrr": 1.1, "recall_at_10": 1.0}))
            with self.assertRaises(ValueError):
                gate.load_external_quality(path)

            path.write_text(json.dumps({"mrr": True, "recall_at_10": 1.0}))
            with self.assertRaises(ValueError):
                gate.load_external_quality(path)

    def test_speed_suite_geometric_mean_and_component_guard(self):
        baseline = self._result(init=100.0, rss=1_000.0, disk=2_000.0, quality=1.0)
        improved = self._result(init=50.0, rss=500.0, disk=1_000.0, quality=1.0)
        improved["metrics"]["queries"]["cold"]["p95_ms"] = 5.0
        improved["metrics"]["sync"]["no_op"]["process"]["wall_time_ms"] = 5.0
        improved["metrics"]["sync"]["one_file"]["process"]["wall_time_ms"] = 10.0
        improved["metrics"]["sync"]["one_percent"]["process"]["wall_time_ms"] = 20.0
        comparison = gate.compare_to_baseline(
            improved,
            baseline,
            {
                "max_speed_suite_ratio": 0.5,
                "max_speed_component_ratio": 1.05,
            },
        )
        self.assertEqual(comparison["status"], "passed")
        suite = next(
            check
            for check in comparison["checks"]
            if check["name"] == "max_speed_suite_ratio"
        )
        self.assertAlmostEqual(suite["ratio"], 0.5)

        improved["metrics"]["init"]["wall_time_ms"] = 10.0
        improved["metrics"]["queries"]["cold"]["p95_ms"] = 1.0
        improved["metrics"]["sync"]["no_op"]["process"]["wall_time_ms"] = 10.6
        improved["metrics"]["sync"]["one_file"]["process"]["wall_time_ms"] = 2.0
        improved["metrics"]["sync"]["one_percent"]["process"]["wall_time_ms"] = 4.0
        comparison = gate.compare_to_baseline(
            improved,
            baseline,
            {
                "max_speed_suite_ratio": 0.5,
                "max_speed_component_ratio": 1.05,
            },
        )
        self.assertEqual(comparison["status"], "failed")
        failed = {
            check["name"]
            for check in comparison["checks"]
            if check["status"] == "failed"
        }
        self.assertEqual(failed, {"max_speed_component_ratio:no_op_sync"})

    def test_one_file_sync_uses_a_hard_capped_absolute_delta(self):
        baseline = self._result(init=1.0, rss=1.0, disk=1.0, quality=1.0)
        current = self._result(init=1.0, rss=1.0, disk=1.0, quality=1.0)
        baseline["metrics"]["sync"]["one_file"]["process"]["wall_time_ms"] = 2_856.0
        current["metrics"]["sync"]["one_file"]["process"]["wall_time_ms"] = 3_282.0
        policy = {
            "max_speed_component_ratio": 1.05,
            "max_one_file_sync_regression_ms": 500.0,
        }
        comparison = gate.compare_to_baseline(current, baseline, policy)
        self.assertEqual(comparison["status"], "passed")
        self.assertNotIn(
            "max_speed_component_ratio:one_file_sync",
            {check["name"] for check in comparison["checks"]},
        )
        delta_check = next(
            check
            for check in comparison["checks"]
            if check["name"] == "max_one_file_sync_regression_ms"
        )
        self.assertEqual(delta_check["regression_ms"], 426.0)
        self.assertEqual(delta_check["effective_limit_ms"], 3_356.0)

        current["metrics"]["sync"]["one_file"]["process"]["wall_time_ms"] = 3_357.0
        comparison = gate.compare_to_baseline(current, baseline, policy)
        self.assertEqual(comparison["status"], "failed")

        current["metrics"]["sync"]["one_file"]["process"]["wall_time_ms"] = 3_356.0
        comparison = gate.compare_to_baseline(
            current,
            baseline,
            {"max_one_file_sync_regression_ms": 5_000.0},
        )
        self.assertEqual(comparison["status"], "passed")
        self.assertEqual(comparison["checks"][0]["allowed_delta_ms"], 500.0)

    def test_explicit_legacy_one_file_ratio_remains_supported(self):
        baseline = self._result(init=1.0, rss=1.0, disk=1.0, quality=1.0)
        current = self._result(init=1.0, rss=1.0, disk=1.0, quality=1.0)
        current["metrics"]["sync"]["one_file"]["process"]["wall_time_ms"] = 10.0
        comparison = gate.compare_to_baseline(
            current, baseline, {"max_one_file_sync_ratio": 0.5}
        )
        self.assertEqual(comparison["status"], "passed")

        current["metrics"]["sync"]["one_file"]["process"]["wall_time_ms"] = 10.1
        comparison = gate.compare_to_baseline(
            current, baseline, {"max_one_file_sync_ratio": 0.5}
        )
        self.assertEqual(comparison["status"], "failed")

    def test_warm_latency_uses_ratio_floor_and_regression_delta(self):
        baseline = self._result(init=1.0, rss=1.0, disk=1.0, quality=1.0)
        current = self._result(init=1.0, rss=1.0, disk=1.0, quality=1.0)
        baseline["metrics"]["queries"]["warm"]["client_round_trip"]["p95_ms"] = 4.0
        current["metrics"]["queries"]["warm"]["client_round_trip"]["p95_ms"] = 5.5
        policy = {
            "max_warm_query_p95_ratio": 0.5,
            "warm_query_absolute_floor_ms": 10.0,
            "max_warm_query_regression_ms": 2.0,
        }
        comparison = gate.compare_to_baseline(current, baseline, policy)
        self.assertEqual(comparison["status"], "passed")
        self.assertEqual(comparison["checks"][0]["effective_limit_ms"], 6.0)

        current["metrics"]["queries"]["warm"]["client_round_trip"]["p95_ms"] = 6.1
        comparison = gate.compare_to_baseline(current, baseline, policy)
        self.assertEqual(comparison["status"], "failed")

    def test_incremental_contract_gates_no_op_and_one_percent(self):
        baseline = self._result(init=1.0, rss=1.0, disk=1_000.0, quality=1.0)
        current = self._result(init=1.0, rss=1.0, disk=500.0, quality=1.0)
        current["metrics"]["sync"]["one_percent"]["process"]["wall_time_ms"] = 20.0
        current["metrics"]["sync"]["one_percent"][
            "artifact_bytes_rewritten_estimate"
        ] = 250.0
        current["metrics"]["sync"]["one_percent"]["effective_rewrite_bytes"] = 250.0
        policy = {
            "max_no_op_rewrite_bytes": 0.0,
            "max_one_percent_sync_ratio": 0.5,
            "max_one_percent_rewrite_ratio": 0.5,
        }
        comparison = gate.compare_to_baseline(current, baseline, policy)
        self.assertEqual(comparison["status"], "passed")

        current["metrics"]["sync"]["no_op"]["artifact_bytes_rewritten_estimate"] = 1.0
        current["metrics"]["sync"]["no_op"]["effective_rewrite_bytes"] = 1.0
        comparison = gate.compare_to_baseline(current, baseline, policy)
        self.assertEqual(comparison["status"], "failed")
        failed = {
            check["name"]
            for check in comparison["checks"]
            if check["status"] == "failed"
        }
        self.assertEqual(failed, {"max_no_op_rewrite_bytes"})

    def test_parse_args_accepts_composite_contract_flags(self):
        with mock.patch.object(
            sys,
            "argv",
            [
                "large_repo_gate.py",
                "--max-speed-suite-ratio",
                "0.75",
                "--max-speed-component-ratio",
                "1.05",
                "--max-post-sync-disk-ratio",
                "0.5",
                "--max-one-file-sync-regression-ms",
                "500",
                "--max-one-percent-sync-ratio",
                "1.05",
                "--max-one-percent-rewrite-ratio",
                "0.5",
                "--max-no-op-rewrite-bytes",
                "0",
            ],
        ):
            args = gate.parse_args()
        self.assertEqual(args.max_speed_suite_ratio, 0.75)
        self.assertEqual(args.max_speed_component_ratio, 1.05)
        self.assertEqual(args.max_post_sync_disk_ratio, 0.5)
        self.assertEqual(args.max_one_file_sync_regression_ms, 500.0)
        self.assertIsNone(args.max_one_file_sync_ratio)
        self.assertEqual(args.max_one_percent_sync_ratio, 1.05)
        self.assertEqual(args.max_one_percent_rewrite_ratio, 0.5)
        self.assertEqual(args.max_no_op_rewrite_bytes, 0.0)

        with mock.patch.object(sys, "argv", ["large_repo_gate.py"]):
            defaults = gate.parse_args()
        default_policy = {
            name: getattr(defaults, name)
            for name in (*gate.STRICT_CLAIM_MAXIMUMS, *gate.STRICT_CLAIM_MINIMUMS)
        }
        default_gate = gate.strict_claim_policy_gate(default_policy)
        self.assertEqual(default_gate["status"], "failed")
        self.assertIn(
            "max_speed_suite_ratio",
            {
                check["name"]
                for check in default_gate["checks"]
                if check["status"] == "failed"
            },
        )

        with mock.patch.object(
            sys,
            "argv",
            [
                "large_repo_gate.py",
                "--bootstrap-legacy-build",
                "--source-root",
                "/tmp/source",
                "--legacy-build-target",
                "/tmp/target",
                "--legacy-build-provenance",
                "/tmp/receipt.json",
            ],
        ):
            bootstrap = gate.parse_args()
        self.assertTrue(bootstrap.bootstrap_legacy_build)
        self.assertEqual(bootstrap.legacy_build_target, Path("/tmp/target"))
        self.assertEqual(bootstrap.legacy_build_provenance, Path("/tmp/receipt.json"))

    def test_ci_manual_quality_inputs_are_raw_json_not_runner_paths(self):
        workflow = (
            Path(__file__).resolve().parents[1] / ".github" / "workflows" / "ci.yml"
        ).read_text()
        self.assertIn("baseline_quality_json:", workflow)
        self.assertIn("candidate_quality_json:", workflow)
        self.assertNotIn("baseline_quality_result:", workflow)
        self.assertNotIn("candidate_quality_result:", workflow)
        self.assertIn(
            'printf \'%s\' "$baseline_quality_json" > "$baseline_quality_path"',
            workflow,
        )
        self.assertIn("SCHEDULED_BASELINE_QUALITY_JSON", workflow)
        self.assertIn("--bootstrap-legacy-build", workflow)
        self.assertIn("--legacy-build-provenance", workflow)

    def test_strict_candidate_cannot_accept_a_legacy_receipt_override(self):
        with (
            mock.patch.object(
                sys,
                "argv",
                [
                    "large_repo_gate.py",
                    "--profile",
                    "100k",
                    "--evidence-mode",
                    "strict-claim",
                    "--source-root",
                    "/tmp/source",
                    "--legacy-build-provenance",
                    "/tmp/receipt.json",
                ],
            ),
            self.assertRaisesRegex(ValueError, "baseline-only"),
        ):
            gate.main()

    def test_strict_evidence_rejects_legacy_current_cached_result(self):
        baseline = self._result(init=10.0, rss=100.0, disk=200.0, quality=1.0)
        current = self._result(init=5.0, rss=50.0, disk=100.0, quality=1.0)
        baseline["evidence_mode"] = "claim-baseline"
        current["evidence_mode"] = "strict-claim"

        source = current["source"]
        binaries = current["binaries"]
        target_dir = str(Path(binaries["codixing"]["path"]).parent.parent)
        source_identity = {
            "root": source["root"],
            "revision": source["git_revision"],
            "tree": source["git_tree"],
            "dirty": False,
        }
        receipt = {
            "schema_version": gate.LEGACY_BUILD_PROVENANCE_SCHEMA_VERSION,
            "origin": gate.LEGACY_BUILD_PROVENANCE_ORIGIN,
            "source": {"before": source_identity, "after": source_identity},
            "build": {
                "profile": "release",
                "features": "default",
                "locked": True,
                "packages": ["codixing", "codixing-server"],
                "target_dir": target_dir,
                "rustc": {
                    "before": source["rustc_version"],
                    "after": source["rustc_version"],
                },
                "environment": current["configuration"]["build_environment"],
            },
            "binaries": {
                name: {"path": binary["path"], "sha256": binary["sha256"]}
                for name, binary in binaries.items()
            },
        }
        for binary in binaries.values():
            binary["build_provenance"] = {
                "schema_version": 1,
                "origin": gate.LEGACY_BUILD_PROVENANCE_ORIGIN,
                "revision": source["git_revision"],
                "tree": source["git_tree"],
                "dirty": False,
                "binary_sha256": binary["sha256"],
            }
        current["legacy_build_provenance"] = {"receipt": receipt}

        self.assertEqual(
            gate.result_binary_source_provenance_gate(current)["status"], "passed"
        )

        evidence = gate.strict_claim_evidence_gate(
            current,
            baseline,
            expected_current_revision=source["git_revision"],
            expected_baseline_revision=baseline["source"]["git_revision"],
        )
        embedded_origin_check = next(
            check
            for check in evidence["checks"]
            if check["name"] == "current_embedded_binary_origins"
        )
        self.assertEqual(embedded_origin_check["status"], "failed")

        strict_policy = {
            **gate.STRICT_CLAIM_MAXIMUMS,
            **gate.STRICT_CLAIM_MINIMUMS,
        }
        comparison = gate.compare_to_baseline(
            current,
            baseline,
            strict_policy,
            strict_claim=True,
            expected_current_revision=source["git_revision"],
            expected_baseline_revision=baseline["source"]["git_revision"],
        )
        self.assertEqual(comparison["status"], "failed")
        self.assertIn("current_embedded_binary_origins", comparison["reason"])

    def test_baseline_comparison_passes_and_fails_without_inventing_data(self):
        baseline = self._result(init=100.0, rss=1_000.0, disk=2_000.0, quality=1.0)
        improved = self._result(init=50.0, rss=500.0, disk=1_000.0, quality=1.0)
        ratios = {
            "max_init_ratio": 0.5,
            "max_rss_ratio": 0.5,
            "max_resident_rss_ratio": 0.5,
            "max_disk_ratio": 0.5,
            "max_one_file_rewrite_ratio": 0.5,
            "min_quality_ratio": 0.99,
        }
        comparison = gate.compare_to_baseline(improved, baseline, ratios)
        self.assertEqual(comparison["status"], "passed")

        regressed = self._result(init=51.0, rss=500.0, disk=1_000.0, quality=0.5)
        comparison = gate.compare_to_baseline(regressed, baseline, ratios)
        self.assertEqual(comparison["status"], "failed")
        self.assertEqual(
            {
                check["name"]
                for check in comparison["checks"]
                if check["status"] == "failed"
            },
            {"max_init_ratio", "min_quality_ratio"},
        )

    def test_resident_rss_ratio_is_current_over_baseline_with_a_maximum(self):
        baseline = self._result(init=1.0, rss=1_000.0, disk=1.0, quality=1.0)
        current = self._result(init=1.0, rss=500.0, disk=1.0, quality=1.0)
        comparison = gate.compare_to_baseline(
            current, baseline, {"max_resident_rss_ratio": 0.5}
        )
        self.assertEqual(comparison["status"], "passed")
        self.assertEqual(comparison["checks"][0]["ratio"], 0.5)

    def test_post_sync_disk_ratio_uses_final_deduplicated_allocation(self):
        baseline = self._result(init=1.0, rss=1.0, disk=2_000.0, quality=1.0)
        current = self._result(init=1.0, rss=1.0, disk=1_000.0, quality=1.0)
        comparison = gate.compare_to_baseline(
            current, baseline, {"max_post_sync_disk_ratio": 0.5}
        )
        self.assertEqual(comparison["status"], "passed")
        self.assertEqual(
            comparison["checks"][0]["metric"],
            "metrics.disk.post_sync.allocated_bytes",
        )

        current["metrics"]["disk"]["post_sync"]["allocated_bytes"] = 1_001.0
        comparison = gate.compare_to_baseline(
            current, baseline, {"max_post_sync_disk_ratio": 0.5}
        )
        self.assertEqual(comparison["status"], "failed")

        current["metrics"]["queries"]["warm"]["server_process"]["peak_rss_bytes"] = (
            501.0
        )
        comparison = gate.compare_to_baseline(
            current, baseline, {"max_resident_rss_ratio": 0.5}
        )
        self.assertEqual(comparison["status"], "failed")

        current["metrics"]["queries"]["warm"]["server_process"]["peak_rss_bytes"] = None
        comparison = gate.compare_to_baseline(
            current, baseline, {"max_resident_rss_ratio": 0.5}
        )
        self.assertEqual(comparison["status"], "failed")
        self.assertIn("metric missing", comparison["checks"][0]["reason"])

    def test_cold_rss_and_effective_io_rewrite_are_hard_gates(self):
        baseline = self._result(init=1.0, rss=1_000.0, disk=4_000.0, quality=1.0)
        current = self._result(init=1.0, rss=500.0, disk=2_000.0, quality=1.0)
        current["metrics"]["queries"]["cold"]["peak_rss_bytes"] = 501.0
        current["metrics"]["sync"]["one_file"].update(
            {
                "artifact_bytes_rewritten_estimate": 0.0,
                "effective_rewrite_bytes": 2_001.0,
            }
        )
        comparison = gate.compare_to_baseline(
            current,
            baseline,
            {
                "max_cold_query_rss_ratio": 0.5,
                "max_one_file_rewrite_ratio": 0.5,
            },
        )
        failed = {
            check["name"]
            for check in comparison["checks"]
            if check["status"] == "failed"
        }
        self.assertEqual(
            failed,
            {"max_cold_query_rss_ratio", "max_one_file_rewrite_ratio"},
        )

    def test_baseline_rejects_a_different_scale(self):
        baseline = self._result(init=100.0, rss=1_000.0, disk=2_000.0, quality=1.0)
        current = self._result(init=50.0, rss=500.0, disk=1_000.0, quality=1.0)
        current["profile"]["file_count"] = 10_000
        comparison = gate.compare_to_baseline(
            current, baseline, {"max_init_ratio": 0.5}
        )
        self.assertEqual(comparison["status"], "failed")
        self.assertIn("file_count", comparison["reason"])

    def test_incremental_sync_rss_ratios_are_hard_gates(self):
        baseline = self._result(init=1.0, rss=1_000.0, disk=1.0, quality=1.0)
        current = self._result(init=1.0, rss=500.0, disk=1.0, quality=1.0)
        limits = {
            "max_one_file_sync_rss_ratio": 0.5,
            "max_one_percent_sync_rss_ratio": 0.5,
        }

        comparison = gate.compare_to_baseline(current, baseline, limits)
        self.assertEqual(comparison["status"], "passed")

        current["metrics"]["sync"]["one_file"]["process"]["peak_rss_bytes"] = 501
        comparison = gate.compare_to_baseline(current, baseline, limits)
        self.assertEqual(comparison["status"], "failed")
        failed = {
            check["name"]
            for check in comparison["checks"]
            if check["status"] == "failed"
        }
        self.assertEqual(failed, {"max_one_file_sync_rss_ratio"})

    def test_baseline_rejects_a_different_canonical_work_parent(self):
        baseline = self._result(init=100.0, rss=1_000.0, disk=2_000.0, quality=1.0)
        current = self._result(init=50.0, rss=500.0, disk=1_000.0, quality=1.0)
        current["work_dir_parent"] = "/private/tmp/other-benchmark-parent"
        comparison = gate.compare_to_baseline(
            current, baseline, {"max_init_ratio": 0.5}
        )
        self.assertEqual(comparison["status"], "failed")
        self.assertIn("work_dir_parent", comparison["reason"])

    def test_baseline_rejects_unstable_repeated_sync_measurements(self):
        baseline = self._result(init=100.0, rss=1_000.0, disk=2_000.0, quality=1.0)
        current = self._result(init=50.0, rss=500.0, disk=1_000.0, quality=1.0)
        baseline["metrics"]["sync"]["one_file"]["wall_time_summary"]["stable"] = False
        comparison = gate.compare_to_baseline(
            current, baseline, {"max_init_ratio": 0.5}
        )
        self.assertEqual(comparison["status"], "failed")
        self.assertIn("baseline.metrics.sync.one_file", comparison["reason"])

    def test_baseline_comparison_rejects_dirty_or_missing_source_provenance(self):
        baseline = self._result(init=100.0, rss=1_000.0, disk=2_000.0, quality=1.0)
        current = self._result(init=50.0, rss=500.0, disk=1_000.0, quality=1.0)
        current["source"]["git_dirty"] = True
        comparison = gate.compare_to_baseline(
            current, baseline, {"max_init_ratio": 0.5}
        )
        self.assertEqual(comparison["status"], "failed")
        self.assertIn("current.source (git_clean)", comparison["reason"])

        current = self._result(init=50.0, rss=500.0, disk=1_000.0, quality=1.0)
        baseline["source"]["source_tree_sha256"] = None
        comparison = gate.compare_to_baseline(
            current, baseline, {"max_init_ratio": 0.5}
        )
        self.assertEqual(comparison["status"], "failed")
        self.assertIn("baseline.source (source_tree_sha256)", comparison["reason"])

    def test_cached_comparison_rejects_missing_or_mismatched_binary_attestation(self):
        baseline = self._result(init=100.0, rss=1_000.0, disk=2_000.0, quality=1.0)
        current = self._result(init=50.0, rss=500.0, disk=1_000.0, quality=1.0)
        del baseline["binaries"]["server"]["build_provenance"]
        comparison = gate.compare_to_baseline(
            current, baseline, {"max_init_ratio": 0.5}
        )
        self.assertEqual(comparison["status"], "failed")
        self.assertIn("baseline (server_schema", comparison["reason"])

        baseline = self._result(init=100.0, rss=1_000.0, disk=2_000.0, quality=1.0)
        baseline["binaries"]["codixing"]["build_provenance"]["tree"] = "e" * 40
        comparison = gate.compare_to_baseline(
            current, baseline, {"max_init_ratio": 0.5}
        )
        self.assertEqual(comparison["status"], "failed")
        self.assertIn("codixing_matches_source_tree", comparison["reason"])

    def test_cached_comparison_revalidates_owned_legacy_receipt(self):
        baseline = self._result(init=100.0, rss=1_000.0, disk=2_000.0, quality=1.0)
        current = self._result(init=100.0, rss=1_000.0, disk=2_000.0, quality=1.0)
        source = baseline["source"]
        binaries = baseline["binaries"]
        target_dir = str(Path(binaries["codixing"]["path"]).parent.parent)
        identity = {
            "root": source["root"],
            "revision": source["git_revision"],
            "tree": source["git_tree"],
            "dirty": False,
        }
        receipt = {
            "schema_version": gate.LEGACY_BUILD_PROVENANCE_SCHEMA_VERSION,
            "origin": gate.LEGACY_BUILD_PROVENANCE_ORIGIN,
            "source": {"before": identity, "after": identity},
            "build": {
                "profile": "release",
                "features": "default",
                "locked": True,
                "packages": ["codixing", "codixing-server"],
                "target_dir": target_dir,
                "rustc": {
                    "before": source["rustc_version"],
                    "after": source["rustc_version"],
                },
                "environment": baseline["configuration"]["build_environment"],
            },
            "binaries": {
                name: {"path": binary["path"], "sha256": binary["sha256"]}
                for name, binary in binaries.items()
            },
        }
        for name, binary in binaries.items():
            binary["build_provenance"] = {
                "schema_version": 1,
                "origin": gate.LEGACY_BUILD_PROVENANCE_ORIGIN,
                "revision": source["git_revision"],
                "tree": source["git_tree"],
                "dirty": False,
                "binary_sha256": binary["sha256"],
            }
        baseline["legacy_build_provenance"] = {
            "receipt": receipt,
            "gate": gate.legacy_build_provenance_gate(
                receipt, source, binaries, baseline["configuration"]
            ),
        }

        comparison = gate.compare_to_baseline(
            current, baseline, {"max_init_ratio": 1.0}
        )
        self.assertEqual(comparison["status"], "passed")

        baseline["legacy_build_provenance"]["receipt"]["binaries"]["server"][
            "sha256"
        ] = "0" * 64
        comparison = gate.compare_to_baseline(
            current, baseline, {"max_init_ratio": 1.0}
        )
        self.assertEqual(comparison["status"], "failed")
        self.assertIn("legacy_receipt", comparison["reason"])

    def test_baseline_rejects_a_different_quality_source(self):
        baseline = self._result(init=100.0, rss=1_000.0, disk=2_000.0, quality=1.0)
        current = self._result(init=50.0, rss=500.0, disk=1_000.0, quality=1.0)
        baseline["metrics"]["quality"]["comparison_source"] = "external"
        current["metrics"]["quality"]["comparison_source"] = "synthetic_exact"
        comparison = gate.compare_to_baseline(
            current, baseline, {"max_init_ratio": 0.5}
        )
        self.assertEqual(comparison["status"], "failed")
        self.assertIn("comparison_source", comparison["reason"])

    def test_baseline_rejects_a_different_schema_or_synthetic_task_set(self):
        baseline = self._result(init=100.0, rss=1_000.0, disk=2_000.0, quality=1.0)
        current = self._result(init=50.0, rss=500.0, disk=1_000.0, quality=1.0)
        current["schema_version"] = gate.SCHEMA_VERSION + 1
        comparison = gate.compare_to_baseline(
            current, baseline, {"max_init_ratio": 0.5}
        )
        self.assertEqual(comparison["status"], "failed")
        self.assertIn("schema_version", comparison["reason"])

        current["schema_version"] = baseline["schema_version"]
        current["metrics"]["quality"]["synthetic_exact"]["cases"][0]["query"] = (
            "different-query"
        )
        comparison = gate.compare_to_baseline(
            current, baseline, {"max_init_ratio": 0.5}
        )
        self.assertEqual(comparison["status"], "failed")
        self.assertIn("synthetic_exact.cases", comparison["reason"])

    def test_baseline_comparison_requires_the_current_schema_on_both_sides(self):
        baseline = self._result(init=100.0, rss=1_000.0, disk=2_000.0, quality=1.0)
        current = self._result(init=50.0, rss=500.0, disk=1_000.0, quality=1.0)

        cases = (
            (
                "equal old schemas",
                lambda candidate, reference: (
                    candidate.update(schema_version=gate.SCHEMA_VERSION - 1),
                    reference.update(schema_version=gate.SCHEMA_VERSION - 1),
                ),
            ),
            (
                "missing current schema",
                lambda candidate, _reference: candidate.pop("schema_version"),
            ),
            (
                "null baseline schema",
                lambda _candidate, reference: reference.update(schema_version=None),
            ),
            (
                "boolean current schema",
                lambda candidate, _reference: candidate.update(schema_version=True),
            ),
            (
                "floating current schema",
                lambda candidate, _reference: candidate.update(
                    schema_version=float(gate.SCHEMA_VERSION)
                ),
            ),
        )
        for label, mutate in cases:
            candidate = copy.deepcopy(current)
            reference = copy.deepcopy(baseline)
            mutate(candidate, reference)
            with self.subTest(label=label):
                comparison = gate.compare_to_baseline(
                    candidate, reference, {"max_init_ratio": 0.5}
                )
                self.assertEqual(comparison["status"], "failed")
                self.assertIn(
                    "unsupported benchmark result schema", comparison["reason"]
                )

    def test_baseline_rejects_a_different_external_task_set(self):
        baseline = self._result(init=100.0, rss=1_000.0, disk=2_000.0, quality=1.0)
        current = self._result(init=50.0, rss=500.0, disk=1_000.0, quality=1.0)
        for result, dataset_sha256 in (
            (baseline, "a" * 64),
            (current, "b" * 64),
        ):
            result["metrics"]["quality"].update(
                {
                    "comparison_source": "external",
                    "external": {
                        "source": "tasks-v1",
                        "task_count": 40,
                        "dataset_sha256": dataset_sha256,
                    },
                }
            )
        comparison = gate.compare_to_baseline(
            current, baseline, {"max_init_ratio": 0.5}
        )
        self.assertEqual(comparison["status"], "failed")
        self.assertIn("quality.external", comparison["reason"])

    def test_strict_evidence_rejects_shipping_default_null_worker_telemetry(self):
        baseline = self._result(100.0, 1_000.0, 2_000.0, 1.0)
        current = self._result(50.0, 500.0, 1_000.0, 1.0)
        baseline["evidence_mode"] = "claim-baseline"
        current["evidence_mode"] = "strict-claim"
        for result in (baseline, current):
            result["configuration"].update(
                {
                    "threads": None,
                    "worker_mode": "shipping-default",
                    "requested_threads": None,
                    "effective_worker_threads": None,
                }
            )

        evidence = gate.strict_claim_evidence_gate(
            current,
            baseline,
            expected_current_revision="a" * 40,
            expected_baseline_revision="a" * 40,
        )
        worker_check = next(
            check
            for check in evidence["checks"]
            if check["name"] == "fixed_worker_comparison"
        )
        self.assertEqual(worker_check["status"], "failed")

    def test_worker_comparison_rejects_mismatched_fixed_counts(self):
        baseline = self._result(100.0, 1_000.0, 2_000.0, 1.0)
        current = self._result(50.0, 500.0, 1_000.0, 1.0)
        current["configuration"].update(
            {
                "threads": 4,
                "requested_threads": 4,
                "effective_worker_threads": 4,
            }
        )

        evidence = gate.worker_comparison_gate(current, baseline)
        self.assertEqual(evidence["status"], "failed")
        self.assertEqual(
            evidence["checks"][-1]["name"], "equal_effective_worker_threads"
        )
        self.assertEqual(evidence["checks"][-1]["status"], "failed")

    def test_worker_comparison_accepts_equal_fixed_counts(self):
        baseline = self._result(100.0, 1_000.0, 2_000.0, 1.0)
        current = self._result(50.0, 500.0, 1_000.0, 1.0)

        evidence = gate.worker_comparison_gate(current, baseline)
        self.assertEqual(evidence["status"], "passed")

    def test_worker_comparison_rejects_missing_effective_telemetry(self):
        baseline = self._result(100.0, 1_000.0, 2_000.0, 1.0)
        current = self._result(50.0, 500.0, 1_000.0, 1.0)
        current["configuration"].pop("effective_worker_threads")

        evidence = gate.worker_comparison_gate(current, baseline)
        self.assertEqual(evidence["status"], "failed")
        self.assertEqual(evidence["checks"][0]["status"], "failed")

    @staticmethod
    def _result(init: float, rss: float, disk: float, quality: float):
        return {
            "schema_version": gate.SCHEMA_VERSION,
            "measurement_scope": gate.MEASUREMENT_SCOPE,
            "profile_name": "100k",
            "evidence_mode": "regression",
            "source": {
                "root": "/private/tmp/codixing-source",
                "git_revision": "a" * 40,
                "git_tree": "f" * 40,
                "git_dirty": False,
                "source_tree_sha256": "b" * 64,
                "rustc_version": "rustc test",
            },
            "work_dir_parent": "/private/tmp/codixing-benchmark-parent",
            "binaries": {
                "codixing": {
                    "path": "/private/tmp/codixing-target/release/codixing",
                    "sha256": "d" * 64,
                    "build_provenance": {
                        "schema_version": 1,
                        "origin": gate.EMBEDDED_BUILD_PROVENANCE_ORIGIN,
                        "revision": "a" * 40,
                        "tree": "f" * 40,
                        "dirty": False,
                    },
                },
                "server": {
                    "path": "/private/tmp/codixing-target/release/codixing-server",
                    "sha256": "e" * 64,
                    "build_provenance": {
                        "schema_version": 1,
                        "origin": gate.EMBEDDED_BUILD_PROVENANCE_ORIGIN,
                        "revision": "a" * 40,
                        "tree": "f" * 40,
                        "dirty": False,
                    },
                },
            },
            "configuration": {
                "threads": 8,
                "worker_mode": "fixed",
                "requested_threads": 8,
                "effective_worker_threads": 8,
                "embedding_enabled": False,
                "build_profile": "release",
                "build_features": "default",
                "rustflags": "",
                "build_environment": {},
                "sync_repetitions": gate.SYNC_REPETITIONS,
                "sync_mad_relative_limit": gate.SYNC_MAD_RELATIVE_LIMIT,
                "sync_mad_absolute_floor_ms": gate.SYNC_MAD_ABSOLUTE_FLOOR_MS,
                "sync_iqr_relative_limit": gate.SYNC_IQR_RELATIVE_LIMIT,
                "sync_iqr_absolute_floor_ms": gate.SYNC_IQR_ABSOLUTE_FLOOR_MS,
            },
            "fixture": {
                "schema_hash": gate.fixture_schema_hash(),
                "manifest_sha256": "c" * 64,
            },
            "host": {
                "machine": "test-machine",
                "system": "TestOS",
                "logical_cpus": 8,
                "kernel_release": "test-kernel",
                "cpu_model": "test-cpu",
                "filesystem": {
                    "device": 1,
                    "block_size": 4096,
                    "fragment_size": 4096,
                    "name_max": 255,
                },
            },
            "profile": {
                "file_count": 100_000,
                "query_runs": 25,
                "warmup_runs": 5,
                "monitor_interval_ms": 50,
                "timeout_s": 7_200,
            },
            "metrics": {
                "init": {
                    "wall_time_ms": init,
                    "peak_rss_bytes": rss,
                    "peak_rss_source": "linux_wait4_direct_child_ru_maxrss",
                    "memory_complete": True,
                },
                "source": {"file_count": 100_000, "total_bytes": 30_000_000},
                "disk": {
                    "total_bytes": disk,
                    "allocated_bytes": disk,
                    "post_sync": {
                        "total_bytes": disk,
                        "allocated_bytes": disk,
                        "layout": {
                            "kind": "generational",
                            "generation_count": 1,
                            "active_generation": "gen-0000000000000001",
                            "abandoned_generations": [],
                            "observed_generations": ["gen-0000000000000001"],
                            "source": "doctor_and_filesystem",
                        },
                    },
                },
                "queries": {
                    "cold": {
                        "p95_ms": 10.0,
                        "peak_rss_bytes": rss,
                        "peak_rss_source": "linux_wait4_direct_child_ru_maxrss",
                        "memory_complete": True,
                    },
                    "warm": {
                        "client_round_trip": {"p95_ms": 5.0},
                        "server_process": {
                            "peak_rss_bytes": rss,
                            "source": "linux_proc_direct_child_poll",
                        },
                        "validation": {"all_found": True},
                    },
                },
                "sync": {
                    "no_op": {
                        "process": {
                            "wall_time_ms": 10.0,
                            "io_source": "linux_proc_direct_child_io_final",
                        },
                        "artifact_bytes_rewritten_estimate": 0.0,
                        "effective_rewrite_bytes": 0.0,
                        "effective_rewrite_bytes_source": (
                            "max(surviving_changed_inode_allocated_bytes,"
                            "direct_child_io_write_bytes)"
                        ),
                        "process_io_complete": True,
                        "wall_time_summary": {"stable": True},
                    },
                    "one_file": {
                        "process": {
                            "wall_time_ms": 20.0,
                            "peak_rss_bytes": rss,
                            "peak_rss_source": "linux_wait4_direct_child_ru_maxrss",
                            "memory_complete": True,
                            "io_source": "linux_proc_direct_child_io_final",
                        },
                        "artifact_bytes_rewritten_estimate": disk / 2,
                        "effective_rewrite_bytes": disk / 2,
                        "effective_rewrite_bytes_source": (
                            "max(surviving_changed_inode_allocated_bytes,"
                            "direct_child_io_write_bytes)"
                        ),
                        "process_io_complete": True,
                        "wall_time_summary": {"stable": True},
                        "repetitions": gate.SYNC_REPETITIONS,
                        "distinct_edited_files": gate.SYNC_REPETITIONS,
                        "validations": [
                            {
                                "token": f"one_file_token_{index}",
                                "expected_file": f"src/one_file_{index}.rs",
                                "status": "passed",
                            }
                            for index in range(gate.SYNC_REPETITIONS)
                        ],
                    },
                    "one_percent": {
                        "process": {
                            "wall_time_ms": 40.0,
                            "peak_rss_bytes": rss,
                            "peak_rss_source": "linux_wait4_direct_child_ru_maxrss",
                            "memory_complete": True,
                            "io_source": "linux_proc_direct_child_io_final",
                        },
                        "artifact_bytes_rewritten_estimate": disk / 2,
                        "effective_rewrite_bytes": disk / 2,
                        "effective_rewrite_bytes_source": (
                            "max(surviving_changed_inode_allocated_bytes,"
                            "direct_child_io_write_bytes)"
                        ),
                        "process_io_complete": True,
                        "edited_files": 1_000,
                        "validations": [
                            {
                                "position": position,
                                "token": f"one_percent_token_{index}",
                                "expected_file": f"src/one_percent_{index}.rs",
                                "status": "passed",
                            }
                            for index, position in enumerate(
                                ("first", "middle", "last")
                            )
                        ],
                    },
                },
                "quality": {
                    "mrr": quality,
                    "recall_at_10": quality,
                    "comparison_source": "synthetic_exact",
                    "synthetic_exact": {
                        "cases": [
                            {
                                "query": "unique_target_000000",
                                "expected_file": "src/shard_0000/widget_000000.rs",
                                "strategy": "exact",
                                "found": True,
                            }
                        ]
                    },
                },
            },
            "doctor": {
                "index": {
                    "status": "ok",
                    "meta": {"file_count": 100_000, "chunk_count": 400_000},
                    "config": {"embedding_enabled": False},
                    "layout": {
                        "kind": "generational",
                        "generation_count": 1,
                        "active_generation": "gen-0000000000000001",
                        "abandoned_generations": [],
                    },
                }
            },
        }


if __name__ == "__main__":
    unittest.main()

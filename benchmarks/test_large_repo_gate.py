#!/usr/bin/env python3
"""Unit tests for the large-repository benchmark gate's pure logic."""

import json
import sys
import tempfile
import time
import unittest
from pathlib import Path

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
        self.assertEqual(gate.rewritten_bytes_estimate(before, after), 17)

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

    def test_reciprocal_rank_accepts_cli_and_server_path_fields(self):
        results = [
            {"file": "wrong.rs"},
            {"file_path": "/tmp/root/src/target.rs"},
        ]
        self.assertEqual(gate.reciprocal_rank(results, "src/target.rs"), 0.5)
        self.assertEqual(gate.reciprocal_rank(results, "src/missing.rs"), 0.0)

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
                    }
                )
            )
            loaded = gate.load_external_quality(path)
            self.assertEqual(loaded["mrr"], 0.91)
            self.assertEqual(loaded["task_count"], 40)

            path.write_text(json.dumps({"mrr": 1.1, "recall_at_10": 1.0}))
            with self.assertRaises(ValueError):
                gate.load_external_quality(path)

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

    def test_baseline_rejects_a_different_scale(self):
        baseline = self._result(init=100.0, rss=1_000.0, disk=2_000.0, quality=1.0)
        current = self._result(init=50.0, rss=500.0, disk=1_000.0, quality=1.0)
        current["profile"]["file_count"] = 10_000
        comparison = gate.compare_to_baseline(
            current, baseline, {"max_init_ratio": 0.5}
        )
        self.assertEqual(comparison["status"], "failed")
        self.assertIn("file_count", comparison["reason"])

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

    def test_baseline_rejects_a_different_external_task_set(self):
        baseline = self._result(init=100.0, rss=1_000.0, disk=2_000.0, quality=1.0)
        current = self._result(init=50.0, rss=500.0, disk=1_000.0, quality=1.0)
        for result, source in ((baseline, "tasks-v1"), (current, "tasks-v2")):
            result["metrics"]["quality"].update(
                {
                    "comparison_source": "external",
                    "external": {"source": source, "task_count": 40},
                }
            )
        comparison = gate.compare_to_baseline(
            current, baseline, {"max_init_ratio": 0.5}
        )
        self.assertEqual(comparison["status"], "failed")
        self.assertIn("quality.external", comparison["reason"])

    @staticmethod
    def _result(init: float, rss: float, disk: float, quality: float):
        return {
            "schema_version": gate.SCHEMA_VERSION,
            "fixture": {"schema_hash": gate.fixture_schema_hash()},
            "host": {
                "machine": "test-machine",
                "system": "TestOS",
                "logical_cpus": 8,
                "kernel_release": "test-kernel",
                "cpu_model": "test-cpu",
                "filesystem": {"device": 1, "block_size": 4096},
            },
            "profile": {
                "file_count": 100_000,
                "query_runs": 25,
                "warmup_runs": 5,
            },
            "metrics": {
                "init": {"wall_time_ms": init, "peak_rss_bytes": rss},
                "disk": {"total_bytes": disk, "allocated_bytes": disk},
                "queries": {
                    "cold": {"p95_ms": 10.0},
                    "warm": {
                        "client_round_trip": {"p95_ms": 5.0},
                        "server_process": {"peak_rss_bytes": rss},
                    },
                },
                "sync": {
                    "one_file": {
                        "process": {"wall_time_ms": 20.0},
                        "artifact_bytes_rewritten_estimate": disk / 2,
                    }
                },
                "quality": {
                    "mrr": quality,
                    "comparison_source": "synthetic_exact",
                    "synthetic_exact": {
                        "cases": [
                            {
                                "query": "unique_target_000000",
                                "expected_file": "src/shard_0000/widget_000000.rs",
                                "strategy": "exact",
                            }
                        ]
                    },
                },
            },
        }


if __name__ == "__main__":
    unittest.main()

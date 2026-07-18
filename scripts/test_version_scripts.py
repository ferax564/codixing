#!/usr/bin/env python3
"""Deterministic fixture tests for the release version transaction."""

from __future__ import annotations

import contextlib
import io
import json
import os
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path


SCRIPT_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPT_DIR))

import bump_version  # noqa: E402
import check_version_consistency  # noqa: E402


OLD_VERSION = "0.45.0"
NEW_VERSION = "0.46.0"


def write_release_fixture(
    root: Path, local_versions: dict[str, str] | None = None
) -> None:
    versions = {name: OLD_VERSION for name in bump_version.WORKSPACE_PACKAGE_NAMES}
    versions.update(local_versions or {})

    (root / "npm").mkdir(parents=True)
    (root / "editors" / "vscode").mkdir(parents=True)
    (root / "claude-plugin" / ".claude-plugin").mkdir(parents=True)
    (root / ".claude-plugin").mkdir(parents=True)
    (root / "Cargo.toml").write_text(
        """[workspace]
members = []

[workspace.package]
version = "0.45.0"
edition = "2024"

[workspace.dependencies]
coincidental = "0.45.0"
"""
    )

    lock_blocks = ["# generated fixture\nversion = 4\n"]
    for name in bump_version.WORKSPACE_PACKAGE_NAMES:
        lock_blocks.append(
            f'\n[[package]]\nname = "{name}"\nversion = "{versions[name]}"\n'
        )
    # These entries prove the rewrite does not replace version strings globally
    # or touch a registry package that happens to share a workspace package name.
    lock_blocks.extend(
        [
            """
[[package]]
name = "unrelated"
version = "0.45.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "fixture"
""",
            """
[[package]]
name = "codixing-core"
version = "9.9.9"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "fixture"
""",
        ]
    )
    (root / "Cargo.lock").write_text("".join(lock_blocks))

    (root / "npm" / "package.json").write_text(
        json.dumps({"name": "codixing-mcp", "version": OLD_VERSION}) + "\n"
    )
    (root / "editors" / "vscode" / "package.json").write_text(
        json.dumps({"name": "codixing", "version": OLD_VERSION}) + "\n"
    )
    (root / "editors" / "vscode" / "package-lock.json").write_text(
        json.dumps(
            {
                "name": "codixing",
                "version": OLD_VERSION,
                "lockfileVersion": 3,
                "packages": {
                    "": {"name": "codixing", "version": OLD_VERSION},
                    "node_modules/example": {"version": OLD_VERSION},
                },
            }
        )
        + "\n"
    )
    (root / "claude-plugin" / ".claude-plugin" / "plugin.json").write_text(
        json.dumps({"name": "codixing", "version": OLD_VERSION}) + "\n"
    )
    (root / ".claude-plugin" / "marketplace.json").write_text(
        json.dumps(
            {
                "metadata": {"version": OLD_VERSION},
                "plugins": [
                    {
                        "name": "codixing",
                        "version": OLD_VERSION,
                        "source": {
                            "source": "git-subdir",
                            "ref": f"v{OLD_VERSION}",
                        },
                    }
                ],
            }
        )
        + "\n"
    )


class VersionTransactionTests(unittest.TestCase):
    def test_stage_updates_only_release_fields_and_publishes_consistently(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_release_fixture(root)

            staged = bump_version.stage_version_bump(root, NEW_VERSION)
            self.assertEqual(
                {path.relative_to(root).as_posix() for path in staged},
                {
                    "Cargo.toml",
                    "Cargo.lock",
                    "npm/package.json",
                    "editors/vscode/package.json",
                    "editors/vscode/package-lock.json",
                    "claude-plugin/.claude-plugin/plugin.json",
                    ".claude-plugin/marketplace.json",
                },
            )
            self.assertIn('coincidental = "0.45.0"', staged[root / "Cargo.toml"])

            lock_data = tomllib.loads(staged[root / "Cargo.lock"])
            local = {
                package["name"]: package["version"]
                for package in lock_data["package"]
                if "source" not in package
            }
            self.assertEqual(
                local,
                {name: NEW_VERSION for name in bump_version.WORKSPACE_PACKAGE_NAMES},
            )
            registry = [
                package
                for package in lock_data["package"]
                if package.get("source") is not None
            ]
            self.assertEqual(
                {(package["name"], package["version"]) for package in registry},
                {("unrelated", OLD_VERSION), ("codixing-core", "9.9.9")},
            )
            vscode_lock = json.loads(
                staged[root / "editors" / "vscode" / "package-lock.json"]
            )
            vscode_manifest = json.loads(
                staged[root / "editors" / "vscode" / "package.json"]
            )
            self.assertEqual(vscode_manifest["version"], NEW_VERSION)
            self.assertEqual(vscode_lock["version"], NEW_VERSION)
            self.assertEqual(vscode_lock["packages"][""]["version"], NEW_VERSION)
            self.assertEqual(
                vscode_lock["packages"]["node_modules/example"]["version"],
                OLD_VERSION,
            )
            marketplace = json.loads(
                staged[root / ".claude-plugin" / "marketplace.json"]
            )
            self.assertEqual(marketplace["plugins"][0]["version"], NEW_VERSION)
            self.assertEqual(
                marketplace["plugins"][0]["source"]["ref"], f"v{NEW_VERSION}"
            )

            bump_version.publish_with_rollback(staged)
            found = check_version_consistency.versions(root)
            self.assertEqual(
                check_version_consistency.find_mismatches(found, NEW_VERSION), {}
            )

    def test_stage_fails_closed_when_vscode_root_version_is_missing(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_release_fixture(root)
            lock_path = root / "editors" / "vscode" / "package-lock.json"
            lock_data = json.loads(lock_path.read_text())
            del lock_data["packages"][""]["version"]
            lock_path.write_text(json.dumps(lock_data) + "\n")
            originals = {
                path: path.read_text()
                for path in (
                    root / "Cargo.toml",
                    root / "Cargo.lock",
                    root / "npm" / "package.json",
                    root / "editors" / "vscode" / "package.json",
                    lock_path,
                    root / "claude-plugin" / ".claude-plugin" / "plugin.json",
                    root / ".claude-plugin" / "marketplace.json",
                )
            }

            with self.assertRaisesRegex(ValueError, r"packages\[''\].*version"):
                bump_version.stage_version_bump(root, NEW_VERSION)

            self.assertEqual(
                {path: path.read_text() for path in originals}, originals
            )

    def test_consistency_check_reports_one_drifted_lock_package(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_release_fixture(root, {"codixing-mcp": "0.44.0"})
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                result = check_version_consistency.main([], root)
            self.assertEqual(result, 1)
            self.assertIn("Cargo.lock [[package]] codixing-mcp", stderr.getvalue())

    def test_consistency_check_reports_each_vscode_version_field(self) -> None:
        cases = (
            (
                "package.json",
                ("version",),
                "editors/vscode/package.json",
            ),
            (
                "package-lock.json",
                ("version",),
                "editors/vscode/package-lock.json version",
            ),
            (
                "package-lock.json",
                ("packages", "", "version"),
                "editors/vscode/package-lock.json packages[''].version",
            ),
        )
        for filename, key_path, expected_label in cases:
            with self.subTest(field=expected_label), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                write_release_fixture(root)
                path = root / "editors" / "vscode" / filename
                data = json.loads(path.read_text())
                container = data
                for key in key_path[:-1]:
                    container = container[key]
                container[key_path[-1]] = "0.44.0"
                path.write_text(json.dumps(data) + "\n")

                stderr = io.StringIO()
                with contextlib.redirect_stderr(stderr):
                    result = check_version_consistency.main([], root)
                self.assertEqual(result, 1)
                self.assertIn(expected_label, stderr.getvalue())

    def test_consistency_check_reports_mutable_marketplace_ref(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_release_fixture(root)
            path = root / ".claude-plugin" / "marketplace.json"
            data = json.loads(path.read_text())
            data["plugins"][0]["source"]["ref"] = "main"
            path.write_text(json.dumps(data) + "\n")

            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                result = check_version_consistency.main([], root)
            self.assertEqual(result, 1)
            self.assertIn("source.ref", stderr.getvalue())

    def test_publication_failure_restores_every_original(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            first = root / "first.txt"
            second = root / "second.txt"
            first.write_text("first-old\n")
            second.write_text("second-old\n")
            failed = False

            def fail_second_publication(
                source: str | Path, destination: str | Path
            ) -> None:
                nonlocal failed
                source_path = Path(source)
                destination_path = Path(destination)
                if (
                    not failed
                    and destination_path == second
                    and source_path.name.startswith(".second.txt.new.")
                ):
                    failed = True
                    raise OSError("injected publication failure")
                os.replace(source_path, destination_path)

            with self.assertRaisesRegex(OSError, "injected publication failure"):
                bump_version.publish_with_rollback(
                    {first: "first-new\n", second: "second-new\n"},
                    replace=fail_second_publication,
                )

            self.assertEqual(first.read_text(), "first-old\n")
            self.assertEqual(second.read_text(), "second-old\n")
            self.assertEqual(list(root.glob(".*.new.*")), [])
            self.assertEqual(list(root.glob(".*.old.*")), [])


if __name__ == "__main__":
    unittest.main()

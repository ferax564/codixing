#!/usr/bin/env python3
"""Validate every independently versioned Codixing release artifact."""

from __future__ import annotations

import json
import re
import sys
import tomllib
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
WORKSPACE_PACKAGE_NAMES = (
    "codixing",
    "codixing-core",
    "codixing-lsp",
    "codixing-mcp",
    "codixing-server",
)


def load_json(relative: str, root: Path = ROOT) -> dict:
    return json.loads((root / relative).read_text())


def cargo_lock_workspace_versions(root: Path = ROOT) -> dict[str, str]:
    """Return exact source-less workspace package versions from Cargo.lock."""
    lock_data = tomllib.loads((root / "Cargo.lock").read_text())
    packages = lock_data.get("package")
    if not isinstance(packages, list):
        raise ValueError("Cargo.lock has no [[package]] entries")

    found: dict[str, str] = {}
    for name in WORKSPACE_PACKAGE_NAMES:
        matches = [
            package
            for package in packages
            if isinstance(package, dict)
            and package.get("name") == name
            and "source" not in package
        ]
        if len(matches) != 1:
            raise ValueError(
                "Cargo.lock must contain exactly one source-less entry for "
                f"workspace package {name!r}; found {len(matches)}"
            )
        found[f"Cargo.lock [[package]] {name}"] = str(matches[0].get("version", ""))
    return found


def versions(root: Path = ROOT) -> dict[str, str]:
    cargo_text = (root / "Cargo.toml").read_text()
    match = re.search(
        r'\[workspace\.package\](?:(?!\n\[)[\s\S])*?\nversion\s*=\s*"([^"]+)"',
        cargo_text,
    )
    if match is None:
        raise ValueError("Cargo.toml has no workspace.package.version")

    npm = load_json("npm/package.json", root)
    vscode = load_json("editors/vscode/package.json", root)
    vscode_lock = load_json("editors/vscode/package-lock.json", root)
    vscode_packages = vscode_lock.get("packages")
    vscode_lock_root = (
        vscode_packages.get("") if isinstance(vscode_packages, dict) else None
    )
    if not isinstance(vscode_lock_root, dict):
        raise ValueError("editors/vscode/package-lock.json has no packages['']")
    if vscode.get("name") != "codixing" or vscode_lock.get("name") != "codixing":
        raise ValueError("VS Code manifests do not identify the Codixing extension")
    if vscode_lock_root.get("name") != "codixing":
        raise ValueError(
            "editors/vscode/package-lock.json packages[''] is not Codixing"
        )
    plugin = load_json("claude-plugin/.claude-plugin/plugin.json", root)
    marketplace = load_json(".claude-plugin/marketplace.json", root)
    plugins = marketplace.get("plugins")
    if not isinstance(plugins, list):
        raise ValueError("marketplace.json has no plugins list")
    matching_plugins = [
        entry
        for entry in plugins
        if isinstance(entry, dict) and entry.get("name") == "codixing"
    ]
    if len(matching_plugins) != 1:
        raise ValueError(
            "marketplace.json must contain exactly one Codixing plugin entry"
        )
    marketplace_plugin = matching_plugins[0]
    source = marketplace_plugin.get("source")
    if not isinstance(source, dict) or source.get("source") != "git-subdir":
        raise ValueError("marketplace.json Codixing plugin has no git-subdir source")
    source_ref = source.get("ref")
    if not isinstance(source_ref, str) or re.fullmatch(
        r"v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)",
        source_ref,
    ) is None:
        raise ValueError(
            "marketplace.json Codixing source.ref must be canonical vX.Y.Z"
        )

    found = {
        "Cargo.toml": match.group(1),
        "npm/package.json": str(npm.get("version", "")),
        "editors/vscode/package.json": str(vscode.get("version", "")),
        "editors/vscode/package-lock.json version": str(
            vscode_lock.get("version", "")
        ),
        "editors/vscode/package-lock.json packages[''].version": str(
            vscode_lock_root.get("version", "")
        ),
        "claude-plugin/.claude-plugin/plugin.json": str(plugin.get("version", "")),
        ".claude-plugin/marketplace.json metadata.version": str(
            marketplace.get("metadata", {}).get("version", "")
        ),
        ".claude-plugin/marketplace.json Codixing plugin version": str(
            marketplace_plugin.get("version", "")
        ),
        ".claude-plugin/marketplace.json Codixing source.ref": str(
            source_ref
        ).removeprefix("v"),
    }
    found.update(cargo_lock_workspace_versions(root))
    return found


def find_mismatches(found: dict[str, str], expected: str) -> dict[str, str]:
    return {name: value for name, value in found.items() if value != expected}


def main(argv: list[str] | None = None, root: Path = ROOT) -> int:
    args = sys.argv[1:] if argv is None else argv
    if len(args) > 1:
        print("Usage: check_version_consistency.py [EXPECTED_VERSION]", file=sys.stderr)
        return 2
    try:
        found = versions(root)
    except (OSError, ValueError, json.JSONDecodeError, tomllib.TOMLDecodeError) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 1
    expected = args[0] if args else found["Cargo.toml"]
    mismatches = find_mismatches(found, expected)
    if mismatches:
        for name, value in mismatches.items():
            print(f"ERROR: {name} has {value or '<missing>'}, expected {expected}", file=sys.stderr)
        return 1
    print(f"All versioned release artifacts are consistent: {expected}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Bump version in all 5 locations atomically.

Usage: python3 scripts/bump_version.py 0.35.0

Locations updated:
  1. Cargo.toml              workspace.package.version
  2. npm/package.json        version
  3. docs/install.sh         VERSION=
  4. claude-plugin/.claude-plugin/plugin.json   version
  5. .claude-plugin/marketplace.json            metadata.version + plugins[0].version

The script prepares all new file contents in memory first and validates every
substitution before writing anything. If any step fails, nothing is written and
the repo stays in its original state.
"""

import json
import re
import sys
from pathlib import Path


def bump(new_version: str) -> None:
    root = Path(__file__).parent.parent
    staged: dict[Path, str] = {}

    # 1. Cargo.toml — regex targets only workspace.package.version.
    # The (?:(?!\n\[)[\s\S])*? pattern allows [ inside string values while
    # stopping at the next section header, avoiding the [^\[]*? trap.
    cargo = root / "Cargo.toml"
    text, n = re.subn(
        r'(\[workspace\.package\](?:(?!\n\[)[\s\S])*?version\s*=\s*)"[^"]*"',
        rf'\g<1>"{new_version}"',
        cargo.read_text(),
        count=1,
    )
    if n != 1:
        print(
            f"Error: could not locate [workspace.package] version in {cargo}",
            file=sys.stderr,
        )
        sys.exit(1)
    staged[cargo] = text

    # 2. npm/package.json — load/dump prevents duplicate keys
    npm_pkg = root / "npm" / "package.json"
    data = json.loads(npm_pkg.read_text())
    data["version"] = new_version
    staged[npm_pkg] = json.dumps(data, indent=2) + "\n"

    # 3. docs/install.sh — preserve optional quoting style (VERSION=X or VERSION="X")
    install = root / "docs" / "install.sh"
    text, n = re.subn(
        r'^VERSION="?[^"\n]*"?$',
        f'VERSION="{new_version}"',
        install.read_text(),
        flags=re.MULTILINE,
    )
    if n < 1:
        print(f"Error: could not locate VERSION= line in {install}", file=sys.stderr)
        sys.exit(1)
    staged[install] = text

    # 4. claude-plugin/.claude-plugin/plugin.json — load/dump
    plugin_json = root / "claude-plugin" / ".claude-plugin" / "plugin.json"
    data = json.loads(plugin_json.read_text())
    data["version"] = new_version
    staged[plugin_json] = json.dumps(data, indent=2) + "\n"

    # 5. .claude-plugin/marketplace.json — load/dump (two fields)
    market = root / ".claude-plugin" / "marketplace.json"
    data = json.loads(market.read_text())
    if "metadata" not in data or not isinstance(data["metadata"], dict):
        print("Error: marketplace.json missing 'metadata' object", file=sys.stderr)
        sys.exit(1)
    data["metadata"]["version"] = new_version
    plugins = data.get("plugins", [])
    if not plugins:
        print("Error: marketplace.json has no entries in 'plugins'", file=sys.stderr)
        sys.exit(1)
    plugins[0]["version"] = new_version
    staged[market] = json.dumps(data, indent=2) + "\n"

    # All validations passed — commit to disk.
    for path, content in staged.items():
        path.write_text(content)
        print(f"  {path.relative_to(root)} → {new_version}")

    print(f"\nAll 5 locations bumped to {new_version}.")
    print(
        f'Verify: grep -rn "{new_version}" Cargo.toml npm/package.json '
        f"docs/install.sh claude-plugin/.claude-plugin/plugin.json .claude-plugin/marketplace.json"
    )


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} NEW_VERSION", file=sys.stderr)
        sys.exit(1)
    if not re.fullmatch(r"\d+\.\d+\.\d+", sys.argv[1]):
        print(
            f"Error: version must be X.Y.Z (semver), got: {sys.argv[1]!r}",
            file=sys.stderr,
        )
        sys.exit(1)
    bump(sys.argv[1])

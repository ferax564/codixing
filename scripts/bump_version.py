#!/usr/bin/env python3
"""Bump version in all 5 locations atomically.

Usage: python3 scripts/bump_version.py 0.35.0

Locations updated:
  1. Cargo.toml              workspace.package.version
  2. npm/package.json        version
  3. docs/install.sh         VERSION=
  4. claude-plugin/.claude-plugin/plugin.json   version
  5. .claude-plugin/marketplace.json            metadata.version + plugins[0].version
"""

import json
import re
import sys
from pathlib import Path


def bump(new_version: str) -> None:
    root = Path(__file__).parent.parent

    # 1. Cargo.toml — use regex to target only workspace.package.version
    cargo = root / "Cargo.toml"
    text = cargo.read_text()
    # Match: [workspace.package] ... version = "X.Y.Z", stopping at the next
    # section header (\n[).  Using (?:(?!\n\[)[\s\S])*? allows [ inside string
    # values (e.g. description = "foo [bar]") while correctly stopping at \n[.
    text = re.sub(
        r'(\[workspace\.package\](?:(?!\n\[)[\s\S])*?version\s*=\s*)"[^"]*"',
        rf'\g<1>"{new_version}"',
        text,
        count=1,
    )
    cargo.write_text(text)
    print(f"  Cargo.toml            → {new_version}")

    # 2. npm/package.json — load/dump prevents duplicate keys
    npm_pkg = root / "npm" / "package.json"
    data = json.loads(npm_pkg.read_text())
    data["version"] = new_version
    npm_pkg.write_text(json.dumps(data, indent=2) + "\n")
    print(f"  npm/package.json      → {new_version}")

    # 3. docs/install.sh — preserve optional quoting style (VERSION=X or VERSION="X")
    install = root / "docs" / "install.sh"
    text = install.read_text()
    text = re.sub(
        r'^VERSION="?[^"\n]*"?$',
        f'VERSION="{new_version}"',
        text,
        flags=re.MULTILINE,
    )
    install.write_text(text)
    print(f"  docs/install.sh       → {new_version}")

    # 4. claude-plugin/.claude-plugin/plugin.json — load/dump
    plugin_json = root / "claude-plugin" / ".claude-plugin" / "plugin.json"
    data = json.loads(plugin_json.read_text())
    data["version"] = new_version
    plugin_json.write_text(json.dumps(data, indent=2) + "\n")
    print(f"  plugin.json           → {new_version}")

    # 5. .claude-plugin/marketplace.json — load/dump (two fields)
    market = root / ".claude-plugin" / "marketplace.json"
    data = json.loads(market.read_text())
    data["metadata"]["version"] = new_version
    plugins = data.get("plugins", [])
    if not plugins:
        print("Error: marketplace.json has no entries in 'plugins'", file=sys.stderr)
        sys.exit(1)
    plugins[0]["version"] = new_version
    market.write_text(json.dumps(data, indent=2) + "\n")
    print(f"  marketplace.json      → {new_version} (metadata + plugins[0])")

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

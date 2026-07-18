#!/usr/bin/env python3
"""Bump every release version field as one rollback-capable transaction.

Usage: python3 scripts/bump_version.py 0.35.0

Locations updated:
  1. Cargo.toml              workspace.package.version
  2. Cargo.lock              five local workspace package versions
  3. npm/package.json        version
  4. editors/vscode/package.json                 version
  5. editors/vscode/package-lock.json            version + packages[""].version
  6. claude-plugin/.claude-plugin/plugin.json   version
  7. .claude-plugin/marketplace.json            metadata.version + plugin version + source.ref

`docs/install.sh` intentionally resolves GitHub's latest release at runtime;
set CODIXING_VERSION to pin an older release. It must not be version-bumped.

The script prepares and fsyncs every replacement before publication. Runtime
publication failures restore every file already replaced. A cross-file update
cannot be power-loss atomic on a normal filesystem, so release CI always runs
`check_version_consistency.py` before creating a tag.
"""

import json
import os
import re
import stat
import sys
import tempfile
import tomllib
from collections.abc import Callable
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
WORKSPACE_PACKAGE_NAMES = (
    "codixing",
    "codixing-core",
    "codixing-lsp",
    "codixing-mcp",
    "codixing-server",
)
PACKAGE_BLOCK_RE = re.compile(
    r"^\[\[package\]\][ \t]*\r?\n.*?(?=^\[\[package\]\][ \t]*$|\Z)",
    re.MULTILINE | re.DOTALL,
)


def publish_with_rollback(
    staged: dict[Path, str],
    replace: Callable[[str | Path, str | Path], None] = os.replace,
) -> None:
    """Publish sibling temp files and restore all originals on any error."""
    replacements: dict[Path, Path] = {}
    backups: dict[Path, Path] = {}
    committed: list[Path] = []
    published = False

    try:
        for path, content in staged.items():
            mode = stat.S_IMODE(path.stat().st_mode)
            with tempfile.NamedTemporaryFile(
                mode="w",
                encoding="utf-8",
                dir=path.parent,
                prefix=f".{path.name}.new.",
                delete=False,
            ) as handle:
                handle.write(content)
                handle.flush()
                os.fsync(handle.fileno())
                replacement = Path(handle.name)
            replacement.chmod(mode)
            replacements[path] = replacement

        for path, replacement in replacements.items():
            with tempfile.NamedTemporaryFile(
                dir=path.parent,
                prefix=f".{path.name}.old.",
                delete=False,
            ) as handle:
                backup = Path(handle.name)
            backup.unlink()
            replace(path, backup)
            backups[path] = backup
            try:
                replace(replacement, path)
            except BaseException:
                replace(backup, path)
                backups.pop(path, None)
                raise
            committed.append(path)
        published = True
    except BaseException:
        rollback_failures: list[str] = []
        for path in reversed(committed):
            backup = backups.get(path)
            if backup is not None and backup.exists():
                try:
                    replace(backup, path)
                except OSError as error:
                    rollback_failures.append(f"{path}: {error}")
        if rollback_failures:
            raise RuntimeError(
                "version publication failed and rollback was incomplete; "
                "backup files were retained: " + "; ".join(rollback_failures)
            )
        raise
    finally:
        for replacement in replacements.values():
            replacement.unlink(missing_ok=True)
        if published:
            for backup in backups.values():
                try:
                    backup.unlink(missing_ok=True)
                except OSError:
                    pass


def update_workspace_lock_versions(lock_text: str, new_version: str) -> str:
    """Update exactly one source-less Cargo.lock entry per workspace package."""
    # Parse the complete lockfile first so malformed input can never be partly
    # interpreted by the formatting-preserving block rewrite below.
    parsed = tomllib.loads(lock_text)
    packages = parsed.get("package")
    if not isinstance(packages, list):
        raise ValueError("Cargo.lock has no [[package]] entries")

    expected = set(WORKSPACE_PACKAGE_NAMES)
    local_counts = {name: 0 for name in WORKSPACE_PACKAGE_NAMES}
    for package in packages:
        if not isinstance(package, dict):
            continue
        name = package.get("name")
        if name in expected and "source" not in package:
            local_counts[name] += 1

    invalid = {name: count for name, count in local_counts.items() if count != 1}
    if invalid:
        details = ", ".join(f"{name}={count}" for name, count in invalid.items())
        raise ValueError(
            "Cargo.lock must contain exactly one source-less entry for each "
            f"workspace package ({details})"
        )

    updated: set[str] = set()

    def rewrite_block(match: re.Match[str]) -> str:
        block = match.group(0)
        block_package = tomllib.loads(block)["package"][0]
        name = block_package.get("name")
        if name not in expected or "source" in block_package:
            return block

        replacement, count = re.subn(
            r'^(version\s*=\s*)"[^"]+"([ \t]*)$',
            rf'\g<1>"{new_version}"\g<2>',
            block,
            count=1,
            flags=re.MULTILINE,
        )
        if count != 1:
            raise ValueError(f"Cargo.lock package {name!r} has no unique version field")
        updated.add(str(name))
        return replacement

    result = PACKAGE_BLOCK_RE.sub(rewrite_block, lock_text)
    missing = expected - updated
    if missing:
        raise ValueError(
            "Cargo.lock rewrite missed workspace packages: " + ", ".join(sorted(missing))
        )
    return result


def stage_version_bump(root: Path, new_version: str) -> dict[Path, str]:
    """Validate and prepare every release version update without publishing it."""
    staged: dict[Path, str] = {}

    # 1. Cargo.toml — regex targets only workspace.package.version.
    # The (?:(?!\n\[)[\s\S])*? pattern allows [ inside string values while
    # stopping at the next section header, avoiding the [^\[]*? trap.
    cargo = root / "Cargo.toml"
    text, count = re.subn(
        r'(\[workspace\.package\](?:(?!\n\[)[\s\S])*?version\s*=\s*)"[^"]*"',
        rf'\g<1>"{new_version}"',
        cargo.read_text(),
        count=1,
    )
    if count != 1:
        raise ValueError(
            f"could not locate [workspace.package] version in {cargo}"
        )
    staged[cargo] = text

    # 2. Cargo.lock — target only the source-less entries with exact workspace
    # package names. Registry packages with coincidental versions stay intact.
    cargo_lock = root / "Cargo.lock"
    staged[cargo_lock] = update_workspace_lock_versions(
        cargo_lock.read_text(), new_version
    )

    # 3. npm/package.json — load/dump prevents duplicate keys.
    npm_pkg = root / "npm" / "package.json"
    data = json.loads(npm_pkg.read_text())
    data["version"] = new_version
    staged[npm_pkg] = json.dumps(data, indent=2) + "\n"

    # 4. VS Code extension manifest — load/dump.
    vscode_pkg = root / "editors" / "vscode" / "package.json"
    vscode_data = json.loads(vscode_pkg.read_text())
    if not isinstance(vscode_data, dict) or vscode_data.get("name") != "codixing":
        raise ValueError("editors/vscode/package.json is not the Codixing manifest")
    if "version" not in vscode_data:
        raise ValueError("editors/vscode/package.json has no version")
    vscode_data["version"] = new_version
    staged[vscode_pkg] = json.dumps(vscode_data, indent=2) + "\n"

    # 5. VS Code lockfile — update only its two root-project version fields.
    vscode_lock = root / "editors" / "vscode" / "package-lock.json"
    vscode_lock_data = json.loads(vscode_lock.read_text())
    if (
        not isinstance(vscode_lock_data, dict)
        or vscode_lock_data.get("name") != "codixing"
        or "version" not in vscode_lock_data
    ):
        raise ValueError("editors/vscode/package-lock.json has no Codixing root version")
    vscode_packages = vscode_lock_data.get("packages")
    vscode_root = (
        vscode_packages.get("") if isinstance(vscode_packages, dict) else None
    )
    if (
        not isinstance(vscode_root, dict)
        or vscode_root.get("name") != "codixing"
        or "version" not in vscode_root
    ):
        raise ValueError(
            "editors/vscode/package-lock.json has no packages[''] Codixing version"
        )
    vscode_lock_data["version"] = new_version
    vscode_root["version"] = new_version
    staged[vscode_lock] = json.dumps(vscode_lock_data, indent=2) + "\n"

    # 6. claude-plugin/.claude-plugin/plugin.json — load/dump.
    plugin_json = root / "claude-plugin" / ".claude-plugin" / "plugin.json"
    data = json.loads(plugin_json.read_text())
    data["version"] = new_version
    staged[plugin_json] = json.dumps(data, indent=2) + "\n"

    # 7. .claude-plugin/marketplace.json — version and immutable source ref.
    market = root / ".claude-plugin" / "marketplace.json"
    data = json.loads(market.read_text())
    if "metadata" not in data or not isinstance(data["metadata"], dict):
        raise ValueError("marketplace.json missing 'metadata' object")
    data["metadata"]["version"] = new_version
    plugins = data.get("plugins", [])
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
    if "ref" not in source:
        raise ValueError("marketplace.json Codixing plugin source has no ref")
    marketplace_plugin["version"] = new_version
    source["ref"] = f"v{new_version}"
    staged[market] = json.dumps(data, indent=2) + "\n"

    return staged


def bump(new_version: str, root: Path = ROOT) -> None:
    staged = stage_version_bump(root, new_version)

    # All validations passed — publish the prepared set with rollback.
    publish_with_rollback(staged)
    for path in staged:
        print(f"  {path.relative_to(root)} → {new_version}")

    print(f"\nAll 14 version fields across 7 files bumped to {new_version}.")
    print(f"Verify: python3 scripts/check_version_consistency.py {new_version}")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} NEW_VERSION", file=sys.stderr)
        sys.exit(1)
    if not re.fullmatch(
        r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)",
        sys.argv[1],
    ):
        print(
            f"Error: version must be X.Y.Z (semver), got: {sys.argv[1]!r}",
            file=sys.stderr,
        )
        sys.exit(1)
    try:
        bump(sys.argv[1])
    except (OSError, ValueError, json.JSONDecodeError, tomllib.TOMLDecodeError) as error:
        print(f"Error: {error}", file=sys.stderr)
        sys.exit(1)

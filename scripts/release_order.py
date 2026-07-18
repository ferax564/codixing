#!/usr/bin/env python3
"""Fail-closed release ordering for the GitHub publication workflow."""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path


TAG_RE = re.compile(
    r"^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$"
)


def version(tag: str) -> tuple[int, int, int]:
    match = TAG_RE.fullmatch(tag)
    if match is None:
        raise ValueError(f"non-canonical published release tag {tag!r}")
    return tuple(map(int, match.groups()))  # type: ignore[return-value]


def published_tags(pages: object) -> list[str]:
    if not isinstance(pages, list) or any(not isinstance(page, list) for page in pages):
        raise ValueError("GitHub releases response is not a paginated list")

    tags: list[str] = []
    for page in pages:
        for release in page:
            if not isinstance(release, dict):
                raise ValueError("GitHub releases response contains a non-object release")
            draft = release.get("draft")
            prerelease = release.get("prerelease")
            if not isinstance(draft, bool) or not isinstance(prerelease, bool):
                raise ValueError("GitHub release has invalid draft/prerelease state")
            if not draft and not prerelease:
                tag = release.get("tag_name")
                if not isinstance(tag, str):
                    raise ValueError("published GitHub release has no string tag_name")
                version(tag)
                tags.append(tag)
    return tags


def evaluate_release_order(
    candidate_tag: str, latest_tag: str, pages: object
) -> tuple[str, str]:
    candidate = version(candidate_tag)
    published = published_tags(pages)
    if not published:
        if latest_tag:
            raise ValueError(
                "GitHub latest release exists but the published release list is empty"
            )
        return "first", ""

    maximum_tag = max(published, key=version)
    maximum = version(maximum_tag)
    if latest_tag != maximum_tag:
        raise ValueError(
            f"GitHub latest marker {latest_tag!r} does not match maximum "
            f"published version {maximum_tag!r}"
        )

    relation = (
        "newer" if candidate > maximum else "same" if candidate == maximum else "older"
    )
    return relation, maximum_tag


def main(argv: list[str] | None = None) -> int:
    args = sys.argv[1:] if argv is None else argv
    if len(args) != 3:
        print(
            "usage: release_order.py CANDIDATE_TAG LATEST_TAG RELEASES_JSON",
            file=sys.stderr,
        )
        return 2
    candidate_tag, latest_tag, releases_path = args
    try:
        pages = json.loads(Path(releases_path).read_text(encoding="utf-8"))
        relation, maximum_tag = evaluate_release_order(
            candidate_tag, latest_tag, pages
        )
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 1
    print(f"{relation}|{maximum_tag}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

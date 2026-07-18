#!/usr/bin/env python3
from __future__ import annotations

import unittest

from release_order import evaluate_release_order


def pages(*tags: str) -> list[list[dict[str, object]]]:
    return [
        [
            {"tag_name": tag, "draft": False, "prerelease": False}
            for tag in tags
        ]
    ]


class ReleaseOrderTests(unittest.TestCase):
    def test_first_release(self) -> None:
        self.assertEqual(evaluate_release_order("v0.1.0", "", [[]]), ("first", ""))

    def test_compares_against_maximum_published_semver(self) -> None:
        releases = pages("v0.9.0", "v0.11.0", "v0.10.5")
        self.assertEqual(
            evaluate_release_order("v0.12.0", "v0.11.0", releases),
            ("newer", "v0.11.0"),
        )
        self.assertEqual(
            evaluate_release_order("v0.11.0", "v0.11.0", releases),
            ("same", "v0.11.0"),
        )
        self.assertEqual(
            evaluate_release_order("v0.10.9", "v0.11.0", releases),
            ("older", "v0.11.0"),
        )

    def test_rejects_latest_marker_rollback(self) -> None:
        with self.assertRaisesRegex(ValueError, "maximum published version"):
            evaluate_release_order(
                "v0.12.0", "v0.10.5", pages("v0.10.5", "v0.11.0")
            )

    def test_rejects_noncanonical_stable_release_tag(self) -> None:
        with self.assertRaisesRegex(ValueError, "non-canonical"):
            evaluate_release_order("v0.12.0", "v0.11.0", pages("release-0.11.0"))

    def test_ignores_drafts_and_prereleases(self) -> None:
        releases = pages("v0.11.0")
        releases[0].extend(
            [
                {"tag_name": "v99.0.0", "draft": True, "prerelease": False},
                {"tag_name": "v98.0.0-rc1", "draft": False, "prerelease": True},
            ]
        )
        self.assertEqual(
            evaluate_release_order("v0.12.0", "v0.11.0", releases),
            ("newer", "v0.11.0"),
        )


if __name__ == "__main__":
    unittest.main()

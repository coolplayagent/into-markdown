#!/usr/bin/env python3
"""Compare the two independently generated Web bundles byte for byte."""

from __future__ import annotations

import pathlib
import stat
import unittest


# Keep this path inside the runfiles tree. Resolving __file__ first follows the
# source-file symlink back into the checkout on Unix and loses generated data.
ROOT = pathlib.Path(__file__).parents[1]


def inventory(root: pathlib.Path) -> dict[str, bytes]:
    # Bazel exposes tree artifacts as directory symlinks on Linux. pathlib does
    # not descend when the rglob root itself is a symlink, so bind
    # the comparison to the resolved tree artifact before walking it.
    root = root.resolve(strict=True)
    if not root.is_dir():
        raise AssertionError(f"generated Web bundle is not a directory: {root}")
    result: dict[str, bytes] = {}
    for path in sorted(root.rglob("*"), key=lambda candidate: candidate.as_posix()):
        # Individual generated files can also be runfiles symlinks. Follow the
        # Bazel-owned link for classification and content while retaining the
        # stable runfiles-relative name in the inventory.
        metadata = path.stat()
        if stat.S_ISDIR(metadata.st_mode):
            continue
        if not stat.S_ISREG(metadata.st_mode):
            raise AssertionError(f"generated Web bundle contains a non-regular file: {path}")
        result[path.relative_to(root).as_posix()] = path.read_bytes()
    return result


class DeterminismTest(unittest.TestCase):
    def test_independent_bundles_are_byte_identical(self) -> None:
        first = inventory(ROOT / "generated_assets")
        second = inventory(ROOT / "generated_assets_repeat")
        self.assertTrue(first, "generated Web bundle is empty")
        self.assertEqual(first, second)


if __name__ == "__main__":
    unittest.main()

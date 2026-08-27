from __future__ import annotations

import pathlib
import tempfile
import unittest

from tools.release_version import VersionError, validate_version, workspace_version


class ReleaseVersionTest(unittest.TestCase):
    def workspace(self, version: str = "1.2.3-rc.1") -> pathlib.Path:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = pathlib.Path(temporary.name)
        (root / "Cargo.toml").write_text(
            f'[workspace]\n[workspace.package]\nversion = "{version}"\n',
            encoding="utf-8",
        )
        return root

    def test_reads_and_accepts_exact_workspace_version(self) -> None:
        root = self.workspace()
        self.assertEqual(workspace_version(root), "1.2.3-rc.1")
        self.assertEqual(validate_version("1.2.3-rc.1", root), "1.2.3-rc.1")

    def test_accepts_semver_build_metadata(self) -> None:
        root = self.workspace("1.2.3-rc.1+build.7")
        self.assertEqual(
            validate_version("1.2.3-rc.1+build.7", root),
            "1.2.3-rc.1+build.7",
        )

    def test_rejects_tag_prefix_and_version_drift(self) -> None:
        root = self.workspace()
        with self.assertRaisesRegex(VersionError, "without a leading v"):
            validate_version("v1.2.3-rc.1", root)
        with self.assertRaisesRegex(VersionError, "disagrees"):
            validate_version("1.2.4", root)

    def test_rejects_noncanonical_semver(self) -> None:
        root = self.workspace()
        for value in ("01.2.3", "1.2", "1.2.3-01", "1.2.3+"):
            with self.subTest(value=value), self.assertRaises(VersionError):
                validate_version(value, root)


if __name__ == "__main__":
    unittest.main()

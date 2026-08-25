#!/usr/bin/env python3

from __future__ import annotations

import pathlib
import tempfile
import unittest

from tools.publish_release_assets import PLUGIN_IDS, materialize, sha256


class PublishReleaseAssetsTests(unittest.TestCase):
    def test_materializes_target_unique_bytes_hashes_and_signature(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            source = root / "source"
            source.mkdir()
            for index, plugin_id in enumerate(PLUGIN_IDS):
                package = source / f"{plugin_id}.imp"
                package.write_bytes(f"package-{index}".encode())
            (source / f"{PLUGIN_IDS[0]}.imp.asc").write_bytes(b"signature")
            output = root / "published"

            materialize(source, output, "x86_64-pc-windows-msvc")

            for index, plugin_id in enumerate(PLUGIN_IDS):
                package = output / f"{plugin_id}-x86_64-pc-windows-msvc.imp"
                self.assertEqual(package.read_bytes(), f"package-{index}".encode())
                self.assertEqual(
                    package.with_name(f"{package.name}.sha256").read_text(encoding="ascii"),
                    f"{sha256(package)}  {package.name}\n",
                )
            self.assertEqual(
                (output / f"{PLUGIN_IDS[0]}-x86_64-pc-windows-msvc.imp.asc").read_bytes(),
                b"signature",
            )

    def test_rejects_unsafe_target_and_partial_source(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            source = root / "source"
            source.mkdir()
            with self.assertRaisesRegex(RuntimeError, "target"):
                materialize(source, root / "bad", "../windows")
            with self.assertRaisesRegex(RuntimeError, "missing"):
                materialize(source, root / "missing", "aarch64-apple-darwin")


if __name__ == "__main__":
    unittest.main()

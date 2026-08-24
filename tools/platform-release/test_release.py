#!/usr/bin/env python3
"""Host-independent contract tests for the Linux and Windows release assembler."""

from __future__ import annotations

import pathlib
import sys
import tempfile
import unittest
import zipfile

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from common import authority, sha256
from legacy_authority import LEGACY_PLUGIN_ID, inventory_by_identity
from release import (
    CORE_COMPONENTS,
    OCR_COMPONENTS,
    SPEECH_COMPONENTS,
    create_archive,
    distributed_source_ids,
    libreoffice_component,
)


class PlatformReleaseTests(unittest.TestCase):
    def test_release_projection_excludes_non_distributed_source_records(self) -> None:
        manifest = {
            "components": [
                {"id": "cargo:runtime@1", "distributed": True},
                {"id": "npm:build@1", "distributed": False},
                {"id": "font:test", "distributed": False},
            ]
        }
        self.assertEqual(distributed_source_ids(manifest), ["cargo:runtime@1"])

    def test_release_matrix_has_exact_core_and_plugin_resource_partition(self) -> None:
        self.assertEqual(CORE_COMPONENTS, ["pdfium"])
        for target in authority()["targets"]:
            groups = {
                "core": set(CORE_COMPONENTS),
                "ocr": set(OCR_COMPONENTS),
                "speech": set(SPEECH_COMPONENTS),
                "legacy-office": {libreoffice_component(target)},
            }
            for plugin, components in groups.items():
                if plugin != "core":
                    self.assertFalse(
                        groups["core"] & components,
                        f"{target}: Core and {plugin} duplicate release resources",
                    )
            self.assertIn("onnxruntime-cpu", groups["ocr"])
            self.assertIn("onnxruntime-cpu", groups["speech"])

    def test_authority_is_exact_and_hash_pinned(self) -> None:
        value = authority()
        self.assertEqual(value["sourceDateEpoch"], 1_767_225_600)
        for target, config in value["targets"].items():
            self.assertIn(config["os"], {"linux", "windows"})
            for name in ["pdfium", "onnxruntime", "libreoffice"]:
                download = config[name]
                self.assertTrue(download["url"].startswith("https://"), (target, name))
                self.assertRegex(download["sha256"], r"^[0-9a-f]{64}$")
                if name != "onnxruntime":
                    self.assertGreater(download["bytes"], 0)
        for download in value["sharedDownloads"].values():
            self.assertTrue(download["url"].startswith("https://"))
            self.assertGreater(download["bytes"], 0)
            self.assertRegex(download["sha256"], r"^[0-9a-f]{64}$")

    def test_dependency_index_never_silently_overwrites_duplicate_names(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            (root / "a").mkdir()
            (root / "b").mkdir()
            (root / "a/libsame.so").write_bytes(b"a")
            (root / "b/libsame.so").write_bytes(b"b")
            inventory = inventory_by_identity(root, case_sensitive=True)
            self.assertEqual(len(inventory["libsame.so"]), 2)

    def test_windows_legacy_sandbox_identity_matches_the_installer_contract(self) -> None:
        import hashlib

        suffix = hashlib.sha256(LEGACY_PLUGIN_ID.encode("ascii")).hexdigest()[:24]
        self.assertEqual(suffix, "8d67189097d50455950e62f7")

    def test_windows_zip_is_byte_reproducible_and_contains_only_regular_files(self) -> None:
        config = {"archive": "zip"}
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            source = root / "source"
            source.mkdir()
            (source / "z.txt").write_text("z\n", encoding="utf-8")
            (source / "a.txt").write_text("a\n", encoding="utf-8")
            first = root / "first.zip"
            second = root / "second.zip"
            create_archive(source, first, config, 1_767_225_600)
            create_archive(source, second, config, 1_767_225_600)
            self.assertEqual(sha256(first), sha256(second))
            with zipfile.ZipFile(first) as archive:
                self.assertEqual(archive.namelist(), ["a.txt", "z.txt"])
                self.assertEqual(
                    [item.date_time for item in archive.infolist()],
                    [(2026, 1, 1, 0, 0, 0), (2026, 1, 1, 0, 0, 0)],
                )


if __name__ == "__main__":
    unittest.main()

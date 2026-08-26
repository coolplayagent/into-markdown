#!/usr/bin/env python3
"""Host-independent contract tests for the Linux and Windows release assembler."""

from __future__ import annotations

import pathlib
import re
import sys
import tempfile
import unittest
import zipfile

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from common import authority, sha256
from release import (
    CORE_COMPONENTS,
    OCR_COMPONENTS,
    SPEECH_COMPONENTS,
    create_archive,
    distributed_source_ids,
    published_plugin_file,
)


class PlatformReleaseTests(unittest.TestCase):
    def test_published_plugin_names_are_flat_and_target_unique(self) -> None:
        self.assertEqual(
            published_plugin_file("official.ocr.ppocrv6.imp", "x86_64-pc-windows-msvc"),
            "official.ocr.ppocrv6-x86_64-pc-windows-msvc.imp",
        )
        with self.assertRaisesRegex(RuntimeError, "filename"):
            published_plugin_file("nested/package.imp", "x86_64-pc-windows-msvc")

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
            baseline = config["buildBaseline"]
            self.assertNotIn("native", baseline["cpu"])
            if config["os"] == "linux":
                self.assertEqual(baseline["glibcMaximum"], "2.28")
                self.assertEqual(baseline["kernelMinimum"], "5.15")
                self.assertTrue(
                    baseline["container"].startswith(
                        "docker.io/rockylinux/rockylinux:8.10@sha256:"
                    ),
                    (target, baseline["container"]),
                )
                self.assertRegex(baseline["container"], r"@sha256:[0-9a-f]{64}$")
            else:
                self.assertRegex(baseline["msvcTools"], r"^\d+\.\d+\.\d+$")
                self.assertRegex(baseline["windowsSdk"], r"^\d+\.\d+\.\d+\.\d+$")
            for name in ["pdfium", "onnxruntime"]:
                download = config[name]
                self.assertTrue(download["url"].startswith("https://"), (target, name))
                self.assertRegex(download["sha256"], r"^[0-9a-f]{64}$")
                if name == "pdfium":
                    self.assertGreater(download["bytes"], 0)
        for download in value["sharedDownloads"].values():
            self.assertTrue(download["url"].startswith("https://"))
            self.assertGreater(download["bytes"], 0)
            self.assertRegex(download["sha256"], r"^[0-9a-f]{64}$")

    def test_installed_smoke_uses_the_pinned_windows_native_toolchain(self) -> None:
        windows = authority()["targets"]["x86_64-pc-windows-msvc"]["buildBaseline"]
        consumer = (
            pathlib.Path(__file__).resolve().parents[1]
            / "installed-smoke"
            / "src"
            / "rust_consumer.rs"
        ).read_text(encoding="utf-8")
        constants = dict(
            re.findall(r'const (MSVC_VERSION|SDK_VERSION): &str = "([^"]+)";', consumer)
        )
        self.assertEqual(constants["MSVC_VERSION"], windows["msvcTools"])
        self.assertEqual(constants["SDK_VERSION"], windows["windowsSdk"])

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

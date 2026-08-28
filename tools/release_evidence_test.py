#!/usr/bin/env python3
from __future__ import annotations

import json
import pathlib
import tempfile
import unittest
import zipfile

from tools.release_evidence import build_bundle


REVISION = "a" * 40


class ReleaseEvidenceTests(unittest.TestCase):
    def test_recurses_deduplicates_and_is_deterministic(self):
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            source = root / "source"
            (source / "windows/core").mkdir(parents=True)
            (source / "linux/core").mkdir(parents=True)
            (source / "windows/core/SOURCES.json").write_bytes(b"shared")
            (source / "linux/core/SOURCES.json").write_bytes(b"shared")
            (source / "linux/core/platform-audit.json").write_bytes(b"linux")
            first = root / "first.zip"
            second = root / "second.zip"

            build_bundle(source, first, REVISION)
            build_bundle(source, second, REVISION)

            self.assertEqual(first.read_bytes(), second.read_bytes())
            with zipfile.ZipFile(first) as bundle:
                names = bundle.namelist()
                self.assertEqual(names, sorted(names))
                self.assertEqual(
                    len([name for name in names if name.startswith("objects/")]), 2
                )
                manifest = json.loads(bundle.read("manifest.json"))
            shared = next(item for item in manifest["objects"] if item["bytes"] == 6)
            self.assertEqual(len(shared["sourcePaths"]), 2)
            self.assertEqual(manifest["sourceRevision"], REVISION)

    def test_rejects_signing_material_and_existing_output(self):
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            source = root / "source"
            source.mkdir()
            (source / "plugin-key.pk8").write_bytes(b"secret")
            with self.assertRaisesRegex(RuntimeError, "signing material"):
                build_bundle(source, root / "bad.zip", REVISION)

            (source / "plugin-key.pk8").unlink()
            (source / "report.json").write_text("{}", encoding="utf-8")
            output = root / "output.zip"
            build_bundle(source, output, REVISION)
            with self.assertRaisesRegex(RuntimeError, "already exists"):
                build_bundle(source, output, REVISION)

    def test_incremental_merge_is_rejected(self):
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            source = root / "source"
            source.mkdir()
            (source / "report.json").write_text("{}", encoding="utf-8")
            with self.assertRaisesRegex(RuntimeError, "aggregate all targets"):
                build_bundle(source, root / "output.zip", REVISION, root / "old.zip")


if __name__ == "__main__":
    unittest.main()

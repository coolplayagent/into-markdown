#!/usr/bin/env python3

from __future__ import annotations

import json
import pathlib
import tempfile
import unittest
import zipfile

from tools.release_evidence import build_bundle


REVISION = "a" * 40
OTHER_REVISION = "b" * 40


def write_target(source: pathlib.Path, target: str, revision: str, marker: str) -> None:
    (source / f"{target}-installed-smoke.json").write_text(
        json.dumps({"marker": marker}), encoding="utf-8"
    )
    (source / f"into-markdown-{target}-release-set.json").write_text(
        json.dumps({"source_revision": revision}), encoding="utf-8"
    )
    (source / f"into-markdown-{target}-release-set.spdx.json").write_text(
        json.dumps({"name": marker}), encoding="utf-8"
    )
    (source / f"into-md-{target}-core.tar.gz.asc").write_text(
        f"signature-{marker}\n", encoding="ascii"
    )
    core_names = {
        "x86_64-unknown-linux-gnu": "into-md-linux-x86_64-core.tar.gz",
        "aarch64-apple-darwin": "into-md-macos-arm64-core.dmg",
    }
    (source / f"{core_names[target]}.sha256").write_text(
        f"{'0' * 64}  {core_names[target]}\n", encoding="ascii"
    )
    (source / f"official.media.whisper-{target}.imp.sha256").write_text(
        f"{'1' * 64}  official.media.whisper-{target}.imp\n", encoding="ascii"
    )


class ReleaseEvidenceTests(unittest.TestCase):
    def test_merges_targets_deterministically_and_writes_digest(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            linux = root / "linux"
            linux.mkdir()
            write_target(linux, "x86_64-unknown-linux-gnu", REVISION, "linux")
            first = root / "first.zip"
            build_bundle(linux, first, REVISION)

            macos = root / "macos"
            macos.mkdir()
            write_target(macos, "aarch64-apple-darwin", REVISION, "macos")
            merged = root / "merged.zip"
            build_bundle(macos, merged, REVISION, first)
            repeated = root / "repeated.zip"
            build_bundle(macos, repeated, REVISION, first)

            self.assertEqual(merged.read_bytes(), repeated.read_bytes())
            with zipfile.ZipFile(merged) as bundle:
                self.assertEqual(len(bundle.namelist()), 12)
                self.assertEqual(bundle.namelist(), sorted(bundle.namelist()))
            sidecar = pathlib.Path(f"{merged}.sha256").read_text(encoding="ascii")
            self.assertTrue(sidecar.endswith("  merged.zip\n"))

    def test_discards_existing_archive_from_another_revision(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            old = root / "old"
            old.mkdir()
            write_target(old, "x86_64-unknown-linux-gnu", OTHER_REVISION, "old")
            old_bundle = root / "old.zip"
            build_bundle(old, old_bundle, OTHER_REVISION)

            current = root / "current"
            current.mkdir()
            write_target(current, "aarch64-apple-darwin", REVISION, "current")
            output = root / "output.zip"
            build_bundle(current, output, REVISION, old_bundle)

            with zipfile.ZipFile(output) as bundle:
                self.assertEqual(len(bundle.namelist()), 6)
                self.assertFalse(any("linux" in item for item in bundle.namelist()))

    def test_rejects_unexpected_names_and_disagreeing_bytes(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            source = root / "source"
            source.mkdir()
            (source / "secret.txt").write_text("no", encoding="utf-8")
            with self.assertRaisesRegex(RuntimeError, "unexpected"):
                build_bundle(source, root / "bad.zip", REVISION)

            source.joinpath("secret.txt").unlink()
            (source / "secret.sha256").write_text("no", encoding="utf-8")
            with self.assertRaisesRegex(RuntimeError, "unexpected"):
                build_bundle(source, root / "bad-checksum.zip", REVISION)
            source.joinpath("secret.sha256").unlink()
            write_target(source, "aarch64-apple-darwin", REVISION, "first")
            first = root / "first.zip"
            build_bundle(source, first, REVISION)
            (source / "aarch64-apple-darwin-installed-smoke.json").write_text(
                json.dumps({"marker": "changed"}), encoding="utf-8"
            )
            with self.assertRaisesRegex(RuntimeError, "disagree"):
                build_bundle(source, root / "changed.zip", REVISION, first)


if __name__ == "__main__":
    unittest.main()

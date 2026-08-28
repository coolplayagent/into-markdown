#!/usr/bin/env python3

from __future__ import annotations

import pathlib
import tempfile
import unittest

from tools.stage_release_artifact import REPORT_NAMES, stage_release_artifact


class StageReleaseArtifactTests(unittest.TestCase):
    def create_inputs(self, root: pathlib.Path) -> dict[str, pathlib.Path]:
        core = root / "into-md-linux-x86_64-core.tar.gz"
        core.write_bytes(b"core")
        pathlib.Path(f"{core}.sha256").write_text("digest\n", encoding="ascii")
        plugins = root / "plugins"
        plugins.mkdir()
        (plugins / "official.ocr.imp").write_bytes(b"ocr")
        (plugins / "official.ocr.imp.sha256").write_text("ocr digest\n", encoding="ascii")
        metadata = root / "metadata"
        metadata.mkdir()
        (metadata / "release-set.json").write_text("{}", encoding="utf-8")
        reports = []
        for name in REPORT_NAMES:
            report = root / name
            report.write_text("{}", encoding="utf-8")
            reports.append(report)
        signing_policy = root / "x86_64-unknown-linux-gnu-signing-policy.json"
        signing_policy.write_text("{}", encoding="utf-8")
        return {
            "core": core,
            "plugins": plugins,
            "metadata": metadata,
            "platform_audit": reports[0],
            "installed_smoke": reports[1],
            "platform_acceptance": reports[2],
            "signing_policy": signing_policy,
        }

    def stage(self, inputs: dict[str, pathlib.Path], output: pathlib.Path, signed: bool = False) -> None:
        stage_release_artifact(
            inputs["core"],
            inputs["plugins"],
            inputs["metadata"],
            inputs["platform_audit"],
            inputs["installed_smoke"],
            inputs["platform_acceptance"],
            inputs["signing_policy"],
            output,
            signed,
        )

    def test_stages_portable_complete_hierarchy(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            inputs = self.create_inputs(root)
            output = root / "release-upload"

            self.stage(inputs, output)

            self.assertEqual((output / inputs["core"].name).read_bytes(), b"core")
            self.assertTrue((output / f"{inputs['core'].name}.sha256").is_file())
            self.assertTrue((output / "published-plugins" / "official.ocr.imp").is_file())
            self.assertTrue((output / "release-metadata" / "release-set.json").is_file())
            for report in REPORT_NAMES:
                self.assertTrue((output / report).is_file())
            self.assertTrue((output / inputs["signing_policy"].name).is_file())

    def test_requires_signature_in_signed_mode_and_rejects_existing_output(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            inputs = self.create_inputs(root)
            with self.assertRaisesRegex(RuntimeError, "detached signature"):
                self.stage(inputs, root / "missing-signature", signed=True)
            output = root / "existing"
            output.mkdir()
            with self.assertRaisesRegex(RuntimeError, "already exists"):
                self.stage(inputs, output)


if __name__ == "__main__":
    unittest.main()

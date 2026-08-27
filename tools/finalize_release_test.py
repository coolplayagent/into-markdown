from __future__ import annotations

import hashlib
import importlib.util
import json
import pathlib
import tempfile
import unittest
from unittest import mock


PATH = pathlib.Path(__file__).with_name("finalize_release.py")
SPEC = importlib.util.spec_from_file_location("finalize_release", PATH)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)


class FinalizeReleaseTest(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = pathlib.Path(temporary.name)
        self.source = self.root / "source"
        self.output = self.root / "output"
        self.source.mkdir()
        self.version = "1.2.3"
        self.revision = "a" * 40
        self.materialize("into-markdown-agent-skill", "into-markdown-skill.zip")
        for target, core in MODULE.TARGET_CORES.items():
            artifact = (
                "into-md-macos-arm64-modular"
                if target == "aarch64-apple-darwin"
                else f"modular-{target}"
            )
            self.materialize(artifact, core)
            for plugin_id in MODULE.PLUGIN_IDS:
                self.materialize(artifact, f"{plugin_id}-{target}.imp")
            for release_name in [
                core,
                *(f"{plugin_id}-{target}.imp" for plugin_id in MODULE.PLUGIN_IDS),
            ]:
                self.write_json(artifact, f"{release_name}.spdx.json", {"name": release_name})
                self.write_json(artifact, f"{release_name}.sources.json", {"name": release_name})
                self.write_text(
                    artifact,
                    f"{release_name}.THIRD_PARTY_NOTICES.md",
                    "notices\n",
                )
            self.write_json(
                artifact,
                f"{target}-signing-policy.json",
                {
                    "schemaVersion": 1,
                    "target": target,
                    "sourceRevision": self.revision,
                    "mode": "unsigned",
                    "externalPublisherIdentityVerified": False,
                },
            )
            self.write_json(artifact, "platform-audit.json", {"target": target, "passed": True})
            self.write_json(
                artifact,
                "platform-acceptance.json",
                {"target": target, "conclusion": "passed"},
            )
            self.write_json(
                artifact,
                "installed-smoke.json",
                {
                    "platform": MODULE.TARGET_HOSTS[target][0],
                    "architecture": MODULE.TARGET_HOSTS[target][1],
                    "passed": True,
                },
            )
            self.write_json(
                artifact,
                f"into-markdown-{target}-release-set.json",
                {
                    "schema_version": 1,
                    "target": target,
                    "version": self.version,
                    "source_revision": self.revision,
                    "profiles": {
                        "core": [core],
                        "complete-offline": [
                            core,
                            f"official.ocr.ppocrv6-{target}.imp",
                            f"official.media.whisper-{target}.imp",
                        ],
                    },
                    "complete_offline_minus_core": {
                        "artifacts": [
                            f"official.ocr.ppocrv6-{target}.imp",
                            f"official.media.whisper-{target}.imp",
                        ]
                    },
                    "artifacts": [
                        self.release_set_artifact(artifact, "core", core),
                        self.release_set_artifact(
                            artifact,
                            "ocr-plugin",
                            f"official.ocr.ppocrv6-{target}.imp",
                        ),
                        self.release_set_artifact(
                            artifact,
                            "media-plugin",
                            f"official.media.whisper-{target}.imp",
                        ),
                    ],
                },
            )
            self.write_json(
                artifact,
                f"into-markdown-{target}-release-set.spdx.json",
                {"name": target},
            )

    def materialize(self, artifact: str, name: str) -> None:
        root = self.source / artifact
        root.mkdir(parents=True, exist_ok=True)
        path = root / name
        path.write_bytes(name.encode())
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        (root / f"{name}.sha256").write_text(f"{digest}  {name}\n", encoding="ascii")

    def write_json(self, artifact: str, name: str, value: dict) -> None:
        self.write_text(artifact, name, json.dumps(value))

    def write_text(self, artifact: str, name: str, value: str) -> None:
        root = self.source / artifact
        root.mkdir(parents=True, exist_ok=True)
        (root / name).write_text(value, encoding="utf-8")

    def release_set_artifact(self, artifact: str, kind: str, name: str) -> dict:
        root = self.source / artifact
        path = root / name
        return {
            "artifact": kind,
            "file_name": name,
            "bytes": path.stat().st_size,
            "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
            "components": [f"component:{kind}"],
            "sbom_sha256": hashlib.sha256(
                (root / f"{name}.spdx.json").read_bytes()
            ).hexdigest(),
            "sources_sha256": hashlib.sha256(
                (root / f"{name}.sources.json").read_bytes()
            ).hexdigest(),
            "notices_sha256": hashlib.sha256(
                (root / f"{name}.THIRD_PARTY_NOTICES.md").read_bytes()
            ).hexdigest(),
        }

    @mock.patch.object(MODULE, "validate_version", return_value="1.2.3")
    def test_flattens_complete_target_set_and_writes_manifest(self, _: mock.Mock) -> None:
        manifest = MODULE.finalize(
            self.source, self.output, "v1.2.3", self.version, self.revision, "unsigned"
        )
        self.assertEqual(manifest["targets"], sorted(MODULE.TARGET_CORES))
        self.assertTrue((self.output / "release-manifest.json").is_file())
        self.assertTrue((self.output / "SHA256SUMS").is_file())
        self.assertTrue(
            (self.output / "aarch64-pc-windows-msvc-platform-acceptance.json").is_file()
        )

    @mock.patch.object(MODULE, "validate_version", return_value="1.2.3")
    def test_rejects_duplicate_flat_assets(self, _: mock.Mock) -> None:
        duplicate = self.source / "modular-x86_64-unknown-linux-gnu" / "nested"
        duplicate.mkdir()
        (duplicate / "into-markdown-skill.zip").write_bytes(b"duplicate")
        with self.assertRaisesRegex(RuntimeError, "duplicate flat release asset"):
            MODULE.finalize(
                self.source,
                self.output,
                "v1.2.3",
                self.version,
                self.revision,
                "unsigned",
            )

    @mock.patch.object(MODULE, "validate_version", return_value="1.2.3")
    def test_rejects_checksum_drift(self, _: mock.Mock) -> None:
        path = (
            self.source
            / "modular-aarch64-pc-windows-msvc"
            / "into-md-windows-arm64-core.zip"
        )
        path.write_bytes(b"drift")
        with self.assertRaisesRegex(RuntimeError, "sidecar disagrees"):
            MODULE.finalize(
                self.source,
                self.output,
                "v1.2.3",
                self.version,
                self.revision,
                "unsigned",
            )

    @mock.patch.object(MODULE, "validate_version", return_value="1.2.3")
    def test_rejects_unexpected_release_asset(self, _: mock.Mock) -> None:
        self.write_text("modular-aarch64-pc-windows-msvc", "orphan.bin", "orphan")
        with self.assertRaisesRegex(RuntimeError, "payload is not exact"):
            MODULE.finalize(
                self.source,
                self.output,
                "v1.2.3",
                self.version,
                self.revision,
                "unsigned",
            )

    @mock.patch.object(MODULE, "validate_version", return_value="1.2.3")
    def test_signed_payload_requires_exact_linux_signatures(self, _: mock.Mock) -> None:
        for target, core in MODULE.TARGET_CORES.items():
            artifact = (
                "into-md-macos-arm64-modular"
                if target == "aarch64-apple-darwin"
                else f"modular-{target}"
            )
            self.write_json(
                artifact,
                f"{target}-signing-policy.json",
                {
                    "schemaVersion": 1,
                    "target": target,
                    "sourceRevision": self.revision,
                    "mode": "signed",
                    "externalPublisherIdentityVerified": True,
                },
            )
            if target.endswith("linux-gnu"):
                for name in [
                    core,
                    *(f"{plugin_id}-{target}.imp" for plugin_id in MODULE.PLUGIN_IDS),
                ]:
                    self.write_text(artifact, f"{name}.asc", "signature")
        MODULE.finalize(
            self.source,
            self.output,
            "v1.2.3",
            self.version,
            self.revision,
            "signed",
        )
        self.assertTrue((self.output / "into-md-linux-arm64-core.tar.gz.asc").is_file())


if __name__ == "__main__":
    unittest.main()

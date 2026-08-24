#!/usr/bin/env python3
"""Contract tests for final release artifact projections."""

from __future__ import annotations

import importlib.util
import json
import pathlib
import tempfile
import unittest
import warnings
import zipfile


SCRIPT = pathlib.Path(__file__).with_name("release-metadata.py")
SPEC = importlib.util.spec_from_file_location("release_metadata", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
release_metadata = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release_metadata)


class ReleaseMetadataTests(unittest.TestCase):
    def test_core_projection_requires_exact_archive_manifest_members(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            temporary = pathlib.Path(name)
            root = temporary / "stage"
            root.mkdir()
            artifact = temporary / "core.tar.gz"
            artifact.write_bytes(b"artifact")
            payload = root / "bin" / "into-md"
            payload.parent.mkdir()
            payload.write_bytes(b"binary")
            manifest = {
                "target": "aarch64-apple-darwin",
                "version": "0.0.0",
                "source_revision": "0" * 40,
                "components": ["cargo:example@1.0.0"],
                "files": [
                    {
                        "path": "bin/into-md",
                        "bytes": payload.stat().st_size,
                        "sha256": release_metadata.sha256(payload),
                        "kind": "project",
                        "embedded_components": ["cargo:example@1.0.0"],
                    }
                ],
            }
            (root / "archive-manifest.json").write_text(
                json.dumps(manifest), encoding="utf-8"
            )
            projection = release_metadata.core_projection(
                "aarch64-apple-darwin", "0.0.0", "0" * 40, artifact, root
            )
            self.assertEqual(projection["sha256"], release_metadata.sha256(artifact))
            self.assertEqual(projection["source_revision"], "0" * 40)
            self.assertEqual(
                {item["path"] for item in projection["files"]},
                {"archive-manifest.json", "bin/into-md"},
            )
            self.assertTrue(all(len(item["sha1"]) == 40 for item in projection["files"]))

            payload.write_bytes(b"drift")
            with self.assertRaisesRegex(RuntimeError, "differs from archive manifest"):
                release_metadata.core_projection(
                    "aarch64-apple-darwin", "0.0.0", "0" * 40, artifact, root
                )

    def test_plugin_projection_filters_build_inputs_and_owns_runtime_files(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            artifact = pathlib.Path(name) / "official.ocr.ppocrv6.imp"
            sources = {
                "target": "x86_64-unknown-linux-gnu",
                "version": "0.0.0",
                "source_revision": "1" * 40,
                "artifact": "official.ocr.ppocrv6",
                "components": [
                    {"id": "cargo:example@1.0.0", "distributed": True},
                    {"id": "onnxruntime-cpu", "distributed": True},
                    {"id": "npm:vite@1.0.0", "distributed": False},
                ]
            }
            contents = {
                "SOURCES.json": json.dumps(sources).encode(),
                "bin/into-md-ocr-provider": b"provider",
                "onnxruntime/lib/libonnxruntime.so": b"runtime",
            }
            inventory = [
                {
                    "path": path,
                    "bytes": len(value),
                    "sha256": release_metadata.sha256_bytes(value),
                    "executable": path.startswith("bin/"),
                }
                for path, value in contents.items()
            ]
            provider = json.dumps(
                {
                    "targets": [
                        {
                            "triple": "x86_64-unknown-linux-gnu",
                            "files": inventory,
                        }
                    ]
                }
            ).encode()
            contents["provider.json"] = provider
            signed_inventory = [
                {
                    "path": path,
                    "bytes": len(value),
                    "sha256": release_metadata.sha256_bytes(value),
                    "executable": path.startswith("bin/"),
                }
                for path, value in contents.items()
            ]
            manifest = {
                "supportedTargets": ["x86_64-unknown-linux-gnu"],
                "entrypoints": {
                    "x86_64-unknown-linux-gnu": "bin/into-md-ocr-provider"
                },
                "files": signed_inventory,
                "signature": {
                    "publicKeySha256": "a" * 64,
                    "signedPayloadSha256": "b" * 64,
                },
            }
            with zipfile.ZipFile(artifact, "w") as package:
                for path, value in contents.items():
                    package.writestr(path, value)
                package.writestr("plugin.json", json.dumps(manifest))
            projection = release_metadata.plugin_projection(
                "x86_64-unknown-linux-gnu", "0.0.0", "1" * 40, artifact
            )
            self.assertEqual(
                projection["components"],
                ["cargo:example@1.0.0", "onnxruntime-cpu"],
            )
            provider = next(
                item
                for item in projection["files"]
                if item["path"] == "bin/into-md-ocr-provider"
            )
            self.assertEqual(provider["embedded_components"], ["cargo:example@1.0.0"])
            runtime = next(
                item
                for item in projection["files"]
                if item["path"].endswith("libonnxruntime.so")
            )
            self.assertEqual(runtime["component_id"], "onnxruntime-cpu")
            self.assertTrue(all(len(item["sha1"]) == 40 for item in projection["files"]))

    def test_plugin_projection_rejects_duplicate_zip_members(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            artifact = pathlib.Path(name) / "official.ocr.ppocrv6.imp"
            with warnings.catch_warnings():
                warnings.simplefilter("ignore", UserWarning)
                with zipfile.ZipFile(artifact, "w") as package:
                    package.writestr("SOURCES.json", '{"components":[]}')
                    package.writestr("SOURCES.json", '{"components":[]}')
            with self.assertRaisesRegex(RuntimeError, "duplicate members"):
                release_metadata.plugin_projection(
                    "aarch64-apple-darwin", "0.0.0", "2" * 40, artifact
                )


if __name__ == "__main__":
    unittest.main()

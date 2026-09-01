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
from types import SimpleNamespace
from unittest import mock


SCRIPT = pathlib.Path(__file__).with_name("release-metadata.py")
SPEC = importlib.util.spec_from_file_location("release_metadata", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
release_metadata = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release_metadata)


class ReleaseMetadataTests(unittest.TestCase):
    def test_default_release_metadata_version_matches_current_release(self) -> None:
        self.assertEqual(release_metadata.project_version(), "0.0.6")

    def test_linux_cxx_execution_uses_a_portable_projection_identity(self) -> None:
        self.assertEqual(release_metadata.build_tool_execution_name("c++"), "cxx")
        self.assertEqual(release_metadata.build_tool_execution_name("cc"), "cc")

    def test_generated_metadata_preserves_declared_utf8_bytes(self) -> None:
        contents = "line one\nline two\n"
        encoded = contents.encode("utf-8")
        metadata = {
            "sbom": {
                "path": "artifact.spdx.json",
                "contents": contents,
                "bytes": len(encoded),
                "sha256": release_metadata.sha256_bytes(encoded),
            }
        }
        with tempfile.TemporaryDirectory() as name:
            output = pathlib.Path(name)
            release_metadata.write_generated(output, metadata)
            self.assertEqual((output / "artifact.spdx.json").read_bytes(), encoded)

    def test_projection_subprocess_always_decodes_utf8(self) -> None:
        completed = SimpleNamespace(returncode=0, stdout='{"status":"ok"}', stderr="")
        with mock.patch.object(release_metadata.subprocess, "run", return_value=completed) as run:
            result = release_metadata.run_projection(
                pathlib.Path("release-projection"), "finalize", {"鍚嶇О": "鍙戝竷"}
            )
        self.assertEqual(result, {"status": "ok"})
        self.assertEqual(run.call_args.kwargs["encoding"], "utf-8")
        self.assertEqual(run.call_args.kwargs["errors"], "replace")

    def test_build_tool_integrity_digests_match_every_authority_subject(self) -> None:
        repository = SCRIPT.parent.parent
        inventory = json.loads(
            (repository / "third_party/licenses/build-tools.json").read_text(
                encoding="utf-8"
            )
        )
        for tool in inventory["tools"]:
            for integrity in tool["integrity"]:
                self.assertEqual(integrity["algorithm"], "SHA-256")
                subject = repository / integrity["subject"]
                self.assertEqual(
                    integrity["digest"],
                    release_metadata.sha256(subject),
                    f"{tool['id']} integrity drifted for {integrity['subject']}",
                )

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
                    {
                        "id": "ppocrv6-tiny-detector-onnx-model",
                        "distributed": True,
                    },
                    {
                        "id": "ppocrv6-tiny-recognizer-character-table",
                        "distributed": True,
                    },
                    {
                        "id": "ppocrv6-tiny-recognizer-onnx-model",
                        "distributed": True,
                    },
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
                [
                    "cargo:example@1.0.0",
                    "onnxruntime-cpu",
                    "ppocrv6-tiny-detector-onnx-model",
                    "ppocrv6-tiny-recognizer-character-table",
                    "ppocrv6-tiny-recognizer-onnx-model",
                ],
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

            published = artifact.with_name(
                "official.ocr.ppocrv6-x86_64-unknown-linux-gnu.imp"
            )
            artifact.rename(published)
            projection = release_metadata.plugin_projection(
                "x86_64-unknown-linux-gnu", "0.0.0", "1" * 40, published
            )
            self.assertEqual(projection["artifact"], "ocr-plugin")
            self.assertEqual(projection["file_name"], published.name)

    def test_plugin_runtime_components_are_scoped_to_the_artifact(self) -> None:
        global_sources = [
            "cargo:shared@1.0.0",
            *release_metadata.PLUGIN_RUNTIME_COMPONENTS["ocr-plugin"],
            *release_metadata.PLUGIN_RUNTIME_COMPONENTS["media-plugin"],
        ]
        for artifact, expected in release_metadata.PLUGIN_RUNTIME_COMPONENTS.items():
            selected = release_metadata.scoped_plugin_components(
                global_sources, artifact
            )
            self.assertEqual(
                set(selected) & release_metadata.OCR_MODEL_COMPONENTS,
                release_metadata.OCR_MODEL_COMPONENTS
                if artifact == "ocr-plugin"
                else set(),
            )
            self.assertEqual(
                set(selected)
                & set(release_metadata.PLUGIN_RUNTIME_COMPONENTS["media-plugin"]),
                set(expected) if artifact == "media-plugin" else {"onnxruntime-cpu"},
            )

    def test_ocr_model_ownership_is_digest_bound_across_archive_paths(self) -> None:
        detector_digest = (
            "193bab7a04fca699a6c82e6abb5b81bdb28177f0abd4062552b04908dafb19f8"
        )
        self.assertEqual(
            release_metadata.plugin_owner("models/runtime/detector.onnx", detector_digest),
            "ppocrv6-tiny-detector-onnx-model",
        )
        with self.assertRaisesRegex(RuntimeError, "differs from download authority"):
            release_metadata.plugin_owner(
                "models/pp-ocrv6-tiny-detector-onnx/inference.onnx", "0" * 64
            )

    def test_projection_ownership_failure_identifies_artifact_and_candidates(self) -> None:
        projection = {
            "artifact": "ocr-plugin",
            "file_name": "official.ocr.ppocrv6-aarch64-apple-darwin.imp",
            "components": ["ppocrv6-tiny-detector-onnx-model"],
            "files": [
                {
                    "path": "models/unexpected/detector.onnx",
                    "bytes": 7,
                    "sha256": "a" * 64,
                    "kind": "project",
                }
            ],
        }
        with self.assertRaisesRegex(
            RuntimeError,
            "ocr-plugin .*unowned components.*models/unexpected/detector.onnx",
        ):
            release_metadata.validate_projection_ownership(projection)

    def test_bundled_ocr_is_owned_by_core_instead_of_a_release_plugin(self) -> None:
        package_path = release_metadata.BUNDLED_OCR_PATH.as_posix()
        core = {
            "artifact": "core",
            "components": ["pdfium", "cargo:core@1.0.0"],
            "files": [
                {
                    "path": package_path,
                    "bytes": 7,
                    "sha256": "a" * 64,
                    "kind": "project",
                }
            ],
        }
        ocr = {
            "artifact": "ocr-plugin",
            "components": ["onnxruntime-cpu", "ppocrv6-tiny-detector-onnx-model"],
            "bytes": 7,
            "sha256": "a" * 64,
        }
        release_metadata.fold_bundled_ocr_into_core(core, ocr)
        self.assertEqual(
            set(core["components"]),
            {
                "pdfium",
                "cargo:core@1.0.0",
                "onnxruntime-cpu",
                "ppocrv6-tiny-detector-onnx-model",
            },
        )
        self.assertEqual(
            set(core["files"][0]["embedded_components"]), set(ocr["components"])
        )
        self.assertEqual(
            release_metadata.EXTERNAL_ARTIFACTS,
            {"official.media.whisper.imp": "media-plugin"},
        )

        core["files"][0]["sha256"] = "b" * 64
        with self.assertRaisesRegex(RuntimeError, "differs from verified"):
            release_metadata.fold_bundled_ocr_into_core(core, ocr)

    def test_release_metadata_rejects_extra_imp_packages(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            (root / "official.ocr.ppocrv6.imp").write_bytes(b"ocr")
            (root / "official.media.whisper.imp").write_bytes(b"speech")
            selected = release_metadata.resolve_plugin_artifacts(
                root, "x86_64-pc-windows-msvc"
            )
            self.assertEqual(set(selected), set(release_metadata.ARTIFACTS))
            (root / "unexpected.imp").write_bytes(b"unexpected")
            with self.assertRaisesRegex(RuntimeError, "unauthorized IMP"):
                release_metadata.resolve_plugin_artifacts(
                    root, "x86_64-pc-windows-msvc"
                )

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

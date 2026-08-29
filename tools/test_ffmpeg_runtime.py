from __future__ import annotations

import hashlib
import importlib.util
import json
import pathlib
import tempfile
import unittest
from unittest import mock


PATH = pathlib.Path(__file__).with_name("ffmpeg_runtime.py")
SPEC = importlib.util.spec_from_file_location("ffmpeg_runtime", PATH)
assert SPEC and SPEC.loader
runtime = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runtime)


class FfmpegRuntimeTests(unittest.TestCase):
    TARGET = "x86_64-unknown-linux-gnu"

    def source(self, root: pathlib.Path) -> pathlib.Path:
        source = root / "source"
        source.mkdir()
        for name in runtime.expected_names(self.TARGET):
            (source / name).write_bytes(name.encode("ascii"))
        return source

    def test_package_is_deterministic_and_acquire_checks_every_member(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            source = self.source(root)
            first = root / "ffmpeg-lgpl-8.1.2-x86_64-unknown-linux-gnu.zip"
            second = root / "second.zip"
            record = runtime.package(self.TARGET, source, first)
            self.assertEqual(record["sha256"], runtime.package(self.TARGET, source, second)["sha256"])
            manifest = root / "manifest.json"
            targets = {}
            for target in [
                "aarch64-apple-darwin",
                "aarch64-unknown-linux-gnu",
                "x86_64-pc-windows-msvc",
                "x86_64-unknown-linux-gnu",
            ]:
                targets[target] = record if target == self.TARGET else {
                    "url": "https://github.com/coolplayagent/into-markdown/releases/download/"
                    f"runtime-assets/ffmpeg-lgpl-8.1.2-{target}.zip",
                    "bytes": 1,
                    "sha256": "0" * 64,
                    "members": {
                        member: {"bytes": 1, "sha256": "0" * 64}
                        for member in runtime.expected_names(target)
                    },
                }
            manifest.write_text(
                json.dumps({"schemaVersion": 1, "releaseTag": "runtime-assets", "ffmpegVersion": "8.1.2", "sourceRevision": "a" * 40, "sourceSha256": "464beb5e7bf0c311e68b45ae2f04e9cc2af88851abb4082231742a74d97b524c", "targets": targets}),
                encoding="utf-8",
            )

            def local_acquire(cache, _downloads):
                (cache / "ffmpeg-runtime.zip").write_bytes(first.read_bytes())

            output = root / "output"
            with mock.patch.object(runtime, "acquire_pinned", side_effect=local_acquire):
                runtime.acquire(self.TARGET, output, manifest)
            self.assertEqual(
                {path.name for path in output.iterdir()},
                set(runtime.expected_names(self.TARGET)),
            )

            targets[self.TARGET] = dict(record)
            targets[self.TARGET]["members"] = dict(record["members"])
            member = runtime.expected_names(self.TARGET)[0]
            targets[self.TARGET]["members"][member] = {
                "bytes": len(member),
                "sha256": hashlib.sha256(b"wrong").hexdigest(),
            }
            manifest.write_text(
                json.dumps({"schemaVersion": 1, "releaseTag": "runtime-assets", "ffmpegVersion": "8.1.2", "sourceRevision": "a" * 40, "sourceSha256": "464beb5e7bf0c311e68b45ae2f04e9cc2af88851abb4082231742a74d97b524c", "targets": targets}),
                encoding="utf-8",
            )
            with mock.patch.object(runtime, "acquire_pinned", side_effect=local_acquire), self.assertRaisesRegex(
                runtime.RuntimeAssetError, "differs from authority"
            ):
                runtime.acquire(self.TARGET, root / "rejected", manifest)

    def test_rejects_extra_audit_file(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            source = self.source(root)
            (source / "unexpected.txt").write_text("no", encoding="utf-8")
            with self.assertRaisesRegex(runtime.RuntimeAssetError, "exact reviewed set"):
                runtime.package(self.TARGET, source, root / "runtime.zip")


if __name__ == "__main__":
    unittest.main()

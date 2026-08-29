from __future__ import annotations

import importlib.util
import json
import pathlib
import stat
import struct
import sys
import tempfile
import unittest
import zipfile


PATH = pathlib.Path(__file__).with_name("post_release_e2e.py")
SPEC = importlib.util.spec_from_file_location("post_release_e2e", PATH)
assert SPEC and SPEC.loader
e2e = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = e2e
SPEC.loader.exec_module(e2e)


def elf(machine: int = 62) -> bytes:
    value = bytearray(64)
    value[:7] = b"\x7fELF\x02\x01\x01"
    struct.pack_into("<H", value, 18, machine)
    return bytes(value)


class PostReleaseE2ETests(unittest.TestCase):
    def test_release_url_is_stable_and_rejects_unsafe_identity(self) -> None:
        self.assertEqual(
            e2e.release_asset_url("owner/repository", "0.0.3", "asset.zip"),
            "https://github.com/owner/repository/releases/download/0.0.3/asset.zip",
        )
        for repository, tag in (("owner", "0.0.3"), ("owner/../repo", "0.0.3"), ("owner/repo", "../tag")):
            with self.subTest(repository=repository, tag=tag), self.assertRaises(e2e.E2EError):
                e2e.release_asset_url(repository, tag, "asset.zip")

    def test_core_archive_requires_one_direct_run_binary_and_mode(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            archive_path = root / "into-md-linux-x86_64.zip"
            info = zipfile.ZipInfo("into-md", (2026, 1, 1, 0, 0, 0))
            info.create_system = 3
            info.external_attr = (stat.S_IFREG | 0o755) << 16
            with zipfile.ZipFile(archive_path, "w") as archive:
                archive.writestr(info, elf())
            report = e2e.extract_single_core(archive_path, "linux", root / "into-md")
            self.assertEqual((report["format"], report["architecture"]), ("ELF", "x86_64"))
            with zipfile.ZipFile(archive_path, "a") as archive:
                archive.writestr("NOTICE", "unexpected")
            with self.assertRaisesRegex(e2e.E2EError, "only into-md"):
                e2e.extract_single_core(archive_path, "linux", root / "other")

    def test_speech_identity_and_audit_only_rejection(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            package = root / "speech.imp"
            manifest = {
                "id": "official.media.whisper",
                "signature": {"keyId": "official", "publicKeySha256": "a" * 64},
            }
            with zipfile.ZipFile(package, "w") as archive:
                archive.writestr("plugin.json", json.dumps(manifest))
                archive.writestr("bin/provider", b"provider")
            self.assertEqual(e2e.plugin_identity(package)["signingKeyId"], "official")
            forbidden = root / "forbidden.imp"
            with zipfile.ZipFile(forbidden, "w") as archive:
                archive.writestr("plugin.json", json.dumps(manifest))
                archive.writestr("source/ffmpeg.tar.xz", b"source")
            with self.assertRaisesRegex(e2e.E2EError, "audit-only"):
                e2e.plugin_identity(forbidden)

    def test_acquire_assets_reuses_local_regular_files(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            for asset in (
                e2e.SKILL_ARCHIVE,
                e2e.TARGETS["linux"]["core"],
                e2e.TARGETS["linux"]["speech"],
            ):
                (root / asset).write_bytes(asset.encode())
            records = e2e.acquire_assets(root, "owner/repo", "0.0.3", ["linux"])
            self.assertEqual(len(records), 3)
            self.assertTrue(all(not record["downloaded"] for record in records.values()))
            self.assertTrue(all(len(record["sha256"]) == 64 for record in records.values()))

    def test_dispatch_snapshot_detection_is_scoped_to_the_isolated_temp(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            environment = {"TEMP": str(root)}
            (root / "unrelated").mkdir()
            residual = root / "into-md-plugin-dispatch-deadbeef"
            residual.mkdir()
            self.assertEqual(e2e.dispatch_directories(environment), [residual])
            with self.assertRaisesRegex(e2e.E2EError, "dispatch snapshots"):
                e2e.assert_dispatch_clean(environment, "transcription")

    def test_isolated_environment_routes_unix_and_windows_temp_variables_together(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            environment, _ = e2e._isolated_environment(pathlib.Path(name) / "state", "linux")
            self.assertEqual(environment["TEMP"], environment["TMP"])
            self.assertEqual(environment["TEMP"], environment["TMPDIR"])


if __name__ == "__main__":
    unittest.main()

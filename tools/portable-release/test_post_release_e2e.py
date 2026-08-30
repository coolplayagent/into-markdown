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
from unittest import mock


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


def pe_x86_64() -> bytes:
    value = bytearray(128)
    value[:2] = b"MZ"
    struct.pack_into("<I", value, 0x3C, 64)
    value[64:68] = b"PE\0\0"
    struct.pack_into("<H", value, 68, 0x8664)
    return bytes(value)


def member(name: str, mode: int = 0o644) -> zipfile.ZipInfo:
    info = zipfile.ZipInfo(name, (2026, 1, 1, 0, 0, 0))
    info.create_system = 3
    info.external_attr = (stat.S_IFREG | mode) << 16
    return info


def directory(name: str) -> zipfile.ZipInfo:
    info = zipfile.ZipInfo(name, (2026, 1, 1, 0, 0, 0))
    info.create_system = 3
    info.external_attr = ((stat.S_IFDIR | 0o755) << 16) | 0x10
    return info


def write_core_archive(
    path: pathlib.Path,
    platform: str,
    runtime: bytes | None = None,
    extra: str | None = None,
) -> None:
    executable = e2e.TARGETS[platform]["member"]
    entries = [(executable, pe_x86_64() if platform == "windows" else elf(), 0o644 if platform == "windows" else 0o755)]
    if runtime is not None:
        entries.append((e2e.WINDOWS_PDFIUM_MEMBER, runtime, 0o644))
    entries.extend((name, f"material:{name}".encode(), 0o644) for name in e2e.CORE_MATERIAL_MEMBERS)
    records = [
        {
            "path": name,
            "bytes": len(data),
            "sha256": __import__("hashlib").sha256(data).hexdigest(),
            "mode": f"{mode:04o}",
            "kind": (
                "component"
                if name == e2e.WINDOWS_PDFIUM_MEMBER
                else "license-material"
                if name.startswith("licenses/")
                else "declaration"
                if name in {"LICENSE", "NOTICE"}
                else "generated"
                if name in {"THIRD_PARTY_NOTICES.md", "SBOM.spdx.json", "SOURCES.json"}
                else "project"
            ),
            **(
                {"componentId": "pdfium"}
                if name == e2e.WINDOWS_PDFIUM_MEMBER or name.startswith("licenses/pdfium/")
                else {}
            ),
        }
        for name, data, mode in entries
    ]
    manifest = json.dumps(
        {
            "schemaVersion": 1,
            "target": e2e.TARGETS[platform]["target"],
            "files": records,
        }
    ).encode()
    with zipfile.ZipFile(path, "w") as archive:
        for name, data, mode in entries:
            archive.writestr(member(name, mode), data)
        archive.writestr(member(e2e.CORE_ARCHIVE_MANIFEST), manifest)
        if extra:
            archive.writestr(member(extra), b"unexpected")


def write_skill_archive(path: pathlib.Path, runtime: bytes | None = None) -> None:
    files = {
        "into-markdown/LICENSE": (e2e.ROOT / "LICENSE").read_bytes(),
        "into-markdown/NOTICE": b"notice",
        "into-markdown/SBOM.spdx.json": b"{}",
        "into-markdown/SKILL.md": b"skill",
        "into-markdown/SOURCES.json": b"{}",
        "into-markdown/THIRD_PARTY_NOTICES.md": b"notices",
        "into-markdown/agents/openai.yaml": b"interface: {}",
        "into-markdown/assets/linux-arm64/into-md": elf(183),
        "into-markdown/assets/linux-x86_64/into-md": elf(),
        "into-markdown/assets/windows-x86_64/into-md.exe": pe_x86_64(),
        "into-markdown/references/cli-workflows.md": b"reference",
    }
    if runtime is not None:
        files[e2e.WINDOWS_SKILL_PDFIUM] = runtime
    for relative in e2e.CORE_MATERIAL_MEMBERS:
        if relative == "LICENSE":
            continue
        authority = {
            "licenses/npm/npm-release.spdx.json": e2e.ROOT
            / "third_party/licenses/npm-release.spdx.json",
            "licenses/npm/lucide-ISC-MIT.txt": e2e.ROOT
            / "third_party/licenses/npm/lucide-ISC-MIT.txt",
            "licenses/npm/react-MIT.txt": e2e.ROOT
            / "third_party/licenses/npm/react-MIT.txt",
        }.get(relative)
        files[f"into-markdown/{relative}"] = (
            authority.read_bytes() if authority else f"material:{relative}".encode()
        )
    ordered_names = ["into-markdown/", *sorted(set(e2e.SKILL_DIRECTORIES[1:]) | set(e2e.SKILL_FILES))]
    records = []
    for name in ordered_names:
        if name.endswith("/") or name == e2e.SKILL_MANIFEST or name not in files:
            continue
        data = files[name]
        relative = name.removeprefix("into-markdown/")
        mode = 0o755 if name in {
            "into-markdown/assets/linux-arm64/into-md",
            "into-markdown/assets/linux-x86_64/into-md",
        } else 0o644
        record = {
            "path": relative,
            "bytes": len(data),
            "sha256": __import__("hashlib").sha256(data).hexdigest(),
            "mode": f"{mode:04o}",
            "kind": (
                "component"
                if name == e2e.WINDOWS_SKILL_PDFIUM
                else "license-material"
                if relative.startswith("licenses/")
                else "generated"
                if relative in {"THIRD_PARTY_NOTICES.md", "SBOM.spdx.json", "SOURCES.json"}
                else "declaration"
                if relative in {"LICENSE", "NOTICE"}
                else "executable"
                if relative.startswith("assets/")
                else "skill-source"
            ),
            **(
                {"componentId": "pdfium"}
                if name == e2e.WINDOWS_SKILL_PDFIUM or relative.startswith("licenses/pdfium/")
                else {}
            ),
        }
        records.append(record)
    files[e2e.SKILL_MANIFEST] = json.dumps(
        {"schemaVersion": 1, "files": records}
    ).encode()
    with zipfile.ZipFile(path, "w") as archive:
        for name in ordered_names:
            if name.endswith("/"):
                archive.writestr(directory(name), b"")
            elif name not in files:
                continue
            else:
                mode = 0o755 if name in {
                    "into-markdown/assets/linux-arm64/into-md",
                    "into-markdown/assets/linux-x86_64/into-md",
                } else 0o644
                archive.writestr(member(name, mode), files[name])


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
            write_core_archive(archive_path, "linux")
            report = e2e.extract_single_core(archive_path, "linux", root / "into-md")
            self.assertEqual((report["format"], report["architecture"]), ("ELF", "x86_64"))
            with zipfile.ZipFile(archive_path, "a") as archive:
                archive.writestr("unexpected", "unexpected")
            with self.assertRaisesRegex(e2e.E2EError, "exactly into-md"):
                e2e.extract_single_core(archive_path, "linux", root / "other")

    def test_windows_core_archive_retains_only_authenticated_pdfium(self) -> None:
        runtime = b"pinned-pdfium"
        authority = {
            "library_size": len(runtime),
            "library_sha256": __import__("hashlib").sha256(runtime).hexdigest(),
        }
        with tempfile.TemporaryDirectory() as name, mock.patch.object(
            e2e, "WINDOWS_PDFIUM_AUTHORITY", authority
        ):
            root = pathlib.Path(name)
            archive_path = root / "into-md-windows-x86_64.zip"
            write_core_archive(archive_path, "windows", runtime)
            output = root / "core" / "into-md.exe"
            report = e2e.extract_single_core(archive_path, "windows", output)
            self.assertEqual(
                report["memberCount"], len(e2e.CORE_MATERIAL_MEMBERS) + 3
            )
            self.assertEqual(
                (output.parent / e2e.WINDOWS_PDFIUM_MEMBER).read_bytes(), runtime
            )

            for bad_runtime, extra in ((b"tampered-pdfium", None), (runtime, "unexpected")):
                write_core_archive(archive_path, "windows", bad_runtime, extra)
                with self.subTest(runtime=bad_runtime, extra=extra), self.assertRaises(e2e.E2EError):
                    e2e.extract_single_core(archive_path, "windows", root / "rejected.exe")

            write_core_archive(archive_path, "windows")
            with self.assertRaisesRegex(e2e.E2EError, "pdfium.dll"):
                e2e.extract_single_core(archive_path, "windows", root / "missing.exe")

    def test_windows_skill_retains_authenticated_pdfium_beside_core(self) -> None:
        runtime = b"pinned-pdfium"
        authority = {
            "library_size": len(runtime),
            "library_sha256": __import__("hashlib").sha256(runtime).hexdigest(),
        }
        with tempfile.TemporaryDirectory() as name, mock.patch.object(
            e2e, "WINDOWS_PDFIUM_AUTHORITY", authority
        ):
            root = pathlib.Path(name)
            archive_path = root / e2e.SKILL_ARCHIVE
            write_skill_archive(archive_path, runtime)
            output = root / "skill" / "skill.exe"
            e2e.extract_skill_binary(archive_path, "windows", output)
            self.assertEqual(
                (output.parent / e2e.WINDOWS_PDFIUM_MEMBER).read_bytes(), runtime
            )

            write_skill_archive(archive_path)
            with self.assertRaisesRegex(e2e.E2EError, "exact reviewed archive inventory"):
                e2e.extract_skill_binary(archive_path, "windows", root / "missing.exe")

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

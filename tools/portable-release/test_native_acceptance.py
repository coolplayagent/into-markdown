from __future__ import annotations

import importlib.util
import hashlib
import json
import pathlib
import stat
import struct
import tempfile
import unittest
import zipfile
from unittest import mock


PATH = pathlib.Path(__file__).with_name("native_acceptance.py")
SPEC = importlib.util.spec_from_file_location("portable_native_acceptance", PATH)
assert SPEC and SPEC.loader
acceptance = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(acceptance)


def elf(machine: int) -> bytes:
    value = bytearray(64)
    value[:7] = b"\x7fELF\x02\x01\x01"
    struct.pack_into("<H", value, 18, machine)
    return bytes(value)


class NativeAcceptanceTests(unittest.TestCase):
    def archive(
        self,
        root: pathlib.Path,
        target: str,
        data: bytes,
        mode: int = 0o755,
        runtime: bytes | None = None,
        runtime_mode: int = 0o644,
    ) -> pathlib.Path:
        archive_name, member, _, _ = acceptance.CORE_ARCHIVES[target]
        path = root / "release" / archive_name
        path.parent.mkdir(exist_ok=True)
        entries = [(member, data, mode)]
        if runtime is not None:
            entries.append((acceptance.WINDOWS_PDFIUM_MEMBER, runtime, runtime_mode))
        entries.extend(
            (name, f"material:{name}".encode(), 0o644)
            for name in acceptance.CORE_MATERIAL_MEMBERS
        )
        records = [
            {
                "path": name,
                "bytes": len(contents),
                "sha256": hashlib.sha256(contents).hexdigest(),
                "mode": f"{entry_mode:04o}",
                "kind": (
                    "component"
                    if name == acceptance.WINDOWS_PDFIUM_MEMBER
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
                    if name == acceptance.WINDOWS_PDFIUM_MEMBER
                    or name.startswith("licenses/pdfium/")
                    else {}
                ),
            }
            for name, contents, entry_mode in entries
        ]
        manifest = json.dumps(
            {"schemaVersion": 1, "target": target, "files": records}
        ).encode()
        with zipfile.ZipFile(path, "w") as archive:
            for name, contents, entry_mode in entries:
                info = zipfile.ZipInfo(name, (2026, 1, 1, 0, 0, 0))
                info.create_system = 3
                info.external_attr = (stat.S_IFREG | entry_mode) << 16
                archive.writestr(info, contents)
            info = zipfile.ZipInfo(acceptance.CORE_ARCHIVE_MANIFEST, (2026, 1, 1, 0, 0, 0))
            info.create_system = 3
            info.external_attr = (stat.S_IFREG | 0o644) << 16
            archive.writestr(info, manifest)
        return path

    def test_audit_accepts_exact_linux_core(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            self.archive(root, "x86_64-unknown-linux-gnu", elf(62))
            report, contents, member = acceptance.audit_archive(
                root, "x86_64-unknown-linux-gnu"
            )
            self.assertEqual((report["format"], report["architecture"]), ("ELF", "x86_64"))
            self.assertEqual((contents[member], member), (elf(62), "into-md"))
            self.assertEqual(report["conclusion"], "pass")

    def test_audit_rejects_wrong_architecture(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            self.archive(root, "x86_64-unknown-linux-gnu", elf(183))
            with self.assertRaisesRegex(acceptance.AcceptanceError, "architecture"):
                acceptance.audit_archive(root, "x86_64-unknown-linux-gnu")

    def test_audit_rejects_extra_member_and_wrong_mode(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            path = self.archive(root, "x86_64-unknown-linux-gnu", elf(62), 0o644)
            with self.assertRaisesRegex(acceptance.AcceptanceError, "mode"):
                acceptance.audit_archive(root, "x86_64-unknown-linux-gnu")
            path.unlink()
            self.archive(root, "x86_64-unknown-linux-gnu", elf(62))
            with zipfile.ZipFile(path, "a") as archive:
                archive.writestr("unexpected", "unexpected")
            with self.assertRaisesRegex(acceptance.AcceptanceError, "inventory"):
                acceptance.audit_archive(root, "x86_64-unknown-linux-gnu")

    def test_windows_audit_rejects_missing_tampered_and_link_pdfium(self) -> None:
        target = "x86_64-pc-windows-msvc"
        archive_name, member, _, _ = acceptance.CORE_ARCHIVES[target]
        pe = bytearray(128)
        pe[:2] = b"MZ"
        struct.pack_into("<I", pe, 0x3C, 64)
        pe[64:68] = b"PE\0\0"
        struct.pack_into("<H", pe, 68, 0x8664)
        pinned = b"pinned"
        authority = {
            "version": "test",
            "targets": {
                target: {
                    "library_size": len(pinned),
                    "library_sha256": hashlib.sha256(pinned).hexdigest(),
                }
            },
        }
        with tempfile.TemporaryDirectory() as name, mock.patch.object(
            acceptance, "PDFIUM_MANIFEST", authority
        ):
            root = pathlib.Path(name)
            path = self.archive(root, target, bytes(pe), 0o644)
            with self.assertRaisesRegex(acceptance.AcceptanceError, "inventory"):
                acceptance.audit_archive(root, target)

            self.archive(root, target, bytes(pe), 0o644, b"tampered")
            with self.assertRaisesRegex(acceptance.AcceptanceError, "pinned manifest"):
                acceptance.audit_archive(root, target)

            self.archive(root, target, bytes(pe), 0o644, pinned, 0o777)
            with zipfile.ZipFile(path, "r") as source:
                entries = [(info, source.read(info)) for info in source.infolist()]
            with zipfile.ZipFile(path, "w") as archive:
                for info, contents in entries:
                    if info.filename == acceptance.WINDOWS_PDFIUM_MEMBER:
                        info.external_attr = (stat.S_IFLNK | 0o777) << 16
                    archive.writestr(info, contents)
            with self.assertRaisesRegex(acceptance.AcceptanceError, "regular file"):
                acceptance.audit_archive(root, target)


if __name__ == "__main__":
    unittest.main()

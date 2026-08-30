from __future__ import annotations

import importlib.util
import pathlib
import stat
import struct
import tempfile
import unittest
import zipfile


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
    def archive(self, root: pathlib.Path, target: str, data: bytes, mode: int = 0o755) -> pathlib.Path:
        archive_name, member, _, _ = acceptance.CORE_ARCHIVES[target]
        path = root / "release" / archive_name
        path.parent.mkdir(exist_ok=True)
        info = zipfile.ZipInfo(member, (2026, 1, 1, 0, 0, 0))
        info.create_system = 3
        info.external_attr = (stat.S_IFREG | mode) << 16
        with zipfile.ZipFile(path, "w") as archive:
            archive.writestr(info, data)
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
                archive.writestr("NOTICE", "unexpected")
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
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            path = root / "release" / archive_name
            path.parent.mkdir()
            binary = zipfile.ZipInfo(member)
            binary.create_system = 3
            binary.external_attr = (stat.S_IFREG | 0o644) << 16
            with zipfile.ZipFile(path, "w") as archive:
                archive.writestr(binary, pe)
            with self.assertRaisesRegex(acceptance.AcceptanceError, "inventory"):
                acceptance.audit_archive(root, target)

            runtime = zipfile.ZipInfo(acceptance.WINDOWS_PDFIUM_MEMBER)
            runtime.create_system = 3
            runtime.external_attr = (stat.S_IFREG | 0o644) << 16
            with zipfile.ZipFile(path, "w") as archive:
                archive.writestr(binary, pe)
                archive.writestr(runtime, b"tampered")
            with self.assertRaisesRegex(acceptance.AcceptanceError, "pinned manifest"):
                acceptance.audit_archive(root, target)

            runtime.external_attr = (stat.S_IFLNK | 0o777) << 16
            with zipfile.ZipFile(path, "w") as archive:
                archive.writestr(binary, pe)
                archive.writestr(runtime, b"outside/pdfium.dll")
            with self.assertRaisesRegex(acceptance.AcceptanceError, "regular file"):
                acceptance.audit_archive(root, target)


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Contract tests for the bundled Into Markdown skill."""

from __future__ import annotations

import hashlib
import pathlib
import stat
import struct
import sys
import tempfile
import unittest
import zipfile
from unittest import mock

import skill_release_main

from skill_release import (
    ALLOWED_FILES,
    ARCHIVE_MANIFEST,
    ASSET_SPECS,
    CORE_MATERIAL_RELATIVES,
    FIXED_TIMESTAMP,
    ROOT,
    SKILL_NAME,
    SKILL_SOURCE,
    WINDOWS_PDFIUM_RELATIVE,
    SkillReleaseError,
    core_inputs,
    create_archive,
    materialize,
    validate,
    validate_materialized,
    verify_release,
)


def pe(machine: int = 0x8664, marker: bytes = b"") -> bytes:
    result = bytearray(256)
    result[:2] = b"MZ"
    struct.pack_into("<I", result, 0x3C, 0x80)
    result[0x80:0x84] = b"PE\0\0"
    struct.pack_into("<H", result, 0x84, machine)
    result.extend(marker)
    return bytes(result)


def elf(machine: int, marker: bytes = b"") -> bytes:
    result = bytearray(64)
    result[:7] = b"\x7fELF\x02\x01\x01"
    struct.pack_into("<HH", result, 16, 3, machine)
    result.extend(marker)
    return bytes(result)


def write_cores(root: pathlib.Path) -> dict[pathlib.PurePosixPath, pathlib.Path]:
    windows = root / "windows-core.exe"
    pdfium = root / "pdfium.dll"
    linux_x86_64 = root / "linux-x86_64-core"
    linux_arm64 = root / "linux-arm64-core"
    windows.write_bytes(pe(marker=b"windows"))
    pdfium.write_bytes(b"test-pdfium")
    linux_x86_64.write_bytes(elf(62, b"linux-x86_64"))
    linux_arm64.write_bytes(elf(183, b"linux-arm64"))
    (root / "LICENSE").write_bytes((ROOT / "LICENSE").read_bytes())
    static_materials = {
        pathlib.PurePosixPath("licenses/npm/npm-release.spdx.json"): ROOT
        / "third_party/licenses/npm-release.spdx.json",
        pathlib.PurePosixPath("licenses/npm/lucide-ISC-MIT.txt"): ROOT
        / "third_party/licenses/npm/lucide-ISC-MIT.txt",
        pathlib.PurePosixPath("licenses/npm/react-MIT.txt"): ROOT
        / "third_party/licenses/npm/react-MIT.txt",
    }
    for relative in CORE_MATERIAL_RELATIVES:
        path = root / pathlib.Path(relative.as_posix())
        path.parent.mkdir(parents=True, exist_ok=True)
        source = static_materials.get(relative)
        path.write_bytes(source.read_bytes() if source else f"material:{relative}".encode())
    return core_inputs(windows, pdfium, linux_x86_64, linux_arm64)


def rewrite_entry(
    source: pathlib.Path,
    destination: pathlib.Path,
    entry_name: str,
    *,
    contents: bytes | None = None,
    mode: int | None = None,
) -> None:
    with zipfile.ZipFile(source) as input_archive, zipfile.ZipFile(destination, "x") as output_archive:
        for original in input_archive.infolist():
            info = zipfile.ZipInfo(original.filename, original.date_time)
            info.create_system = original.create_system
            info.compress_type = original.compress_type
            info.external_attr = original.external_attr
            data = input_archive.read(original)
            if original.filename == entry_name:
                data = data if contents is None else contents
                if mode is not None:
                    info.external_attr = (stat.S_IFREG | mode) << 16
            output_archive.writestr(info, data, compress_type=info.compress_type, compresslevel=9)


class SkillReleaseTests(unittest.TestCase):
    def setUp(self) -> None:
        self.pdfium_authority = mock.patch(
            "skill_release.WINDOWS_PDFIUM_AUTHORITY",
            {
                "library_size": len(b"test-pdfium"),
                "library_sha256": hashlib.sha256(b"test-pdfium").hexdigest(),
            },
        )
        self.pdfium_authority.start()

    def tearDown(self) -> None:
        self.pdfium_authority.stop()

    def test_canonical_skill_routes_only_to_bundled_assets(self) -> None:
        paths = validate()
        self.assertEqual(
            [path.relative_to(SKILL_SOURCE).as_posix() for path in paths],
            [path.as_posix() for path in ALLOWED_FILES],
        )
        text = (SKILL_SOURCE / "SKILL.md").read_text(encoding="utf-8")
        self.assertIn("Do not search `PATH`", text)
        for spec in ASSET_SPECS:
            self.assertIn(spec.relative.as_posix(), text)

    def test_archive_is_deterministic_and_has_exact_assets_and_modes(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            cores = write_cores(root)
            first = create_archive(root / "first.zip", cores)
            second = create_archive(root / "second.zip", cores)
            self.assertEqual(first.read_bytes(), second.read_bytes())
            self.assertFalse(first.with_name(first.name + ".sha256").exists())
            verify_release(first)
            with zipfile.ZipFile(first) as archive:
                infos = archive.infolist()
                names = [info.filename for info in infos]
                self.assertEqual(names, [f"{SKILL_NAME}/", *sorted(names[1:])])
                self.assertEqual(len(names), len(set(names)))
                self.assertTrue(all(info.date_time == FIXED_TIMESTAMP for info in infos))
                for spec in ASSET_SPECS:
                    name_in_zip = f"{SKILL_NAME}/{spec.relative.as_posix()}"
                    info = archive.getinfo(name_in_zip)
                    self.assertEqual((info.external_attr >> 16) & 0o177777, stat.S_IFREG | spec.mode)
                    self.assertEqual(archive.read(info), cores[spec.relative].read_bytes())
                runtime = archive.getinfo(
                    f"{SKILL_NAME}/{WINDOWS_PDFIUM_RELATIVE.as_posix()}"
                )
                self.assertEqual(
                    (runtime.external_attr >> 16) & 0o177777,
                    stat.S_IFREG | 0o644,
                )
                self.assertEqual(
                    archive.read(runtime), cores[WINDOWS_PDFIUM_RELATIVE].read_bytes()
                )
                manifest = archive.getinfo(f"{SKILL_NAME}/{ARCHIVE_MANIFEST.as_posix()}")
                self.assertIn(b'"schemaVersion": 1', archive.read(manifest))
                for relative in CORE_MATERIAL_RELATIVES:
                    self.assertEqual(
                        archive.read(f"{SKILL_NAME}/{relative.as_posix()}"),
                        cores[relative].read_bytes(),
                    )

    def test_verify_is_standalone_and_rejects_sidecars_or_extra_entries(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            archive_path = create_archive(root / "skill.zip", write_cores(root))
            verify_release(archive_path)
            sidecar = archive_path.with_name(archive_path.name + ".sha256")
            sidecar.write_text("forbidden\n", encoding="ascii")
            with self.assertRaisesRegex(SkillReleaseError, "must not have"):
                verify_release(archive_path)
            sidecar.unlink()
            with zipfile.ZipFile(archive_path, "a") as archive:
                archive.writestr(f"{SKILL_NAME}/README.md", b"unexpected")
            with self.assertRaisesRegex(SkillReleaseError, "exact reviewed entries"):
                verify_release(archive_path)

    def test_wrong_binary_format_and_architecture_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            cores = write_cores(root)
            cores[ASSET_SPECS[0].relative].write_bytes(elf(62))
            with self.assertRaisesRegex(SkillReleaseError, "not a PE"):
                create_archive(root / "wrong-format.zip", cores)

            cores = write_cores(root)
            cores[ASSET_SPECS[2].relative].write_bytes(elf(62))
            with self.assertRaisesRegex(SkillReleaseError, "wrong ELF architecture"):
                create_archive(root / "wrong-architecture.zip", cores)

    def test_standalone_verify_rejects_asset_architecture_and_permissions(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            valid = create_archive(root / "valid.zip", write_cores(root))
            arm_name = f"{SKILL_NAME}/{ASSET_SPECS[2].relative.as_posix()}"
            rewrite_entry(valid, root / "wrong-architecture.zip", arm_name, contents=elf(62))
            with self.assertRaisesRegex(SkillReleaseError, "wrong ELF architecture"):
                verify_release(root / "wrong-architecture.zip")

            x86_name = f"{SKILL_NAME}/{ASSET_SPECS[1].relative.as_posix()}"
            rewrite_entry(valid, root / "wrong-mode.zip", x86_name, mode=0o644)
            with self.assertRaisesRegex(SkillReleaseError, "permissions are invalid"):
                verify_release(root / "wrong-mode.zip")

    def test_release_materials_are_exact_and_manifest_bound(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            valid = create_archive(root / "valid.zip", write_cores(root))
            notice = f"{SKILL_NAME}/NOTICE"
            rewrite_entry(valid, root / "tampered.zip", notice, contents=b"tampered")
            with self.assertRaisesRegex(SkillReleaseError, "bidirectional projection"):
                verify_release(root / "tampered.zip")

            cores = write_cores(root)
            cores.pop(pathlib.PurePosixPath("licenses/pdfium/licenses/zlib.txt"))
            with self.assertRaisesRegex(SkillReleaseError, "exact release materials"):
                create_archive(root / "missing.zip", cores)

    def test_materialized_canonical_copy_remains_instruction_only(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            destination = pathlib.Path(name) / SKILL_NAME
            materialize(destination)
            validate_materialized(destination)
            self.assertFalse((destination / "assets").exists())
            for relative in ALLOWED_FILES:
                self.assertEqual((destination / relative).read_bytes(), (SKILL_SOURCE / relative).read_bytes())

    def test_unexpected_canonical_files_and_symbolic_links_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            source = pathlib.Path(name) / SKILL_NAME
            materialize(source)
            (source / "README.md").write_text("unexpected", encoding="utf-8")
            with self.assertRaisesRegex(SkillReleaseError, "exact reviewed file set"):
                validate(source)
            (source / "README.md").unlink()
            try:
                (source / "references/linked.md").symlink_to(source / "SKILL.md")
            except OSError:
                self.skipTest("symbolic links are not available to this test user")
            with self.assertRaisesRegex(SkillReleaseError, "symbolic link"):
                validate(source)

    def test_cli_build_requires_three_cores_pdfium_and_verify_needs_only_archive(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            cores = write_cores(root)
            archive = root / "skill.zip"
            command = [
                "skill-release",
                "build",
                "--archive",
                str(archive),
                "--windows-x86-64-core",
                str(cores[ASSET_SPECS[0].relative]),
                "--windows-x86-64-pdfium",
                str(cores[WINDOWS_PDFIUM_RELATIVE]),
                "--linux-x86-64-core",
                str(cores[ASSET_SPECS[1].relative]),
                "--linux-arm64-core",
                str(cores[ASSET_SPECS[2].relative]),
            ]
            with mock.patch.object(sys, "argv", command):
                skill_release_main.main()
            with mock.patch.object(
                sys,
                "argv",
                ["skill-release", "verify", "--archive", str(archive)],
            ):
                skill_release_main.main()


if __name__ == "__main__":
    unittest.main()

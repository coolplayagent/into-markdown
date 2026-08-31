#!/usr/bin/env python3
"""Contract tests for the bundled Into Markdown skill."""

from __future__ import annotations

import hashlib
import json
import pathlib
import stat
import struct
import sys
import tempfile
import tomllib
import unittest
import zipfile
from unittest import mock

import skill_release_main

from skill_release import (
    ALLOWED_FILES,
    ARCHIVE_MANIFEST,
    ASSET_SPECS,
    AUTHORITY_MATERIALS,
    AUTHORITY_TARGET,
    CORE_MATERIAL_RELATIVES,
    FIXED_TIMESTAMP,
    ROOT,
    SKILL_NAME,
    SKILL_SOURCE,
    WINDOWS_PDFIUM_RELATIVE,
    TARGET_LAYOUTS,
    evidence_relative,
    SkillReleaseError,
    core_inputs,
    create_archive as create_release_archive,
    materialize,
    validate,
    validate_materialized,
    verify_release as verify_release_archive,
)
from core_archive import write_authority


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
    windows = root / "windows" / "windows-core.exe"
    pdfium = root / "windows" / "pdfium.dll"
    linux_x86_64 = root / "linux-x86_64" / "linux-x86_64-core"
    linux_arm64 = root / "linux-arm64" / "linux-arm64-core"
    for path in (windows, pdfium, linux_x86_64, linux_arm64):
        path.parent.mkdir(parents=True, exist_ok=True)
    windows.write_bytes(pe(marker=b"windows"))
    pdfium.write_bytes(b"test-pdfium")
    linux_x86_64.write_bytes(elf(62, b"linux-x86_64"))
    linux_arm64.write_bytes(elf(183, b"linux-arm64"))
    static_materials = {
        pathlib.PurePosixPath("licenses/npm/npm-release.spdx.json"): ROOT
        / "third_party/licenses/npm-release.spdx.json",
        pathlib.PurePosixPath("licenses/npm/lucide-ISC-MIT.txt"): ROOT
        / "third_party/licenses/npm/lucide-ISC-MIT.txt",
        pathlib.PurePosixPath("licenses/npm/react-MIT.txt"): ROOT
        / "third_party/licenses/npm/react-MIT.txt",
    }
    for target, base in (
        ("x86_64-pc-windows-msvc", windows.parent),
        ("x86_64-unknown-linux-gnu", linux_x86_64.parent),
        ("aarch64-unknown-linux-gnu", linux_arm64.parent),
    ):
        (base / "LICENSE").write_bytes((ROOT / "LICENSE").read_bytes())
        for relative in CORE_MATERIAL_RELATIVES:
            path = base / pathlib.Path(relative.as_posix())
            path.parent.mkdir(parents=True, exist_ok=True)
            source = static_materials.get(relative)
            if source:
                contents = source.read_bytes()
            elif relative.as_posix() == "SBOM.spdx.json":
                contents = json.dumps(
                    {
                        "SPDXID": "SPDXRef-DOCUMENT",
                        "spdxVersion": "SPDX-2.3",
                        "dataLicense": "CC0-1.0",
                        "creationInfo": {},
                        "packages": [
                            {"name": f"fixture-{target}"},
                            {"name": "cargo:whisper-rs-sys@0.15.0"},
                            {"name": "cargo:whisper-rs@0.16.0"},
                        ],
                    }
                ).encode()
            elif relative.as_posix() == "SOURCES.json":
                contents = json.dumps(
                    {
                        "schema_version": 1,
                        "target": target,
                        "artifact": "into-markdown-core",
                        "version": "0.0.3",
                        "source_revision": "fixture",
                        "components": [
                            {"id": f"fixture-{target}"},
                            {"id": "cargo:whisper-rs-sys@0.15.0"},
                            {"id": "cargo:whisper-rs@0.16.0"},
                        ],
                    }
                ).encode()
            else:
                contents = f"material:{target}:{relative}".encode()
            path.write_bytes(contents)
    return core_inputs(windows, pdfium, linux_x86_64, linux_arm64)


def write_test_authorities(
    root: pathlib.Path,
    cores: dict[pathlib.PurePosixPath, pathlib.Path],
) -> dict[str, pathlib.Path]:
    result = {}
    for target, _asset in TARGET_LAYOUTS:
        authority = root / f"material-authority-{target}.json"
        materials = {
            member: cores[evidence_relative(target, member)]
            for member in AUTHORITY_MATERIALS
        }
        if not authority.exists():
            write_authority(authority, materials, target, AUTHORITY_MATERIALS)
        result[target] = authority
    return result


def create_archive(
    destination: pathlib.Path,
    cores: dict[pathlib.PurePosixPath, pathlib.Path],
) -> pathlib.Path:
    authority = destination.parent / "skill-authority.json"
    output = authority if not authority.exists() else destination.with_suffix(".authority.json")
    result = create_release_archive(
        destination, cores, write_test_authorities(destination.parent, cores), output
    )
    if output != authority:
        if output.read_bytes() != authority.read_bytes():
            raise AssertionError("deterministic builds produced different Skill authority")
        output.unlink()
    return result


def verify_release(archive: pathlib.Path) -> None:
    verify_release_archive(archive, archive.parent / "skill-authority.json")


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


def replace_material_and_recompute_manifest(
    source: pathlib.Path,
    destination: pathlib.Path,
    relative: str,
    contents: bytes,
) -> None:
    entry_name = f"{SKILL_NAME}/{relative}"
    with zipfile.ZipFile(source) as input_archive:
        entries = [(info, input_archive.read(info)) for info in input_archive.infolist()]
    manifest_name = f"{SKILL_NAME}/{ARCHIVE_MANIFEST.as_posix()}"
    rewritten = []
    for info, data in entries:
        if info.filename == entry_name:
            data = contents
        rewritten.append((info, data))
    manifest = json.loads(next(data for info, data in rewritten if info.filename == manifest_name))
    for record in manifest["files"]:
        if record["path"] == relative:
            record["bytes"] = len(contents)
            record["sha256"] = hashlib.sha256(contents).hexdigest()
    manifest_bytes = (
        json.dumps(manifest, ensure_ascii=False, indent=2, sort_keys=True).encode()
        + b"\n"
    )
    with zipfile.ZipFile(destination, "x") as output_archive:
        for original, data in rewritten:
            info = zipfile.ZipInfo(original.filename, original.date_time)
            info.create_system = original.create_system
            info.compress_type = original.compress_type
            info.external_attr = original.external_attr
            output_archive.writestr(
                info,
                manifest_bytes if original.filename == manifest_name else data,
                compress_type=info.compress_type,
                compresslevel=9,
            )


class SkillReleaseTests(unittest.TestCase):
    def setUp(self) -> None:
        self.product_version = tomllib.loads(
            (ROOT / "Cargo.toml").read_text(encoding="utf-8")
        )["workspace"]["package"]["version"]
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
        self.assertIn(f'metadata:\n  version: "{self.product_version}"\n', text)
        self.assertIn("Do not search `PATH`", text)
        for spec in ASSET_SPECS:
            self.assertIn(spec.relative.as_posix(), text)

    def test_invalid_version_frontmatter_fails_closed(self) -> None:
        text = (SKILL_SOURCE / "SKILL.md").read_text(encoding="utf-8")
        _opening, header, body = text.split("---", 2)
        fields, version_field = header.split("metadata:\n", 1)
        metadata_block = "metadata:\n" + version_field
        invalid_metadata = {
            "missing metadata": "",
            "missing version": "metadata:\n",
            "empty value": "metadata:\n  version:\n",
            "empty string": 'metadata:\n  version: ""\n',
            "blank string": 'metadata:\n  version: " "\n',
            "null": "metadata:\n  version: null\n",
            "boolean": "metadata:\n  version: true\n",
            "number": "metadata:\n  version: 4\n",
            "list": 'metadata:\n  version: ["0.0.4"]\n',
            "mapping": 'metadata:\n  version: {"value": "0.0.4"}\n',
            "unquoted": "metadata:\n  version: 0.0.4\n",
            "unclosed string": 'metadata:\n  version: "0.0.4\n',
            "top-level version": 'version: "0.0.4"\n',
            "unindented version": 'metadata:\nversion: "0.0.4"\n',
            "wrong indentation": 'metadata:\n    version: "0.0.4"\n',
            "scalar metadata": 'metadata: "0.0.4"\n',
            "flow metadata": 'metadata: {version: "0.0.4"}\n',
            "duplicate metadata": metadata_block * 2,
            "duplicate version": metadata_block + version_field,
            "duplicate name": metadata_block + "name: into-markdown\n",
            "duplicate description": metadata_block + "description: duplicate\n",
            "extra metadata field": metadata_block + '  author: "extra"\n',
            "version drift": 'metadata:\n  version: "999.999.999"\n',
        }
        with tempfile.TemporaryDirectory() as name:
            source = materialize(pathlib.Path(name) / SKILL_NAME)
            for label, metadata in invalid_metadata.items():
                with self.subTest(case=label):
                    (source / "SKILL.md").write_text(
                        "---" + fields + metadata + "---" + body, encoding="utf-8"
                    )
                    with self.assertRaisesRegex(SkillReleaseError, "SKILL.md"):
                        validate(source)

    def test_product_version_bump_requires_matching_skill_version(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            source = materialize(root / SKILL_NAME)
            (root / "LICENSE").write_bytes((ROOT / "LICENSE").read_bytes())
            cargo = root / "Cargo.toml"
            text = (source / "SKILL.md").read_text(encoding="utf-8")
            # Exercise the actual manifest reader, including prerelease/build metadata.
            with mock.patch("skill_release.ROOT", root):
                for version in ("999.999.999", "999.999.999-rc.1+build.2"):
                    with self.subTest(version=version):
                        cargo.write_text(
                            f'[workspace.package]\nversion = "{version}"\n', encoding="utf-8"
                        )
                        (source / "SKILL.md").write_text(text, encoding="utf-8")
                        with self.assertRaisesRegex(SkillReleaseError, "differs from.*workspace.package.version"):
                            validate(source)
                        (source / "SKILL.md").write_text(
                            text.replace(f'  version: "{self.product_version}"', f'  version: "{version}"'),
                            encoding="utf-8",
                        )
                        validate(source)

    def test_unavailable_or_invalid_product_version_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            source = materialize(root / SKILL_NAME)
            with mock.patch("skill_release.ROOT", root):
                with self.assertRaisesRegex(SkillReleaseError, "Cargo.toml workspace.package.version"):
                    validate(source)
                for manifest in (
                    "invalid TOML",
                    "[workspace.package]\n",
                    "[workspace.package]\nversion = 4\n",
                    '[workspace.package]\nversion = ""\n',
                ):
                    with self.subTest(manifest=manifest):
                        (root / "Cargo.toml").write_text(manifest, encoding="utf-8")
                        with self.assertRaisesRegex(SkillReleaseError, "Cargo.toml workspace.package.version"):
                            validate(source)

    def test_archive_is_deterministic_and_has_exact_assets_and_modes(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            cores = write_cores(root)
            first = create_archive(root / "first.zip", cores)
            authority_bytes = (root / "skill-authority.json").read_bytes()
            second = create_archive(root / "second.zip", cores)
            self.assertEqual(first.read_bytes(), second.read_bytes())
            self.assertEqual(authority_bytes, (root / "skill-authority.json").read_bytes())
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
                skill_bytes = (SKILL_SOURCE / "SKILL.md").read_bytes()
                self.assertEqual(archive.read(f"{SKILL_NAME}/SKILL.md"), skill_bytes)
                skill_record = next(
                    record for record in json.loads(archive.read(manifest))["files"]
                    if record["path"] == "SKILL.md"
                )
                self.assertEqual(skill_record["bytes"], len(skill_bytes))
                self.assertEqual(skill_record["sha256"], hashlib.sha256(skill_bytes).hexdigest())
                for target, _asset in TARGET_LAYOUTS:
                    for member in AUTHORITY_MATERIALS:
                        relative = evidence_relative(target, member)
                        self.assertEqual(
                            archive.read(f"{SKILL_NAME}/{relative.as_posix()}"),
                            cores[relative].read_bytes(),
                        )
                self.assertFalse(any("whisper" in name.lower() or "ffmpeg" in name.lower() for name in names))

    def test_changed_skill_version_fails_even_with_recomputed_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            valid = create_archive(root / "valid.zip", write_cores(root))
            contents = (SKILL_SOURCE / "SKILL.md").read_bytes().replace(
                f'  version: "{self.product_version}"'.encode(), b'  version: "999.999.999"'
            )
            rewrite_entry(valid, root / "tampered.zip", f"{SKILL_NAME}/SKILL.md", contents=contents)
            replace_material_and_recompute_manifest(
                valid, root / "recomputed.zip", "SKILL.md", contents
            )
            for archive in (root / "tampered.zip", root / "recomputed.zip"):
                with self.subTest(archive=archive.name), self.assertRaisesRegex(
                    SkillReleaseError, "instruction metadata or bytes"
                ):
                    verify_release(archive)

    def test_three_target_materials_and_matching_binaries_are_authority_bound(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            valid = create_archive(root / "valid.zip", write_cores(root))
            for target, _asset in TARGET_LAYOUTS:
                relative = evidence_relative(target, "SOURCES.json").as_posix()
                replaced = root / f"replaced-{target}.zip"
                replace_material_and_recompute_manifest(valid, replaced, relative, b'{"target":"attacker"}')
                with self.subTest(target=target), self.assertRaisesRegex(
                    SkillReleaseError, "differs from authority"
                ):
                    verify_release(replaced)

            asset = ASSET_SPECS[1].relative.as_posix()
            replaced_binary = root / "replaced-binary.zip"
            replace_material_and_recompute_manifest(
                valid, replaced_binary, asset, elf(62, b"other-valid-core")
            )
            with self.assertRaisesRegex(SkillReleaseError, "differs from authority"):
                verify_release(replaced_binary)

    def test_optional_speech_components_are_rejected_by_structured_identity(self) -> None:
        cases = (
            ("SOURCES.json", "components", {"id": "ffmpeg"}),
            ("SBOM.spdx.json", "packages", {"name": "whisper-small"}),
        )
        for member, collection, component in cases:
            with self.subTest(member=member), tempfile.TemporaryDirectory() as name:
                root = pathlib.Path(name)
                cores = write_cores(root)
                relative = evidence_relative("x86_64-pc-windows-msvc", member)
                structured = json.loads(cores[relative].read_text(encoding="utf-8"))
                structured[collection].append(component)
                cores[relative].write_text(json.dumps(structured), encoding="utf-8")
                with self.assertRaisesRegex(SkillReleaseError, "optional speech components"):
                    create_archive(root / "skill.zip", cores)

    def test_skill_authority_rejects_namespace_missing_duplicate_and_unsorted_targets(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            valid = create_archive(root / "valid.zip", write_cores(root))
            original = json.loads((root / "skill-authority.json").read_text(encoding="utf-8"))
            variants = {}
            namespace = json.loads(json.dumps(original))
            namespace["namespace"] = "attacker"
            variants["namespace"] = namespace
            missing = json.loads(json.dumps(original))
            missing["targets"].pop()
            variants["missing"] = missing
            duplicate = json.loads(json.dumps(original))
            duplicate["targets"][1] = duplicate["targets"][0]
            variants["duplicate"] = duplicate
            unsorted = json.loads(json.dumps(original))
            unsorted["targets"].reverse()
            variants["unsorted"] = unsorted
            for case, value in variants.items():
                path = root / f"authority-{case}.json"
                path.write_text(json.dumps(value), encoding="utf-8")
                with self.subTest(case=case), self.assertRaises(SkillReleaseError):
                    verify_release_archive(valid, path)

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
            notice_relative = evidence_relative(AUTHORITY_TARGET, "NOTICE").as_posix()
            notice = f"{SKILL_NAME}/{notice_relative}"
            rewrite_entry(valid, root / "tampered.zip", notice, contents=b"tampered")
            with self.assertRaisesRegex(SkillReleaseError, "bidirectional projection"):
                verify_release(root / "tampered.zip")

            replace_material_and_recompute_manifest(
                valid,
                root / "recomputed-manifest.zip",
                notice_relative,
                b"attacker-controlled notice",
            )
            with self.assertRaisesRegex(
                SkillReleaseError, "differs from authority"
            ):
                verify_release(root / "recomputed-manifest.zip")

            cores = write_cores(root)
            authorities = write_test_authorities(root, cores)
            cores.pop(evidence_relative(AUTHORITY_TARGET, "licenses/pdfium/licenses/zlib.txt"))
            with self.assertRaisesRegex(SkillReleaseError, "exact release materials"):
                create_release_archive(
                    root / "missing.zip",
                    cores,
                    authorities,
                    root / "missing-authority.json",
                )

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

    def test_cli_build_and_verify_require_independent_material_authority(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            cores = write_cores(root)
            archive = root / "skill.zip"
            authorities = write_test_authorities(root, cores)
            skill_authority = root / "skill-authority.json"
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
                "--windows-material-authority",
                str(authorities["x86_64-pc-windows-msvc"]),
                "--linux-x86-64-material-authority",
                str(authorities["x86_64-unknown-linux-gnu"]),
                "--linux-arm64-material-authority",
                str(authorities["aarch64-unknown-linux-gnu"]),
                "--authority-output",
                str(skill_authority),
            ]
            with mock.patch.object(sys, "argv", command):
                skill_release_main.main()
            with mock.patch.object(
                sys,
                "argv",
                [
                    "skill-release",
                    "verify",
                    "--archive",
                    str(archive),
                    "--skill-authority",
                    str(skill_authority),
                ],
            ):
                skill_release_main.main()


if __name__ == "__main__":
    unittest.main()

from __future__ import annotations

import importlib.util
import base64
import hashlib
import io
import json
import pathlib
import stat
import struct
import tarfile
import tempfile
import threading
import unittest
import warnings
import zipfile
from unittest import mock


PATH = pathlib.Path(__file__).with_name("assemble.py")
SPEC = importlib.util.spec_from_file_location("portable_release_assemble", PATH)
assert SPEC and SPEC.loader
assemble = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(assemble)


def pe_x86_64() -> bytes:
    value = bytearray(128)
    value[:2] = b"MZ"
    struct.pack_into("<I", value, 0x3C, 64)
    value[64:68] = b"PE\0\0"
    struct.pack_into("<H", value, 68, 0x8664)
    return bytes(value)


def core_materials(root: pathlib.Path) -> dict[str, pathlib.Path]:
    static = {
        "LICENSE": assemble.ROOT / "LICENSE",
        "licenses/npm/npm-release.spdx.json": assemble.ROOT
        / "third_party/licenses/npm-release.spdx.json",
        "licenses/npm/lucide-ISC-MIT.txt": assemble.ROOT
        / "third_party/licenses/npm/lucide-ISC-MIT.txt",
        "licenses/npm/react-MIT.txt": assemble.ROOT
        / "third_party/licenses/npm/react-MIT.txt",
        "licenses/diagram-design-MIT.txt": assemble.ROOT / "third_party/licenses/diagram-design-MIT.txt",
    }
    result = {}
    for name in assemble.CORE_MATERIAL_MEMBERS:
        if name in static:
            result[name] = static[name]
            continue
        path = root / "materials" / pathlib.PurePosixPath(name)
        path.parent.mkdir(parents=True, exist_ok=True)
        if name == "SBOM.spdx.json":
            contents = json.dumps(
                {
                    "SPDXID": "SPDXRef-DOCUMENT",
                    "spdxVersion": "SPDX-2.3",
                    "dataLicense": "CC0-1.0",
                    "creationInfo": {},
                    "packages": [{"name": "fixture"}],
                }
            ).encode()
        elif name == "SOURCES.json":
            contents = json.dumps(
                {
                    "schema_version": 1,
                    "target": "fixture",
                    "artifact": "into-markdown-core",
                    "version": "0.0.3",
                    "source_revision": "fixture",
                    "components": [{"id": "fixture"}],
                }
            ).encode()
        else:
            contents = f"reviewed material: {name}\n".encode()
        path.write_bytes(contents)
        result[name] = path
    return result


def core_authority(materials: dict[str, pathlib.Path], target: str) -> dict:
    return {
        "schemaVersion": 1,
        "target": target,
        "files": [
            {
                "path": name,
                "bytes": materials[name].stat().st_size,
                "sha256": hashlib.sha256(materials[name].read_bytes()).hexdigest(),
            }
            for name in assemble.CORE_MATERIAL_MEMBERS
        ],
    }


class PortableReleaseTests(unittest.TestCase):
    TARGET = "x86_64-unknown-linux-gnu"

    def test_core_materials_preserve_the_exact_pdfium_license_inventory(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            pdfium = root / "pdfium.tar.gz"
            with tarfile.open(pdfium, "w:gz") as archive:
                for member_name in assemble.PDFIUM_LICENSE_FILES:
                    data = f"upstream:{member_name}".encode()
                    info = tarfile.TarInfo(member_name)
                    info.size = len(data)
                    archive.addfile(info, io.BytesIO(data))
            evidence = root / "evidence"
            evidence.mkdir()
            for evidence_name in (
                "NOTICE",
                "THIRD_PARTY_NOTICES.md",
                "SBOM.spdx.json",
                "SOURCES.json",
            ):
                contents = (
                    json.dumps(
                        {
                            "SPDXID": "SPDXRef-DOCUMENT",
                            "spdxVersion": "SPDX-2.3",
                            "dataLicense": "CC0-1.0",
                            "creationInfo": {},
                            "packages": [{"name": "fixture"}],
                        }
                    )
                    if evidence_name == "SBOM.spdx.json"
                    else json.dumps(
                        {
                            "schema_version": 1,
                            "target": "fixture",
                            "artifact": "into-markdown-core",
                            "version": "0.0.3",
                            "source_revision": "fixture",
                            "components": [{"id": "fixture"}],
                        }
                    )
                    if evidence_name == "SOURCES.json"
                    else evidence_name
                )
                (evidence / evidence_name).write_text(contents, encoding="utf-8")
            materials = assemble.stage_core_archive_materials(
                pdfium, evidence, root / "materials"
            )
            self.assertEqual(tuple(materials), assemble.CORE_MATERIAL_MEMBERS)
            for member_name in assemble.PDFIUM_LICENSE_FILES:
                self.assertEqual(
                    materials[f"licenses/pdfium/{member_name}"].read_bytes(),
                    f"upstream:{member_name}".encode(),
                )

            incomplete = root / "incomplete.tar.gz"
            with tarfile.open(incomplete, "w:gz") as archive:
                data = b"license"
                info = tarfile.TarInfo("LICENSE")
                info.size = len(data)
                archive.addfile(info, io.BytesIO(data))
            with self.assertRaisesRegex(assemble.PortableReleaseError, "incomplete"):
                assemble.stage_core_archive_materials(
                    incomplete, evidence, root / "incomplete-materials"
                )

    class ConcurrentRelease:
        def __init__(self, fail_build: bool = False, fail_acquire: bool = False):
            self.barrier = threading.Barrier(2)
            self.fail_build = fail_build
            self.fail_acquire = fail_acquire
            self.downloads = {"model": {"sha256": "0" * 64}}

        def downloads_for(self, _config):
            return self.downloads

        def build(self, _target, output):
            self.barrier.wait(timeout=2)
            if self.fail_build:
                raise RuntimeError("build failed")
            return output / "release"

        def acquire(self, cache, downloads):
            self.barrier.wait(timeout=2)
            if self.fail_acquire:
                raise RuntimeError("acquire failed")
            if downloads != self.downloads:
                raise RuntimeError("download authority changed")
            cache.mkdir(parents=True)

    def write_speech_package(
        self,
        path: pathlib.Path,
        *,
        extra: tuple[str, bytes] | None = None,
        entry_mode: dict[str, int] | None = None,
        corrupt_signature: bool = False,
    ) -> None:
        elf = bytearray(64)
        elf[:6] = b"\x7fELF\x02\x01"
        struct.pack_into("<H", elf, 18, 62)
        runtime = {
            "bin/into-md-media-provider": b"provider",
            "ffmpeg/ffmpeg": b"ffmpeg",
            **{name: bytes(elf) for name in assemble.GGML_RUNTIME_FILES[self.TARGET]},
        }
        if extra is not None:
            runtime[extra[0]] = extra[1]
        runtime_records = [
            {
                "path": name,
                "bytes": len(contents),
                "sha256": hashlib.sha256(contents).hexdigest(),
                "executable": name in {"bin/into-md-media-provider", "ffmpeg/ffmpeg"},
            }
            for name, contents in sorted(runtime.items())
        ]
        provider = {
            "id": "official.media.whisper",
            "version": "0.0.3",
            "targets": [
                {
                    "triple": self.TARGET,
                    "entrypoint": "bin/into-md-media-provider",
                    "files": runtime_records,
                }
            ],
        }
        provider_bytes = json.dumps(provider, separators=(",", ":")).encode()
        package_files = sorted(
            [
                *runtime_records,
                {
                    "path": "provider.json",
                    "bytes": len(provider_bytes),
                    "sha256": hashlib.sha256(provider_bytes).hexdigest(),
                    "executable": False,
                },
            ],
            key=lambda item: item["path"],
        )
        public_key = b"p" * 32
        manifest = {
            "schemaVersion": 1,
            "id": "official.media.whisper",
            "version": "0.0.3",
            "protocol": "process-v1",
            "supportedTargets": [self.TARGET],
            "entrypoints": {self.TARGET: "bin/into-md-media-provider"},
            "runtimeManifest": None,
            "files": package_files,
            "signature": {
                "signedPayloadVersion": 1,
                "algorithm": "ed25519",
                "keyId": "test",
                "publicKeyBase64": base64.b64encode(public_key).decode(),
                "publicKeySha256": hashlib.sha256(public_key).hexdigest(),
                "signedPayloadSha256": "",
                "signatureBase64": base64.b64encode(b"s" * 64).decode(),
            },
        }
        manifest["signature"]["signedPayloadSha256"] = hashlib.sha256(
            assemble._canonical_signed_payload(manifest)
        ).hexdigest()
        if corrupt_signature:
            manifest["signature"]["signedPayloadSha256"] = "0" * 64
        members = {**runtime, "provider.json": provider_bytes}
        members["plugin.json"] = json.dumps(manifest, separators=(",", ":")).encode()
        with zipfile.ZipFile(path, "x", compression=zipfile.ZIP_STORED) as package:
            for name, contents in sorted(members.items()):
                info = zipfile.ZipInfo(name, (1980, 1, 1, 0, 0, 0))
                info.create_system = 3
                info.external_attr = (
                    stat.S_IFREG | (entry_mode or {}).get(name, 0o644)
                ) << 16
                package.writestr(info, contents, compress_type=zipfile.ZIP_STORED)

    def test_embedded_runtime_build_contract_is_present_without_large_payloads(self) -> None:
        build_script = (assemble.ROOT / "apps/cli/build.rs").read_text(encoding="utf-8")
        platform_release = (assemble.ROOT / "tools/platform-release/release.py").read_text(
            encoding="utf-8"
        )
        macos_release = (assemble.ROOT / "tools/macos-release/release.py").read_text(
            encoding="utf-8"
        )
        for required in [
            "CARGO_FEATURE_EMBEDDED_RUNTIME",
            "INTO_MD_EMBEDDED_PDFIUM_ROOT",
            "INTO_MD_EMBEDDED_OCR_ROOT",
            "validate_pdfium",
            "validate_ocr",
            "runtime payload contains link",
            'target_os == "windows"',
            '("&[]".to_owned(), String::new(), String::new())',
        ]:
            self.assertIn(required, build_script)
        for authority in [platform_release, macos_release]:
            self.assertIn('"embedded-runtime"', authority)
            self.assertIn("INTO_MD_EMBEDDED_PDFIUM_ROOT", authority)
            self.assertIn("INTO_MD_EMBEDDED_OCR_ROOT", authority)

    def test_platform_build_and_downloads_run_concurrently_before_publication(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            release = self.ConcurrentRelease()
            result = assemble.build_and_acquire(
                release,
                self.TARGET,
                {},
                root / "build",
                root / "cache",
                root / "timings.json",
            )
            self.assertEqual(result, root / "build/release")
            self.assertTrue((root / "cache").is_dir())
            self.assertFalse((root / "release").exists())
            timings = json.loads((root / "timings.json").read_text(encoding="utf-8"))
            self.assertEqual(timings["target"], self.TARGET)
            self.assertIn("helper-provider-build", timings["phases"])

    def test_parallel_platform_input_failure_propagates_before_publication(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            for release, failure in [
                (self.ConcurrentRelease(fail_build=True), "build failed"),
                (self.ConcurrentRelease(fail_acquire=True), "acquire failed"),
            ]:
                with self.subTest(failure=failure):
                    with self.assertRaisesRegex(RuntimeError, failure):
                        assemble.build_and_acquire(
                            release,
                            self.TARGET,
                            {},
                            root / "build",
                            root / "cache",
                        )
                    self.assertFalse((root / "release").exists())

    def test_non_windows_core_archive_has_one_direct_run_binary(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            binary = root / "source"
            elf = bytearray(64)
            elf[:6] = b"\x7fELF\x02\x01"
            struct.pack_into("<H", elf, 18, 62)
            binary.write_bytes(elf)
            archive = root / "into-md-linux-x86_64.zip"
            materials = core_materials(root)
            assemble.create_core_archive(
                binary,
                archive,
                "into-md",
                materials=materials,
                target=self.TARGET,
            )
            with zipfile.ZipFile(archive) as value:
                self.assertEqual(
                    value.namelist(),
                    [
                        "into-md",
                        *assemble.CORE_MATERIAL_MEMBERS,
                        assemble.CORE_ARCHIVE_MANIFEST,
                    ],
                )
                info = value.infolist()[0]
                self.assertEqual(
                    (info.external_attr >> 16) & 0o177777,
                    stat.S_IFREG | 0o755,
                )

            with self.assertRaisesRegex(
                assemble.PortableReleaseError, "authority is required"
            ):
                assemble.verify_core_archive(archive, self.TARGET)
            assemble.verify_core_archive(
                archive, self.TARGET, core_authority(materials, self.TARGET)
            )

    def test_core_rejects_replaced_material_with_recomputed_package_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            binary = root / "into-md"
            elf = bytearray(64)
            elf[:6] = b"\x7fELF\x02\x01"
            struct.pack_into("<H", elf, 18, 62)
            binary.write_bytes(elf)
            materials = core_materials(root)
            valid = root / "valid.zip"
            assemble.create_core_archive(
                binary,
                valid,
                "into-md",
                materials=materials,
                target=self.TARGET,
            )
            with zipfile.ZipFile(valid) as source:
                entries = [(info, source.read(info)) for info in source.infolist()]
            replacement = b"attacker-controlled notice"
            manifest = json.loads(
                next(
                    data
                    for info, data in entries
                    if info.filename == assemble.CORE_ARCHIVE_MANIFEST
                )
            )
            for record in manifest["files"]:
                if record["path"] == "NOTICE":
                    record["bytes"] = len(replacement)
                    record["sha256"] = hashlib.sha256(replacement).hexdigest()
            manifest_bytes = json.dumps(manifest).encode()
            tampered = root / "tampered.zip"
            with zipfile.ZipFile(tampered, "x") as output:
                for original, data in entries:
                    info = zipfile.ZipInfo(original.filename, original.date_time)
                    info.create_system = original.create_system
                    info.compress_type = original.compress_type
                    info.external_attr = original.external_attr
                    if original.filename == "NOTICE":
                        data = replacement
                    elif original.filename == assemble.CORE_ARCHIVE_MANIFEST:
                        data = manifest_bytes
                    output.writestr(
                        info,
                        data,
                        compress_type=info.compress_type,
                        compresslevel=9,
                    )
            with self.assertRaisesRegex(
                assemble.PortableReleaseError, "independent authority"
            ):
                assemble.verify_core_archive(
                    tampered,
                    self.TARGET,
                    core_authority(materials, self.TARGET),
                )

    def test_core_rejects_structurally_invalid_generated_json_materials(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            binary = root / "into-md"
            elf = bytearray(64)
            elf[:6] = b"\x7fELF\x02\x01"
            struct.pack_into("<H", elf, 18, 62)
            binary.write_bytes(elf)
            for material_name in ("SBOM.spdx.json", "SOURCES.json"):
                with self.subTest(material=material_name):
                    materials = core_materials(root)
                    materials[material_name].write_text("{}", encoding="utf-8")
                    with self.assertRaisesRegex(
                        assemble.PortableReleaseError, "invalid schema"
                    ):
                        assemble.create_core_archive(
                            binary,
                            root / f"invalid-{material_name}.zip",
                            "into-md",
                            materials=materials,
                            target=self.TARGET,
                        )

    def test_core_authority_preserves_non_utf8_upstream_license_bytes(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            materials = core_materials(root)
            license_name = "licenses/pdfium/licenses/freetype.txt"
            materials[license_name].write_bytes(b"Copyright \xa9 upstream")
            authority = core_authority(materials, self.TARGET)
            destination = root / assemble.CORE_MATERIAL_AUTHORITY
            assemble.write_core_material_authority(
                destination, materials, self.TARGET
            )
            self.assertEqual(json.loads(destination.read_text()), authority)

    def test_windows_core_archive_requires_manifest_pinned_pdfium(self) -> None:
        target = "x86_64-pc-windows-msvc"
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            binary = root / "into-md.exe"
            binary.write_bytes(pe_x86_64())
            archive = root / "core.zip"
            assemble.create_core_archive(binary, archive, "into-md.exe")
            with self.assertRaisesRegex(assemble.PortableReleaseError, "inventory"):
                assemble.verify_core_archive(archive, target)

            archive.unlink()
            runtime = root / "pdfium.dll"
            runtime.write_bytes(b"tampered")
            assemble.create_core_archive(
                binary,
                archive,
                "into-md.exe",
                (runtime, assemble.WINDOWS_PDFIUM_MEMBER),
                core_materials(root),
                target,
            )
            with self.assertRaisesRegex(assemble.PortableReleaseError, "pinned manifest"):
                assemble.verify_core_archive(archive, target)

    def test_windows_core_archive_rejects_link_typed_pdfium_member(self) -> None:
        target = "x86_64-pc-windows-msvc"
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            archive = root / "core.zip"
            binary = root / "into-md.exe"
            runtime = root / "pdfium.dll"
            binary.write_bytes(pe_x86_64())
            runtime.write_bytes(b"outside/pdfium.dll")
            assemble.create_core_archive(
                binary,
                archive,
                "into-md.exe",
                (runtime, assemble.WINDOWS_PDFIUM_MEMBER),
                core_materials(root),
                target,
            )
            with zipfile.ZipFile(archive) as source:
                entries = [(info, source.read(info)) for info in source.infolist()]
            archive.unlink()
            with zipfile.ZipFile(archive, "w") as value:
                for info, data in entries:
                    if info.filename == assemble.WINDOWS_PDFIUM_MEMBER:
                        info.external_attr = (stat.S_IFLNK | 0o777) << 16
                    value.writestr(info, data)
            with self.assertRaisesRegex(assemble.PortableReleaseError, "regular file"):
                assemble.verify_core_archive(archive, target)

    def test_windows_core_archive_rejects_pdfium_authority_embedded_in_binary(self) -> None:
        target = "x86_64-pc-windows-msvc"
        runtime = b"pinned-runtime"
        authority = {
            "library_size": len(runtime),
            "library_sha256": hashlib.sha256(runtime).hexdigest(),
        }
        with tempfile.TemporaryDirectory() as name, mock.patch.object(
            assemble, "windows_pdfium_authority", return_value=authority
        ):
            root = pathlib.Path(name)
            binary = root / "into-md.exe"
            embedded = io.BytesIO()
            with zipfile.ZipFile(
                embedded, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9
            ) as payload:
                payload.writestr(assemble.WINDOWS_PDFIUM_MEMBER, runtime)
            binary.write_bytes(pe_x86_64() + embedded.getvalue())
            runtime_path = root / "pdfium.dll"
            runtime_path.write_bytes(runtime)
            archive = root / "core.zip"
            assemble.create_core_archive(
                binary,
                archive,
                "into-md.exe",
                (runtime_path, assemble.WINDOWS_PDFIUM_MEMBER),
                core_materials(root),
                target,
            )
            with self.assertRaisesRegex(assemble.PortableReleaseError, "embedded PDFium"):
                assemble.verify_core_archive(archive, target)

            archive.unlink()
            binary.write_bytes(
                pe_x86_64()
                + b"PK\x03\x04"
                + bytes(80)
                + assemble.WINDOWS_PDFIUM_MEMBER.encode("ascii")
            )
            assemble.create_core_archive(
                binary,
                archive,
                "into-md.exe",
                (runtime_path, assemble.WINDOWS_PDFIUM_MEMBER),
                core_materials(root),
                target,
            )
            assemble.verify_core_archive(
                archive, target, core_authority(core_materials(root), target)
            )

    def test_forbidden_speech_evidence_contract_is_complete(self) -> None:
        self.assertIn("SBOM.spdx.json", assemble.FORBIDDEN_PLUGIN_FILES)
        self.assertIn("source/", assemble.FORBIDDEN_PLUGIN_PREFIXES)
        self.assertIn("relink/", assemble.FORBIDDEN_PLUGIN_PREFIXES)

    def test_macos_architecture_requires_arm64_cpu_type(self) -> None:
        arm64 = bytearray(32)
        arm64[:4] = b"\xcf\xfa\xed\xfe"
        struct.pack_into("<I", arm64, 4, 0x0100000C)
        x86_64 = bytearray(arm64)
        struct.pack_into("<I", x86_64, 4, 0x01000007)
        self.assertTrue(assemble.binary_architecture(arm64, "aarch64-apple-darwin"))
        self.assertFalse(assemble.binary_architecture(x86_64, "aarch64-apple-darwin"))

    def test_speech_package_requires_signed_complete_manifests(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            package_path = root / "speech.imp"
            self.write_speech_package(package_path)
            with zipfile.ZipFile(package_path) as package:
                assemble._validate_speech_package(package, self.TARGET)
            corrupt = root / "corrupt-signature.imp"
            self.write_speech_package(corrupt, corrupt_signature=True)
            with zipfile.ZipFile(corrupt) as package, self.assertRaisesRegex(
                assemble.PortableReleaseError, "signature authority"
            ):
                assemble._validate_speech_package(package, self.TARGET)

    def test_speech_package_rejects_nested_audit_entries(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            package_path = pathlib.Path(name) / "speech.imp"
            self.write_speech_package(
                package_path, extra=("audit/SBOM.spdx.json", b"{}")
            )
            with zipfile.ZipFile(package_path) as package, self.assertRaisesRegex(
                assemble.PortableReleaseError, "audit-only"
            ):
                assemble._validate_speech_package(package, self.TARGET)

    def test_speech_package_requires_exact_ggml_runtime(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            package_path = pathlib.Path(name) / "speech.imp"
            self.write_speech_package(
                package_path,
                extra=("bin/libggml-cpu-attacker.so", b"not an ELF"),
            )
            with zipfile.ZipFile(package_path) as package, self.assertRaisesRegex(
                assemble.PortableReleaseError, "GGML runtime inventory"
            ):
                assemble._validate_speech_package(package, self.TARGET)

    def test_speech_package_rejects_duplicate_link_or_abnormal_mode(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            valid = root / "valid.imp"
            self.write_speech_package(valid)

            duplicate = root / "duplicate.imp"
            duplicate.write_bytes(valid.read_bytes())
            with warnings.catch_warnings():
                warnings.simplefilter("ignore", UserWarning)
                with zipfile.ZipFile(duplicate, "a") as package:
                    package.writestr("provider.json", b"{}")
            with zipfile.ZipFile(duplicate) as package, self.assertRaisesRegex(
                assemble.PortableReleaseError, "duplicate"
            ):
                assemble._validate_speech_package(package, self.TARGET)

            link = root / "link.imp"
            self.write_speech_package(
                link,
                entry_mode={"bin/into-md-media-provider": stat.S_IFLNK | 0o777},
            )
            with zipfile.ZipFile(link) as package, self.assertRaisesRegex(
                assemble.PortableReleaseError, "metadata"
            ):
                assemble._validate_speech_package(package, self.TARGET)

            mode = root / "mode.imp"
            self.write_speech_package(
                mode, entry_mode={"bin/into-md-media-provider": 0o600}
            )
            with zipfile.ZipFile(mode) as package, self.assertRaisesRegex(
                assemble.PortableReleaseError, "metadata"
            ):
                assemble._validate_speech_package(package, self.TARGET)


if __name__ == "__main__":
    unittest.main()

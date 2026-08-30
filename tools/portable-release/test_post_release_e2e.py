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


def material_bytes(name: str) -> bytes:
    static = {
        "LICENSE": e2e.ROOT / "LICENSE",
        "licenses/npm/npm-release.spdx.json": e2e.ROOT
        / "third_party/licenses/npm-release.spdx.json",
        "licenses/npm/lucide-ISC-MIT.txt": e2e.ROOT
        / "third_party/licenses/npm/lucide-ISC-MIT.txt",
        "licenses/npm/react-MIT.txt": e2e.ROOT
        / "third_party/licenses/npm/react-MIT.txt",
    }
    if name in static:
        return static[name].read_bytes()
    if name == "SBOM.spdx.json":
        return json.dumps(
            {
                "SPDXID": "SPDXRef-DOCUMENT",
                "spdxVersion": "SPDX-2.3",
                "dataLicense": "CC0-1.0",
                "creationInfo": {},
                "packages": [{"name": "fixture"}],
            }
        ).encode()
    if name == "SOURCES.json":
        return json.dumps(
            {
                "schema_version": 1,
                "target": "fixture",
                "artifact": "into-markdown-core",
                "version": "0.0.3",
                "source_revision": "fixture",
                "components": [{"id": "fixture"}],
            }
        ).encode()
    return f"material:{name}".encode()


def material_authority(target: str) -> dict:
    return {
        "schemaVersion": 1,
        "target": target,
        "files": [
            {
                "path": name,
                "bytes": len(material_bytes(name)),
                "sha256": __import__("hashlib").sha256(material_bytes(name)).hexdigest(),
            }
            for name in e2e.CORE_MATERIAL_MEMBERS
        ],
    }


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
    entries.extend((name, material_bytes(name), 0o644) for name in e2e.CORE_MATERIAL_MEMBERS)
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


def write_skill_archive(path: pathlib.Path, runtime: bytes | None = None) -> dict:
    files = {
        "into-markdown/LICENSE": (e2e.ROOT / "LICENSE").read_bytes(),
        "into-markdown/SKILL.md": b"skill",
        "into-markdown/agents/openai.yaml": b"interface: {}",
        "into-markdown/assets/linux-arm64/into-md": elf(183),
        "into-markdown/assets/linux-x86_64/into-md": elf(),
        "into-markdown/assets/windows-x86_64/into-md.exe": pe_x86_64(),
        "into-markdown/references/cli-workflows.md": b"reference",
    }
    if runtime is not None:
        files[e2e.WINDOWS_SKILL_PDFIUM] = runtime
    for target, _asset in e2e.SKILL_TARGETS:
        for relative in e2e.CORE_MATERIAL_MEMBERS:
            files[f"into-markdown/evidence/{target}/{relative}"] = material_bytes(relative)
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
                else "target-evidence"
                if relative.startswith("evidence/")
                else "skill-source"
            ),
            **(
                {"componentId": "pdfium"}
                if name == e2e.WINDOWS_SKILL_PDFIUM or "/licenses/pdfium/" in relative
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
    targets = []
    for target, asset in e2e.SKILL_TARGETS:
        binary = files[f"into-markdown/{asset}"]
        targets.append(
            {
                "target": target,
                "binary": {
                    "path": asset,
                    "bytes": len(binary),
                    "sha256": __import__("hashlib").sha256(binary).hexdigest(),
                },
                "materials": [
                    {
                        "path": relative,
                        "bytes": len(material_bytes(relative)),
                        "sha256": __import__("hashlib").sha256(material_bytes(relative)).hexdigest(),
                    }
                    for relative in sorted(e2e.CORE_MATERIAL_MEMBERS)
                ],
            }
        )
    return {
        "schemaVersion": 1,
        "namespace": "into-markdown/skill-release-authority",
        "artifact": "into-markdown",
        "targets": targets,
    }


class PostReleaseE2ETests(unittest.TestCase):
    def test_release_url_is_stable_and_rejects_unsafe_identity(self) -> None:
        self.assertEqual(
            e2e.release_asset_url("owner/repository", "0.0.3", "asset.zip"),
            "https://github.com/owner/repository/releases/download/0.0.3/asset.zip",
        )
        for repository, tag in (("owner", "0.0.3"), ("owner/../repo", "0.0.3"), ("owner/repo", "../tag")):
            with self.subTest(repository=repository, tag=tag), self.assertRaises(e2e.E2EError):
                e2e.release_asset_url(repository, tag, "asset.zip")

    def test_release_version_accepts_plain_and_v_prefixed_tag_spellings(self) -> None:
        self.assertEqual(e2e.normalize_release_version("0.0.3"), "0.0.3")
        self.assertEqual(e2e.normalize_release_version("v0.0.3"), "0.0.3")
        for value in ("", "vv0.0.3"):
            with self.subTest(value=value), self.assertRaises(e2e.E2EError):
                e2e.normalize_release_version(value)

    def test_core_archive_requires_one_direct_run_binary_and_mode(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            archive_path = root / "into-md-linux-x86_64.zip"
            write_core_archive(archive_path, "linux")
            authority = material_authority(e2e.TARGETS["linux"]["target"])
            report = e2e.extract_single_core(
                archive_path, "linux", root / "into-md", authority
            )
            self.assertEqual((report["format"], report["architecture"]), ("ELF", "x86_64"))
            with zipfile.ZipFile(archive_path, "a") as archive:
                archive.writestr("unexpected", "unexpected")
            with self.assertRaisesRegex(e2e.E2EError, "exactly into-md"):
                e2e.extract_single_core(
                    archive_path, "linux", root / "other", authority
                )

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
            material = material_authority(e2e.TARGETS["windows"]["target"])
            report = e2e.extract_single_core(
                archive_path, "windows", output, material
            )
            self.assertEqual(
                report["memberCount"], len(e2e.CORE_MATERIAL_MEMBERS) + 3
            )
            self.assertEqual(
                (output.parent / e2e.WINDOWS_PDFIUM_MEMBER).read_bytes(), runtime
            )

            for bad_runtime, extra in ((b"tampered-pdfium", None), (runtime, "unexpected")):
                write_core_archive(archive_path, "windows", bad_runtime, extra)
                with self.subTest(runtime=bad_runtime, extra=extra), self.assertRaises(e2e.E2EError):
                    e2e.extract_single_core(
                        archive_path, "windows", root / "rejected.exe", material
                    )

            write_core_archive(archive_path, "windows")
            with self.assertRaisesRegex(e2e.E2EError, "pdfium.dll"):
                e2e.extract_single_core(
                    archive_path, "windows", root / "missing.exe", material
                )

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
            material = write_skill_archive(archive_path, runtime)
            output = root / "skill" / "skill.exe"
            e2e.extract_skill_binary(archive_path, "windows", output, material)
            self.assertEqual(
                (output.parent / e2e.WINDOWS_PDFIUM_MEMBER).read_bytes(), runtime
            )

            write_skill_archive(archive_path)
            with self.assertRaisesRegex(e2e.E2EError, "exact reviewed archive inventory"):
                e2e.extract_skill_binary(
                    archive_path, "windows", root / "missing.exe", material
                )

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
            expected = {
                name: {"bytes": record["bytes"], "sha256": record["sha256"]}
                for name, record in records.items()
                if name != e2e.SKILL_ARCHIVE
            }
            expected[e2e.TARGETS["linux"]["core"]]["sha256"] = "0" * 64
            with self.assertRaisesRegex(e2e.E2EError, "independent authority"):
                e2e.acquire_assets(
                    root, "owner/repo", "0.0.3", ["linux"], expected_assets=expected
                )

    def test_download_rejects_missing_length_insecure_redirect_and_length_mismatch(self) -> None:
        class Response:
            def __init__(self, data: bytes, url: str, length: str | None):
                self.data = data
                self.url = url
                self.headers = {} if length is None else {"Content-Length": length}
                self.offset = 0

            def __enter__(self):
                return self

            def __exit__(self, *_args):
                return False

            def geturl(self):
                return self.url

            def read(self, size):
                value = self.data[self.offset : self.offset + size]
                self.offset += len(value)
                return value

        cases = (
            (Response(b"x", "https://github.com/asset", None), "Content-Length"),
            (Response(b"x", "http://attacker.invalid/asset", "1"), "not HTTPS"),
            (Response(b"xx", "https://github.com/asset", "1"), "declared length"),
        )
        for response, message in cases:
            with self.subTest(message=message), tempfile.TemporaryDirectory() as name:
                root = pathlib.Path(name)
                for asset in (e2e.TARGETS["linux"]["core"], e2e.TARGETS["linux"]["speech"]):
                    (root / asset).write_bytes(b"local")
                with mock.patch("release_artifacts.urllib.request.urlopen", return_value=response), self.assertRaisesRegex(e2e.E2EError, message):
                    e2e.acquire_assets(root, "owner/repo", "0.0.3", ["linux"])

    def test_zip_preflight_rejects_entry_flood_ratio_bomb_and_shared_budget(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            flood = root / "flood.zip"
            with zipfile.ZipFile(flood, "w") as archive:
                for index in range(e2e.MAX_ARCHIVE_ENTRIES + 1):
                    archive.writestr(f"entry-{index}", b"")
            with self.assertRaisesRegex(e2e.E2EError, "central directory"):
                e2e.extract_single_core(
                    flood, "linux", root / "flood-output", material_authority(e2e.TARGETS["linux"]["target"])
                )

            bomb = root / "bomb.zip"
            with zipfile.ZipFile(bomb, "w", compression=zipfile.ZIP_DEFLATED) as archive:
                archive.writestr("bomb", b"0" * (8 * 1024 * 1024))
            with self.assertRaisesRegex(e2e.E2EError, "compression ratio"):
                e2e.extract_single_core(
                    bomb, "linux", root / "bomb-output", material_authority(e2e.TARGETS["linux"]["target"])
                )

            core = root / "core.zip"
            write_core_archive(core, "linux")
            budget = e2e.ResourceBudget(max_entries=1)
            with self.assertRaisesRegex(e2e.E2EError, "entries budget"):
                e2e.extract_single_core(
                    core,
                    "linux",
                    root / "budget-output",
                    material_authority(e2e.TARGETS["linux"]["target"]),
                    budget,
                )

    def test_skill_black_box_rejects_no_output_wrong_version_and_wrong_content(self) -> None:
        class Result:
            def __init__(self, stdout=b""):
                self.stdout = stdout
                self.stderr = b""
                self.elapsed_ms = 1

        def protect(path, _platform):
            path.mkdir(parents=True, exist_ok=False)
            return path

        def isolated(path, _platform):
            path.mkdir(parents=True, exist_ok=False)
            return {"PATH": "", "XDG_CACHE_HOME": str(path / "cache")}, path

        for scenario, expected in (
            ("no-output", "missing or empty"),
            ("wrong-version", "version does not match"),
            ("wrong-text", "fixture authority text"),
            ("wrong-pdf", "differs from the authenticated Core"),
            ("wrong-ocr", "fixture authority text"),
        ):
            with self.subTest(scenario=scenario), tempfile.TemporaryDirectory() as name:
                root = pathlib.Path(name)

                class FakeRunner:
                    def __init__(self, _binary, _environment, _work):
                        self.cases = []

                    def call(self, case, arguments):
                        self.cases.append({"name": case})
                        if case == "skill-version-empty-path":
                            version = "9.9.9" if scenario == "wrong-version" else "0.0.3"
                            return Result(json.dumps({"name": "into-md", "version": version}).encode())
                        if scenario != "no-output":
                            output = pathlib.Path(arguments[arguments.index("-o") + 1])
                            if case == "skill-text-empty-path":
                                output.write_text("wrong" if scenario == "wrong-text" else "Alpha 中文 line\nSecond line", encoding="utf-8")
                            elif case == "skill-pdf-empty-path":
                                output.write_bytes(b"wrong" if scenario == "wrong-pdf" else b"core-pdf")
                            else:
                                output.write_text(
                                    "wrong" if scenario == "wrong-ocr" else "clear scans verify document conversion quality",
                                    encoding="utf-8",
                                )
                        return Result()

                with self.assertRaisesRegex(e2e.E2EError, expected):
                    e2e.run_skill_packaged_runtime(
                        root / "skill",
                        "linux",
                        e2e.ROOT / "fixtures",
                        root / "run",
                        0,
                        "0.0.3",
                        b"core-pdf",
                        protect,
                        isolated,
                        FakeRunner,
                        e2e._copy_fixture,
                        e2e.conversion_arguments,
                        e2e.assert_output,
                        lambda _environment, _platform: [],
                    )

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

    def test_windows_packaged_pdfium_negative_matrix_covers_core_and_skill_layouts(self) -> None:
        class Result:
            stdout = b""
            stderr = b"componentUnavailable: packaged PDFium is unavailable"

        class FakeRunner:
            def __init__(self, binary, environment, work):
                self.binary = binary
                self.environment = environment
                self.work = work
                self.cases = []

            def call(self, case, _arguments, *, succeed):
                self.cases.append({"name": case, "exitCode": 1})
                self_test.assertFalse(succeed)
                self_test.assertTrue(
                    (pathlib.Path(self.environment["PATH"]) / "pdfium.dll").is_file()
                )
                self_test.assertTrue((self.work / "pdfium.dll").is_file())
                return Result()

        def protect(path, _platform):
            path.mkdir(parents=True, exist_ok=False)
            return path

        def isolated(path, _platform):
            path.mkdir(parents=True, exist_ok=False)
            return {"PATH": ""}, path

        def copy_fixture(_fixtures, _relative, destination):
            destination.write_bytes(b"%PDF fixture")
            return destination

        def fake_reparse(path, target, _platform):
            target.mkdir(parents=True, exist_ok=False)
            path.mkdir(parents=True, exist_ok=False)

        self_test = self
        with tempfile.TemporaryDirectory() as name, mock.patch(
            "post_release_scenarios.create_runtime_reparse", fake_reparse
        ):
            root = pathlib.Path(name)
            for layout in ("core", "skill"):
                binary = root / layout / "into-md.exe"
                binary.parent.mkdir(parents=True)
                binary.write_bytes(pe_x86_64())
                runtime = binary.parent / e2e.WINDOWS_PDFIUM_MEMBER
                runtime.parent.mkdir(parents=True)
                runtime.write_bytes(b"authenticated-pdfium")
                cases = e2e.run_packaged_pdfium_negative_cases(
                    binary,
                    root,
                    root / f"{layout}-negative",
                    "windows",
                    protect,
                    isolated,
                    copy_fixture,
                    FakeRunner,
                    e2e.conversion_arguments,
                    lambda value: value.decode(),
                )
                self.assertEqual(
                    [case["name"] for case in cases],
                    [
                        "packaged-pdfium-missing",
                        "packaged-pdfium-tampered",
                        "packaged-pdfium-reparse",
                    ],
                )


if __name__ == "__main__":
    unittest.main()

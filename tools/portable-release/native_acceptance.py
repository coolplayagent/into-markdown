#!/usr/bin/env python3
"""Audit and minimally exercise one compact native Core release artifact."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import stat
import struct
import subprocess
import sys
import tempfile
import time
import zipfile


PORTABLE_RELEASE_DIR = pathlib.Path(__file__).resolve().parent
if str(PORTABLE_RELEASE_DIR) not in sys.path:
    sys.path.insert(0, str(PORTABLE_RELEASE_DIR))
from drawio_smoke import drawio_cases


CORE_ARCHIVES = {
    "x86_64-pc-windows-msvc": ("into-md-windows-x86_64.zip", "into-md.exe", "PE", "x86_64"),
    "x86_64-unknown-linux-gnu": ("into-md-linux-x86_64.zip", "into-md", "ELF", "x86_64"),
    "aarch64-unknown-linux-gnu": ("into-md-linux-arm64.zip", "into-md", "ELF", "aarch64"),
    "aarch64-apple-darwin": ("into-md-macos-arm64.zip", "into-md", "Mach-O", "arm64"),
}
MAX_OUTPUT_BYTES = 64 * 1024
LEGACY_OFFICE_FIXTURES = (
    pathlib.Path(__file__).resolve().parents[1] / "macos-release" / "fixtures"
)
ROOT = pathlib.Path(__file__).resolve().parents[2]
PDF_FIXTURE = ROOT / "fixtures/small/pdf/structures.pdf"
PDFIUM_MANIFEST = json.loads(
    (ROOT / "third_party/pdfium/manifest.json").read_text(encoding="utf-8")
)
WINDOWS_PDFIUM_MEMBER = "lib/pdfium/pdfium.dll"
CORE_ARCHIVE_MANIFEST = "archive-manifest.json"
PDFIUM_LICENSE_FILES = (
    "LICENSE",
    "licenses/abseil.txt",
    "licenses/agg23.txt",
    "licenses/fast_float.txt",
    "licenses/freetype.txt",
    "licenses/icu.txt",
    "licenses/lcms.txt",
    "licenses/libjpeg_turbo.ijg",
    "licenses/libjpeg_turbo.md",
    "licenses/libopenjpeg.txt",
    "licenses/libpng.txt",
    "licenses/libtiff.txt",
    "licenses/llvm-libc.txt",
    "licenses/pdfium.txt",
    "licenses/simdutf.txt",
    "licenses/zlib.txt",
)
CORE_MATERIAL_MEMBERS = (
    "LICENSE",
    "NOTICE",
    "THIRD_PARTY_NOTICES.md",
    "SBOM.spdx.json",
    "SOURCES.json",
    "licenses/npm/npm-release.spdx.json",
    "licenses/npm/lucide-ISC-MIT.txt",
    "licenses/npm/react-MIT.txt",
    "licenses/diagram-design-MIT.txt",
    *(f"licenses/pdfium/{path}" for path in PDFIUM_LICENSE_FILES),
)


class AcceptanceError(RuntimeError):
    """The compact artifact failed its native release acceptance."""


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def sha256_file(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def inspect_binary(data: bytes, target: str) -> tuple[str, str]:
    if target == "x86_64-pc-windows-msvc":
        if len(data) < 64 or data[:2] != b"MZ":
            raise AcceptanceError("Core is not a PE executable")
        offset = struct.unpack_from("<I", data, 0x3C)[0]
        if offset + 6 > len(data) or data[offset : offset + 4] != b"PE\0\0":
            raise AcceptanceError("Core has an invalid PE header")
        if struct.unpack_from("<H", data, offset + 4)[0] != 0x8664:
            raise AcceptanceError("Core PE architecture is not x86_64")
        return "PE", "x86_64"
    if target.endswith("linux-gnu"):
        expected = 62 if target.startswith("x86_64") else 183
        if len(data) < 20 or data[:7] != b"\x7fELF\x02\x01\x01":
            raise AcceptanceError("Core is not a 64-bit little-endian ELF executable")
        if struct.unpack_from("<H", data, 18)[0] != expected:
            raise AcceptanceError("Core ELF architecture does not match its target")
        return "ELF", "x86_64" if expected == 62 else "aarch64"
    if len(data) < 8 or data[:4] not in {b"\xcf\xfa\xed\xfe", b"\xfe\xed\xfa\xcf"}:
        raise AcceptanceError("Core is not a 64-bit Mach-O executable")
    endian = "<" if data[:4] == b"\xcf\xfa\xed\xfe" else ">"
    if struct.unpack_from(f"{endian}I", data, 4)[0] != 0x0100000C:
        raise AcceptanceError("Core Mach-O architecture is not arm64")
    return "Mach-O", "arm64"


def audit_archive(output: pathlib.Path, target: str) -> tuple[dict, dict[str, bytes], str]:
    archive_name, member, expected_format, expected_arch = CORE_ARCHIVES[target]
    archive_path = output / "release" / archive_name
    if not archive_path.is_file() or archive_path.is_symlink():
        raise AcceptanceError(f"Core archive is unavailable: {archive_name}")
    with zipfile.ZipFile(archive_path) as archive:
        infos = archive.infolist()
        expected = [member]
        if target == "x86_64-pc-windows-msvc":
            expected.append(WINDOWS_PDFIUM_MEMBER)
        expected.extend((*CORE_MATERIAL_MEMBERS, CORE_ARCHIVE_MANIFEST))
        if [info.filename for info in infos] != expected or any(info.is_dir() for info in infos):
            raise AcceptanceError("Core ZIP member inventory is invalid")
        info = infos[0]
        mode = (info.external_attr >> 16) & 0o177777
        expected_mode = stat.S_IFREG | (0o644 if member.endswith(".exe") else 0o755)
        if mode != expected_mode:
            raise AcceptanceError("Core ZIP member mode is invalid")
        contents = {item.filename: archive.read(item) for item in infos}
        data = contents[member]
        if target == "x86_64-pc-windows-msvc":
            runtime = infos[1]
            runtime_mode = (runtime.external_attr >> 16) & 0o177777
            if runtime_mode != stat.S_IFREG | 0o644:
                raise AcceptanceError("Windows PDFium member is not a regular file")
            authority = PDFIUM_MANIFEST["targets"][target]
            runtime_data = contents[WINDOWS_PDFIUM_MEMBER]
            if (
                len(runtime_data) != authority["library_size"]
                or sha256_bytes(runtime_data) != authority["library_sha256"]
            ):
                raise AcceptanceError("Windows PDFium differs from the pinned manifest")
        for material in infos[2 if target == "x86_64-pc-windows-msvc" else 1 :]:
            if (material.external_attr >> 16) & 0o177777 != stat.S_IFREG | 0o644:
                raise AcceptanceError("Core license or manifest member mode is invalid")
        if any(not contents[f"licenses/pdfium/{name}"] for name in PDFIUM_LICENSE_FILES):
            raise AcceptanceError("Core contains an empty PDFium license member")
        try:
            manifest = json.loads(contents[CORE_ARCHIVE_MANIFEST])
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise AcceptanceError("Core archive manifest is invalid") from error
        projected = manifest.get("files") if isinstance(manifest, dict) else None
        observed = [
            {
                "path": item.filename,
                "bytes": len(contents[item.filename]),
                "sha256": sha256_bytes(contents[item.filename]),
                "mode": f"{((item.external_attr >> 16) & 0o777):04o}",
                "kind": (
                    "component"
                    if item.filename == WINDOWS_PDFIUM_MEMBER
                    else "license-material"
                    if item.filename.startswith("licenses/")
                    else "declaration"
                    if item.filename in {"LICENSE", "NOTICE"}
                    else "generated"
                    if item.filename
                    in {"THIRD_PARTY_NOTICES.md", "SBOM.spdx.json", "SOURCES.json"}
                    else "project"
                ),
                **(
                    {"componentId": "pdfium"}
                    if item.filename == WINDOWS_PDFIUM_MEMBER
                    or item.filename.startswith("licenses/pdfium/")
                    else {}
                ),
            }
            for item in infos[:-1]
        ]
        if (
            not isinstance(manifest, dict)
            or set(manifest) != {"schemaVersion", "target", "files"}
            or manifest.get("schemaVersion") != 1
            or manifest.get("target") != target
            or projected != observed
        ):
            raise AcceptanceError("Core archive differs from its bidirectional manifest")
    format_name, architecture = inspect_binary(data, target)
    if (format_name, architecture) != (expected_format, expected_arch):
        raise AcceptanceError("Core binary identity does not match the release target")
    report = {
        "schemaVersion": 1,
        "target": target,
        "artifact": archive_name,
        "artifactSha256": sha256_file(archive_path),
        "artifactBytes": archive_path.stat().st_size,
        "member": member,
        "memberCount": len(contents),
        "binarySha256": sha256_bytes(data),
        "binaryBytes": len(data),
        "format": format_name,
        "architecture": architecture,
        "mode": f"{mode & 0o777:04o}",
        "conclusion": "pass",
    }
    if target == "x86_64-pc-windows-msvc":
        authority = PDFIUM_MANIFEST["targets"][target]
        report["pdfiumRuntime"] = {
            "version": PDFIUM_MANIFEST["version"],
            "member": WINDOWS_PDFIUM_MEMBER,
            "sha256": authority["library_sha256"],
            "bytes": authority["library_size"],
        }
    return report, contents, member


def bounded(value: bytes) -> str:
    return value[:MAX_OUTPUT_BYTES].decode("utf-8", errors="replace")


def run_case(
    name: str,
    binary: pathlib.Path,
    arguments: list[str],
    cwd: pathlib.Path,
    environment: dict[str, str],
) -> tuple[dict, bytes]:
    started = time.monotonic()
    result = subprocess.run(
        [str(binary), *arguments],
        cwd=cwd,
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=20,
        check=False,
    )
    elapsed = round((time.monotonic() - started) * 1000)
    if result.returncode != 0:
        detail = bounded(result.stderr or result.stdout).strip()
        raise AcceptanceError(f"Core {' '.join(arguments)} failed ({result.returncode}): {detail}")
    return (
        {
            "name": name,
            "exitCode": result.returncode,
            "elapsedMs": elapsed,
            "stdoutSha256": sha256_bytes(result.stdout),
            "stderrSha256": sha256_bytes(result.stderr),
        },
        result.stdout,
    )


def run_failure_case(
    name: str,
    binary: pathlib.Path,
    arguments: list[str],
    cwd: pathlib.Path,
    environment: dict[str, str],
) -> dict:
    result = subprocess.run(
        [str(binary), *arguments, "--log-format", "json"],
        cwd=cwd,
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=20,
        check=False,
    )
    try:
        event = json.loads(result.stderr)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise AcceptanceError(f"{name} did not emit a JSON error") from error
    if result.returncode != 9 or event.get("code") != "componentUnavailable":
        raise AcceptanceError(
            f"{name} did not fail closed as componentUnavailable: "
            f"exit={result.returncode}, event={event}"
        )
    return {"name": name, "exitCode": result.returncode, "code": event["code"]}


def assert_runtime_absent(
    cache: pathlib.Path,
    home: pathlib.Path,
    temporary: pathlib.Path,
    detail: str,
) -> None:
    runtime_roots = [
        cache / "into-markdown" / "runtime",
        home / ".cache" / "into-markdown" / "runtime",
        home / "Library" / "Caches" / "into-markdown" / "runtime",
    ]
    fallback = list(temporary.glob("into-markdown-runtime-*"))
    if any(path.exists() for path in runtime_roots) or fallback:
        raise AcceptanceError(f"{detail} materialized native runtime")


def run_e2e(
    target: str,
    contents: dict[str, bytes],
    member: str,
    artifact_sha: str,
    expected_version: str,
) -> dict:
    temporary_parent = "/var/tmp" if target == "aarch64-apple-darwin" else None
    with tempfile.TemporaryDirectory(
        prefix="into-md-native-e2e-", dir=temporary_parent
    ) as name:
        outer = pathlib.Path(name)
        root = outer / ("bin" if target == "x86_64-pc-windows-msvc" else "core")
        root.mkdir()
        for relative, data in contents.items():
            path = root / pathlib.PurePosixPath(relative)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
        binary = root / member
        binary.chmod(0o700)
        home = outer / "home"
        cache = outer / "cache"
        temporary = outer / "tmp"
        work = outer / "work"
        for directory in (home, cache, temporary, work):
            directory.mkdir(mode=0o700)
        environment = os.environ.copy()
        environment.update(
            {
                "HOME": str(home),
                "USERPROFILE": str(home),
                "LOCALAPPDATA": str(cache),
                "APPDATA": str(root / "appdata"),
                "XDG_CACHE_HOME": str(cache),
                "XDG_CONFIG_HOME": str(root / "config"),
                "TMP": str(temporary),
                "TEMP": str(temporary),
                "TMPDIR": str(temporary),
                "NO_PROXY": "*",
            }
        )
        environment.pop("PDFIUM_LIBRARY", None)
        source = work / "source.txt"
        result = work / "result.md"
        source.write_text("Into Markdown portable release acceptance\n", encoding="utf-8")
        help_case, _ = run_case("help", binary, ["-h"], work, environment)
        version_case, version_output = run_case(
            "version", binary, ["version", "--json", "--no-config"], work, environment
        )
        try:
            version = json.loads(version_output)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise AcceptanceError("Core version output is not JSON") from error
        if version.get("name") != "into-md" or version.get("version") != expected_version:
            raise AcceptanceError("Core version output does not match the release")
        text_case, _ = run_case(
            "plain-text",
            binary,
            [str(source), "-o", str(result), "--conflict", "error", "--no-config"],
            work,
            environment,
        )
        cases = [help_case, version_case, text_case, *drawio_cases(binary, work, environment, run_case, AcceptanceError)]
        if not result.is_file() or "portable release acceptance" not in result.read_text(
            encoding="utf-8"
        ):
            raise AcceptanceError("plain-text conversion output is missing or invalid")
        assert_runtime_absent(cache, home, temporary, "help, version, or plain text conversion")
        for extension in ("doc", "ppt", "xls"):
            fixture = LEGACY_OFFICE_FIXTURES / f"normal.{extension}"
            if not fixture.is_file():
                raise AcceptanceError(f"legacy Office fixture is unavailable: {fixture.name}")
            legacy_result = work / f"normal-{extension}.md"
            legacy_case, _ = run_case(
                f"legacy-office-{extension}",
                binary,
                [
                    str(fixture),
                    "-o",
                    str(legacy_result),
                    "--conflict",
                    "error",
                    "--no-config",
                    "--progress",
                    "never",
                ],
                work,
                environment,
            )
            cases.append(legacy_case)
            if not legacy_result.is_file() or not legacy_result.read_bytes():
                raise AcceptanceError(
                    f"legacy Office {extension.upper()} output is missing or empty"
                )
            assert_runtime_absent(
                cache,
                home,
                temporary,
                f"default legacy Office {extension.upper()} conversion",
            )
        negative_cases = []
        pdfium_runtime = None
        if target == "x86_64-pc-windows-msvc":
            if not PDF_FIXTURE.is_file():
                raise AcceptanceError("real PDF acceptance fixture is unavailable")
            pdf_result = work / "structures.md"
            pdf_case, _ = run_case(
                "real-pdf",
                binary,
                [
                    str(PDF_FIXTURE),
                    "-o",
                    str(pdf_result),
                    "--conflict",
                    "error",
                    "--no-config",
                    "--progress",
                    "never",
                ],
                work,
                environment,
            )
            cases.append(pdf_case)
            if not pdf_result.is_file() or not pdf_result.read_bytes():
                raise AcceptanceError("real PDF conversion output is missing or empty")
            assert_runtime_absent(cache, home, temporary, "packaged PDF conversion")

            runtime = root / WINDOWS_PDFIUM_MEMBER
            pinned = runtime.read_bytes()
            decoy = work / "pdfium.dll"
            decoy.write_bytes(pinned)
            failure_environment = dict(environment)
            failure_environment["PATH"] = str(work) + os.pathsep + environment.get("PATH", "")
            failure_arguments = [
                str(PDF_FIXTURE),
                "-o",
                str(work / "negative.md"),
                "--conflict",
                "overwrite",
                "--no-config",
                "--progress",
                "never",
            ]

            runtime.unlink()
            negative_cases.append(
                run_failure_case(
                    "missing-pdfium-with-path-decoy",
                    binary,
                    failure_arguments,
                    work,
                    failure_environment,
                )
            )
            runtime.write_bytes(bytes([pinned[0] ^ 0xFF]) + pinned[1:])
            negative_cases.append(
                run_failure_case(
                    "tampered-pdfium",
                    binary,
                    failure_arguments,
                    work,
                    failure_environment,
                )
            )
            runtime.unlink()
            outside = root / "outside-pdfium"
            outside.mkdir()
            (outside / "pdfium.dll").write_bytes(pinned)
            runtime.parent.rmdir()
            try:
                os.symlink(outside, runtime.parent, target_is_directory=True)
            except OSError as error:
                raise AcceptanceError(
                    f"cannot create PDFium reparse-point fixture: {error}"
                ) from error
            try:
                negative_cases.append(
                    run_failure_case(
                        "reparse-point-pdfium",
                        binary,
                        failure_arguments,
                        work,
                        failure_environment,
                    )
                )
            finally:
                os.rmdir(runtime.parent)
            runtime.parent.mkdir()
            runtime.write_bytes(pinned)

            explicit_file_root = root / "explicit-file"
            explicit_file_root.mkdir()
            explicit_file = explicit_file_root / "pdfium.dll"
            os.symlink(runtime, explicit_file, target_is_directory=False)
            explicit_environment = dict(environment)
            explicit_environment["PDFIUM_LIBRARY"] = str(explicit_file)
            try:
                negative_cases.append(
                    run_failure_case(
                        "explicit-linked-pdfium",
                        binary,
                        failure_arguments,
                        work,
                        explicit_environment,
                    )
                )
            finally:
                explicit_file.unlink()

            explicit_outside = root / "explicit-outside"
            explicit_outside.mkdir()
            (explicit_outside / "pdfium.dll").write_bytes(pinned)
            explicit_parent = root / "explicit-reparse"
            os.symlink(explicit_outside, explicit_parent, target_is_directory=True)
            explicit_environment["PDFIUM_LIBRARY"] = str(explicit_parent / "pdfium.dll")
            try:
                negative_cases.append(
                    run_failure_case(
                        "explicit-reparse-point-pdfium",
                        binary,
                        failure_arguments,
                        work,
                        explicit_environment,
                    )
                )
            finally:
                os.rmdir(explicit_parent)
            authority = PDFIUM_MANIFEST["targets"][target]
            pdfium_runtime = {
                "version": PDFIUM_MANIFEST["version"],
                "sha256": sha256_bytes(pinned),
                "bytes": len(pinned),
            }
            if (
                pdfium_runtime["sha256"] != authority["library_sha256"]
                or pdfium_runtime["bytes"] != authority["library_size"]
            ):
                raise AcceptanceError("executed PDFium runtime differs from the pinned manifest")
        elif target == "aarch64-apple-darwin":
            resolved_outer = outer.resolve(strict=True)
            if not outer.as_posix().startswith("/var/") or not resolved_outer.as_posix().startswith(
                "/private/var/"
            ):
                raise AcceptanceError("macOS acceptance did not traverse the trusted /var alias")
            unsafe_cache = outer / "unsafe-cache"
            unsafe_cache.mkdir()
            os.symlink(unsafe_cache, home / "Library", target_is_directory=True)
            pdf_result = work / "structures.md"
            pdf_case, _ = run_case(
                "real-pdf-macos-var-fallback",
                binary,
                [
                    str(PDF_FIXTURE),
                    "-o",
                    str(pdf_result),
                    "--conflict",
                    "error",
                    "--no-config",
                    "--progress",
                    "never",
                ],
                work,
                environment,
            )
            cases.append(pdf_case)
            if not pdf_result.is_file() or not pdf_result.read_bytes():
                raise AcceptanceError("macOS /var fallback PDF output is missing or empty")
            assert_runtime_absent(cache, home, temporary, "macOS /var fallback cleanup")
            authority = PDFIUM_MANIFEST["targets"][target]
            pdfium_runtime = {
                "version": PDFIUM_MANIFEST["version"],
                "sha256": authority["library_sha256"],
                "bytes": authority["library_size"],
                "materialization": "canonical-var-fallback",
            }
        return {
            "schemaVersion": 1,
            "target": target,
            "artifactSha256": artifact_sha,
            "version": expected_version,
            "cases": cases,
            "plainTextOutputSha256": sha256_file(result),
            "pdfiumRuntime": pdfium_runtime,
            "negativeCases": negative_cases,
            "runtimeCacheCreated": False,
            "networkRequired": False,
            "conclusion": "pass",
        }


def write_json(path: pathlib.Path, value: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8", newline="\n")


def parse() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", required=True, choices=sorted(CORE_ARCHIVES))
    parser.add_argument("--output", required=True, type=pathlib.Path)
    parser.add_argument("--expected-version", required=True)
    return parser.parse_args()


def main() -> None:
    arguments = parse()
    output = arguments.output.resolve()
    audit, contents, member = audit_archive(output, arguments.target)
    expected_version = arguments.expected_version.removeprefix("v")
    if not expected_version:
        raise AcceptanceError("expected release version is empty")
    e2e = run_e2e(
        arguments.target, contents, member, audit["artifactSha256"], expected_version
    )
    import pdf_acceptance

    pdf_regression = pdf_acceptance.run(contents, member)
    evidence = output / "evidence" / arguments.target
    write_json(evidence / "native-audit.json", audit)
    write_json(evidence / "e2e.json", e2e)
    write_json(evidence / "pdf-regression.json", pdf_regression)


if __name__ == "__main__":
    try:
        main()
    except (AcceptanceError, OSError, subprocess.SubprocessError, zipfile.BadZipFile) as error:
        print(f"native-acceptance: {error}", file=sys.stderr)
        raise SystemExit(1)

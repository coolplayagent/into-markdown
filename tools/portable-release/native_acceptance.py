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


CORE_ARCHIVES = {
    "x86_64-pc-windows-msvc": ("into-md-windows-x86_64.zip", "into-md.exe", "PE", "x86_64"),
    "x86_64-unknown-linux-gnu": ("into-md-linux-x86_64.zip", "into-md", "ELF", "x86_64"),
    "aarch64-unknown-linux-gnu": ("into-md-linux-arm64.zip", "into-md", "ELF", "aarch64"),
    "aarch64-apple-darwin": ("into-md-macos-arm64.zip", "into-md", "Mach-O", "arm64"),
}
MAX_OUTPUT_BYTES = 64 * 1024


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


def audit_archive(output: pathlib.Path, target: str) -> tuple[dict, bytes, str]:
    archive_name, member, expected_format, expected_arch = CORE_ARCHIVES[target]
    archive_path = output / "release" / archive_name
    if not archive_path.is_file() or archive_path.is_symlink():
        raise AcceptanceError(f"Core archive is unavailable: {archive_name}")
    with zipfile.ZipFile(archive_path) as archive:
        infos = archive.infolist()
        if len(infos) != 1 or infos[0].filename != member or infos[0].is_dir():
            raise AcceptanceError("Core ZIP must contain exactly its direct-run binary")
        info = infos[0]
        mode = (info.external_attr >> 16) & 0o177777
        expected_mode = stat.S_IFREG | (0o644 if member.endswith(".exe") else 0o755)
        if mode != expected_mode:
            raise AcceptanceError("Core ZIP member mode is invalid")
        data = archive.read(info)
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
        "memberCount": 1,
        "binarySha256": sha256_bytes(data),
        "binaryBytes": len(data),
        "format": format_name,
        "architecture": architecture,
        "mode": f"{mode & 0o777:04o}",
        "conclusion": "pass",
    }
    return report, data, member


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


def run_e2e(
    target: str, data: bytes, member: str, artifact_sha: str, expected_version: str
) -> dict:
    with tempfile.TemporaryDirectory(prefix="into-md-native-e2e-") as name:
        root = pathlib.Path(name)
        binary = root / member
        binary.write_bytes(data)
        binary.chmod(0o700)
        home = root / "home"
        cache = root / "cache"
        temporary = root / "tmp"
        work = root / "work"
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
                "NO_PROXY": "*",
            }
        )
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
        cases = [help_case, version_case, text_case]
        if not result.is_file() or "portable release acceptance" not in result.read_text(encoding="utf-8"):
            raise AcceptanceError("plain-text conversion output is missing or invalid")
        runtime_roots = [
            cache / "into-markdown" / "runtime",
            home / ".cache" / "into-markdown" / "runtime",
            home / "Library" / "Caches" / "into-markdown" / "runtime",
        ]
        fallback = list(temporary.glob("into-markdown-runtime-*"))
        if any(path.exists() for path in runtime_roots) or fallback:
            raise AcceptanceError("help, version, or plain text conversion materialized native runtime")
        return {
            "schemaVersion": 1,
            "target": target,
            "artifactSha256": artifact_sha,
            "version": expected_version,
            "cases": cases,
            "plainTextOutputSha256": sha256_file(result),
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
    audit, data, member = audit_archive(output, arguments.target)
    expected_version = arguments.expected_version.removeprefix("v")
    if not expected_version:
        raise AcceptanceError("expected release version is empty")
    e2e = run_e2e(
        arguments.target, data, member, audit["artifactSha256"], expected_version
    )
    evidence = output / "evidence" / arguments.target
    write_json(evidence / "native-audit.json", audit)
    write_json(evidence / "e2e.json", e2e)


if __name__ == "__main__":
    try:
        main()
    except (AcceptanceError, OSError, subprocess.SubprocessError, zipfile.BadZipFile) as error:
        print(f"native-acceptance: {error}", file=sys.stderr)
        raise SystemExit(1)

"""Acquire and authenticate published Core and Skill release artifacts."""

from __future__ import annotations

import hashlib
import json
import os
import pathlib
import shutil
import stat
import struct
import urllib.request
import zipfile
from typing import Any

from core_archive import (
    CORE_ARCHIVE_MANIFEST,
    CORE_MATERIAL_MEMBERS,
    PDFIUM_LICENSE_FILES,
    WINDOWS_PDFIUM_MEMBER,
    archive_record,
)


ROOT = pathlib.Path(__file__).resolve().parents[2]
WINDOWS_SKILL_PDFIUM = (
    f"into-markdown/assets/windows-x86_64/{WINDOWS_PDFIUM_MEMBER}"
)
WINDOWS_PDFIUM_AUTHORITY = json.loads(
    (ROOT / "third_party/pdfium/manifest.json").read_text(encoding="utf-8")
)["targets"]["x86_64-pc-windows-msvc"]
TARGETS = {
    "windows": {
        "target": "x86_64-pc-windows-msvc",
        "core": "into-md-windows-x86_64.zip",
        "member": "into-md.exe",
        "speech": "official.media.whisper-x86_64-pc-windows-msvc.imp",
        "skill": "into-markdown/assets/windows-x86_64/into-md.exe",
    },
    "linux": {
        "target": "x86_64-unknown-linux-gnu",
        "core": "into-md-linux-x86_64.zip",
        "member": "into-md",
        "speech": "official.media.whisper-x86_64-unknown-linux-gnu.imp",
        "skill": "into-markdown/assets/linux-x86_64/into-md",
    },
}
SKILL_ARCHIVE = "into-markdown-skill.zip"
SKILL_MANIFEST = "into-markdown/archive-manifest.json"
SKILL_DIRECTORIES = (
    "into-markdown/",
    "into-markdown/agents/",
    "into-markdown/assets/",
    "into-markdown/assets/linux-arm64/",
    "into-markdown/assets/linux-x86_64/",
    "into-markdown/assets/windows-x86_64/",
    "into-markdown/assets/windows-x86_64/lib/",
    "into-markdown/assets/windows-x86_64/lib/pdfium/",
    "into-markdown/licenses/",
    "into-markdown/licenses/npm/",
    "into-markdown/licenses/pdfium/",
    "into-markdown/licenses/pdfium/licenses/",
    "into-markdown/references/",
)
SKILL_FILES = (
    "into-markdown/LICENSE",
    "into-markdown/NOTICE",
    "into-markdown/SBOM.spdx.json",
    "into-markdown/SKILL.md",
    "into-markdown/SOURCES.json",
    "into-markdown/THIRD_PARTY_NOTICES.md",
    "into-markdown/agents/openai.yaml",
    "into-markdown/assets/linux-arm64/into-md",
    "into-markdown/assets/linux-x86_64/into-md",
    "into-markdown/assets/windows-x86_64/into-md.exe",
    WINDOWS_SKILL_PDFIUM,
    *(
        f"into-markdown/{member}"
        for member in CORE_MATERIAL_MEMBERS
        if member
        not in {
            "LICENSE",
            "NOTICE",
            "THIRD_PARTY_NOTICES.md",
            "SBOM.spdx.json",
            "SOURCES.json",
        }
    ),
    "into-markdown/references/cli-workflows.md",
    SKILL_MANIFEST,
)


class E2EError(RuntimeError):
    """A published release artifact or black-box behavior is invalid."""


def sha256_file(path: pathlib.Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            value.update(chunk)
    return value.hexdigest()


def release_asset_url(repository: str, tag: str, name: str) -> str:
    if not repository or any(part in repository for part in ("..", "\\", "?", "#")):
        raise E2EError("repository must be an owner/name pair")
    if repository.count("/") != 1 or not tag or "/" in tag or "\\" in tag:
        raise E2EError("release repository or tag is invalid")
    return f"https://github.com/{repository}/releases/download/{tag}/{name}"


def acquire_assets(
    assets: pathlib.Path, repository: str, tag: str, platforms: list[str]
) -> dict[str, dict[str, Any]]:
    """Download missing release assets without replacing reviewed local files."""
    assets.mkdir(parents=True, exist_ok=True)
    required = {SKILL_ARCHIVE}
    for platform in platforms:
        required.update((TARGETS[platform]["core"], TARGETS[platform]["speech"]))
    records: dict[str, dict[str, Any]] = {}
    for name in sorted(required):
        destination = assets / name
        downloaded = False
        if not destination.is_file():
            temporary = assets / f".{name}.download"
            if temporary.exists():
                temporary.unlink()
            request = urllib.request.Request(
                release_asset_url(repository, tag, name),
                headers={"User-Agent": "into-markdown-post-release-e2e"},
            )
            try:
                with urllib.request.urlopen(request, timeout=120) as response, temporary.open(
                    "xb"
                ) as output:
                    shutil.copyfileobj(response, output, length=1024 * 1024)
                os.replace(temporary, destination)
                downloaded = True
            finally:
                temporary.unlink(missing_ok=True)
        if destination.is_symlink() or not destination.is_file():
            raise E2EError(f"release asset is not a regular file: {name}")
        records[name] = {
            "path": str(destination.resolve()),
            "bytes": destination.stat().st_size,
            "sha256": sha256_file(destination),
            "downloaded": downloaded,
        }
    return records


def inspect_core(data: bytes, platform: str) -> dict[str, str]:
    if platform == "windows":
        if len(data) < 64 or data[:2] != b"MZ":
            raise E2EError("Windows Core is not a PE executable")
        offset = struct.unpack_from("<I", data, 0x3C)[0]
        if offset + 6 > len(data) or data[offset : offset + 4] != b"PE\0\0":
            raise E2EError("Windows Core has an invalid PE header")
        if struct.unpack_from("<H", data, offset + 4)[0] != 0x8664:
            raise E2EError("Windows Core is not x86_64")
        return {"format": "PE", "architecture": "x86_64"}
    if len(data) < 20 or data[:7] != b"\x7fELF\x02\x01\x01":
        raise E2EError("Linux Core is not a 64-bit little-endian ELF executable")
    if struct.unpack_from("<H", data, 18)[0] != 62:
        raise E2EError("Linux Core is not x86_64")
    return {"format": "ELF", "architecture": "x86_64"}


def extract_single_core(
    archive_path: pathlib.Path,
    platform: str,
    output: pathlib.Path,
    pdfium_authority: dict | None = None,
) -> dict:
    """Authenticate and extract one platform's Core archive."""
    expected = TARGETS[platform]["member"]
    with zipfile.ZipFile(archive_path) as archive:
        infos = archive.infolist()
        wanted = [expected, *([WINDOWS_PDFIUM_MEMBER] if platform == "windows" else [])]
        wanted.extend((*CORE_MATERIAL_MEMBERS, CORE_ARCHIVE_MANIFEST))
        if [info.filename for info in infos] != wanted or any(
            info.is_dir() for info in infos
        ):
            raise E2EError(
                f"{archive_path.name} must contain exactly {', '.join(wanted)}"
            )
        info = infos[0]
        mode = (info.external_attr >> 16) & 0o177777
        expected_mode = stat.S_IFREG | (0o644 if platform == "windows" else 0o755)
        if mode != expected_mode:
            raise E2EError(f"{archive_path.name} has an invalid member mode")
        contents = {item.filename: archive.read(item) for item in infos}
        data = contents[info.filename]
        runtime_data = _verify_core_pdfium(
            archive_path,
            platform,
            infos,
            contents,
            pdfium_authority or WINDOWS_PDFIUM_AUTHORITY,
        )
        material_offset = 2 if platform == "windows" else 1
        for material in infos[material_offset:]:
            if (material.external_attr >> 16) & 0o177777 != stat.S_IFREG | 0o644:
                raise E2EError(
                    f"{archive_path.name} has an invalid license member mode"
                )
        if any(
            not contents[f"licenses/pdfium/{name}"] for name in PDFIUM_LICENSE_FILES
        ):
            raise E2EError(f"{archive_path.name} has an empty PDFium license member")
        _verify_core_manifest(archive_path, platform, infos, contents)
    identity = inspect_core(data, platform)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(data)
    output.chmod(0o700)
    runtime_report = _write_runtime(output, runtime_data)
    return {
        "archive": archive_path.name,
        "archiveSha256": sha256_file(archive_path),
        "binarySha256": hashlib.sha256(data).hexdigest(),
        "binaryBytes": len(data),
        "memberCount": len(infos),
        "pdfium": runtime_report,
        **identity,
    }


def _verify_core_pdfium(
    archive_path: pathlib.Path,
    platform: str,
    infos: list[zipfile.ZipInfo],
    contents: dict[str, bytes],
    authority: dict,
) -> bytes | None:
    if platform != "windows":
        return None
    runtime = infos[1]
    if (runtime.external_attr >> 16) & 0o177777 != stat.S_IFREG | 0o644:
        raise E2EError(f"{archive_path.name} has an invalid PDFium member mode")
    runtime_data = contents[WINDOWS_PDFIUM_MEMBER]
    if (
        len(runtime_data) != authority["library_size"]
        or hashlib.sha256(runtime_data).hexdigest()
        != authority["library_sha256"]
    ):
        raise E2EError(f"{archive_path.name} PDFium differs from the pinned manifest")
    return runtime_data


def _verify_core_manifest(
    archive_path: pathlib.Path,
    platform: str,
    infos: list[zipfile.ZipInfo],
    contents: dict[str, bytes],
) -> None:
    try:
        manifest = json.loads(contents[CORE_ARCHIVE_MANIFEST])
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise E2EError(f"{archive_path.name} manifest is invalid") from error
    observed = [
        archive_record(
            item.filename,
            contents[item.filename],
            (item.external_attr >> 16) & 0o777,
        )
        for item in infos[:-1]
    ]
    if (
        not isinstance(manifest, dict)
        or set(manifest) != {"schemaVersion", "target", "files"}
        or manifest.get("schemaVersion") != 1
        or manifest.get("target") != TARGETS[platform]["target"]
        or manifest.get("files") != observed
    ):
        raise E2EError(f"{archive_path.name} differs from its bidirectional manifest")


def extract_skill_binary(
    archive_path: pathlib.Path,
    platform: str,
    output: pathlib.Path,
    pdfium_authority: dict | None = None,
) -> dict:
    """Authenticate the full Skill inventory and extract its platform Core."""
    wanted = TARGETS[platform]["skill"]
    with zipfile.ZipFile(archive_path) as archive:
        infos = archive.infolist()
        names = [info.filename for info in infos]
        expected_names = [
            "into-markdown/",
            *sorted(set(SKILL_DIRECTORIES[1:]) | set(SKILL_FILES)),
        ]
        if names != expected_names or len(names) != len(set(names)):
            raise E2EError("Skill does not contain the exact reviewed archive inventory")
        contents = {
            info.filename: archive.read(info) for info in infos if not info.is_dir()
        }
        _verify_skill_metadata(archive, infos)
        _verify_skill_materials(contents)
        _verify_skill_manifest(infos, contents)
        info = archive.getinfo(wanted)
        if info.is_dir():
            raise E2EError("Skill Core asset is a directory")
        data = archive.read(info)
        runtime_data = _verify_skill_pdfium(
            archive, platform, pdfium_authority or WINDOWS_PDFIUM_AUTHORITY
        )
    identity = inspect_core(data, platform)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(data)
    output.chmod(0o700)
    _write_runtime(output, runtime_data)
    return {
        "archiveSha256": sha256_file(archive_path),
        "binarySha256": hashlib.sha256(data).hexdigest(),
        "asset": wanted,
        **identity,
    }


def _verify_skill_metadata(
    archive: zipfile.ZipFile, infos: list[zipfile.ZipInfo]
) -> None:
    executable_members = {
        "into-markdown/assets/linux-arm64/into-md",
        "into-markdown/assets/linux-x86_64/into-md",
    }
    for info in infos:
        mode = (info.external_attr >> 16) & 0o177777
        if info.is_dir():
            if mode != stat.S_IFDIR | 0o755 or archive.read(info):
                raise E2EError("Skill contains invalid directory metadata")
        elif mode != stat.S_IFREG | (
            0o755 if info.filename in executable_members else 0o644
        ):
            raise E2EError("Skill contains invalid file metadata")


def _verify_skill_materials(contents: dict[str, bytes]) -> None:
    if contents["into-markdown/LICENSE"] != (ROOT / "LICENSE").read_bytes():
        raise E2EError("Skill project LICENSE differs from the repository authority")
    for relative, authority in (
        (
            "licenses/npm/npm-release.spdx.json",
            ROOT / "third_party/licenses/npm-release.spdx.json",
        ),
        (
            "licenses/npm/lucide-ISC-MIT.txt",
            ROOT / "third_party/licenses/npm/lucide-ISC-MIT.txt",
        ),
        (
            "licenses/npm/react-MIT.txt",
            ROOT / "third_party/licenses/npm/react-MIT.txt",
        ),
    ):
        if contents[f"into-markdown/{relative}"] != authority.read_bytes():
            raise E2EError(f"Skill {relative} differs from the repository authority")
    if any(
        not contents[f"into-markdown/licenses/pdfium/{name}"]
        for name in PDFIUM_LICENSE_FILES
    ):
        raise E2EError("Skill has an empty PDFium license member")


def _skill_record(info: zipfile.ZipInfo, data: bytes) -> dict[str, object]:
    relative = info.filename.removeprefix("into-markdown/")
    record: dict[str, object] = {
        "path": relative,
        "bytes": len(data),
        "sha256": hashlib.sha256(data).hexdigest(),
        "mode": f"{((info.external_attr >> 16) & 0o7777):04o}",
        "kind": (
            "component"
            if info.filename == WINDOWS_SKILL_PDFIUM
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
    }
    if info.filename == WINDOWS_SKILL_PDFIUM or relative.startswith("licenses/pdfium/"):
        record["componentId"] = "pdfium"
    return record


def _verify_skill_manifest(
    infos: list[zipfile.ZipInfo], contents: dict[str, bytes]
) -> None:
    observed = [
        _skill_record(info, contents[info.filename])
        for info in infos
        if not info.is_dir() and info.filename != SKILL_MANIFEST
    ]
    try:
        manifest = json.loads(contents[SKILL_MANIFEST])
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise E2EError("Skill archive manifest is invalid") from error
    if manifest != {"schemaVersion": 1, "files": observed}:
        raise E2EError("Skill differs from its bidirectional manifest")


def _verify_skill_pdfium(
    archive: zipfile.ZipFile, platform: str, authority: dict
) -> bytes | None:
    if platform != "windows":
        return None
    runtime = archive.getinfo(WINDOWS_SKILL_PDFIUM)
    if (
        runtime.is_dir()
        or (runtime.external_attr >> 16) & 0o177777 != stat.S_IFREG | 0o644
    ):
        raise E2EError("Skill PDFium asset is not a regular 0644 file")
    runtime_data = archive.read(runtime)
    if (
        len(runtime_data) != authority["library_size"]
        or hashlib.sha256(runtime_data).hexdigest()
        != authority["library_sha256"]
    ):
        raise E2EError("Skill PDFium differs from the pinned manifest")
    return runtime_data


def _write_runtime(output: pathlib.Path, runtime_data: bytes | None) -> dict | None:
    if runtime_data is None:
        return None
    runtime_output = output.parent / WINDOWS_PDFIUM_MEMBER
    runtime_output.parent.mkdir(parents=True, exist_ok=True)
    runtime_output.write_bytes(runtime_data)
    runtime_output.chmod(0o600)
    return {
        "asset": WINDOWS_PDFIUM_MEMBER,
        "bytes": len(runtime_data),
        "sha256": hashlib.sha256(runtime_data).hexdigest(),
    }

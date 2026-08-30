"""Acquire and authenticate published Core and Skill release artifacts."""

from __future__ import annotations

import hashlib
import json
import os
import pathlib
import stat
import struct
import urllib.request
import zipfile
from dataclasses import dataclass
from typing import Any
from urllib.parse import urlsplit

from core_archive import (
    CORE_ARCHIVE_MANIFEST,
    CORE_MATERIAL_AUTHORITY,
    CORE_MATERIAL_MEMBERS,
    WINDOWS_PDFIUM_MEMBER,
    MaterialAuthorityError,
    load_authority,
    verify_materials,
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
SKILL_AUTHORITY = "into-markdown-skill-authority.json"
SKILL_TARGETS = (
    ("aarch64-unknown-linux-gnu", "assets/linux-arm64/into-md"),
    ("x86_64-pc-windows-msvc", "assets/windows-x86_64/into-md.exe"),
    ("x86_64-unknown-linux-gnu", "assets/linux-x86_64/into-md"),
)
SKILL_DIRECTORIES = (
    "into-markdown/",
    "into-markdown/agents/",
    "into-markdown/assets/",
    "into-markdown/assets/linux-arm64/",
    "into-markdown/assets/linux-x86_64/",
    "into-markdown/assets/windows-x86_64/",
    "into-markdown/assets/windows-x86_64/lib/",
    "into-markdown/assets/windows-x86_64/lib/pdfium/",
    "into-markdown/evidence/",
    *(
        name
        for target, _asset in SKILL_TARGETS
        for name in (
            f"into-markdown/evidence/{target}/",
            f"into-markdown/evidence/{target}/licenses/",
            f"into-markdown/evidence/{target}/licenses/npm/",
            f"into-markdown/evidence/{target}/licenses/pdfium/",
            f"into-markdown/evidence/{target}/licenses/pdfium/licenses/",
        )
    ),
    "into-markdown/references/",
)
SKILL_FILES = (
    "into-markdown/LICENSE",
    "into-markdown/SKILL.md",
    "into-markdown/agents/openai.yaml",
    "into-markdown/assets/linux-arm64/into-md",
    "into-markdown/assets/linux-x86_64/into-md",
    "into-markdown/assets/windows-x86_64/into-md.exe",
    WINDOWS_SKILL_PDFIUM,
    *(
        f"into-markdown/evidence/{target}/{member}"
        for target, _asset in SKILL_TARGETS
        for member in CORE_MATERIAL_MEMBERS
    ),
    "into-markdown/references/cli-workflows.md",
    SKILL_MANIFEST,
)


class E2EError(RuntimeError):
    """A published release artifact or black-box behavior is invalid."""


MAX_ASSET_BYTES = 768 * 1024 * 1024
MAX_ARCHIVE_ENTRIES = 512
MAX_CENTRAL_DIRECTORY_BYTES = 4 * 1024 * 1024
MAX_MEMBER_BYTES = 512 * 1024 * 1024
MAX_ARCHIVE_UNCOMPRESSED_BYTES = 1024 * 1024 * 1024
MAX_COMPRESSION_RATIO = 200


@dataclass
class ResourceBudget:
    """Invocation-shared limits for attacker-controlled transport and archives."""

    max_download_bytes: int = 2 * 1024 * 1024 * 1024
    max_entries: int = 2048
    max_compressed_bytes: int = 2 * 1024 * 1024 * 1024
    max_uncompressed_bytes: int = 2 * 1024 * 1024 * 1024
    max_temp_bytes: int = 2 * 1024 * 1024 * 1024
    download_bytes: int = 0
    entries: int = 0
    compressed_bytes: int = 0
    uncompressed_bytes: int = 0
    temp_bytes: int = 0

    def charge(self, field: str, amount: int, limit_field: str) -> None:
        if amount < 0:
            raise E2EError("resource budget received a negative charge")
        value = getattr(self, field) + amount
        if value > getattr(self, limit_field):
            raise E2EError(f"invocation {field.replace('_', ' ')} budget exceeded")
        setattr(self, field, value)


def _preflight_zip(path: pathlib.Path, budget: ResourceBudget) -> list[zipfile.ZipInfo]:
    size = path.stat().st_size
    if size < 22 or size > MAX_ASSET_BYTES:
        raise E2EError(f"ZIP size is outside the release contract: {path.name}")
    tail_size = min(size, 65_557)
    with path.open("rb") as source:
        source.seek(size - tail_size)
        tail = source.read(tail_size)
    marker = tail.rfind(b"PK\x05\x06")
    if marker < 0 or marker + 22 > len(tail):
        raise E2EError(f"ZIP EOCD is missing or outside the bounded search: {path.name}")
    fields = struct.unpack_from("<4s4H2LH", tail, marker)
    _signature, disk, central_disk, disk_entries, total_entries, central_size, central_offset, comment_size = fields
    if marker + 22 + comment_size != len(tail) or disk or central_disk or disk_entries != total_entries:
        raise E2EError(f"ZIP EOCD is multi-disk or malformed: {path.name}")
    if total_entries == 0xFFFF or central_size == 0xFFFFFFFF or central_offset == 0xFFFFFFFF:
        locator_offset = marker - 20
        if locator_offset < 0 or tail[locator_offset : locator_offset + 4] != b"PK\x06\x07":
            raise E2EError(f"ZIP64 locator is malformed: {path.name}")
        raise E2EError(f"ZIP64 is outside the bounded release contract: {path.name}")
    if (
        total_entries > MAX_ARCHIVE_ENTRIES
        or central_size > MAX_CENTRAL_DIRECTORY_BYTES
        or central_offset + central_size > size - (len(tail) - marker)
    ):
        raise E2EError(f"ZIP central directory exceeds the release contract: {path.name}")
    budget.charge("entries", total_entries, "max_entries")
    with zipfile.ZipFile(path) as archive:
        infos = archive.infolist()
    if len(infos) != total_entries:
        raise E2EError(f"ZIP central directory entry count is inconsistent: {path.name}")
    total_uncompressed = 0
    total_compressed = 0
    for info in infos:
        if info.file_size > MAX_MEMBER_BYTES:
            raise E2EError(f"ZIP member exceeds the release contract: {info.filename}")
        total_uncompressed += info.file_size
        total_compressed += info.compress_size
        if not info.is_dir() and info.file_size > max(1, info.compress_size) * MAX_COMPRESSION_RATIO:
            raise E2EError(f"ZIP member compression ratio exceeds the release contract: {info.filename}")
    if total_uncompressed > MAX_ARCHIVE_UNCOMPRESSED_BYTES:
        raise E2EError(f"ZIP uncompressed size exceeds the release contract: {path.name}")
    budget.charge("compressed_bytes", total_compressed, "max_compressed_bytes")
    budget.charge("uncompressed_bytes", total_uncompressed, "max_uncompressed_bytes")
    if budget.uncompressed_bytes > max(1, budget.compressed_bytes) * MAX_COMPRESSION_RATIO:
        raise E2EError("invocation ZIP compression ratio budget exceeded")
    return infos


def _stream_member(
    archive: zipfile.ZipFile,
    info: zipfile.ZipInfo,
    *,
    output: pathlib.Path | None = None,
    capture_limit: int | None = 16 * 1024 * 1024,
) -> tuple[bytes, int, str, bytes | None]:
    digest = hashlib.sha256()
    size = 0
    header = bytearray()
    captured = bytearray() if output is None and capture_limit is not None else None
    destination = output.open("xb") if output is not None else None
    try:
        with archive.open(info) as source:
            while chunk := source.read(1024 * 1024):
                size += len(chunk)
                if size > info.file_size:
                    raise E2EError(f"ZIP member exceeded its declared size: {info.filename}")
                digest.update(chunk)
                if len(header) < 4096:
                    header.extend(chunk[: 4096 - len(header)])
                if destination is not None:
                    destination.write(chunk)
                elif captured is not None:
                    if capture_limit is not None and size > capture_limit:
                        raise E2EError(f"ZIP metadata member is too large: {info.filename}")
                    captured.extend(chunk)
    finally:
        if destination is not None:
            destination.close()
    if size != info.file_size:
        raise E2EError(f"ZIP member size differs from its header: {info.filename}")
    return bytes(header), size, digest.hexdigest(), bytes(captured) if captured is not None else None


def sha256_file(path: pathlib.Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            value.update(chunk)
    return value.hexdigest()


def write_json(path: pathlib.Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
        newline="\n",
    )


def release_asset_url(repository: str, tag: str, name: str) -> str:
    if not repository or any(part in repository for part in ("..", "\\", "?", "#")):
        raise E2EError("repository must be an owner/name pair")
    if repository.count("/") != 1 or not tag or "/" in tag or "\\" in tag:
        raise E2EError("release repository or tag is invalid")
    return f"https://github.com/{repository}/releases/download/{tag}/{name}"


def normalize_release_version(value: str) -> str:
    """Normalize the accepted workflow tag spelling to the Cargo version spelling."""
    normalized = value.removeprefix("v")
    if not normalized or normalized.startswith("v"):
        raise E2EError("release version is invalid")
    return normalized


def load_release_file_authority(
    evidence: pathlib.Path, platforms: list[str]
) -> dict[str, dict[str, object]]:
    """Load the independently generated per-target archive/download records."""
    records: dict[str, dict[str, object]] = {}
    for platform in platforms:
        target = TARGETS[platform]["target"]
        path = evidence / target / "release-files.json"
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
            raise E2EError(f"release file authority is unreadable: {target}") from error
        expected_names = sorted((TARGETS[platform]["core"], TARGETS[platform]["speech"]))
        if (
            not isinstance(value, dict)
            or set(value) != {"schemaVersion", "target", "files"}
            or value.get("schemaVersion") != 1
            or value.get("target") != target
            or not isinstance(value.get("files"), list)
            or [item.get("name") for item in value["files"] if isinstance(item, dict)] != expected_names
        ):
            raise E2EError(f"release file authority schema is invalid: {target}")
        for record in value["files"]:
            if (
                set(record) != {"name", "bytes", "sha256"}
                or not isinstance(record["bytes"], int)
                or isinstance(record["bytes"], bool)
                or record["bytes"] <= 0
                or not isinstance(record["sha256"], str)
                or len(record["sha256"]) != 64
                or any(character not in "0123456789abcdef" for character in record["sha256"])
                or record["name"] in records
            ):
                raise E2EError("release file authority contains an invalid or duplicate record")
            records[record["name"]] = {
                "bytes": record["bytes"],
                "sha256": record["sha256"],
            }
    return records


def acquire_assets(
    assets: pathlib.Path,
    repository: str,
    tag: str,
    platforms: list[str],
    budget: ResourceBudget | None = None,
    expected_assets: dict[str, dict[str, object]] | None = None,
) -> dict[str, dict[str, Any]]:
    """Download missing release assets without replacing reviewed local files."""
    assets.mkdir(parents=True, exist_ok=True)
    budget = budget or ResourceBudget()
    required = {SKILL_ARCHIVE}
    for platform in platforms:
        required.update((TARGETS[platform]["core"], TARGETS[platform]["speech"]))
    records: dict[str, dict[str, Any]] = {}
    for name in sorted(required):
        destination = assets / name
        downloaded = False
        digest_value: str | None = None
        observed_size: int | None = None
        if not destination.is_file():
            temporary = assets / f".{name}.download"
            if temporary.exists():
                temporary.unlink()
            request = urllib.request.Request(
                release_asset_url(repository, tag, name),
                headers={"User-Agent": "into-markdown-post-release-e2e"},
            )
            try:
                with urllib.request.urlopen(request, timeout=120) as response, temporary.open("xb") as output:
                    final_url = response.geturl()
                    if urlsplit(final_url).scheme.lower() != "https":
                        raise E2EError(f"release asset redirect is not HTTPS: {name}")
                    declared_value = response.headers.get("Content-Length")
                    try:
                        declared = int(declared_value) if declared_value is not None else -1
                    except ValueError as error:
                        raise E2EError(f"release asset Content-Length is invalid: {name}") from error
                    if declared < 0 or declared > MAX_ASSET_BYTES:
                        raise E2EError(f"release asset Content-Length is outside the contract: {name}")
                    digest = hashlib.sha256()
                    observed_size = 0
                    while chunk := response.read(1024 * 1024):
                        observed_size += len(chunk)
                        if observed_size > declared or observed_size > MAX_ASSET_BYTES:
                            raise E2EError(f"release asset exceeded its declared length: {name}")
                        budget.charge("download_bytes", len(chunk), "max_download_bytes")
                        budget.charge("temp_bytes", len(chunk), "max_temp_bytes")
                        digest.update(chunk)
                        output.write(chunk)
                    if observed_size != declared:
                        raise E2EError(f"release asset length differs from Content-Length: {name}")
                    digest_value = digest.hexdigest()
                os.replace(temporary, destination)
                downloaded = True
            finally:
                temporary.unlink(missing_ok=True)
        if destination.is_symlink() or not destination.is_file():
            raise E2EError(f"release asset is not a regular file: {name}")
        if not downloaded:
            observed_size = destination.stat().st_size
            if observed_size > MAX_ASSET_BYTES:
                raise E2EError(f"release asset exceeds the size contract: {name}")
            budget.charge("download_bytes", observed_size, "max_download_bytes")
            digest_value = sha256_file(destination)
        records[name] = {
            "path": str(destination.resolve()),
            "bytes": observed_size,
            "sha256": digest_value,
            "downloaded": downloaded,
        }
        if expected_assets is not None and name != SKILL_ARCHIVE:
            if expected_assets.get(name) != {
                "bytes": observed_size,
                "sha256": digest_value,
            }:
                raise E2EError(f"release asset differs from independent authority: {name}")
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
    expected_machine = 183 if platform == "linux-arm64" else 62
    if struct.unpack_from("<H", data, 18)[0] != expected_machine:
        raise E2EError("Linux Core has the wrong architecture")
    return {
        "format": "ELF",
        "architecture": "arm64" if platform == "linux-arm64" else "x86_64",
    }


def extract_single_core(
    archive_path: pathlib.Path,
    platform: str,
    output: pathlib.Path,
    material_authority: dict,
    pdfium_authority: dict | None = None,
    budget: ResourceBudget | None = None,
    archive_sha256: str | None = None,
) -> dict:
    """Authenticate and extract one platform's Core archive."""
    budget = budget or ResourceBudget()
    expected = TARGETS[platform]["member"]
    infos = _preflight_zip(archive_path, budget)
    binary_temporary = output.with_name(f".{output.name}.authenticated")
    runtime_output = output.parent / WINDOWS_PDFIUM_MEMBER
    runtime_temporary = runtime_output.with_name(f".{runtime_output.name}.authenticated")
    for path in (binary_temporary, runtime_temporary):
        path.unlink(missing_ok=True)
    try:
        with zipfile.ZipFile(archive_path) as archive:
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
            output.parent.mkdir(parents=True, exist_ok=True)
            header, binary_size, binary_digest, _ = _stream_member(
                archive, info, output=binary_temporary
            )
            budget.charge("temp_bytes", binary_size, "max_temp_bytes")
            observed_records = [
                _core_record_from_digest(info.filename, binary_size, binary_digest, mode & 0o777)
            ]
            runtime_report = None
            material_offset = 1
            if platform == "windows":
                runtime = infos[1]
                runtime_mode = (runtime.external_attr >> 16) & 0o177777
                if runtime_mode != stat.S_IFREG | 0o644:
                    raise E2EError(f"{archive_path.name} has an invalid PDFium member mode")
                runtime_temporary.parent.mkdir(parents=True, exist_ok=True)
                _runtime_header, runtime_size, runtime_digest, _ = _stream_member(
                    archive, runtime, output=runtime_temporary
                )
                budget.charge("temp_bytes", runtime_size, "max_temp_bytes")
                authority = pdfium_authority or WINDOWS_PDFIUM_AUTHORITY
                if runtime_size != authority["library_size"] or runtime_digest != authority["library_sha256"]:
                    raise E2EError(f"{archive_path.name} PDFium differs from the pinned manifest")
                runtime_report = {
                    "asset": WINDOWS_PDFIUM_MEMBER,
                    "bytes": runtime_size,
                    "sha256": runtime_digest,
                }
                observed_records.append(
                    _core_record_from_digest(
                        runtime.filename, runtime_size, runtime_digest, runtime_mode & 0o777
                    )
                )
                material_offset = 2
            materials: dict[str, bytes] = {}
            manifest: bytes | None = None
            for item in infos[material_offset:]:
                item_mode = (item.external_attr >> 16) & 0o177777
                if item_mode != stat.S_IFREG | 0o644:
                    raise E2EError(f"{archive_path.name} has an invalid license member mode")
                _header, item_size, item_digest, data = _stream_member(archive, item)
                assert data is not None
                if item.filename == CORE_ARCHIVE_MANIFEST:
                    manifest = data
                else:
                    materials[item.filename] = data
                    observed_records.append(
                        _core_record_from_digest(
                            item.filename, item_size, item_digest, item_mode & 0o777
                        )
                    )
            _verify_release_materials(materials, material_authority, CORE_MATERIAL_MEMBERS)
            _verify_core_manifest_records(archive_path, platform, manifest, observed_records)
        identity = inspect_core(header, platform)
        binary_temporary.chmod(0o700)
        os.replace(binary_temporary, output)
        if platform == "windows":
            runtime_output.parent.mkdir(parents=True, exist_ok=True)
            runtime_temporary.chmod(0o600)
            os.replace(runtime_temporary, runtime_output)
    finally:
        binary_temporary.unlink(missing_ok=True)
        runtime_temporary.unlink(missing_ok=True)
    return {
        "archive": archive_path.name,
        "archiveSha256": archive_sha256 or sha256_file(archive_path),
        "binarySha256": binary_digest,
        "binaryBytes": binary_size,
        "memberCount": len(infos),
        "pdfium": runtime_report,
        **identity,
    }


def _core_record_from_digest(name: str, size: int, digest: str, mode: int) -> dict[str, object]:
    record: dict[str, object] = {
        "path": name,
        "bytes": size,
        "sha256": digest,
        "mode": f"{mode:04o}",
        "kind": (
            "component"
            if name == WINDOWS_PDFIUM_MEMBER
            else "license-material"
            if name.startswith("licenses/")
            else "declaration"
            if name in {"LICENSE", "NOTICE"}
            else "generated"
            if name in {"THIRD_PARTY_NOTICES.md", "SBOM.spdx.json", "SOURCES.json"}
            else "project"
        ),
    }
    if name == WINDOWS_PDFIUM_MEMBER or name.startswith("licenses/pdfium/"):
        record["componentId"] = "pdfium"
    return record


def _verify_core_manifest_records(
    archive_path: pathlib.Path,
    platform: str,
    data: bytes | None,
    observed: list[dict[str, object]],
) -> None:
    try:
        manifest = json.loads(data) if data is not None else None
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise E2EError(f"{archive_path.name} manifest is invalid") from error
    if manifest != {
        "schemaVersion": 1,
        "target": TARGETS[platform]["target"],
        "files": observed,
    }:
        raise E2EError(f"{archive_path.name} differs from its bidirectional manifest")


def extract_skill_binary(
    archive_path: pathlib.Path,
    platform: str,
    output: pathlib.Path,
    material_authority: dict,
    pdfium_authority: dict | None = None,
    budget: ResourceBudget | None = None,
    archive_sha256: str | None = None,
) -> dict:
    """Authenticate the full Skill inventory and extract its platform Core."""
    budget = budget or ResourceBudget()
    wanted = TARGETS[platform]["skill"]
    infos = _preflight_zip(archive_path, budget)
    temporary = output.with_name(f".{output.name}.authenticated")
    runtime_output = output.parent / WINDOWS_PDFIUM_MEMBER
    runtime_temporary = runtime_output.with_name(f".{runtime_output.name}.authenticated")
    for path in (temporary, runtime_temporary):
        path.unlink(missing_ok=True)
    try:
        names = [info.filename for info in infos]
        expected_names = [
            "into-markdown/",
            *sorted(set(SKILL_DIRECTORIES[1:]) | set(SKILL_FILES)),
        ]
        if names != expected_names or len(names) != len(set(names)):
            raise E2EError("Skill does not contain the exact reviewed archive inventory")
        executable_members = {
            "into-markdown/assets/linux-arm64/into-md",
            "into-markdown/assets/linux-x86_64/into-md",
        }
        observed: dict[str, dict[str, object]] = {}
        contents: dict[str, bytes] = {}
        selected_header = b""
        selected_digest = ""
        with zipfile.ZipFile(archive_path) as archive:
            for info in infos:
                mode = (info.external_attr >> 16) & 0o177777
                if info.is_dir():
                    if mode != stat.S_IFDIR | 0o755 or info.file_size != 0:
                        raise E2EError("Skill contains invalid directory metadata")
                    continue
                expected_mode = stat.S_IFREG | (
                    0o755 if info.filename in executable_members else 0o644
                )
                if mode != expected_mode:
                    raise E2EError("Skill contains invalid file metadata")
                is_large = info.filename.startswith("into-markdown/assets/")
                destination = (
                    temporary
                    if info.filename == wanted
                    else runtime_temporary
                    if platform == "windows" and info.filename == WINDOWS_SKILL_PDFIUM
                    else None
                )
                if destination is not None:
                    destination.parent.mkdir(parents=True, exist_ok=True)
                header, size, digest, data = _stream_member(
                    archive,
                    info,
                    output=destination,
                    capture_limit=None if is_large and destination is None else 16 * 1024 * 1024,
                )
                if destination is not None:
                    budget.charge("temp_bytes", size, "max_temp_bytes")
                observed[info.filename] = {"bytes": size, "sha256": digest}
                if data is not None:
                    contents[info.filename] = data
                if info.filename == wanted:
                    selected_header = header
                    selected_digest = digest
                for target, asset in SKILL_TARGETS:
                    if info.filename == f"into-markdown/{asset}":
                        inspect_core(
                            header,
                            "windows"
                            if target.endswith("windows-msvc")
                            else "linux-arm64"
                            if target.startswith("aarch64")
                            else "linux",
                        )
                if info.filename == WINDOWS_SKILL_PDFIUM:
                    authority = pdfium_authority or WINDOWS_PDFIUM_AUTHORITY
                    if size != authority["library_size"] or digest != authority["library_sha256"]:
                        raise E2EError("Skill PDFium differs from the pinned manifest")
            _verify_skill_materials(contents, observed, material_authority)
            _verify_skill_manifest_records(infos, contents.get(SKILL_MANIFEST), observed)
        identity = inspect_core(selected_header, platform)
        output.parent.mkdir(parents=True, exist_ok=True)
        temporary.chmod(0o700)
        os.replace(temporary, output)
        if platform == "windows":
            runtime_output.parent.mkdir(parents=True, exist_ok=True)
            runtime_temporary.chmod(0o600)
            os.replace(runtime_temporary, runtime_output)
    finally:
        temporary.unlink(missing_ok=True)
        runtime_temporary.unlink(missing_ok=True)
    return {
        "archiveSha256": archive_sha256 or sha256_file(archive_path),
        "binarySha256": selected_digest,
        "asset": wanted,
        **identity,
    }


def _verify_skill_materials(
    contents: dict[str, bytes],
    observed: dict[str, dict[str, object]],
    material_authority: dict,
) -> None:
    if contents["into-markdown/LICENSE"] != (ROOT / "LICENSE").read_bytes():
        raise E2EError("Skill project LICENSE differs from the repository authority")
    if (
        not isinstance(material_authority, dict)
        or material_authority.get("namespace") != "into-markdown/skill-release-authority"
        or material_authority.get("artifact") != "into-markdown"
        or material_authority.get("schemaVersion") != 1
    ):
        raise E2EError("Skill authority namespace or schema is invalid")
    targets = material_authority.get("targets")
    if not isinstance(targets, list) or [item.get("target") for item in targets] != [
        target for target, _asset in SKILL_TARGETS
    ]:
        raise E2EError("Skill authority targets are missing, duplicated, or unsorted")
    target_assets = dict(SKILL_TARGETS)
    for target in targets:
        if set(target) != {"target", "binary", "materials"}:
            raise E2EError("Skill authority target schema is invalid")
        records = target["materials"]
        if [record.get("path") for record in records] != sorted(CORE_MATERIAL_MEMBERS):
            raise E2EError("Skill authority materials are missing, duplicated, or unsorted")
        _verify_skill_authority_record(
            observed,
            target["binary"],
            f"into-markdown/{target_assets[target['target']]}",
        )
        for record in records:
            _verify_skill_authority_record(
                observed,
                record,
                f"into-markdown/evidence/{target['target']}/{record['path']}",
            )


def _verify_skill_authority_record(
    observed: dict[str, dict[str, object]], record: object, archive_name: str
) -> None:
    if (
        not isinstance(record, dict)
        or set(record) != {"path", "bytes", "sha256"}
        or not isinstance(record.get("bytes"), int)
        or isinstance(record.get("bytes"), bool)
        or record["bytes"] <= 0
        or not isinstance(record.get("sha256"), str)
        or len(record["sha256"]) != 64
        or any(character not in "0123456789abcdef" for character in record["sha256"])
    ):
        raise E2EError("Skill authority contains an invalid file record")
    value = observed.get(archive_name)
    if value != {"bytes": record["bytes"], "sha256": record["sha256"]}:
        raise E2EError(f"Skill member differs from independent authority: {archive_name}")


def _verify_release_materials(
    contents: dict[str, bytes], authority: dict, members: tuple[str, ...]
) -> None:
    try:
        verify_materials(contents, authority, members)
    except MaterialAuthorityError as error:
        raise E2EError(str(error)) from error


def _skill_record_from_observed(
    info: zipfile.ZipInfo, observed: dict[str, object]
) -> dict[str, object]:
    relative = info.filename.removeprefix("into-markdown/")
    record: dict[str, object] = {
        "path": relative,
        "bytes": observed["bytes"],
        "sha256": observed["sha256"],
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
            else "target-evidence"
            if relative.startswith("evidence/")
            else "skill-source"
        ),
    }
    if info.filename == WINDOWS_SKILL_PDFIUM or "/licenses/pdfium/" in relative:
        record["componentId"] = "pdfium"
    return record


def _verify_skill_manifest_records(
    infos: list[zipfile.ZipInfo],
    data: bytes | None,
    observed: dict[str, dict[str, object]],
) -> None:
    records = [
        _skill_record_from_observed(info, observed[info.filename])
        for info in infos
        if not info.is_dir() and info.filename != SKILL_MANIFEST
    ]
    try:
        manifest = json.loads(data) if data is not None else None
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise E2EError("Skill archive manifest is invalid") from error
    if manifest != {"schemaVersion": 1, "files": records}:
        raise E2EError("Skill differs from its bidirectional manifest")

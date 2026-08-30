"""Create and authenticate the self-contained Core release archives."""

from __future__ import annotations

import hashlib
import json
import pathlib
import stat
import struct
import tarfile
import zipfile
import zlib
from collections.abc import Callable


ROOT = pathlib.Path(__file__).resolve().parents[2]
CORE_ARCHIVES = {
    "x86_64-pc-windows-msvc": ("into-md-windows-x86_64.zip", "into-md.exe"),
    "x86_64-unknown-linux-gnu": ("into-md-linux-x86_64.zip", "into-md"),
    "aarch64-unknown-linux-gnu": ("into-md-linux-arm64.zip", "into-md"),
    "aarch64-apple-darwin": ("into-md-macos-arm64.zip", "into-md"),
}
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
    *(f"licenses/pdfium/{path}" for path in PDFIUM_LICENSE_FILES),
)
ARCHIVE_TIMESTAMP = (2026, 1, 1, 0, 0, 0)


class PortableReleaseError(RuntimeError):
    """The compact release could not be assembled or verified."""


def archive_record(name: str, data: bytes, mode: int) -> dict[str, object]:
    """Return the manifest authority for one authenticated archive member."""
    record: dict[str, object] = {
        "path": name,
        "bytes": len(data),
        "sha256": hashlib.sha256(data).hexdigest(),
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


def create_core_archive(
    binary: pathlib.Path,
    destination: pathlib.Path,
    member: str,
    runtime: tuple[pathlib.Path, str] | None = None,
    materials: dict[str, pathlib.Path] | None = None,
    target: str | None = None,
) -> None:
    """Create a deterministic Core archive and its bidirectional manifest."""
    if not binary.is_file() or binary.is_symlink():
        raise PortableReleaseError("final Core binary is unavailable")
    if runtime is not None and (
        not runtime[0].is_file() or runtime[0].is_symlink() or runtime[1] == member
    ):
        raise PortableReleaseError("final Core runtime is unavailable")
    if destination.exists() or destination.is_symlink():
        raise PortableReleaseError("Core archive destination already exists")
    materials = materials or {}
    if materials and tuple(materials) != CORE_MATERIAL_MEMBERS:
        raise PortableReleaseError("Core archive material inventory is invalid")
    target = target or (
        "x86_64-pc-windows-msvc"
        if member.endswith(".exe")
        else "x86_64-unknown-linux-gnu"
    )
    entries = [(member, binary, 0o755 if member == "into-md" else 0o644)]
    if runtime is not None:
        entries.append((runtime[1], runtime[0], 0o644))
    entries.extend(
        (name, materials[name], 0o644)
        for name in CORE_MATERIAL_MEMBERS
        if name in materials
    )
    for name, source, _mode in entries:
        if not source.is_file() or source.is_symlink():
            raise PortableReleaseError(f"Core archive member is unavailable: {name}")
    entry_data = [(name, source.read_bytes(), mode) for name, source, mode in entries]
    manifest = (
        json.dumps(
            {
                "schemaVersion": 1,
                "target": target,
                "files": [archive_record(name, data, mode) for name, data, mode in entry_data],
            },
            ensure_ascii=False,
            indent=2,
            sort_keys=True,
        )
        + "\n"
    ).encode("utf-8")
    destination.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(
        destination, "x", compression=zipfile.ZIP_DEFLATED, compresslevel=9
    ) as archive:
        for name, data, mode in entry_data:
            _write_member(archive, name, data, mode)
        _write_member(archive, CORE_ARCHIVE_MANIFEST, manifest, 0o644)


def _write_member(
    archive: zipfile.ZipFile, name: str, data: bytes, mode: int
) -> None:
    info = zipfile.ZipInfo(name, ARCHIVE_TIMESTAMP)
    info.create_system = 3
    info.external_attr = (stat.S_IFREG | mode) << 16
    archive.writestr(
        info,
        data,
        compress_type=zipfile.ZIP_DEFLATED,
        compresslevel=9,
    )


def stage_core_archive_materials(
    pdfium_archive: pathlib.Path,
    evidence: pathlib.Path,
    destination: pathlib.Path,
) -> dict[str, pathlib.Path]:
    """Stage the exact license inventory used by every Core archive."""
    if destination.exists() or destination.is_symlink():
        raise PortableReleaseError("Core archive material staging already exists")
    destination.mkdir(parents=True)
    pdfium_root = destination / "licenses/pdfium"
    pdfium_root.mkdir(parents=True)
    _extract_pdfium_licenses(pdfium_archive, pdfium_root)
    materials = {
        "LICENSE": ROOT / "LICENSE",
        "NOTICE": evidence / "NOTICE",
        "THIRD_PARTY_NOTICES.md": evidence / "THIRD_PARTY_NOTICES.md",
        "SBOM.spdx.json": evidence / "SBOM.spdx.json",
        "SOURCES.json": evidence / "SOURCES.json",
        "licenses/npm/npm-release.spdx.json": ROOT
        / "third_party/licenses/npm-release.spdx.json",
        "licenses/npm/lucide-ISC-MIT.txt": ROOT
        / "third_party/licenses/npm/lucide-ISC-MIT.txt",
        "licenses/npm/react-MIT.txt": ROOT / "third_party/licenses/npm/react-MIT.txt",
        **{
            f"licenses/pdfium/{name}": pdfium_root / pathlib.PurePosixPath(name)
            for name in PDFIUM_LICENSE_FILES
        },
    }
    if tuple(materials) != CORE_MATERIAL_MEMBERS or any(
        not path.is_file() or path.is_symlink() for path in materials.values()
    ):
        raise PortableReleaseError("Core archive materials are incomplete")
    return materials


def _extract_pdfium_licenses(
    pdfium_archive: pathlib.Path, destination: pathlib.Path
) -> None:
    try:
        with tarfile.open(pdfium_archive, "r:gz") as archive:
            regular_members = [member for member in archive.getmembers() if member.isfile()]
            members = {member.name: member for member in regular_members}
            if len(members) != len(regular_members):
                raise PortableReleaseError("PDFium license archive contains duplicate files")
            selected = {
                name: member
                for name, member in members.items()
                if name == "LICENSE" or name.startswith("licenses/")
            }
            if tuple(sorted(selected)) != tuple(sorted(PDFIUM_LICENSE_FILES)):
                raise PortableReleaseError("PDFium license inventory is incomplete or unexpected")
            for name in PDFIUM_LICENSE_FILES:
                member = selected[name]
                if (
                    member.issym()
                    or member.islnk()
                    or member.size <= 0
                    or member.size > 1024 * 1024
                ):
                    raise PortableReleaseError("PDFium license member is unsafe")
                source = archive.extractfile(member)
                if source is None:
                    raise PortableReleaseError("PDFium license member is unreadable")
                data = source.read(1024 * 1024 + 1)
                if len(data) != member.size:
                    raise PortableReleaseError("PDFium license member length changed")
                output = destination / pathlib.PurePosixPath(name)
                output.parent.mkdir(parents=True, exist_ok=True)
                output.write_bytes(data)
    except PortableReleaseError:
        raise
    except (OSError, tarfile.TarError) as error:
        raise PortableReleaseError("PDFium license archive is unreadable") from error


def windows_pdfium_authority() -> dict:
    """Cross-check the two release authorities before trusting PDFium metadata."""
    platform = json.loads(
        (ROOT / "tools/platform-release/authority.json").read_text(encoding="utf-8")
    )["targets"]["x86_64-pc-windows-msvc"]["pdfium"]
    manifest = json.loads(
        (ROOT / "third_party/pdfium/manifest.json").read_text(encoding="utf-8")
    )["targets"]["x86_64-pc-windows-msvc"]
    if (
        platform["destination"] != WINDOWS_PDFIUM_MEMBER
        or platform["member"] != manifest["library"]
        or platform["bytes"] != manifest["archive_size"]
        or platform["sha256"] != manifest["archive_sha256"]
    ):
        raise PortableReleaseError("Windows PDFium release authorities disagree")
    return manifest


def contains_embedded_pdfium(binary: bytes, member: str, authority: dict) -> bool:
    """Recognize only an exact pinned PDFium local-file ZIP record in the binary."""
    signature = b"PK\x03\x04"
    wanted = member.encode("ascii")
    offset = 0
    while (found := binary.find(signature, offset)) >= 0:
        offset = found + len(signature)
        if found + 30 > len(binary):
            continue
        (
            _signature,
            _version,
            flags,
            method,
            _time,
            _date,
            crc32,
            compressed_size,
            uncompressed_size,
            name_size,
            extra_size,
        ) = struct.unpack_from("<IHHHHHIIIHH", binary, found)
        name_start = found + 30
        data_start = name_start + name_size + extra_size
        data_end = data_start + compressed_size
        if (
            flags != 0
            or method != zipfile.ZIP_DEFLATED
            or binary[name_start : name_start + name_size] != wanted
            or uncompressed_size != authority["library_size"]
            or data_end > len(binary)
        ):
            continue
        try:
            payload = zlib.decompress(binary[data_start:data_end], -zlib.MAX_WBITS)
        except zlib.error:
            continue
        if (
            len(payload) == authority["library_size"]
            and zlib.crc32(payload) == crc32
            and hashlib.sha256(payload).hexdigest() == authority["library_sha256"]
        ):
            return True
    return False


def verify_core_archive(
    archive_path: pathlib.Path,
    target: str,
    binary_architecture: Callable[[bytes, str], bool],
    pdfium_authority: dict | None = None,
) -> None:
    """Authenticate the exact archive inventory, bytes, modes, and manifest."""
    _archive_name, member = CORE_ARCHIVES[target]
    with zipfile.ZipFile(archive_path) as archive:
        infos = archive.infolist()
        expected = [member]
        if target == "x86_64-pc-windows-msvc":
            expected.append(WINDOWS_PDFIUM_MEMBER)
        expected.extend((*CORE_MATERIAL_MEMBERS, CORE_ARCHIVE_MANIFEST))
        if [info.filename for info in infos] != expected or any(
            info.is_dir() for info in infos
        ):
            raise PortableReleaseError("Core ZIP member inventory is invalid")
        if any(
            info.date_time != ARCHIVE_TIMESTAMP
            or info.create_system != 3
            or info.flag_bits & 0x1
            or info.comment
            or info.extra
            or info.compress_type != zipfile.ZIP_DEFLATED
            for info in infos
        ):
            raise PortableReleaseError("Core ZIP metadata is not deterministic")
        binary_data = archive.read(infos[0])
        expected_mode = stat.S_IFREG | (0o755 if member == "into-md" else 0o644)
        if (
            (infos[0].external_attr >> 16) & 0o177777 != expected_mode
            or not binary_architecture(binary_data, target)
        ):
            raise PortableReleaseError("Core binary architecture or mode is invalid")
        if target == "x86_64-pc-windows-msvc":
            _verify_windows_pdfium(
                archive,
                infos[1],
                binary_data,
                pdfium_authority or windows_pdfium_authority(),
            )
        for info in infos[2 if target == "x86_64-pc-windows-msvc" else 1 :]:
            if (info.external_attr >> 16) & 0o177777 != stat.S_IFREG | 0o644:
                raise PortableReleaseError(
                    "Core declaration or license member mode is invalid"
                )
        _verify_materials(archive)
        _verify_manifest(archive, infos, target)


def _verify_windows_pdfium(
    archive: zipfile.ZipFile,
    runtime: zipfile.ZipInfo,
    binary_data: bytes,
    authority: dict,
) -> None:
    if (runtime.external_attr >> 16) & 0o177777 != stat.S_IFREG | 0o644:
        raise PortableReleaseError("Windows PDFium archive member is not a regular file")
    data = archive.read(runtime)
    if (
        len(data) != authority["library_size"]
        or hashlib.sha256(data).hexdigest() != authority["library_sha256"]
    ):
        raise PortableReleaseError(
            "Windows PDFium archive member differs from the pinned manifest"
        )
    if contains_embedded_pdfium(binary_data, WINDOWS_PDFIUM_MEMBER, authority):
        raise PortableReleaseError("Windows Core still contains an embedded PDFium payload")


def _verify_materials(archive: zipfile.ZipFile) -> None:
    static_materials = {
        "LICENSE": ROOT / "LICENSE",
        "licenses/npm/npm-release.spdx.json": ROOT
        / "third_party/licenses/npm-release.spdx.json",
        "licenses/npm/lucide-ISC-MIT.txt": ROOT
        / "third_party/licenses/npm/lucide-ISC-MIT.txt",
        "licenses/npm/react-MIT.txt": ROOT / "third_party/licenses/npm/react-MIT.txt",
    }
    for name, source in static_materials.items():
        if archive.read(name) != source.read_bytes():
            raise PortableReleaseError(f"Core archive static license differs: {name}")
    if any(not archive.read(f"licenses/pdfium/{name}") for name in PDFIUM_LICENSE_FILES):
        raise PortableReleaseError("Core archive contains an empty PDFium license member")


def _verify_manifest(
    archive: zipfile.ZipFile, infos: list[zipfile.ZipInfo], target: str
) -> None:
    try:
        manifest = json.loads(archive.read(CORE_ARCHIVE_MANIFEST))
    except (KeyError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise PortableReleaseError("Core archive manifest is invalid") from error
    if (
        not isinstance(manifest, dict)
        or set(manifest) != {"schemaVersion", "target", "files"}
        or manifest["schemaVersion"] != 1
        or manifest["target"] != target
        or not isinstance(manifest["files"], list)
    ):
        raise PortableReleaseError("Core archive manifest authority is invalid")
    observed = [
        archive_record(
            info.filename,
            archive.read(info),
            (info.external_attr >> 16) & 0o777,
        )
        for info in infos[:-1]
    ]
    if manifest["files"] != observed:
        raise PortableReleaseError("Core archive differs from its bidirectional manifest")

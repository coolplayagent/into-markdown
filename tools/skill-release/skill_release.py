"""Validate and package the self-contained Into Markdown skill."""

from __future__ import annotations

import hashlib
import json
import pathlib
import shutil
import stat
import struct
import sys
import tomllib
import zipfile
from dataclasses import dataclass
from typing import Mapping


ROOT = pathlib.Path(__file__).resolve().parents[2]
PORTABLE_RELEASE_DIR = ROOT / "tools/portable-release"
if str(PORTABLE_RELEASE_DIR) not in sys.path:
    sys.path.insert(0, str(PORTABLE_RELEASE_DIR))
from core_archive import (  # noqa: E402
    MaterialAuthorityError,
    load_authority,
    verify_materials,
)
from release_artifacts import ResourceBudget, _preflight_zip  # noqa: E402

SKILL_NAME = "into-markdown"
SKILL_SOURCE = ROOT / ".agents/skills" / SKILL_NAME
# Kept for release-tool compatibility while Core packages stop embedding the skill.
CORE_RELATIVE = pathlib.Path("share/into-markdown/skills") / SKILL_NAME
FIXED_TIMESTAMP = (2026, 1, 1, 0, 0, 0)
ARCHIVE_MANIFEST = pathlib.PurePosixPath("archive-manifest.json")
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
CORE_MATERIAL_RELATIVES = tuple(
    pathlib.PurePosixPath(value)
    for value in (
        "NOTICE",
        "THIRD_PARTY_NOTICES.md",
        "SBOM.spdx.json",
        "SOURCES.json",
        "licenses/npm/npm-release.spdx.json",
        "licenses/npm/lucide-ISC-MIT.txt",
        "licenses/npm/react-MIT.txt",
        "licenses/diagram-design-MIT.txt",
        *(f"licenses/pdfium/{name}" for name in PDFIUM_LICENSE_FILES),
    )
)
AUTHORITY_MATERIALS = ("LICENSE", *(path.as_posix() for path in CORE_MATERIAL_RELATIVES))
SKILL_AUTHORITY_NAMESPACE = "into-markdown/skill-release-authority"
TARGET_LAYOUTS = (
    ("x86_64-pc-windows-msvc", pathlib.PurePosixPath("assets/windows-x86_64/into-md.exe")),
    ("x86_64-unknown-linux-gnu", pathlib.PurePosixPath("assets/linux-x86_64/into-md")),
    ("aarch64-unknown-linux-gnu", pathlib.PurePosixPath("assets/linux-arm64/into-md")),
)
# Compatibility name for source-material fixtures; Skill authority is never single-target.
AUTHORITY_TARGET = TARGET_LAYOUTS[0][0]
EVIDENCE_ROOT = pathlib.PurePosixPath("evidence")


def evidence_relative(target: str, member: str) -> pathlib.PurePosixPath:
    return EVIDENCE_ROOT / target / pathlib.PurePosixPath(member)


MATERIAL_DIRECTORIES = tuple(
    pathlib.PurePosixPath(value)
    for value in (
        "evidence",
        *(
            item
            for target, _asset in TARGET_LAYOUTS
            for item in (
                f"evidence/{target}",
                f"evidence/{target}/licenses",
                f"evidence/{target}/licenses/npm",
                f"evidence/{target}/licenses/pdfium",
                f"evidence/{target}/licenses/pdfium/licenses",
            )
        ),
    )
)
CANONICAL_FILES = (
    pathlib.PurePosixPath("LICENSE"),
    pathlib.PurePosixPath("SKILL.md"),
    pathlib.PurePosixPath("agents/openai.yaml"),
    pathlib.PurePosixPath("references/cli-workflows.md"),
)
CANONICAL_DIRECTORIES = (
    pathlib.PurePosixPath("agents"),
    pathlib.PurePosixPath("references"),
)
# Backward-compatible names for consumers of the canonical instruction tree.
ALLOWED_FILES = CANONICAL_FILES
ALLOWED_DIRECTORIES = CANONICAL_DIRECTORIES


@dataclass(frozen=True)
class AssetSpec:
    relative: pathlib.PurePosixPath
    format_name: str
    machine: int
    mode: int


ASSET_SPECS = (
    AssetSpec(pathlib.PurePosixPath("assets/windows-x86_64/into-md.exe"), "PE", 0x8664, 0o644),
    AssetSpec(pathlib.PurePosixPath("assets/linux-x86_64/into-md"), "ELF", 62, 0o755),
    AssetSpec(pathlib.PurePosixPath("assets/linux-arm64/into-md"), "ELF", 183, 0o755),
)
WINDOWS_PDFIUM_RELATIVE = pathlib.PurePosixPath(
    "assets/windows-x86_64/lib/pdfium/pdfium.dll"
)
OPTIONAL_SPEECH_COMPONENT_IDS = frozenset(
    {
        "ffmpeg",
        "whisper-small",
        "silero-vad-half-onnx-model",
        "3dspeaker-eres2net-base-onnx-model",
    }
)
WINDOWS_PDFIUM_AUTHORITY = json.loads(
    (ROOT / "third_party/pdfium/manifest.json").read_text(encoding="utf-8")
)["targets"]["x86_64-pc-windows-msvc"]
ASSET_DIRECTORIES = (
    pathlib.PurePosixPath("assets"),
    pathlib.PurePosixPath("assets/linux-arm64"),
    pathlib.PurePosixPath("assets/linux-x86_64"),
    pathlib.PurePosixPath("assets/windows-x86_64"),
    pathlib.PurePosixPath("assets/windows-x86_64/lib"),
    pathlib.PurePosixPath("assets/windows-x86_64/lib/pdfium"),
)


class SkillReleaseError(RuntimeError):
    """The canonical skill, bundled Core, or archive violated the fixed contract."""


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def core_inputs(
    windows_x86_64_core: pathlib.Path,
    windows_x86_64_pdfium: pathlib.Path,
    linux_x86_64_core: pathlib.Path,
    linux_arm64_core: pathlib.Path,
) -> dict[pathlib.PurePosixPath, pathlib.Path]:
    """Bind reviewed Core inputs and their release materials to immutable skill paths."""
    inputs = {
        ASSET_SPECS[0].relative: windows_x86_64_core,
        WINDOWS_PDFIUM_RELATIVE: windows_x86_64_pdfium,
        ASSET_SPECS[1].relative: linux_x86_64_core,
        ASSET_SPECS[2].relative: linux_arm64_core,
    }
    roots = {
        TARGET_LAYOUTS[0][0]: windows_x86_64_core.parent,
        TARGET_LAYOUTS[1][0]: linux_x86_64_core.parent,
        TARGET_LAYOUTS[2][0]: linux_arm64_core.parent,
    }
    for target, root in roots.items():
        for member in AUTHORITY_MATERIALS:
            inputs[evidence_relative(target, member)] = root / pathlib.Path(member)
    return inputs


def validate(source: pathlib.Path = SKILL_SOURCE) -> tuple[pathlib.Path, ...]:
    if source.is_symlink():
        raise SkillReleaseError("skill source is not a trusted directory")
    source = source.resolve()
    if not source.is_dir():
        raise SkillReleaseError("skill source is not a trusted directory")
    files: list[pathlib.PurePosixPath] = []
    directories: list[pathlib.PurePosixPath] = []
    for path in sorted(source.rglob("*"), key=lambda item: item.as_posix()):
        relative = pathlib.PurePosixPath(path.relative_to(source).as_posix())
        metadata = path.lstat()
        if stat.S_ISLNK(metadata.st_mode):
            raise SkillReleaseError(f"skill source contains a symbolic link: {relative}")
        if stat.S_ISDIR(metadata.st_mode):
            directories.append(relative)
        elif stat.S_ISREG(metadata.st_mode):
            files.append(relative)
        else:
            raise SkillReleaseError(f"skill source contains an unsupported file: {relative}")
    if tuple(files) != CANONICAL_FILES or tuple(directories) != CANONICAL_DIRECTORIES:
        raise SkillReleaseError("skill source does not contain the exact reviewed file set")

    texts = {}
    for relative in CANONICAL_FILES:
        try:
            texts[relative.as_posix()] = (source / relative).read_text(encoding="utf-8")
        except UnicodeDecodeError as error:
            raise SkillReleaseError(f"skill file is not UTF-8: {relative}") from error
    if any("TODO" in value for value in texts.values()):
        raise SkillReleaseError("skill source contains an unfinished placeholder")
    _validate_skill_markdown(texts["SKILL.md"])
    _validate_openai_yaml(texts["agents/openai.yaml"])
    if (source / "LICENSE").read_bytes() != (ROOT / "LICENSE").read_bytes():
        raise SkillReleaseError("skill LICENSE differs from the project license")
    return tuple(source / relative for relative in CANONICAL_FILES)


def _workspace_version() -> str:
    try:
        manifest = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
        version = manifest["workspace"]["package"]["version"]
    except (OSError, UnicodeDecodeError, ValueError, KeyError, TypeError) as error:
        raise SkillReleaseError("cannot read Cargo.toml workspace.package.version") from error
    if not isinstance(version, str) or not version.strip():
        raise SkillReleaseError("Cargo.toml workspace.package.version must be a non-empty string")
    return version


def _validate_skill_markdown(text: str) -> None:
    lines = text.splitlines()
    if not lines or lines[0] != "---":
        raise SkillReleaseError("SKILL.md has no YAML frontmatter")
    try:
        end = lines.index("---", 1)
    except ValueError as error:
        raise SkillReleaseError("SKILL.md frontmatter is not closed") from error
    fields = {}
    frontmatter = iter(lines[1:end])
    # Parse the reviewed block layout; version uses a double-quoted YAML string.
    for line in frontmatter:
        key, separator, value = line.partition(":")
        if not separator or key not in {"name", "description", "metadata"}:
            raise SkillReleaseError("SKILL.md frontmatter contains an invalid field")
        if key in fields:
            raise SkillReleaseError(f"SKILL.md frontmatter contains a duplicate field: {key}")
        if key == "metadata":
            version_key, version_separator, version_value = next(frontmatter, "").partition(":")
            if value.strip() or version_key != "  version" or not version_separator:
                raise SkillReleaseError("SKILL.md metadata must contain an indented version field")
            try:
                version = json.loads(version_value.strip())
            except json.JSONDecodeError as error:
                raise SkillReleaseError("SKILL.md metadata.version must be a double-quoted string") from error
            if not isinstance(version, str) or not version.strip():
                raise SkillReleaseError("SKILL.md metadata.version must be a non-empty string")
            fields[key] = version
        else:
            if not value.strip():
                raise SkillReleaseError("SKILL.md frontmatter contains an invalid field")
            fields[key] = value.strip().strip('"')
    if set(fields) != {"name", "description", "metadata"} or fields["name"] != SKILL_NAME:
        raise SkillReleaseError("SKILL.md must declare the reviewed name, description, and metadata.version")
    expected_version = _workspace_version()
    if fields["metadata"] != expected_version:
        raise SkillReleaseError(
            f"SKILL.md metadata.version {fields['metadata']!r} differs from "
            f"Cargo.toml workspace.package.version {expected_version!r}"
        )
    description = fields["description"].lower()
    positive = [
        "documents",
        "images",
        "audio",
        "video",
        "standard input",
        "directories",
        "remote sources",
        "bundled into-md",
    ]
    negative = [
        "do not use",
        "editing",
        "summarization",
        "web ui administration",
        "plugin management",
        "provider configuration",
    ]
    if not all(fragment in description for fragment in positive + negative):
        raise SkillReleaseError("SKILL.md description does not preserve its routing boundaries")
    required_body = [
        "references/cli-workflows.md",
        "assets/windows-x86_64/into-md.exe",
        "assets/linux-x86_64/into-md",
        "assets/linux-arm64/into-md",
        "Do not search `PATH`",
        "this host is unsupported",
    ]
    if not all(fragment in text for fragment in required_body):
        raise SkillReleaseError("SKILL.md does not preserve bundled executable routing")


def _validate_openai_yaml(text: str) -> None:
    required = {
        '  display_name: "Into Markdown"',
        '  short_description: "Convert documents and media into Markdown"',
        '  default_prompt: "Use $into-markdown to convert these files into verified Markdown artifacts."',
        "  allow_implicit_invocation: true",
    }
    lines = set(text.splitlines())
    if not required <= lines or not text.startswith("interface:\n") or "\npolicy:\n" not in text:
        raise SkillReleaseError("agents/openai.yaml is missing reviewed interface or policy fields")
    if "dependencies:" in text:
        raise SkillReleaseError("the bundled skill must not declare external tool dependencies")


def _validate_core_path(path: pathlib.Path, spec: AssetSpec) -> pathlib.Path:
    if path.is_symlink():
        raise SkillReleaseError(f"{spec.relative} input is a symbolic link")
    try:
        metadata = path.stat()
    except OSError as error:
        raise SkillReleaseError(f"{spec.relative} input is not a readable regular file") from error
    if not stat.S_ISREG(metadata.st_mode):
        raise SkillReleaseError(f"{spec.relative} input is not a readable regular file")
    try:
        with path.open("rb") as binary:
            header = binary.read(4096)
    except OSError as error:
        raise SkillReleaseError(f"{spec.relative} input is not a readable regular file") from error
    _validate_binary_header(header, metadata.st_size, spec)
    return path


def _validate_binary_header(header: bytes, size: int, spec: AssetSpec) -> None:
    if spec.format_name == "PE":
        if size < 64 or len(header) < 64 or header[:2] != b"MZ":
            raise SkillReleaseError(f"{spec.relative} is not a PE executable")
        pe_offset = struct.unpack_from("<I", header, 0x3C)[0]
        if pe_offset > 4096 - 24 or pe_offset + 24 > size or header[pe_offset : pe_offset + 4] != b"PE\0\0":
            raise SkillReleaseError(f"{spec.relative} has an invalid PE header")
        machine = struct.unpack_from("<H", header, pe_offset + 4)[0]
        if machine != spec.machine:
            raise SkillReleaseError(f"{spec.relative} has the wrong PE architecture")
        return

    if size < 64 or len(header) < 64 or header[:4] != b"\x7fELF":
        raise SkillReleaseError(f"{spec.relative} is not an ELF executable")
    if header[4:7] != b"\x02\x01\x01":
        raise SkillReleaseError(f"{spec.relative} is not a 64-bit little-endian ELF executable")
    elf_type, machine = struct.unpack_from("<HH", header, 16)
    if elf_type not in (2, 3):
        raise SkillReleaseError(f"{spec.relative} is not an ELF executable or PIE")
    if machine != spec.machine:
        raise SkillReleaseError(f"{spec.relative} has the wrong ELF architecture")


def _validate_material(path: pathlib.Path, relative: pathlib.PurePosixPath) -> pathlib.Path:
    if path.is_symlink():
        raise SkillReleaseError(f"{relative} input is a symbolic link")
    try:
        metadata = path.stat()
    except OSError as error:
        raise SkillReleaseError(f"{relative} input is not a readable regular file") from error
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_size == 0:
        raise SkillReleaseError(f"{relative} input is not a non-empty regular file")
    return path


def _validated_cores(
    cores: Mapping[pathlib.PurePosixPath, pathlib.Path],
) -> dict[pathlib.PurePosixPath, pathlib.Path]:
    expected = (
        {spec.relative for spec in ASSET_SPECS}
        | {WINDOWS_PDFIUM_RELATIVE}
        | {
            evidence_relative(target, member)
            for target, _asset in TARGET_LAYOUTS
            for member in AUTHORITY_MATERIALS
        }
    )
    if set(cores) != expected:
        raise SkillReleaseError(
            "the three reviewed Core inputs, Windows PDFium, and exact release materials are required"
        )
    validated = {
        spec.relative: _validate_core_path(pathlib.Path(cores[spec.relative]), spec)
        for spec in ASSET_SPECS
    }
    runtime = pathlib.Path(cores[WINDOWS_PDFIUM_RELATIVE])
    if runtime.is_symlink() or not runtime.is_file():
        raise SkillReleaseError("Windows PDFium input is not a regular file")
    if (
        runtime.stat().st_size != WINDOWS_PDFIUM_AUTHORITY["library_size"]
        or sha256(runtime) != WINDOWS_PDFIUM_AUTHORITY["library_sha256"]
    ):
        raise SkillReleaseError("Windows PDFium input differs from the pinned manifest")
    validated[WINDOWS_PDFIUM_RELATIVE] = runtime
    static_materials = {
        "LICENSE": ROOT / "LICENSE",
        "licenses/npm/npm-release.spdx.json": ROOT / "third_party/licenses/npm-release.spdx.json",
        "licenses/npm/lucide-ISC-MIT.txt": ROOT / "third_party/licenses/npm/lucide-ISC-MIT.txt",
        "licenses/npm/react-MIT.txt": ROOT / "third_party/licenses/npm/react-MIT.txt",
        "licenses/diagram-design-MIT.txt": ROOT / "third_party/licenses/diagram-design-MIT.txt",
    }
    for target, _asset in TARGET_LAYOUTS:
        for member in AUTHORITY_MATERIALS:
            relative = evidence_relative(target, member)
            path = _validate_material(pathlib.Path(cores[relative]), relative)
            validated[relative] = path
            authority_source = static_materials.get(member)
            if authority_source is not None and path.read_bytes() != authority_source.read_bytes():
                raise SkillReleaseError(f"{relative} differs from the repository authority")
            if member in {"SBOM.spdx.json", "SOURCES.json"}:
                try:
                    structured = json.loads(path.read_text(encoding="utf-8"))
                except (UnicodeDecodeError, json.JSONDecodeError) as error:
                    raise SkillReleaseError(f"{relative} is not valid JSON") from error
                if member == "SBOM.spdx.json":
                    component_ids = {
                        package.get("name")
                        for package in structured.get("packages", [])
                        if isinstance(package, dict)
                    }
                else:
                    component_ids = {
                        component.get("id")
                        for component in structured.get("components", [])
                        if isinstance(component, dict)
                    }
                if component_ids & OPTIONAL_SPEECH_COMPONENT_IDS:
                    raise SkillReleaseError("Skill target evidence must not include optional speech components")
                if member == "SOURCES.json" and (
                    structured.get("target") != target
                    or structured.get("artifact") != "into-markdown-core"
                ):
                    raise SkillReleaseError(f"{relative} target or artifact identity is invalid")
    return validated


def materialize(destination: pathlib.Path, source: pathlib.Path = SKILL_SOURCE) -> pathlib.Path:
    """Copy only canonical instructions; release archives add platform Core assets."""
    files = validate(source)
    if destination.exists() or destination.is_symlink():
        raise SkillReleaseError("skill release destination already exists")
    destination.mkdir(parents=True)
    destination.chmod(0o755)
    for path in files:
        relative = path.relative_to(source.resolve())
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.parent.chmod(0o755)
        shutil.copyfile(path, target)
        target.chmod(0o644)
    validate_materialized(destination, source)
    return destination


def validate_materialized(destination: pathlib.Path, source: pathlib.Path = SKILL_SOURCE) -> None:
    expected = {path.relative_to(source.resolve()).as_posix(): path.read_bytes() for path in validate(source)}
    actual = {}
    if not destination.is_dir() or destination.is_symlink():
        raise SkillReleaseError("materialized skill is not a trusted directory")
    for path in destination.rglob("*"):
        metadata = path.lstat()
        if stat.S_ISLNK(metadata.st_mode):
            raise SkillReleaseError("materialized skill contains a symbolic link")
        if stat.S_ISREG(metadata.st_mode):
            if sys.platform != "win32" and stat.S_IMODE(metadata.st_mode) != 0o644:
                raise SkillReleaseError("materialized skill file permissions are invalid")
            actual[path.relative_to(destination).as_posix()] = path.read_bytes()
        elif stat.S_ISDIR(metadata.st_mode):
            if sys.platform != "win32" and stat.S_IMODE(metadata.st_mode) != 0o755:
                raise SkillReleaseError("materialized skill directory permissions are invalid")
        else:
            raise SkillReleaseError("materialized skill contains an unsupported file")
    if actual != expected:
        raise SkillReleaseError("materialized skill differs from the canonical source")


def _file_record(path: pathlib.Path, relative: str) -> dict[str, object]:
    return {"path": relative, "bytes": path.stat().st_size, "sha256": sha256(path)}


def build_skill_authority(
    cores: Mapping[pathlib.PurePosixPath, pathlib.Path],
    material_authorities: Mapping[str, pathlib.Path],
) -> dict[str, object]:
    """Bind every target's Core binary and target-scoped release materials."""
    validated = _validated_cores(cores)
    expected_targets = {target for target, _asset in TARGET_LAYOUTS}
    if set(material_authorities) != expected_targets:
        raise SkillReleaseError("exactly one material authority is required for each Skill target")
    targets = []
    for target, asset in sorted(TARGET_LAYOUTS):
        try:
            authority = load_authority(
                pathlib.Path(material_authorities[target]), target, AUTHORITY_MATERIALS
            )
            materials = {
                member: validated[evidence_relative(target, member)].read_bytes()
                for member in AUTHORITY_MATERIALS
            }
            verify_materials(materials, authority, AUTHORITY_MATERIALS)
        except MaterialAuthorityError as error:
            raise SkillReleaseError(str(error)) from error
        targets.append(
            {
                "target": target,
                "binary": _file_record(validated[asset], asset.as_posix()),
                "materials": [
                    _file_record(
                        validated[evidence_relative(target, member)], member
                    )
                    for member in sorted(AUTHORITY_MATERIALS)
                ],
            }
        )
    return {
        "schemaVersion": 1,
        "namespace": SKILL_AUTHORITY_NAMESPACE,
        "artifact": SKILL_NAME,
        "targets": targets,
    }


def write_skill_authority(path: pathlib.Path, authority: dict[str, object]) -> None:
    if path.exists() or path.is_symlink():
        raise SkillReleaseError("Skill authority destination already exists")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(authority, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
        newline="\n",
    )


def load_skill_authority(path: pathlib.Path) -> dict[str, object]:
    try:
        authority = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise SkillReleaseError("Skill authority is not readable canonical JSON") from error
    if (
        not isinstance(authority, dict)
        or set(authority) != {"schemaVersion", "namespace", "artifact", "targets"}
        or authority.get("schemaVersion") != 1
        or authority.get("namespace") != SKILL_AUTHORITY_NAMESPACE
        or authority.get("artifact") != SKILL_NAME
        or not isinstance(authority.get("targets"), list)
    ):
        raise SkillReleaseError("Skill authority namespace or schema is invalid")
    expected_targets = sorted(target for target, _asset in TARGET_LAYOUTS)
    observed_targets = [item.get("target") for item in authority["targets"] if isinstance(item, dict)]
    if observed_targets != expected_targets or len(observed_targets) != len(authority["targets"]):
        raise SkillReleaseError("Skill authority targets are missing, duplicated, or unsorted")
    assets = dict(TARGET_LAYOUTS)
    for item in authority["targets"]:
        if set(item) != {"target", "binary", "materials"}:
            raise SkillReleaseError("Skill authority target schema is invalid")
        expected = [*sorted(AUTHORITY_MATERIALS)]
        records = item["materials"]
        if not isinstance(records, list) or [record.get("path") for record in records if isinstance(record, dict)] != expected:
            raise SkillReleaseError("Skill authority materials are missing, duplicated, or unsorted")
        _validate_authority_record(item["binary"], assets[item["target"]].as_posix())
        for record, member in zip(records, expected, strict=True):
            _validate_authority_record(record, member)
    return authority


def _validate_authority_record(record: object, expected_path: str) -> None:
    if (
        not isinstance(record, dict)
        or set(record) != {"path", "bytes", "sha256"}
        or record.get("path") != expected_path
        or not isinstance(record.get("bytes"), int)
        or isinstance(record.get("bytes"), bool)
        or record["bytes"] <= 0
        or not isinstance(record.get("sha256"), str)
        or len(record["sha256"]) != 64
        or any(character not in "0123456789abcdef" for character in record["sha256"])
    ):
        raise SkillReleaseError("Skill authority contains an invalid file record")


def create_archive(
    destination: pathlib.Path,
    cores: Mapping[pathlib.PurePosixPath, pathlib.Path],
    material_authorities: Mapping[str, pathlib.Path],
    authority_output: pathlib.Path,
    source: pathlib.Path = SKILL_SOURCE,
) -> pathlib.Path:
    files = validate(source)
    validated_cores = _validated_cores(cores)
    authority = build_skill_authority(validated_cores, material_authorities)
    sidecar = destination.with_name(destination.name + ".sha256")
    if destination.exists() or destination.is_symlink() or sidecar.exists() or sidecar.is_symlink():
        raise SkillReleaseError("skill archive destination or forbidden checksum sidecar already exists")
    destination.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(destination, "x", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        archive.comment = b""
        _write_directory(archive, f"{SKILL_NAME}/")
        entries: list[tuple[str, pathlib.Path | None, int]] = [
            *(
                (relative.as_posix(), None, 0o755)
                for relative in CANONICAL_DIRECTORIES + ASSET_DIRECTORIES + MATERIAL_DIRECTORIES
            ),
            *((path.relative_to(source.resolve()).as_posix(), path, 0o644) for path in files),
            *((spec.relative.as_posix(), validated_cores[spec.relative], spec.mode) for spec in ASSET_SPECS),
            (WINDOWS_PDFIUM_RELATIVE.as_posix(), validated_cores[WINDOWS_PDFIUM_RELATIVE], 0o644),
            *(
                (relative.as_posix(), validated_cores[relative], 0o644)
                for relative in sorted(
                    path
                    for path in validated_cores
                    if path.parts and path.parts[0] == EVIDENCE_ROOT.as_posix()
                )
            ),
        ]
        manifest_files = [
            _manifest_record(relative, path, mode)
            for relative, path, mode in sorted(entries, key=lambda entry: entry[0])
            if path is not None
        ]
        manifest = json.dumps(
            {"schemaVersion": 1, "files": manifest_files},
            ensure_ascii=False,
            indent=2,
            sort_keys=True,
        ).encode("utf-8") + b"\n"
        entries.append((ARCHIVE_MANIFEST.as_posix(), None, 0o644))
        for relative, path, mode in sorted(entries, key=lambda entry: entry[0]):
            if relative == ARCHIVE_MANIFEST.as_posix():
                _write_bytes(archive, f"{SKILL_NAME}/{relative}", manifest, mode)
            elif path is None:
                _write_directory(archive, f"{SKILL_NAME}/{relative}/")
            else:
                _write_file(archive, f"{SKILL_NAME}/{relative}", path, mode)
    verify_archive(destination, authority, source, expected_cores=validated_cores)
    write_skill_authority(authority_output, authority)
    verify_release(destination, authority_output, source, expected_cores=validated_cores)
    return destination


def _write_directory(archive: zipfile.ZipFile, name: str) -> None:
    info = zipfile.ZipInfo(name, FIXED_TIMESTAMP)
    info.create_system = 3
    info.compress_type = zipfile.ZIP_DEFLATED
    info.external_attr = ((stat.S_IFDIR | 0o755) << 16) | 0x10
    archive.writestr(info, b"", compress_type=zipfile.ZIP_DEFLATED, compresslevel=9)


def _write_file(archive: zipfile.ZipFile, name: str, path: pathlib.Path, mode: int) -> None:
    info = zipfile.ZipInfo(name, FIXED_TIMESTAMP)
    info.create_system = 3
    info.compress_type = zipfile.ZIP_DEFLATED
    info.external_attr = (stat.S_IFREG | mode) << 16
    with path.open("rb") as source, archive.open(info, "w") as output:
        shutil.copyfileobj(source, output, length=1024 * 1024)


def _write_bytes(archive: zipfile.ZipFile, name: str, data: bytes, mode: int) -> None:
    info = zipfile.ZipInfo(name, FIXED_TIMESTAMP)
    info.create_system = 3
    info.compress_type = zipfile.ZIP_DEFLATED
    info.external_attr = (stat.S_IFREG | mode) << 16
    archive.writestr(info, data, compress_type=zipfile.ZIP_DEFLATED, compresslevel=9)


def _manifest_record(relative: str, path: pathlib.Path, mode: int) -> dict[str, object]:
    record: dict[str, object] = {
        "path": relative,
        "bytes": path.stat().st_size,
        "sha256": sha256(path),
        "mode": f"{mode:04o}",
        "kind": (
            "component"
            if relative == WINDOWS_PDFIUM_RELATIVE.as_posix()
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
    if relative == WINDOWS_PDFIUM_RELATIVE.as_posix() or "/licenses/pdfium/" in relative:
        record["componentId"] = "pdfium"
    return record


def _expected_archive_entries(
    source: pathlib.Path,
) -> tuple[list[str], dict[str, bytes], dict[str, AssetSpec]]:
    canonical = {
        f"{SKILL_NAME}/{path.relative_to(source.resolve()).as_posix()}": path.read_bytes()
        for path in validate(source)
    }
    assets = {f"{SKILL_NAME}/{spec.relative.as_posix()}": spec for spec in ASSET_SPECS}
    directories = {
        f"{SKILL_NAME}/",
        *(
            f"{SKILL_NAME}/{relative.as_posix()}/"
            for relative in CANONICAL_DIRECTORIES + ASSET_DIRECTORIES + MATERIAL_DIRECTORIES
        ),
    }
    names = [
        f"{SKILL_NAME}/",
        *sorted(
            (directories - {f"{SKILL_NAME}/"})
            | set(canonical)
            | set(assets)
            | {f"{SKILL_NAME}/{WINDOWS_PDFIUM_RELATIVE.as_posix()}"}
            | {
                f"{SKILL_NAME}/{evidence_relative(target, member).as_posix()}"
                for target, _asset in TARGET_LAYOUTS
                for member in AUTHORITY_MATERIALS
            }
            | {f"{SKILL_NAME}/{ARCHIVE_MANIFEST.as_posix()}"}
        ),
    ]
    return names, canonical, assets


def _stream_verified_member(
    archive: zipfile.ZipFile, info: zipfile.ZipInfo, capture: bool
) -> tuple[bytes, int, str, bytes | None]:
    digest = hashlib.sha256()
    header = bytearray()
    contents = bytearray() if capture else None
    size = 0
    with archive.open(info) as source:
        while chunk := source.read(1024 * 1024):
            size += len(chunk)
            if size > info.file_size:
                raise SkillReleaseError(f"skill archive member exceeded its declared size: {info.filename}")
            digest.update(chunk)
            if len(header) < 4096:
                header.extend(chunk[: 4096 - len(header)])
            if contents is not None:
                if size > 16 * 1024 * 1024:
                    raise SkillReleaseError("skill archive metadata member is too large")
                contents.extend(chunk)
    if size != info.file_size:
        raise SkillReleaseError(f"skill archive member size is inconsistent: {info.filename}")
    return bytes(header), size, digest.hexdigest(), bytes(contents) if contents is not None else None


def _observed_manifest_record(
    relative: str, size: int, digest: str, mode: int
) -> dict[str, object]:
    record: dict[str, object] = {
        "path": relative,
        "bytes": size,
        "sha256": digest,
        "mode": f"{mode & 0o7777:04o}",
        "kind": (
            "component"
            if relative == WINDOWS_PDFIUM_RELATIVE.as_posix()
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
    if relative == WINDOWS_PDFIUM_RELATIVE.as_posix() or "/licenses/pdfium/" in relative:
        record["componentId"] = "pdfium"
    return record


def verify_archive(
    archive_path: pathlib.Path,
    material_authority: dict,
    source: pathlib.Path = SKILL_SOURCE,
    expected_cores: Mapping[pathlib.PurePosixPath, pathlib.Path] | None = None,
) -> None:
    expected_names, canonical, assets = _expected_archive_entries(source)
    expected_hashes = None
    if expected_cores is not None:
        expected_hashes = {relative: sha256(path) for relative, path in _validated_cores(expected_cores).items()}
    try:
        infos = _preflight_zip(archive_path, ResourceBudget())
        with zipfile.ZipFile(archive_path) as archive:
            names = [info.filename for info in infos]
            if names != expected_names or len(names) != len(set(names)):
                raise SkillReleaseError("skill archive does not contain the exact reviewed entries in sorted order")
            if archive.comment:
                raise SkillReleaseError("skill archive metadata is not deterministic")
            observed_manifest_files: list[dict[str, object]] = []
            observed_authority_files: dict[str, dict[str, object]] = {}
            manifest: object = None
            for info in infos:
                if (
                    info.date_time != FIXED_TIMESTAMP
                    or info.create_system != 3
                    or info.flag_bits & 0x1
                    or info.comment
                    or info.extra
                    or info.compress_type != zipfile.ZIP_DEFLATED
                ):
                    raise SkillReleaseError("skill archive metadata is not deterministic")
                mode = (info.external_attr >> 16) & 0o177777
                if info.is_dir():
                    if mode != stat.S_IFDIR | 0o755 or info.file_size != 0:
                        raise SkillReleaseError("skill archive directory metadata is invalid")
                    continue
                relative = info.filename.removeprefix(f"{SKILL_NAME}/")
                capture = info.filename in canonical or relative == ARCHIVE_MANIFEST.as_posix()
                header, size, digest, data = _stream_verified_member(archive, info, capture)
                if relative == ARCHIVE_MANIFEST.as_posix():
                    if mode != stat.S_IFREG | 0o644:
                        raise SkillReleaseError("skill archive manifest permissions are invalid")
                    try:
                        assert data is not None
                        manifest = json.loads(data)
                    except (UnicodeDecodeError, json.JSONDecodeError) as error:
                        raise SkillReleaseError("skill archive manifest is not valid JSON") from error
                    continue
                if relative.startswith(f"{EVIDENCE_ROOT.as_posix()}/") or relative.startswith("assets/"):
                    observed_authority_files[relative] = {"bytes": size, "sha256": digest}
                observed_manifest_files.append(_observed_manifest_record(relative, size, digest, mode))
                if info.filename in canonical:
                    assert data is not None
                    if mode != stat.S_IFREG | 0o644 or data != canonical[info.filename]:
                        raise SkillReleaseError("skill archive instruction metadata or bytes are invalid")
                    continue
                if info.filename == f"{SKILL_NAME}/{WINDOWS_PDFIUM_RELATIVE.as_posix()}":
                    if (
                        mode != stat.S_IFREG | 0o644
                        or size != WINDOWS_PDFIUM_AUTHORITY["library_size"]
                        or digest != WINDOWS_PDFIUM_AUTHORITY["library_sha256"]
                    ):
                        raise SkillReleaseError(
                            "Windows PDFium archive member differs from the pinned manifest"
                        )
                    continue
                material = pathlib.PurePosixPath(relative)
                if material.parts and material.parts[0] == EVIDENCE_ROOT.as_posix():
                    if mode != stat.S_IFREG | 0o644 or size == 0:
                        raise SkillReleaseError("skill archive release material is invalid")
                    if expected_hashes is not None and digest != expected_hashes[material]:
                        raise SkillReleaseError(f"{material} bytes differ from the supplied Core")
                    continue
                spec = assets[info.filename]
                if mode != stat.S_IFREG | spec.mode:
                    raise SkillReleaseError(f"{spec.relative} archive permissions are invalid")
                _validate_binary_header(header, size, spec)
                if expected_hashes is not None and digest != expected_hashes[spec.relative]:
                    raise SkillReleaseError(f"{spec.relative} bytes differ from the supplied Core")
            expected_manifest = {"schemaVersion": 1, "files": observed_manifest_files}
            if manifest != expected_manifest:
                raise SkillReleaseError("skill archive manifest is not an exact bidirectional projection")
            _verify_skill_authority_files(observed_authority_files, material_authority)
    except (OSError, zipfile.BadZipFile, RuntimeError) as error:
        if isinstance(error, SkillReleaseError):
            raise
        raise SkillReleaseError("skill archive is not a readable ZIP") from error


def verify_release(
    archive_path: pathlib.Path,
    material_authority: pathlib.Path,
    source: pathlib.Path = SKILL_SOURCE,
    expected_cores: Mapping[pathlib.PurePosixPath, pathlib.Path] | None = None,
) -> None:
    verify_archive(
        archive_path,
        load_skill_authority(material_authority),
        source,
        expected_cores,
    )
    sidecar = archive_path.with_name(archive_path.name + ".sha256")
    if sidecar.exists() or sidecar.is_symlink():
        raise SkillReleaseError("skill release must not have an external checksum sidecar")


def _verify_skill_authority_files(
    files: Mapping[str, dict[str, object]], authority: Mapping[str, object]
) -> None:
    expected_names: set[str] = set()
    for target in authority["targets"]:
        binary = target["binary"]
        expected_names.add(binary["path"])
        _verify_authority_bytes(files.get(binary["path"]), binary)
        for material in target["materials"]:
            relative = evidence_relative(target["target"], material["path"]).as_posix()
            expected_names.add(relative)
            _verify_authority_bytes(files.get(relative), material)
    if set(files) & ({path.as_posix() for _target, path in TARGET_LAYOUTS} | {name for name in files if name.startswith("evidence/")}) != expected_names:
        raise SkillReleaseError("Skill archive authority projection is not exact")


def _verify_authority_bytes(data: Mapping[str, object] | None, record: Mapping[str, object]) -> None:
    if data != {"bytes": record["bytes"], "sha256": record["sha256"]}:
        raise SkillReleaseError(f"Skill archive member differs from authority: {record['path']}")

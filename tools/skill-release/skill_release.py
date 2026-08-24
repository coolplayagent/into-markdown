"""Validate, materialize, and package the platform-neutral Into Markdown skill."""

from __future__ import annotations

import hashlib
import pathlib
import shutil
import stat
import sys
import zipfile


ROOT = pathlib.Path(__file__).resolve().parents[2]
SKILL_NAME = "into-markdown"
SKILL_SOURCE = ROOT / ".agents/skills" / SKILL_NAME
CORE_RELATIVE = pathlib.Path("share/into-markdown/skills") / SKILL_NAME
FIXED_TIMESTAMP = (2026, 1, 1, 0, 0, 0)
ALLOWED_FILES = (
    pathlib.PurePosixPath("LICENSE"),
    pathlib.PurePosixPath("SKILL.md"),
    pathlib.PurePosixPath("agents/openai.yaml"),
    pathlib.PurePosixPath("references/cli-workflows.md"),
)
ALLOWED_DIRECTORIES = (
    pathlib.PurePosixPath("agents"),
    pathlib.PurePosixPath("references"),
)


class SkillReleaseError(RuntimeError):
    """The canonical skill or a release copy violated its fixed contract."""


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


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
    if tuple(files) != ALLOWED_FILES or tuple(directories) != ALLOWED_DIRECTORIES:
        raise SkillReleaseError("skill source does not contain the exact reviewed file set")

    texts = {}
    for relative in ALLOWED_FILES:
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
    return tuple(source / relative for relative in ALLOWED_FILES)


def _validate_skill_markdown(text: str) -> None:
    lines = text.splitlines()
    if not lines or lines[0] != "---":
        raise SkillReleaseError("SKILL.md has no YAML frontmatter")
    try:
        end = lines.index("---", 1)
    except ValueError as error:
        raise SkillReleaseError("SKILL.md frontmatter is not closed") from error
    fields = {}
    for line in lines[1:end]:
        key, separator, value = line.partition(":")
        if not separator or not key or not value.strip():
            raise SkillReleaseError("SKILL.md frontmatter contains an invalid field")
        fields[key] = value.strip().strip('"')
    if set(fields) != {"name", "description"} or fields["name"] != SKILL_NAME:
        raise SkillReleaseError("SKILL.md must declare the reviewed name and description only")
    description = fields["description"].lower()
    positive = [
        "documents",
        "images",
        "audio",
        "video",
        "standard input",
        "directories",
        "remote sources",
        "into-md",
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
    if "references/cli-workflows.md" not in text:
        raise SkillReleaseError("SKILL.md does not route conditional workflows to its reference")


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
        raise SkillReleaseError("the instruction-only skill must not declare tool dependencies")


def materialize(destination: pathlib.Path, source: pathlib.Path = SKILL_SOURCE) -> pathlib.Path:
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


def create_archive(destination: pathlib.Path, source: pathlib.Path = SKILL_SOURCE) -> pathlib.Path:
    files = validate(source)
    if destination.exists() or destination.is_symlink():
        raise SkillReleaseError("skill archive destination already exists")
    destination.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(destination, "x", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        _write_directory(archive, f"{SKILL_NAME}/")
        entries = [(relative.as_posix(), None) for relative in ALLOWED_DIRECTORIES]
        entries.extend(
            (path.relative_to(source.resolve()).as_posix(), path)
            for path in files
        )
        for relative, path in sorted(entries, key=lambda entry: entry[0]):
            if path is None:
                _write_directory(archive, f"{SKILL_NAME}/{relative}/")
            else:
                info = zipfile.ZipInfo(f"{SKILL_NAME}/{relative}", FIXED_TIMESTAMP)
                info.create_system = 3
                info.external_attr = (stat.S_IFREG | 0o644) << 16
                archive.writestr(
                    info,
                    path.read_bytes(),
                    compress_type=zipfile.ZIP_DEFLATED,
                    compresslevel=9,
                )
    checksum = destination.with_name(destination.name + ".sha256")
    checksum.write_text(f"{sha256(destination)}  {destination.name}\n", encoding="ascii")
    verify_release(destination, source)
    return destination


def _write_directory(archive: zipfile.ZipFile, name: str) -> None:
    info = zipfile.ZipInfo(name, FIXED_TIMESTAMP)
    info.create_system = 3
    info.external_attr = ((stat.S_IFDIR | 0o755) << 16) | 0x10
    archive.writestr(info, b"")


def verify_archive(archive_path: pathlib.Path, source: pathlib.Path = SKILL_SOURCE) -> None:
    expected_files = {
        f"{SKILL_NAME}/{path.relative_to(source.resolve()).as_posix()}": path.read_bytes()
        for path in validate(source)
    }
    expected_directories = {
        f"{SKILL_NAME}/",
        *(f"{SKILL_NAME}/{relative.as_posix()}/" for relative in ALLOWED_DIRECTORIES),
    }
    try:
        with zipfile.ZipFile(archive_path) as archive:
            infos = archive.infolist()
            names = [info.filename for info in infos]
            if len(names) != len(set(names)) or set(names) != set(expected_files) | expected_directories:
                raise SkillReleaseError("skill archive does not contain the exact reviewed entries")
            for info in infos:
                if info.date_time != FIXED_TIMESTAMP or info.flag_bits & 0x1:
                    raise SkillReleaseError("skill archive metadata is not deterministic")
                mode = (info.external_attr >> 16) & 0o177777
                if info.filename in expected_directories:
                    if mode != stat.S_IFDIR | 0o755 or archive.read(info):
                        raise SkillReleaseError("skill archive directory metadata is invalid")
                elif mode != stat.S_IFREG | 0o644 or archive.read(info) != expected_files[info.filename]:
                    raise SkillReleaseError("skill archive file metadata or bytes are invalid")
    except zipfile.BadZipFile as error:
        raise SkillReleaseError("skill archive is not a readable ZIP") from error


def checksum_sidecar_matches(archive_path: pathlib.Path) -> bool:
    sidecar = archive_path.with_name(archive_path.name + ".sha256")
    return sidecar.is_file() and sidecar.read_text(encoding="ascii") == (
        f"{sha256(archive_path)}  {archive_path.name}\n"
    )


def verify_release(archive_path: pathlib.Path, source: pathlib.Path = SKILL_SOURCE) -> None:
    verify_archive(archive_path, source)
    if not checksum_sidecar_matches(archive_path):
        raise SkillReleaseError("skill archive checksum sidecar is missing or invalid")

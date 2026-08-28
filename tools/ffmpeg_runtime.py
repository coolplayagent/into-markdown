#!/usr/bin/env python3
"""Acquire or package the repository-owned, audited FFmpeg release runtime."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import shutil
import stat
import sys
import tempfile
import zipfile


ROOT = pathlib.Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "third_party/ffmpeg/runtime-assets.json"
PLATFORM_TOOLS = ROOT / "tools/platform-release"
if str(PLATFORM_TOOLS) not in sys.path:
    sys.path.insert(0, str(PLATFORM_TOOLS))

from acquire import acquire as acquire_pinned  # noqa: E402
from common import ReleaseError, sha256  # noqa: E402


class RuntimeAssetError(RuntimeError):
    """The audited FFmpeg runtime asset is invalid."""


def load_manifest(path: pathlib.Path = MANIFEST) -> dict:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise RuntimeAssetError("FFmpeg runtime asset manifest is invalid") from error
    targets = value.get("targets")
    if (
        value.get("schemaVersion") != 1
        or value.get("releaseTag") != "runtime-assets"
        or value.get("ffmpegVersion") != "8.1.2"
        or not isinstance(value.get("sourceRevision"), str)
        or len(value["sourceRevision"]) != 40
        or any(character not in "0123456789abcdef" for character in value["sourceRevision"])
        or value.get("sourceSha256")
        != "464beb5e7bf0c311e68b45ae2f04e9cc2af88851abb4082231742a74d97b524c"
        or not isinstance(targets, dict)
        or set(targets)
        != {
            "aarch64-apple-darwin",
            "aarch64-unknown-linux-gnu",
            "x86_64-pc-windows-msvc",
            "x86_64-unknown-linux-gnu",
        }
    ):
        raise RuntimeAssetError("FFmpeg runtime asset manifest authority is invalid")
    for target, record in targets.items():
        expected_url = (
            "https://github.com/coolplayagent/into-markdown/releases/download/"
            f"runtime-assets/ffmpeg-lgpl-8.1.2-{target}.zip"
        )
        members = record.get("members") if isinstance(record, dict) else None
        if (
            not isinstance(record, dict)
            or set(record) != {"url", "bytes", "sha256", "members"}
            or record.get("url") != expected_url
            or type(record.get("bytes")) is not int
            or record["bytes"] <= 0
            or not _sha256(record.get("sha256"))
            or not isinstance(members, dict)
            or set(members) != set(expected_names(target))
        ):
            raise RuntimeAssetError("FFmpeg runtime target authority is invalid")
        for member in members.values():
            if (
                not isinstance(member, dict)
                or set(member) != {"bytes", "sha256"}
                or type(member.get("bytes")) is not int
                or member["bytes"] <= 0
                or not _sha256(member.get("sha256"))
            ):
                raise RuntimeAssetError("FFmpeg runtime member authority is invalid")
    return value


def _sha256(value: object) -> bool:
    return (
        isinstance(value, str)
        and len(value) == 64
        and all(character in "0123456789abcdef" for character in value)
    )


def expected_names(target: str) -> tuple[str, ...]:
    suffix = ".exe" if target == "x86_64-pc-windows-msvc" else ""
    return (
        "COPYING.LGPLv2.1",
        f"ffmpeg-{target}{suffix}",
        f"ffmpeg-authority-{target}.json",
        f"ffmpeg-inventory-{target}.json",
        f"ffmpeg-relink-{target}.tar",
    )


def _regular_info(info: zipfile.ZipInfo) -> bool:
    mode = (info.external_attr >> 16) & 0o177777
    return (
        not info.is_dir()
        and info.create_system == 3
        and stat.S_IFMT(mode) == stat.S_IFREG
        and stat.S_IMODE(mode) in {0o644, 0o755}
        and not info.comment
        and not info.extra
        and not (info.flag_bits & 0x1)
    )


def _validate_members(archive: zipfile.ZipFile, target: str, authority: dict) -> None:
    infos = archive.infolist()
    names = [info.filename for info in infos]
    expected = list(expected_names(target))
    if names != expected or len(names) != len(set(names)) or archive.comment:
        raise RuntimeAssetError("FFmpeg runtime archive does not contain the exact reviewed set")
    declared = authority.get("members")
    if not isinstance(declared, dict) or set(declared) != set(expected):
        raise RuntimeAssetError("FFmpeg runtime member authority is incomplete")
    for info in infos:
        mode = stat.S_IMODE((info.external_attr >> 16) & 0o177777)
        binary = info.filename.startswith(f"ffmpeg-{target}") and not info.filename.endswith(
            (".json", ".tar")
        )
        if (
            pathlib.PurePosixPath(info.filename).name != info.filename
            or not _regular_info(info)
            or mode != (0o755 if binary else 0o644)
        ):
            raise RuntimeAssetError(f"unsafe FFmpeg runtime member: {info.filename}")
        record = declared[info.filename]
        data = archive.read(info)
        if (
            not isinstance(record, dict)
            or set(record) != {"bytes", "sha256"}
            or record["bytes"] != len(data)
            or record["sha256"] != hashlib.sha256(data).hexdigest()
        ):
            raise RuntimeAssetError(
                f"FFmpeg runtime member differs from authority: {info.filename}"
            )


def acquire(target: str, output: pathlib.Path, manifest_path: pathlib.Path = MANIFEST) -> None:
    manifest = load_manifest(manifest_path)
    try:
        authority = manifest["targets"][target]
    except KeyError as error:
        raise RuntimeAssetError(f"unsupported FFmpeg runtime target: {target}") from error
    if output.exists() or output.is_symlink():
        raise RuntimeAssetError("FFmpeg runtime output must not already exist")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(
        prefix=".into-md-ffmpeg-", dir=output.parent
    ) as name:
        temporary_root = pathlib.Path(name)
        try:
            acquire_pinned(
                temporary_root,
                {
                    "ffmpeg-runtime.zip": {
                        key: authority[key]
                        for key in ("url", "bytes", "sha256")
                    }
                },
            )
        except ReleaseError as error:
            raise RuntimeAssetError(str(error)) from error
        archive_path = temporary_root / "ffmpeg-runtime.zip"
        try:
            with zipfile.ZipFile(archive_path) as archive:
                _validate_members(archive, target, authority)
                staged = temporary_root / "staged"
                staged.mkdir()
                for info in archive.infolist():
                    destination = staged / info.filename
                    with archive.open(info) as source, destination.open("xb") as sink:
                        shutil.copyfileobj(source, sink, 1024 * 1024)
                    destination.chmod(
                        0o755 if info.filename.startswith(f"ffmpeg-{target}") and not info.filename.endswith((".json", ".tar")) else 0o644
                    )
        except (OSError, zipfile.BadZipFile) as error:
            raise RuntimeAssetError("FFmpeg runtime archive cannot be read") from error
        os.replace(staged, output)


def package(target: str, source: pathlib.Path, destination: pathlib.Path) -> dict:
    names = expected_names(target)
    if not source.is_dir() or source.is_symlink():
        raise RuntimeAssetError("FFmpeg audit source is not a trusted directory")
    entries = list(source.iterdir())
    if {entry.name for entry in entries} != set(names) or any(
        not entry.is_file() or entry.is_symlink() for entry in entries
    ):
        raise RuntimeAssetError("FFmpeg audit source does not contain the exact reviewed set")
    if destination.exists() or destination.is_symlink():
        raise RuntimeAssetError("FFmpeg runtime archive destination already exists")
    destination.parent.mkdir(parents=True, exist_ok=True)
    members: dict[str, dict] = {}
    with zipfile.ZipFile(
        destination, "x", compression=zipfile.ZIP_DEFLATED, compresslevel=9
    ) as archive:
        for name in names:
            path = source / name
            executable = name.startswith(f"ffmpeg-{target}") and not name.endswith(
                (".json", ".tar")
            )
            info = zipfile.ZipInfo(name, (2026, 1, 1, 0, 0, 0))
            info.create_system = 3
            info.external_attr = (stat.S_IFREG | (0o755 if executable else 0o644)) << 16
            data = path.read_bytes()
            archive.writestr(
                info, data, compress_type=zipfile.ZIP_DEFLATED, compresslevel=9
            )
            members[name] = {
                "bytes": len(data),
                "sha256": hashlib.sha256(data).hexdigest(),
            }
    return {
        "url": f"https://github.com/coolplayagent/into-markdown/releases/download/runtime-assets/{destination.name}",
        "bytes": destination.stat().st_size,
        "sha256": sha256(destination),
        "members": members,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)
    acquire_parser = subparsers.add_parser("acquire")
    acquire_parser.add_argument("--target", required=True)
    acquire_parser.add_argument("--output", required=True, type=pathlib.Path)
    acquire_parser.add_argument("--manifest", type=pathlib.Path, default=MANIFEST)
    package_parser = subparsers.add_parser("package")
    package_parser.add_argument("--target", required=True)
    package_parser.add_argument("--source", required=True, type=pathlib.Path)
    package_parser.add_argument("--output", required=True, type=pathlib.Path)
    arguments = parser.parse_args()
    if arguments.command == "acquire":
        acquire(arguments.target, arguments.output.resolve(), arguments.manifest.resolve())
    else:
        print(
            json.dumps(
                package(arguments.target, arguments.source.resolve(), arguments.output.resolve()),
                indent=2,
                sort_keys=True,
            )
        )


if __name__ == "__main__":
    main()

"""Deterministic tar.gz writer and strict extractor."""

from __future__ import annotations

import argparse
import gzip
import os
import pathlib
import stat
import tarfile

from common import ReleaseError, regular_files


def create(source: pathlib.Path, destination: pathlib.Path, epoch: int) -> None:
    files = regular_files(source)
    directories = sorted(
        {parent for file in files for parent in file.relative_to(source).parents if parent != pathlib.Path(".")},
        key=lambda path: path.as_posix(),
    )
    with destination.open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=epoch, compresslevel=9) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as archive:
                for relative in directories:
                    info = tarfile.TarInfo(relative.as_posix() + "/")
                    set_metadata(info, epoch, 0o755, tarfile.DIRTYPE)
                    archive.addfile(info)
                for path in files:
                    relative = path.relative_to(source)
                    mode = 0o755 if os.access(path, os.X_OK) else 0o644
                    info = tarfile.TarInfo(relative.as_posix())
                    set_metadata(info, epoch, mode, tarfile.REGTYPE)
                    info.size = path.stat().st_size
                    with path.open("rb") as contents:
                        archive.addfile(info, contents)


def set_metadata(info: tarfile.TarInfo, epoch: int, mode: int, kind: bytes) -> None:
    info.uid = info.gid = 0
    info.uname = info.gname = "root"
    info.mtime = epoch
    info.mode = mode
    info.type = kind


def extract(archive: pathlib.Path, destination: pathlib.Path) -> None:
    destination.mkdir(parents=True, exist_ok=False)
    with tarfile.open(archive, "r:gz") as source:
        seen: set[str] = set()
        for member in source:
            name = pathlib.PurePosixPath(member.name)
            if name.is_absolute() or ".." in name.parts or member.name in seen:
                raise ReleaseError("release archive contains an unsafe or duplicate path")
            seen.add(member.name)
            if member.isdir():
                (destination / member.name).mkdir(parents=True, exist_ok=True)
                continue
            if not member.isfile() or member.mode not in {0o644, 0o755}:
                raise ReleaseError("release archive contains an unauthorized entry type or mode")
            target = destination / member.name
            target.parent.mkdir(parents=True, exist_ok=True)
            opened = source.extractfile(member)
            if opened is None:
                raise ReleaseError("release archive entry cannot be read")
            descriptor = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, member.mode)
            with os.fdopen(descriptor, "wb") as output:
                while chunk := opened.read(1024 * 1024):
                    output.write(chunk)


def main() -> None:
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    create_parser = commands.add_parser("create")
    create_parser.add_argument("source", type=pathlib.Path)
    create_parser.add_argument("destination", type=pathlib.Path)
    create_parser.add_argument("--epoch", type=int, required=True)
    extract_parser = commands.add_parser("extract")
    extract_parser.add_argument("archive", type=pathlib.Path)
    extract_parser.add_argument("destination", type=pathlib.Path)
    arguments = parser.parse_args()
    if arguments.command == "create":
        create(arguments.source.resolve(), arguments.destination.resolve(), arguments.epoch)
    else:
        extract(arguments.archive.resolve(), arguments.destination.resolve())


if __name__ == "__main__":
    main()

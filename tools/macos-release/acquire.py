"""Explicit fixed-download and no-follow extraction boundary."""

from __future__ import annotations

import argparse
import os
import pathlib
import shutil
import tarfile
import urllib.error
import urllib.parse
import urllib.request

from common import ReleaseError, authority, sha256

ALLOWED_HOSTS = {
    "cdn-lfs-us-1.hf.co",
    "ffmpeg.org",
    "github.com",
    "huggingface.co",
    "objects.githubusercontent.com",
    "paddle-model-ecology.bj.bcebos.com",
    "raw.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "us.aws.cdn.hf.co",
}
DOWNLOAD_ATTEMPTS = 4


def acquire(cache: pathlib.Path, selected: set[str] | None = None) -> None:
    cache.mkdir(parents=True, exist_ok=True)
    for item in authority()["downloads"]:
        if selected is not None and item["id"] not in selected:
            continue
        destination = cache / item["id"]
        if destination.is_file() and validate(destination, item):
            continue
        temporary = destination.with_suffix(".download")
        for attempt in range(1, DOWNLOAD_ATTEMPTS + 1):
            temporary.unlink(missing_ok=True)
            try:
                request = urllib.request.Request(
                    item["url"], headers={"User-Agent": "into-markdown-release/1"}
                )
                with urllib.request.urlopen(request, timeout=180) as response, temporary.open(
                    "xb"
                ) as output:
                    parsed = urllib.parse.urlparse(response.geturl())
                    if parsed.scheme != "https" or parsed.hostname not in ALLOWED_HOSTS:
                        raise ReleaseError("download redirected to an unauthorized host")
                    remaining = item["bytes"]
                    while remaining:
                        chunk = response.read(min(1024 * 1024, remaining + 1))
                        if not chunk or len(chunk) > remaining:
                            raise ReleaseError(
                                f"{item['id']} download size disagrees with authority"
                            )
                        output.write(chunk)
                        remaining -= len(chunk)
                    if response.read(1):
                        raise ReleaseError(f"{item['id']} download exceeds authority")
            except (TimeoutError, urllib.error.URLError, OSError) as error:
                temporary.unlink(missing_ok=True)
                if attempt == DOWNLOAD_ATTEMPTS:
                    raise ReleaseError(
                        f"{item['id']} download failed after {DOWNLOAD_ATTEMPTS} attempts"
                    ) from error
                continue
            if not validate(temporary, item):
                temporary.unlink(missing_ok=True)
                raise ReleaseError(f"{item['id']} download hash disagrees with authority")
            os.replace(temporary, destination)
            break


def validate(path: pathlib.Path, item: dict) -> bool:
    return path.stat().st_size == item["bytes"] and sha256(path) == item["sha256"]


def extract_tar(archive: pathlib.Path, destination: pathlib.Path, members: dict[str, str]) -> None:
    destination.mkdir(parents=True, exist_ok=True)
    found: set[str] = set()
    with tarfile.open(archive, "r:*") as source:
        for member in source:
            normalized = pathlib.PurePosixPath(member.name)
            if normalized.is_absolute() or ".." in normalized.parts:
                raise ReleaseError("download archive contains an unsafe entry")
            if member.issym() or member.islnk():
                link = pathlib.PurePosixPath(member.linkname)
                if link.is_absolute() or ".." in link.parts or member.name in members:
                    raise ReleaseError("download archive contains an unsafe link entry")
                continue
            if member.name not in members:
                continue
            if not member.isfile():
                raise ReleaseError("authorized archive member is not regular")
            target = destination / members[member.name]
            target.parent.mkdir(parents=True, exist_ok=True)
            opened = source.extractfile(member)
            if opened is None:
                raise ReleaseError("authorized archive member cannot be read")
            with target.open("xb") as output:
                shutil.copyfileobj(opened, output, 1024 * 1024)
            found.add(member.name)
    if found != set(members):
        raise ReleaseError("download archive omits an authorized member")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("cache", type=pathlib.Path)
    arguments = parser.parse_args()
    acquire(arguments.cache.resolve())


if __name__ == "__main__":
    main()

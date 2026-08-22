"""Exact-size, exact-hash downloads for native platform release inputs."""

from __future__ import annotations

import os
import pathlib
import urllib.parse
import urllib.request

from common import ReleaseError, sha256

ALLOWED_HOSTS = {
    "cdn-lfs-us-1.hf.co",
    "ffmpeg.org",
    "github.com",
    "huggingface.co",
    "mirror.aarnet.edu.au",
    "objects.githubusercontent.com",
    "paddle-model-ecology.bj.bcebos.com",
    "raw.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "us.aws.cdn.hf.co",
}


def acquire(cache: pathlib.Path, downloads: dict[str, dict]) -> None:
    cache.mkdir(parents=True, exist_ok=True)
    for identity, item in sorted(downloads.items()):
        destination = cache / identity
        if destination.is_file() and valid(destination, item):
            continue
        temporary = destination.with_suffix(".download")
        temporary.unlink(missing_ok=True)
        request = urllib.request.Request(
            item["url"], headers={"User-Agent": "into-markdown-release/1"}
        )
        with urllib.request.urlopen(request, timeout=180) as response, temporary.open(
            "xb"
        ) as output:
            parsed = urllib.parse.urlparse(response.geturl())
            if parsed.scheme != "https" or parsed.hostname not in ALLOWED_HOSTS:
                raise ReleaseError(f"{identity} redirected outside the release allowlist")
            expected = item.get("bytes")
            total = 0
            while chunk := response.read(1024 * 1024):
                total += len(chunk)
                if expected is not None and total > expected:
                    raise ReleaseError(f"{identity} download exceeds authority")
                output.write(chunk)
        if not valid(temporary, item):
            raise ReleaseError(f"{identity} download differs from authority")
        os.replace(temporary, destination)


def valid(path: pathlib.Path, item: dict) -> bool:
    expected = item.get("bytes")
    return (
        (expected is None or path.stat().st_size == expected)
        and sha256(path) == item["sha256"]
    )
